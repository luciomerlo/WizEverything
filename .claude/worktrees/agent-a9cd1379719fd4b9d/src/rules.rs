//! Selection rules for duplicates: protected folders, preferred folder/drive and
//! the marking strategies (smart, oldest, newest, shortest/longest path).
//!
//! Pure functions over [`DupGroup`]s so the app, the MCP proposal tools and the
//! tests all mark exactly the same copies.

use std::collections::{HashMap, HashSet};

use crate::dup::DupGroup;
use crate::i18n::tr;

/// Lowercase, backslashes, no trailing backslash ("C:\" → "c:").
pub fn norm(path: &str) -> String {
    path.trim().replace('/', "\\").trim_end_matches('\\').to_lowercase()
}

/// `path` is `dir` or lies inside it (both normalized).
fn within(path: &str, dir: &str) -> bool {
    !dir.is_empty() && (path == dir || (path.starts_with(dir) && path[dir.len()..].starts_with('\\')))
}

/// The user's duplicate rules.
#[derive(Clone, Default, Debug)]
pub struct Rules {
    /// Folders whose copies are never marked, recycled or replaced.
    pub protect: Vec<String>,
    /// "Keep the copy in this folder" (empty = none).
    pub prefer_folder: String,
    /// Drive whose copy wins (e.g. "H:"; empty = none).
    pub prefer_drive: String,
}

impl Rules {
    pub fn from_settings(s: &crate::i18n::Settings) -> Self {
        Self { protect: s.dup_protect.clone(), prefer_folder: s.dup_prefer_folder.clone(), prefer_drive: s.dup_prefer.clone() }
    }

    /// Inside a protected folder, or a folder that contains one (recycling it
    /// would take the protected folder along).
    pub fn is_protected(&self, path: &str) -> bool {
        let p = norm(path);
        self.protect.iter().map(|d| norm(d)).any(|d| within(&p, &d) || within(&d, &p))
    }

    pub fn in_prefer_folder(&self, path: &str) -> bool {
        within(&norm(path), &norm(&self.prefer_folder))
    }

    fn on_prefer_drive(&self, path: &str) -> bool {
        within(&norm(path), &norm(&self.prefer_drive))
    }

    /// Smart score: [`crate::app::keep_score`] plus the folder rules.
    /// Preferred folder > protected folder > preferred drive > path heuristics.
    pub fn score(&self, path: &str) -> (i32, &'static str) {
        let drive = if self.prefer_drive.is_empty() { String::new() } else { format!("{}\\", norm(&self.prefer_drive)) };
        let (mut s, mut why) = crate::app::keep_score(path, &drive);
        if self.is_protected(path) {
            s += 200;
            why = tr("carpeta protegida", "protected folder");
        }
        if self.in_prefer_folder(path) {
            s += 300;
            why = tr("carpeta preferida", "preferred folder");
        }
        (s, why)
    }
}

/// Which copy of each group to keep.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Keep {
    Smart,
    Oldest,
    Newest,
    ShortestPath,
    LongestPath,
}

impl Keep {
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s.trim().to_lowercase().replace(['-', ' '], "_").as_str() {
            "" | "smart" | "suggest" => Keep::Smart,
            "oldest" => Keep::Oldest,
            "newest" => Keep::Newest,
            "shortest" | "shortest_path" => Keep::ShortestPath,
            "longest" | "longest_path" => Keep::LongestPath,
            _ => return None,
        })
    }
    pub fn id(self) -> &'static str {
        match self {
            Keep::Smart => "smart",
            Keep::Oldest => "oldest",
            Keep::Newest => "newest",
            Keep::ShortestPath => "shortest_path",
            Keep::LongestPath => "longest_path",
        }
    }
}

/// Result of a marking rule.
#[derive(Default, Clone, Debug)]
pub struct Marks {
    pub marked: HashSet<String>,
    /// Kept copy → why it was kept.
    pub reasons: HashMap<String, String>,
    /// Copies left unmarked only because they are protected.
    pub protected: Vec<String>,
}

/// True if path or one of its parent folders is in marked.
pub fn under_marked(marked: &HashSet<String>, path: &str) -> bool {
    let mut p = path;
    loop {
        if marked.contains(p) {
            return true;
        }
        match p.rfind('\\') {
            Some(i) if i > 2 => p = &p[..i],
            _ => return false,
        }
    }
}

