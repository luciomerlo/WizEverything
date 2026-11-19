//! Árbol de carpetas/archivos construido a partir del índice de Everything.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

use crate::everything;

pub const NO_EXT: u32 = u32::MAX;
pub const ROOT: u32 = 0;

pub struct Node {
    pub name: Box<str>,
    pub parent: u32,
    /// Índice de extensión para archivos; `NO_EXT` para carpetas.
    pub ext: u32,
    pub is_dir: bool,
    pub size: u64,
    pub alloc: u64,
    pub modified: u64,
    pub files: u32,
    pub folders: u32,
    /// Hijos ordenados por tamaño descendente (solo carpetas).
    pub children: Vec<u32>,
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
    pub exts: Vec<ExtInfo>,
    pub load_ms: u128,
    pub built_at: std::time::Instant,
}

#[derive(Default)]
pub struct Progress {
    pub done: AtomicU32,
    pub total: AtomicU32,
    /// true mientras se espera a que Everything arranque / termine de cargar su base.
    pub waiting: std::sync::atomic::AtomicBool,
}

fn cluster_size(drive: &str) -> u64 {
    use windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceW;
    let root = everything::wide(&format!("{}\\", drive.trim_end_matches('\\')));
    let (mut spc, mut bps, mut free, mut total) = (0u32, 0u32, 0u32, 0u32);
    let ok = unsafe { GetDiskFreeSpaceW(root.as_ptr(), &mut spc, &mut bps, &mut free, &mut total) };
    if ok != 0 && spc > 0 && bps > 0 { spc as u64 * bps as u64 } else { 4096 }
}

/// Espacio (total, libre) de una unidad, si se puede consultar.
pub fn disk_space(drive: &str) -> Option<(u64, u64)> {
    use windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;
    let root = everything::wide(&format!("{}\\", drive.trim_end_matches('\\')));
    let (mut avail, mut total, mut free) = (0u64, 0u64, 0u64);
    let ok = unsafe { GetDiskFreeSpaceExW(root.as_ptr(), &mut avail, &mut total, &mut free) };
    if ok != 0 { Some((total, free)) } else { None }
}

struct Builder {
    nodes: Vec<Node>,
    dirs: HashMap<Box<str>, u32>,
    ext_ids: HashMap<String, u32>,
    ext_names: Vec<String>,
    last_dir: (String, u32),
}

impl Builder {
    fn new_node(&mut self, name: &str, parent: u32, is_dir: bool) -> u32 {
        let id = self.nodes.len() as u32;
        self.nodes.push(Node {
            name: name.into(),
            parent,
            ext: NO_EXT,
            is_dir,
            size: 0,
            alloc: 0,
            modified: 0,
            files: 0,
            folders: 0,
            children: Vec::new(),
        });
        if parent != u32::MAX {
            self.nodes[parent as usize].children.push(id);
        }
        id
    }

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
            let id = self.new_node(name, parent, true);
            self.dirs.insert(path.into(), id);
            id
        };
        self.last_dir = (path.to_string(), id);
        id
    }

    fn ext(&mut self, name: &str) -> u32 {
        let ext = match name.rfind('.') {
            Some(i) if i > 0 && i + 1 < name.len() => name[i + 1..].to_lowercase(),
            _ => String::new(),
        };
        if let Some(&id) = self.ext_ids.get(&ext) {
            return id;
        }
        let id = self.ext_names.len() as u32;
        self.ext_names.push(ext.clone());
        self.ext_ids.insert(ext, id);
        id
    }
}

