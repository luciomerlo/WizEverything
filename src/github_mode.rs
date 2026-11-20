//! "GitHub mode": compare any chosen folder (a Git repo or a folder inside one)
//! against its remote. Answers the headline question first — is the local copy
//! up to date with the web, or newer than it? — and then explains the
//! differences: commits, files, size by folder and where the space goes.
//!
//! Read-only: it may `git fetch` (updates remote-tracking refs only) and call
//! the GitHub API, but never touches the working tree, index or history.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::{Value, json};

pub struct TreeSizes {
    pub logical: u64,
    pub alloc: u64,
    pub children: Vec<(String, u64, u64)>,
}

pub struct Opts {
    /// Query the remote (`git fetch`). When false, only local data is used.
    pub fetch: bool,
    pub limit: usize,
}

fn git(root: &Path, args: &[&str]) -> Result<String, String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GCM_INTERACTIVE", "never")
        .env("GIT_ASKPASS", "echo")
        .output()
        .map_err(|e| format!("git could not be run: {e}"))?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
    } else {
        Err(String::from_utf8_lossy(&out.stderr).trim().to_string())
    }
}

/// Repo root for `path` (walks up), or `None` when it is not a Git repo.
pub fn repo_root(path: &Path) -> Option<PathBuf> {
    let s = git(path, &["rev-parse", "--show-toplevel"]).ok()?;
    let s = s.trim();
    if s.is_empty() {
        None
    } else {
        Some(PathBuf::from(s.replace('/', "\\")))
    }
}

fn fmt_bytes(n: u64) -> String {
    let f = n as f64;
    if f >= 1024.0 * 1024.0 * 1024.0 {
        format!("{:.2} GiB", f / (1024.0 * 1024.0 * 1024.0))
    } else if f >= 1024.0 * 1024.0 {
        format!("{:.1} MiB", f / (1024.0 * 1024.0))
    } else if f >= 1024.0 {
        format!("{:.0} KiB", f / 1024.0)
    } else {
        format!("{n} B")
    }
}

fn github_slug(remote: &str) -> Option<String> {
    let r = remote.trim().trim_end_matches(".git");
    if let Some(rest) = r.strip_prefix("https://github.com/") {
        return Some(rest.to_string());
    }
    if let Some(rest) = r.strip_prefix("http://github.com/") {
        return Some(rest.to_string());
    }
    if let Some(rest) = r.strip_prefix("git@github.com:") {
        return Some(rest.to_string());
    }
    None
}

fn host_of(remote: &str) -> String {
    let r = remote.trim();
    if let Some(rest) = r.split("://").nth(1) {
        return rest.split('/').next().unwrap_or("").to_string();
    }
    if let Some((_user, rest)) = r.split_once('@') {
        return rest.split(':').next().unwrap_or("").to_string();
    }
    String::new()
}

fn parse_log(out: &str) -> Vec<Value> {
    out.lines()
        .filter_map(|l| {
            let mut it = l.splitn(3, '|');
            let hash = it.next()?.trim();
            if hash.is_empty() {
                return None;
            }
            Some(json!({
                "hash": hash,
                "date": it.next().unwrap_or("").trim(),
                "subject": it.next().unwrap_or("").trim(),
            }))
        })
        .collect()
}

/// Sync status from the raw counts (pure, so it is easy to test).
fn classify(has_remote: bool, detached: bool, verified: bool, ahead: u64, behind: u64) -> &'static str {
    if !has_remote {
        "no_remote"
    } else if detached {
        "detached"
    } else if !verified {
        "unknown"
    } else if ahead > 0 && behind > 0 {
        "diverged"
    } else if ahead > 0 {
        "local_newer"
    } else if behind > 0 {
        "behind"
    } else {
        "up_to_date"
    }
}

fn top_folder(p: &str) -> String {
    let p = p.replace('\\', "/");
    let first = p.split('/').next().unwrap_or("").to_string();
    if first.is_empty() { "(root)".to_string() } else { first }
}

