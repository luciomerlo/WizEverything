//! Duplicate file finder.
//!
//! 1. Candidates from the in-memory index: same size (and, optionally, same name).
//!    Groups are processed largest-potential-savings first.
//! 2. Progressive comparison: all files of a group are read block by block
//!    (64 KB, then growing up to 8 MB); after each block they are split by the
//!    block's hash and any file left alone is dropped immediately. Files that
//!    differ early are never read to the end.
//! 3. Each confirmed group is streamed to the caller as soon as it is found.
//!
//! Hard links (same physical file) are collapsed, and cloud placeholders whose
//! content is not on disk are skipped so the search never triggers downloads.

use std::collections::HashMap;
use std::io::Read;
use std::os::windows::io::AsRawHandle;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use xxhash_rust::xxh3::xxh3_64;

use crate::everything::wide;
use crate::tree::Tree;

const FIRST_BLOCK: usize = 64 * 1024;
const MAX_BLOCK: usize = 8 * 1024 * 1024;
/// Groups larger than this are first split by the hash of their first block,
/// one file at a time, so we never hold thousands of open handles.
const MAX_OPEN: usize = 64;

fn threads() -> usize {
    std::thread::available_parallelism().map_or(4, |n| n.get()).clamp(2, 8)
}

#[derive(Clone)]
pub struct DupFile {
    pub path: String,
    pub modified: u64,
}

#[derive(Clone)]
pub struct DupGroup {
    pub size: u64,
    /// The duplicated items: files, or whole folders when `folder` is true.
    pub files: Vec<DupFile>,
    pub folder: bool,
    /// Number of files inside each folder (folders only).
    pub inner_files: u32,
    /// Content confirmed by hashing (false in quick mode).
    pub verified: bool,
}

impl DupGroup {
    pub fn wasted(&self) -> u64 {
        self.size * (self.files.len() as u64).saturating_sub(1)
    }
    pub fn name(&self) -> &str {
        self.files.first().map_or("", |f| f.path.rsplit('\\').next().unwrap_or(&f.path))
    }
}

#[derive(Default)]
pub struct DupProgress {
    /// 0 = collecting candidates, 1 = comparing contents.
    pub stage: AtomicU8,
    pub groups_done: AtomicU64,
    pub groups_total: AtomicU64,
    pub files_total: AtomicU64,
    pub bytes_done: AtomicU64,
    pub bytes_total: AtomicU64,
    pub skipped: AtomicU64,
    pub cancel: AtomicBool,
}

#[derive(Clone, Copy)]
pub struct DupOptions {
    pub root: u32,
    pub min_size: u64,
    pub same_name: bool,
    /// Advanced mode: confirm by content (hashes). Quick mode only matches the
    /// index (name + size, folder signatures) and never reads a file.
    pub verify: bool,
}

struct Cand {
    path: String,
    modified: u64,
}

/// Attributes that mean the content isn't local (cloud placeholder / offline).
fn is_not_local(path: &str) -> bool {
    use windows_sys::Win32::Storage::FileSystem::{GetFileAttributesW, INVALID_FILE_ATTRIBUTES};
    const OFFLINE: u32 = 0x1000;
    const RECALL_ON_OPEN: u32 = 0x4_0000;
    const RECALL_ON_DATA_ACCESS: u32 = 0x40_0000;
    let a = unsafe { GetFileAttributesW(wide(path).as_ptr()) };
    a == INVALID_FILE_ATTRIBUTES || a & (OFFLINE | RECALL_ON_OPEN | RECALL_ON_DATA_ACCESS) != 0
}

/// (volume serial, file index): identical for hard links to the same file.
fn file_id(f: &std::fs::File) -> Option<(u32, u64)> {
    use windows_sys::Win32::Storage::FileSystem::{BY_HANDLE_FILE_INFORMATION, GetFileInformationByHandle};
    let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
    let ok = unsafe { GetFileInformationByHandle(f.as_raw_handle() as _, &mut info) };
    (ok != 0).then_some((info.dwVolumeSerialNumber, ((info.nFileIndexHigh as u64) << 32) | info.nFileIndexLow as u64))
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

struct Open<'a> {
    cand: &'a Cand,
    file: std::fs::File,
}

