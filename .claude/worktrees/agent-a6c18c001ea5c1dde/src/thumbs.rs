//! Image thumbnails on hover, from the Windows shell (IShellItemImageFactory,
//! which also uses the system thumbnail cache). Loaded lazily in one worker
//! thread. Cloud placeholders, Google Drive streaming drives and network paths
//! are skipped: hovering must never trigger a download.

use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
use std::ffi::c_void;
use std::sync::mpsc::{Receiver, Sender, channel};

use eframe::egui;

use crate::i18n::tr;

/// Thumbnail edge in pixels.
pub const SIZE: i32 = 192;
/// Larger files are skipped (RAW photos and the like are slow to decode).
const MAX_BYTES: u64 = 100 << 20;
/// Textures kept in memory.
const CACHE: usize = 96;

const IMAGE_EXTS: &[&str] = &[
    "jpg", "jpeg", "jfif", "png", "gif", "bmp", "tif", "tiff", "webp", "heic", "heif", "avif", "jxl", "ico",
];

pub fn is_image(ext: &str) -> bool {
    IMAGE_EXTS.contains(&ext)
}

/// Content not on the local disk (cloud placeholder / offline), or unreadable.
fn not_local(path: &str) -> bool {
    use windows_sys::Win32::Storage::FileSystem::{GetFileAttributesW, INVALID_FILE_ATTRIBUTES};
    const OFFLINE: u32 = 0x1000;
    const RECALL_ON_OPEN: u32 = 0x4_0000;
    const RECALL_ON_DATA_ACCESS: u32 = 0x40_0000;
    const UNPINNED: u32 = 0x10_0000;
    let a = unsafe { GetFileAttributesW(crate::everything::wide(path).as_ptr()) };
    a == INVALID_FILE_ATTRIBUTES || a & (OFFLINE | RECALL_ON_OPEN | RECALL_ON_DATA_ACCESS | UNPINNED) != 0
}

/// Decide whether a file can be read for a thumbnail without side effects.
/// `stream_drive(drive)`: true for virtual drives that download on read.
pub fn allowed(path: &str, size: u64, mut stream_drive: impl FnMut(&str) -> bool) -> bool {
    if path.starts_with("\\\\") || size > MAX_BYTES || size == 0 {
        return false;
    }
    let drive = path.get(..2).unwrap_or("");
    !stream_drive(drive) && !not_local(path)
}

const IID_ISHELLITEMIMAGEFACTORY: windows_sys::core::GUID = windows_sys::core::GUID::from_u128(0xbcc18b79_ba16_442f_80c4_8a59c30c463b);

#[repr(C)]
struct FactoryVtbl {
    _query_interface: usize,
    _add_ref: usize,
    release: unsafe extern "system" fn(*mut c_void) -> u32,
    get_image: unsafe extern "system" fn(*mut c_void, windows_sys::Win32::Foundation::SIZE, i32, *mut *mut c_void) -> i32,
}

