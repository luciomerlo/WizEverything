//! Backup verification: "is this folder backed up?".
//!
//! Compares a source folder against another copy (local drive, external or a
//! synced folder) by **relative path**: a file counts as backed up when the same
//! relative path exists in the target with the same size. Sizes are compared
//! instead of mtimes because backup tools often rewrite timestamps.

use std::collections::HashMap;

use serde_json::{json, Value};

use crate::tree::Tree;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RelEntry {
    pub rel: String,
    pub size: u64,
}

fn norm_rel(s: &str) -> String {
    s.replace('/', "\\")
        .trim_start_matches('\\')
        .to_lowercase()
}

#[derive(Clone, Debug, Default)]
pub struct Report {
    pub source_files: usize,
    pub source_bytes: u64,
    pub covered: usize,
    pub covered_bytes: u64,
    pub missing: Vec<RelEntry>,
    pub differs: Vec<(String, u64, u64)>,
    pub extra: Vec<RelEntry>,
    /// True when the comparison used a capped sample instead of every file.
    pub truncated: bool,
}

impl Report {
    pub fn missing_bytes(&self) -> u64 {
        self.missing.iter().map(|e| e.size).sum()
    }
    pub fn differs_bytes(&self) -> u64 {
        self.differs.iter().map(|(_, s, _)| *s).sum()
    }
    pub fn extra_bytes(&self) -> u64 {
        self.extra.iter().map(|e| e.size).sum()
    }
    pub fn coverage_pct(&self) -> f64 {
        if self.source_bytes == 0 {
            100.0
        } else {
            self.covered_bytes as f64 * 100.0 / self.source_bytes as f64
        }
    }

    pub fn to_json(&self, limit: usize) -> Value {
        let mut missing = self.missing.clone();
        missing.sort_by(|a, b| b.size.cmp(&a.size));
        let mut differs = self.differs.clone();
        differs.sort_by(|a, b| b.1.cmp(&a.1));
        let mut extra = self.extra.clone();
        extra.sort_by(|a, b| b.size.cmp(&a.size));
        json!({
            "kind": "backup",
            "source_files": self.source_files,
            "source_bytes": self.source_bytes,
            "covered": self.covered,
            "covered_bytes": self.covered_bytes,
            "missing": self.missing.len(),
            "missing_bytes": self.missing_bytes(),
            "differs": self.differs.len(),
            "differs_bytes": self.differs_bytes(),
            "extra": self.extra.len(),
            "extra_bytes": self.extra_bytes(),
            "coverage_pct": (self.coverage_pct() * 10.0).round() / 10.0,
            "truncated": self.truncated,
            "top_missing": missing.iter().take(limit).map(|e| json!({ "rel": e.rel, "bytes": e.size })).collect::<Vec<_>>(),
            "top_differs": differs.iter().take(limit).map(|(r, s, d)| json!({ "rel": r, "source": s, "target": d })).collect::<Vec<_>>(),
            "top_extra": extra.iter().take(limit).map(|e| json!({ "rel": e.rel, "bytes": e.size })).collect::<Vec<_>>(),
        })
    }
}

/// Pure comparison, easy to test: same relative path + same size = covered.
pub fn compare_sets(source: &[RelEntry], target: &[RelEntry]) -> Report {
    let dst: HashMap<&str, u64> = target.iter().map(|e| (e.rel.as_str(), e.size)).collect();
    let src_set: HashMap<&str, u64> = source.iter().map(|e| (e.rel.as_str(), e.size)).collect();
    let mut r = Report {
        source_files: source.len(),
        source_bytes: source.iter().map(|e| e.size).sum(),
        ..Default::default()
    };
    for s in source {
        match dst.get(s.rel.as_str()) {
            None => r.missing.push(s.clone()),
            Some(&sz) if sz == s.size => {
                r.covered += 1;
                r.covered_bytes += s.size;
            }
            Some(&sz) => r.differs.push((s.rel.clone(), s.size, sz)),
        }
    }
    for d in target {
        if !src_set.contains_key(d.rel.as_str()) {
            r.extra.push(d.clone());
        }
    }
    r
}

/// Collect relative paths under `root`, or a single file. Caps memory use.
fn collect(t: &Tree, root: u32, base: &str, cap: usize) -> (Vec<RelEntry>, bool) {
    let n = &t.nodes[root as usize];
    let one = |id: u32| {
        let p = t.full_path(id);
        RelEntry {
            rel: norm_rel(p.strip_prefix(base).unwrap_or(&p)),
            size: t.nodes[id as usize].size,
        }
    };
    if !n.is_dir() {
        return (vec![one(root)], false);
    }
    let mut out = Vec::new();
    let mut truncated = false;
    for id in t.files_under(root) {
        if out.len() >= cap {
            truncated = true;
            break;
        }
        out.push(one(id));
    }
    (out, truncated)
}

/// Compare two folders/files from the index. Returns an error when a root is
/// unknown, they are the same, or one of them is the synthetic all-drives root.
pub fn compare_trees(t: &Tree, source: u32, target: u32, max_entries: usize) -> Result<Report, String> {
    if source == target {
        return Err("source and target are the same path".into());
    }
    let sb = t.full_path(source);
    let tb = t.full_path(target);
    if sb.trim_end_matches('\\').is_empty() || tb.trim_end_matches('\\').is_empty() {
        return Err("pick concrete folders, not the all-drives root".into());
    }
    let (src, ts) = collect(t, source, &sb, max_entries);
    let (dst, td) = collect(t, target, &tb, max_entries);
    let mut r = compare_sets(&src, &dst);
    r.truncated = ts || td;
    Ok(r)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn e(rel: &str, size: u64) -> RelEntry {
        RelEntry { rel: norm_rel(rel), size }
    }

    #[test]
    fn counts_missing_differing_and_extra() {
        let src = vec![e("a.jpg", 10), e("sub\\b.jpg", 20), e("c.jpg", 30)];
        let dst = vec![e("a.jpg", 10), e("sub/b.jpg", 25), e("extra.jpg", 5)];
        let r = compare_sets(&src, &dst);
        assert_eq!(r.source_files, 3);
        assert_eq!(r.source_bytes, 60);
        assert_eq!(r.covered, 1);
        assert_eq!(r.covered_bytes, 10);
        assert_eq!(r.missing.len(), 1);
        assert_eq!(r.missing[0].rel, "c.jpg");
        assert_eq!(r.differs.len(), 1);
        assert_eq!(r.differs[0].1, 20);
        assert_eq!(r.differs[0].2, 25);
        assert_eq!(r.extra.len(), 1);
        assert!((r.coverage_pct() - 16.666).abs() < 0.1);
    }

    #[test]
    fn identical_trees_are_fully_covered() {
        let src = vec![e("x\\1.bin", 1), e("x\\2.bin", 2)];
        let r = compare_sets(&src, &src);
        assert_eq!(r.missing.len(), 0);
        assert_eq!(r.differs.len(), 0);
        assert_eq!(r.covered, 2);
        assert_eq!(r.coverage_pct(), 100.0);
    }

    #[test]
    fn path_normalisation_is_case_and_separator_insensitive() {
        assert_eq!(norm_rel("\\Fotos\\A.JPG"), "fotos\\a.jpg");
        assert_eq!(norm_rel("fotos/a.jpg"), "fotos\\a.jpg");
    }

    #[test]
    fn empty_source_is_full_coverage() {
        let r = compare_sets(&[], &[e("a", 1)]);
        assert_eq!(r.coverage_pct(), 100.0);
        assert_eq!(r.extra.len(), 1);
    }
}
