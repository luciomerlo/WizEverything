//! Unit and integration tests.
//!
//! `cargo test`                → unit tests (no Everything needed)
//! `cargo test -- --ignored`   → also the live tests against a running Everything

use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::time::Instant;

use crate::app::{self, Col};
use crate::dup::{self, DupOptions, DupProgress};
use crate::i18n::{self, Lang};
use crate::tree::{Builder, Change, ROOT, Tree};
use crate::{sunburst, treemap};

// Drive names that can't exist, so cluster size falls back to 4096.
const D1: &str = "Q9:";
const D2: &str = "R9:";

/// Q9:\ big.iso (1 000 000) · docs\a.pdf (5000) · docs\b.PDF (100) · docs\sub\c.txt (10) · noext (1)
/// R9:\ u.bin (300)
fn sample() -> Tree {
    let mut b = Builder::new();
    b.add(true, "", D1, None, 1);
    b.add(true, D1, "docs", None, 2);
    b.add(false, "Q9:\\docs", "a.pdf", Some(5000), 3);
    b.add(false, "Q9:\\docs", "b.PDF", Some(100), 4);
    b.add(false, "Q9:\\docs\\sub", "c.txt", Some(10), 5); // parent created implicitly
    b.add(false, D1, "big.iso", Some(1_000_000), 6);
    b.add(false, D1, "noext", Some(1), 7);
    b.add(true, "", D2, None, 1);
    b.add(false, D2, "u.bin", Some(300), 1);
    b.finish(&[], Instant::now(), "2026-01-01T00:00:00".into())
}

fn id(t: &Tree, path: &str) -> u32 {
    t.find_path(path).unwrap_or_else(|| panic!("missing {path}"))
}

fn names(t: &Tree, ids: &[u32]) -> Vec<String> {
    ids.iter().map(|&i| t.name(i).to_string()).collect()
}

// ------------------------------------------------------------------ tree

#[test]
fn tree_aggregates_sizes_and_counts() {
    let t = sample();
    let q = &t.nodes[id(&t, D1) as usize];
    assert_eq!(q.size, 1_005_111);
    assert_eq!((q.files, q.folders), (5, 2));
    let docs = &t.nodes[id(&t, "Q9:\\docs") as usize];
    assert_eq!(docs.size, 5110);
    assert_eq!((docs.files, docs.folders), (3, 1));
    let root = &t.nodes[ROOT as usize];
    assert_eq!(root.size, 1_005_411);
    assert_eq!((root.files, root.folders), (6, 4));
}

#[test]
fn tree_parents_precede_children() {
    let t = sample();
    for (i, n) in t.nodes.iter().enumerate().skip(1) {
        assert!((n.parent as usize) < i, "node {i} has parent {}", n.parent);
    }
}

#[test]
fn tree_children_sorted_by_size_desc() {
    let t = sample();
    assert_eq!(names(&t, t.children(id(&t, D1))), ["big.iso", "docs", "noext"]);
    assert_eq!(names(&t, t.children(ROOT)), [D1, D2]);
}

#[test]
fn tree_paths_roundtrip() {
    let t = sample();
    let c = id(&t, "Q9:\\docs\\sub\\c.txt");
    assert_eq!(t.full_path(c), "Q9:\\docs\\sub\\c.txt");
    assert_eq!(t.full_path(id(&t, D1)), "Q9:\\");
    assert_eq!(t.find_path("q9:\\DOCS\\SUB\\C.TXT"), Some(c), "lookup is case-insensitive");
    assert_eq!(t.find_path("Q9:\\nope"), None);
    assert_eq!(t.ancestors(c).len(), 5); // root, Q9:, docs, sub, c.txt
    assert_eq!(t.drive_of(c), Some(id(&t, D1)));
}

#[test]
fn tree_allocation_rounds_to_cluster() {
    let t = sample();
    assert_eq!(t.nodes[id(&t, "Q9:\\docs\\a.pdf") as usize].alloc, 8192);
    assert_eq!(t.nodes[id(&t, "Q9:\\noext") as usize].alloc, 4096);
    assert_eq!(t.nodes[id(&t, "Q9:\\docs") as usize].alloc, 8192 + 4096 + 4096);
}

#[test]
fn tree_extensions_are_case_insensitive() {
    let t = sample();
    let stats = t.ext_stats(ROOT);
    let pdf = stats.iter().find(|(e, _, _)| t.exts[*e as usize].name == "pdf").expect("pdf");
    assert_eq!((pdf.1, pdf.2), (5100, 2));
    assert!(stats.iter().any(|(e, _, _)| t.exts[*e as usize].name.is_empty()), "file without extension");
    let total: u64 = stats.iter().map(|s| s.1).sum();
    assert_eq!(total, t.nodes[ROOT as usize].size);
}

#[test]
fn tree_largest_files_and_filter() {
    let t = sample();
    assert_eq!(names(&t, &t.largest_files(ROOT, "", 2)), ["big.iso", "a.pdf"]);
    assert_eq!(names(&t, &t.largest_files(ROOT, "PDF", 10)), ["a.pdf", "b.PDF"]);
    assert_eq!(t.files_under(id(&t, "Q9:\\docs")).len(), 3);
}

#[test]
fn tree_dominant_color_is_largest_file() {
    let t = sample();
    let a = &t.nodes[id(&t, "Q9:\\docs\\a.pdf") as usize];
    assert_eq!(t.dominant_color(id(&t, "Q9:\\docs")), t.exts[a.ext as usize].color);
}

