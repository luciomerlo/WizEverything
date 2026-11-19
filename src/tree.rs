//! Compact folder/file tree built from Everything's index.
//!
//! Layout (≈56 bytes per node): names live in one shared `String`, children in
//! one flat `Vec<u32>`. Every node's parent has a smaller index than the node
//! itself, which makes aggregation a single reverse pass.

use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::time::Instant;

use crate::everything;
use crate::i18n::tr;

pub const NO_EXT: u32 = u32::MAX;
pub const ROOT: u32 = 0;
const NONE: u32 = u32::MAX;

const F_DIR: u8 = 1;
const F_REAL: u8 = 2;

#[derive(Clone, Copy)]
pub struct Node {
    pub parent: u32,
    /// Extension index for files; `NO_EXT` for folders.
    pub ext: u32,
    name_off: u32,
    name_len: u16,
    flags: u8,
    pub size: u64,
    pub alloc: u64,
    pub modified: u64,
    pub files: u32,
    pub folders: u32,
    first: u32,
    count: u32,
}

impl Node {
    pub fn is_dir(&self) -> bool {
        self.flags & F_DIR != 0
    }
}

pub struct ExtInfo {
    pub name: String,
    pub size: u64,
    pub alloc: u64,
    pub count: u64,
    pub color: [u8; 3],
}

pub struct Tree {
    pub nodes: Vec<Node>,
    names: String,
    kids: Vec<u32>,
    pub exts: Vec<ExtInfo>,
    ext_ids: HashMap<String, u32>,
    /// All drives (ROOT's children may exclude some of them).
    pub drives: Vec<u32>,
    excluded: HashSet<u32>,
    clusters: HashMap<u32, u64>,
    labels: HashMap<u32, String>,
    pub load_ms: u128,
    pub built_at: Instant,
    /// Local time (Everything search syntax) when the last query started.
    pub sync_stamp: String,
}

#[derive(Default)]
pub struct Progress {
    pub done: AtomicU32,
    pub total: AtomicU32,
    /// true while waiting for Everything to start / finish loading its database.
    pub waiting: AtomicBool,
}

/// A file reported by Everything as changed since the last sync.
pub struct Change {
    pub dir: String,
    pub name: String,
    pub size: u64,
    pub modified: u64,
}

pub fn local_stamp() -> String {
    use windows_sys::Win32::System::SystemInformation::GetLocalTime;
    let mut t = unsafe { std::mem::zeroed() };
    unsafe { GetLocalTime(&mut t) };
    format!("{:04}-{:02}-{:02}T{:02}:{:02}:{:02}", t.wYear, t.wMonth, t.wDay, t.wHour, t.wMinute, t.wSecond)
}

fn cluster_size(drive: &str) -> u64 {
    use windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceW;
    let root = everything::wide(&format!("{}\\", drive.trim_end_matches('\\')));
    let (mut spc, mut bps, mut free, mut total) = (0u32, 0u32, 0u32, 0u32);
    let ok = unsafe { GetDiskFreeSpaceW(root.as_ptr(), &mut spc, &mut bps, &mut free, &mut total) };
    if ok != 0 && spc > 0 && bps > 0 { spc as u64 * bps as u64 } else { 4096 }
}

/// Display name of a drive: volume label, with Google Drive's mode detected.
/// Examples: "Disco local", "luciomerlo@gmail.com - Google Drive (stream)".
pub fn volume_label(drive: &str) -> String {
    use windows_sys::Win32::Storage::FileSystem::GetVolumeInformationW;
    let root = everything::wide(&format!("{}\\", drive.trim_end_matches('\\')));
    let mut label = [0u16; 261];
    let mut fs = [0u16; 261];
    let ok = unsafe {
        GetVolumeInformationW(
            root.as_ptr(),
            label.as_mut_ptr(),
            label.len() as u32,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            fs.as_mut_ptr(),
            fs.len() as u32,
        )
    };
    if ok == 0 {
        return String::new();
    }
    let s = |b: &[u16]| String::from_utf16_lossy(&b[..b.iter().position(|&c| c == 0).unwrap_or(b.len())]);
    let (label, fs) = (s(&label), s(&fs));
    if label.contains("Google Drive") {
        // Streaming: "My Drive" lives inside the virtual drive.
        // Mirroring: "My Drive" is a local folder, so the virtual drive doesn't have it.
        let base = drive.trim_end_matches('\\');
        let streams = ["Mi unidad", "My Drive", "Mein Ablage", "Meu Drive", "Mon Drive"]
            .iter()
            .any(|d| std::path::Path::new(&format!("{base}\\{d}")).is_dir());
        return format!("{label} ({})", if streams { "stream" } else { "clone" });
    }
    if label.is_empty() {
        return if fs.is_empty() { String::new() } else { tr("Disco local", "Local Disk").to_string() };
    }
    label
}

