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

fn ancestor(a: &str, b: &str) -> bool {
    let (a, b) = (a.to_lowercase(), b.to_lowercase());
    b.len() > a.len() && b.starts_with(&a) && b.as_bytes().get(a.len()) == Some(&b'\\')
}

fn skip_name(name: &str) -> bool {
    name.eq_ignore_ascii_case(".git")
        || name.eq_ignore_ascii_case("$RECYCLE.BIN")
        || name.eq_ignore_ascii_case("System Volume Information")
        || name.eq_ignore_ascii_case("Windows")
        || name.eq_ignore_ascii_case("WinSxS")
}

fn hash_dir(t: &Tree, id: u32, memo: &mut HashMap<u32, u64>) -> u64 {
    if let Some(&h) = memo.get(&id) {
        return h;
    }
    let mut parts: Vec<(String, u64)> = Vec::new();
    for &c in t.children(id) {
        let n = &t.nodes[c as usize];
        let name = t.name(c).to_ascii_lowercase();
        if n.is_dir() {
            if skip_name(&name) {
                continue;
            }
            let h = hash_dir(t, c, memo);
            parts.push((format!("d:{name}"), h));
        } else {
            parts.push((format!("f:{name}"), n.size));
        }
    }
    parts.sort();
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
    while let Some(id) = stack.pop() {
        if id != ROOT && t.nodes[id as usize].is_dir() {
            let name = t.name(id);
            if skip_name(name) {
                continue;
            }
            let n = &t.nodes[id as usize];
            if n.files >= 2 && n.size > 0 {
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

    // Una carpeta dentro de otra duplicada duplicaría los bytes. Se marca el
    // grupo anidado y se le pone redundancia 0 para no inflar el total.
    let all_paths: Vec<String> = out
        .iter()
        .flat_map(|g| g["items"].as_array().into_iter().flatten())
        .filter_map(|i| i["path"].as_str().map(|s| s.to_string()))
        .collect();
    for g in &mut out {
        let Some(items) = g["items"].as_array() else { continue };
        let nested = items.iter().any(|i| {
            let p = i["path"].as_str().unwrap_or("");
            all_paths.iter().any(|q| q != p && ancestor(q, p))
        });
        if nested {
            g["nested"] = json!(true);
            g["redundant_bytes"] = json!(0u64);
        }
    }
    out.sort_by(|a, b| b["redundant_bytes"].as_u64().cmp(&a["redundant_bytes"].as_u64()));
    let shown = out.len().min(limit);
    out.truncate(limit);
    let redundant: u64 = out.iter().map(|g| g["redundant_bytes"].as_u64().unwrap_or(0)).sum();

    json!({
        "kind": "duplicate_folders",
        "path": t.full_path(root),
        "folders": scanned,
        "groups": shown,
        "items": out,
        "redundant_bytes": redundant,
        "partial": scanned >= cap,
        "mode": if deep { "deep" } else { "fast" },
        "elapsed_ms": started.elapsed().as_millis() as u64,
        "note": "Comparación estructural (nombres + tamaños). No borra nada.",
        "notes": [
            "Se ignoran .git, Papelera, System Volume Information, Windows y WinSxS.",
            "Dos carpetas pueden coincidir en estructura y no en contenido: revisá antes de actuar.",
        ],
    })
}