#[test]
fn tree_excluded_drive_leaves_all_view() {
    let mut t = sample();
    t.set_excluded(&["r9:".to_string()]);
    assert!(t.is_excluded(id(&t, D2)));
    assert_eq!(names(&t, t.children(ROOT)), [D1]);
    assert_eq!(t.nodes[ROOT as usize].size, 1_005_111);
    assert_eq!(t.drives.len(), 2, "the drive list still has every drive");
    assert_eq!(t.find_path(D2), Some(t.drives[1]), "excluded drive is still addressable by path");
    t.set_excluded(&[]);
    assert_eq!(t.nodes[ROOT as usize].size, 1_005_411);
}

#[test]
fn tree_apply_changes_updates_and_inserts() {
    let mut t = sample();
    let changes = vec![
        Change { dir: "Q9:\\docs".into(), name: "A.PDF".into(), size: 6000, modified: 9 }, // update (case-insensitive)
        Change { dir: "Q9:\\new\\deep".into(), name: "n.txt".into(), size: 50, modified: 9 }, // new folders + file
    ];
    assert_eq!(t.apply_changes(&changes), 2);
    assert_eq!(t.nodes[id(&t, "Q9:\\docs") as usize].size, 6110);
    let q = &t.nodes[id(&t, D1) as usize];
    assert_eq!(q.size, 1_005_111 + 1000 + 50);
    assert_eq!((q.files, q.folders), (6, 4));
    assert_eq!(t.nodes[ROOT as usize].size, 1_005_411 + 1050);
    assert_eq!(t.full_path(id(&t, "Q9:\\new\\deep\\n.txt")), "Q9:\\new\\deep\\n.txt");
    // children stay sorted by size after the update
    let kids = t.children(id(&t, "Q9:\\docs"));
    assert!(kids.windows(2).all(|w| t.nodes[w[0] as usize].size >= t.nodes[w[1] as usize].size));
    // applying the same change again is a no-op
    assert_eq!(t.apply_changes(&changes[..1]), 0);
}

#[test]
fn tree_changes_in_excluded_drive_dont_reach_all() {
    let mut t = sample();
    t.set_excluded(&[D2.to_string()]);
    let before = t.nodes[ROOT as usize].size;
    t.apply_changes(&[Change { dir: D2.into(), name: "x".into(), size: 999, modified: 1 }]);
    assert_eq!(t.nodes[ROOT as usize].size, before);
    assert_eq!(t.nodes[t.drives[1] as usize].size, 1299);
    assert_eq!(t.drives.len(), 2, "no duplicate drive was created");
}

#[test]
fn tree_real_alloc_propagates() {
    let mut t = sample();
    let a = id(&t, "Q9:\\docs\\a.pdf");
    let docs_before = t.nodes[id(&t, "Q9:\\docs") as usize].alloc;
    t.set_real_alloc(&[(a, 0)]);
    assert_eq!(t.nodes[a as usize].alloc, 0);
    assert_eq!(t.nodes[id(&t, "Q9:\\docs") as usize].alloc, docs_before - 8192);
    assert_eq!(t.nodes[ROOT as usize].alloc, t.drives.iter().map(|&d| t.nodes[d as usize].alloc).sum::<u64>());
}

#[test]
fn tree_csv_export() {
    let t = sample();
    let path = std::env::temp_dir().join(format!("wiz_test_{}.csv", std::process::id()));
    let done = AtomicU64::new(0);
    let rows = t.write_csv(id(&t, D1), &path, &done).unwrap();
    let text = std::fs::read_to_string(&path).unwrap();
    std::fs::remove_file(&path).ok();
    assert_eq!(rows, 8); // Q9:, docs, sub, 5 files
    assert!(text.starts_with('\u{feff}'));
    assert!(text.contains("\"Q9:\\docs\\sub\\c.txt\",File,10,4096,"));
    assert!(text.contains("\"Q9:\\docs\",Folder,5110,"));
    assert_eq!(text.lines().count(), 9);
}

#[test]
fn tree_memory_is_compact() {
    assert!(std::mem::size_of::<crate::tree::Node>() <= 64, "node grew to {}", std::mem::size_of::<crate::tree::Node>());
}

#[test]
fn tree_sync_reserves_child_slots() {
    let mut t = sample();
    let before = t.memory();
    let changes: Vec<Change> =
        (0..2000).map(|i| Change { dir: "Q9:\\docs".into(), name: format!("n{i}.txt"), size: 1, modified: 1 }).collect();
    assert_eq!(t.apply_changes(&changes), 2000);
    assert_eq!(t.children(id(&t, "Q9:\\docs")).len(), 2003);
    // Geometric growth: far below the ~2000²/2 slots a copy-per-insert would need.
    assert!(t.memory() - before < 2_000_000, "grew {} bytes", t.memory() - before);
    assert!(t.find_path("Q9:\\docs\\n1999.txt").is_some());
}

#[test]
fn unc_paths_roundtrip() {
    assert_eq!(crate::tree::path_parts("\\\\nas\\share\\a\\b.txt"), ["\\\\nas", "share", "a", "b.txt"]);
    assert_eq!(crate::tree::path_parts("C:\\x"), ["C:", "x"]);
    let mut b = Builder::new();
    b.add(false, "\\\\nas\\share\\media", "song.mp3", Some(5), 1);
    let mut t = b.finish(&[], Instant::now(), String::new());
    let song = t.find_path("\\\\nas\\share\\media\\song.mp3").expect("UNC lookup");
    assert_eq!(t.full_path(song), "\\\\nas\\share\\media\\song.mp3");
    t.apply_changes(&[Change { dir: "\\\\nas\\share\\media".into(), name: "x.mp3".into(), size: 7, modified: 1 }]);
    assert_eq!(t.drives.len(), 1, "sync into a share doesn't create a phantom drive");
    assert_eq!(t.nodes[ROOT as usize].size, 12);
}

// ------------------------------------------------------------------ rows / sorting