/// (total, free) bytes of a drive, if it can be queried.
pub fn disk_space(drive: &str) -> Option<(u64, u64)> {
    use windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;
    let root = everything::wide(&format!("{}\\", drive.trim_end_matches('\\')));
    let (mut avail, mut total, mut free) = (0u64, 0u64, 0u64);
    let ok = unsafe { GetDiskFreeSpaceExW(root.as_ptr(), &mut avail, &mut total, &mut free) };
    if ok != 0 { Some((total, free)) } else { None }
}

/// Real allocated size of a file (compression/sparse aware), rounded to the cluster.
pub fn real_file_alloc(path: &str, cluster: u64) -> Option<u64> {
    use windows_sys::Win32::Foundation::{GetLastError, NO_ERROR};
    use windows_sys::Win32::Storage::FileSystem::GetCompressedFileSizeW;
    let w = everything::wide(path);
    let mut high = 0u32;
    let low = unsafe { GetCompressedFileSizeW(w.as_ptr(), &mut high) };
    if low == u32::MAX && unsafe { GetLastError() } != NO_ERROR {
        return None;
    }
    let s = ((high as u64) << 32) | low as u64;
    Some(s.div_ceil(cluster) * cluster)
}

fn ext_of(name: &str) -> String {
    match name.rfind('.') {
        Some(i) if i > 0 && i + 1 < name.len() => name[i + 1..].to_lowercase(),
        _ => String::new(),
    }
}

fn new_node(nodes: &mut Vec<Node>, names: &mut String, name: &str, parent: u32, dir: bool) -> u32 {
    let id = nodes.len() as u32;
    let off = names.len() as u32;
    names.push_str(name);
    nodes.push(Node {
        parent,
        ext: NO_EXT,
        name_off: off,
        name_len: name.len().min(u16::MAX as usize) as u16,
        flags: if dir { F_DIR } else { 0 },
        size: 0,
        alloc: 0,
        modified: 0,
        files: 0,
        folders: 0,
        first: 0,
        count: 0,
    });
    id
}

pub(crate) struct Builder {
    nodes: Vec<Node>,
    names: String,
    dirs: HashMap<Box<str>, u32>,
    ext_ids: HashMap<String, u32>,
    ext_names: Vec<String>,
    last_dir: (String, u32),
}

impl Builder {
    fn dir(&mut self, path: &str) -> u32 {
        let path = path.trim_end_matches('\\');
        if self.last_dir.0 == path {
            return self.last_dir.1;
        }
        let id = if let Some(&id) = self.dirs.get(path) {
            id
        } else {
            let (parent, name) = match path.rfind('\\') {
                Some(i) if i > 0 && !path[..i].trim_end_matches('\\').is_empty() => (self.dir(&path[..i]), &path[i + 1..]),
                _ => (ROOT, path),
            };
            let id = new_node(&mut self.nodes, &mut self.names, name, parent, true);
            self.dirs.insert(path.into(), id);
            id
        };
        self.last_dir.0.clear();
        self.last_dir.0.push_str(path);
        self.last_dir.1 = id;
        id
    }

    fn ext(&mut self, name: &str) -> u32 {
        let ext = ext_of(name);
        if let Some(&id) = self.ext_ids.get(&ext) {
            return id;
        }
        let id = self.ext_names.len() as u32;
        self.ext_names.push(ext.clone());
        self.ext_ids.insert(ext, id);
        id
    }
}

fn decode(dst: &mut String, src: &[u16]) {
    dst.clear();
    dst.extend(char::decode_utf16(src.iter().copied()).map(|c| c.unwrap_or('\u{FFFD}')));
}

impl Builder {
    pub(crate) fn new() -> Self {
        let mut b = Builder {
            nodes: Vec::new(),
            names: String::new(),
            dirs: HashMap::new(),
            ext_ids: HashMap::new(),
            ext_names: Vec::new(),
            last_dir: (String::from("\0"), 0),
        };
        new_node(&mut b.nodes, &mut b.names, "", NONE, true); // virtual root ("This PC")
        b
    }

