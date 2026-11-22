//! Comparar dos máquinas/volúmenes a partir de sus snapshots `.wzsnap` o
//! catálogos `.ecat`. Cruza por hash de ruta y reporta qué falta, sobra o cambió
//! de tamaño. Sólo lectura.

use std::collections::HashMap;
use std::path::Path;

use serde_json::{json, Value};

use crate::snapshot::{Entry, Snapshot};

const MAX_FILE_BYTES: u64 = crate::snapshot::MAX_FILE_BYTES;

fn load(path: &str) -> Result<(String, Snapshot), String> {
    let meta = std::fs::metadata(path).map_err(|e| format!("No pude leer {path}: {e}"))?;
    if meta.len() > MAX_FILE_BYTES {
        return Err(format!(
            "{path} es demasiado grande ({} MB); límite 512 MB.",
            meta.len() / (1024 * 1024)
        ));
    }
    // Lectura acotada: reserva exacta según el tamaño y nunca crece más allá del tope.
    let bytes = crate::snapshot::read_bounded(Path::new(path), MAX_FILE_BYTES)
        .map_err(|e| format!("No pude leer {path}: {e}"))?;
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
    // Sólo tomamos prestados los nombres y unimos al final: una sola asignación.
    let mut parts: Vec<&str> = Vec::with_capacity(8);
    parts.push(&e.name);
    let mut parent = e.parent_hash;
    for _ in 0..64 {
        let Some(p) = map.get(&parent) else { break };
        if p.name.is_empty() {
            break;
        }
        parts.push(&p.name);
        let next = p.parent_hash;
        if next == parent {
            break;
        }
        parent = next;
    }
    parts.reverse();
    parts.join("\\")
}

/// Normaliza una raíz a quitar (`\` final fuera, minúsculas) una sola vez por lado.
/// `Some("")` significa "cruzar por ruta completa sin quitar raíz", igual que antes.
fn normalize_root(prefix: Option<&str>) -> Option<String> {
    Some(prefix?.trim_end_matches('\\').to_lowercase())
}

/// Quita una raíz ya normalizada (minúsculas, sin `\` final). No asigna si el
/// camino no empieza por ella; el camino se devuelve tal cual (preserva mayúsculas).
fn strip_normalized(path: &str, root: Option<&str>) -> String {
    let Some(pre) = root else {
        return path.to_string();
    };
    if pre.is_empty() {
        return path.to_string();
    }
    // Camino rápido: comparación de bytes sin asignar (cubre ASCII, el caso normal).
    let starts = path.is_char_boundary(pre.len())
        && path
            .as_bytes()
            .get(..pre.len())
            .is_some_and(|head| head.eq_ignore_ascii_case(pre.as_bytes()));
    if starts || path.to_lowercase().starts_with(pre) {
        return path.get(pre.len()..).unwrap_or(path).trim_start_matches('\\').to_string();
    }
    path.to_string()
}

fn key_of(e: &Entry, map: &HashMap<u64, &Entry>, root: Option<&str>) -> String {
    // Sin prefijo, el hash de ruta es exacto y evita colisiones de nombres.
    match root {
        None => format!("#{:016x}", e.path_hash),
        Some(_) => strip_normalized(&path_of(e, map), root).to_lowercase(),
    }
}

fn index_by_hash(entries: &[Entry]) -> HashMap<u64, &Entry> {
    let mut map = HashMap::with_capacity(entries.len());
    for e in entries {
        map.insert(e.path_hash, e);
    }
    map
}

/// Compara A (máquina/volumen 1) contra B (máquina/volumen 2).
pub fn compare(a_path: &str, b_path: &str, limit: usize) -> Result<Value, String> {
    compare_stripped(a_path, b_path, limit, None, None)
}