#[test]
fn rows_sort_applies_to_this_pc_level() {
    let t = sample();
    let mut exp = HashSet::new();
    exp.insert(ROOT);
    let rows = app::flatten_rows(&t, ROOT, &exp, None);
    assert_eq!(names(&t, &rows.iter().map(|r| r.0).collect::<Vec<_>>())[1..], [D1, D2]);
    let rows = app::flatten_rows(&t, ROOT, &exp, Some((Col::Name, false)));
    assert_eq!(names(&t, &rows.iter().map(|r| r.0).collect::<Vec<_>>())[1..], [D2, D1], "Folder ▼ sorts the drives");
    let rows = app::flatten_rows(&t, ROOT, &exp, Some((Col::Size, true)));
    assert_eq!(names(&t, &rows.iter().map(|r| r.0).collect::<Vec<_>>())[1..], [D2, D1]);
}

#[test]
fn rows_follow_expansion_and_depth() {
    let t = sample();
    let q = id(&t, D1);
    let docs = id(&t, "Q9:\\docs");
    let exp: HashSet<u32> = [q, docs].into_iter().collect();
    let rows = app::flatten_rows(&t, q, &exp, Some((Col::Name, true)));
    let got: Vec<(String, u16)> = rows.iter().map(|&(i, d)| (t.name(i).to_string(), d)).collect();
    let want = [("Q9:", 0), ("big.iso", 1), ("docs", 1), ("a.pdf", 2), ("b.PDF", 2), ("sub", 2), ("noext", 1)];
    // (sorted by name: mixed, as in Explorer)
    assert_eq!(got, want.map(|(n, d)| (n.to_string(), d)));
}

// ------------------------------------------------------------------ treemap / rings

#[test]
fn squarify_fills_rect_exactly() {
    let items: Vec<(u32, f64)> = [50.0, 25.0, 12.0, 8.0, 3.0, 1.0, 1.0].iter().enumerate().map(|(i, &s)| (i as u32, s)).collect();
    let rect = treemap::R { x: 10.0, y: 20.0, w: 300.0, h: 120.0 };
    let mut out = Vec::new();
    treemap::squarify(&items, rect, 100.0, &mut out);
    assert_eq!(out.len(), items.len());
    let area: f32 = out.iter().map(|(_, r)| r.w * r.h).sum();
    assert!((area - rect.w * rect.h).abs() < 1.0, "area {area}");
    for (i, r) in &out {
        assert!(r.x >= rect.x - 0.01 && r.y >= rect.y - 0.01, "{i} starts outside");
        assert!(r.x + r.w <= rect.x + rect.w + 0.01 && r.y + r.h <= rect.y + rect.h + 0.01, "{i} ends outside");
        let expect = items[*i as usize].1 as f32 / 100.0 * rect.w * rect.h;
        assert!((r.w * r.h - expect).abs() < 1.0, "{i} area");
    }
}

#[test]
fn treemap_covers_every_pixel() {
    let t = sample();
    let l = treemap::build(&t, ROOT, 240, 120, None, [255, 255, 255]);
    let covered = l.owner.iter().filter(|&&o| o != u32::MAX).count();
    assert!(covered as f64 / l.owner.len() as f64 > 0.99, "covered {covered}");
    // the biggest file owns about its share of the pixels
    let big = id(&t, "Q9:\\big.iso");
    let share = l.owner.iter().filter(|&&o| o == big).count() as f64 / l.owner.len() as f64;
    assert!((share - 1_000_000.0 / 1_005_411.0).abs() < 0.02, "share {share}");
}

#[test]
fn rings_center_is_root_and_arcs_are_valid() {
    let t = sample();
    let p = sunburst::Params { highlight: None, bg: [0, 0, 0] };
    let root = id(&t, D1);
    let l = sunburst::build(&t, root, 300, 200, &p);
    assert_eq!(l.owner[100 * 300 + 150], root, "center pixel is the view root");
    assert_eq!(l.owner[0], u32::MAX, "corner is background");
    let lvl0: f32 = l.arcs.values().filter(|a| a[2] == 0.0).map(|a| a[1] - a[0]).sum();
    assert!(lvl0 <= std::f32::consts::TAU + 1e-3);
    assert!(l.arcs.contains_key(&id(&t, "Q9:\\big.iso")));
    assert!(l.arcs.values().all(|a| a[0] >= 0.0 && a[1] <= std::f32::consts::TAU + 1e-3 && a[0] < a[1]));
    let outline = sunburst::arc_outline(l.geom.unwrap(), l.arcs[&id(&t, "Q9:\\docs")]);
    assert!(outline.len() >= 6);
}

// ------------------------------------------------------------------ duplicates

struct TempDir(std::path::PathBuf);
impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Builds a tree over a real temporary folder:
///   d1/same.bin = A · d2/same.bin = A · d3/same.bin = A' (differs after 64 KB)
///   d4/renamed.bin = A · d5/same.bin = hard link to d1/same.bin · small/x.txt
fn dup_fixture() -> (TempDir, Tree, u32) {
    init_cache();
    static NEXT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let base = std::env::temp_dir().join(format!("wiz_dup_test_{}_{n}", std::process::id()));
    let a: Vec<u8> = (0..200_000u32).map(|i| (i.wrapping_mul(2654435761) >> 13) as u8).collect();
    let mut a2 = a.clone();
    *a2.last_mut().unwrap() ^= 0xFF;
    for d in ["d1", "d2", "d3", "d4", "d5", "small"] {
        std::fs::create_dir_all(base.join(d)).unwrap();
    }
    std::fs::write(base.join("d1/same.bin"), &a).unwrap();
    std::fs::write(base.join("d2/same.bin"), &a).unwrap();
    std::fs::write(base.join("d3/same.bin"), &a2).unwrap();
    std::fs::write(base.join("d4/renamed.bin"), &a).unwrap();
    std::fs::hard_link(base.join("d1/same.bin"), base.join("d5/same.bin")).unwrap();
    std::fs::write(base.join("small/x.txt"), b"tiny").unwrap();

    let mut b = Builder::new();
    for entry in walk(&base) {
        let meta = std::fs::metadata(&entry).unwrap();
        let parent = entry.parent().unwrap().to_string_lossy().to_string();
        let name = entry.file_name().unwrap().to_string_lossy().to_string();
        b.add(meta.is_dir(), &parent, &name, (!meta.is_dir()).then_some(meta.len()), 1);
    }
    let tree = b.finish(&[], Instant::now(), String::new());
    let root = tree.find_path(&base.to_string_lossy()).expect("fixture root in tree");
    (TempDir(base), tree, root)
}

