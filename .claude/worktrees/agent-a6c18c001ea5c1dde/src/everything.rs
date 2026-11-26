//! Cliente IPC mínimo para Everything (voidtools) vía WM_COPYDATA.
//! Implementa el protocolo QUERY2 del SDK oficial sin necesitar Everything64.dll.

use std::cell::RefCell;
use std::ptr::null_mut;
use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows_sys::Win32::System::DataExchange::COPYDATASTRUCT;
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::WindowsAndMessaging::*;

use crate::i18n::tr;

const IPC_CLASS: &str = "EVERYTHING_TASKBAR_NOTIFICATION";
const COPYDATA_QUERY2W: usize = 18;
const IPC_IS_DB_LOADED: usize = 401;

const REQ_NAME: u32 = 0x0000_0001;
const REQ_PATH: u32 = 0x0000_0002;
const REQ_SIZE: u32 = 0x0000_0010;
const REQ_DATE_MODIFIED: u32 = 0x0000_0040;

const ITEM_FOLDER: u32 = 0x1;
const ITEM_DRIVE: u32 = 0x2;

const SORT_NAME_ASCENDING: u32 = 1;
const REPLY_ID: u32 = 0x5745_5A31;
const PAGE: u32 = 250_000;

pub fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Un elemento devuelto por Everything (las cadenas apuntan al buffer de la respuesta).
pub struct Item<'a> {
    pub is_dir: bool,
    pub name: &'a [u16],
    pub path: &'a [u16],
    /// `None` si Everything no tiene el tamaño indexado (p. ej. carpetas).
    pub size: Option<u64>,
    pub modified: u64,
}

#[derive(Debug)]
pub enum Status {
    NotRunning,
    Loading,
    Ready,
}

fn everything_hwnd() -> HWND {
    let class = wide(IPC_CLASS);
    unsafe { FindWindowW(class.as_ptr(), null_mut()) }
}

pub fn status() -> Status {
    let hwnd = everything_hwnd();
    if hwnd.is_null() {
        return Status::NotRunning;
    }
    let loaded = unsafe { SendMessageW(hwnd, WM_USER, IPC_IS_DB_LOADED, 0) };
    if loaded != 0 { Status::Ready } else { Status::Loading }
}

thread_local! {
    static REPLY: RefCell<Option<Vec<u8>>> = const { RefCell::new(None) };
}

unsafe extern "system" fn reply_wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if msg == WM_COPYDATA {
        let cds = unsafe { &*(lparam as *const COPYDATASTRUCT) };
        if cds.dwData == REPLY_ID as usize {
            let data = unsafe { std::slice::from_raw_parts(cds.lpData as *const u8, cds.cbData as usize) };
            REPLY.with(|r| *r.borrow_mut() = Some(data.to_vec()));
            return 1;
        }
    }
    unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
}

struct ReplyWindow(HWND);

impl ReplyWindow {
    fn new() -> Result<Self, String> {
        unsafe {
            let hinst = GetModuleHandleW(null_mut());
            let class = wide("WizEverythingIpcReply");
            let wc = WNDCLASSW {
                style: 0,
                lpfnWndProc: Some(reply_wndproc),
                cbClsExtra: 0,
                cbWndExtra: 0,
                hInstance: hinst,
                hIcon: null_mut(),
                hCursor: null_mut(),
                hbrBackground: null_mut(),
                lpszMenuName: null_mut(),
                lpszClassName: class.as_ptr(),
            };
            RegisterClassW(&wc); // falla sin problema si ya estaba registrada
            let hwnd = CreateWindowExW(
                0,
                class.as_ptr(),
                class.as_ptr(),
                0,
                0,
                0,
                0,
                0,
                HWND_MESSAGE,
                null_mut(),
                hinst,
                null_mut(),
            );
            if hwnd.is_null() {
                return Err(tr("No se pudo crear la ventana IPC", "Could not create the IPC window").into());
            }
            Ok(Self(hwnd))
        }
    }
}

impl Drop for ReplyWindow {
    fn drop(&mut self) {
        unsafe { DestroyWindow(self.0) };
    }
}