/// Igual que [`compare`], pero quita una raíz distinta en cada lado antes de
/// cruzar (`C:\Users\Lucio` vs `C:\Users\Otro`).
pub fn compare_stripped(
    a_path: &str,
    b_path: &str,
    limit: usize,
    strip_a: Option<&str>,
    strip_b: Option<&str>,
) -> Result<Value, String> {
    let (name_a, a) = load(a_path)?;
    let (name_b, b) = load(b_path)?;
    let map_a = index_by_hash(&a.entries);
    let map_b = index_by_hash(&b.entries);
    // Las raíces se normalizan una sola vez, no por archivo.
    let root_a = normalize_root(strip_a);
    let root_b = normalize_root(strip_b);
    // Una clave por archivo (antes se construían dos veces por lado) y un mapa
    // prestado para el cruce; el Vec conserva el orden de las entradas.
    let keys_a: Vec<(String, &Entry)> = a
        .entries
        .iter()
        .map(|e| (key_of(e, &map_a, root_a.as_deref()), e))
        .collect();
    let keys_b: Vec<(String, &Entry)> = b
        .entries
        .iter()
        .map(|e| (key_of(e, &map_b, root_b.as_deref()), e))
        .collect();
    let lookup_a: HashMap<&str, &Entry> = keys_a.iter().map(|(k, e)| (k.as_str(), *e)).collect();
    let lookup_b: HashMap<&str, &Entry> = keys_b.iter().map(|(k, e)| (k.as_str(), *e)).collect();

    let files_a = a.entries.iter().filter(|e| !crate::snapshot::is_directory(e)).count();
    let files_b = b.entries.iter().filter(|e| !crate::snapshot::is_directory(e)).count();
    let mut matched = 0usize;
    let mut only_a: Vec<Value> = Vec::new();
    let mut only_b: Vec<Value> = Vec::new();
    let mut grown: Vec<Value> = Vec::new();
    let mut shrunk: Vec<Value> = Vec::new();
    let (mut only_a_bytes, mut only_b_bytes, mut grown_bytes, mut shrunk_bytes) = (0u64, 0u64, 0u64, 0u64);
    let (mut only_a_n, mut only_b_n, mut grown_n, mut shrunk_n) = (0u64, 0u64, 0u64, 0u64);

    for (key, e) in keys_a.iter().filter(|(_, e)| !crate::snapshot::is_directory(e)) {
        match lookup_b.get(key.as_str()) {
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
    for (key, e) in keys_b.iter().filter(|(_, e)| !crate::snapshot::is_directory(e)) {
        if !lookup_a.contains_key(key.as_str()) {
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
        "strip_a": strip_a,
        "strip_b": strip_b,
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
    fn strip_prefix_removes_only_a_root() {
        let strip = |path: &str, prefix: Option<&str>| {
            let root = normalize_root(prefix);
            strip_normalized(path, root.as_deref())
        };
        assert_eq!(strip(r"C:\Users\me\a.bin", Some(r"C:\Users\me")), r"a.bin");
        assert_eq!(strip(r"D:\x\a.bin", Some(r"C:\Users\me")), r"D:\x\a.bin");
        // La raíz se normaliza (mayúsculas y `\` final) y se conserva el resto tal cual.
        assert_eq!(strip(r"C:\Users\Me\Sub\A.BIN", Some(r"c:\users\me\")), r"Sub\A.BIN");
        assert_eq!(strip(r"C:\x\a.bin", Some("\\")), r"C:\x\a.bin");
    }

    #[test]
    fn keys_are_stable_across_roots() {
        let entries = vec![
            Entry::from_path(Path::new(r"C:\Users\me\foto.jpg"), 10, 10, 0, 0),
            Entry::from_path(Path::new(r"C:\Users\me"), 0, 0, 0, 0),
            Entry::from_path(Path::new(r"C:\Users"), 0, 0, 0, 0),
        ];
        let map = index_by_hash(&entries);
        let root = normalize_root(Some(r"users\me"));
        assert_eq!(key_of(&entries[0], &map, root.as_deref()), "foto.jpg");
        assert_eq!(key_of(&entries[0], &map, None), format!("#{:016x}", entries[0].path_hash));
        // Raíz vacía: se cruza por ruta completa (minúsculas), sin quitar nada.
        let empty = normalize_root(Some("\\"));
        assert_eq!(key_of(&entries[0], &map, empty.as_deref()), r"users\me\foto.jpg");
    }

    #[test]
    fn paths_are_reconstructed() {
        let a = Entry::from_path(Path::new(r"C:\Users\me\foto.jpg"), 10, 10, 0, 0);
        let me = Entry::from_path(Path::new(r"C:\Users\me"), 0, 0, 0, 0);
        let users = Entry::from_path(Path::new(r"C:\Users"), 0, 0, 0, 0);
        let entries = vec![a.clone(), me, users];
        let map: HashMap<u64, &Entry> = entries.iter().map(|e| (e.path_hash, e)).collect();
        assert!(path_of(&a, &map).ends_with(r"Users\me\foto.jpg"));
    }

    #[test]
    fn compare_stripped_loads_snapshot_and_catalog() {
        let dir = std::env::temp_dir().join(format!("wz_cmp_e2e_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();

        let a = Snapshot::new(
            1,
            vec![
                Entry::from_path(Path::new(r"C:\Users"), 0, 0, 0, crate::snapshot::ATTR_DIRECTORY),
                Entry::from_path(Path::new(r"C:\Users\me"), 0, 0, 0, crate::snapshot::ATTR_DIRECTORY),
                Entry::from_path(Path::new(r"C:\Users\me\a.bin"), 10, 10, 0, 0),
                Entry::from_path(Path::new(r"C:\Users\me\b.bin"), 20, 20, 0, 0),
            ],
        );
        let b_entries = vec![
            Entry::from_path(Path::new(r"C:\Users"), 0, 0, 0, crate::snapshot::ATTR_DIRECTORY),
            Entry::from_path(Path::new(r"C:\Users\other"), 0, 0, 0, crate::snapshot::ATTR_DIRECTORY),
            Entry::from_path(Path::new(r"C:\Users\other\a.bin"), 15, 15, 0, 0),
            Entry::from_path(Path::new(r"C:\Users\other\c.bin"), 15, 15, 0, 0),
        ];
        let b = crate::catalog::Catalog::new("USB", 7, 2, b_entries).encode().unwrap();

        let pa = dir.join("a.wzsnap");
        let pb = dir.join("b.ecat");
        std::fs::write(&pa, crate::snapshot::encode(&a)).unwrap();
        std::fs::write(&pb, b).unwrap();

        let v = compare_stripped(
            pa.to_str().unwrap(),
            pb.to_str().unwrap(),
            10,
            Some(r"users\me"),
            Some(r"users\other"),
        )
        .unwrap();

        assert_eq!(v["a"]["label"], "a");
        assert_eq!(v["b"]["label"], "USB");
        assert_eq!(v["only_a"]["files"], 1);
        assert_eq!(v["only_a"]["bytes"], 20);
        assert_eq!(v["only_b"]["files"], 1);
        assert_eq!(v["only_b"]["bytes"], 15);
        assert_eq!(v["grown"]["files"], 1);
        assert_eq!(v["grown"]["bytes"], 5);
        assert_eq!(v["grown"]["top"][0]["path"], r"Users\me\a.bin");
        // Totales iguales: 10 + 20 = 15 + 15.
        assert_eq!(v["net_bytes"], 0);

        let _ = std::fs::remove_dir_all(&dir);
    }
}