fn walk(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    for e in std::fs::read_dir(dir).unwrap() {
        let p = e.unwrap().path();
        out.push(p.clone());
        if p.is_dir() {
            out.extend(walk(&p));
        }
    }
    out
}

fn short(path: &str) -> String {
    let p = path.replace('\\', "/");
    let parts: Vec<&str> = p.rsplitn(3, '/').collect();
    format!("{}/{}", parts[1], parts[0])
}

#[test]
fn dups_same_name_confirms_by_full_hash_and_skips_hardlinks() {
    let (_tmp, tree, root) = dup_fixture();
    let opt = DupOptions { root, min_size: 1, same_name: true, verify: true, exclude: Default::default(), versions: false };
    let all = dup::find(Arc::new(tree), opt.clone(), Arc::new(DupProgress::default()));
    let groups: Vec<_> = all.iter().filter(|g| !g.folder).collect();
    assert_eq!(groups.len(), 1, "one file group");
    let mut files: Vec<String> = groups[0].files.iter().map(|f| short(&f.path)).collect();
    files.sort();
    // d3 differs only after the first 64 KB → rejected by the full hash.
    // d5 is a hard link of d1 → collapsed into one entry.
    assert_eq!(files.len(), 2, "{files:?}");
    assert!(files.contains(&"d2/same.bin".to_string()));
    assert!(files.iter().any(|f| f == "d1/same.bin" || f == "d5/same.bin"));
    assert_eq!(groups[0].size, 200_000);
    assert_eq!(groups[0].wasted(), 200_000);
}

#[test]
fn dups_size_only_finds_renamed_copies() {
    let (_tmp, tree, root) = dup_fixture();
    let opt = DupOptions { root, min_size: 1, same_name: false, verify: true, exclude: Default::default(), versions: false };
    let all = dup::find(Arc::new(tree), opt.clone(), Arc::new(DupProgress::default()));
    let groups: Vec<_> = all.iter().filter(|g| !g.folder).collect();
    assert_eq!(groups.len(), 1);
    let files: Vec<String> = groups[0].files.iter().map(|f| short(&f.path)).collect();
    assert_eq!(files.len(), 3, "{files:?}");
    assert!(files.contains(&"d4/renamed.bin".to_string()));
}

#[test]
fn dups_min_size_and_cancel() {
    let (_tmp, tree, root) = dup_fixture();
    let tree = Arc::new(tree);
    let big = DupOptions { root, min_size: 300_000, same_name: false, verify: true, exclude: Default::default(), versions: false };
    assert!(dup::find(tree.clone(), big, Arc::new(DupProgress::default())).is_empty());
    let p = Arc::new(DupProgress::default());
    p.cancel.store(true, std::sync::atomic::Ordering::Relaxed);
    let opt = DupOptions { root, min_size: 1, same_name: false, verify: true, exclude: Default::default(), versions: false };
    assert!(dup::find(tree, opt.clone(), p).is_empty(), "cancelled search returns nothing");
}

#[test]
fn dups_csv_export() {
    let (_tmp, tree, root) = dup_fixture();
    let groups = dup::find(Arc::new(tree), DupOptions { root, min_size: 1, same_name: true, verify: true, exclude: Default::default(), versions: false }, Arc::new(DupProgress::default()));
    let path = std::env::temp_dir().join(format!("wiz_dups_{}.csv", std::process::id()));
    dup::write_csv(&groups, &path).unwrap();
    let text = std::fs::read_to_string(&path).unwrap();
    std::fs::remove_file(&path).ok();
    // header + 2 file paths + 2 identical folders (d2 and d1/d5, which are hard links)
    assert_eq!(text.lines().count(), 5, "{text}");
    assert!(text.contains(",File,200000,"));
    assert!(text.contains(",Folder,200000,"));
}

#[test]
fn dups_finds_identical_folders_and_partitions_mismatches() {
    let (_tmp, tree, root) = dup_fixture();
    let all = dup::find(Arc::new(tree), DupOptions { root, min_size: 1, same_name: true, verify: true, exclude: Default::default(), versions: false }, Arc::new(DupProgress::default()));
    let folders: Vec<_> = all.iter().filter(|g| g.folder).collect();
    // d1, d2, d3, d5 share the same signature (same.bin, 200000 bytes) but d3's
    // content differs: it must be split off, not sink the whole group.
    assert_eq!(folders.len(), 1, "one folder group");
    let mut names: Vec<String> = folders[0].files.iter().map(|f| f.path.rsplit('\\').next().unwrap().to_string()).collect();
    names.sort();
    // d5 is a hard link of d1 (same physical file): only one of them counts.
    assert_eq!(names.len(), 2, "{names:?}");
    assert!(names.contains(&"d2".to_string()));
    assert!(names.iter().any(|n| n == "d1" || n == "d5"));
    assert_eq!(folders[0].inner_files, 1);
    assert_eq!(folders[0].wasted(), 200_000);
}

