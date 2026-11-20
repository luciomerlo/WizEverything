//! Módulo 1 (núcleo) — Fusión segura de duplicados a NTFS hardlinks.
//!
//! Dado un archivo maestro `A` y un duplicado `B` en el **mismo volumen NTFS**,
//! convierte `B` en un hardlink a `A` preservando la ruta original, de forma
//! atómica:
//!
//! 1. Se crea un hardlink temporal `B.tmp` apuntando a `A` con `CreateHardLinkW`.
//! 2. Se reemplaza `B` por ese temporal con `MoveFileExW(MOVEFILE_REPLACE_EXISTING)`.
//! 3. Si el reemplazo falla (archivo bloqueado), se borra el temporal y se
//!    devuelve el error para que el llamador decida (p.ej. mandar `B` a la papelera).
//!
//! Toda operación tiene un `dry_run` previo: [`plan`] valida volumen, identidad y
//! espacio recuperable sin tocar el disco.

use std::io;
use std::path::{Path, PathBuf};

use crate::winphys::{self, FileId};

use windows_sys::Win32::Storage::FileSystem::{
    CreateHardLinkW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW,
    DeleteFileW,
};

/// Plan de fusión calculado en modo simulación.
#[derive(Debug, Clone)]
pub struct LinkPlan {
    pub master: PathBuf,
    pub duplicate: PathBuf,
    /// Bytes físicos que se recuperarían al fusionar (tamaño físico del duplicado).
    pub recoverable: u64,
    /// `true` si el duplicado ya es un hardlink del maestro (no hay nada que hacer).
    pub already_linked: bool,
}

/// Error de seguridad: la operación no es válida para este par de archivos.
#[derive(Debug)]
pub enum MergeError {
    Io(io::Error),
    /// Los archivos están en volúmenes distintos: no se puede hardlinkear.
    DifferentVolume,
    /// El duplicado ya apunta físicamente al maestro.
    AlreadyLinked,
    /// El duplicado no existe.
    Missing,
}

impl std::fmt::Display for MergeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MergeError::Io(e) => write!(f, "error de E/S: {e}"),
            MergeError::DifferentVolume => write!(f, "archivos en volúmenes distintos"),
            MergeError::AlreadyLinked => write!(f, "el archivo ya es un hardlink del maestro"),
            MergeError::Missing => write!(f, "el duplicado no existe"),
        }
    }
}

impl From<io::Error> for MergeError {
    fn from(e: io::Error) -> Self {
        MergeError::Io(e)
    }
}

fn ids(path: &Path) -> Result<FileId, MergeError> {
    winphys::file_id(path).map_err(MergeError::Io)
}

/// `true` si dos rutas son el mismo archivo físico (mismo FRN y volumen).
pub fn same_file(a: &Path, b: &Path) -> io::Result<bool> {
    match (winphys::file_id(a), winphys::file_id(b)) {
        (Ok(x), Ok(y)) => Ok(x == y),
        (Err(e), _) | (_, Err(e)) => Err(e),
    }
}

/// Calcula el plan de fusión sin tocar el disco.
pub fn plan(master: &Path, duplicate: &Path) -> Result<LinkPlan, MergeError> {
    if !duplicate.exists() {
        return Err(MergeError::Missing);
    }
    let a = ids(master)?;
    let b = ids(duplicate)?;
    if a.volume_serial != b.volume_serial {
        return Err(MergeError::DifferentVolume);
    }
    let already_linked = a.file_id == b.file_id;
    let recoverable = if already_linked {
        0
    } else {
        winphys::physical_size(duplicate).unwrap_or(0)
    };
    Ok(LinkPlan {
        master: master.to_path_buf(),
        duplicate: duplicate.to_path_buf(),
        recoverable,
        already_linked,
    })
}

fn wide(path: &Path) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    path.as_os_str().encode_wide().chain(std::iter::once(0)).collect()
}

