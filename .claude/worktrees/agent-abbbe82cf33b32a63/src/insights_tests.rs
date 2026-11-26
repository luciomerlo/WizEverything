//! Tests for the "Cleanup" tab analysis (synthetic trees, no Everything needed).

use std::time::Instant;

use super::*;
use crate::tree::Builder;

// A drive name that can't exist (cluster size falls back to 4096).
const D: &str = "Q8:";

fn id(t: &Tree, path: &str) -> u32 {
    t.find_path(path).unwrap_or_else(|| panic!("missing {path}"))
}

fn file(b: &mut Builder, path: &str, size: u64, modified: u64) {
    let (dir, name) = path.rsplit_once('\\').unwrap();
    b.add(false, dir, name, Some(size), modified);
}

fn cleanup_tree() -> Tree {
    let mut b = Builder::new();
    b.add(true, "", D, None, 1);
    let f = |b: &mut Builder, p: &str, s: u64| file(b, &format!("{D}\\{p}"), s, 1);
    // node_modules (nested node_modules counted once)
    f(&mut b, "proj\\package.json", 1);
    f(&mut b, "proj\\node_modules\\a\\index.js", 1000);
    f(&mut b, "proj\\node_modules\\a\\node_modules\\b\\x.js", 500);
    // Rust target only next to Cargo.toml
    f(&mut b, "rs\\Cargo.toml", 1);
    f(&mut b, "rs\\target\\debug\\app.exe", 2000);
    f(&mut b, "other\\target\\x.bin", 300);
    // Python
    f(&mut b, "py\\__pycache__\\m.pyc", 50);
    // user temp: whole folder, the .tmp inside is not counted again
    f(&mut b, "Users\\me\\AppData\\Local\\Temp\\t1.log", 700);
    f(&mut b, "Users\\me\\AppData\\Local\\Temp\\sub\\f.tmp", 10);
    // browser cache vs. a normal "Cache" folder
    f(&mut b, "Users\\me\\AppData\\Local\\Google\\Chrome\\User Data\\Default\\Cache\\Cache_Data\\f_0001", 4000);
    f(&mut b, "Users\\me\\AppData\\Local\\Mozilla\\Firefox\\Profiles\\x.default\\cache2\\entries\\e1", 600);
    f(&mut b, "Users\\me\\Documents\\Cache\\notes.txt", 5);
    // Windows
    f(&mut b, "Windows\\Temp\\x.etl", 60);
    f(&mut b, "Windows\\SoftwareDistribution\\Download\\abc\\u.cab", 9000);
    f(&mut b, "Windows\\System32\\Temp\\keep.dat", 7); // not Windows\Temp
    f(&mut b, "$Recycle.Bin\\S-1-5\\$R123.txt", 80);
    // loose files
    f(&mut b, "crash\\app.dmp", 3000);
    f(&mut b, "stuff\\x.tmp", 20);
    f(&mut b, "stuff\\empty.tmp", 0);
    // build outputs
    f(&mut b, "web\\package.json", 1);
    f(&mut b, "web\\dist\\bundle.js", 400);
    f(&mut b, "web\\.next\\cache\\c.bin", 90);
    f(&mut b, "photos\\dist\\p.jpg", 100);
    // Gradle: project .gradle whole, ~/.gradle only its caches
    f(&mut b, "gproj\\build.gradle", 1);
    f(&mut b, "gproj\\.gradle\\x.lock", 30);
    f(&mut b, "Users\\me\\.gradle\\caches\\c.jar", 70);
    f(&mut b, "Users\\me\\.gradle\\gradle.properties", 1);
    b.finish(&[], Instant::now(), "2026-01-01T00:00:00".into())
}

fn by_path(t: &Tree, cands: &[Candidate]) -> Vec<(String, Kind, u64)> {
    let mut v: Vec<_> = cands.iter().map(|c| (t.full_path(c.id), c.kind, c.size)).collect();
    v.sort();
    v
}

#[test]
fn cleanup_finds_known_folders_and_files() {
    let t = cleanup_tree();
    let c = find_cleanup(&t, ROOT);
    let p = |s: &str| format!("{D}\\{s}");
    let mut want = vec![
        (p("proj\\node_modules"), Kind::NodeModules, 1500),
        (p("rs\\target"), Kind::RustTarget, 2000),
        (p("py\\__pycache__"), Kind::PyCache, 50),
        (p("Users\\me\\AppData\\Local\\Temp"), Kind::UserTemp, 710),
        (p("Users\\me\\AppData\\Local\\Google\\Chrome\\User Data\\Default\\Cache"), Kind::BrowserCache, 4000),
        (p("Users\\me\\AppData\\Local\\Mozilla\\Firefox\\Profiles\\x.default\\cache2"), Kind::BrowserCache, 600),
        (p("Windows\\Temp"), Kind::WindowsTemp, 60),
        (p("Windows\\SoftwareDistribution\\Download"), Kind::WindowsUpdate, 9000),
        (p("$Recycle.Bin"), Kind::RecycleBin, 80),
        (p("crash\\app.dmp"), Kind::CrashDump, 3000),
        (p("stuff\\x.tmp"), Kind::TempFile, 20),
        (p("web\\dist"), Kind::BuildOutput, 400),
        (p("web\\.next"), Kind::BuildOutput, 90),
        (p("gproj\\.gradle"), Kind::Gradle, 30),
        (p("Users\\me\\.gradle\\caches"), Kind::Gradle, 70),
    ];
    want.sort();
    assert_eq!(by_path(&t, &c), want);
    // biggest first
    assert!(c.windows(2).all(|w| w[0].size >= w[1].size));
    assert_eq!(t.full_path(c[0].id), p("Windows\\SoftwareDistribution\\Download"));
}