#[test]
fn dups_stream_groups_as_found() {
    let (_tmp, tree, root) = dup_fixture();
    let seen = std::sync::Mutex::new(0usize);
    let p = DupProgress::default();
    dup::find_streaming(Arc::new(tree), DupOptions { root, min_size: 1, same_name: false, verify: true, exclude: Default::default(), versions: false }, &p, &|_| {
        *seen.lock().unwrap() += 1;
    });
    assert!(*seen.lock().unwrap() >= 2, "file + folder groups were streamed");
    let (done, total) = (p.groups_done.load(std::sync::atomic::Ordering::Relaxed), p.groups_total.load(std::sync::atomic::Ordering::Relaxed));
    assert_eq!(done, total);
    assert!(p.bytes_done.load(std::sync::atomic::Ordering::Relaxed) >= p.bytes_total.load(std::sync::atomic::Ordering::Relaxed) / 2);
}

#[test]
fn drive_labels() {
    let t = sample();
    assert_eq!(t.drive_label(t.drives[0]), D1, "unknown volume: just the letter");
    let c = crate::tree::volume_label("C:");
    assert!(!c.is_empty(), "C: has a label or 'Local Disk'");
}

#[test]
fn parallel_bands_cover_all_rows() {
    for (w, h) in [(1usize, 1usize), (7, 3), (64, 33), (300, 1)] {
        let mut px = vec![[0u8; 4]; w * h];
        let mut ow = vec![0u32; w * h];
        treemap::par_bands(w, &mut px, &mut ow, |y0, p, o| {
            for (i, (a, b)) in p.iter_mut().zip(o.iter_mut()).enumerate() {
                *a = [1, 1, 1, 1];
                *b = (y0 * w + i) as u32;
            }
        });
        assert!(px.iter().all(|p| p[0] == 1), "{w}x{h}");
        assert!(ow.iter().enumerate().all(|(i, &o)| o as usize == i), "{w}x{h} row offsets");
    }
}

// ------------------------------------------------------------------ formatting / i18n

#[test]
fn formatting_follows_language() {
    i18n::set_lang(Lang::Es);
    assert_eq!(app::fmt_size(1536), "1,5 KB");
    assert_eq!(app::fmt_size(512), "512 B");
    assert_eq!(app::fmt_int(1_234_567), "1.234.567");
    assert_eq!(i18n::tr("hola", "hello"), "hola");
    i18n::set_lang(Lang::En);
    assert_eq!(app::fmt_size(1536), "1.5 KB");
    assert_eq!(app::fmt_size(3 * 1024u64.pow(4)), "3.0 TB");
    assert_eq!(app::fmt_int(1_234_567), "1,234,567");
    assert_eq!(app::fmt_int(12), "12");
    assert_eq!(i18n::tr("hola", "hello"), "hello");
    // 2024-01-02 03:04:05 UTC as FILETIME → local time, so only check the shape
    let ft = (1_704_164_645u64 + 11_644_473_600) * 10_000_000;
    let iso = app::fmt_iso(ft);
    assert_eq!(iso.len(), 19, "{iso}");
    assert!(iso.starts_with("2024-01-0"));
    assert_eq!(app::fmt_iso(0), "");
}

#[test]
fn icon_is_tilted_right() {
    let px = crate::icon_rgba(64);
    assert_eq!(px.len(), 64 * 64 * 4);
    let alpha = |x: usize, y: usize| px[(y * 64 + x) * 4 + 3];
    assert!(alpha(6, 58) > 0, "handle at the bottom-left");
    assert_eq!(alpha(60, 60), 0, "bottom-right is empty");
    assert!(alpha(52, 26) > 0, "lens on the upper-right");
}

// ------------------------------------------------------------------ live (needs Everything)

#[test]
#[ignore = "needs a running Everything"]
fn live_everything_query() {
    assert!(matches!(crate::everything::status(), crate::everything::Status::Ready));
    let mut n = 0;
    crate::everything::for_each_item("file: dm:today", |_| n += 1, |_, _| {}).unwrap();
    assert!(n > 0);
    let changes = crate::tree::query_changes(&crate::tree::local_stamp()).unwrap();
    assert!(changes.len() < 100_000);
}

#[test]
#[ignore = "needs a running Everything"]
fn live_full_load() {
    let t = crate::tree::load(Arc::new(crate::tree::Progress::default()), &[]).unwrap();
    assert!(t.nodes.len() > 1000);
    let sum: u64 = t.children(ROOT).iter().map(|&d| t.nodes[d as usize].size).sum();
    assert_eq!(sum, t.nodes[ROOT as usize].size);
    for (i, n) in t.nodes.iter().enumerate().skip(1) {
        assert!((n.parent as usize) < i);
    }
    println!("{} nodes, {} MB, {} ms", t.nodes.len(), t.memory() / (1024 * 1024), t.load_ms);
}

// ------------------------------------------------------------------ MCP

fn mcp_server() -> crate::mcp::Server {
    crate::mcp::Server::new(Box::new(|| Ok(Arc::new(sample()))))
}

fn call(s: &mut crate::mcp::Server, name: &str, args: serde_json::Value) -> serde_json::Value {
    let req = serde_json::json!({"jsonrpc":"2.0","id":7,"method":"tools/call","params":{"name":name,"arguments":args}});
    let r = s.handle(&req).expect("reply");
    let text = r.pointer("/result/content/0/text").and_then(|v| v.as_str()).expect("text").to_string();
    serde_json::from_str(&text).unwrap_or(serde_json::Value::String(text))
}