/// Crea un hardlink `link` que apunta a `target`.
pub fn create_hardlink(link: &Path, target: &Path) -> io::Result<()> {
    let l = wide(link);
    let t = wide(target);
    let ok = unsafe { CreateHardLinkW(l.as_ptr(), t.as_ptr(), std::ptr::null()) };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Nombre temporal único junto al duplicado (en la misma carpeta y volumen).
fn temp_link_path(duplicate: &Path) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let pid = std::process::id();
    let file = duplicate
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("dup");
    duplicate.with_file_name(format!(".{file}.wizelink.{pid}.{nanos}"))
}

/// Ejecuta la fusión atómica: reemplaza `duplicate` por un hardlink a `master`.
///
/// Es la operación mutativa real; el llamador debe haber corrido [`plan`] antes
/// y estar en modo no-simulación.
pub fn execute(master: &Path, duplicate: &Path) -> Result<(), MergeError> {
    let p = plan(master, duplicate)?;
    if p.already_linked {
        return Err(MergeError::AlreadyLinked);
    }

    let tmp = temp_link_path(duplicate);

    // 1) Hardlink temporal hacia el maestro.
    create_hardlink(&tmp, master)?;

    // 2) Reemplazo atómico del duplicado por el temporal.
    let from = wide(&tmp);
    let to = wide(duplicate);
    let ok = unsafe {
        MoveFileExW(
            from.as_ptr(),
            to.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    };

    if ok == 0 {
        // 3) Limpieza del temporal y propagación del error.
        let err = io::Error::last_os_error();
        unsafe {
            DeleteFileW(from.as_ptr());
        }
        return Err(MergeError::Io(err));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn temp_pair() -> (PathBuf, PathBuf) {
        let base = std::env::temp_dir();
        let tag = format!(
            "{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        let a = base.join(format!("wize_hl_master_{tag}.bin"));
        let b = base.join(format!("wize_hl_dup_{tag}.bin"));
        for p in [&a, &b] {
            let mut f = std::fs::File::create(p).unwrap();
            f.write_all(b"contenido identico para deduplicar").unwrap();
        }
        (a, b)
    }

    #[test]
    fn plan_detects_same_volume_and_recoverable() {
        let (a, b) = temp_pair();
        let p = plan(&a, &b).unwrap();
        assert!(!p.already_linked);
        assert!(p.recoverable > 0);
        std::fs::remove_file(&a).ok();
        std::fs::remove_file(&b).ok();
    }

    #[test]
    fn execute_creates_hardlink_preserving_paths() {
        let (a, b) = temp_pair();
        let before = winphys::hard_link_count(&a).unwrap();

        execute(&a, &b).unwrap();

        // Ambas rutas siguen existiendo y son el mismo archivo físico.
        assert!(a.exists() && b.exists());
        assert!(same_file(&a, &b).unwrap());
        // Subió el contador de hardlinks del maestro.
        assert_eq!(winphys::hard_link_count(&a).unwrap(), before + 1);
        // Y el plan ahora dice que ya están enlazados.
        assert!(plan(&a, &b).unwrap().already_linked);
        // Reintentar es un no-op seguro (error explícito, sin romper nada).
        assert!(matches!(execute(&a, &b), Err(MergeError::AlreadyLinked)));

        std::fs::remove_file(&a).ok();
        std::fs::remove_file(&b).ok();
    }

    #[test]
    fn editing_one_link_affects_the_other() {
        let (a, b) = temp_pair();
        execute(&a, &b).unwrap();
        // Al ser el mismo inodo, escribir en `a` se ve en `b`.
        {
            let mut f = std::fs::OpenOptions::new().write(true).open(&a).unwrap();
            f.write_all(b"CAMBIADO").unwrap();
            f.flush().unwrap();
        }
        let data = std::fs::read(&b).unwrap();
        assert_eq!(&data[..8], b"CAMBIADO");
        std::fs::remove_file(&a).ok();
        std::fs::remove_file(&b).ok();
    }

    #[test]
    fn missing_duplicate_is_reported() {
        let (a, _b) = temp_pair();
        let ghost = a.with_file_name("wize_no_existe_xyz.bin");
        assert!(matches!(plan(&a, &ghost), Err(MergeError::Missing)));
        std::fs::remove_file(&a).ok();
    }
}
