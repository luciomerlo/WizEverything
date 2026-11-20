//! "Why is it full?" and "what grew?".
//!
//! The first question needs only the index: top folders/files and the path that
//! leads to the biggest leaf. The second compares the two most recent snapshots
//! (`.wzsnap`) and attributes each added/grown file to its parent folder, so the
//! answer is a ranked list of folders instead of a pile of filenames.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use crate::snapshot::{self, Entry, Snapshot};
use crate::tree::{self, Tree, ROOT};

fn pct(part: u64, total: u64) -> f64 {
    if total == 0 {
        0.0
    } else {
        (part as f64 * 1000.0 / total as f64).round() / 10.0
    }
}

/// Top folders/files and the deepest "path to the bottom" for the current root.
pub fn why_full(t: &Tree, root: u32, limit: usize) -> Value {
    let n = &t.nodes[root as usize];
    let total = n.size.max(1);
    let mut folders: Vec<Value> = t
        .children(root)
        .iter()
        .filter(|&&c| t.nodes[c as usize].is_dir())
        .map(|&c| {
            let cn = &t.nodes[c as usize];
            json!({
                "path": t.full_path(c),
                "name": t.name(c),
                "bytes": cn.size,
                "alloc": cn.alloc,
                "files": cn.files,
                "folders": cn.folders,
                "pct": pct(cn.size, total),
            })
        })
        .collect();
    folders.sort_by(|a, b| b["bytes"].as_u64().cmp(&a["bytes"].as_u64()));
    folders.truncate(limit);

    let files: Vec<Value> = t
        .largest_files(root, "", limit)
        .iter()
        .map(|&id| {
            let fn_ = &t.nodes[id as usize];
            json!({
                "path": t.full_path(id),
                "bytes": fn_.size,
                "pct": pct(fn_.size, total),
            })
        })
        .collect();

    // Follow the biggest child down to show where the space really goes.
    let mut chain: Vec<Value> = Vec::new();
    let mut cur = root;
    for _ in 0..6 {
        let kids = t.children(cur);
        let Some(&next) = kids.iter().max_by_key(|&&k| t.nodes[k as usize].size) else {
            break;
        };
        let kn = &t.nodes[next as usize];
        if kn.size == 0 {
            break;
        }
        chain.push(json!({
            "path": t.full_path(next),
            "name": t.name(next),
            "bytes": kn.size,
            "pct": pct(kn.size, total),
            "is_dir": kn.is_dir(),
        }));
        if !kn.is_dir() {
            break;
        }
        cur = next;
    }

    let drive_free = t
        .drive_of(root)
        .and_then(|d| tree::disk_space(t.name(d)))
        .map(|(total, free)| json!({ "free": free, "total": total, "pct_free": pct(free, total) }))
        .unwrap_or(Value::Null);

    json!({
        "kind": "why",
        "path": t.full_path(root),
        "total_bytes": n.size,
        "total_alloc": n.alloc,
        "files": n.files,
        "folders": n.folders,
        "top_folders": folders,
        "top_files": files,
        "chain": chain,
        "drive": drive_free,
        "notes": [
            "Los porcentajes son sobre el tamaño lógico de la carpeta analizada.",
            "La cadena sigue siempre el hijo más grande: es el camino que explica el espacio.",
            "Para crecimientos hace falta al menos un snapshot anterior (¿Qué creció? → Guardar snapshot)."
        ],
    })
}

/// `%LOCALAPPDATA%\WizEverything\snapshots` (same folder the MCP tools use).
pub fn snapshots_dir() -> Result<PathBuf, String> {
    let base = std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("APPDATA").map(PathBuf::from))
        .ok_or("LOCALAPPDATA is not set")?;
    let dir = base.join("WizEverything").join("snapshots");
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    Ok(dir)
}

/// The two newest snapshots (old, new).
pub fn latest_two() -> Result<(PathBuf, PathBuf), String> {
    let dir = snapshots_dir()?;
    let mut files: Vec<PathBuf> = std::fs::read_dir(&dir)
        .map_err(|e| e.to_string())?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().and_then(|x| x.to_str()) == Some("wzsnap"))
        .collect();
    files.sort_by(|a, b| a.file_name().cmp(&b.file_name()));
    if files.len() < 2 {
        return Err(format!(
            "Hacen falta dos snapshots para comparar; hay {}.",
            files.len()
        ));
    }
    let n = files.len();
    Ok((files[n - 2].clone(), files[n - 1].clone()))
}

pub fn load(path: &Path) -> Result<Snapshot, String> {
    let bytes = std::fs::read(path).map_err(|e| e.to_string())?;
    snapshot::decode(&bytes).map_err(|e| e.to_string())
}