#[test]
fn mcp_handshake_and_tools() {
    let mut s = mcp_server();
    let init = s.handle(&serde_json::json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18"}})).unwrap();
    assert_eq!(init["result"]["serverInfo"]["name"], "wizeverything");
    assert!(s.handle(&serde_json::json!({"jsonrpc":"2.0","method":"notifications/initialized"})).is_none());
    let list = s.handle(&serde_json::json!({"jsonrpc":"2.0","id":2,"method":"tools/list"})).unwrap();
    let names: Vec<&str> = list["result"]["tools"].as_array().unwrap().iter().map(|t| t["name"].as_str().unwrap()).collect();
    assert_eq!(names, ["drives", "folder_sizes", "largest_files", "file_types", "find_duplicates", "dev_artifacts", "scan_integrity", "cloud_audit", "hardlink_plan", "cold_files", "cold_folders", "compression_candidates", "snapshot_save", "snapshot_diff", "catalog_save", "catalog_check", "similar_images", "similar_audio", "github_mode", "why_full", "growth", "advice", "backup_check", "volumes", "volume_duplicates", "volume_lookup", "provenance", "timeline", "quarantine_list", "speed_test", "addons", "refresh"]);
    let writers = ["snapshot_save", "catalog_save"];
    assert!(
        list["result"]["tools"].as_array().unwrap().iter().all(|t| {
            let is_writer = writers.contains(&t["name"].as_str().unwrap());
            t["annotations"]["readOnlyHint"] == serde_json::Value::Bool(!is_writer)
        }),
        "only snapshot_save/catalog_save may write"
    );
    let bad = s.handle(&serde_json::json!({"jsonrpc":"2.0","id":3,"method":"nope"})).unwrap();
    assert_eq!(bad["error"]["code"], -32601);
}

#[test]
fn mcp_tools_answer_from_the_index() {
    let mut s = mcp_server();
    let f = call(&mut s, "folder_sizes", serde_json::json!({"path":"Q9:\\","limit":2}));
    assert_eq!(f["folder"]["size"], 1_005_111);
    assert_eq!(f["largest_children"][0]["path"], "Q9:\\big.iso");
    assert_eq!(f["largest_children"].as_array().unwrap().len(), 2);
    let l = call(&mut s, "largest_files", serde_json::json!({"name_contains":"pdf"}));
    assert_eq!(l.as_array().unwrap().len(), 2);
    let ft = call(&mut s, "file_types", serde_json::json!({"path":"Q9:\\docs"}));
    assert!(ft.as_array().unwrap().iter().any(|c| c["top_extensions"][0]["ext"] == "pdf"));
    let d = call(&mut s, "drives", serde_json::json!({}));
    assert_eq!(d.as_array().unwrap().len(), 2);
    let missing = s.handle(&serde_json::json!({"jsonrpc":"2.0","id":9,"method":"tools/call","params":{"name":"folder_sizes","arguments":{"path":"Z:\\nope"}}})).unwrap();
    assert_eq!(missing["result"]["isError"], true);
    let guard = s.handle(&serde_json::json!({"jsonrpc":"2.0","id":10,"method":"tools/call","params":{"name":"find_duplicates","arguments":{}}})).unwrap();
    assert_eq!(guard["result"]["isError"], true, "whole-PC duplicate scans need an explicit opt-in");
}
#[test]
fn dups_quick_mode_uses_only_the_index() {
    let (_tmp, tree, root) = dup_fixture();
    let opt = DupOptions { root, min_size: 1, same_name: false, verify: false, exclude: Default::default(), versions: false };
    let groups = dup::find(Arc::new(tree), opt.clone(), Arc::new(DupProgress::default()));
    let files: Vec<_> = groups.iter().filter(|g| !g.folder).collect();
    // Quick mode always matches by name + size: d1, d2, d3 and d5 all have same.bin
    // (d3's content differs, which only the advanced mode can tell).
    assert_eq!(files.len(), 1);
    assert_eq!(files[0].files.len(), 4);
    assert!(groups.iter().all(|g| !g.verified));
}
/// All tests share one throwaway hash cache file (never the user's real one).
fn init_cache() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let p = std::env::temp_dir().join(format!("wiz_hash_cache_test_{}.bin", std::process::id()));
        let _ = std::fs::remove_file(&p);
        crate::hashcache::set_path(p);
    });
}

#[test]
fn hash_cache_second_search_reads_nothing() {
    let (tmp, tree, root) = dup_fixture();
    let tree = Arc::new(tree);
    let opt = DupOptions { root, min_size: 1, same_name: true, verify: true, exclude: Default::default(), versions: false };
    let first = Arc::new(DupProgress::default());
    let g1 = dup::find(tree.clone(), opt.clone(), first.clone());
    assert!(first.bytes_read.load(std::sync::atomic::Ordering::Relaxed) > 0);

    let second = Arc::new(DupProgress::default());
    let g2 = dup::find(tree.clone(), opt.clone(), second.clone());
    assert_eq!(g1.len(), g2.len(), "same result from the cache");
    assert_eq!(second.bytes_read.load(std::sync::atomic::Ordering::Relaxed), 0, "nothing read from disk");
    assert!(second.cache_hits.load(std::sync::atomic::Ordering::Relaxed) > 0);

    // Change d2's content (same size): its entry no longer matches and is re-hashed.
    std::thread::sleep(std::time::Duration::from_millis(20));
    let path = tmp.0.join("d2/same.bin");
    let mut data = std::fs::read(&path).unwrap();
    data[0] ^= 0xFF;
    std::fs::write(&path, &data).unwrap();
    let third = Arc::new(DupProgress::default());
    let g3 = dup::find(tree, opt.clone(), third.clone());
    assert!(third.bytes_read.load(std::sync::atomic::Ordering::Relaxed) > 0, "changed file read again");
    let files: Vec<_> = g3.iter().filter(|g| !g.folder).collect();
    assert!(files.iter().all(|g| g.files.iter().all(|f| !f.path.ends_with("d2\\same.bin"))), "d2 no longer a duplicate");
}