/// Mark every copy but one per group. Protected copies are never marked;
/// folders go first so a kept file never sits inside a marked folder.
pub fn plan_marks(groups: &[DupGroup], rules: &Rules, keep: Keep) -> Marks {
    let mut out = Marks::default();
    for folders in [true, false] {
        let mut add = Vec::new();
        for g in groups.iter().filter(|g| g.folder == folders) {
            let Some((k, why)) = pick(g, rules, keep, &out.marked) else { continue };
            out.reasons.insert(g.files[k].path.clone(), why);
            for (i, f) in g.files.iter().enumerate() {
                if i == k {
                    continue;
                }
                if rules.is_protected(&f.path) {
                    out.protected.push(f.path.clone());
                } else {
                    add.push(f.path.clone());
                }
            }
        }
        out.marked.extend(add);
    }
    out
}

/// The copy to keep in one group, and why.
fn pick(g: &DupGroup, rules: &Rules, keep: Keep, marked: &HashSet<String>) -> Option<(usize, String)> {
    // Prefer a copy that is not inside an already-marked folder.
    let ok: Vec<usize> = (0..g.files.len()).filter(|&i| !under_marked(marked, &g.files[i].path)).collect();
    let mut pool = if ok.is_empty() { (0..g.files.len()).collect() } else { ok };
    if keep == Keep::Smart {
        let scored: Vec<(i32, u64, usize, &'static str)> = pool
            .iter()
            .map(|&i| {
                let (s, w) = rules.score(&g.files[i].path);
                (s, g.files[i].modified, i, w)
            })
            .collect();
        let &(best, _, k, why) = scored.iter().max_by_key(|x| (x.0, x.1))?;
        // Explain with the winner's bonus, else with what was wrong with the others.
        let reason = if best > 0 && !why.is_empty() {
            why.to_string()
        } else {
            scored
                .iter()
                .filter(|x| x.2 != k && x.0 < best)
                .map(|x| x.3)
                .find(|w| !w.is_empty())
                .map_or_else(|| tr("la más nueva", "the newest").to_string(), String::from)
        };
        return Some((k, reason));
    }
    // Tiers: preferred folder, then protected copies, then the preferred drive.
    let mut tier = "";
    let tiers: [(&dyn Fn(&str) -> bool, &'static str); 3] = [
        (&|p| rules.in_prefer_folder(p), tr("carpeta preferida", "preferred folder")),
        (&|p| rules.is_protected(p), tr("carpeta protegida", "protected folder")),
        (&|p| rules.on_prefer_drive(p), tr("unidad preferida", "preferred drive")),
    ];
    for (test, name) in tiers {
        let sub: Vec<usize> = pool.iter().copied().filter(|&i| test(&g.files[i].path)).collect();
        if !sub.is_empty() {
            if sub.len() < pool.len() {
                tier = name;
            }
            pool = sub;
            break;
        }
    }
    let len = |i: &usize| g.files[*i].path.chars().count();
    let (k, rule) = match keep {
        Keep::Oldest => (pool.into_iter().min_by_key(|&i| g.files[i].modified)?, tr("la más antigua", "the oldest")),
        Keep::Newest | Keep::Smart => (pool.into_iter().max_by_key(|&i| g.files[i].modified)?, tr("la más nueva", "the newest")),
        Keep::ShortestPath => (pool.into_iter().min_by_key(len)?, tr("ruta más corta", "shortest path")),
        Keep::LongestPath => (pool.into_iter().max_by_key(len)?, tr("ruta más larga", "longest path")),
    };
    Some((k, if tier.is_empty() { rule.to_string() } else { tier.to_string() }))
}

/// Bytes freed by removing the marked copies.
pub fn marked_bytes(groups: &[DupGroup], marked: &HashSet<String>) -> u64 {
    groups.iter().flat_map(|g| g.files.iter().filter(|f| marked.contains(&f.path)).map(|f| f.size)).sum()
}

// ---------------------------------------------------------------- proposals

/// Something the assistant suggests. Proposals are data: the app shows them as
/// a card, and applying one only marks copies in the Duplicates tab.
#[derive(Clone)]
pub struct Proposal {
    /// "duplicate_marks" or "cleanup".
    pub kind: &'static str,
    pub rule: Keep,
    /// Duplicate groups the marks refer to (loaded into the tab on apply).
    pub groups: Vec<DupGroup>,
    pub marks: Marks,
    /// Informational cleanup candidates (path, size, reason); never marked.
    pub items: Vec<(String, u64, String)>,
}

impl Proposal {
    pub fn mark_bytes(&self) -> u64 {
        marked_bytes(&self.groups, &self.marks.marked)
    }
    pub fn item_bytes(&self) -> u64 {
        self.items.iter().map(|i| i.1).sum()
    }
}
