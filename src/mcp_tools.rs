//! MCP tool implementations backed by the storage-analysis engines.
//!
//! Read-only tools: `cloud_audit` (M2), `hardlink_plan` (M1), `cold_files` (M5),
//! `compression_candidates` (M5), `snapshot_diff` (M4), `catalog_check` (M7).
//! Writing tools (only into the app's own data dir): `snapshot_save`, `catalog_save`.

use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use crate::everything_adapter;
use crate::tree::{ROOT, Tree};

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// FILETIME (100 ns desde 1601) -> segundos unix.
fn filetime_to_unix(ft: u64) -> Option<u64> {
    const EPOCH_DIFF: u64 = 11_644_473_600;
    if ft == 0 {
        return None;
    }
    (ft / 10_000_000).checked_sub(EPOCH_DIFF)
}

fn app_data_dir(sub: &str) -> Result<PathBuf, String> {
    let base = std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("APPDATA").map(PathBuf::from))
        .ok_or("LOCALAPPDATA is not set")?;
    let dir = base.join("WizEverything").join(sub);
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    Ok(dir)
}

fn need_path(root: u32, a: &Value) -> Result<(), String> {
    if root == ROOT && !a.get("all_drives").and_then(Value::as_bool).unwrap_or(false) {
        return Err("Give a 'path' (or all_drives=true).".into());
    }
    Ok(())
}

/// M2 — sparse / offline / reparse-point audit with logical vs physical size.
pub fn cloud_audit(t: &Tree, root: u32, a: &Value, limit: usize) -> Result<Value, String> {
    need_path(root, a)?;
    let max_scan = a.get("max_scan").and_then(Value::as_u64).unwrap_or(2000).clamp(1, 100_000) as usize;
    let mut scanned = 0usize;
    let mut cloud = 0usize;
    let mut logical = 0u64;
    let mut physical = 0u64;
    let mut items = Vec::new();
    for id in t.files_under(root) {
        if scanned >= max_scan {
            break;
        }
        scanned += 1;
        let path = t.full_path(id);
        if let Ok(m) = crate::winphys::probe(Path::new(&path)) {
            if m.is_sparse() || m.is_offline() || m.is_reparse_point() {
                cloud += 1;
                let phys = m.physical_size.unwrap_or(0);
                logical += m.logical_size;
                physical += phys;
                if items.len() < limit {
                    items.push(json!({
                        "path": path,
                        "logical": m.logical_size,
                        "physical": phys,
                        "sparse": m.is_sparse(),
                        "offline": m.is_offline(),
                        "reparse": m.is_reparse_point(),
                    }));
                }
            }
        }
    }
    Ok(json!({
        "scanned": scanned,
        "cloud_files": cloud,
        "logical_bytes": logical,
        "physical_bytes": physical,
        "saved_bytes": logical.saturating_sub(physical),
        "top": items,
    }))
}

/// M1 — dry-run plan: which exact duplicates could be merged to hardlinks.
pub fn hardlink_plan(t: &Tree, root: u32, a: &Value, limit: usize) -> Result<Value, String> {
    need_path(root, a)?;
    let min = (a.get("min_size_mb").and_then(Value::as_f64).unwrap_or(1.0) * 1024.0 * 1024.0) as u64;
    let max_files = a.get("max_files").and_then(Value::as_u64).unwrap_or(200_000).clamp(1, 2_000_000) as usize;
    let mut items = Vec::new();
    for id in t.files_under(root) {
        if items.len() >= max_files {
            break;
        }
        let n = &t.nodes[id as usize];
        if n.size < min {
            continue;
        }
        items.push(crate::dedup::Item {
            path: PathBuf::from(t.full_path(id)),
            size: n.size,
            file_id: None,
        });
    }
    let groups = crate::dedup::classify(&items);
    let mut plans = Vec::new();
    let mut recoverable = 0u64;
    let mut mergeable_groups = 0usize;
    for g in &groups {
        if g.paths.len() < 2 {
            continue;
        }
        let master = crate::winphys::file_id(&g.paths[0]).ok();
        let mut mergeable = 0u64;
        if let Some(mid) = master {
            for d in &g.paths[1..] {
                if let Ok(did) = crate::winphys::file_id(d) {
                    if did.volume_serial == mid.volume_serial {
                        mergeable += 1;
                    }
                }
            }
        }
        let rec = g.size.saturating_mul(mergeable);
        if rec > 0 {
            recoverable += rec;
            mergeable_groups += 1;
            if plans.len() < limit {
                plans.push(json!({
                    "size": g.size,
                    "copies": g.paths.len(),
                    "mergeable": mergeable,
                    "reclaimable": rec,
                    "paths": g.paths.iter().map(|p| p.to_string_lossy()).collect::<Vec<_>>(),
                }));
            }
        }
    }
    Ok(json!({
        "groups": groups.len(),
        "mergeable_groups": mergeable_groups,
        "reclaimable_estimate": recoverable,
        "dry_run": true,
        "note": "Read-only plan. Merging is only done from the app.",
        "top": plans,
    }))
}

