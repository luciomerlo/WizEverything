//! Replace verified duplicate copies with NTFS hard links to the kept copy.
//!
//! For each marked copy: re-check attributes and volume, re-verify size and the
//! full content of both files (byte by byte, plus xxh3 hashes) while the kept
//! copy is held open without write sharing, create a hard link to the kept copy
//! under a temporary name in the copy's folder, then atomically rename it over
//! the copy (MoveFileExW + MOVEFILE_REPLACE_EXISTING). Any failure leaves the
//! original copy untouched and removes the temporary link.

use std::collections::{HashMap, HashSet};
use std::io::Read;
use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
use std::os::windows::io::AsRawHandle;
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};

use xxhash_rust::xxh3::Xxh3;

use crate::dup::DupGroup;
use crate::everything::wide;
use crate::i18n::tr;
use crate::rules::Rules;

const READONLY: u32 = 0x1;
const SYSTEM: u32 = 0x4;
const DIRECTORY: u32 = 0x10;
const REPARSE_POINT: u32 = 0x400;
const OFFLINE: u32 = 0x1000;
const RECALL_ON_OPEN: u32 = 0x4_0000;
const PINNED: u32 = 0x8_0000;
const UNPINNED: u32 = 0x10_0000;
const RECALL_ON_DATA_ACCESS: u32 = 0x40_0000;
const SHARE_READ: u32 = 0x1;
const SHARE_DELETE: u32 = 0x4;

/// One marked copy to replace by a link to `keep`.
#[derive(Clone, Debug)]
pub struct Job {
    pub keep: String,
    pub target: String,
    pub size: u64,
}

#[derive(Default, Debug)]
pub struct Report {
    pub linked: Vec<String>,
    /// Already the same physical file (nothing to do).
    pub already: Vec<String>,
    pub failed: Vec<(String, String)>,
    /// Bytes actually released (copies that had no other hard link).
    pub freed: u64,
}

/// Drive part of a path ("c:" or "\\server\share"), lowercase.
fn drive(path: &str) -> String {
    let p = path.to_lowercase();
    if let Some(rest) = p.strip_prefix("\\\\") {
        let mut it = rest.splitn(3, '\\');
        return format!("\\\\{}\\{}", it.next().unwrap_or(""), it.next().unwrap_or(""));
    }
    p.split('\\').next().unwrap_or("").to_string()
}

/// Which marked copies can be replaced, and why the others can't.
/// Only verified (hash-confirmed) file groups qualify.
pub fn plan(groups: &[DupGroup], marked: &HashSet<String>, rules: &Rules) -> (Vec<Job>, Vec<(String, String)>) {
    let mut jobs = Vec::new();
    let mut refused = Vec::new();
    for g in groups {
        let targets: Vec<&String> = g.files.iter().map(|f| &f.path).filter(|p| marked.contains(*p)).collect();
        if targets.is_empty() {
            continue;
        }
        let why = if g.folder {
            Some(tr("carpetas: solo se enlazan archivos", "folders: only files can be linked"))
        } else if g.versions {
            Some(tr("versiones: el contenido puede diferir", "versions: content may differ"))
        } else if !g.verified {
            Some(tr("sin verificar por contenido (modo rápido)", "not verified by content (quick mode)"))
        } else {
            None
        };
        if let Some(w) = why {
            refused.extend(targets.into_iter().map(|t| (t.clone(), w.to_string())));
            continue;
        }
        let kept: Vec<&String> = g.files.iter().map(|f| &f.path).filter(|p| !marked.contains(*p)).collect();
        for t in targets {
            if rules.is_protected(t) {
                refused.push((t.clone(), tr("carpeta protegida", "protected folder").to_string()));
                continue;
            }
            match kept.iter().find(|k| drive(k) == drive(t)) {
                Some(k) => jobs.push(Job { keep: (*k).clone(), target: t.clone(), size: g.size }),
                None if kept.is_empty() => {
                    refused.push((t.clone(), tr("todas las copias están marcadas", "every copy is marked").to_string()))
                }
                None => refused.push((
                    t.clone(),
                    tr("la copia conservada está en otra unidad", "the kept copy is on another volume").to_string(),
                )),
            }
        }
    }
    (jobs, refused)
}

fn attrs(path: &str) -> Result<u32, String> {
    use windows_sys::Win32::Storage::FileSystem::{GetFileAttributesW, INVALID_FILE_ATTRIBUTES};
    let a = unsafe { GetFileAttributesW(wide(path).as_ptr()) };
    if a == INVALID_FILE_ATTRIBUTES {
        return Err(format!("{}: {}", tr("no se puede leer", "cannot read"), std::io::Error::last_os_error()));
    }
    Ok(a)
}

