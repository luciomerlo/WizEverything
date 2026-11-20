//! MCP server (Model Context Protocol) over stdio: `wizeverything.exe --mcp`.
//!
//! Lets an AI assistant ask about disk usage, file types and duplicates using
//! the same in-memory index as the app. JSON-RPC 2.0, one message per line.

use std::io::{BufRead, Write};
use std::path::Path;
use std::sync::Arc;

use serde_json::{Value, json};

use crate::app::{CATEGORIES, category_name, category_of, fmt_iso};
use crate::dup::{self, DupOptions, DupProgress};
use crate::tree::{self, Progress, ROOT, Tree};

const PROTOCOL: &str = "2025-06-18";

pub struct Server {
    tree: Option<Arc<Tree>>,
    loader: Box<dyn Fn() -> Result<Arc<Tree>, String> + Send>,
}

impl Server {
    pub fn new(loader: Box<dyn Fn() -> Result<Arc<Tree>, String> + Send>) -> Self {
        Self { tree: None, loader }
    }

    fn tree(&mut self) -> Result<Arc<Tree>, String> {
        if self.tree.is_none() {
            self.tree = Some((self.loader)()?);
        }
        Ok(self.tree.clone().expect("loaded"))
    }

    /// Handle one JSON-RPC message; `None` for notifications.
    pub fn handle(&mut self, msg: &Value) -> Option<Value> {
        let id = msg.get("id").cloned()?; // notifications have no id
        let method = msg.get("method").and_then(Value::as_str).unwrap_or("");
        let result = match method {
            "initialize" => Ok(json!({
                "protocolVersion": msg.pointer("/params/protocolVersion").and_then(Value::as_str).unwrap_or(PROTOCOL),
                "capabilities": { "tools": {} },
                "serverInfo": { "name": "wizeverything", "version": env!("CARGO_PKG_VERSION") },
                "instructions": "Mostly read-only. Disk usage, file types, duplicates and storage-analysis reports from the Everything index on this Windows PC. Only snapshot_save and catalog_save write, and only into %LOCALAPPDATA%\\WizEverything. It never deletes or changes your files. Sizes are in bytes. Paths look like C:\\Users\\name."
            })),
            "ping" => Ok(json!({})),
            "tools/list" => Ok(json!({ "tools": tools() })),
            "tools/call" => {
                let name = msg.pointer("/params/name").and_then(Value::as_str).unwrap_or("");
                let args = msg.pointer("/params/arguments").cloned().unwrap_or(json!({}));
                Ok(match self.call_tool(name, &args) {
                    Ok(v) => json!({ "content": [{ "type": "text", "text": serde_json::to_string_pretty(&v).unwrap_or_default() }] }),
                    Err(e) => json!({ "content": [{ "type": "text", "text": e }], "isError": true }),
                })
            }
            _ => Err(json!({ "code": -32601, "message": format!("Method not found: {method}") })),
        };
        Some(match result {
            Ok(r) => json!({ "jsonrpc": "2.0", "id": id, "result": r }),
            Err(e) => json!({ "jsonrpc": "2.0", "id": id, "error": e }),
        })
    }