    fn reserve(&mut self, total: usize) {
        self.nodes.reserve_exact(total + total / 8);
        self.names.reserve(total * 20);
        self.dirs.reserve(total / 5);
    }

    /// Add one item as Everything reports it: `path` is the parent folder.
    pub(crate) fn add(&mut self, is_dir: bool, path: &str, name: &str, size: Option<u64>, modified: u64) {
        if is_dir {
            let full = if path.is_empty() { name.to_string() } else { format!("{}\\{}", path.trim_end_matches('\\'), name) };
            let id = self.dir(&full);
            self.nodes[id as usize].modified = modified;
        } else {
            let parent = if path.is_empty() { ROOT } else { self.dir(path) };
            let ext = self.ext(name);
            let id = new_node(&mut self.nodes, &mut self.names, name, parent, false);
            let n = &mut self.nodes[id as usize];
            n.ext = ext;
            n.size = size.unwrap_or(0);
            n.modified = modified;
        }
    }
}

pub fn load(progress: Arc<Progress>, excluded: &[String]) -> Result<Tree, String> {
    let start = Instant::now();
    let stamp = local_stamp();
    let mut b = Builder::new();
    let total_hint = std::cell::Cell::new(0u32);
    let mut path = String::new();
    let mut name = String::new();
    everything::for_each_item(
        "",
        |it| {
            if b.nodes.len() == 1 && total_hint.get() > 0 {
                b.reserve(total_hint.get() as usize);
            }
            decode(&mut path, it.path);
            decode(&mut name, it.name);
            b.add(it.is_dir, &path, &name, it.size, it.modified);
        },
        |done, total| {
            total_hint.set(total);
            progress.done.store(done, Ordering::Relaxed);
            progress.total.store(total, Ordering::Relaxed);
        },
    )?;
    Ok(b.finish(excluded, start, stamp))
}

impl Builder {
    pub(crate) fn finish(self, excluded: &[String], start: Instant, stamp: String) -> Tree {
    let Builder { mut nodes, names, ext_ids, ext_names, .. } = self;
    nodes.shrink_to_fit();
    let n = nodes.len();

    // Drive of each node (parents always come first) → cluster-rounded allocation.
    let mut clusters = HashMap::new();
    let mut drive_of = vec![NONE; n];
    for id in 1..n {
        let p = nodes[id].parent;
        drive_of[id] = if p == ROOT { id as u32 } else { drive_of[p as usize] };
        if p == ROOT {
            let nm = &names[nodes[id].name_off as usize..][..nodes[id].name_len as usize];
            clusters.insert(id as u32, cluster_size(nm));
        }
    }
    for id in 1..n {
        if !nodes[id].is_dir() {
            let cs = clusters.get(&drive_of[id]).copied().unwrap_or(4096);
            nodes[id].alloc = nodes[id].size.div_ceil(cs) * cs;
        }
    }
    drop(drive_of);

    // Aggregate bottom-up in a single reverse pass.
    for id in (1..n).rev() {
        let c = nodes[id];
        let p = &mut nodes[c.parent as usize];
        p.size += c.size;
        p.alloc += c.alloc;
        p.files += c.files + u32::from(!c.is_dir());
        p.folders += c.folders + u32::from(c.is_dir());
        p.count += 1;
    }

    // Flat children array (counting sort by parent), each slice sorted by size desc.
    let mut next = 0u32;
    for node in nodes.iter_mut() {
        node.first = next;
        next += node.count;
        node.count = 0;
    }
    let mut kids = vec![0u32; next as usize];
    for id in 1..n {
        let p = nodes[id].parent as usize;
        let slot = nodes[p].first + nodes[p].count;
        kids[slot as usize] = id as u32;
        nodes[p].count += 1;
    }
    // Sort every children slice in parallel: node ranges map to contiguous,
    // disjoint ranges of `kids` (offsets are prefix sums in node order).
    {
        let threads = std::thread::available_parallelism().map_or(4, |t| t.get()).max(1);
        let per = n.div_ceil(threads).max(1);
        let total = kids.len();
        let nodes = &nodes;
        std::thread::scope(|s| {
            let mut rest: &mut [u32] = &mut kids;
            let (mut start, mut consumed) = (0usize, 0usize);
            while start < n {
                let end = (start + per).min(n);
                let end_kid = if end < n { nodes[end].first as usize } else { total };
                let (chunk, r) = std::mem::take(&mut rest).split_at_mut(end_kid - consumed);
                rest = r;
                let base = consumed;
                s.spawn(move || {
                    for id in start..end {
                        let (f, c) = (nodes[id].first as usize - base, nodes[id].count as usize);
                        if c > 1 {
                            chunk[f..f + c].sort_unstable_by(|a, b| nodes[*b as usize].size.cmp(&nodes[*a as usize].size));
                        }
                    }
                });
                consumed = end_kid;
                start = end;
            }
        });
    }

    let mut exts: Vec<ExtInfo> =
        ext_names.into_iter().map(|name| ExtInfo { name, size: 0, alloc: 0, count: 0, color: [0; 3] }).collect();
    for node in &nodes {
        if !node.is_dir() {
            let e = &mut exts[node.ext as usize];
            e.size += node.size;
            e.alloc += node.alloc;
            e.count += 1;
        }
    }
    assign_colors(&mut exts);

    let drives = kids[nodes[0].first as usize..][..nodes[0].count as usize].to_vec();
    let mut tree = Tree {
        nodes,
        names,
        kids,
        exts,
        ext_ids,
        drives,
        excluded: HashSet::new(),
        labels: HashMap::new(),
        clusters,
        load_ms: start.elapsed().as_millis(),
        built_at: Instant::now(),
        sync_stamp: stamp,
    };
    tree.labels = tree.drives.iter().map(|&d| (d, volume_label(tree.name(d)))).collect();
    tree.set_excluded(excluded);
    tree
    }
}