/// Refuse anything that is not a plain local file.
fn check_plain(path: &str) -> Result<(), String> {
    let a = attrs(path)?;
    let why = if a & DIRECTORY != 0 {
        tr("es una carpeta", "is a folder")
    } else if a & (OFFLINE | RECALL_ON_OPEN | RECALL_ON_DATA_ACCESS | PINNED | UNPINNED) != 0 {
        tr("archivo en la nube (marcador de posición)", "cloud file (placeholder)")
    } else if a & REPARSE_POINT != 0 {
        tr("punto de reanálisis (enlace o nube)", "reparse point (link or cloud)")
    } else if a & READONLY != 0 {
        tr("archivo de solo lectura", "read-only file")
    } else if a & SYSTEM != 0 {
        tr("archivo de sistema", "system file")
    } else {
        return Ok(());
    };
    Err(format!("{why}: {path}"))
}

/// Volume mount point of a path ("C:\").
fn volume_of(path: &str) -> Option<String> {
    use windows_sys::Win32::Storage::FileSystem::GetVolumePathNameW;
    let mut buf = [0u16; 1024];
    let ok = unsafe { GetVolumePathNameW(wide(path).as_ptr(), buf.as_mut_ptr(), buf.len() as u32) };
    (ok != 0).then(|| String::from_utf16_lossy(&buf[..buf.iter().position(|&c| c == 0).unwrap_or(0)]))
}

fn fs_name(root: &str) -> Option<String> {
    use windows_sys::Win32::Storage::FileSystem::GetVolumeInformationW;
    let mut name = [0u16; 64];
    let ok = unsafe {
        GetVolumeInformationW(
            wide(root).as_ptr(),
            std::ptr::null_mut(),
            0,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            name.as_mut_ptr(),
            name.len() as u32,
        )
    };
    (ok != 0).then(|| String::from_utf16_lossy(&name[..name.iter().position(|&c| c == 0).unwrap_or(0)]))
}

/// Both paths on one NTFS volume.
fn check_same_ntfs(a: &str, b: &str) -> Result<(), String> {
    let (Some(va), Some(vb)) = (volume_of(a), volume_of(b)) else {
        return Err(tr("no se pudo determinar el volumen", "could not determine the volume").into());
    };
    if !va.eq_ignore_ascii_case(&vb) {
        return Err(format!("{} ({va} ≠ {vb})", tr("están en volúmenes distintos", "they are on different volumes")));
    }
    match fs_name(&va) {
        Some(fs) if fs.eq_ignore_ascii_case("NTFS") => Ok(()),
        Some(fs) => Err(format!("{} ({fs})", tr("el volumen no es NTFS", "the volume is not NTFS"))),
        None => Err(tr("no se pudo leer el sistema de archivos", "could not read the file system").into()),
    }
}

/// (volume serial, file index, link count, size) of an open file.
struct Info {
    id: (u32, u64),
    links: u32,
    size: u64,
}

fn info(f: &std::fs::File) -> Option<Info> {
    use windows_sys::Win32::Storage::FileSystem::{BY_HANDLE_FILE_INFORMATION, GetFileInformationByHandle};
    let mut i: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
    let ok = unsafe { GetFileInformationByHandle(f.as_raw_handle() as _, &mut i) };
    (ok != 0).then(|| Info {
        id: (i.dwVolumeSerialNumber, ((i.nFileIndexHigh as u64) << 32) | i.nFileIndexLow as u64),
        links: i.nNumberOfLinks,
        size: ((i.nFileSizeHigh as u64) << 32) | i.nFileSizeLow as u64,
    })
}

/// (volume serial, file index): equal for two names of one physical file.
pub fn file_id(path: &str) -> Option<(u32, u64)> {
    info(&std::fs::File::open(path).ok()?).map(|i| i.id)
}

/// Number of names (hard links) the file has.
#[cfg_attr(not(test), allow(dead_code))]
pub fn link_count(path: &str) -> Option<u32> {
    info(&std::fs::File::open(path).ok()?).map(|i| i.links)
}

/// Open for reading while denying writers (deletes/renames are still allowed,
/// so the link can be renamed into place).
fn open_locked(path: &str) -> Result<std::fs::File, String> {
    std::fs::OpenOptions::new()
        .read(true)
        .share_mode(SHARE_READ | SHARE_DELETE)
        .open(path)
        .map_err(|e| format!("{} {path}: {e}", tr("no se pudo abrir", "could not open")))
}

/// Same bytes from start to end (also compares xxh3 hashes of both).
fn same_content(a: &mut std::fs::File, b: &mut std::fs::File, size: u64) -> Result<bool, String> {
    let (mut ha, mut hb) = (Xxh3::new(), Xxh3::new());
    let mut ba = vec![0u8; 1 << 20];
    let mut bb = vec![0u8; 1 << 20];
    let mut total = 0u64;
    loop {
        let na = read_full(a, &mut ba).map_err(|e| e.to_string())?;
        let nb = read_full(b, &mut bb).map_err(|e| e.to_string())?;
        if na != nb || ba[..na] != bb[..nb] {
            return Ok(false);
        }
        if na == 0 {
            break;
        }
        ha.update(&ba[..na]);
        hb.update(&bb[..nb]);
        total += na as u64;
    }
    Ok(total == size && ha.digest128() == hb.digest128())
}