    /// Run one tool (also used by the Groq assistant).
    pub fn call_tool(&mut self, name: &str, a: &Value) -> Result<Value, String> {
        if name == "refresh" {
            self.tree = None;
            let t = self.tree()?;
            return Ok(json!({ "items": t.nodes.len() - 1, "load_ms": t.load_ms }));
        }
        // Estos dos no exigen que la ruta exista en el índice (puede ser de un
        // disco guardado que ya no está conectado).
        if name == "volume_lookup" {
            let t = self.tree()?;
            return crate::mcp_tools::volume_lookup(&t, ROOT, a);
        }
        let t = self.tree()?;
        let root = match a.get("path").and_then(Value::as_str).filter(|p| !p.is_empty()) {
            // Accept C:/Users/... as well as C:\Users\...
            Some(p) => t.find_path(&p.replace('/', "\\")).ok_or_else(|| format!("Path not found in the index: {p}"))?,
            None => ROOT,
        };
        let limit = a.get("limit").and_then(Value::as_u64).unwrap_or(25).clamp(1, 1000) as usize;
        let node = |id: u32| {
            let n = &t.nodes[id as usize];
            let mut v = json!({
                "path": t.full_path(id),
                "size": n.size,
                "allocated": n.alloc,
                "modified": fmt_iso(n.modified),
                "type": if n.is_dir() { "folder" } else { "file" },
            });
            if n.is_dir() {
                v["files"] = json!(n.files);
                v["folders"] = json!(n.folders);
            }
            v
        };
        match name {
            "drives" => Ok(json!(t
                .drives
                .iter()
                .map(|&d| {
                    let mut v = node(d);
                    v["label"] = json!(t.drive_label(d));
                    v["excluded_from_all"] = json!(t.is_excluded(d));
                    if let Some((total, free)) = tree::disk_space(t.name(d)) {
                        v["disk_total"] = json!(total);
                        v["disk_free"] = json!(free.min(total));
                    }
                    v
                })
                .collect::<Vec<_>>())),
            "folder_sizes" => {
                let total = t.nodes[root as usize].size.max(1) as f64;
                let children: Vec<Value> = t
                    .children(root)
                    .iter()
                    .take(limit)
                    .map(|&c| {
                        let mut v = node(c);
                        v["percent"] = json!((t.nodes[c as usize].size as f64 / total * 1000.0).round() / 10.0);
                        v
                    })
                    .collect();
                Ok(json!({ "folder": node(root), "largest_children": children }))
            }
            "largest_files" => {
                let filter = a.get("name_contains").and_then(Value::as_str).unwrap_or("");
                Ok(json!(t.largest_files(root, filter, limit).into_iter().map(node).collect::<Vec<_>>()))
            }
            "file_types" => {
                let stats = t.ext_stats(root);
                let mut cats: Vec<(usize, u64, u64, Vec<(String, u64, u64)>)> =
                    (0..CATEGORIES.len()).map(|c| (c, 0, 0, Vec::new())).collect();
                for (e, size, count) in stats {
                    let name = t.exts[e as usize].name.clone();
                    let c = category_of(&name);
                    cats[c].1 += size;
                    cats[c].2 += count;
                    cats[c].3.push((if name.is_empty() { "(none)".into() } else { name }, size, count));
                }
                cats.retain(|c| c.2 > 0);
                cats.sort_by(|a, b| b.1.cmp(&a.1));
                Ok(json!(cats
                    .into_iter()
                    .map(|(c, size, count, mut exts)| {
                        exts.sort_by(|a, b| b.1.cmp(&a.1));
                        exts.truncate(limit);
                        json!({
                            "category": category_name(c),
                            "size": size,
                            "files": count,
                            "top_extensions": exts.into_iter().map(|(e, s, n)| json!({ "ext": e, "size": s, "files": n })).collect::<Vec<_>>(),
                        })
                    })
                    .collect::<Vec<_>>()))
            }
            "find_duplicates" => {
                if root == ROOT && !a.get("all_drives").and_then(Value::as_bool).unwrap_or(false) {
                    return Err("Give a 'path' to search (or all_drives=true; that can read a lot of data).".into());
                }
                let deep = crate::mcp_tools::is_deep(&a);
                let opt = DupOptions {
                    root,
                    min_size: (a.get("min_size_mb").and_then(Value::as_f64).unwrap_or(1.0) * 1024.0 * 1024.0) as u64,
                    same_name: a.get("same_name").and_then(Value::as_bool).unwrap_or(true),
                    verify: a.get("verify").and_then(Value::as_bool).unwrap_or(deep),
                    versions: a.get("versions").and_then(Value::as_bool).unwrap_or(false),
                    // The app's saved exclusions plus any given here.
                    exclude: crate::i18n::Settings::load()
                        .dup_exclude
                        .into_iter()
                        .chain(a.get("exclude").and_then(Value::as_array).into_iter().flatten().filter_map(|v| v.as_str().map(String::from)))
                        .filter_map(|p| t.find_path(&p.replace('/', "\\")))
                        .filter(|&id| id != ROOT)
                        .collect(),
                };
                let prog = Arc::new(DupProgress::default());
                let groups = dup::find(t.clone(), opt, prog.clone());
                let reclaimable: u64 = groups.iter().map(|g| g.wasted()).sum();
                Ok(json!({
                    "mode": if deep { "deep" } else { "fast" },
                    "groups": groups.len(),
                    "bytes_read_from_disk": prog.bytes_read.load(std::sync::atomic::Ordering::Relaxed),
                    "files_from_hash_cache": prog.cache_hits.load(std::sync::atomic::Ordering::Relaxed),
                    "reclaimable": reclaimable,
                    "top": groups.iter().take(limit).map(|g| json!({
                        "kind": if g.folder { "folder" } else { "file" },
                        "verified_by_content": g.verified,
                        "size": g.size,
                        "reclaimable": g.wasted(),
                        "paths": g.files.iter().map(|f| f.path.clone()).collect::<Vec<_>>(),
                    })).collect::<Vec<_>>(),
                }))
            }
            "dev_artifacts" => {
                if root == ROOT && !a.get("all_drives").and_then(Value::as_bool).unwrap_or(false) {
                    return Err("Give a 'path' to scan (or all_drives=true).".into());
                }
                let threshold = a
                    .get("idle_days")
                    .and_then(Value::as_u64)
                    .unwrap_or(crate::sweeper::DEFAULT_IDLE_DAYS);
                let now = now_unix();
                let mut found: Vec<crate::sweeper::SweepCandidate> = Vec::new();
                walk_dirs(&t, root, &mut |id| {
                    if id == root {
                        return;
                    }
                    let node = &t.nodes[id as usize];
                    if let Some(kind) = crate::sweeper::classify_dir(t.name(id)) {
                        // La actividad del proyecto la marca su carpeta padre.
                        let src = filetime_to_unix(t.nodes[node.parent as usize].modified);
                        let bytes = node.alloc.max(node.size);
                        found.push(crate::sweeper::evaluate(
                            Path::new(&t.full_path(id)),
                            kind,
                            now,
                            src,
                            bytes,
                            threshold,
                        ));
                    }
                });
                found.retain(|c| c.safe_to_delete);
                found.sort_by(|x, y| y.bytes.cmp(&x.bytes));
                let reclaimable: u64 = found.iter().map(|c| c.bytes).sum();
                let top: Vec<Value> = found
                    .iter()
                    .take(limit)
                    .map(|c| {
                        json!({
                            "path": c.path.to_string_lossy(),
                            "kind": c.kind.label(),
                            "idle_days": c.idle_days,
                            "bytes": c.bytes,
                        })
                    })
                    .collect();
                Ok(json!({
                    "candidates": found.len(),
                    "reclaimable": reclaimable,
                    "idle_days_threshold": threshold,
                    "top": top,
                }))
            }
            "scan_integrity" => {
                if root == ROOT && !a.get("all_drives").and_then(Value::as_bool).unwrap_or(false) {
                    return Err(
                        "Give a 'path' to scan (or all_drives=true; it reads the first bytes of many files)."
                            .into(),
                    );
                }
                let deep = crate::mcp_tools::is_deep(&a);
                let max_scan = a
                    .get("max_scan")
                    .and_then(Value::as_u64)
                    .unwrap_or(if deep { 50_000 } else { 3_000 })
                    .clamp(1, 50_000) as usize;
                let files = t.files_under(root);
                let mut scanned = 0usize;
                let mut items: Vec<Value> = Vec::new();
                for id in files {
                    if scanned >= max_scan || items.len() >= limit {
                        break;
                    }
                    let n = &t.nodes[id as usize];
                    let ext = t.exts.get(n.ext as usize).map(|e| e.name.as_str()).unwrap_or("");
                    if !crate::magic::is_structured_ext(ext) {
                        continue;
                    }
                    scanned += 1;
                    let path = t.full_path(id);
                    if let Ok(v) = crate::magic::sniff(Path::new(&path)) {
                        if v.is_suspicious() {
                            items.push(json!({
                                "path": path,
                                "ext": ext,
                                "size": n.size,
                                "issue": v.as_str(),
                            }));
                        }
                    }
                }
                Ok(json!({
                    "mode": if deep { "deep" } else { "fast" },
                    "scanned": scanned,
                    "suspicious": items.len(),
                    "scan_cap": max_scan,
                    "items": items,
                }))
            }
            "cloud_audit" => crate::mcp_tools::cloud_audit(&t, root, &a, limit),
            "hardlink_plan" => crate::mcp_tools::hardlink_plan(&t, root, &a, limit),
            "cold_files" => crate::mcp_tools::cold_files(&t, root, &a, limit),
            "cold_folders" => crate::mcp_tools::cold_folders(&t, root, &a, limit),
            "compression_candidates" => crate::mcp_tools::compression_candidates(&t, root, &a, limit),
            "snapshot_save" => crate::mcp_tools::snapshot_save(&t, root, &a),
            "snapshot_diff" => crate::mcp_tools::snapshot_diff(&a, limit),
            "catalog_save" => crate::mcp_tools::catalog_save(&t, root, &a),
            "catalog_check" => crate::mcp_tools::catalog_check(&t, root, &a, limit),
            "similar_images" => crate::mcp_tools::similar_images(&t, root, &a, limit),
            "similar_audio" => crate::mcp_tools::similar_audio(&t, root, &a, limit),
            "github_mode" => crate::mcp_tools::github_mode(&t, root, &a, limit),
            "why_full" => crate::mcp_tools::why_full(&t, root, &a, limit),
            "growth" => crate::mcp_tools::growth_report(&t, root, &a, limit),
            "advice" => crate::mcp_tools::advice(&t, root, &a),
            "backup_check" => crate::mcp_tools::backup_check(&t, root, &a, limit),
            "volumes" => crate::mcp_tools::volumes(&t, root, &a, limit),
            "volume_duplicates" => crate::mcp_tools::volume_duplicates(&t, root, &a, limit),
            "provenance" => crate::mcp_tools::provenance(&t, root, &a),
            "timeline" => crate::mcp_tools::timeline(&t, root, &a, limit),
            "quarantine_list" => crate::mcp_tools::quarantine_list(&t, root, &a, limit),
            "speed_test" => crate::mcp_tools::speed_test(&t, root, &a),
            "addons" => crate::mcp_tools::addons(&t, root, &a),
            "content_search" => crate::mcp_tools::content_search(&t, root, &a, limit),
            "content_estimate" => crate::mcp_tools::content_estimate(&t, root, &a),
            _ => Err(format!("Unknown tool: {name}")),
        }
    }
}