/// WizTree-like palette: saturated colors for the heaviest extensions,
/// hashed hues for the rest.
fn assign_colors(exts: &mut [ExtInfo]) {
    let mut order: Vec<usize> = (0..exts.len()).collect();
    order.sort_by(|a, b| exts[*b].size.cmp(&exts[*a].size));
    for (rank, &i) in order.iter().enumerate() {
        exts[i].color = ext_color(&exts[i].name, Some(rank));
    }
}

fn ext_color(name: &str, rank: Option<usize>) -> [u8; 3] {
    const PALETTE: [[u8; 3]; 24] = [
        [0, 92, 255], [255, 40, 40], [40, 220, 40], [255, 220, 0], [0, 220, 255], [255, 0, 220],
        [255, 140, 0], [140, 80, 255], [0, 160, 110], [190, 190, 0], [255, 120, 160], [100, 180, 255],
        [160, 60, 0], [0, 120, 160], [200, 0, 90], [120, 220, 140], [90, 90, 200], [230, 160, 90],
        [0, 200, 180], [180, 110, 200], [150, 150, 150], [220, 90, 60], [60, 140, 40], [240, 200, 140],
    ];
    if let Some(r) = rank
        && r < PALETTE.len()
    {
        return PALETTE[r];
    }
    let h = name.bytes().fold(2166136261u32, |h, c| (h ^ c as u32).wrapping_mul(16777619));
    hsv((h % 360) as f32, 0.45 + (h >> 9 & 0xff) as f32 / 255.0 * 0.3, 0.75)
}

fn hsv(h: f32, s: f32, v: f32) -> [u8; 3] {
    let c = v * s;
    let x = c * (1.0 - ((h / 60.0) % 2.0 - 1.0).abs());
    let m = v - c;
    let (r, g, b) = match (h / 60.0) as u32 {
        0 => (c, x, 0.0),
        1 => (x, c, 0.0),
        2 => (0.0, c, x),
        3 => (0.0, x, c),
        4 => (x, 0.0, c),
        _ => (c, 0.0, x),
    };
    [((r + m) * 255.0) as u8, ((g + m) * 255.0) as u8, ((b + m) * 255.0) as u8]
}

impl Tree {
    pub fn name(&self, id: u32) -> &str {
        let n = &self.nodes[id as usize];
        &self.names[n.name_off as usize..][..n.name_len as usize]
    }

    pub fn children(&self, id: u32) -> &[u32] {
        let n = &self.nodes[id as usize];
        &self.kids[n.first as usize..][..n.count as usize]
    }

    /// C:  Disco local style name for a drive node.
    pub fn drive_label(&self, id: u32) -> String {
        match self.labels.get(&id).filter(|l| !l.is_empty()) {
            Some(l) => format!("{}  {l}", self.name(id)),
            None => self.name(id).to_string(),
        }
    }