/// Compare a set of same-size files; returns the classes of identical files.
fn compare_group<'a>(size: u64, cands: &'a [Cand], p: &DupProgress) -> Vec<Vec<&'a Cand>> {
    // Open (skipping cloud placeholders) and collapse hard links.
    let mut seen = std::collections::HashSet::new();
    let mut open: Vec<Open<'a>> = Vec::with_capacity(cands.len());
    for c in cands {
        if is_not_local(&c.path) {
            p.skipped.fetch_add(1, Ordering::Relaxed);
            p.bytes_done.fetch_add(size, Ordering::Relaxed);
            continue;
        }
        let Ok(file) = std::fs::File::open(&c.path) else {
            p.bytes_done.fetch_add(size, Ordering::Relaxed);
            continue;
        };
        if let Some(id) = file_id(&file)
            && !seen.insert(id)
        {
            p.bytes_done.fetch_add(size, Ordering::Relaxed);
            continue; // another path to a file we already have
        }
        open.push(Open { cand: c, file });
    }

    let mut classes: Vec<Vec<Open<'a>>> = vec![open];
    let mut done: Vec<Vec<&'a Cand>> = Vec::new();
    let mut offset = 0u64;
    let mut block = FIRST_BLOCK;
    let mut buf = vec![0u8; FIRST_BLOCK];
    while !classes.is_empty() {
        if p.cancel.load(Ordering::Relaxed) {
            return Vec::new();
        }
        if offset >= size {
            done.extend(classes.into_iter().map(|c| c.into_iter().map(|o| o.cand).collect()));
            break;
        }
        let want = block.min((size - offset) as usize);
        if buf.len() < want {
            buf.resize(want, 0);
        }
        let mut next: Vec<Vec<Open<'a>>> = Vec::new();
        for class in classes {
            let mut split: HashMap<u64, Vec<Open<'a>>> = HashMap::new();
            for mut o in class {
                match read_full(&mut o.file, &mut buf[..want]) {
                    Ok(n) if n == want => {
                        p.bytes_done.fetch_add(n as u64, Ordering::Relaxed);
                        split.entry(xxh3_64(&buf[..n])).or_default().push(o);
                    }
                    _ => {
                        p.bytes_done.fetch_add(size - offset, Ordering::Relaxed); // vanished / shrank
                    }
                }
            }
            for (_, v) in split {
                if v.len() > 1 {
                    next.push(v);
                } else {
                    // Unique from here on: count the unread rest as done.
                    p.bytes_done.fetch_add(size - offset - want as u64, Ordering::Relaxed);
                }
            }
        }
        classes = next;
        offset += want as u64;
        block = (block * 4).min(MAX_BLOCK);
    }
    done
}

/// Split a big group by the hash of its first block, one file at a time.
fn presplit<'a>(size: u64, cands: &'a [Cand], p: &DupProgress) -> Vec<Vec<&'a Cand>> {
    let mut by: HashMap<u64, Vec<&'a Cand>> = HashMap::new();
    let mut buf = vec![0u8; FIRST_BLOCK];
    for c in cands {
        if p.cancel.load(Ordering::Relaxed) {
            return Vec::new();
        }
        let opened = if is_not_local(&c.path) {
            p.skipped.fetch_add(1, Ordering::Relaxed);
            None
        } else {
            std::fs::File::open(&c.path).ok().and_then(|mut f| read_full(&mut f, &mut buf).ok())
        };
        match opened {
            Some(n) => {
                by.entry(xxh3_64(&buf[..n])).or_default().push(c);
            }
            None => {
                p.bytes_done.fetch_add(size, Ordering::Relaxed);
            }
        }
    }
    let (keep, drop): (Vec<_>, Vec<_>) = by.into_values().partition(|v| v.len() > 1);
    // Files that turned out unique are done: count them for the progress bar.
    p.bytes_done.fetch_add(size * drop.len() as u64, Ordering::Relaxed);
    keep
}

/// Split same-size files into classes of identical content (indices into
/// `paths`); files left alone are dropped. Progressive, with early exit.
fn partition(size: u64, paths: &[String], p: &DupProgress) -> Vec<Vec<usize>> {
    let mut files: Vec<Option<std::fs::File>> = paths
        .iter()
        .map(|path| if is_not_local(path) { None } else { std::fs::File::open(path).ok() })
        .collect();
    // Paths that are the same physical file (hard links) free nothing: keep one.
    let mut seen = std::collections::HashSet::new();
    for f in files.iter_mut() {
        if let Some(id) = f.as_ref().and_then(file_id)
            && !seen.insert(id)
        {
            *f = None;
        }
    }
    let mut classes: Vec<Vec<usize>> = vec![(0..paths.len()).filter(|&i| files[i].is_some()).collect()];
    classes.retain(|c| c.len() > 1);
    let (mut offset, mut block) = (0u64, FIRST_BLOCK);
    let mut buf = vec![0u8; FIRST_BLOCK];
    while offset < size && !classes.is_empty() {
        if p.cancel.load(Ordering::Relaxed) {
            return Vec::new();
        }
        let want = block.min((size - offset) as usize);
        if buf.len() < want {
            buf.resize(want, 0);
        }
        let mut next = Vec::new();
        for class in classes {
            let mut split: HashMap<u64, Vec<usize>> = HashMap::new();
            for i in class {
                let Some(f) = files[i].as_mut() else { continue };
                if matches!(read_full(f, &mut buf[..want]), Ok(n) if n == want) {
                    split.entry(xxh3_64(&buf[..want])).or_default().push(i);
                } else {
                    files[i] = None;
                }
            }
            next.extend(split.into_values().filter(|v| v.len() > 1));
        }
        classes = next;
        offset += want as u64;
        block = (block * 4).min(MAX_BLOCK);
    }
    classes
}

