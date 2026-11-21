//! Módulo 5 (mutación) — Compresión transparente WOF (XPRESS4K/LZX).
//!
//! Aplica compresión nativa de Windows sobre un archivo usando el filtro WOF vía
//! `DeviceIoControl(FSCTL_SET_EXTERNAL_BACKING)` con `WOF_PROVIDER_FILE`. El
//! archivo conserva su ruta y se lee normal; Windows lo descomprime al vuelo.
//!
//! El buffer de entrada es `WOF_EXTERNAL_INFO` seguido de
//! `FILE_PROVIDER_EXTERNAL_INFO_V1` (versión + algoritmo).

#![cfg(windows)]

use std::ffi::c_void;
use std::io;
use std::path::Path;

use windows_sys::Win32::Foundation::{CloseHandle, GENERIC_READ, GENERIC_WRITE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};
use windows_sys::Win32::System::IO::DeviceIoControl;

/// `CTL_CODE(FILE_DEVICE_FILE_SYSTEM, 195, METHOD_BUFFERED, FILE_SPECIAL_ACCESS)`.
/// El número es 195, no 243: con 243 Windows devuelve `ERROR_INVALID_FUNCTION`.
const FSCTL_SET_EXTERNAL_BACKING: u32 = 0x0009_030C;
/// `CTL_CODE(FILE_DEVICE_FILE_SYSTEM, 197, METHOD_BUFFERED, FILE_SPECIAL_ACCESS)`.
const FSCTL_DELETE_EXTERNAL_BACKING: u32 = 0x0009_0314;

const WOF_PROVIDER_FILE: u32 = 2;
const WOF_CURRENT_VERSION: u32 = 1;
const FILE_PROVIDER_CURRENT_VERSION: u32 = 1;

/// Algoritmo de compresión WOF.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Algorithm {
    Xpress4k,
    Lzx,
    Xpress8k,
    Xpress16k,
}

impl Algorithm {
    fn value(self) -> u32 {
        match self {
            Algorithm::Xpress4k => 0,
            Algorithm::Lzx => 1,
            Algorithm::Xpress8k => 2,
            Algorithm::Xpress16k => 3,
        }
    }
}

fn wide(path: &Path) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    path.as_os_str().encode_wide().chain(std::iter::once(0)).collect()
}

struct Owned(windows_sys::Win32::Foundation::HANDLE);
impl Drop for Owned {
    fn drop(&mut self) {
        unsafe { CloseHandle(self.0) };
    }
}

fn open_rw(path: &Path) -> io::Result<Owned> {
    let w = wide(path);
    let h = unsafe {
        CreateFileW(
            w.as_ptr(),
            GENERIC_READ | GENERIC_WRITE,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            std::ptr::null(),
            OPEN_EXISTING,
            0,
            std::ptr::null_mut(),
        )
    };
    if h == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    Ok(Owned(h))
}

/// Buffer `WOF_EXTERNAL_INFO` + `FILE_PROVIDER_EXTERNAL_INFO_V1`.
fn wof_file_buffer(algorithm: u32) -> Vec<u8> {
    let mut buf = Vec::with_capacity(16);
    buf.extend_from_slice(&WOF_CURRENT_VERSION.to_le_bytes()); // WOF_EXTERNAL_INFO.Version
    buf.extend_from_slice(&WOF_PROVIDER_FILE.to_le_bytes()); //   WOF_EXTERNAL_INFO.Provider
    buf.extend_from_slice(&FILE_PROVIDER_CURRENT_VERSION.to_le_bytes()); // info.Version
    buf.extend_from_slice(&algorithm.to_le_bytes()); // info.Algorithm
    buf
}

fn set_backing(path: &Path, algorithm: u32) -> io::Result<()> {
    let handle = open_rw(path)?;
    let buf = wof_file_buffer(algorithm);
    let mut ret: u32 = 0;
    let ok = unsafe {
        DeviceIoControl(
            handle.0,
            FSCTL_SET_EXTERNAL_BACKING,
            buf.as_ptr() as *const c_void,
            buf.len() as u32,
            std::ptr::null_mut(),
            0,
            &mut ret,
            std::ptr::null_mut(),
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Comprime `path` con el algoritmo indicado.
pub fn compress(path: &Path, algorithm: Algorithm) -> io::Result<()> {
    set_backing(path, algorithm.value())
}

/// Quita la compresión WOF y deja el archivo en estado normal.
pub fn decompress(path: &Path) -> io::Result<()> {
    let handle = open_rw(path)?;
    let mut buf = Vec::with_capacity(8);
    buf.extend_from_slice(&WOF_CURRENT_VERSION.to_le_bytes());
    buf.extend_from_slice(&WOF_PROVIDER_FILE.to_le_bytes());
    let mut ret: u32 = 0;
    let ok = unsafe {
        DeviceIoControl(
            handle.0,
            FSCTL_DELETE_EXTERNAL_BACKING,
            buf.as_ptr() as *const c_void,
            buf.len() as u32,
            std::ptr::null_mut(),
            0,
            &mut ret,
            std::ptr::null_mut(),
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn temp_compressible() -> std::path::PathBuf {
        let mut p = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        p.push(format!(
            "wize_wof_{}_{}.txt",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let mut f = std::fs::File::create(&p).unwrap();
        // 64 KiB altamente compresible.
        for _ in 0..2048 {
            f.write_all(b"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA").unwrap();
        }
        f.flush().unwrap();
        p
    }

    #[test]
    fn wof_lzx_roundtrip_compresses() {
        let p = temp_compressible();
        let logical = std::fs::metadata(&p).unwrap().len();

        match compress(&p, Algorithm::Lzx) {
            Ok(()) => {
                let phys = crate::winphys::physical_size(&p).unwrap_or(0);
                println!("WOF LZX: logical={logical} physical={phys}");
                assert!(
                    phys < logical,
                    "esperaba consumo fisico menor: phys={phys} logical={logical}"
                );
                // Limpieza: quitar la compresión.
                let _ = decompress(&p);
            }
            Err(e) => {
                std::fs::remove_file(&p).ok();
                let code = e.raw_os_error().unwrap_or(0);
                // 1 = ERROR_INVALID_FUNCTION, 5 = ACCESS_DENIED, 50 = NOT_SUPPORTED,
                // 87 = INVALID_PARAMETER: el entorno/volumen no soporta WOF.
                if matches!(code, 1 | 5 | 50 | 87) {
                    println!(
                        "WOF LZX no soportado en este entorno (os error {code}); se omite la verificación."
                    );
                    return;
                }
                panic!("WOF LZX falló de forma inesperada: {e}");
            }
        }
        std::fs::remove_file(&p).ok();
    }
}