/// Bytes of the tracked HEAD tree, total and per top-level folder.
fn tracked_sizes(root: &Path) -> (u64, HashMap<String, u64>) {
    let mut total = 0u64;
    let mut by: HashMap<String, u64> = HashMap::new();
    let Ok(out) = git(root, &["ls-tree", "-r", "-l", "HEAD"]) else {
        return (0, by);
    };
    for line in out.lines() {
        let Some((meta, path)) = line.split_once('\t') else { continue };
        let size: u64 = meta
            .split_whitespace()
            .nth(3)
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
        total += size;
        *by.entry(top_folder(path)).or_insert(0) += size;
    }
    (total, by)
}

fn parse_porcelain_path(line: &str) -> Option<String> {
    let rest = line.get(3..)?.trim();
    let p = if let Some((_, new)) = rest.split_once(" -> ") { new } else { rest };
    Some(p.trim_matches('"').replace('\\', "/"))
}

fn github_api(slug: &str) -> Option<Value> {
    let url = format!("https://api.github.com/repos/{slug}");
    let mut req = ureq::get(&url)
        .set("User-Agent", "WizEverything")
        .set("Accept", "application/vnd.github+json");
    if let Ok(t) = std::env::var("GITHUB_TOKEN")
        && !t.trim().is_empty()
    {
        req = req.set("Authorization", &format!("Bearer {}", t.trim()));
    }
    let resp = req.call().ok()?;
    resp.into_json::<Value>().ok()
}

