//! Carpetas duplicadas: agrupa directorios con la misma estructura y tamaños.
//!
//! Es una comparación **estructural** (nombres relativos + tamaños), no de
//! contenido byte a byte. Sirve para detectar copias de fotos, backups o
//! proyectos repetidos sin leer todos los archivos. Nunca borra nada.

use std::collections::{HashMap, hash_map::DefaultHasher};
use std::hash::{Hash, Hasher};

use serde_json::{json, Value};

use crate::tree::{ROOT, Tree};

const FAST_DIRS: usize = 20_000;
const DEEP_DIRS: usize = 200_000;

fn is_drive_child(t: &Tree, id: u32) -> bool {
    let p = t.nodes[id as usize].parent;
    p != ROOT && t.nodes[p as usize].parent == ROOT
}

fn skip_id(t: &Tree, id: u32) -> bool {
    let name = t.name(id);
    if name.eq_ignore_ascii_case(".git")
        || name.eq_ignore_ascii_case("$RECYCLE.BIN")
        || name.eq_ignore_ascii_case("System Volume Information")
    {
        return true;
    }
    // Windows/WinSxS sólo en la raíz de una unidad, no en una carpeta de usuario
    // llamada "Windows".
    is_drive_child(t, id)
        && (name.eq_ignore_ascii_case("Windows") || name.eq_ignore_ascii_case("WinSxS"))
}

fn ancestor_owner(path: &str, map: &HashMap<String, usize>) -> Option<usize> {
    let mut s = path;
    while let Some(i) = s.rfind('\\') {
        s = &s[..i];
        if s.is_empty() {
            break;
        }
        if let Some(&g) = map.get(&s.to_lowercase()) {
            return Some(g);
        }
    }
    None
}

/// Hash of a child name + kind, without allocating a lowercased copy.
///
/// The previous version did `format!("d:{name}")` and sorted Strings for every
/// child; with 100k files that was 100k+ allocations. Two u64s keep the same
/// structural meaning (kind + case-insensitive name + size/child hash) and the
/// same 64-bit collision profile as before.
fn name_key(name: &str, dir: bool) -> u64 {
    let mut h = DefaultHasher::new();
    h.write_u8(if dir { b'd' } else { b'f' });
    for b in name.bytes() {
        h.write_u8(b.to_ascii_lowercase());
    }
    h.finish()
}

fn hash_dir(t: &Tree, id: u32, memo: &mut HashMap<u32, u64>) -> u64 {
    if let Some(&h) = memo.get(&id) {
        return h;
    }
    let mut parts: Vec<(u64, u64)> = Vec::with_capacity(t.children(id).len());
    for &c in t.children(id) {
        let n = &t.nodes[c as usize];
        let key = name_key(t.name(c), n.is_dir());
        if n.is_dir() {
            if skip_id(t, c) {
                continue;
            }
            parts.push((key, hash_dir(t, c, memo)));
        } else {
            parts.push((key, n.size));
        }
    }
    parts.sort_unstable();
    let mut hasher = DefaultHasher::new();
    parts.hash(&mut hasher);
    let h = hasher.finish();
    memo.insert(id, h);
    h
}

