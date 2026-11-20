//! Capa común de metadatos Win32 para los analizadores.
//!
//! Todo lo que necesita hablar con el kernel vive acá, separado del adaptador
//! de Everything y de la UI:
//! - **Tamaño físico real** (`GetCompressedFileSizeW`): clusters realmente
//!   asignados en disco, distinto del tamaño lógico para archivos *sparse*,
//!   comprimidos NTFS/LZX o *placeholders* de la nube.
//! - **FRN** (`FILE_ID_INFO` vía `GetFileInformationByHandleEx`): identidad
//!   única de archivo por volumen. Sirve para no sumar dos veces los *hardlinks*
//!   y para cortar bucles de *junctions/symlinks*.
//! - **Atributos NTFS**: reparse point, offline, sparse, pinned/unpinned.
//! - **Penalidad de seek** (`IOCTL_STORAGE_QUERY_PROPERTY`): distingue NVMe/SSD
//!   de HDD mecánico para programar la concurrencia de I/O.

#![cfg(windows)]

use std::ffi::{OsStr, c_void};
use std::io;
use std::os::windows::ffi::OsStrExt;
use std::path::Path;

use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::Storage::FileSystem::{
    BY_HANDLE_FILE_INFORMATION, CreateFileW, FILE_FLAG_BACKUP_SEMANTICS, FILE_ID_INFO,
    FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, FileIdInfo,
    GetCompressedFileSizeW, GetFileInformationByHandle, GetFileInformationByHandleEx,
    OPEN_EXISTING,
};
use windows_sys::Win32::System::IO::DeviceIoControl;
use windows_sys::Win32::System::Ioctl::{
    DEVICE_SEEK_PENALTY_DESCRIPTOR, IOCTL_STORAGE_QUERY_PROPERTY, PropertyStandardQuery,
    STORAGE_PROPERTY_QUERY, StorageDeviceSeekPenaltyProperty,
};

// ---- Atributos NTFS que nos importan -------------------------------------
pub const ATTR_SPARSE_FILE: u32 = 0x0000_0200;
pub const ATTR_REPARSE_POINT: u32 = 0x0000_0400;
pub const ATTR_OFFLINE: u32 = 0x0000_1000;
pub const ATTR_PINNED: u32 = 0x0008_0000;
pub const ATTR_UNPINNED: u32 = 0x0010_0000;
pub const ATTR_RECALL_ON_DATA_ACCESS: u32 = 0x0040_0000;
pub const ATTR_RECALL_ON_OPEN: u32 = 0x0004_0000;

/// Identidad física de un archivo dentro de un volumen.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct FileId {
    /// Número de serie del volumen (64 bits, `FILE_ID_INFO.VolumeSerialNumber`).
    pub volume_serial: u64,
    /// Identificador único del archivo dentro del volumen (128 bits, FRN).
    pub file_id: u128,
}

/// Metadatos físicos combinados de un archivo.
#[derive(Debug, Clone)]
pub struct PhysMeta {
    /// Tamaño lógico (lo que reporta Everything / `metadata().len()`).
    pub logical_size: u64,
    /// Tamaño físico real en disco, si se pudo consultar.
    pub physical_size: Option<u64>,
    /// Máscara de atributos NTFS (`FILE_ATTRIBUTE_*`).
    pub attributes: u32,
    /// Identidad física, si se pudo abrir el archivo para consultarla.
    pub file_id: Option<FileId>,
}

impl PhysMeta {
    pub fn is_reparse_point(&self) -> bool {
        self.attributes & ATTR_REPARSE_POINT != 0
    }
    pub fn is_offline(&self) -> bool {
        self.attributes & ATTR_OFFLINE != 0
    }
    pub fn is_sparse(&self) -> bool {
        self.attributes & ATTR_SPARSE_FILE != 0
    }
    pub fn is_pinned(&self) -> bool {
        self.attributes & ATTR_PINNED != 0
    }
    pub fn is_unpinned(&self) -> bool {
        self.attributes & ATTR_UNPINNED != 0
    }
    /// `true` si el consumo físico es menor que el lógico (sparse/compresión/nube).
    pub fn is_thinner_than_logical(&self) -> bool {
        matches!(self.physical_size, Some(p) if p < self.logical_size)
    }
}