/// Escanea vía Everything (metadatos completos) y calcula el cold score.
fn cold_scan(t: &Tree, root: u32, _a: &Value) -> Result<Vec<(everything_adapter::Candidate, u8)>, String> {
    let query = if root == ROOT {
        String::new()
    } else {
        format!("path:\"{}\"", t.full_path(root))
    };
    let files = everything_adapter::query_files(&query)?;
    let now = now_unix();
    let mut out = Vec::with_capacity(files.len());
    for c in files {
        let score = crate::cold::cold_score(
            now,
            crate::cold::ColdInput {
                modified_unix: filetime_to_unix(c.modified).unwrap_or(0),
                accessed_unix: filetime_to_unix(c.accessed),
                run_count: c.run_count,
            },
        );
        out.push((c, score));
    }
    Ok(out)
}

/// M5 — coldest files (candidates to archive).
pub fn cold_files(t: &Tree, root: u32, a: &Value, limit: usize) -> Result<Value, String> {
    need_path(root, a)?;
    let min_score = a.get("min_score").and_then(Value::as_u64).unwrap_or(60).clamp(0, 100) as u8;
    let mut items = cold_scan(t, root, a)?;
    items.retain(|(_, s)| *s >= min_score);
    items.sort_by(|x, y| y.1.cmp(&x.1).then(y.0.size.cmp(&x.0.size)));
    let total: u64 = items.iter().map(|(c, _)| c.size).sum();
    let top: Vec<Value> = items
        .iter()
        .take(limit)
        .map(|(c, s)| {
            json!({
                "path": c.path.to_string_lossy(),
                "size": c.size,
                "cold_score": s,
                "modified_unix": filetime_to_unix(c.modified),
                "run_count": c.run_count,
            })
        })
        .collect();
    Ok(json!({ "cold_files": items.len(), "min_score": min_score, "total_bytes": total, "top": top }))
}

/// M5 — files worth compressing with WOF LZX (cold + not already compressed).
pub fn compression_candidates(t: &Tree, root: u32, a: &Value, limit: usize) -> Result<Value, String> {
    need_path(root, a)?;
    let min_score = a.get("min_score").and_then(Value::as_u64).unwrap_or(70).clamp(0, 100) as u8;
    let mut items = cold_scan(t, root, a)?;
    items.retain(|(c, s)| {
        let ext = c.path.extension().and_then(|e| e.to_str()).unwrap_or("");
        crate::cold::should_compress(ext, *s, min_score)
    });
    items.sort_by(|x, y| y.1.cmp(&x.1).then(y.0.size.cmp(&x.0.size)));
    let total: u64 = items.iter().map(|(c, _)| c.size).sum();
    let top: Vec<Value> = items
        .iter()
        .take(limit)
        .map(|(c, s)| {
            json!({
                "path": c.path.to_string_lossy(),
                "size": c.size,
                "cold_score": s,
                "already_compressed": false,
            })
        })
        .collect();
    Ok(json!({ "candidates": items.len(), "min_score": min_score, "total_bytes": total, "top": top }))
}

/// M4 — guarda un snapshot del índice actual en el directorio de datos de la app.
pub fn snapshot_save(t: &Tree, root: u32, a: &Value) -> Result<Value, String> {
    need_path(root, a)?;
    let max_entries = a.get("max_entries").and_then(Value::as_u64).unwrap_or(500_000).clamp(1, 5_000_000) as usize;
    let mut entries = Vec::new();
    for id in t.files_under(root) {
        if entries.len() >= max_entries {
            break;
        }
        let n = &t.nodes[id as usize];
        let path = PathBuf::from(t.full_path(id));
        entries.push(crate::snapshot::Entry::from_path(&path, n.size, n.alloc, n.modified as i64, 0));
    }
    let snap = crate::snapshot::Snapshot::new(now_unix() as i64, entries);
    let bytes = crate::snapshot::encode(&snap);
    let dir = app_data_dir("snapshots")?;
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let file = dir.join(format!("snap_{stamp}.wzsnap"));
    std::fs::write(&file, &bytes).map_err(|e| e.to_string())?;
    Ok(json!({
        "path": file.to_string_lossy(),
        "entries": snap.entries.len(),
        "bytes": bytes.len(),
    }))
}