pub fn inspect(start: &Path, sizes: Option<&TreeSizes>, opts: &Opts) -> Value {
    let Some(root) = repo_root(start) else {
        return json!({
            "status": "not_repo",
            "title": "Not a Git repository",
            "title_es": "No es un repositorio Git",
            "path": start.to_string_lossy(),
            "notes": ["Elegí una carpeta que sea un repo, o una carpeta dentro de uno."],
        });
    };
    let root_s = root.to_string_lossy().to_string();

    let remote = git(&root, &["remote", "get-url", "origin"]).unwrap_or_default();
    let remote = remote.trim().to_string();
    let slug = github_slug(&remote);
    let host = host_of(&remote);
    let is_github = slug.is_some();

    let branch_raw = git(&root, &["rev-parse", "--abbrev-ref", "HEAD"]).unwrap_or_default();
    let detached = branch_raw == "HEAD" || branch_raw.is_empty();
    let branch = if detached { "HEAD".to_string() } else { branch_raw };
    let head = git(&root, &["rev-parse", "HEAD"]).unwrap_or_default();
    let head_short = head.chars().take(7).collect::<String>();

    let mut notes: Vec<String> = Vec::new();
    let mut fetched = false;
    let mut remote_sha = String::new();
    let mut upstream = String::new();

    if remote.is_empty() {
        notes.push("No 'origin' remote: nothing to compare against.".into());
    } else if opts.fetch {
        match git(&root, &["fetch", "--quiet", "--prune", "origin"]) {
            Ok(_) => fetched = true,
            Err(e) => notes.push(format!("Could not fetch (network/credentials): {e}")),
        }
    }
    if remote.is_empty() {
        // nothing
    } else {
        // Fast mode reads the local origin refs; deep mode has just refreshed them.
        if !fetched {
            notes.push("Sin fetch: se compara contra la última copia conocida de origin. Usá profundo para actualizar.".into());
        }
        let mut up = git(&root, &["rev-parse", "--abbrev-ref", "--symbolic-full-name", "@{u}"])
            .unwrap_or_default();
        if up.is_empty() || git(&root, &["rev-parse", "--verify", "--quiet", &up]).is_err() {
            // No upstream: compare against the branch on origin, then the default branch.
            for cand in [
                format!("origin/{branch}"),
                "origin/HEAD".to_string(),
                "origin/main".to_string(),
                "origin/master".to_string(),
            ] {
                if git(&root, &["rev-parse", "--verify", "--quiet", &cand]).is_ok() {
                    up = cand;
                    break;
                }
            }
        }
        if up == "origin/HEAD"
            && let Ok(b) = git(&root, &["rev-parse", "--abbrev-ref", "origin/HEAD"])
            && !b.is_empty()
            && b != "HEAD"
        {
            up = b;
        }
        if !up.is_empty() && !detached && up != format!("origin/{branch}") {
            notes.push(format!("Branch '{branch}' has no upstream; compared against {up}."));
        }
        upstream = up;
        remote_sha = git(&root, &["rev-parse", "--verify", &upstream]).unwrap_or_default();
    }

    let mut ahead = 0u64;
    let mut behind = 0u64;
    let mut verified = false;
    if !remote_sha.is_empty() {
        if remote_sha == head {
            verified = true;
        } else {
            if let Ok(c) = git(&root, &["rev-list", "--left-right", "--count", &format!("HEAD...{upstream}")]) {
                let mut parts = c.split_whitespace();
                ahead = parts.next().and_then(|x| x.parse().ok()).unwrap_or(0);
                behind = parts.next().and_then(|x| x.parse().ok()).unwrap_or(0);
                verified = true;
            }
        }
    }

    let status = classify(!remote.is_empty(), detached, verified, ahead, behind);

    let (title, title_es) = match status {
        "no_remote" => ("Local repo with no remote".to_string(), "Repo local sin remoto".to_string()),
        "detached" => ("Detached HEAD".to_string(), "HEAD separado (detached)".to_string()),
        "unknown" => (
            "Could not verify the remote".to_string(),
            "No se pudo verificar el remoto".to_string(),
        ),
        "diverged" => (
            format!("Diverged: {ahead} ahead, {behind} behind"),
            format!("DIVERGIDO: {ahead} adelante / {behind} atrás de la web"),
        ),
        "local_newer" => (
            format!("Local is newer than the web ({ahead} unpushed commits)"),
            format!("LOCAL MÁS NUEVO QUE LA WEB ({ahead} commits sin subir)"),
        ),
        "behind" => (
            format!("Behind the web by {behind} commits"),
            format!("ATRASADO respecto de la web ({behind} commits por detrás)"),
        ),
        _ => (
            "Up to date with GitHub".to_string(),
            "AL DÍA con GitHub".to_string(),
        ),
    };

    // Commits on each side.
    let mut commits_ahead = Vec::new();
    let mut commits_behind = Vec::new();
    if verified && !upstream.is_empty() {
        if ahead > 0
            && let Ok(o) = git(&root, &["log", "--format=%h|%ad|%s", "--date=short", "-n", "20", &format!("{upstream}..HEAD")])
        {
            commits_ahead = parse_log(&o);
        }
        if behind > 0
            && let Ok(o) = git(&root, &["log", "--format=%h|%ad|%s", "--date=short", "-n", "20", &format!("HEAD..{upstream}")])
        {
            commits_behind = parse_log(&o);
        }
    }

    // Working tree changes.
    let mut dirty: Vec<String> = Vec::new();
    if let Ok(o) = git(&root, &["status", "--porcelain"]) {
        for line in o.lines().take(500) {
            if let Some(p) = parse_porcelain_path(line) {
                dirty.push(p);
            }
        }
    }
    let mut dirty_by: HashMap<String, u64> = HashMap::new();
    for p in &dirty {
        *dirty_by.entry(top_folder(p)).or_insert(0) += 1;
    }
    let mut dirty_folders: Vec<Value> = dirty_by
        .into_iter()
        .map(|(folder, files)| json!({ "folder": folder, "files": files }))
        .collect();
    dirty_folders.sort_by(|a, b| {
        b["files"].as_u64().unwrap_or(0).cmp(&a["files"].as_u64().unwrap_or(0))
    });

    // Files changed upstream (what you are missing).
    let mut upstream_files: Vec<Value> = Vec::new();
    let mut upstream_by: Vec<Value> = Vec::new();
    if verified && behind > 0 {
        if let Ok(o) = git(&root, &["diff", "--name-status", &format!("HEAD..{upstream}")]) {
            for line in o.lines().take(200) {
                let mut it = line.split('\t');
                let code = it.next().unwrap_or("").to_string();
                let path = it.next().unwrap_or("").replace('\\', "/");
                if path.is_empty() {
                    continue;
                }
                upstream_files.push(json!({ "status": code, "path": path }));
            }
        }
        if let Ok(o) = git(&root, &["diff", "--dirstat=files,0", &format!("HEAD..{upstream}")]) {
            for line in o.lines().take(40) {
                let mut it = line.split_whitespace();
                let pct = it.next().unwrap_or("").trim_end_matches('%').replace(',', ".");
                let folder = it.last().unwrap_or("");
                if let Ok(p) = pct.parse::<f64>() {
                    upstream_by.push(json!({ "percent": p, "folder": folder }));
                }
            }
        }
    }

    // Size: tracked content, .git data, local disk (from the index when known).
    let (tracked_total, tracked_by) = tracked_sizes(&root);
    let (mut loose_kb, mut pack_kb) = (0u64, 0u64);
    if let Ok(o) = git(&root, &["count-objects", "-v"]) {
        for line in o.lines() {
            if let Some(v) = line.strip_prefix("size: ") {
                loose_kb = v.trim().parse().unwrap_or(0);
            }
            if let Some(v) = line.strip_prefix("size-pack: ") {
                pack_kb = v.trim().parse().unwrap_or(0);
            }
        }
    }
    let git_bytes = (loose_kb + pack_kb) * 1024;

    let mut github = Value::Null;
    if let Some(slug) = &slug
        && let Some(v) = github_api(slug)
    {
        github = json!({
            "repo": slug,
            "size_kb": v.get("size").cloned().unwrap_or(Value::Null),
            "default_branch": v.get("default_branch").cloned().unwrap_or(Value::Null),
            "pushed_at": v.get("pushed_at").cloned().unwrap_or(Value::Null),
            "archived": v.get("archived").cloned().unwrap_or(Value::Null),
            "private": v.get("private").cloned().unwrap_or(Value::Null),
        });
    }
    let github_kb = github.get("size_kb").and_then(Value::as_u64);

    let (local_logical, local_alloc) = sizes.map(|s| (s.logical, s.alloc)).unwrap_or((0, 0));

    // Per-folder table: local disk (index) vs tracked bytes.
    let mut folders: Vec<Value> = Vec::new();
    if let Some(s) = sizes {
        for (name, _logical, alloc) in &s.children {
            let tracked = tracked_by.get(name).copied().unwrap_or(0);
            let delta = (*alloc as i64) - (tracked as i64);
            folders.push(json!({
                "folder": name,
                "local_bytes": alloc,
                "tracked_bytes": tracked,
                "delta_bytes": delta,
                "percent": if local_alloc > 0 { (*alloc as f64) * 100.0 / local_alloc as f64 } else { 0.0 },
            }));
        }
        folders.sort_by(|a, b| {
            b["local_bytes"].as_u64().unwrap_or(0).cmp(&a["local_bytes"].as_u64().unwrap_or(0))
        });
        folders.truncate(opts.limit.max(1));
    }

    let biggest: Vec<String> = folders
        .iter()
        .take(3)
        .filter_map(|f| f.get("folder").and_then(Value::as_str).map(|s| s.to_string()))
        .collect();
    let explanation = if sizes.is_some() {
        format!(
            "On disk {} · versioned {} · .git {} (loose {} + pack {}). GitHub reports {}. The gap is mostly untracked/ignored data{}.",
            fmt_bytes(local_alloc),
            fmt_bytes(tracked_total),
            fmt_bytes(git_bytes),
            fmt_bytes(loose_kb * 1024),
            fmt_bytes(pack_kb * 1024),
            github_kb.map(|k| format!("{} KiB", k)).unwrap_or_else(|| "n/a".into()),
            if biggest.is_empty() { String::new() } else { format!(", led by {}", biggest.join(", ")) },
        )
    } else {
        format!(
            "Versioned {} · .git {}. (Open the repo folder in the app to get its on-disk size.)",
            fmt_bytes(tracked_total),
            fmt_bytes(git_bytes)
        )
    };

    let mut alternatives = vec![
        "If local is newer: push the unpushed commits (review them first).",
        "If behind: pull/rebase after reading which folders changed upstream.",
        "If diverged: review both sides before merging; keep a branch as backup.",
        "Build artifacts (target/, node_modules/) explain most of the disk gap: quarantine instead of deleting.",
        "Uncommitted work: commit or stash before updating.",
    ];
    if loose_kb > 0 && pack_kb == 0 {
        alternatives.push("Loose objects only: `git gc` would pack .git and save space.");
    }

    json!({
        "status": status,
        "title": title,
        "title_es": title_es,
        "path": start.to_string_lossy(),
        "repo_root": root_s,
        "remote": remote,
        "host": host,
        "is_github": is_github,
        "branch": branch,
        "head": head_short,
        "upstream": upstream,
        "fetched": fetched,
        "verified": verified,
        "ahead": ahead,
        "behind": behind,
        "dirty": dirty.len(),
        "dirty_files": dirty.iter().take(50).collect::<Vec<_>>(),
        "dirty_by_folder": dirty_folders,
        "commits": { "ahead": commits_ahead, "behind": commits_behind },
        "changes": { "upstream_files": upstream_files, "upstream_by_folder": upstream_by },
        "size": {
            "local_logical": local_logical,
            "local_alloc": local_alloc,
            "tracked": tracked_total,
            "git_loose": loose_kb * 1024,
            "git_pack": pack_kb * 1024,
            "github_kb": github_kb,
            "explanation": explanation,
        },
        "github": github,
        "folders": folders,
        "alternatives": alternatives,
        "notes": notes,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slugs_from_common_remotes() {
        assert_eq!(
            github_slug("https://github.com/luciomerlo/WizEverything.git").as_deref(),
            Some("luciomerlo/WizEverything")
        );
        assert_eq!(
            github_slug("git@github.com:owner/repo.git").as_deref(),
            Some("owner/repo")
        );
        assert_eq!(github_slug("https://gitlab.com/a/b.git"), None);
    }

    #[test]
    fn host_parsing() {
        assert_eq!(host_of("https://github.com/a/b"), "github.com");
        assert_eq!(host_of("git@github.com:a/b"), "github.com");
    }

    #[test]
    fn top_folder_normalizes() {
        assert_eq!(top_folder("src\\app.rs"), "src");
        assert_eq!(top_folder("README.md"), "README.md");
    }

    #[test]
    fn porcelain_paths() {
        assert_eq!(parse_porcelain_path(" M src/app.rs").as_deref(), Some("src/app.rs"));
        assert_eq!(parse_porcelain_path("R  a.rs -> b.rs").as_deref(), Some("b.rs"));
    }

    #[test]
    fn log_parsing() {
        let v = parse_log("abc1234|2026-11-20|Fix thing\ndef5678|2026-11-19|Add thing");
        assert_eq!(v.len(), 2);
        assert_eq!(v[0]["hash"], "abc1234");
    }

    #[test]
    fn status_classification() {
        assert_eq!(classify(false, false, false, 0, 0), "no_remote");
        assert_eq!(classify(true, true, true, 0, 0), "detached");
        assert_eq!(classify(true, false, false, 0, 0), "unknown");
        assert_eq!(classify(true, false, true, 0, 0), "up_to_date");
        assert_eq!(classify(true, false, true, 3, 0), "local_newer");
        assert_eq!(classify(true, false, true, 0, 5), "behind");
        assert_eq!(classify(true, false, true, 2, 4), "diverged");
    }
}
