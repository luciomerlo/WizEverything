//! MCP server (Model Context Protocol) over stdio: `wizeverything.exe --mcp`.
//!
//! Lets an AI assistant ask about disk usage, file types and duplicates using
//! the same in-memory index as the app. JSON-RPC 2.0, one message per line.

use std::io::{BufRead, Write};
use std::sync::Arc;

use serde_json::{Value, json};

use crate::app::{CATEGORIES, category_name, category_of, fmt_iso};
use crate::dup::{self, DupGroup, DupOptions, DupProgress};
use crate::rules::{self, Keep, Proposal, Rules};
use crate::tree::{self, Progress, ROOT, Tree};

const PROTOCOL: &str = "2025-06-18";

pub struct Server {
    tree: Option<Arc<Tree>>,
    loader: Box<dyn Fn() -> Result<Arc<Tree>, String> + Send>,
    /// Duplicate rules for the proposal tools (None = the saved settings).
    rules: Option<Rules>,
    /// Folders the duplicate tools skip (None = the saved exclusions).
    exclude: Option<Vec<String>>,
    /// Made by the propose_* tools; the app shows them as cards. Data only:
    /// nothing is marked until the user applies one.
    proposals: Vec<Proposal>,
}

/// Told to the model with every proposal.
const PROPOSAL_NOTE: &str = "Proposal only: nothing was marked, moved or deleted. The app shows it as a card; \
     the user may apply the marks in the Duplicates tab and must still confirm any Recycle Bin or hard-link action.";

impl Server {
    pub fn new(loader: Box<dyn Fn() -> Result<Arc<Tree>, String> + Send>) -> Self {
        Self { tree: None, loader, rules: None, exclude: None, proposals: Vec::new() }
    }

    /// Use these duplicate rules instead of the saved settings.
    pub fn with_rules(mut self, rules: Rules) -> Self {
        self.rules = Some(rules);
        self
    }

    /// Use these duplicate exclusions instead of the saved ones.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn with_exclusions(mut self, folders: Vec<String>) -> Self {
        self.exclude = Some(folders);
        self
    }

    fn saved_exclude(&self) -> Vec<String> {
        self.exclude.clone().unwrap_or_else(|| crate::i18n::Settings::load().dup_exclude)
    }

    /// Proposals made since the last call.
    pub fn take_proposals(&mut self) -> Vec<Proposal> {
        std::mem::take(&mut self.proposals)
    }

    fn push_proposal(&mut self, p: Proposal) {
        // A standalone MCP server never collects them: keep only the latest few.
        if self.proposals.len() >= 4 {
            self.proposals.remove(0);
        }
        self.proposals.push(p);
    }

    /// Rules in effect for this call: the app's (or saved) rules plus any extra
    /// protected folders given by the caller.
    fn rules_for(&self, a: &Value) -> Rules {
        let mut r = self.rules.clone().unwrap_or_else(|| Rules::from_settings(&crate::i18n::Settings::load()));
        r.protect.extend(a.get("protect").and_then(Value::as_array).into_iter().flatten().filter_map(|v| v.as_str().map(String::from)));
        if let Some(f) = a.get("prefer_folder").and_then(Value::as_str).filter(|f| !f.trim().is_empty()) {
            r.prefer_folder = f.to_string();
        }
        r
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
                let (groups, prog) = dup_search(&t, root, a, 1.0, self.saved_exclude())?;
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
            "propose_duplicate_marks" => {
                let keep = Keep::parse(a.get("keep").and_then(Value::as_str).unwrap_or(""))
                    .ok_or("keep must be one of: smart, oldest, newest, shortest_path, longest_path")?;
                let rules = self.rules_for(a);
                let (groups, _) = dup_search(&t, root, a, 1.0, self.saved_exclude())?;
                let marks = rules::plan_marks(&groups, &rules, keep);
                let v = json!({
                    "proposal_only": true,
                    "note": PROPOSAL_NOTE,
                    "rule": keep.id(),
                    "groups": groups.len(),
                    "all_verified_by_content": groups.iter().all(|g| g.verified),
                    "copies_to_mark": marks.marked.len(),
                    "bytes_freed": rules::marked_bytes(&groups, &marks.marked),
                    "protected_copies_left_unmarked": marks.protected.len(),
                    "suggestions": groups.iter().take(limit).map(|g| mark_json(g, &marks)).collect::<Vec<_>>(),
                });
                self.push_proposal(Proposal { kind: "duplicate_marks", rule: keep, groups, marks, items: Vec::new() });
                Ok(v)
            }
            "propose_cleanup" => {
                let keep = Keep::parse(a.get("keep").and_then(Value::as_str).unwrap_or("")).unwrap_or(Keep::Smart);
                let rules = self.rules_for(a);
                let min_mb = a.get("min_size_mb").and_then(Value::as_f64).unwrap_or(10.0);
                let (groups, _) = dup_search(&t, root, a, min_mb, self.saved_exclude())?;
                let marks = rules::plan_marks(&groups, &rules, keep);
                let items = cleanup_candidates(&t, root, (min_mb * 1024.0 * 1024.0) as u64, &rules);
                let dup_bytes = rules::marked_bytes(&groups, &marks.marked);
                let item_bytes: u64 = items.iter().map(|i| i.1).sum();
                let v = json!({
                    "proposal_only": true,
                    "note": PROPOSAL_NOTE,
                    "total_bytes": dup_bytes + item_bytes,
                    "duplicates": {
                        "rule": keep.id(),
                        "copies_to_mark": marks.marked.len(),
                        "bytes": dup_bytes,
                        "all_verified_by_content": groups.iter().all(|g| g.verified),
                        "top": groups.iter().take(limit.min(10)).map(|g| mark_json(g, &marks)).collect::<Vec<_>>(),
                    },
                    "other_candidates_bytes": item_bytes,
                    "other_candidates": items.iter().take(limit).map(|(p, s, w)| json!({ "path": p, "size": s, "why": w })).collect::<Vec<_>>(),
                });
                self.push_proposal(Proposal { kind: "cleanup", rule: keep, groups, marks, items });
                Ok(v)
            }
            _ => Err(format!("Unknown tool: {name}")),
        }
    }
}