fn query_page(reply: &ReplyWindow, search: &str, offset: u32, max: u32) -> Result<Vec<u8>, String> {
    let target = everything_hwnd();
    if target.is_null() {
        return Err(tr("Everything no está en ejecución", "Everything is not running").into());
    }
    let request = REQ_NAME | REQ_PATH | REQ_SIZE | REQ_DATE_MODIFIED;
    let mut buf: Vec<u8> = Vec::with_capacity(64);
    for v in [reply.0 as usize as u32, REPLY_ID, 0, offset, max, request, SORT_NAME_ASCENDING] {
        buf.extend_from_slice(&v.to_le_bytes());
    }
    for c in wide(search) {
        buf.extend_from_slice(&c.to_le_bytes());
    }
    let cds = COPYDATASTRUCT {
        dwData: COPYDATA_QUERY2W,
        cbData: buf.len() as u32,
        lpData: buf.as_mut_ptr() as *mut _,
    };
    REPLY.with(|r| *r.borrow_mut() = None);
    let ok = unsafe { SendMessageW(target, WM_COPYDATA, reply.0 as usize, &cds as *const _ as isize) };
    if ok == 0 {
        return Err(tr("Everything rechazó la consulta IPC", "Everything rejected the IPC query").into());
    }
    let start = Instant::now();
    loop {
        if let Some(data) = REPLY.with(|r| r.borrow_mut().take()) {
            return Ok(data);
        }
        unsafe {
            let mut msg: MSG = std::mem::zeroed();
            while PeekMessageW(&mut msg, null_mut(), 0, 0, PM_REMOVE) != 0 {
                TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
        if start.elapsed() > Duration::from_secs(120) {
            return Err(tr("Tiempo de espera agotado esperando a Everything", "Timed out waiting for Everything").into());
        }
        std::thread::sleep(Duration::from_millis(2));
    }
}

fn rd_u32(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(b[at..at + 4].try_into().unwrap())
}
fn rd_u64(b: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(b[at..at + 8].try_into().unwrap())
}

/// Lee una cadena IPC: DWORD longitud (en caracteres) + UTF-16 + terminador nulo.
fn rd_str<'a>(b: &'a [u8], at: &mut usize) -> &'a [u16] {
    let len = rd_u32(b, *at) as usize;
    *at += 4;
    let bytes = &b[*at..*at + len * 2];
    *at += len * 2 + 2;
    // El buffer viene de un Vec<u8>; puede no estar alineado a 2, así que lo
    // reinterpretamos solo si lo está, si no copiamos (raro en la práctica).
    if (bytes.as_ptr() as usize).is_multiple_of(2) {
        unsafe { std::slice::from_raw_parts(bytes.as_ptr() as *const u16, len) }
    } else {
        // Leak controlado: no debería ocurrir porque Everything alinea los datos.
        let v: Vec<u16> = bytes.as_chunks::<2>().0.iter().map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
        Box::leak(v.into_boxed_slice())
    }
}

/// Recorre todo el índice de Everything y llama a `f` por cada elemento.
/// `progress(hechos, total)` se invoca tras cada página.
pub fn for_each_item(
    search: &str,
    mut f: impl FnMut(Item<'_>),
    mut progress: impl FnMut(u32, u32),
) -> Result<(), String> {
    // Pipeline: a fetcher thread asks Everything for the next page while this
    // thread parses the current one.
    let (tx, rx) = std::sync::mpsc::sync_channel::<Result<Vec<u8>, String>>(2);
    let search_owned = search.to_string();
    let fetcher = std::thread::spawn(move || {
        let reply = match ReplyWindow::new() {
            Ok(r) => r,
            Err(e) => {
                let _ = tx.send(Err(e));
                return;
            }
        };
        let mut offset = 0u32;
        loop {
            let page = query_page(&reply, &search_owned, offset, PAGE);
            let (num, total) = match &page {
                Ok(d) if d.len() >= 20 => (rd_u32(d, 4), rd_u32(d, 0)),
                _ => (0, 0),
            };
            let failed = page.is_err();
            if tx.send(page).is_err() || failed || num == 0 {
                return;
            }
            offset += num;
            if offset >= total {
                return;
            }
        }
    });
    let result = parse_pages(rx, &mut f, &mut progress);
    let _ = fetcher.join();
    result
}

fn parse_pages(
    rx: std::sync::mpsc::Receiver<Result<Vec<u8>, String>>,
    f: &mut impl FnMut(Item<'_>),
    progress: &mut impl FnMut(u32, u32),
) -> Result<(), String> {
    let mut offset = 0u32;
    loop {
        let Ok(data) = rx.recv() else { return Ok(()) };
        let data = data?;
        if data.len() < 20 {
            return Err(tr("Respuesta IPC inválida", "Invalid IPC reply").into());
        }
        let total = rd_u32(&data, 0);
        let num = rd_u32(&data, 4);
        let request = rd_u32(&data, 12);
        progress(offset, total); // permite reservar memoria antes de procesar
        for i in 0..num as usize {
            let flags = rd_u32(&data, 20 + i * 8);
            let mut at = rd_u32(&data, 20 + i * 8 + 4) as usize;
            let name = if request & REQ_NAME != 0 { rd_str(&data, &mut at) } else { &[] };
            let path = if request & REQ_PATH != 0 { rd_str(&data, &mut at) } else { &[] };
            let size = if request & REQ_SIZE != 0 {
                let s = rd_u64(&data, at);
                at += 8;
                if s == u64::MAX { None } else { Some(s) }
            } else {
                None
            };
            let modified = if request & REQ_DATE_MODIFIED != 0 { rd_u64(&data, at) } else { 0 };
            f(Item {
                is_dir: flags & (ITEM_FOLDER | ITEM_DRIVE) != 0,
                name,
                path,
                size,
                modified,
            });
        }
        offset += num;
        progress(offset, total);
        if num == 0 || offset >= total {
            return Ok(());
        }
    }
}