pub fn load(progress: Arc<Progress>) -> Result<Tree, String> {
    let start = std::time::Instant::now();
    let mut b = Builder {
        nodes: Vec::with_capacity(2_500_000),
        dirs: HashMap::with_capacity(500_000),
        ext_ids: HashMap::new(),
        ext_names: Vec::new(),
        last_dir: (String::from("\0"), 0),
    };
    b.new_node("Este equipo", u32::MAX, true);

    let mut path = String::new();
    let mut name = String::new();
    everything::for_each_item(
        "",
        |it| {
            path.clear();
            name.clear();
            path.extend(char::decode_utf16(it.path.iter().copied()).map(|c| c.unwrap_or('\u{FFFD}')));
            name.extend(char::decode_utf16(it.name.iter().copied()).map(|c| c.unwrap_or('\u{FFFD}')));
            if it.is_dir {
                let full = if path.is_empty() { name.clone() } else { format!("{}\\{}", path.trim_end_matches('\\'), name) };
                let id = b.dir(&full);
                b.nodes[id as usize].modified = it.modified;
            } else {
                let parent = if path.is_empty() { ROOT } else { b.dir(&path) };
                let ext = b.ext(&name);
                let id = b.new_node(&name, parent, false);
                let n = &mut b.nodes[id as usize];
                n.ext = ext;
                n.size = it.size.unwrap_or(0);
                n.modified = it.modified;
            }
        },
        |done, total| {
            progress.done.store(done, Ordering::Relaxed);
            progress.total.store(total, Ordering::Relaxed);
        },
    )?;

    let Builder { mut nodes, ext_names, .. } = b;

    // Tamaño asignado aproximado: redondeo al tamaño de clúster de cada unidad.
    let roots: Vec<u32> = nodes[ROOT as usize].children.clone();
    for &r in &roots {
        let cs = cluster_size(&nodes[r as usize].name);
        let mut stack = vec![r];
        while let Some(id) = stack.pop() {
            let n = &mut nodes[id as usize];
            if n.is_dir {
                stack.extend_from_slice(&n.children);
            } else {
                n.alloc = n.size.div_ceil(cs) * cs;
            }
        }
    }

    // Recorrido post-orden para acumular tamaños y contadores.
    let mut order = Vec::with_capacity(nodes.len());
    let mut stack = vec![ROOT];
    while let Some(id) = stack.pop() {
        order.push(id);
        stack.extend_from_slice(&nodes[id as usize].children);
    }
    for &id in order.iter().rev() {
        let (parent, is_dir, size, alloc, files, folders, modified) = {
            let n = &nodes[id as usize];
            (n.parent, n.is_dir, n.size, n.alloc, n.files, n.folders, n.modified)
        };
        if is_dir {
            // ordenar hijos por tamaño descendente
            let mut ch = std::mem::take(&mut nodes[id as usize].children);
            ch.sort_unstable_by(|a, b| nodes[*b as usize].size.cmp(&nodes[*a as usize].size));
            nodes[id as usize].children = ch;
        }
        if parent != u32::MAX {
            let p = &mut nodes[parent as usize];
            p.size += size;
            p.alloc += alloc;
            p.files += files + u32::from(!is_dir);
            p.folders += folders + u32::from(is_dir);
            if modified > p.modified && !is_dir {
                // la fecha de la carpeta la da Everything; solo se usa si falta
                if p.modified == 0 {
                    p.modified = modified;
                }
            }
        }
    }

    let mut exts: Vec<ExtInfo> = ext_names
        .into_iter()
        .map(|name| ExtInfo { name, size: 0, alloc: 0, count: 0, color: [0; 3] })
        .collect();
    for n in &nodes {
        if !n.is_dir {
            let e = &mut exts[n.ext as usize];
            e.size += n.size;
            e.alloc += n.alloc;
            e.count += 1;
        }
    }
    assign_colors(&mut exts);

    Ok(Tree { nodes, exts, load_ms: start.elapsed().as_millis(), built_at: std::time::Instant::now() })
}