/// Shell thumbnail as RGBA (thread must have COM initialized).
fn shell_thumbnail(path: &str, edge: i32) -> Option<egui::ColorImage> {
    use windows_sys::Win32::Graphics::Gdi::*;
    use windows_sys::Win32::UI::Shell::SHCreateItemFromParsingName;
    const SIIGBF_THUMBNAILONLY: i32 = 0x8;
    let w = crate::everything::wide(path);
    let mut obj: *mut c_void = std::ptr::null_mut();
    let hr = unsafe { SHCreateItemFromParsingName(w.as_ptr(), std::ptr::null_mut(), &IID_ISHELLITEMIMAGEFACTORY, &mut obj) };
    if hr < 0 || obj.is_null() {
        return None;
    }
    let vtbl = unsafe { &**(obj as *const *const FactoryVtbl) };
    let mut hbmp: *mut c_void = std::ptr::null_mut();
    let size = windows_sys::Win32::Foundation::SIZE { cx: edge, cy: edge };
    let hr = unsafe { (vtbl.get_image)(obj, size, SIIGBF_THUMBNAILONLY, &mut hbmp) };
    unsafe { (vtbl.release)(obj) };
    if hr < 0 || hbmp.is_null() {
        return None;
    }
    let hbmp = hbmp as HBITMAP;
    let img = unsafe {
        let mut bm: BITMAP = std::mem::zeroed();
        let ok = GetObjectW(hbmp as _, std::mem::size_of::<BITMAP>() as i32, &mut bm as *mut _ as *mut c_void);
        let (bw, bh) = (bm.bmWidth, bm.bmHeight.abs());
        let mut img = None;
        if ok != 0 && bw > 0 && bh > 0 && bw <= 4096 && bh <= 4096 {
            let mut bi: BITMAPINFO = std::mem::zeroed();
            bi.bmiHeader.biSize = std::mem::size_of::<BITMAPINFOHEADER>() as u32;
            bi.bmiHeader.biWidth = bw;
            bi.bmiHeader.biHeight = -bh; // top-down
            bi.bmiHeader.biPlanes = 1;
            bi.bmiHeader.biBitCount = 32;
            bi.bmiHeader.biCompression = BI_RGB as u32;
            let mut buf = vec![0u8; bw as usize * bh as usize * 4];
            let dc = CreateCompatibleDC(std::ptr::null_mut());
            let lines = GetDIBits(dc, hbmp, 0, bh as u32, buf.as_mut_ptr() as *mut c_void, &mut bi, DIB_RGB_COLORS);
            DeleteDC(dc);
            if lines > 0 {
                img = Some(to_image(bw as usize, bh as usize, buf));
            }
        }
        DeleteObject(hbmp as _);
        img
    };
    img
}

/// BGRA (premultiplied, or with no alpha at all) → egui image.
fn to_image(w: usize, h: usize, mut buf: Vec<u8>) -> egui::ColorImage {
    let has_alpha = buf.chunks_exact(4).any(|p| p[3] != 0);
    for p in buf.chunks_exact_mut(4) {
        p.swap(0, 2);
        if !has_alpha {
            p[3] = 255;
        }
    }
    egui::ColorImage::from_rgba_premultiplied([w, h], &buf)
}

enum Slot {
    Pending,
    Ready(egui::TextureHandle),
    Missing,
}

#[derive(Default)]
struct Inner {
    tx: Option<Sender<String>>,
    rx: Option<Receiver<(String, Option<Option<egui::ColorImage>>)>>,
    cache: HashMap<String, Slot>,
    order: VecDeque<String>,
}

/// Lazily loaded thumbnails (interior mutability so table rows can use it).
#[derive(Default)]
pub struct Thumbs {
    inner: RefCell<Inner>,
}

fn worker(ctx: egui::Context, rx: Receiver<String>, tx: Sender<(String, Option<Option<egui::ColorImage>>)>) {
    use windows_sys::Win32::System::Com::{COINIT_APARTMENTTHREADED, CoInitializeEx};
    unsafe { CoInitializeEx(std::ptr::null(), COINIT_APARTMENTTHREADED as _) };
    let mut streams: HashMap<String, bool> = HashMap::new();
    while let Ok(mut path) = rx.recv() {
        // Only the latest hover matters: skip the ones the pointer already left.
        while let Ok(newer) = rx.try_recv() {
            let _ = tx.send((std::mem::replace(&mut path, newer), None));
        }
        let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
        let mut stream = |d: &str| {
            *streams.entry(d.to_uppercase()).or_insert_with(|| crate::tree::volume_label(d).contains("Google Drive"))
        };
        let img = if allowed(&path, size, &mut stream) { shell_thumbnail(&path, SIZE) } else { None };
        if tx.send((path, Some(img))).is_err() {
            return;
        }
        ctx.request_repaint();
    }
}

impl Thumbs {
    fn poll(&self, ctx: &egui::Context) {
        let mut inner = self.inner.borrow_mut();
        let Some(rx) = &inner.rx else { return };
        let got: Vec<_> = rx.try_iter().collect();
        for (path, img) in got {
            match img {
                None => {
                    inner.cache.remove(&path); // skipped: asked again on the next hover
                }
                Some(None) => {
                    inner.cache.insert(path, Slot::Missing);
                }
                Some(Some(img)) => {
                    let tex = ctx.load_texture(format!("thumb:{path}"), img, egui::TextureOptions::LINEAR);
                    inner.cache.insert(path.clone(), Slot::Ready(tex));
                    inner.order.push_back(path);
                    while inner.order.len() > CACHE {
                        if let Some(old) = inner.order.pop_front() {
                            inner.cache.remove(&old);
                        }
                    }
                }
            }
        }
    }