#[test]
fn hash_cache_persists_to_disk() {
    init_cache();
    let path = crate::hashcache::default_path();
    let (tmp, tree, root) = dup_fixture();
    let _ = dup::find(Arc::new(tree), DupOptions { root, min_size: 1, same_name: true, verify: true, exclude: Default::default(), versions: false }, Arc::new(DupProgress::default()));
    let bytes = std::fs::read(&path).expect("cache file written");
    assert!(bytes.starts_with(b"WZHC01"));
    let needle = tmp.0.to_string_lossy().to_lowercase();
    assert!(String::from_utf8_lossy(&bytes).contains(&needle), "entries for the fixture were saved");
}
#[test]
fn dups_skip_excluded_folders() {
    let (_tmp, tree, root) = dup_fixture();
    let d2 = tree.find_path(&format!("{}\\d2", tree.full_path(root))).unwrap();
    let mut exclude = std::collections::HashSet::new();
    exclude.insert(d2);
    let opt = DupOptions { root, min_size: 1, same_name: true, verify: true, exclude, versions: false };
    let groups = dup::find(Arc::new(tree), opt.clone(), Arc::new(DupProgress::default()));
    // Without d2, the only same-name match left is d1/d5 (hard links: one file) → nothing.
    assert!(groups.iter().all(|g| g.files.iter().all(|f| !f.path.contains("\\d2"))), "d2 never examined");
    assert!(groups.iter().filter(|g| !g.folder).all(|g| g.files.len() >= 2));
}

#[test]
fn settings_keep_dup_exclusions() {
    let mut s = crate::i18n::Settings::parse("", Lang::Es);
    s.dup_exclude = vec!["C:\\Windows".into(), "H:\\Mi unidad\\Backups".into()];
    let back = crate::i18n::Settings::parse(&s.to_text(), Lang::Es);
    assert_eq!(back.dup_exclude, ["C:\\Windows", "H:\\Mi unidad\\Backups"]);
}
#[test]
fn rows_show_folders_before_files_unless_sorted_by_name() {
    let t = sample();
    let q = id(&t, D1);
    let exp: HashSet<u32> = [q].into_iter().collect();
    let names_of = |sort| app::flatten_rows(&t, q, &exp, sort).iter().skip(1).map(|r| t.name(r.0).to_string()).collect::<Vec<_>>();
    // big.iso (1 MB) is bigger than docs, but folders come first.
    assert_eq!(names_of(None), ["docs", "big.iso", "noext"]);
    assert_eq!(names_of(Some((Col::Size, true))), ["docs", "noext", "big.iso"]);
    assert_eq!(names_of(Some((Col::Name, true))), ["big.iso", "docs", "noext"]);
}
// ------------------------------------------------------------------ Groq (mocked)

/// Fake Groq: lists two models; the first is out of quota, the second answers
/// after one tool call.
struct FakeGroq {
    calls: std::sync::Mutex<Vec<String>>,
}

impl crate::ai::Transport for FakeGroq {
    fn get(&self, _path: &str) -> Result<(u16, serde_json::Value), String> {
        Ok((200, serde_json::json!({"data":[{"id":"openai/gpt-oss-120b"},{"id":"llama-3.3-70b-versatile"},{"id":"whisper-large-v3"}]})))
    }
    fn post(&self, _path: &str, body: &serde_json::Value) -> Result<(u16, serde_json::Value), String> {
        let model = body["model"].as_str().unwrap().to_string();
        self.calls.lock().unwrap().push(model.clone());
        if model == "openai/gpt-oss-120b" {
            return Ok((429, serde_json::json!({"error":{"message":"rate limit"},"_rl":{"req_left":0,"tok_left":0}})));
        }
        let msgs = body["messages"].as_array().unwrap();
        if msgs.iter().any(|m| m["role"] == "tool") {
            let tool = msgs.iter().rev().find(|m| m["role"] == "tool").unwrap()["content"].as_str().unwrap().to_string();
            return Ok((200, serde_json::json!({"choices":[{"message":{"content": format!("ok {}", tool.contains("Q9:"))}}],"_rl":{"req_left":999,"tok_left":8000}})));
        }
        Ok((200, serde_json::json!({"choices":[{"message":{"content":null,"tool_calls":[{"id":"c1","type":"function","function":{"name":"drives","arguments":"{}"}}]}}]})))
    }
}

#[test]
fn groq_fails_over_to_a_model_with_quota() {
    let fake = FakeGroq { calls: Default::default() };
    let mut server = mcp_server();
    let tried = std::sync::Mutex::new(Vec::new());
    let (model, answer) = crate::ai::ask_auto(&fake, None, "¿unidades?", &mut server, true, &|m| tried.lock().unwrap().push(m.to_string())).unwrap();
    assert_eq!(model, "llama-3.3-70b-versatile");
    assert_eq!(answer, "ok true", "the tool ran against the index");
    assert_eq!(*tried.lock().unwrap(), ["openai/gpt-oss-120b", "llama-3.3-70b-versatile"], "whisper is not a chat model");
    assert_eq!(crate::ai::quota("llama-3.3-70b-versatile").unwrap().req_left, Some(999));
    // Next question: the exhausted model is known and tried last.
    let tried2 = std::sync::Mutex::new(Vec::new());
    let (model2, _) = crate::ai::ask_auto(&fake, None, "otra", &mut server, true, &|m| tried2.lock().unwrap().push(m.to_string())).unwrap();
    assert_eq!(model2, "llama-3.3-70b-versatile");
    assert_eq!(tried2.lock().unwrap()[0], "llama-3.3-70b-versatile", "no attempt wasted on the exhausted model");
}