/// Diff plus path resolution and per-folder growth, ready for the UI/MCP.
pub fn growth(t: &Tree, old: &Snapshot, new: &Snapshot, limit: usize, deep: bool) -> Value {
    let started = std::time::Instant::now();
    let d = snapshot::diff(old, new);

    // Fast mode resolves the biggest changes first; deep resolves everything.
    const FAST_ENTRIES: usize = 2_000;
    let mut cand: Vec<(u64, u64, Option<u64>)> =
        Vec::with_capacity(d.added.len() + d.grown.len() + d.shrunk.len() + d.removed.len());
    for e in &d.added {
        cand.push((e.logical_size, e.path_hash, Some(e.parent_hash)));
    }
    for c in &d.grown {
        cand.push((c.new_size, c.path_hash, None));
    }
    for c in &d.shrunk {
        cand.push((c.old_size, c.path_hash, None));
    }
    for e in &d.removed {
        cand.push((e.logical_size, 0, Some(e.parent_hash)));
    }
    cand.sort_by(|a, b| b.0.cmp(&a.0));
    if !deep {
        cand.truncate(FAST_ENTRIES);
    }
    let mut needed: HashSet<u64> = HashSet::with_capacity(cand.len() * 2);
    for (_, path_hash, parent) in cand {
        if path_hash != 0 {
            needed.insert(path_hash);
        }
        if let Some(p) = parent {
            needed.insert(p);
        }
    }
    let mut file_path: HashMap<u64, String> = HashMap::new();
    let mut dir_path: HashMap<u64, String> = HashMap::new();
    for id in t.files_under(ROOT) {
        let p = t.full_path(id);
        let e = Entry::from_path(Path::new(&p), 0, 0, 0, 0);
        if needed.contains(&e.path_hash) {
            file_path.insert(e.path_hash, p.clone());
        }
        if needed.contains(&e.parent_hash)
            && let Some(parent) = Path::new(&p).parent()
        {
            dir_path.insert(e.parent_hash, parent.to_string_lossy().to_string());
        }
    }

    let mut by_folder: HashMap<String, i64> = HashMap::new();
    let mut new_files: Vec<Value> = Vec::new();
    for e in &d.added {
        let dir = dir_path
            .get(&e.parent_hash)
            .cloned()
            .unwrap_or_else(|| "(carpeta desconocida)".into());
        *by_folder.entry(dir).or_default() += e.logical_size as i64;
        if let Some(p) = file_path.get(&e.path_hash) {
            new_files.push(json!({ "path": p, "bytes": e.logical_size }));
        }
    }
    let mut grown_files: Vec<Value> = Vec::new();
    for c in &d.grown {
        if let Some(p) = file_path.get(&c.path_hash) {
            let dir = Path::new(p)
                .parent()
                .map(|d| d.to_string_lossy().to_string())
                .unwrap_or_else(|| "(carpeta desconocida)".into());
            *by_folder.entry(dir).or_default() += c.delta();
            grown_files.push(json!({
                "path": p,
                "from": c.old_size,
                "to": c.new_size,
                "delta": c.delta(),
            }));
        }
    }
    // Shrunk files still exist: attribute the negative delta to their folder.
    for c in &d.shrunk {
        if let Some(p) = file_path.get(&c.path_hash) {
            let dir = Path::new(p)
                .parent()
                .map(|d| d.to_string_lossy().to_string())
                .unwrap_or_else(|| "(carpeta desconocida)".into());
            *by_folder.entry(dir).or_default() += c.delta();
        }
    }
    for e in &d.removed {
        let dir = dir_path
            .get(&e.parent_hash)
            .cloned()
            .unwrap_or_else(|| "(carpeta desconocida)".into());
        *by_folder.entry(dir).or_default() -= e.logical_size as i64;
    }

    let mut folders: Vec<(String, i64)> = by_folder.into_iter().collect();
    folders.sort_by(|a, b| b.1.abs().cmp(&a.1.abs()));
    folders.truncate(limit);
    new_files.sort_by(|a, b| b["bytes"].as_u64().cmp(&a["bytes"].as_u64()));
    new_files.truncate(limit);
    grown_files.sort_by(|a, b| b["delta"].as_i64().cmp(&a["delta"].as_i64()));
    grown_files.truncate(limit);

    let resolved = d.added.iter().filter(|e| file_path.contains_key(&e.path_hash)).count()
        + d.grown.iter().filter(|c| file_path.contains_key(&c.path_hash)).count()
        + d.shrunk.iter().filter(|c| file_path.contains_key(&c.path_hash)).count();
    let total_diff = d.added.len() + d.grown.len() + d.shrunk.len() + d.removed.len();
    let attributed_pct = if total_diff == 0 {
        100.0
    } else {
        resolved as f64 * 100.0 / total_diff as f64
    };
    json!({
        "kind": "growth",
        "attributed_pct": (attributed_pct * 10.0).round() / 10.0,
        "elapsed_ms": started.elapsed().as_millis() as u64,
        "old": { "path": "", "created": old.created, "entries": old.entries.len() },
        "new": { "created": new.created, "entries": new.entries.len() },
        "added": d.added.len(),
        "removed": d.removed.len(),
        "grown": d.grown.len(),
        "shrunk": d.shrunk.len(),
        "net_bytes": d.net_bytes,
        "folders": folders.iter().map(|(f, b)| json!({ "folder": f, "bytes": b })).collect::<Vec<_>>(),
        "new_files": new_files,
        "grown_files": grown_files,
        "notes": [
            "Cada archivo nuevo o que creció se atribuye a la carpeta que lo contiene.",
            "Los negativos son archivos que se achicaron o se borraron desde el snapshot anterior.",
            "Comparación por tamaño lógico; los snapshots no guardan hashes de contenido.",
        ],
    })
}