/// Busca carpetas con la misma firma bajo `root`.
pub fn duplicate_folders(t: &Tree, root: u32, limit: usize, deep: bool) -> Value {
    let started = std::time::Instant::now();
    let cap = if deep { DEEP_DIRS } else { FAST_DIRS };
    let mut memo: HashMap<u32, u64> = HashMap::new();
    let mut groups: HashMap<u64, Vec<u32>> = HashMap::new();
    let mut stack = vec![root];
    let mut scanned = 0usize;
    let mut skipped = 0usize;
    while let Some(id) = stack.pop() {
        if id != ROOT && t.nodes[id as usize].is_dir() {
            if skip_id(t, id) {
                skipped += 1;
                continue;
            }
            let n = &t.nodes[id as usize];
            // Una carpeta con un solo archivo grande también puede ser una copia.
            if n.files >= 1 && n.size > 0 {
                let h = hash_dir(t, id, &mut memo);
                groups.entry(h).or_default().push(id);
                scanned += 1;
                if scanned >= cap {
                    break;
                }
            }
        }
        for &c in t.children(id) {
            if t.nodes[c as usize].is_dir() {
                stack.push(c);
            }
        }
    }

    let mut out: Vec<Value> = groups
        .into_values()
        .filter(|ids| ids.len() > 1)
        .map(|ids| {
            let mut items: Vec<Value> = ids
                .iter()
                .map(|&id| {
                    let n = &t.nodes[id as usize];
                    json!({
                        "path": t.full_path(id),
                        "bytes": n.size,
                        "files": n.files,
                        "modified": n.modified,
                    })
                })
                .collect();
            items.sort_by(|a, b| b["bytes"].as_u64().cmp(&a["bytes"].as_u64()));
            let total: u64 = items.iter().map(|i| i["bytes"].as_u64().unwrap_or(0)).sum();
            let max = items.first().and_then(|i| i["bytes"].as_u64()).unwrap_or(0);
            let keep = items
                .iter()
                .max_by_key(|i| i["modified"].as_u64().unwrap_or(0))
                .and_then(|i| i["path"].as_str())
                .unwrap_or("")
                .to_string();
            json!({
                "copies": items.len(),
                "total_bytes": total,
                "redundant_bytes": total.saturating_sub(max),
                "keep_suggestion": keep,
                "items": items,
                "note": "Misma estructura y tamaños; verificar contenido antes de borrar.",
            })
        })
        .collect();

    // Resolver anidamientos con un mapa de rutas, no comparando todos contra
    // todos: con 20.000 carpetas eso era O(N²) y congelaba la UI.
    let mut path_group: HashMap<String, usize> = HashMap::new();
    for (gi, g) in out.iter().enumerate() {
        if let Some(items) = g["items"].as_array() {
            for item in items {
                if let Some(p) = item["path"].as_str() {
                    path_group.insert(p.to_lowercase(), gi);
                }
            }
        }
    }
    for gi in 0..out.len() {
        let paths: Vec<String> = out[gi]["items"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|i| i["path"].as_str().map(|s| s.to_string()))
            .collect();
        let owners: Vec<Option<usize>> = paths
            .iter()
            .map(|p| ancestor_owner(p, &path_group).filter(|&o| o != gi))
            .collect();
        let nested_count = owners.iter().filter(|o| o.is_some()).count();
        let all_nested = nested_count == paths.len() && paths.len() > 1;
        let same_owner = owners.iter().flatten().copied().collect::<std::collections::HashSet<_>>().len() == 1;
        let suppressed = all_nested && same_owner;

        let (mut effective_total, mut effective_max, mut effective_n) = (0u64, 0u64, 0usize);
        if let Some(items) = out[gi]["items"].as_array() {
            for (i, item) in items.iter().enumerate() {
                if suppressed || owners.get(i).and_then(|o| *o).is_none() {
                    let b = item["bytes"].as_u64().unwrap_or(0);
                    effective_total += b;
                    effective_max = effective_max.max(b);
                    effective_n += 1;
                }
            }
        }
        let redundant = if suppressed { 0 } else { effective_total.saturating_sub(effective_max) };
        if let Some(items) = out[gi]["items"].as_array_mut() {
            for (i, item) in items.iter_mut().enumerate() {
                item["nested"] = json!(owners.get(i).and_then(|o| *o).is_some());
            }
        }
        out[gi]["nested"] = json!(suppressed);
        out[gi]["nested_copies"] = json!(nested_count);
        out[gi]["effective_copies"] = json!(if suppressed { 0 } else { effective_n });
        out[gi]["redundant_bytes"] = json!(redundant);
    }

    out.sort_by(|a, b| b["redundant_bytes"].as_u64().cmp(&a["redundant_bytes"].as_u64()));
    let total_groups = out.len();
    let redundant: u64 = out.iter().map(|g| g["redundant_bytes"].as_u64().unwrap_or(0)).sum();
    let shown = total_groups.min(limit);
    out.truncate(limit);

    json!({
        "kind": "duplicate_folders",
        "path": t.full_path(root),
        "folders": scanned,
        "groups": total_groups,
        "shown": shown,
        "items": out,
        "redundant_bytes": redundant,
        "partial": scanned >= cap,
        "truncated": total_groups > shown,
        "skipped_folders": skipped,
        "mode": if deep { "deep" } else { "fast" },
        "elapsed_ms": started.elapsed().as_millis() as u64,
        "note": "Comparación estructural (nombres + tamaños). No borra nada.",
        "notes": [
            "Se ignoran .git, Papelera, System Volume Information y Windows/WinSxS en la raíz de una unidad.",
            "Los grupos anidados dentro de otro grupo se marcan nested y no suman bytes redundantes.",
            "Dos carpetas pueden coincidir en estructura y no en contenido: revisá antes de actuar.",
        ],
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ancestor_owner_uses_nearest_path() {
        let mut map = HashMap::new();
        map.insert(r"c:\a".to_string(), 0);
        map.insert(r"c:\a\sub".to_string(), 1);
        assert_eq!(ancestor_owner(r"C:\a\sub\f.bin", &map), Some(1));
        assert_eq!(ancestor_owner(r"C:\b\f.bin", &map), None);
    }
}