    pub fn is_excluded(&self, drive: u32) -> bool {
        self.excluded.contains(&drive)
    }

    pub fn full_path(&self, mut id: u32) -> String {
        let mut parts = Vec::new();
        while id != ROOT && id != NONE {
            parts.push(self.name(id));
            id = self.nodes[id as usize].parent;
        }
        parts.reverse();
        let mut s = parts.join("\\");
        if parts.len() == 1 {
            s.push('\\');
        }
        s
    }

    /// Ancestors from the root down to `id` (inclusive).
    pub fn ancestors(&self, mut id: u32) -> Vec<u32> {
        let mut v = vec![id];
        while id != ROOT {
            id = self.nodes[id as usize].parent;
            v.push(id);
        }
        v.reverse();
        v
    }

    pub fn drive_of(&self, id: u32) -> Option<u32> {
        self.ancestors(id).get(1).copied()
    }

    pub fn ext_label(&self, id: u32) -> &str {
        let e = &self.exts[id as usize].name;
        if e.is_empty() { tr("(sin extensión)", "(no extension)") } else { e }
    }

    /// Representative color of a folder: the color of its largest file.
    pub fn dominant_color(&self, mut id: u32) -> [u8; 3] {
        loop {
            let n = &self.nodes[id as usize];
            if !n.is_dir() {
                return self.exts[n.ext as usize].color;
            }
            match self.children(id).first() {
                Some(&c) => id = c,
                None => return [110, 110, 110],
            }
        }
    }

    pub fn find_child(&self, parent: u32, name: &str, dir: bool) -> Option<u32> {
        self.children(parent).iter().copied().find(|&c| {
            self.nodes[c as usize].is_dir() == dir && self.name(c).eq_ignore_ascii_case(name)
        })
    }

    /// Drive by name, including drives excluded from "This PC".
    pub fn find_drive(&self, name: &str) -> Option<u32> {
        self.drives.iter().copied().find(|&d| self.name(d).eq_ignore_ascii_case(name))
    }

    pub fn find_path(&self, path: &str) -> Option<u32> {
        let mut id = ROOT;
        for part in path.split('\\').filter(|p| !p.is_empty()) {
            id = if id == ROOT {
                self.find_drive(part)?
            } else {
                *self.children(id).iter().find(|&&c| self.name(c).eq_ignore_ascii_case(part))?
            };
        }
        Some(id)
    }

    /// Per-extension totals (ext, size, count) restricted to a subtree.
    pub fn ext_stats(&self, root: u32) -> Vec<(u32, u64, u64)> {
        let mut acc: HashMap<u32, (u64, u64)> = HashMap::new();
        let mut stack = vec![root];
        while let Some(id) = stack.pop() {
            let n = &self.nodes[id as usize];
            if n.is_dir() {
                stack.extend_from_slice(self.children(id));
            } else {
                let e = acc.entry(n.ext).or_default();
                e.0 += n.size;
                e.1 += 1;
            }
        }
        acc.into_iter().map(|(k, (s, c))| (k, s, c)).collect()
    }

    /// The `limit` largest files of a subtree (optionally name-filtered).
    pub fn largest_files(&self, root: u32, filter: &str, limit: usize) -> Vec<u32> {
        let filter = filter.to_lowercase();
        let mut heap = std::collections::BinaryHeap::new();
        let mut stack = vec![root];
        while let Some(id) = stack.pop() {
            let n = &self.nodes[id as usize];
            if n.is_dir() {
                stack.extend_from_slice(self.children(id));
                continue;
            }
            if !filter.is_empty() && !self.name(id).to_lowercase().contains(&filter) {
                continue;
            }
            heap.push(std::cmp::Reverse((n.size, id)));
            if heap.len() > limit {
                heap.pop();
            }
        }
        let mut v: Vec<(u64, u32)> = heap.into_iter().map(|r| r.0).collect();
        v.sort_unstable_by(|a, b| b.cmp(a));
        v.into_iter().map(|(_, id)| id).collect()
    }

    /// All files of a subtree (for the real-size worker).
    pub fn files_under(&self, root: u32) -> Vec<u32> {
        let mut out = Vec::new();
        let mut stack = vec![root];
        while let Some(id) = stack.pop() {
            if self.nodes[id as usize].is_dir() {
                stack.extend_from_slice(self.children(id));
            } else {
                out.push(id);
            }
        }
        out
    }