#[test]
fn cleanup_respects_the_view_root() {
    let t = cleanup_tree();
    let c = find_cleanup(&t, id(&t, &format!("{D}\\proj")));
    assert_eq!(c.len(), 1);
    assert_eq!(c[0].kind, Kind::NodeModules);
    // A view inside a candidate: the candidate is the view itself.
    let nm = id(&t, &format!("{D}\\proj\\node_modules"));
    assert_eq!(find_cleanup(&t, nm), vec![Candidate { kind: Kind::NodeModules, id: nm, size: 1500 }]);
}

#[test]
fn cleanup_totals_per_kind() {
    let t = cleanup_tree();
    let s = summarize(&find_cleanup(&t, ROOT));
    let get = |k: Kind| s.iter().find(|x| x.kind == k).copied().unwrap();
    assert_eq!((get(Kind::BrowserCache).count, get(Kind::BrowserCache).size), (2, 4600));
    assert_eq!((get(Kind::BuildOutput).count, get(Kind::BuildOutput).size), (2, 490));
    assert_eq!((get(Kind::Gradle).count, get(Kind::Gradle).size), (2, 100));
    assert_eq!(s[0].kind, Kind::WindowsUpdate);
    assert!(s.windows(2).all(|w| w[0].size >= w[1].size));
    let total: u64 = s.iter().map(|x| x.size).sum();
    assert_eq!(total, 1500 + 2000 + 50 + 710 + 4600 + 60 + 9000 + 80 + 3000 + 20 + 490 + 100);
}

#[test]
fn recycle_paths_use_contents_of_system_folders() {
    let t = cleanup_tree();
    let c = find_cleanup(&t, ROOT);
    let pick = |k: Kind| -> Vec<Candidate> { c.iter().copied().filter(|x| x.kind == k).collect() };
    let p = |s: &str| format!("{D}\\{s}");

    let mut temp = recycle_paths(&t, &pick(Kind::UserTemp));
    temp.sort();
    assert_eq!(temp, [p("Users\\me\\AppData\\Local\\Temp\\sub"), p("Users\\me\\AppData\\Local\\Temp\\t1.log")]);
    assert_eq!(recycle_paths(&t, &pick(Kind::NodeModules)), [p("proj\\node_modules")]);
    assert_eq!(recycle_paths(&t, &pick(Kind::CrashDump)), [p("crash\\app.dmp")]);
    assert!(recycle_paths(&t, &pick(Kind::RecycleBin)).is_empty());
    assert!(!Kind::RecycleBin.recyclable());
}

#[test]
fn classify_needs_the_right_context() {
    let t = cleanup_tree();
    let k = |s: &str| classify_dir(&t, id(&t, &format!("{D}\\{s}")));
    assert_eq!(k("other\\target"), None);
    assert_eq!(k("photos\\dist"), None);
    assert_eq!(k("Users\\me\\Documents\\Cache"), None);
    assert_eq!(k("Windows\\System32\\Temp"), None);
    assert_eq!(k("Users\\me\\.gradle"), None);
    assert_eq!(classify_dir(&t, ROOT), None);
}

/// Q8:\a\b\c\f1 (1000, t=200) · a\b\c\f2 (500, t=300) · a\x (10, t=50, old) · d\g (300, t=150) · top (5, t=400)
fn growth_tree() -> Tree {
    let mut b = Builder::new();
    b.add(true, "", D, None, 1);
    file(&mut b, &format!("{D}\\a\\b\\c\\f1"), 1000, 200);
    file(&mut b, &format!("{D}\\a\\b\\c\\f2"), 500, 300);
    file(&mut b, &format!("{D}\\a\\x"), 10, 50);
    file(&mut b, &format!("{D}\\d\\g"), 300, 150);
    file(&mut b, &format!("{D}\\top"), 5, 400);
    file(&mut b, &format!("{D}\\nodate"), 99, 0);
    b.finish(&[], Instant::now(), "2026-01-01T00:00:00".into())
}

#[test]
fn growth_totals_and_top_files() {
    let t = growth_tree();
    let g = growth(&t, ROOT, 100, 50, 50);
    assert_eq!((g.total, g.count), (1805, 4));
    let names: Vec<&str> = g.files.iter().map(|&i| t.name(i)).collect();
    assert_eq!(names, ["f1", "f2", "g", "top"]);
    let g = growth(&t, ROOT, 100, 50, 2);
    assert_eq!(g.files.len(), 2);
    let g = growth(&t, ROOT, 1000, 50, 50);
    assert_eq!((g.total, g.count, g.folders.len(), g.files.len()), (0, 0, 0, 0));
}