/// Paleta tipo WizTree: colores saturados para las extensiones más pesadas,
/// el resto con tonos derivados del nombre.
fn assign_colors(exts: &mut [ExtInfo]) {
    const PALETTE: [[u8; 3]; 24] = [
        [0, 92, 255], [255, 40, 40], [40, 220, 40], [255, 220, 0], [0, 220, 255], [255, 0, 220],
        [255, 140, 0], [140, 80, 255], [0, 160, 110], [190, 190, 0], [255, 120, 160], [100, 180, 255],
        [160, 60, 0], [0, 120, 160], [200, 0, 90], [120, 220, 140], [90, 90, 200], [230, 160, 90],
        [0, 200, 180], [180, 110, 200], [150, 150, 150], [220, 90, 60], [60, 140, 40], [240, 200, 140],
    ];
    let mut order: Vec<usize> = (0..exts.len()).collect();
    order.sort_by(|a, b| exts[*b].size.cmp(&exts[*a].size));
    for (rank, &i) in order.iter().enumerate() {
        exts[i].color = if rank < PALETTE.len() {
            PALETTE[rank]
        } else {
            let h = exts[i].name.bytes().fold(2166136261u32, |h, c| (h ^ c as u32).wrapping_mul(16777619));
            hsv((h % 360) as f32, 0.45 + (h >> 9 & 0xff) as f32 / 255.0 * 0.3, 0.75)
        };
    }
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
    pub fn full_path(&self, mut id: u32) -> String {
        let mut parts = Vec::new();
        while id != ROOT && id != u32::MAX {
            parts.push(&*self.nodes[id as usize].name);
            id = self.nodes[id as usize].parent;
        }
        parts.reverse();
        let mut s = parts.join("\\");
        if parts.len() == 1 {
            s.push('\\');
        }
        s
    }

    /// Ruta de ancestros desde la raíz (incluido `id`).
    pub fn ancestors(&self, mut id: u32) -> Vec<u32> {
        let mut v = vec![id];
        while id != ROOT {
            id = self.nodes[id as usize].parent;
            v.push(id);
        }
        v.reverse();
        v
    }

    pub fn ext_label(&self, id: u32) -> &str {
        let e = &self.exts[id as usize].name;
        if e.is_empty() { "(sin extensión)" } else { e }
    }

    /// Color representativo de una carpeta: el del archivo más grande que contiene.
    pub fn dominant_color(&self, mut id: u32) -> [u8; 3] {
        loop {
            let n = &self.nodes[id as usize];
            if !n.is_dir {
                return self.exts[n.ext as usize].color;
            }
            match n.children.first() {
                Some(&c) => id = c,
                None => return [110, 110, 110],
            }
        }
    }

    /// Estadísticas por extensión restringidas a un subárbol.
    pub fn ext_stats(&self, root: u32) -> Vec<(u32, u64, u64)> {
        if root == ROOT {
            return (0..self.exts.len() as u32)
                .map(|i| (i, self.exts[i as usize].size, self.exts[i as usize].count))
                .filter(|e| e.2 > 0)
                .collect::<Vec<_>>()
                .into_iter()
                .collect();
        }
        let mut acc: HashMap<u32, (u64, u64)> = HashMap::new();
        let mut stack = vec![root];
        while let Some(id) = stack.pop() {
            let n = &self.nodes[id as usize];
            if n.is_dir {
                stack.extend_from_slice(&n.children);
            } else {
                let e = acc.entry(n.ext).or_default();
                e.0 += n.size;
                e.1 += 1;
            }
        }
        acc.into_iter().map(|(k, (s, c))| (k, s, c)).collect()
    }

    /// Los `limit` archivos más grandes del subárbol (opcionalmente filtrados).
    pub fn largest_files(&self, root: u32, filter: &str, limit: usize) -> Vec<u32> {
        let filter = filter.to_lowercase();
        let mut heap = std::collections::BinaryHeap::new();
        let mut stack = vec![root];
        while let Some(id) = stack.pop() {
            let n = &self.nodes[id as usize];
            if n.is_dir {
                stack.extend_from_slice(&n.children);
                continue;
            }
            if !filter.is_empty() && !n.name.to_lowercase().contains(&filter) {
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
}