    pub fn cluster_of(&self, id: u32) -> u64 {
        self.drive_of(id).and_then(|d| self.clusters.get(&d).copied()).unwrap_or(4096)
    }

    // ------------------------------------------------------------ mutation

    /// Add deltas to `id` and all its ancestors. Changes inside an excluded drive
    /// stop at the drive and don't reach "This PC".
    fn bump(&mut self, mut id: u32, ds: i64, da: i64, df: i64, dd: i64) {
        let mut prev = NONE;
        while id != NONE {
            if id == ROOT && self.excluded.contains(&prev) {
                break;
            }
            let n = &mut self.nodes[id as usize];
            n.size = (n.size as i64 + ds).max(0) as u64;
            n.alloc = (n.alloc as i64 + da).max(0) as u64;
            n.files = (n.files as i64 + df).max(0) as u32;
            n.folders = (n.folders as i64 + dd).max(0) as u32;
            prev = id;
            id = n.parent;
        }
    }

    fn push_child(&mut self, parent: u32, child: u32) {
        let (f, c) = (self.nodes[parent as usize].first as usize, self.nodes[parent as usize].count as usize);
        let new_first = self.kids.len() as u32;
        self.kids.extend_from_within(f..f + c);
        self.kids.push(child);
        let p = &mut self.nodes[parent as usize];
        p.first = new_first;
        p.count += 1;
    }

    fn resort(&mut self, id: u32) {
        let (f, c) = (self.nodes[id as usize].first as usize, self.nodes[id as usize].count as usize);
        let nodes = &self.nodes;
        self.kids[f..f + c].sort_unstable_by(|a, b| nodes[*b as usize].size.cmp(&nodes[*a as usize].size));
    }

    fn ensure_dir(&mut self, path: &str) -> u32 {
        let mut id = ROOT;
        for part in path.split('\\').filter(|p| !p.is_empty()) {
            let found = if id == ROOT { self.find_drive(part) } else { self.find_child(id, part, true) };
            id = match found {
                Some(c) => c,
                None => {
                    let c = new_node(&mut self.nodes, &mut self.names, part, id, true);
                    self.push_child(id, c);
                    if id == ROOT {
                        self.drives.push(c);
                        self.clusters.insert(c, cluster_size(part));
                    } else {
                        self.bump(id, 0, 0, 0, 1);
                    }
                    c
                }
            };
        }
        id
    }

    fn ext_id(&mut self, name: &str) -> u32 {
        let ext = ext_of(name);
        if let Some(&id) = self.ext_ids.get(&ext) {
            return id;
        }
        let id = self.exts.len() as u32;
        self.exts.push(ExtInfo { name: ext.clone(), size: 0, alloc: 0, count: 0, color: ext_color(&ext, None) });
        self.ext_ids.insert(ext, id);
        id
    }

    /// Apply files reported as new/changed. Returns how many nodes changed.
    pub fn apply_changes(&mut self, changes: &[Change]) -> usize {
        let mut touched = HashSet::new();
        let mut applied = 0;
        for ch in changes {
            let parent = self.ensure_dir(&ch.dir);
            let cs = self.cluster_of(parent);
            let alloc = ch.size.div_ceil(cs) * cs;
            if let Some(id) = self.find_child(parent, &ch.name, false) {
                let n = self.nodes[id as usize];
                if n.size == ch.size && n.modified == ch.modified {
                    continue;
                }
                let (ds, da) = (ch.size as i64 - n.size as i64, alloc as i64 - n.alloc as i64);
                let e = &mut self.exts[n.ext as usize];
                e.size = (e.size as i64 + ds) as u64;
                e.alloc = (e.alloc as i64 + da) as u64;
                let m = &mut self.nodes[id as usize];
                m.size = ch.size;
                m.alloc = alloc;
                m.modified = ch.modified;
                m.flags &= !F_REAL;
                self.bump(parent, ds, da, 0, 0);
            } else {
                let ext = self.ext_id(&ch.name);
                let id = new_node(&mut self.nodes, &mut self.names, &ch.name, parent, false);
                let m = &mut self.nodes[id as usize];
                m.ext = ext;
                m.size = ch.size;
                m.alloc = alloc;
                m.modified = ch.modified;
                self.push_child(parent, id);
                let e = &mut self.exts[ext as usize];
                e.size += ch.size;
                e.alloc += alloc;
                e.count += 1;
                self.bump(parent, ch.size as i64, alloc as i64, 1, 0);
            }
            applied += 1;
            let mut p = parent;
            while p != NONE && touched.insert(p) {
                p = self.nodes[p as usize].parent;
            }
        }
        for id in touched {
            self.resort(id);
        }
        applied
    }