#[test]
fn groq_model_ranking() {
    let ids: Vec<String> = ["llama-3.1-8b-instant", "whisper-large-v3", "some/new-200b", "openai/gpt-oss-120b", "meta-llama/llama-guard-4-12b"].iter().map(|s| s.to_string()).collect();
    assert_eq!(crate::ai::rank_models(&ids), ["openai/gpt-oss-120b", "llama-3.1-8b-instant", "some/new-200b"]);
}

#[test]
fn dpapi_roundtrip() {
    let enc = crate::ai::protect("gsk_test_123").unwrap();
    assert!(!enc.contains("gsk_test"), "not stored in plain text");
    assert_eq!(crate::ai::unprotect(&enc).as_deref(), Some("gsk_test_123"));
    assert_eq!(crate::ai::unprotect("zz"), None);
}
#[test]
fn dups_versions_mode_groups_by_name_and_folder() {
    init_cache();
    let base = std::env::temp_dir().join(format!("wiz_versions_{}", std::process::id()));
    for d in ["drive_new\\Docs", "backup\\old\\Docs", "other\\Docs"] {
        std::fs::create_dir_all(base.join(d)).unwrap();
    }
    std::fs::write(base.join("drive_new\\Docs\\plan.xlsx"), vec![1u8; 3000]).unwrap(); // newer, bigger
    std::fs::write(base.join("backup\\old\\Docs\\plan.xlsx"), vec![2u8; 1000]).unwrap(); // old version
    std::fs::write(base.join("other\\Docs\\notes.txt"), b"x").unwrap();
    let mut b = Builder::new();
    for e in walk(&base) {
        let m = std::fs::metadata(&e).unwrap();
        b.add(m.is_dir(), &e.parent().unwrap().to_string_lossy(), &e.file_name().unwrap().to_string_lossy(), (!m.is_dir()).then_some(m.len()), 1);
    }
    let tree = b.finish(&[], Instant::now(), String::new());
    let root = tree.find_path(&base.to_string_lossy()).unwrap();
    let opt = DupOptions { root, min_size: 1, same_name: true, verify: false, exclude: Default::default(), versions: true };
    let groups = dup::find(Arc::new(tree), opt, Arc::new(DupProgress::default()));
    let _ = std::fs::remove_dir_all(&base);
    assert_eq!(groups.len(), 1, "plan.xlsx in two Docs folders, different sizes");
    let g = &groups[0];
    assert!(g.versions && !g.verified);
    let mut sizes: Vec<u64> = g.files.iter().map(|f| f.size).collect();
    sizes.sort();
    assert_eq!(sizes, [1000, 3000]);
    assert_eq!(g.wasted(), 1000, "keeping the newest frees the old copy");
}
#[test]
fn smart_keep_prefers_the_official_copy() {
    use crate::app::keep_score;
    let s = |p: &str| keep_score(p, "").0;
    assert!(s("C:\\Users\\me\\Documents\\plan.xlsx") > s("C:\\Users\\me\\Downloads\\plan.xlsx"));
    assert!(s("D:\\Fotos\\viaje.jpg") > s("D:\\Fotos\\viaje (1).jpg"));
    assert!(s("D:\\Music\\a.mp3") > s("D:\\Backup\\Music\\a.mp3"));
    assert!(s("C:\\Users\\me\\Pictures\\x.png") > s("C:\\Users\\me\\AppData\\Local\\Temp\\x.png"));
    // The preferred drive beats everything else.
    assert!(keep_score("H:\\Mi unidad\\Backup\\a.mp3", "h:\\").0 > keep_score("C:\\Users\\me\\Music\\a.mp3", "h:\\").0);
}
#[test]
fn fast_and_deep_modes_are_explicit() {
    use crate::mcp_tools::{is_deep, mode_name};
    assert!(!is_deep(&serde_json::json!({})));
    assert!(!is_deep(&serde_json::json!({ "mode": "fast" })));
    assert!(is_deep(&serde_json::json!({ "mode": "deep" })));
    assert!(is_deep(&serde_json::json!({ "deep": true })));
    assert!(!is_deep(&serde_json::json!({ "deep": false, "mode": "fast" })));
    assert_eq!(mode_name(true), "deep");
    assert_eq!(mode_name(false), "fast");
}

#[test]
fn deep_estimates_scale_from_the_fast_run() {
    use crate::app::estimate_deep;
    use serde_json::json;
    // Audio: deep decodes 6x the window (20 s -> 120 s).
    let audio = json!({ "files": 10, "analyzed": 10, "skipped": 0, "partial": false });
    assert_eq!(estimate_deep("audio", 300, &audio), 1_800);
    // Images: fast stopped at half, deep repeats the work it could not finish.
    let imgs = json!({ "files": 4_000, "analyzed": 2_000, "partial": true });
    assert_eq!(estimate_deep("images", 5_000, &imgs), 10_000);
    // Growth: only half the changes were attributed -> ~2x.
    let growth = json!({ "added": 500, "removed": 200, "grown": 200, "shrunk": 100, "attributed_pct": 50.0 });
    assert_eq!(estimate_deep("growth", 1_000, &growth), 2_000);
    // Backup: truncated at the fast cap -> 5x (50k -> 250k).
    let backup = json!({ "truncated": true });
    assert_eq!(estimate_deep("backup", 100, &backup), 500);
    // Never below the measured fast time.
    let clean = json!({ "files": 10, "analyzed": 10, "partial": false });
    assert_eq!(estimate_deep("images", 1_200, &clean), 1_200);
}