/// A candidate set of identical folders: same content signature (names + sizes).
struct FolderCand {
    /// Index of this candidate group, and of the group holding all its parents.
    gid: usize,
    parent: Option<usize>,
    size: u64,
    files: u32,
    folders: Vec<Cand>,
    /// Files inside (path relative to the folder, size), largest first.
    rel: Vec<(String, u64)>,
}

enum Work {
    Files(u64, Vec<Cand>),
    Folders(FolderCand),
}

impl Work {
    fn potential(&self) -> u64 {
        match self {
            Work::Files(s, c) => s * (c.len() as u64 - 1),
            Work::Folders(f) => f.size * (f.folders.len() as u64 - 1),
        }
    }
}

/// Folder signatures computed bottom-up from the index (no disk access):
/// a folder's signature hashes the sorted (name, size | sub-signature) of its children.
fn folder_candidates(tree: &Tree, root: u32, min_size: u64, cancel: &AtomicBool) -> Vec<FolderCand> {
    let mut dirs = Vec::new();
    let mut stack = vec![root];
    while let Some(id) = stack.pop() {
        if tree.nodes[id as usize].is_dir() {
            dirs.push(id);
            stack.extend_from_slice(tree.children(id));
        }
    }
    dirs.sort_unstable_by(|a, b| b.cmp(a)); // children have larger ids: deepest first
    let mut sig: HashMap<u32, u64> = HashMap::with_capacity(dirs.len());
    let mut entries: Vec<u64> = Vec::new();
    for (k, &d) in dirs.iter().enumerate() {
        if k % 16_384 == 0 && cancel.load(Ordering::Relaxed) {
            return Vec::new();
        }
        entries.clear();
        for &c in tree.children(d) {
            let n = &tree.nodes[c as usize];
            let name = tree.name(c).to_lowercase();
            let mut bytes = name.into_bytes();
            if n.is_dir() {
                bytes.push(1);
                bytes.extend_from_slice(&sig.get(&c).copied().unwrap_or(0).to_le_bytes());
            } else {
                bytes.push(0);
                bytes.extend_from_slice(&n.size.to_le_bytes());
            }
            entries.push(xxh3_64(&bytes));
        }
        entries.sort_unstable();
        let raw: Vec<u8> = entries.iter().flat_map(|e| e.to_le_bytes()).collect();
        sig.insert(d, xxh3_64(&raw));
    }

    let mut by_sig: HashMap<(u64, u64), Vec<u32>> = HashMap::new();
    for &d in &dirs {
        let n = &tree.nodes[d as usize];
        if d == root || n.parent == crate::tree::ROOT || n.files == 0 || n.size < min_size.max(1) {
            continue;
        }
        by_sig.entry((sig[&d], n.size)).or_default().push(d);
    }
    let groups: Vec<Vec<u32>> = by_sig.into_values().filter(|v| v.len() > 1).collect();

    // Nested matches are verified too, unless their parent group is confirmed
    // as a whole first (then they're redundant).
    let mut group_of: HashMap<u32, usize> = HashMap::new();
    for (i, g) in groups.iter().enumerate() {
        for &d in g {
            group_of.insert(d, i);
        }
    }
    let mut out = Vec::new();
    for (gid, g) in groups.iter().enumerate() {
        if cancel.load(Ordering::Relaxed) {
            return Vec::new();
        }
        let parent_groups: std::collections::HashSet<Option<usize>> =
            g.iter().map(|&d| group_of.get(&tree.nodes[d as usize].parent).copied()).collect();
        // Nested match: every member's parent is in one same candidate group.
        let parent = if parent_groups.len() == 1 { parent_groups.into_iter().next().flatten() } else { None };
        let first = g[0];
        let base = tree.full_path(first);
        let mut rel: Vec<(String, u64)> = tree
            .files_under(first)
            .into_iter()
            .map(|f| (tree.full_path(f)[base.len()..].trim_start_matches('\\').to_string(), tree.nodes[f as usize].size))
            .collect();
        rel.sort_by_key(|r| std::cmp::Reverse(r.1));
        out.push(FolderCand {
            gid,
            parent,
            size: tree.nodes[first as usize].size,
            files: tree.nodes[first as usize].files,
            folders: g.iter().map(|&d| Cand { path: tree.full_path(d), modified: tree.nodes[d as usize].modified }).collect(),
            rel,
        });
    }
    out
}