/// M4 — compara dos snapshots `.wzsnap`.
pub fn snapshot_diff(a: &Value, limit: usize) -> Result<Value, String> {
    let pa = a.get("a").and_then(Value::as_str).ok_or("missing 'a' (first snapshot path)")?;
    let pb = a.get("b").and_then(Value::as_str).ok_or("missing 'b' (second snapshot path)")?;
    let sa = crate::snapshot::decode(&std::fs::read(pa).map_err(|e| e.to_string())?).map_err(|e| e.to_string())?;
    let sb = crate::snapshot::decode(&std::fs::read(pb).map_err(|e| e.to_string())?).map_err(|e| e.to_string())?;
    let d = crate::snapshot::diff(&sa, &sb);
    Ok(json!({
        "added": d.added.len(),
        "removed": d.removed.len(),
        "grown": d.grown.len(),
        "shrunk": d.shrunk.len(),
        "net_bytes": d.net_bytes,
        "top_added": d.added.iter().take(limit).map(|e| json!({ "name": e.name, "size": e.logical_size })).collect::<Vec<_>>(),
        "top_removed": d.removed.iter().take(limit).map(|e| json!({ "name": e.name, "size": e.logical_size })).collect::<Vec<_>>(),
        "top_grown": d.grown.iter().take(limit).map(|c| json!({ "name": c.name, "delta": c.delta() })).collect::<Vec<_>>(),
    }))
}

/// M7 — guarda un catálogo `.ecat` (Zstd) de un volumen/ruta en el directorio de la app.
pub fn catalog_save(t: &Tree, root: u32, a: &Value) -> Result<Value, String> {
    need_path(root, a)?;
    let name = a.get("volume_name").and_then(Value::as_str).unwrap_or("catalog");
    let max_entries = a.get("max_entries").and_then(Value::as_u64).unwrap_or(500_000).clamp(1, 5_000_000) as usize;
    let mut entries = Vec::new();
    for id in t.files_under(root) {
        if entries.len() >= max_entries {
            break;
        }
        let n = &t.nodes[id as usize];
        let path = PathBuf::from(t.full_path(id));
        entries.push(crate::snapshot::Entry::from_path(&path, n.size, n.alloc, n.modified as i64, 0));
    }
    let cat = crate::catalog::Catalog::new(name, 0, now_unix() as i64, entries);
    let bytes = cat.encode().map_err(|e| e.to_string())?;
    let dir = app_data_dir("catalogs")?;
    let safe: String = name
        .chars()
        .map(|c| if c.is_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
        .collect();
    let file = dir.join(format!("{safe}.ecat"));
    std::fs::write(&file, &bytes).map_err(|e| e.to_string())?;
    Ok(json!({
        "path": file.to_string_lossy(),
        "volume_name": name,
        "entries": cat.entries.len(),
        "bytes": bytes.len(),
    }))
}

/// M7 — cruza archivos locales contra un catálogo de un disco desconectado.
pub fn catalog_check(t: &Tree, root: u32, a: &Value, limit: usize) -> Result<Value, String> {
    let cat_path = a.get("catalog").and_then(Value::as_str).ok_or("missing 'catalog' (.ecat path)")?;
    let cat = crate::catalog::Catalog::decode(&std::fs::read(cat_path).map_err(|e| e.to_string())?)
        .map_err(|e| e.to_string())?;
    need_path(root, a)?;
    let max = a.get("max_entries").and_then(Value::as_u64).unwrap_or(500_000).clamp(1, 5_000_000) as usize;
    let mut checked = 0usize;
    let mut backed = 0usize;
    let mut missing_bytes = 0u64;
    let mut missing = Vec::new();
    for id in t.files_under(root) {
        if checked >= max {
            break;
        }
        let n = &t.nodes[id as usize];
        let path = PathBuf::from(t.full_path(id));
        checked += 1;
        let e = crate::snapshot::Entry::from_path(&path, n.size, n.alloc, 0, 0);
        if cat.lookup_hash(e.path_hash).is_some() {
            backed += 1;
        } else {
            missing_bytes += n.size;
            if missing.len() < limit {
                missing.push(json!({ "path": path.to_string_lossy(), "size": n.size }));
            }
        }
    }
    Ok(json!({
        "catalog_volume": cat.volume_name,
        "checked": checked,
        "backed_up": backed,
        "missing": checked - backed,
        "missing_bytes": missing_bytes,
        "top_missing": missing,
    }))
}