/// Segundos unix actuales.
fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Convierte un FILETIME (100 ns desde 1601) a segundos unix; `None` si es 0.
fn filetime_to_unix(ft: u64) -> Option<u64> {
    const EPOCH_DIFF: u64 = 11_644_473_600; // segundos entre 1601 y 1970
    if ft == 0 {
        return None;
    }
    (ft / 10_000_000).checked_sub(EPOCH_DIFF)
}

/// Recorre todas las carpetas bajo `root` (incluida), sin bajar a archivos.
fn walk_dirs(t: &Tree, root: u32, f: &mut impl FnMut(u32)) {
    f(root);
    for &c in t.children(root) {
        if t.nodes[c as usize].is_dir() {
            walk_dirs(t, c, f);
        }
    }
}

/// Every tool is read-only: nothing here deletes, moves or writes files.
pub fn tools() -> Value {
    let ro = json!({ "readOnlyHint": true, "destructiveHint": false, "openWorldHint": false });
    let wr = json!({ "readOnlyHint": false, "destructiveHint": false, "openWorldHint": false });
    let path = json!({ "type": "string", "description": "Folder or drive, e.g. C:\\ or C:\\Users. Omit for all drives." });
    let limit = json!({ "type": "integer", "description": "Max items to return (default 25)." });
    json!([
        { "name": "drives", "description": "List drives with label, used size, free space and item counts.", "annotations": ro,
          "inputSchema": { "type": "object", "properties": {} } },
        { "name": "folder_sizes", "description": "Size of a folder and its largest children (like WizTree's tree view).", "annotations": ro,
          "inputSchema": { "type": "object", "properties": { "path": path, "limit": limit } } },
        { "name": "largest_files", "description": "Largest files under a folder, optionally filtered by name.", "annotations": ro,
          "inputSchema": { "type": "object", "properties": { "path": path, "limit": limit,
            "name_contains": { "type": "string", "description": "Case-insensitive name filter, e.g. .mp4" } } } },
        { "name": "file_types", "description": "Space by category (Video, Audio, Documents…) and extension under a folder.", "annotations": ro,
          "inputSchema": { "type": "object", "properties": { "path": path, "limit": limit } } },
        { "name": "find_duplicates", "description": "Find duplicate files and folders (verified by content) under a folder. Read-only.", "annotations": ro,
          "inputSchema": { "type": "object", "properties": { "path": path, "limit": limit,
            "min_size_mb": { "type": "number", "description": "Ignore files smaller than this (default 1)." },
            "same_name": { "type": "boolean", "description": "Only compare files with the same name (default true, faster)." }, "mode": { "type": "string", "enum": ["fast", "deep"], "description": "fast (default) uses budgets/caps; deep does the thorough pass." },
            "verify": { "type": "boolean", "description": "Confirm by content hashes (default true). false = name + size only, reads nothing (safe for cloud drives)." },
            "all_drives": { "type": "boolean", "description": "Search every included drive when no path is given." },
            "versions": { "type": "boolean", "description": "Old versions: same name and folder name on any drive, even if size/content differ. Index only." },
            "exclude": { "type": "array", "items": { "type": "string" }, "description": "Extra folders to skip (the app's saved exclusions always apply)." } } } },
        { "name": "dev_artifacts", "description": "Find rebuildable dev artifacts (node_modules, .venv, target, __pycache__, .gradle, .next, .terraform, …) in abandoned projects, with reclaimable size. Index only, reads no files.", "annotations": ro,
          "inputSchema": { "type": "object", "properties": { "path": path, "limit": limit,
            "all_drives": { "type": "boolean", "description": "Scan every included drive when no path is given." },
            "idle_days": { "type": "integer", "description": "Only report projects untouched for at least this many days (default 90)." } } } },
        { "name": "scan_integrity", "description": "Check structured files (pdf, zip, 7z, rar, mp4, mkv, jpg, png, sqlite, exe, dll…) for corruption: zero-filled files, HTML error pages saved as binaries, or signature mismatches. Reads the first 32 bytes of each file. Read-only.", "annotations": ro,
          "inputSchema": { "type": "object", "properties": { "path": path, "limit": limit,
            "all_drives": { "type": "boolean", "description": "Scan every included drive when no path is given." },
            "max_scan": { "type": "integer", "description": "Max files to read (default 3000 fast / 50000 deep, max 50000)." }, "mode": { "type": "string", "enum": ["fast", "deep"], "description": "fast (default) uses budgets/caps; deep does the thorough pass." }, } } },
        { "name": "cloud_audit", "description": "Audit sparse/offline/cloud files and reparse points: shows logical vs real on-disk size and the space that could be recovered. Read-only.", "annotations": ro,
          "inputSchema": { "type": "object", "properties": { "path": path, "limit": limit,
            "all_drives": { "type": "boolean", "description": "Scan every included drive when no path is given." },
            "max_scan": { "type": "integer", "description": "Max files to inspect (default 2000)." } } } },
        { "name": "hardlink_plan", "description": "Dry-run: finds exact duplicates that could be merged into NTFS hardlinks on the same volume, with the space it would reclaim. Does not modify anything. Read-only.", "annotations": ro,
          "inputSchema": { "type": "object", "properties": { "path": path, "limit": limit,
            "all_drives": { "type": "boolean", "description": "Scan every included drive when no path is given." },
            "min_size_mb": { "type": "number", "description": "Ignore files smaller than this (default 1)." },
            "max_files": { "type": "integer", "description": "Safety cap on files considered (default 200000)." } } } },
        { "name": "cold_files", "description": "Coldest files by a score from modified date, last access and run count — candidates to archive. Reads Everything's access/run metadata. Read-only.", "annotations": ro,
          "inputSchema": { "type": "object", "properties": { "path": path, "limit": limit,
            "all_drives": { "type": "boolean", "description": "Scan every included drive when no path is given." },
            "min_score": { "type": "integer", "description": "Minimum cold score 0–100 (default 60)." } } } },
        { "name": "cold_folders", "description": "Cold data grouped by folder: which folders hold the most unused bytes (same cold score as cold_files). Read-only.", "annotations": ro,
          "inputSchema": { "type": "object", "properties": { "path": path, "limit": limit,
            "all_drives": { "type": "boolean", "description": "Scan every included drive when no path is given." },
            "min_score": { "type": "integer", "description": "Minimum cold score 0–100 (default 60)." },
            "min_folder_mb": { "type": "number", "description": "Ignore folders smaller than this (default 0)." } } } },
        { "name": "compression_candidates", "description": "Cold files worth compressing with WOF LZX (excludes formats already compressed). Read-only plan.", "annotations": ro,
          "inputSchema": { "type": "object", "properties": { "path": path, "limit": limit,
            "all_drives": { "type": "boolean", "description": "Scan every included drive when no path is given." },
            "min_score": { "type": "integer", "description": "Minimum cold score (default 70)." } } } },
        { "name": "snapshot_save", "description": "Saves a compact snapshot of the current index for later diffing. Writes only into the app's data folder (%LOCALAPPDATA%\\WizEverything\\snapshots).", "annotations": wr,
          "inputSchema": { "type": "object", "properties": { "path": path,
            "all_drives": { "type": "boolean", "description": "Snapshot every included drive when no path is given." },
            "max_entries": { "type": "integer", "description": "Safety cap (default 500000)." } } } },
        { "name": "snapshot_diff", "description": "Compares two saved snapshots (.wzsnap): added/removed/grown/shrunk files and net bytes. Read-only.", "annotations": ro,
          "inputSchema": { "type": "object", "properties": { "a": { "type": "string", "description": "First .wzsnap path." }, "b": { "type": "string", "description": "Second .wzsnap path." }, "limit": limit }, "required": ["a", "b"] } },
        { "name": "catalog_save", "description": "Saves a portable, Zstd-compressed .ecat catalog of a drive/folder for offline cross-checking. Writes only into the app's data folder.", "annotations": wr,
          "inputSchema": { "type": "object", "properties": { "path": path,
            "all_drives": { "type": "boolean", "description": "Catalog every included drive when no path is given." },
            "volume_name": { "type": "string", "description": "Label for the volume (default 'catalog')." },
            "max_entries": { "type": "integer", "description": "Safety cap (default 500000)." } } } },
        { "name": "catalog_check", "description": "Cross-checks local files against a saved .ecat catalog to show what is already backed up on a disconnected volume. Read-only.", "annotations": ro,
          "inputSchema": { "type": "object", "properties": { "path": path, "limit": limit,
            "all_drives": { "type": "boolean", "description": "Check every included drive when no path is given." },
            "catalog": { "type": "string", "description": "Path to the .ecat file." },
            "max_entries": { "type": "integer", "description": "Safety cap (default 500000)." } }, "required": ["catalog"] } },
        { "name": "similar_images", "description": "Finds near-duplicate images (same photo resized, recompressed or exported twice) using perceptual hashes, and suggests which copy to keep by quality. Read-only. Experimental.", "annotations": ro,
          "inputSchema": { "type": "object", "properties": { "path": path, "limit": limit,
            "all_drives": { "type": "boolean", "description": "Scan every included drive when no path is given." },
            "max_files": { "type": "integer", "description": "Analyse at most this many images, largest first (default 4000)." },
            "threshold": { "type": "integer", "description": "Hamming threshold 2–20 (default 10)." }, "mode": { "type": "string", "enum": ["fast", "deep"], "description": "fast (default) uses budgets/caps; deep does the thorough pass." },
            "budget_ms": { "type": "integer", "description": "Fast-first wall-clock budget (default 5000 ms in fast, 120000 ms in deep; max 120000). When reached, the result is partial and reports how many images were analysed." } } } },
        { "name": "similar_audio", "description": "Groups the same track across formats/bitrates (FLAC vs MP3, etc.) even when tags differ (album vs TV/YouTube rip) using fuzzy title + duration, and suggests which copy to keep by quality. Decodes a bounded 20 s (fast) or 120 s (deep) window per copy under a time budget to compare noise floor, SNR, digital silence, high-frequency content and spectral stability, so denoised copies are matched with the original and rated on how they actually sound (clean vs over-filtered/musical noise). Results include similarity percentages and report elapsed_ms; when the budget is reached the rest is metadata-only and marked as partial. Read-only. Experimental.", "annotations": ro,
          "inputSchema": { "type": "object", "properties": { "path": path, "limit": limit,
            "all_drives": { "type": "boolean", "description": "Scan every included drive when no path is given." },
            "max_files": { "type": "integer", "description": "Analyse at most this many audio files (default 20000)." }, "mode": { "type": "string", "enum": ["fast", "deep"], "description": "fast (default) uses budgets/caps; deep does the thorough pass." }, } } },
        { "name": "github_mode", "description": "GitHub mode for any chosen folder: is the local copy up to date with the web, newer than it, behind or diverged? Then explains the differences by commits, changed files, size and top folders. Read-only; may run git fetch (fetch=false to stay offline). Experimental.", "annotations": ro,
          "inputSchema": { "type": "object", "properties": { "path": path, "limit": limit,
            "fetch": { "type": "boolean", "description": "Query the remote with git fetch (default: deep only)." }, "mode": { "type": "string", "enum": ["fast", "deep"], "description": "fast (default) uses budgets/caps; deep does the thorough pass." } }, "required": ["path"] } },
        { "name": "why_full", "description": "Explains why a folder is full: top folders and files with percentages, plus the chain of biggest children down to the leaf that really holds the space. Read-only.", "annotations": ro,
          "inputSchema": { "type": "object", "properties": { "path": path, "limit": limit,
            "all_drives": { "type": "boolean", "description": "Analyse every included drive when no path is given." } } } },
        { "name": "growth", "description": "What grew since the previous snapshot: compares the two newest .wzsnap files and ranks folders by net change, with new and grown files. Read-only.", "annotations": ro,
          "inputSchema": { "type": "object", "properties": {
            "old": { "type": "string", "description": "Older .wzsnap (default: second newest in the app data folder)." },
            "new": { "type": "string", "description": "Newer .wzsnap (default: newest)." }, "mode": { "type": "string", "enum": ["fast", "deep"], "description": "fast (default) uses budgets/caps; deep does the thorough pass." },
            "limit": limit } } },
        { "name": "advice", "description": "Second opinion before deleting: safe / caution / keep, with reasons (dev artifact, git-tracked, hard links, cloud placeholder, system path, personal data, age) and the real reclaimable bytes. Never deletes. Read-only.", "annotations": ro,
          "inputSchema": { "type": "object", "properties": { "path": path, "mode": { "type": "string", "enum": ["fast", "deep"], "description": "fast (default) uses budgets/caps; deep does the thorough pass." } } } },
        { "name": "backup_check", "description": "Is this folder backed up? Compares source and target by relative path and size: coverage %, missing bytes, files that differ and extras only in the backup. Both folders must be in the index. Read-only.", "annotations": ro,
          "inputSchema": { "type": "object", "properties": {
            "source": { "type": "string", "description": "Folder to verify (default: path/root)." },
            "target": { "type": "string", "description": "Folder that should contain the backup copy." },
            "path": path, "limit": limit, "mode": { "type": "string", "enum": ["fast", "deep"], "description": "fast (default) uses budgets/caps; deep does the thorough pass." } }, "required": ["target"] } },
        { "name": "volumes", "description": "Saved disks: brand, model, device serial, volume serial, bus, last update date, retention mode and how many files have a fast semihash. These are volumes that were connected before and may be offline now. Read-only.", "annotations": ro,
          "inputSchema": { "type": "object", "properties": { "id": { "type": "string", "description": "Only this saved volume." } } } },
        { "name": "volume_duplicates", "description": "Finds copies between files under 'path' and the inventories of saved (possibly disconnected) disks, using size + a sampled semihash (8 KiB fast / 48 KiB deep). Also reports duplicates between saved disks without touching any disk. Read-only. Experimental.", "annotations": ro,
          "inputSchema": { "type": "object", "properties": { "path": path,
            "volume": { "type": "string", "description": "Only this saved volume id." },
            "mode": { "type": "string", "enum": ["fast", "deep"], "description": "fast (default) samples 64 KiB+ files under a 3 s budget; deep samples everything." },
            "limit": limit } } },
        { "name": "volume_lookup", "description": "Is this file on a saved disk? Checks its size and sampled semihash against every saved inventory, even if the disk is disconnected. Read-only. Experimental.", "annotations": ro,
          "inputSchema": { "type": "object", "properties": { "path": { "type": "string", "description": "Local file path." }, "deep": { "type": "boolean", "description": "Use the 48 KiB sampled hash." } }, "required": ["path"] } },
        { "name": "provenance", "description": "Where did this file come from? Reads the Zone.Identifier ADS (download URL), matches the install folder against the Windows Uninstall registry and Steam libraries, and in deep mode adds run_count from Everything. Read-only. Experimental.", "annotations": ro,
          "inputSchema": { "type": "object", "properties": { "path": { "type": "string", "description": "File or folder in the index." }, "mode": { "type": "string", "enum": ["fast", "deep"], "description": "Deep also asks Everything for run_count." } } } },
        { "name": "timeline", "description": "Saved snapshots in chronological order: date and entry count, to see how far back the disk history goes. Read-only. Experimental.", "annotations": ro,
          "inputSchema": { "type": "object", "properties": { "limit": limit } } },
        { "name": "quarantine_list", "description": "Files the app moved to quarantine: original path, size, date and whether the saved copy still exists. Restore/delete happen in the app. Read-only. Experimental.", "annotations": ro,
          "inputSchema": { "type": "object", "properties": { "limit": limit } } },
        { "name": "speed_test", "description": "Measures the real speed of a disk and/or Google Drive before estimating a task. Detects NVMe/SSD/HDD/USB by bus and seek penalty, reads a real file (deep), measures the actual Google Drive download by reading a cloud placeholder (hydrates that file), reports how many bytes are already local/pinned, and returns ETA for 'bytes'/'files'. Read-only except the opt-in hydration/benchmark file. Experimental.", "annotations": ro,
          "inputSchema": { "type": "object", "properties": { "path": { "type": "string", "description": "Folder whose disk/source to measure." },
            "bytes": { "type": "integer", "description": "Task size to estimate (default 1 GB)." },
            "files": { "type": "integer", "description": "Number of files in the task (default 1)." },
            "write": { "type": "boolean", "description": "Deep only: also benchmark writes with a temporary file (default false)." },
            "mode": { "type": "string", "enum": ["fast", "deep"], "description": "fast uses classes and local percentages; deep measures read/download (and write if asked)." } } } },
        { "name": "addons", "description": "Detects optional external tools (rclone and gphotos-sync for Google Photos, Tesseract OCR, pdftotext) and returns setup steps and commands. Read-only; no credentials. Experimental.", "annotations": ro,
          "inputSchema": { "type": "object", "properties": {} } },
        { "name": "content_search", "description": "Search inside files under 'path'. Fast reads text/code and the first pages of PDFs via pdftotext; deep adds OCR for images and scanned PDFs via tesseract and reads more pages. Returns snippets, elapsed_ms and partial. Uses external tools if installed; read-only. Experimental.", "annotations": ro,
          "inputSchema": { "type": "object", "properties": { "path": path, "query": { "type": "string", "description": "Text to find (case-insensitive)." },
            "mode": { "type": "string", "enum": ["fast", "deep"], "description": "fast (default) or deep with OCR." },
            "max_files": { "type": "integer", "description": "Safety cap (default 4000 fast / 50000 deep)." },
            "budget_ms": { "type": "integer", "description": "Wall-clock budget (default 2500 fast / 60000 deep)." },
            "limit": limit }, "required": ["path", "query"] } },
        { "name": "content_estimate", "description": "Estimates how many text/PDF/image files are under a folder and how long a fast or deep content search would take, before running it. Read-only. Experimental.", "annotations": ro,
          "inputSchema": { "type": "object", "properties": { "path": path } } },
        { "name": "refresh", "description": "Reload the index from Everything.", "annotations": ro,
          "inputSchema": { "type": "object", "properties": {} } }
    ])
}

