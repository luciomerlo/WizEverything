//! Filter the whole view with an Everything search (`ext:mp4 size:>1gb dm:lastyear`).
//!
//! The query runs through Everything's IPC in a background thread; every file
//! it returns is mapped to its node in the loaded tree. Folders then show only
//! the bytes of matching files inside them, like the extension highlight does.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::mpsc::{Receiver, channel};

use crate::everything;
use crate::tree::{ROOT, Tree, path_parts};
use crate::treemap::Highlight;

/// Files of a search mapped to tree nodes.
pub struct SearchResult {
    pub query: String,
    /// Tree epoch the node ids belong to.
    pub epoch: u64,
    /// Matching file nodes.
    pub files: Vec<u32>,
    /// Per node: matching bytes (a file's own size, the sum for folders).
    pub bytes: Arc<Highlight>,
    /// Files reported by Everything that aren't in the loaded tree (new since the load).
    pub unmapped: u64,
    /// Folders matched by the query (ignored: the filter works on files).
    pub folders: u64,
}

impl SearchResult {
    pub fn count(&self) -> u64 {
        self.files.len() as u64
    }

    pub fn bytes_of(&self, id: u32) -> u64 {
        self.bytes.bytes.get(id as usize).copied().unwrap_or(0)
    }

    /// Matching bytes in the whole tree ("This PC" may leave excluded drives out).
    pub fn total(&self, tree: &Tree) -> u64 {
        self.files.iter().map(|&f| tree.nodes[f as usize].size).sum()
    }

    /// Recompute the per-node bytes after sizes changed (sync / real size).
    pub fn refresh(&mut self, tree: &Tree) {
        self.bytes = Arc::new(Highlight { bytes: bytes_for(tree, &self.files, None) });
    }
}

/// Maps Everything results (parent folder + name) to node ids, caching folders.
pub struct Mapper<'a> {
    tree: &'a Tree,
    dirs: HashMap<String, Option<u32>>,
    /// Lower-case name → child, built for folders with many children.
    index: HashMap<u32, HashMap<String, u32>>,
}

impl<'a> Mapper<'a> {
    pub fn new(tree: &'a Tree) -> Self {
        Self { tree, dirs: HashMap::new(), index: HashMap::new() }
    }

    fn dir(&mut self, dir: &str) -> Option<u32> {
        if let Some(&d) = self.dirs.get(dir) {
            return d;
        }
        let id = if path_parts(dir).is_empty() { Some(ROOT) } else { self.tree.find_path(dir) };
        let id = id.filter(|&i| self.tree.nodes[i as usize].is_dir());
        self.dirs.insert(dir.to_string(), id);
        id
    }

    /// Node of the file `name` inside folder `dir`.
    pub fn file(&mut self, dir: &str, name: &str) -> Option<u32> {
        let parent = self.dir(dir)?;
        let tree = self.tree;
        let kids = tree.children(parent);
        if kids.len() <= 32 {
            return kids.iter().copied().find(|&c| !tree.nodes[c as usize].is_dir() && tree.name(c).eq_ignore_ascii_case(name));
        }
        let idx = self.index.entry(parent).or_insert_with(|| {
            kids.iter()
                .copied()
                .filter(|&c| !tree.nodes[c as usize].is_dir())
                .map(|c| (tree.name(c).to_lowercase(), c))
                .collect()
        });
        idx.get(&name.to_lowercase()).copied()
    }
}

/// Map (folder, file name) pairs to file nodes. Returns (nodes, not found).
pub fn map_items<'s>(tree: &Tree, items: impl IntoIterator<Item = (&'s str, &'s str)>) -> (Vec<u32>, u64) {
    let mut m = Mapper::new(tree);
    let mut out = Vec::new();
    let mut missing = 0;
    for (dir, name) in items {
        match m.file(dir, name) {
            Some(id) => out.push(id),
            None => missing += 1,
        }
    }
    out.sort_unstable();
    out.dedup();
    (out, missing)
}

/// Bytes of the given files inside every node (optionally only files whose
/// extension passes `pick`). One reverse pass: parents precede children.
pub fn bytes_for(tree: &Tree, files: &[u32], pick: Option<&dyn Fn(u32) -> bool>) -> Vec<u64> {
    let mut v = vec![0u64; tree.nodes.len()];
    for &f in files {
        let n = &tree.nodes[f as usize];
        if pick.is_none_or(|p| p(n.ext)) {
            v[f as usize] = n.size;
        }
    }
    for id in (1..tree.nodes.len()).rev() {
        let p = tree.nodes[id].parent as usize;
        if v[id] > 0 && p < v.len() {
            v[p] += v[id];
        }
    }
    v
}