/// Classes of folders (indices into `f.folders`) whose every file is identical.
fn verify_folders(f: &FolderCand, p: &DupProgress) -> Vec<Vec<usize>> {
    let mut classes: Vec<Vec<usize>> = vec![(0..f.folders.len()).collect()];
    // Smallest files first: cheap mismatches are found before reading big files.
    for (rel, size) in f.rel.iter().rev() {
        let mut next = Vec::new();
        for class in &classes {
            let paths: Vec<String> =
                class.iter().map(|&i| format!("{}\\{rel}", f.folders[i].path.trim_end_matches('\\'))).collect();
            for part in partition(*size, &paths, p) {
                next.push(part.into_iter().map(|j| class[j]).collect());
            }
        }
        classes = next;
        if classes.is_empty() {
            break;
        }
    }
    classes
}

/// Streaming search: `on_group` is called from worker threads for every
/// confirmed group (files and whole folders), as soon as it is found.
pub fn find_streaming(tree: Arc<Tree>, opt: DupOptions, p: &DupProgress, on_group: &(dyn Fn(DupGroup) + Sync)) {
    // 1. Candidates from the in-memory index (no disk access).
    p.stage.store(0, Ordering::Relaxed);
    let mut by_key: HashMap<(u64, String), Vec<u32>> = HashMap::new();
    for (k, id) in tree.files_under(opt.root).into_iter().enumerate() {
        if k % 65_536 == 0 && p.cancel.load(Ordering::Relaxed) {
            return;
        }
        let n = &tree.nodes[id as usize];
        if n.size < opt.min_size.max(1) {
            continue;
        }
        // Quick mode has nothing else to go on, so it always requires the same name.
        let name = if opt.same_name || !opt.verify { tree.name(id).to_lowercase() } else { String::new() };
        by_key.entry((n.size, name)).or_default().push(id);
    }
    let mut work: Vec<Work> = by_key
        .into_iter()
        .filter(|(_, v)| v.len() > 1)
        .map(|((size, _), ids)| {
            let c = ids.into_iter().map(|id| Cand { path: tree.full_path(id), modified: tree.nodes[id as usize].modified }).collect();
            Work::Files(size, c)
        })
        .collect();
    if p.cancel.load(Ordering::Relaxed) {
        return;
    }
    work.extend(folder_candidates(&tree, opt.root, opt.min_size, &p.cancel).into_iter().map(Work::Folders));
    drop(tree);
    // Biggest potential savings first, so the most useful results show up early.
    work.sort_by_key(|w| std::cmp::Reverse(w.potential()));
    p.groups_total.store(work.len() as u64, Ordering::Relaxed);
    p.files_total.store(work.iter().map(|w| if let Work::Files(_, c) = w { c.len() as u64 } else { 0 }).sum(), Ordering::Relaxed);
    p.bytes_total.store(
        work.iter()
            .map(|w| match w {
                Work::Files(s, c) => s * c.len() as u64,
                Work::Folders(f) => f.size * f.folders.len() as u64,
            })
            .sum(),
        Ordering::Relaxed,
    );
    if p.cancel.load(Ordering::Relaxed) {
        return;
    }

    // 2. Compare contents on a small thread pool.
    p.stage.store(1, Ordering::Relaxed);
    let max_gid = work.iter().map(|w| if let Work::Folders(f) = w { f.gid + 1 } else { 0 }).max().unwrap_or(0);
    let whole: Vec<AtomicBool> = (0..max_gid).map(|_| AtomicBool::new(false)).collect();
    let next = AtomicUsize::new(0);
    std::thread::scope(|s| {
        for _ in 0..threads() {
            s.spawn(|| {
                loop {
                    if p.cancel.load(Ordering::Relaxed) {
                        break;
                    }
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    let Some(w) = work.get(i) else { break };
                    if !opt.verify {
                        // Quick mode: report the index match as is, no disk access.
                        let (size, folder, inner, paths) = match w {
                            Work::Files(s, c) => (*s, false, 0, c),
                            Work::Folders(f) => (f.size, true, f.files, &f.folders),
                        };
                        on_group(DupGroup {
                            size,
                            files: paths.iter().map(|c| DupFile { path: c.path.clone(), modified: c.modified }).collect(),
                            folder,
                            inner_files: inner,
                            verified: false,
                        });
                        p.bytes_done.fetch_add(size * paths.len() as u64, Ordering::Relaxed);
                        p.groups_done.fetch_add(1, Ordering::Relaxed);
                        continue;
                    }
                    match w {
                        Work::Files(size, cands) => {
                            let parts: Vec<Vec<&Cand>> =
                                if cands.len() > MAX_OPEN { presplit(*size, cands, p) } else { vec![cands.iter().collect()] };
                            for part in parts {
                                let owned: Vec<Cand> =
                                    part.iter().map(|c| Cand { path: c.path.clone(), modified: c.modified }).collect();
                                for class in compare_group(*size, &owned, p) {
                                    on_group(DupGroup {
                                        size: *size,
                                        files: class.into_iter().map(|c| DupFile { path: c.path.clone(), modified: c.modified }).collect(),
                                        folder: false,
                                        inner_files: 0,
                                        verified: true,
                                    });
                                }
                            }
                        }
                        Work::Folders(f) => {
                            let redundant = f.parent.is_some_and(|pg| whole.get(pg).is_some_and(|w| w.load(Ordering::Relaxed)));
                            let classes = if redundant { Vec::new() } else { verify_folders(f, p) };
                            if classes.len() == 1 && classes[0].len() == f.folders.len() {
                                whole[f.gid].store(true, Ordering::Relaxed);
                            }
                            for class in classes {
                                on_group(DupGroup {
                                    size: f.size,
                                    files: class
                                        .into_iter()
                                        .map(|i| DupFile { path: f.folders[i].path.clone(), modified: f.folders[i].modified })
                                        .collect(),
                                    folder: true,
                                    inner_files: f.files,
                                    verified: true,
                                });
                            }
                            // Count the whole folder set as processed for the progress bar.
                            p.bytes_done.fetch_add(f.size * f.folders.len() as u64, Ordering::Relaxed);
                        }
                    }
                    p.groups_done.fetch_add(1, Ordering::Relaxed);
                }
            });
        }
    });
}

