//! Alta/baja en HKCU\Software\Microsoft\Windows\CurrentVersion\Run.

use std::ptr::null_mut;

use windows_sys::Win32::System::Registry::*;

use crate::everything::wide;

const KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
const VALUE: &str = "WizEverything";

fn open(write: bool) -> Option<HKEY> {
    let mut h: HKEY = null_mut();
    let access = if write { KEY_SET_VALUE | KEY_QUERY_VALUE } else { KEY_QUERY_VALUE };
    let k = wide(KEY);
    let r = unsafe { RegOpenKeyExW(HKEY_CURRENT_USER, k.as_ptr(), 0, access, &mut h) };
    (r == 0).then_some(h)
}

pub fn command() -> String {
    let exe = std::env::current_exe().map(|p| p.display().to_string()).unwrap_or_default();
    format!("\"{exe}\" --background")
}

pub fn is_enabled() -> bool {
    let Some(h) = open(false) else { return false };
    let v = wide(VALUE);
    let mut buf = vec![0u16; 1024];
    let mut len = (buf.len() * 2) as u32;
    let r = unsafe { RegQueryValueExW(h, v.as_ptr(), null_mut(), null_mut(), buf.as_mut_ptr() as *mut u8, &mut len) };
    unsafe { RegCloseKey(h) };
    if r != 0 {
        return false;
    }
    let s = String::from_utf16_lossy(&buf[..(len as usize / 2)]);
    s.trim_end_matches('\0') == command()
}

pub fn set(enabled: bool) -> Result<(), String> {
    let h = open(true).ok_or("No se pudo abrir la clave Run")?;
    let v = wide(VALUE);
    let r = if enabled {
        let data = wide(&command());
        unsafe { RegSetValueExW(h, v.as_ptr(), 0, REG_SZ, data.as_ptr() as *const u8, (data.len() * 2) as u32) }
    } else {
        unsafe { RegDeleteValueW(h, v.as_ptr()) }
    };
    unsafe { RegCloseKey(h) };
    if r == 0 || (!enabled && r == 2) { Ok(()) } else { Err(format!("Error de registro {r}")) }
}