/// Matching files inside `root` (pruned by the per-node bytes), optionally name-filtered.
fn files_under(tree: &Tree, res: &SearchResult, root: u32, mut f: impl FnMut(u32)) {
    let mut stack = vec![root];
    while let Some(id) = stack.pop() {
        let n = &tree.nodes[id as usize];
        if n.is_dir() {
            stack.extend(tree.children(id).iter().copied().filter(|&c| res.bytes_of(c) > 0 || is_empty_match(tree, res, c)));
        } else if res.files.binary_search(&id).is_ok() {
            f(id);
        }
    }
}

/// Empty files match too (they add no bytes, so the byte pruning can't see them).
fn is_empty_match(tree: &Tree, res: &SearchResult, id: u32) -> bool {
    let n = &tree.nodes[id as usize];
    !n.is_dir() && n.size == 0 && res.files.binary_search(&id).is_ok()
}

/// The `limit` largest matching files of a subtree (optionally name-filtered).
pub fn largest_under(tree: &Tree, res: &SearchResult, root: u32, filter: &str, limit: usize) -> Vec<u32> {
    let filter = filter.to_lowercase();
    let mut v = Vec::new();
    files_under(tree, res, root, |id| {
        if filter.is_empty() || tree.name(id).to_lowercase().contains(&filter) {
            v.push(id);
        }
    });
    v.sort_unstable_by(|a, b| tree.nodes[*b as usize].size.cmp(&tree.nodes[*a as usize].size));
    v.truncate(limit);
    v
}

/// Per-extension totals (ext, size, count) of the matching files of a subtree.
pub fn ext_stats(tree: &Tree, res: &SearchResult, root: u32) -> Vec<(u32, u64, u64)> {
    let mut acc: HashMap<u32, (u64, u64)> = HashMap::new();
    files_under(tree, res, root, |id| {
        let n = &tree.nodes[id as usize];
        let e = acc.entry(n.ext).or_default();
        e.0 += n.size;
        e.1 += 1;
    });
    acc.into_iter().map(|(k, (s, c))| (k, s, c)).collect()
}

/// Run `query` against Everything and map the results to `tree`.
pub fn run(tree: &Tree, query: &str, epoch: u64) -> Result<SearchResult, String> {
    let mut m = Mapper::new(tree);
    let (mut files, mut unmapped, mut folders) = (Vec::new(), 0u64, 0u64);
    let (mut dir, mut name) = (String::new(), String::new());
    everything::for_each_item(
        query,
        |it| {
            if it.is_dir {
                folders += 1;
                return;
            }
            dir.clear();
            dir.extend(char::decode_utf16(it.path.iter().copied()).map(|c| c.unwrap_or('\u{FFFD}')));
            name.clear();
            name.extend(char::decode_utf16(it.name.iter().copied()).map(|c| c.unwrap_or('\u{FFFD}')));
            match m.file(&dir, &name) {
                Some(id) => files.push(id),
                None => unmapped += 1,
            }
        },
        |_, _| {},
    )?;
    files.sort_unstable();
    files.dedup();
    let bytes = Arc::new(Highlight { bytes: bytes_for(tree, &files, None) });
    Ok(SearchResult { query: query.to_string(), epoch, files, bytes, unmapped, folders })
}

/// UI state of the search filter.
#[derive(Default)]
pub struct Search {
    pub text: String,
    pub result: Option<SearchResult>,
    pub error: Option<String>,
    /// Bumped whenever the active result changes (views rebuild on it).
    pub generation: u64,
    job: Option<(u64, Receiver<Result<SearchResult, String>>)>,
    /// Tree revision the result's bytes were computed for.
    pub revision: u64,
}

impl Search {
    pub fn busy(&self) -> bool {
        self.job.is_some()
    }

    /// The result, if it belongs to the loaded tree.
    pub fn active(&self, epoch: u64) -> Option<&SearchResult> {
        self.result.as_ref().filter(|r| r.epoch == epoch)
    }

    pub fn start(&mut self, ctx: &eframe::egui::Context, tree: Arc<Tree>, epoch: u64, revision: u64, query: String) {
        let (tx, rx) = channel();
        self.generation += 1;
        let ctx = ctx.clone();
        std::thread::Builder::new()
            .name("everything-search".into())
            .spawn(move || {
                let r = run(&tree, &query, epoch);
                drop(tree);
                let _ = tx.send(r);
                ctx.request_repaint();
            })
            .expect("thread");
        self.job = Some((revision, rx));
        self.error = None;
    }

    pub fn clear(&mut self) {
        self.job = None;
        self.error = None;
        if self.result.take().is_some() {
            self.generation += 1;
        }
    }

    /// Collect a finished search. Returns true if the active result changed.
    pub fn poll(&mut self) -> bool {
        let Some((rev, rx)) = &self.job else { return false };
        let Ok(r) = rx.try_recv() else { return false };
        let rev = *rev;
        self.job = None;
        self.generation += 1;
        match r {
            Ok(res) => {
                self.result = Some(res);
                self.revision = rev;
            }
            Err(e) => {
                self.result = None;
                self.error = Some(e);
            }
        }
        true
    }
}