/// Serve MCP on stdin/stdout until stdin closes.
pub fn serve() {
    let mut server = Server::new(Box::new(|| {
        let excluded = crate::i18n::Settings::load().excluded_drives;
        let saved = crate::volumes::Store::default().enabled_full();
        let offline = crate::volumes::offline_drives(&saved);
        tree::load_with_offline(Arc::new(Progress::default()), &excluded, &offline).map(Arc::new)
    }));
    let stdin = std::io::stdin();
    let mut out = std::io::stdout();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        if line.trim().is_empty() {
            continue;
        }
        let reply = match serde_json::from_str::<Value>(&line) {
            Ok(msg) => server.handle(&msg),
            Err(e) => Some(json!({ "jsonrpc": "2.0", "id": null, "error": { "code": -32700, "message": e.to_string() } })),
        };
        if let Some(r) = reply {
            let _ = writeln!(out, "{r}");
            let _ = out.flush();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filetime_epoch_conversion() {
        assert_eq!(filetime_to_unix(0), None);
        // 1970-01-01 en FILETIME.
        assert_eq!(filetime_to_unix(116_444_736_000_000_000), Some(0));
        // Una hora después.
        assert_eq!(
            filetime_to_unix(116_444_736_000_000_000 + 10_000_000 * 3600),
            Some(3600)
        );
    }
}