    /// Replace estimated allocations with real on-disk sizes.
    pub fn set_real_alloc(&mut self, vals: &[(u32, u64)]) {
        for &(id, real) in vals {
            let n = self.nodes[id as usize];
            let d = real as i64 - n.alloc as i64;
            self.nodes[id as usize].alloc = real;
            self.nodes[id as usize].flags |= F_REAL;
            if d != 0 {
                let e = &mut self.exts[n.ext as usize];
                e.alloc = (e.alloc as i64 + d).max(0) as u64;
                self.bump(n.parent, 0, d, 0, 0);
            }
        }
    }

    /// Choose which drives count towards "This PC" (by name, e.g. "H:").
    pub fn set_excluded(&mut self, names: &[String]) {
        self.excluded = self
            .drives
            .iter()
            .copied()
            .filter(|&d| names.iter().any(|n| n.eq_ignore_ascii_case(self.name(d))))
            .collect();
        let included: Vec<u32> = self.drives.iter().copied().filter(|d| !self.excluded.contains(d)).collect();
        let mut root = self.nodes[ROOT as usize];
        root.size = 0;
        root.alloc = 0;
        root.files = 0;
        root.folders = 0;
        for &d in &included {
            let n = &self.nodes[d as usize];
            root.size += n.size;
            root.alloc += n.alloc;
            root.files += n.files;
            root.folders += n.folders + 1;
        }
        root.first = self.kids.len() as u32;
        root.count = included.len() as u32;
        self.kids.extend_from_slice(&included);
        self.nodes[ROOT as usize] = root;
        self.resort(ROOT);
    }

    /// Approximate heap usage in bytes.
    pub fn memory(&self) -> usize {
        self.nodes.capacity() * std::mem::size_of::<Node>()
            + self.names.capacity()
            + self.kids.capacity() * 4
            + self.exts.iter().map(|e| e.name.capacity() + 48).sum::<usize>()
    }

    /// Write a subtree as CSV (UTF-8 with BOM so Excel opens it correctly).
    pub fn write_csv(&self, root: u32, path: &std::path::Path, done: &AtomicU64) -> std::io::Result<u64> {
        let mut w = std::io::BufWriter::with_capacity(1 << 20, std::fs::File::create(path)?);
        w.write_all("\u{feff}".as_bytes())?;
        writeln!(w, "\"Path\",\"Type\",\"Size\",\"Allocated\",\"Modified\",\"Files\",\"Folders\"")?;
        let mut rows = 0u64;
        let mut stack = vec![(root, self.full_path(root))];
        while let Some((id, path)) = stack.pop() {
            let n = &self.nodes[id as usize];
            let modified = crate::app::fmt_iso(n.modified);
            writeln!(
                w,
                "\"{}\",{},{},{},{},{},{}",
                path.replace('"', "\"\""),
                if n.is_dir() { "Folder" } else { "File" },
                n.size,
                n.alloc,
                modified,
                if n.is_dir() { n.files.to_string() } else { String::new() },
                if n.is_dir() { n.folders.to_string() } else { String::new() },
            )?;
            rows += 1;
            if rows.is_multiple_of(50_000) {
                done.store(rows, Ordering::Relaxed);
            }
            if n.is_dir() {
                let base = path.trim_end_matches('\\');
                for &c in self.children(id).iter().rev() {
                    let p = if id == ROOT { self.name(c).to_string() } else { format!("{base}\\{}", self.name(c)) };
                    stack.push((c, p));
                }
            }
        }
        w.flush()?;
        done.store(rows, Ordering::Relaxed);
        Ok(rows)
    }
}

/// Query Everything for files changed since `since` (local time stamp).
pub fn query_changes(since: &str) -> Result<Vec<Change>, String> {
    let mut out = Vec::new();
    let (mut dir, mut name) = (String::new(), String::new());
    everything::for_each_item(
        &format!("file: rc:>={since}"),
        |it| {
            if it.is_dir {
                return;
            }
            decode(&mut dir, it.path);
            decode(&mut name, it.name);
            out.push(Change { dir: dir.clone(), name: name.clone(), size: it.size.unwrap_or(0), modified: it.modified });
        },
        |_, _| {},
    )?;
    Ok(out)
}