/// Blocking search that returns every group, sorted by reclaimable space.
#[cfg_attr(not(test), allow(dead_code))]
pub fn find(tree: Arc<Tree>, opt: DupOptions, p: Arc<DupProgress>) -> Vec<DupGroup> {
    let out = Mutex::new(Vec::new());
    find_streaming(tree, opt, &p, &|g| out.lock().unwrap().push(g));
    if p.cancel.load(Ordering::Relaxed) {
        return Vec::new();
    }
    let mut v = out.into_inner().unwrap();
    v.sort_by_key(|g| std::cmp::Reverse(g.wasted()));
    v
}

/// Send files to the Recycle Bin. Windows asks before deleting permanently
/// (e.g. on drives without a Recycle Bin). Returns false if it was aborted.
pub fn recycle(paths: &[String]) -> Result<bool, String> {
    use windows_sys::Win32::UI::Shell::*;
    if paths.is_empty() {
        return Ok(true);
    }
    let mut from: Vec<u16> = Vec::new();
    for p in paths {
        from.extend(p.encode_utf16());
        from.push(0);
    }
    from.push(0);
    let mut op: SHFILEOPSTRUCTW = unsafe { std::mem::zeroed() };
    op.wFunc = FO_DELETE;
    op.pFrom = from.as_ptr();
    op.fFlags = (FOF_ALLOWUNDO | FOF_NOCONFIRMATION | FOF_WANTNUKEWARNING) as u16;
    let r = unsafe { SHFileOperationW(&mut op) };
    if r != 0 {
        return Err(format!("SHFileOperation error 0x{r:X}"));
    }
    Ok(op.fAnyOperationsAborted == 0)
}

/// CSV of the duplicate groups.
pub fn write_csv(groups: &[DupGroup], path: &std::path::Path) -> std::io::Result<()> {
    use std::io::Write;
    let mut w = std::io::BufWriter::new(std::fs::File::create(path)?);
    w.write_all("\u{feff}".as_bytes())?;
    writeln!(w, "\"Group\",\"Type\",\"Size\",\"Path\",\"Modified\"")?;
    for (i, g) in groups.iter().enumerate() {
        let kind = if g.folder { "Folder" } else { "File" };
        for f in &g.files {
            writeln!(w, "{},{kind},{},\"{}\",{}", i + 1, g.size, f.path.replace('"', "\"\""), crate::app::fmt_iso(f.modified))?;
        }
    }
    w.flush()
}
