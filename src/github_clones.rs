//! Multi-copia de repositorios Git: agrupa clones por remoto para decidir qué
//! copia conservar. Sólo lectura: no toca `.git`, no hace fetch y no borra.

use std::collections::HashMap;
use std::path::Path;
use std::time::SystemTime;

use serde_json::{json, Value};

use crate::tree::{Tree, ROOT};

const FAST_REPOS: usize = 500;

fn normalize(url: &str) -> String {
    let mut u = url.trim().trim_end_matches('/').to_string();
    if let Some(rest) = u.strip_suffix(".git") {
        u = rest.to_string();
    }
    // git@github.com:user/repo y https://github.com/user/repo son el mismo remoto.
    if let Some(rest) = u.strip_prefix("git@") {
        u = rest.replacen(':', "/", 1);
    }
    for scheme in ["https://", "http://", "ssh://", "git://"] {
        if let Some(rest) = u.strip_prefix(scheme) {
            u = rest.to_string();
            break;
        }
    }
    u.to_lowercase()
}

fn remote_of(repo: &Path) -> Option<String> {
    let cfg = repo.join(".git").join("config");
    let text = std::fs::read_to_string(cfg).ok()?;
    let mut in_origin = false;
    for line in text.lines() {
        let t = line.trim();
        if t.starts_with('[') {
            in_origin = t.eq_ignore_ascii_case("[remote \"origin\"]");
            continue;
        }
        if in_origin && let Some((k, v)) = t.split_once('=') {
            if k.trim().eq_ignore_ascii_case("url") {
                return Some(v.trim().to_string());
            }
        }
    }
    None
}

fn age_days(modified: u64) -> u64 {
    let now = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).map_or(0, |d| d.as_secs());
    if modified == 0 || modified > now { 0 } else { (now - modified) / 86_400 }
}

/// Encuentra repos bajo `root`, agrupa por remoto y calcula bytes redundantes.
pub fn clones(t: &Tree, root: u32, limit: usize, deep: bool) -> Value {
    let started = std::time::Instant::now();
    let cap = if deep { usize::MAX } else { FAST_REPOS };
    let mut repos: Vec<(String, u64, u64)> = Vec::new(); // path, bytes, modified
    let mut stack = vec![root];
    let mut seen = 0usize;
    while let Some(id) = stack.pop() {
        if id != ROOT && t.nodes[id as usize].is_dir() && t.name(id).eq_ignore_ascii_case(".git") {
            let parent = t.nodes[id as usize].parent;
            if parent != ROOT {
                let p = t.full_path(parent);
                let n = &t.nodes[parent as usize];
                repos.push((p, n.size, n.modified));
                seen += 1;
                if seen >= cap {
                    break;
                }
            }
            continue; // no bajar dentro de .git
        }
        for &c in t.children(id) {
            if t.nodes[c as usize].is_dir() {
                stack.push(c);
            }
        }
    }

    let mut groups: HashMap<String, Vec<Value>> = HashMap::new();
    let mut unreadable = 0usize;
    for (path, bytes, modified) in repos {
        let Some(url) = remote_of(Path::new(&path)) else {
            unreadable += 1;
            continue;
        };
        let key = normalize(&url);
        groups.entry(key).or_default().push(json!({
            "path": path,
            "bytes": bytes,
            "modified": modified,
            "age_days": age_days(modified),
        }));
    }

    let mut out: Vec<Value> = groups
        .into_iter()
        .filter(|(_, v)| v.len() > 1)
        .map(|(url, copies)| {
            let total: u64 = copies.iter().map(|c| c["bytes"].as_u64().unwrap_or(0)).sum();
            let max = copies.iter().map(|c| c["bytes"].as_u64().unwrap_or(0)).max().unwrap_or(0);
            json!({
                "remote": url,
                "copies": copies.len(),
                "total_bytes": total,
                "redundant_bytes": total.saturating_sub(max),
                "items": copies,
                "recommendation": "Conservar la copia más reciente y archivar el resto; verificar ramas sin pushear antes.",
            })
        })
        .collect();
    out.sort_by(|a, b| b["redundant_bytes"].as_u64().cmp(&a["redundant_bytes"].as_u64()));
    let shown = out.len().min(limit);
    out.truncate(limit);
    let redundant: u64 = out.iter().map(|g| g["redundant_bytes"].as_u64().unwrap_or(0)).sum();

    json!({
        "kind": "github_clones",
        "repos": seen,
        "groups": shown,
        "items": out,
        "redundant_bytes": redundant,
        "unreadable": unreadable,
        "partial": seen >= cap,
        "mode": if deep { "deep" } else { "fast" },
        "elapsed_ms": started.elapsed().as_millis() as u64,
        "note": "Sólo lectura. No hace fetch, no borra y no modifica .git.",
    })
}

#[cfg(test)]
mod tests {
    use super::normalize;

    #[test]
    fn remotes_are_normalized() {
        assert_eq!(normalize("git@github.com:User/Repo.git"), "github.com/user/repo");
        assert_eq!(normalize("https://github.com/User/Repo.git/"), "github.com/user/repo");
        assert_eq!(normalize("ssh://git@github.com/User/Repo.git"), "git@github.com/user/repo");
    }
}