/// Run a duplicate search from tool arguments (shared by find/propose tools).
fn dup_search(t: &Arc<Tree>, root: u32, a: &Value, default_min_mb: f64, saved_exclude: Vec<String>) -> Result<(Vec<DupGroup>, Arc<DupProgress>), String> {
    if root == ROOT && !a.get("all_drives").and_then(Value::as_bool).unwrap_or(false) {
        return Err("Give a 'path' to search (or all_drives=true; that can read a lot of data).".into());
    }
    let opt = DupOptions {
        root,
        min_size: (a.get("min_size_mb").and_then(Value::as_f64).unwrap_or(default_min_mb) * 1024.0 * 1024.0) as u64,
        same_name: a.get("same_name").and_then(Value::as_bool).unwrap_or(true),
        verify: a.get("verify").and_then(Value::as_bool).unwrap_or(true),
        versions: a.get("versions").and_then(Value::as_bool).unwrap_or(false),
        // The app's saved exclusions plus any given here.
        exclude: saved_exclude
            .into_iter()
            .chain(a.get("exclude").and_then(Value::as_array).into_iter().flatten().filter_map(|v| v.as_str().map(String::from)))
            .filter_map(|p| t.find_path(&p.replace('/', "\\")))
            .filter(|&id| id != ROOT)
            .collect(),
    };
    let prog = Arc::new(DupProgress::default());
    let groups = dup::find(t.clone(), opt, prog.clone());
    Ok((groups, prog))
}

/// One group of a marks proposal: which copy stays, why, and what gets marked.
fn mark_json(g: &DupGroup, marks: &rules::Marks) -> Value {
    let kept = g.files.iter().find(|f| marks.reasons.contains_key(&f.path));
    json!({
        "kind": if g.folder { "folder" } else { "file" },
        "verified_by_content": g.verified,
        "size": g.size,
        "keep": kept.map(|f| f.path.clone()),
        "why": kept.and_then(|f| marks.reasons.get(&f.path)).cloned(),
        "mark": g.files.iter().filter(|f| marks.marked.contains(&f.path)).map(|f| f.path.clone()).collect::<Vec<_>>(),
        "protected": g.files.iter().filter(|f| marks.protected.contains(&f.path)).map(|f| f.path.clone()).collect::<Vec<_>>(),
    })
}