#[test]
fn growth_folders_skip_pass_through_parents() {
    let t = growth_tree();
    let g = growth(&t, ROOT, 100, 50, 50);
    let got: Vec<(String, u64, u32)> = g.folders.iter().map(|f| (t.full_path(f.id), f.bytes, f.files)).collect();
    // a and a\b only pass c's bytes through: left out. The drive (1805, biggest
    // child a = 1500 < 90 %) is shown; "This PC" never is.
    assert_eq!(
        got,
        [(format!("{D}\\"), 1805, 4), (format!("{D}\\a\\b\\c"), 1500, 2), (format!("{D}\\d"), 300, 1)]
    );
    // Inside a folder view.
    let a = id(&t, &format!("{D}\\a"));
    let g = growth(&t, a, 100, 50, 50);
    assert_eq!((g.total, g.count), (1500, 2));
    let got: Vec<String> = g.folders.iter().map(|f| t.full_path(f.id)).collect();
    assert_eq!(got, [format!("{D}\\a\\b\\c")]);
    // folder_limit
    assert_eq!(growth(&t, ROOT, 100, 1, 50).folders.len(), 1);
}

#[test]
fn large_old_filters_by_size_and_date() {
    let mut b = Builder::new();
    b.add(true, "", D, None, 1);
    let now = 10_000 * DAY;
    let old = now - 3 * 365 * DAY;
    file(&mut b, &format!("{D}\\v\\old_big.mkv"), 500 * MB, old);
    file(&mut b, &format!("{D}\\v\\old_bigger.iso"), 900 * MB, old);
    file(&mut b, &format!("{D}\\v\\old_small.txt"), 5 * MB, old);
    file(&mut b, &format!("{D}\\v\\new_big.mkv"), 800 * MB, now - DAY);
    file(&mut b, &format!("{D}\\v\\nodate.bin"), 700 * MB, 0);
    let t = b.finish(&[], Instant::now(), "2026-01-01T00:00:00".into());
    let before = years_before(now, 2);
    let names = |v: Vec<u32>| v.iter().map(|&i| t.name(i).to_string()).collect::<Vec<_>>();
    assert_eq!(names(large_old(&t, ROOT, 100 * MB, before)), ["old_bigger.iso", "old_big.mkv"]);
    assert_eq!(names(large_old(&t, ROOT, MB, before)), ["old_bigger.iso", "old_big.mkv", "old_small.txt"]);
    assert!(large_old(&t, ROOT, 100 * MB, years_before(now, 4)).is_empty());
    assert!(large_old(&t, ROOT, 1000 * MB, before).is_empty());
}

#[test]
fn period_and_years_thresholds() {
    let now = now_ft();
    assert_eq!(period_start(Period::Week, now), now - 7 * DAY);
    assert_eq!(period_start(Period::Month, now), now - 30 * DAY);
    let today = period_start(Period::Today, now);
    assert!(today <= now && today > now - DAY - 3600 * 10_000_000, "midnight within the last day");
    assert_eq!(years_before(now, 2), now - 2 * 36525 * DAY / 100);
    assert_eq!(years_before(0, 2), 0);
}

#[test]
#[ignore = "needs a running Everything"]
fn live_insights_on_real_index() {
    let t = crate::tree::load(std::sync::Arc::new(crate::tree::Progress::default()), &[]).unwrap();
    let start = Instant::now();
    let c = find_cleanup(&t, ROOT);
    let cleanup_ms = start.elapsed().as_millis();
    // No candidate inside another one.
    let ids: std::collections::HashSet<u32> = c.iter().map(|x| x.id).collect();
    for x in &c {
        assert!(t.ancestors(x.id).iter().filter(|a| ids.contains(a)).count() == 1, "nested {}", t.full_path(x.id));
    }
    for s in summarize(&c) {
        println!("{:?}: {} items, {} MB", s.kind, s.count, s.size / MB);
    }

    let now = now_ft();
    let start = Instant::now();
    let g = growth(&t, ROOT, period_start(Period::Today, now), 300, 2000);
    let growth_ms = start.elapsed().as_millis();
    let mut n = 0u64;
    crate::everything::for_each_item("file: dm:today", |_| n += 1, |_, _| {}).unwrap();
    // Files keep changing between the load and the query.
    let diff = g.count.abs_diff(n);
    assert!(diff <= 200 + n / 20, "tree {} vs Everything {n}", g.count);

    let start = Instant::now();
    let old = large_old(&t, ROOT, 100 * MB, years_before(now, 2));
    let old_ms = start.elapsed().as_millis();
    assert!(old.windows(2).all(|w| t.nodes[w[0] as usize].size >= t.nodes[w[1] as usize].size));
    println!(
        "{} nodes · cleanup {} items {cleanup_ms} ms · today {} files ({n} in Everything) {growth_ms} ms · large&old {} {old_ms} ms",
        t.nodes.len(),
        c.len(),
        g.count,
        old.len()
    );
}