/// RAII para cerrar el handle aunque haya un `return` temprano.
struct OwnedHandle(HANDLE);
impl Drop for OwnedHandle {
    fn drop(&mut self) {
        unsafe {
            CloseHandle(self.0);
        }
    }
}

fn wide(path: &Path) -> Vec<u16> {
    path.as_os_str().encode_wide().chain(std::iter::once(0)).collect()
}

/// Abre un handle de atributos sobre `path` (sirve para archivos y carpetas).
pub(crate) fn open_attrs(path: &Path) -> io::Result<OwnedHandle> {
    let w = wide(path);
    let h = unsafe {
        CreateFileW(
            w.as_ptr(),
            FILE_READ_ATTRIBUTES,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            std::ptr::null(),
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS,
            std::ptr::null_mut(),
        )
    };
    if h == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    Ok(OwnedHandle(h))
}

/// Tamaño físico real (clusters asignados) de un archivo.
///
/// Para archivos normales coincide (salvo redondeo al cluster) con el lógico;
/// para *sparse*, comprimidos NTFS o *placeholders* de la nube es menor.
pub fn physical_size(path: &Path) -> io::Result<u64> {
    let w = wide(path);
    let mut high: u32 = 0;
    let low = unsafe { GetCompressedFileSizeW(w.as_ptr(), &mut high) };
    if low == u32::MAX {
        // Puede ser error o un tamaño legítimo; desambiguamos con GetLastError.
        let err = unsafe { GetLastError() };
        if err != 0 {
            return Err(io::Error::from_raw_os_error(err as i32));
        }
    }
    Ok(((high as u64) << 32) | low as u64)
}