fn read_full(f: &mut std::fs::File, buf: &mut [u8]) -> std::io::Result<usize> {
    let mut n = 0;
    while n < buf.len() {
        match f.read(&mut buf[n..])? {
            0 => break,
            r => n += r,
        }
    }
    Ok(n)
}

pub enum Outcome {
    /// Replaced; bytes released.
    Linked(u64),
    Already,
}

/// Replace `job.target` with a hard link to `job.keep`. On any error the target
/// is left as it was.
pub fn replace_one(job: &Job) -> Result<Outcome, String> {
    use windows_sys::Win32::Storage::FileSystem::{CreateHardLinkW, DeleteFileW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW};
    check_plain(&job.keep)?;
    check_plain(&job.target)?;
    check_same_ntfs(&job.keep, &job.target)?;

    // The kept copy stays open (no write sharing) until the link is in place.
    let mut keep = open_locked(&job.keep)?;
    let mut target = open_locked(&job.target)?;
    let (Some(ki), Some(ti)) = (info(&keep), info(&target)) else {
        return Err(tr("no se pudo leer la identidad del archivo", "could not read the file identity").into());
    };
    if ki.id == ti.id {
        return Ok(Outcome::Already);
    }
    if ki.id.0 != ti.id.0 {
        return Err(tr("están en volúmenes distintos", "they are on different volumes").into());
    }
    if ki.size != job.size || ti.size != job.size {
        return Err(tr("el tamaño cambió desde la búsqueda", "the size changed since the search").into());
    }
    let stamp = std::fs::metadata(&job.target).map(|m| m.last_write_time()).ok();
    if !same_content(&mut keep, &mut target, job.size)? {
        return Err(tr("el contenido ya no es idéntico", "the content is no longer identical").into());
    }
    let target_links = ti.links;
    drop(target);
    // Written to between the comparison and now?
    if std::fs::metadata(&job.target).map(|m| m.last_write_time()).ok() != stamp || attrs(&job.target)? & READONLY != 0 {
        return Err(tr("el archivo cambió durante la verificación", "the file changed during verification").into());
    }

    // Link under a temporary name in the target's folder (same volume), then
    // swap it over the target in one atomic rename.
    static NEXT: AtomicU32 = AtomicU32::new(0);
    let dir = job.target.rsplit_once('\\').map_or("", |(d, _)| d);
    let keep_w = wide(&job.keep);
    let mut tmp = String::new();
    for _ in 0..100 {
        tmp = format!("{dir}\\~wizlink_{}_{}.tmp", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed));
        if unsafe { CreateHardLinkW(wide(&tmp).as_ptr(), keep_w.as_ptr(), std::ptr::null()) } != 0 {
            break;
        }
        let e = std::io::Error::last_os_error();
        if !matches!(e.raw_os_error(), Some(80 | 183)) {
            return Err(format!("CreateHardLink: {e}"));
        }
        tmp.clear();
    }
    if tmp.is_empty() {
        return Err("CreateHardLink: no free temporary name".into());
    }
    let ok = unsafe { MoveFileExW(wide(&tmp).as_ptr(), wide(&job.target).as_ptr(), MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH) };
    if ok == 0 {
        let e = std::io::Error::last_os_error();
        unsafe { DeleteFileW(wide(&tmp).as_ptr()) }; // only removes the extra name
        return Err(format!("MoveFileEx: {e}"));
    }
    drop(keep);
    if file_id(&job.target) != Some(ki.id) {
        return Err(tr("el enlace no se pudo comprobar", "the link could not be confirmed").into());
    }
    Ok(Outcome::Linked(if target_links <= 1 { job.size } else { 0 }))
}

/// Run every job in order; `done` counts finished jobs (for progress).
pub fn run(jobs: &[Job], done: &AtomicUsize) -> Report {
    let mut r = Report::default();
    for j in jobs {
        match replace_one(j) {
            Ok(Outcome::Linked(freed)) => {
                r.freed += freed;
                r.linked.push(j.target.clone());
            }
            Ok(Outcome::Already) => r.already.push(j.target.clone()),
            Err(e) => r.failed.push((j.target.clone(), e)),
        }
        done.fetch_add(1, Ordering::Relaxed);
    }
    r
}

/// Drop linked copies from the groups (a group whose copies are all one
/// physical file is no longer a duplicate).
pub fn prune(groups: &mut Vec<DupGroup>, linked: &[String]) {
    let gone: HashSet<&String> = linked.iter().collect();
    for g in groups.iter_mut() {
        g.files.retain(|f| !gone.contains(&f.path));
    }
    groups.retain(|g| g.files.len() > 1);
}

/// Group refusals by reason for a compact summary.
pub fn summarize(refused: &[(String, String)]) -> Vec<(String, usize)> {
    let mut by: HashMap<&str, usize> = HashMap::new();
    for (_, w) in refused {
        *by.entry(w.as_str()).or_default() += 1;
    }
    let mut v: Vec<(String, usize)> = by.into_iter().map(|(w, n)| (w.to_string(), n)).collect();
    v.sort_by(|a, b| b.1.cmp(&a.1));
    v
}
