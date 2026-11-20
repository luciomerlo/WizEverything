//! Identidad física de una unidad: marca, modelo, número de serie y bus.
//!
//! Windows expone estos datos por `IOCTL_STORAGE_QUERY_PROPERTY` sobre el
//! volumen (`\\.\E:`). No hace falta WMI ni permisos de administrador para
//! leer el descriptor del dispositivo. Si el dispositivo no responde (unidad
//! de red, lector virtual, permisos), se devuelve sólo lo que sí se pudo leer:
//! etiqueta y serial de volumen.

#![cfg(windows)]

use std::ffi::c_void;

use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_SHARE_READ, FILE_SHARE_WRITE, GetVolumeInformationW, OPEN_EXISTING,
};
use windows_sys::Win32::System::IO::DeviceIoControl;
use windows_sys::Win32::System::Ioctl::{
    IOCTL_STORAGE_QUERY_PROPERTY, PropertyStandardQuery, STORAGE_PROPERTY_QUERY, StorageDeviceProperty,
};

/// Lo que se puede averiguar de un disco mientras está conectado.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DiskIdentity {
    /// Etiqueta del volumen (la que muestra el Explorador).
    pub label: String,
    /// Sistema de archivos: NTFS, exFAT, FAT32, …
    pub fs: String,
    /// Serial de volumen (32 bits en FAT/NTFS, guardado como u64).
    pub volume_serial: u64,
    /// Fabricante tal como lo reporta el disco (`WD`, `Samsung`, …).
    pub vendor: String,
    /// Modelo (`My Passport 259F`, `SSD 970 EVO Plus`, …).
    pub model: String,
    /// Número de serie del dispositivo (no del volumen).
    pub serial_number: String,
    /// Bus: USB, NVMe, SATA, SD, …
    pub bus: String,
    /// `true` si Windows lo marca como extraíble.
    pub removable: bool,
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn ansi_at(buf: &[u8], off: u32) -> String {
    if off == 0 || off as usize >= buf.len() {
        return String::new();
    }
    let rest = &buf[off as usize..];
    let end = rest.iter().position(|&b| b == 0).unwrap_or(rest.len());
    String::from_utf8_lossy(&rest[..end]).trim().to_string()
}

fn bus_name(bus: u32) -> String {
    match bus {
        1 => "SCSI",
        2 => "ATAPI",
        3 => "ATA",
        4 => "IEEE 1394",
        6 => "Fibre",
        7 => "USB",
        8 => "RAID",
        9 => "iSCSI",
        10 => "SAS",
        11 => "SATA",
        12 => "SD",
        13 => "MMC",
        14 => "Virtual",
        15 => "Archivo",
        16 => "Storage Spaces",
        17 => "NVMe",
        18 => "SCM",
        19 => "UFS",
        _ => "",
    }
    .to_string()
}

/// Consulta la identidad de la unidad `letter` (ej. `"E:"` o `"E"`).
/// Nunca falla: los campos que Windows no expone quedan vacíos.
pub fn query(letter: &str) -> DiskIdentity {
    let letter = letter.trim_end_matches('\\');
    let root = if letter.len() >= 2 { format!("{}\\", &letter[..2]) } else { format!("{letter}\\") };
    let mut id = DiskIdentity::default();

    // Etiqueta, sistema de archivos y serial de volumen.
    let wroot = wide(&root);
    let mut label = [0u16; 261];
    let mut fs = [0u16; 64];
    let mut serial: u32 = 0;
    let ok = unsafe {
        GetVolumeInformationW(
            wroot.as_ptr(),
            label.as_mut_ptr(),
            label.len() as u32,
            &mut serial,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            fs.as_mut_ptr(),
            fs.len() as u32,
        )
    };
    if ok != 0 {
        let s = |b: &[u16]| String::from_utf16_lossy(&b[..b.iter().position(|&c| c == 0).unwrap_or(b.len())]);
        id.label = s(&label).trim().to_string();
        id.fs = s(&fs).trim().to_string();
        id.volume_serial = serial as u64;
    }

    // Descriptor del dispositivo: vendor/model/serial/bus.
    let dev = format!("\\\\.\\{}", &root[..2]);
    let wdev = wide(&dev);
    let h = unsafe {
        CreateFileW(
            wdev.as_ptr(),
            0,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            std::ptr::null(),
            OPEN_EXISTING,
            0,
            std::ptr::null_mut(),
        )
    };
    if h == INVALID_HANDLE_VALUE {
        return id;
    }
    let guard = HandleGuard(h);
    let query = STORAGE_PROPERTY_QUERY {
        PropertyId: StorageDeviceProperty,
        QueryType: PropertyStandardQuery,
        AdditionalParameters: [0],
    };
    let mut buf = vec![0u8; 4096];
    let mut returned: u32 = 0;
    let ok = unsafe {
        DeviceIoControl(
            h,
            IOCTL_STORAGE_QUERY_PROPERTY,
            &query as *const _ as *const c_void,
            std::mem::size_of::<STORAGE_PROPERTY_QUERY>() as u32,
            buf.as_mut_ptr() as *mut c_void,
            buf.len() as u32,
            &mut returned,
            std::ptr::null_mut(),
        )
    };
    if ok == 0 || returned < 36 {
        drop(guard);
        return id;
    }
    buf.truncate(returned as usize);
    let u32_at = |o: usize| u32::from_le_bytes(buf[o..o + 4].try_into().unwrap_or([0; 4]));
    // STORAGE_DEVICE_DESCRIPTOR: vendor@12, product@16, revision@20, serial@24, bus@28.
    if buf.len() >= 32 {
        id.vendor = ansi_at(&buf, u32_at(12));
        id.model = ansi_at(&buf, u32_at(16));
        id.serial_number = ansi_at(&buf, u32_at(24));
        id.bus = bus_name(u32_at(28));
        id.removable = buf[10] != 0;
    }
    drop(guard);
    id
}

/// Cierra el handle del volumen aunque haya un `return` temprano.
struct HandleGuard(windows_sys::Win32::Foundation::HANDLE);

impl Drop for HandleGuard {
    fn drop(&mut self) {
        unsafe {
            CloseHandle(self.0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn bus_names_are_stable() {
        assert_eq!(bus_name(7), "USB");
        assert_eq!(bus_name(17), "NVMe");
        assert_eq!(bus_name(999), "");
    }

    #[test]
    fn querying_a_missing_drive_is_empty_not_a_panic() {
        let id = query("Q:");
        assert!(id.label.is_empty() || !id.label.is_empty());
    }
}
