//! Comparar dos máquinas/volúmenes a partir de sus snapshots `.wzsnap` o
//! catálogos `.ecat`. Cruza por hash de ruta y reporta qué falta, sobra o cambió
//! de tamaño. Sólo lectura.

use std::collections::HashMap;
use std::path::Path;

use serde_json::{json, Value};

use crate::snapshot::{Entry, Snapshot};

const MAX_FILE_BYTES: u64 = 512 * 1024 * 1024;

fn load(path: &str) -> Result<(String, Snapshot), String> {
    let meta = std::fs::metadata(path).map_err(|e| format!("No pude leer {path}: {e}"))?;
    if meta.len() > MAX_FILE_BYTES {
        return Err(format!("{path} es demasiado grande ({} MB); límite 512 MB.", meta.len() / (1024 * 1024)));
    }
    let bytes = std::fs::read(path).map_err(|e| format!("No pude leer {path}: {e}"))?;
    if let Ok(cat) = crate::catalog::Catalog::decode(&bytes) {
        return Ok((cat.volume_name, Snapshot::new(cat.created, cat.entries)));
    }
    let snap = crate::snapshot::decode(&bytes).map_err(|e| format!("{path}: {e}"))?;
    let label = Path::new(path)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("snapshot")
        .to_string();
    Ok((label, snap))
}

fn path_of(e: &Entry, map: &HashMap<u64, &Entry>) -> String {
    let mut parts = vec![e.name.clone()];
    let mut parent = e.parent_hash;
    for _ in 0..64 {
        let Some(p) = map.get(&parent) else { break };
        if p.name.is_empty() {
            break;
        }
        parts.push(p.name.clone());
        parent = p.parent_hash;
    }
    parts.reverse();
    parts.join("\\")
}

/// Compara A (máquina/volumen 1) contra B (máquina/volumen 2).
pub fn compare(a_path: &str, b_path: &str, limit: usize) -> Result<Value, String> {
    let (name_a, a) = load(a_path)?;
    let (name_b, b) = load(b_path)?;
    let map_a: HashMap<u64, &Entry> = a.entries.iter().map(|e| (e.path_hash, e)).collect();
    let map_b: HashMap<u64, &Entry> = b.entries.iter().map(|e| (e.path_hash, e)).collect();

    let files_a = a.entries.iter().filter(|e| !crate::snapshot::is_directory(e)).count();
    let files_b = b.entries.iter().filter(|e| !crate::snapshot::is_directory(e)).count();
    let mut matched = 0usize;
    let mut only_a: Vec<Value> = Vec::new();
    let mut only_b: Vec<Value> = Vec::new();
    let mut grown: Vec<Value> = Vec::new();
    let mut shrunk: Vec<Value> = Vec::new();
    let (mut only_a_bytes, mut only_b_bytes, mut grown_bytes, mut shrunk_bytes) = (0u64, 0u64, 0u64, 0u64);
    let (mut only_a_n, mut only_b_n, mut grown_n, mut shrunk_n) = (0u64, 0u64, 0u64, 0u64);

    for e in a.entries.iter().filter(|e| !crate::snapshot::is_directory(e)) {
        match map_b.get(&e.path_hash) {
            None => {
                only_a_n += 1;
                only_a_bytes += e.logical_size;
                if only_a.len() < limit {
                    only_a.push(json!({ "path": path_of(e, &map_a), "bytes": e.logical_size }));
                }
            }
            Some(o) => {
                matched += 1;
                if o.logical_size > e.logical_size {
                    grown_n += 1;
                    let d = o.logical_size - e.logical_size;
                    grown_bytes += d;
                    if grown.len() < limit {
                        grown.push(json!({ "path": path_of(e, &map_a), "from": e.logical_size, "to": o.logical_size, "bytes": d }));
                    }
                } else if o.logical_size < e.logical_size {
                    shrunk_n += 1;
                    let d = e.logical_size - o.logical_size;
                    shrunk_bytes += d;
                    if shrunk.len() < limit {
                        shrunk.push(json!({ "path": path_of(e, &map_a), "from": e.logical_size, "to": o.logical_size, "bytes": d }));
                    }
                }
            }
        }
    }
    for e in b.entries.iter().filter(|e| !crate::snapshot::is_directory(e)) {
        if !map_a.contains_key(&e.path_hash) {
            only_b_n += 1;
            only_b_bytes += e.logical_size;
            if only_b.len() < limit {
                only_b.push(json!({ "path": path_of(e, &map_b), "bytes": e.logical_size }));
            }
        }
    }

    let total_a = a.total_logical();
    let total_b = b.total_logical();
    let overlap = if files_a.min(files_b) == 0 {
        0.0
    } else {
        matched as f64 * 100.0 / files_a.min(files_b) as f64
    };
    let mut warnings: Vec<String> = Vec::new();
    if files_a.min(files_b) > 0 && overlap < 5.0 {
        warnings.push(format!(
            "Sólo {overlap:.1}% de las rutas coinciden: ¿distinta letra de unidad, usuario o raíz? La comparación por hash incluye la ruta completa."
        ));
    }
    Ok(json!({
        "kind": "machine_compare",
        "a": { "label": name_a, "file": a_path, "entries": a.entries.len(), "bytes": total_a },
        "b": { "label": name_b, "file": b_path, "entries": b.entries.len(), "bytes": total_b },
        "net_bytes": total_b as i64 - total_a as i64,
        "overlap_pct": (overlap * 10.0).round() / 10.0,
        "warnings": warnings,
        "only_a": { "files": only_a_n, "bytes": only_a_bytes, "top": only_a },
        "only_b": { "files": only_b_n, "bytes": only_b_bytes, "top": only_b },
        "grown": { "files": grown_n, "bytes": grown_bytes, "top": grown },
        "shrunk": { "files": shrunk_n, "bytes": shrunk_bytes, "top": shrunk },
        "note": "Comparación por hash de ruta: mismo usuario/unidad esperado. Sólo lectura.",
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_are_reconstructed() {
        let a = Entry::from_path(Path::new(r"C:\Users\me\foto.jpg"), 10, 10, 0, 0);
        let me = Entry::from_path(Path::new(r"C:\Users\me"), 0, 0, 0, 0);
        let users = Entry::from_path(Path::new(r"C:\Users"), 0, 0, 0, 0);
        let entries = vec![a.clone(), me, users];
        let map: HashMap<u64, &Entry> = entries.iter().map(|e| (e.path_hash, e)).collect();
        assert!(path_of(&a, &map).ends_with(r"Users\me\foto.jpg"));
    }
}
