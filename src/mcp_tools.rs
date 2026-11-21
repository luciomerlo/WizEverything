//! MCP tool implementations backed by the storage-analysis engines.
//!
//! Read-only tools: `cloud_audit` (M2), `hardlink_plan` (M1), `cold_files` (M5),
//! `compression_candidates` (M5), `snapshot_diff` (M4), `catalog_check` (M7).
//! Writing tools (only into the app's own data dir): `snapshot_save`, `catalog_save`.

use std::collections::HashMap;
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

/// Fast by default; `mode: "deep"` (or `deep: true`) asks for the thorough path.
pub fn is_deep(a: &Value) -> bool {
    a.get("deep").and_then(Value::as_bool).unwrap_or(false)
        || a.get("mode").and_then(Value::as_str) == Some("deep")
}

pub fn mode_name(deep: bool) -> &'static str {
    if deep { "deep" } else { "fast" }
}

fn need_path(root: u32, a: &Value) -> Result<(), String> {
    if root == ROOT && !a.get("all_drives").and_then(Value::as_bool).unwrap_or(false) {
        return Err("Give a 'path' (or all_drives=true).".into());
    }
    Ok(())
}


/// Planificador: qué archivos elegir para liberar `target_gb` (dry-run).
pub fn plan_100gb(t: &Tree, root: u32, a: &Value, limit: usize) -> Result<Value, String> {
    need_path(root, a)?;
    let gb = a.get("target_gb").and_then(Value::as_u64).unwrap_or(100).clamp(1, 100_000);
    let deep = is_deep(a);
    let mut v = crate::planner::plan_reclaim(t, root, gb.saturating_mul(1_000_000_000), limit, deep);
    v["mode"] = json!(mode_name(deep));
    v["target_gb"] = json!(gb);
    v["root"] = json!(t.full_path(root));
    Ok(v)
}

/// Mapa de calor por antigüedad (bytes y archivos por rango de fecha).
pub fn age_heatmap(t: &Tree, root: u32, a: &Value, limit: usize) -> Result<Value, String> {
    need_path(root, a)?;
    let mut v = crate::planner::age_heatmap(t, root, limit);
    v["root"] = json!(t.full_path(root));
    Ok(v)
}

/// VHDX de WSL/Docker: espacio recuperable por compactación, no borrando.
pub fn wsl_docker(t: &Tree, root: u32, a: &Value, limit: usize) -> Result<Value, String> {
    need_path(root, a)?;
    let mut v = crate::planner::wsl_docker(t, root, limit);
    v["root"] = json!(t.full_path(root));
    Ok(v)
}

