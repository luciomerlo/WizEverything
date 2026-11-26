//! MCP server (Model Context Protocol) over stdio: `wizeverything.exe --mcp`.
//!
//! Lets an AI assistant ask about disk usage, file types and duplicates using
//! the same in-memory index as the app. JSON-RPC 2.0, one message per line.

use std::io::{BufRead, Write};
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
                "instructions": "Read-only. Disk usage, file types and duplicate files from the Everything index on this Windows PC. It never deletes or changes files. Sizes are in bytes. Paths look like C:\\Users\\name."
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
                let opt = DupOptions {
                    root,
                    min_size: (a.get("min_size_mb").and_then(Value::as_f64).unwrap_or(1.0) * 1024.0 * 1024.0) as u64,
                    same_name: a.get("same_name").and_then(Value::as_bool).unwrap_or(true),
                    verify: a.get("verify").and_then(Value::as_bool).unwrap_or(true),
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
            _ => Err(format!("Unknown tool: {name}")),
        }
    }
}

/// Every tool is read-only: nothing here deletes, moves or writes files.
pub fn tools() -> Value {
    let ro = json!({ "readOnlyHint": true, "destructiveHint": false, "openWorldHint": false });
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
            "same_name": { "type": "boolean", "description": "Only compare files with the same name (default true, faster)." },
            "verify": { "type": "boolean", "description": "Confirm by content hashes (default true). false = name + size only, reads nothing (safe for cloud drives)." },
            "all_drives": { "type": "boolean", "description": "Search every included drive when no path is given." },
            "versions": { "type": "boolean", "description": "Old versions: same name and folder name on any drive, even if size/content differ. Index only." },
            "exclude": { "type": "array", "items": { "type": "string" }, "description": "Extra folders to skip (the app's saved exclusions always apply)." } } } },
        { "name": "refresh", "description": "Reload the index from Everything.", "annotations": ro,
          "inputSchema": { "type": "object", "properties": {} } }
    ])
}

/// Serve MCP on stdin/stdout until stdin closes.
pub fn serve() {
    let mut server = Server::new(Box::new(|| {
        tree::load(Arc::new(Progress::default()), &crate::i18n::Settings::load().excluded_drives).map(Arc::new)
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