/// Identidad física (FRN de 128 bits + serie de volumen) de un archivo.
pub fn file_id(path: &Path) -> io::Result<FileId> {
    let handle = open_attrs(path)?;
    let mut info: FILE_ID_INFO = unsafe { std::mem::zeroed() };
    let ok = unsafe {
        GetFileInformationByHandleEx(
            handle.0,
            FileIdInfo,
            &mut info as *mut _ as *mut c_void,
            std::mem::size_of::<FILE_ID_INFO>() as u32,
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(FileId {
        volume_serial: info.VolumeSerialNumber,
        file_id: u128::from_le_bytes(info.FileId.Identifier),
    })
}

/// Cantidad de enlaces (hardlinks) que tiene un archivo.
pub fn hard_link_count(path: &Path) -> io::Result<u32> {
    let handle = open_attrs(path)?;
    let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
    let ok = unsafe { GetFileInformationByHandle(handle.0, &mut info) };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(info.nNumberOfLinks)
}

/// Atributos NTFS crudos de `path`.
pub fn attributes(path: &Path) -> io::Result<u32> {
    let md = std::fs::metadata(path)?;
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        return Ok(md.file_attributes());
    }
    #[allow(unreachable_code)]
    Ok(0)
}

/// Reúne tamaño lógico, físico, atributos y FRN en una sola pasada.
///
/// Los campos opcionales no fallan el resultado global: si no se puede abrir el
/// archivo (bloqueado, permisos), se devuelven `None` en lo que dependa del handle.
pub fn probe(path: &Path) -> io::Result<PhysMeta> {
    let md = std::fs::metadata(path)?;
    let logical_size = md.len();
    let attributes = {
        use std::os::windows::fs::MetadataExt;
        md.file_attributes()
    };
    let physical_size = physical_size(path).ok();
    let file_id = file_id(path).ok();
    Ok(PhysMeta {
        logical_size,
        physical_size,
        attributes,
        file_id,
    })
}

/// `true` si el dispositivo de la unidad `drive` (ej. `'C'`) es mecánico
/// (incurre penalidad de seek). Devuelve `None` si no se pudo consultar.
pub fn incurs_seek_penalty(drive: char) -> Option<bool> {
    let volume: Vec<u16> = format!("\\\\.\\{}:", drive.to_ascii_uppercase())
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let h = unsafe {
        CreateFileW(
            volume.as_ptr(),
            0,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            std::ptr::null(),
            OPEN_EXISTING,
            0,
            std::ptr::null_mut(),
        )
    };
    if h == INVALID_HANDLE_VALUE {
        return None;
    }
    let _guard = OwnedHandle(h);

    let query = STORAGE_PROPERTY_QUERY {
        PropertyId: StorageDeviceSeekPenaltyProperty,
        QueryType: PropertyStandardQuery,
        AdditionalParameters: [0],
    };
    let mut desc: DEVICE_SEEK_PENALTY_DESCRIPTOR = unsafe { std::mem::zeroed() };
    let mut returned: u32 = 0;
    let ok = unsafe {
        DeviceIoControl(
            h,
            IOCTL_STORAGE_QUERY_PROPERTY,
            &query as *const _ as *const c_void,
            std::mem::size_of::<STORAGE_PROPERTY_QUERY>() as u32,
            &mut desc as *mut _ as *mut c_void,
            std::mem::size_of::<DEVICE_SEEK_PENALTY_DESCRIPTOR>() as u32,
            &mut returned,
            std::ptr::null_mut(),
        )
    };
    if ok == 0 {
        return None;
    }
    Some(desc.IncursSeekPenalty)
}

/// Extrae la letra de unidad de una ruta (`C:\...` -> `'C'`).
pub fn drive_letter(path: &Path) -> Option<char> {
    let s = path.to_string_lossy();
    let mut chars = s.chars();
    match (chars.next(), chars.next()) {
        (Some(c), Some(':')) if c.is_ascii_alphabetic() => Some(c.to_ascii_uppercase()),
        _ => None,
    }
}

/// Conveniencia: `incurs_seek_penalty` a partir de una ruta.
pub fn path_incurs_seek_penalty(path: &Path) -> Option<bool> {
    drive_letter(path).and_then(incurs_seek_penalty)
}

#[allow(dead_code)]
fn _assert_osstr(_: &OsStr) {}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn temp_file(bytes: &[u8]) -> std::path::PathBuf {
        let mut p = std::env::temp_dir();
        let name = format!(
            "wize_winphys_{}_{}.bin",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        p.push(name);
        let mut f = std::fs::File::create(&p).unwrap();
        f.write_all(bytes).unwrap();
        f.flush().unwrap();
        p
    }

    #[test]
    fn probe_reports_logical_and_physical() {
        let content = b"hola mundo, metadatos fisicos";
        let p = temp_file(content);
        let m = probe(&p).unwrap();
        assert_eq!(m.logical_size, content.len() as u64);
        // El tamaño físico es multiplo de cluster y >= lógico (salvo compresión).
        if let Some(phys) = m.physical_size {
            assert!(phys >= m.logical_size || phys == 0);
        }
        assert!(!m.is_sparse());
        assert!(!m.is_reparse_point());
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn file_id_is_stable_and_nonzero() {
        let p = temp_file(b"identidad estable");
        let a = file_id(&p).unwrap();
        let b = file_id(&p).unwrap();
        assert_eq!(a, b);
        assert_ne!(a.file_id, 0);
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn attributes_of_normal_file() {
        let p = temp_file(b"x");
        let attrs = attributes(&p).unwrap();
        assert_eq!(attrs & ATTR_REPARSE_POINT, 0);
        assert_eq!(attrs & ATTR_SPARSE_FILE, 0);
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn drive_letter_parsing() {
        assert_eq!(drive_letter(Path::new("C:\\Users\\x")), Some('C'));
        assert_eq!(drive_letter(Path::new("d:/data")), Some('D'));
        assert_eq!(drive_letter(Path::new("\\\\server\\share")), None);
    }

    #[test]
    fn seek_penalty_query_does_not_panic() {
        // Puede devolver None en algunos entornos; solo verificamos que no explote.
        let _ = incurs_seek_penalty('C');
    }

    #[test]
    fn physmeta_flags() {
        let m = PhysMeta {
            logical_size: 1000,
            physical_size: Some(100),
            attributes: ATTR_SPARSE_FILE | ATTR_OFFLINE,
            file_id: None,
        };
        assert!(m.is_sparse());
        assert!(m.is_offline());
        assert!(m.is_thinner_than_logical());
        assert!(!m.is_reparse_point());
    }
}