/// Pronóstico de disco lleno a partir de los snapshots guardados.
pub fn forecast(_t: &Tree, _root: u32, _a: &Value) -> Result<Value, String> {
    crate::planner::forecast()
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

/// M5 — cold data aggregated by folder (which folders hold the most unused bytes).
pub fn cold_folders(t: &Tree, root: u32, a: &Value, limit: usize) -> Result<Value, String> {
    need_path(root, a)?;
    let min_score = a.get("min_score").and_then(Value::as_u64).unwrap_or(60).clamp(0, 100) as u8;
    let min_bytes = (a.get("min_folder_mb").and_then(Value::as_f64).unwrap_or(0.0) * 1024.0 * 1024.0) as u64;
    let items = cold_scan(t, root, a)?;
    // folder -> (bytes, files, max_score, oldest_unix)
    let mut agg: HashMap<String, (u64, u64, u8, u64)> = HashMap::new();
    for (c, s) in items {
        if s < min_score {
            continue;
        }
        let Some(parent) = c.path.parent() else { continue };
        let folder = parent.to_string_lossy().to_string();
        if folder.is_empty() {
            continue;
        }
        let e = agg.entry(folder).or_insert((0, 0, 0, u64::MAX));
        e.0 += c.size;
        e.1 += 1;
        e.2 = e.2.max(s);
        if let Some(m) = filetime_to_unix(c.modified) {
            e.3 = e.3.min(m);
        }
    }
    let mut rows: Vec<(String, (u64, u64, u8, u64))> =
        agg.into_iter().filter(|(_, x)| x.0 >= min_bytes && x.0 > 0).collect();
    rows.sort_by(|x, y| y.1.0.cmp(&x.1.0));
    let total: u64 = rows.iter().map(|(_, x)| x.0).sum();
    let top: Vec<Value> = rows
        .iter()
        .take(limit)
        .map(|(folder, x)| {
            json!({
                "folder": folder,
                "cold_bytes": x.0,
                "cold_files": x.1,
                "max_cold_score": x.2,
                "oldest_unix": if x.3 == u64::MAX { Value::Null } else { json!(x.3) },
            })
        })
        .collect();
    Ok(json!({
        "folders": rows.len(),
        "cold_bytes": total,
        "min_score": min_score,
        "top": top,
    }))
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

/// "Why is it full?" — top folders/files and the biggest path chain.
pub fn why_full(t: &Tree, root: u32, a: &Value, limit: usize) -> Result<Value, String> {
    need_path(root, a)?;
    Ok(crate::why::why_full(t, root, limit))
}

/// "What grew?" — diff of the two newest snapshots, attributed per folder.
pub fn growth_report(t: &Tree, root: u32, a: &Value, limit: usize) -> Result<Value, String> {
    let (oldp, newp) = match (a.get("old").and_then(Value::as_str), a.get("new").and_then(Value::as_str)) {
        (Some(o), Some(n)) => (PathBuf::from(o), PathBuf::from(n)),
        _ => crate::why::latest_two()?,
    };
    let old = crate::why::load(&oldp)?;
    let new = crate::why::load(&newp)?;
    let deep = is_deep(a);
    let mut v = crate::why::growth(t, &old, &new, limit, deep);
    v["old"]["path"] = json!(oldp.to_string_lossy());
    v["new"]["path"] = json!(newp.to_string_lossy());
    v["root"] = json!(t.full_path(root));
    v["mode"] = json!(mode_name(deep));
    Ok(v)
}

/// "Can I delete this?" — verdict with reasons, no mutation.
pub fn advice(t: &Tree, root: u32, a: &Value) -> Result<Value, String> {
    let id = match a.get("path").and_then(Value::as_str) {
        Some(p) => t.find_path(p).ok_or_else(|| format!("path not in the index: {p}"))?,
        None => {
            need_path(root, a)?;
            root
        }
    };
    let deep = is_deep(a);
    let mut v = crate::advice::explain(t, id, deep);
    v["mode"] = json!(mode_name(deep));
    Ok(v)
}

/// "Is it backed up?" — compare a folder against another copy by relative path.
pub fn backup_check(t: &Tree, root: u32, a: &Value, limit: usize) -> Result<Value, String> {
    let source = match a.get("source").and_then(Value::as_str) {
        Some(p) => t.find_path(p).ok_or_else(|| format!("source not in the index: {p}"))?,
        None => {
            need_path(root, a)?;
            root
        }
    };
    let target = a
        .get("target")
        .and_then(Value::as_str)
        .ok_or("missing 'target' (folder with the backup copy)")?;
    let target_id = t
        .find_path(target)
        .ok_or_else(|| format!("target not in the index (is the drive connected?): {target}"))?;
    let deep = is_deep(a);
    let max = if deep { 250_000 } else { 50_000 };
    let mut v = crate::backup::compare_trees(t, source, target_id, max)?.to_json(limit);
    v["source"] = json!(t.full_path(source));
    v["target"] = json!(t.full_path(target_id));
    v["mode"] = json!(mode_name(deep));
    Ok(v)
}

const SIM_IMG_EXT: &str = "jpg;jpeg;png;webp;gif;bmp;tif;tiff";
const SIM_AUD_EXT: &str = "mp3;flac;m4a;aac;ogg;opus;wav;aiff;aif;wma;ape;wv;mka";

/// Archivos del índice bajo `root`, restringidos por extensión (consulta a Everything).
fn sim_files(t: &Tree, root: u32, exts: &str) -> Result<Vec<crate::similar::FileInfo>, String> {
    let query = if root == ROOT {
        format!("ext:{exts}")
    } else {
        format!("path:\"{}\" ext:{exts}", t.full_path(root))
    };
    let files = everything_adapter::query_files(&query)?;
    Ok(files
        .into_iter()
        .filter(|c| !c.is_dir && c.size > 0)
        .map(|c| crate::similar::FileInfo { path: c.path, bytes: c.size })
        .collect())
}

/// Imágenes parecidas (misma foto reescalada/recomprimida) con "conservar la mejor".
pub fn similar_images(t: &Tree, root: u32, a: &Value, limit: usize) -> Result<Value, String> {
    need_path(root, a)?;
    let files = sim_files(t, root, SIM_IMG_EXT)?;
    let max_files = a.get("max_files").and_then(Value::as_u64).unwrap_or(4000).clamp(50, 50_000) as usize;
    let threshold = a.get("threshold").and_then(Value::as_u64).unwrap_or(10).clamp(2, 20) as u32;
    let deep = is_deep(a);
    let budget_ms = a
        .get("budget_ms")
        .and_then(Value::as_u64)
        .unwrap_or(if deep { 120_000 } else { 5_000 })
        .clamp(200, 120_000);
    let mut v = crate::similar::scan_images(files, max_files, threshold, budget_ms).to_json(limit);
    v["mode"] = json!(mode_name(deep));
    Ok(v)
}

/// Audio parecido (mismo tema en FLAC/MP3/…), separando versiones por duración.
pub fn similar_audio(t: &Tree, root: u32, a: &Value, limit: usize) -> Result<Value, String> {
    need_path(root, a)?;
    let files = sim_files(t, root, SIM_AUD_EXT)?;
    let max_files = a.get("max_files").and_then(Value::as_u64).unwrap_or(20_000).clamp(50, 200_000) as usize;
    let deep = is_deep(a);
    let mut v = crate::similar::scan_audio(files, max_files, deep).to_json(limit);
    v["mode"] = json!(mode_name(deep));
    Ok(v)
}

/// "GitHub mode" for any chosen folder: local vs web state, commits, files and size.
pub fn github_mode(t: &Tree, root: u32, a: &Value, limit: usize) -> Result<Value, String> {
    need_path(root, a)?;
    let path = t.full_path(root);
    let start = Path::new(&path);
    let repo = crate::github_mode::repo_root(start);
    let sizes = repo.as_ref().and_then(|r| {
        let p = r.to_string_lossy();
        let node = t.find_path(p.as_ref())?;
        let n = &t.nodes[node as usize];
        Some(crate::github_mode::TreeSizes {
            logical: n.size,
            alloc: n.alloc,
            children: t
                .children(node)
                .iter()
                .filter(|&&c| t.nodes[c as usize].is_dir())
                .map(|&c| (t.name(c).to_string(), t.nodes[c as usize].size, t.nodes[c as usize].alloc))
                .collect(),
        })
    });
    let deep = is_deep(a);
    let fetch = a.get("fetch").and_then(Value::as_bool).unwrap_or(deep);
    let mut v = crate::github_mode::inspect(
        start,
        sizes.as_ref(),
        &crate::github_mode::Opts { fetch, limit },
    );
    v["mode"] = json!(mode_name(deep));
    Ok(v)
}

/// Discos guardados: marca, modelo, serie, fecha de actualización y cobertura.
pub fn volumes(_t: &Tree, _root: u32, a: &Value, _limit: usize) -> Result<Value, String> {
    let store = crate::volumes::Store::default();
    let only = a.get("id").and_then(Value::as_str);
    let list: Vec<Value> = store
        .scan()
        .into_iter()
        .filter(|r| only.is_none_or(|id| r.id == id))
        .map(|r| r.to_json(false))
        .collect();
    let total: u64 = store.scan().iter().map(|r| r.total_logical).sum();
    Ok(json!({
        "kind": "volumes",
        "count": list.len(),
        "total_logical": total,
        "storage_dir": store.dir().to_string_lossy(),
        "retention_modes": [
            { "id": "full", "label": "Completa", "description": "Todas las rutas + tamaños + fechas + huella; navegable y comparable." },
            { "id": "digest", "label": "Resumen", "description": "Totales, carpetas principales, tipos y archivos más grandes; no navegable." }
        ],
        "volumes": list,
    }))
}

/// Duplicados contra discos guardados (también entre discos guardados).
pub fn volume_duplicates(t: &Tree, root: u32, a: &Value, limit: usize) -> Result<Value, String> {
    let store = crate::volumes::Store::default();
    let only = a.get("volume").and_then(Value::as_str);
    let metas = store.scan();
    let records: Vec<crate::volumes::VolumeRecord> = metas
        .iter()
        .filter(|r| only.is_none_or(|id| r.id == id))
        .filter(|r| r.retention == crate::volumes::Retention::Full)
        .filter_map(|r| store.load(&r.id).ok())
        .collect();
    if records.is_empty() {
        return Ok(json!({
            "kind": "volume_duplicates",
            "mode": mode_name(is_deep(a)),
            "volumes": 0,
            "local_matches": 0,
            "saved_pairs": 0,
            "matches": [],
            "saved": [],
            "notes": ["No hay discos guardados con retención Completa. Conectá el disco y guardá una copia desde Opciones."],
        }));
    }
    let deep = is_deep(a);
    let opts = crate::volumes::DupOpts {
        deep,
        budget_ms: if deep { 60_000 } else { 3_000 },
        min_bytes: if deep { 1 } else { 64 * 1024 },
        max_files: if deep { 200_000 } else { 5_000 },
    };
    let mut v = crate::volumes::duplicates(Some(t), root, &records, &opts);
    v["mode"] = json!(mode_name(deep));
    if let Some(m) = v.get_mut("matches").and_then(|x| x.as_array_mut()) {
        m.truncate(limit);
    }
    if let Some(s) = v.get_mut("saved").and_then(|x| x.as_array_mut()) {
        s.truncate(limit);
    }
    Ok(v)
}

/// ¿Este archivo está en un disco guardado? Ruta, tamaño y huella.
pub fn volume_lookup(t: &Tree, _root: u32, a: &Value) -> Result<Value, String> {
    let path = a.get("path").and_then(Value::as_str).filter(|p| !p.is_empty()).ok_or("Give a 'path' to look up.")?;
    let path = path.replace('/', "\\");
    let _ = t;
    let store = crate::volumes::Store::default();
    let records: Vec<crate::volumes::VolumeRecord> = store
        .scan()
        .into_iter()
        .filter(|r| r.retention == crate::volumes::Retention::Full)
        .filter_map(|r| store.load(&r.id).ok())
        .collect();
    let mut v = crate::volumes::lookup(&records, Path::new(&path), is_deep(a));
    v["volumes_checked"] = json!(records.len());
    Ok(v)
}

/// Procedencia: zona de Internet, aplicación instalada, Steam y veces abierto.
pub fn provenance(t: &Tree, root: u32, a: &Value) -> Result<Value, String> {
    let id = match a.get("path").and_then(Value::as_str) {
        Some(p) => t.find_path(&p.replace('/', "\\")).ok_or_else(|| format!("path not in the index: {p}"))?,
        None => {
            need_path(root, a)?;
            root
        }
    };
    let deep = is_deep(a);
    let mut v = crate::provenance::explain(t, id, deep);
    v["mode"] = json!(mode_name(deep));
    Ok(v)
}

/// Línea de tiempo: snapshots guardados (fechas y cantidad de entradas).
pub fn timeline(_t: &Tree, _root: u32, _a: &Value, limit: usize) -> Result<Value, String> {
    Ok(crate::timeline::to_json(limit))
}

/// Cuarentena: archivos movidos por la app, con opción de restaurar desde la GUI.
pub fn quarantine_list(_t: &Tree, _root: u32, _a: &Value, limit: usize) -> Result<Value, String> {
    let mut entries = crate::quarantine::scan();
    let total: u64 = entries.iter().map(|e| e.bytes).sum();
    entries.truncate(limit);
    Ok(json!({
        "kind": "quarantine",
        "count": entries.len(),
        "total_bytes": total,
        "root": crate::quarantine::root().to_string_lossy(),
        "entries": entries.iter().map(|e| e.to_json()).collect::<Vec<_>>(),
        "notes": ["Restaurar y borrar se hacen desde la app (Opciones → Abrir cuarentena). El MCP es sólo lectura."],
    }))
}

/// Velocidad real del disco y/o de Google Drive, con ETA para una tarea.
pub fn speed_test(t: &Tree, root: u32, a: &Value) -> Result<Value, String> {
    use crate::throughput::{self, DiskClass, GoogleMode};
    let path = t.full_path(root);
    let p = Path::new(&path);
    let deep = is_deep(a);
    let task_bytes = a.get("bytes").and_then(Value::as_u64).unwrap_or(1_000_000_000);
    let files = a.get("files").and_then(Value::as_u64).unwrap_or(1);
    let want_write = deep && a.get("write").and_then(Value::as_bool).unwrap_or(false);
    let google = throughput::google_mode(p);
    let class = throughput::class_of(p);
    let (read_fallback, write_fallback) = class.fallback_mbps();

    let mut notes: Vec<String> = Vec::new();
    let mut read = None;
    let mut download = None;
    if google == Some(GoogleMode::Stream) {
        if deep {
            if let Some(id) = throughput::first_cloud_file(t, root, 8 * 1024 * 1024) {
                let f = t.full_path(id);
                match throughput::bench_read(Path::new(&f), 32 * 1024 * 1024) {
                    Ok(b) => {
                        notes.push(format!("La descarga real se midió leyendo {f}; ese archivo quedó hidratado (local)."));
                        download = Some(b);
                    }
                    Err(e) => notes.push(format!("No se pudo medir la descarga: {e}")),
                }
            } else {
                notes.push("No se encontró un placeholder de nube ≥ 8 MB para medir la descarga.".into());
            }
        } else {
            notes.push("Modo rápido: no descarga nada; usa la velocidad típica de red. Pedí profundo para medir la descarga real.".into());
        }
    } else if deep {
        if let Some(id) = throughput::sample_file(t, root, 4 * 1024 * 1024) {
            let f = t.full_path(id);
            match throughput::bench_read(Path::new(&f), 16 * 1024 * 1024) {
                Ok(b) => read = Some(b),
                Err(e) => notes.push(format!("No se pudo medir la lectura: {e}")),
            }
        }
    }

    let write = if want_write && google != Some(GoogleMode::Stream) {
        match throughput::bench_write(p, 16 * 1024 * 1024) {
            Ok(b) => Some(b),
            Err(e) => {
                notes.push(format!("No se pudo medir la escritura en {path}: {e}"));
                None
            }
        }
    } else {
        None
    };

    // La clasificación local/nube sólo tiene sentido en una unidad de Google Drive.
    let cloud = if google.is_some() {
        throughput::cloud_breakdown(t, root, if deep { 5_000 } else { 1_000 }, if deep { 20_000 } else { 5_000 })
    } else {
        Default::default()
    };
    let source_mbps = download.as_ref().or(read.as_ref()).map(|b| b.mbps).unwrap_or_else(|| {
        if google == Some(GoogleMode::Stream) { 30.0 } else { read_fallback }
    });
    let target_mbps = write.as_ref().map(|b| b.mbps).unwrap_or(write_fallback);
    let eta = throughput::estimate(task_bytes, files, source_mbps, target_mbps, 2);
    let cloud_fraction = if cloud.bytes > 0 { cloud.cloud_bytes as f64 / cloud.bytes as f64 } else { 0.0 };
    let eta_cloud = throughput::estimate((task_bytes as f64 * cloud_fraction) as u64, files, source_mbps, target_mbps, 2);

    let mut v = json!({
        "kind": "speed",
        "mode": mode_name(deep),
        "path": path,
        "disk_class": class.as_str(),
        "disk_class_label": class.label().0,
        "read_fallback_mbps": (read_fallback * 10.0).round() / 10.0,
        "write_fallback_mbps": (write_fallback * 10.0).round() / 10.0,
        "read": read.as_ref().map(|b| json!({ "mbps": (b.mbps * 10.0).round() / 10.0, "bytes": b.bytes, "ms": b.ms, "file": b.path })),
        "write": write.as_ref().map(|b| json!({ "mbps": (b.mbps * 10.0).round() / 10.0, "bytes": b.bytes, "ms": b.ms })),
        "download": download.as_ref().map(|b| json!({ "mbps": (b.mbps * 10.0).round() / 10.0, "bytes": b.bytes, "ms": b.ms, "file": b.path })),
        "google_drive": google.map(|g| g.as_str()),
        "cloud": if google.is_some() { cloud.to_json() } else { serde_json::Value::Null },
        "eta": eta.to_json(task_bytes, files),
        "eta_cloud_only": if google.is_some() { eta_cloud.to_json((task_bytes as f64 * cloud_fraction) as u64, files) } else { serde_json::Value::Null },
        "notes": notes,
    });
    if let Some(g) = google {
        v["google_note"] = json!(match g {
            GoogleMode::Stream => "Streaming: los archivos viven en la nube hasta que se abren; las carpetas fijadas ya están locales.",
            GoogleMode::Clone => "Clone: los archivos ya están en el disco local; no hay descarga de Google.",
        });
    }
    let _ = DiskClass::Unknown;
    Ok(v)
}

/// Addons externos para Google Photos/OCR (sólo detección e instrucciones).
pub fn addons(_t: &Tree, _root: u32, _a: &Value) -> Result<Value, String> {
    Ok(crate::addons::to_json())
}

/// Buscar texto dentro de archivos (Fast: texto/PDF; Deep: OCR).
pub fn content_search(t: &Tree, root: u32, a: &Value, limit: usize) -> Result<Value, String> {
    let query = a
        .get("query")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|q| !q.is_empty())
        .ok_or("Give a 'query' to search for.")?;
    need_path(root, a)?;
    let deep = is_deep(a);
    let max_files = a
        .get("max_files")
        .and_then(Value::as_u64)
        .unwrap_or(if deep { 50_000 } else { 4_000 })
        .clamp(1, 200_000) as usize;
    let budget = a
        .get("budget_ms")
        .and_then(Value::as_u64)
        .unwrap_or(if deep { 60_000 } else { 2_500 }) as u128;
    let rep = crate::contents::search(t, root, query, deep, max_files, budget, limit);
    let mut v = rep.to_json(query, limit);
    v["mode"] = json!(mode_name(deep));
    v["root"] = json!(t.full_path(root));
    Ok(v)
}

/// Estimación de cuánto tardaría buscar dentro antes de lanzarlo.
pub fn content_estimate(t: &Tree, root: u32, a: &Value) -> Result<Value, String> {
    need_path(root, a)?;
    let mut v = crate::contents::estimate(t, root);
    v["root"] = json!(t.full_path(root));
    Ok(v)
}