/// Other space worth reviewing (never marked by a proposal): temporary folders,
/// installers and disk images in Downloads, logs and crash dumps. Skips
/// protected folders and program/system folders. Largest first.
fn cleanup_candidates(t: &Tree, root: u32, min_size: u64, rules: &Rules) -> Vec<(String, u64, String)> {
    const TEMP: &[&str] = &["temp", "tmp", "$recycle.bin", "crashdumps"];
    const INSTALL: &[&str] = &["iso", "msi", "exe", "zip", "7z", "rar", "img", "msix", "appx"];
    const LOGS: &[&str] = &["log", "dmp", "mdmp", "etl", "bak", "old", "tmp", "crdownload", "part"];
    let system: Vec<String> = crate::app::suggested_exclusions()
        .iter()
        .map(|p| rules::norm(p))
        .filter(|p| !p.ends_with("$recycle.bin"))
        .collect();
    let in_system = |p: &str| system.iter().any(|s| p == s || p.starts_with(&format!("{s}\\")));
    let programs = |p: &str| p.contains("\\program files") || p.contains("\\programdata");
    let mut out = Vec::new();
    let mut stack = vec![root];
    while let Some(id) = stack.pop() {
        let n = &t.nodes[id as usize];
        let name = t.name(id).to_lowercase();
        if n.is_dir() {
            if id != root && id != ROOT && n.parent != ROOT && TEMP.contains(&name.as_str()) {
                let path = t.full_path(id);
                let lp = rules::norm(&path);
                if n.size >= min_size && !programs(&lp) && !rules.is_protected(&path) {
                    out.push((path, n.size, crate::i18n::tr("carpeta temporal o papelera", "temporary folder or Recycle Bin").to_string()));
                }
                continue; // its content is covered by the folder
            }
            if n.parent == ROOT && name == "$recycle.bin" {
                continue;
            }
            stack.extend_from_slice(t.children(id));
            continue;
        }
        if n.size < min_size {
            continue;
        }
        let ext = t.exts.get(n.ext as usize).map_or("", |e| e.name.as_str());
        let path = t.full_path(id);
        let lp = rules::norm(&path);
        let why = if INSTALL.contains(&ext) && (lp.contains("\\downloads\\") || lp.contains("\\descargas\\")) {
            crate::i18n::tr("instalador o imagen en Descargas", "installer or image in Downloads")
        } else if LOGS.contains(&ext) && !in_system(&lp) {
            crate::i18n::tr("log, volcado o temporal", "log, dump or temporary file")
        } else {
            continue;
        };
        if !rules.is_protected(&path) {
            out.push((path, n.size, why.to_string()));
        }
    }
    out.sort_by(|a, b| b.1.cmp(&a.1));
    out.truncate(500);
    out
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
        { "name": "propose_duplicate_marks", "description": "Suggest which duplicate copies to mark (and which to keep, with the reason) using the app's rules: protected folders are never marked, the preferred folder/drive wins. Returns a proposal only: it marks, moves and deletes nothing; the user applies it in the app.", "annotations": ro,
          "inputSchema": { "type": "object", "properties": { "path": path, "limit": limit,
            "keep": { "type": "string", "enum": ["smart", "oldest", "newest", "shortest_path", "longest_path"], "description": "Which copy to keep (default smart: the most 'official' location)." },
            "min_size_mb": { "type": "number", "description": "Ignore files smaller than this (default 1)." },
            "same_name": { "type": "boolean", "description": "Only compare files with the same name (default true)." },
            "verify": { "type": "boolean", "description": "Confirm by content hashes (default true; false = name + size only, reads nothing)." },
            "all_drives": { "type": "boolean", "description": "Search every included drive when no path is given." },
            "protect": { "type": "array", "items": { "type": "string" }, "description": "Extra folders whose copies must never be marked." },
            "prefer_folder": { "type": "string", "description": "Keep the copy inside this folder when there is one." } } } },
        { "name": "propose_cleanup", "description": "Suggest where space could be freed under a folder: duplicate copies to mark (app rules apply) plus temporary folders, installers in Downloads and logs/dumps to review. Proposal only: nothing is changed.", "annotations": ro,
          "inputSchema": { "type": "object", "properties": { "path": path, "limit": limit,
            "min_size_mb": { "type": "number", "description": "Ignore items smaller than this (default 10)." },
            "verify": { "type": "boolean", "description": "Confirm duplicates by content (default true; false = index only)." },
            "keep": { "type": "string", "enum": ["smart", "oldest", "newest", "shortest_path", "longest_path"], "description": "Which duplicate copy to keep (default smart)." },
            "all_drives": { "type": "boolean", "description": "Look at every included drive when no path is given." } } } },
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