    /// Tooltip body for an image file: its thumbnail, or a spinner while loading.
    pub fn hover_ui(&self, ui: &mut egui::Ui, path: &str) {
        let ctx = ui.ctx().clone();
        self.poll(&ctx);
        let mut inner = self.inner.borrow_mut();
        if !inner.cache.contains_key(path) {
            if inner.tx.is_none() {
                let (tx, rx) = channel();
                let (rtx, rrx) = channel();
                let c = ctx.clone();
                if std::thread::Builder::new().name("thumbnails".into()).spawn(move || worker(c, rx, rtx)).is_ok() {
                    inner.tx = Some(tx);
                    inner.rx = Some(rrx);
                }
            }
            if let Some(tx) = &inner.tx
                && tx.send(path.to_string()).is_ok()
            {
                inner.cache.insert(path.to_string(), Slot::Pending);
            }
        }
        match inner.cache.get(path) {
            Some(Slot::Ready(t)) => {
                let s = t.size_vec2();
                let k = (SIZE as f32 / s.x.max(s.y)).min(1.0);
                ui.image((t.id(), s * k));
            }
            Some(Slot::Pending) => {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.weak(tr("Cargando miniatura…", "Loading thumbnail…"));
                });
                ctx.request_repaint_after(std::time::Duration::from_millis(100));
            }
            _ => {
                ui.weak(tr("(sin miniatura: en la nube o no disponible)", "(no thumbnail: cloud-only or unavailable)"));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thumbnail_rules() {
        assert!(is_image("jpg") && is_image("png") && !is_image("mp4") && !is_image(""));
        assert!(!allowed("\\\\server\\share\\a.jpg", 10, |_| false), "network paths are skipped");
        assert!(!allowed("C:\\a.jpg", MAX_BYTES + 1, |_| false), "huge files are skipped");
        assert!(!allowed("G:\\a.jpg", 10, |d| d == "G:"), "streaming drives are skipped");
        assert!(!allowed("Q9:\\missing.jpg", 10, |_| false), "unreadable files are skipped");
        let img = to_image(1, 2, vec![10, 20, 30, 0, 1, 2, 3, 0]);
        assert_eq!(img.pixels[0], egui::Color32::from_rgb(30, 20, 10), "BGR → RGB, opaque without alpha");
    }

    #[test]
    #[ignore = "needs the Windows shell"]
    fn live_shell_thumbnail() {
        use windows_sys::Win32::System::Com::{COINIT_APARTMENTTHREADED, CoInitializeEx};
        unsafe { CoInitializeEx(std::ptr::null(), COINIT_APARTMENTTHREADED as _) };
        // Write a tiny BMP and ask the shell for its thumbnail.
        let p = std::env::temp_dir().join(format!("wiz_thumb_{}.bmp", std::process::id()));
        let (w, h) = (4u32, 4u32);
        let mut bmp = Vec::new();
        let data = (w * h * 4) as u32;
        bmp.extend_from_slice(b"BM");
        bmp.extend_from_slice(&(54 + data).to_le_bytes());
        bmp.extend_from_slice(&[0, 0, 0, 0]);
        bmp.extend_from_slice(&54u32.to_le_bytes());
        bmp.extend_from_slice(&40u32.to_le_bytes());
        bmp.extend_from_slice(&w.to_le_bytes());
        bmp.extend_from_slice(&h.to_le_bytes());
        bmp.extend_from_slice(&1u16.to_le_bytes());
        bmp.extend_from_slice(&32u16.to_le_bytes());
        bmp.extend_from_slice(&[0; 24]);
        for _ in 0..w * h {
            bmp.extend_from_slice(&[0, 0, 255, 255]);
        }
        std::fs::write(&p, bmp).unwrap();
        let img = shell_thumbnail(p.to_str().unwrap(), 64);
        let _ = std::fs::remove_file(&p);
        let img = img.expect("thumbnail");
        assert!(img.size[0] > 0 && img.size[1] > 0);
    }
}
