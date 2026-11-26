//! Tests for duplicate selection rules, hard-link replacement and the
//! assistant's proposal tools.

use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use std::time::Instant;

use serde_json::{Value, json};

use crate::dup::{DupFile, DupGroup};
use crate::hardlink::{self, Job, Outcome};
use crate::rules::{self, Keep, Rules};
use crate::tree::Builder;

const ALL: [Keep; 5] = [Keep::Smart, Keep::Oldest, Keep::Newest, Keep::ShortestPath, Keep::LongestPath];

/// A group whose i-th copy has modification time i+1 (the last is the newest).
fn grp(paths: &[&str], size: u64, verified: bool, folder: bool) -> DupGroup {
    DupGroup {
        size,
        files: paths.iter().enumerate().map(|(i, p)| DupFile { path: p.to_string(), modified: i as u64 + 1, size }).collect(),
        folder,
        inner_files: if folder { 3 } else { 0 },
        verified,
        versions: false,
    }
}

fn rules_with(protect: &[&str], prefer_folder: &str) -> Rules {
    Rules { protect: protect.iter().map(|s| s.to_string()).collect(), prefer_folder: prefer_folder.into(), prefer_drive: String::new() }
}

// ------------------------------------------------------------------ rules

#[test]
fn protected_folders_are_never_marked() {
    let groups = vec![
        // The protected copy is the oldest, has the longest path and a copy-like name.
        grp(&["C:\\Prot\\deep\\folder\\a (1).bin", "D:\\x\\a.bin", "E:\\a.bin"], 100, true, false),
        // Every copy protected: nothing can be marked.
        grp(&["C:\\Prot\\b.bin", "C:\\PROT\\sub\\b.bin"], 50, true, false),
        // A folder copy that contains a protected folder can't be marked either.
        grp(&["D:\\Data", "F:\\Data"], 1000, true, true),
    ];
    let r = rules_with(&["c:/prot/", "D:\\Data\\Keep"], "");
    for keep in ALL {
        let m = rules::plan_marks(&groups, &r, keep);
        for p in &m.marked {
            assert!(!r.is_protected(p), "{keep:?} marked protected {p}");
        }
        assert!(!m.marked.contains("C:\\Prot\\deep\\folder\\a (1).bin"), "{keep:?}");
        assert!(!m.marked.contains("D:\\Data"), "{keep:?}: folder containing a protected one");
        assert!(m.marked.iter().all(|p| !p.to_lowercase().contains("\\prot\\")), "{keep:?}");
        // The protected copy is kept, so both other copies are marked.
        assert!(m.marked.contains("D:\\x\\a.bin") && m.marked.contains("E:\\a.bin"), "{keep:?}: {:?}", m.marked);
        assert_eq!(m.reasons.get("C:\\Prot\\deep\\folder\\a (1).bin").map(String::as_str), Some(crate::i18n::tr("carpeta protegida", "protected folder")), "{keep:?}");
    }
    assert!(r.is_protected("c:\\prot"));
    assert!(r.is_protected("C:\\Prot\\x\\y.txt"));
    assert!(!r.is_protected("C:\\Protect\\y.txt"), "prefix of a name is not a parent folder");
    assert!(r.is_protected("D:\\"), "a drive that contains a protected folder");
}

#[test]
fn preferred_folder_wins_and_keep_rules() {
    let groups = vec![grp(&["C:\\a\\x.bin", "C:\\Users\\me\\Documents\\long\\x.bin", "H:\\Mi unidad\\x.bin"], 10, true, false)];
    let pref = rules_with(&[], "h:\\mi unidad");
    for keep in ALL {
        let m = rules::plan_marks(&groups, &pref, keep);
        assert!(!m.marked.contains("H:\\Mi unidad\\x.bin"), "{keep:?} keeps the preferred folder copy");
        assert_eq!(m.marked.len(), 2, "{keep:?}");
    }
    let none = Rules::default();
    let kept = |keep| {
        let m = rules::plan_marks(&groups, &none, keep);
        groups[0].files.iter().map(|f| f.path.clone()).find(|p| !m.marked.contains(p)).unwrap()
    };
    assert_eq!(kept(Keep::ShortestPath), "C:\\a\\x.bin");
    assert_eq!(kept(Keep::LongestPath), "C:\\Users\\me\\Documents\\long\\x.bin");
    assert_eq!(kept(Keep::Oldest), "C:\\a\\x.bin");
    assert_eq!(kept(Keep::Newest), "H:\\Mi unidad\\x.bin");
    assert_eq!(kept(Keep::Smart), "C:\\Users\\me\\Documents\\long\\x.bin", "organized personal folder");
    // The smart suggestion respects the preferred and protected folders over its heuristics.
    let prot = rules_with(&["C:\\a"], "");
    let m = rules::plan_marks(&groups, &prot, Keep::Smart);
    assert!(!m.marked.contains("C:\\a\\x.bin"));
    assert!(prot.score("C:\\a\\x.bin").0 > prot.score("C:\\Users\\me\\Documents\\long\\x.bin").0);
    assert!(pref.score("H:\\Mi unidad\\x.bin").0 > prot.score("C:\\a\\x.bin").0, "preferred folder > protected");
    assert_eq!(Keep::parse("shortest-path"), Some(Keep::ShortestPath));
    assert_eq!(Keep::parse(""), Some(Keep::Smart));
    assert_eq!(Keep::parse("biggest"), None);
}

#[test]
fn settings_keep_protected_and_preferred_folders() {
    let mut s = crate::i18n::Settings::parse("", crate::i18n::Lang::Es);
    s.dup_protect = vec!["D:\\Fotos".into(), "H:\\Mi unidad\\Trabajo".into()];
    s.dup_prefer_folder = "H:\\Mi unidad".into();
    let back = crate::i18n::Settings::parse(&s.to_text(), crate::i18n::Lang::Es);
    assert_eq!(back.dup_protect, ["D:\\Fotos", "H:\\Mi unidad\\Trabajo"]);
    assert_eq!(back.dup_prefer_folder, "H:\\Mi unidad");
    let r = Rules::from_settings(&back);
    assert!(r.is_protected("d:\\fotos\\2020\\a.jpg") && r.in_prefer_folder("H:\\Mi unidad\\x"));
}

// ------------------------------------------------------------------ hard links

struct Tmp(std::path::PathBuf);
impl Drop for Tmp {
    fn drop(&mut self) {
        // Clear read-only/system flags set by the tests so the folder can go.
        for e in std::fs::read_dir(&self.0).into_iter().flatten().flatten() {
            set_attrs(&e.path().to_string_lossy(), 0x80);
        }
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn set_attrs(path: &str, a: u32) {
    unsafe { windows_sys::Win32::Storage::FileSystem::SetFileAttributesW(crate::everything::wide(path).as_ptr(), a) };
}

fn tmp_dir(tag: &str) -> Tmp {
    static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let d = std::env::temp_dir().join(format!("wiz_link_{tag}_{}_{n}", std::process::id()));
    std::fs::create_dir_all(&d).unwrap();
    Tmp(d)
}

fn data() -> Vec<u8> {
    (0..300_000u32).map(|i| (i.wrapping_mul(2654435761) >> 11) as u8).collect()
}

fn write(dir: &Tmp, name: &str, bytes: &[u8]) -> String {
    let p = dir.0.join(name);
    std::fs::write(&p, bytes).unwrap();
    p.to_string_lossy().to_string()
}

fn leftovers(dir: &Tmp) -> Vec<String> {
    std::fs::read_dir(&dir.0).unwrap().flatten().map(|e| e.file_name().to_string_lossy().to_string()).filter(|n| n.starts_with("~wizlink")).collect()
}

#[test]
fn hardlink_replaces_marked_copies() {
    let dir = tmp_dir("ok");
    let d = data();
    let a = write(&dir, "a.bin", &d);
    std::fs::create_dir_all(dir.0.join("sub")).unwrap();
    let b = write(&dir, "sub\\b.bin", &d);
    let c = write(&dir, "c copy.bin", &d);
    let groups = vec![grp(&[&a, &b, &c], d.len() as u64, true, false)];
    let marked: HashSet<String> = [b.clone(), c.clone()].into_iter().collect();
    let (jobs, refused) = hardlink::plan(&groups, &marked, &Rules::default());
    assert!(refused.is_empty(), "{refused:?}");
    assert_eq!(jobs.len(), 2);
    assert!(jobs.iter().all(|j| j.keep == a));

    let done = AtomicUsize::new(0);
    let r = hardlink::run(&jobs, &done);
    assert!(r.failed.is_empty(), "{:?}", r.failed);
    assert_eq!(r.linked.len(), 2);
    assert_eq!(r.freed, 2 * d.len() as u64, "both copies had no other link");
    assert_eq!(done.load(std::sync::atomic::Ordering::Relaxed), 2);
    // Same content at every path, one physical file.
    for p in [&a, &b, &c] {
        assert_eq!(std::fs::read(p).unwrap(), d, "{p} readable with the same content");
    }
    let id = hardlink::file_id(&a).unwrap();
    assert_eq!(hardlink::file_id(&b), Some(id));
    assert_eq!(hardlink::file_id(&c), Some(id));
    assert_eq!(hardlink::link_count(&a), Some(3));
    assert!(leftovers(&dir).is_empty() && std::fs::read_dir(dir.0.join("sub")).unwrap().count() == 1, "no temporary names left");

    // Running again: already the same file, nothing to do.
    assert!(matches!(hardlink::replace_one(&jobs[0]), Ok(Outcome::Already)));
    // The results drop the linked copies; the group is gone.
    let mut g = groups.clone();
    hardlink::prune(&mut g, &r.linked);
    assert!(g.is_empty());
}

#[test]
fn hardlink_plan_refuses_unsafe_groups() {
    let d = "C:\\x\\";
    let f = |n: &str| format!("{d}{n}");
    let mut versions = grp(&[&f("v1"), &f("v2")], 10, false, false);
    versions.versions = true;
    let groups = vec![
        grp(&[&f("q1"), &f("q2")], 10, false, false),     // quick mode
        versions,                                          // versions
        grp(&[&f("dirA"), &f("dirB")], 10, true, true),    // folders
        grp(&[&f("p1"), "C:\\Prot\\p2"], 10, true, false), // protected marked copy
        grp(&[&f("m1"), &f("m2")], 10, true, false),       // every copy marked
        grp(&[&f("o1"), "Q9:\\o2"], 10, true, false),      // kept copy on another drive
        grp(&[&f("ok1"), &f("ok2")], 10, true, false),     // fine
    ];
    let marked: HashSet<String> =
        ["q2", "v2", "dirB", "m1", "m2", "ok2"].iter().map(|n| f(n)).chain(["C:\\Prot\\p2".to_string(), "Q9:\\o2".to_string()]).collect();
    let (jobs, refused) = hardlink::plan(&groups, &marked, &rules_with(&["C:\\Prot"], ""));
    assert_eq!(jobs.len(), 1, "{jobs:?}");
    assert_eq!(jobs[0].target, f("ok2"));
    let refused: Vec<&str> = refused.iter().map(|r| r.0.as_str()).collect();
    for p in [f("q2"), f("v2"), f("dirB"), "C:\\Prot\\p2".into(), f("m1"), f("m2"), "Q9:\\o2".into()] {
        assert!(refused.contains(&p.as_str()), "{p} refused: {refused:?}");
    }
    assert!(!hardlink::summarize(&hardlink::plan(&groups, &marked, &Rules::default()).1).is_empty());
}

#[test]
fn hardlink_keeps_the_original_when_unsafe() {
    let dir = tmp_dir("refuse");
    let d = data();
    let keep = write(&dir, "keep.bin", &d);
    let job = |target: &str, size: usize| Job { keep: keep.clone(), target: target.to_string(), size: size as u64 };

    // Same size, different last byte: content changed since the search.
    let mut other = d.clone();
    *other.last_mut().unwrap() ^= 1;
    let changed = write(&dir, "changed.bin", &other);
    assert!(hardlink::replace_one(&job(&changed, d.len())).is_err());
    assert_eq!(std::fs::read(&changed).unwrap(), other, "original left as it was");
    assert_ne!(hardlink::file_id(&changed), hardlink::file_id(&keep));

    // Size different from the search result.
    let same = write(&dir, "same.bin", &d);
    assert!(hardlink::replace_one(&job(&same, d.len() + 1)).is_err());

    // Read-only and system copies are refused.
    let ro = write(&dir, "ro.bin", &d);
    set_attrs(&ro, 0x1);
    assert!(hardlink::replace_one(&job(&ro, d.len())).unwrap_err().contains(crate::i18n::tr("solo lectura", "read-only")));
    let sys = write(&dir, "sys.bin", &d);
    set_attrs(&sys, 0x4);
    assert!(hardlink::replace_one(&job(&sys, d.len())).is_err());
    // A read-only kept copy is refused too.
    set_attrs(&keep, 0x1);
    assert!(hardlink::replace_one(&job(&same, d.len())).is_err());
    set_attrs(&keep, 0x80);

    // Missing file / different volume.
    assert!(hardlink::replace_one(&job(&format!("{}\\nope.bin", dir.0.display()), d.len())).is_err());
    assert!(hardlink::replace_one(&Job { keep: keep.clone(), target: "Q9:\\x.bin".into(), size: d.len() as u64 }).is_err());

    for p in [&ro, &sys, &same] {
        assert_eq!(std::fs::read(p).unwrap(), d);
        assert_ne!(hardlink::file_id(p), hardlink::file_id(&keep), "{p} not linked");
    }
    assert!(leftovers(&dir).is_empty());
}

// ------------------------------------------------------------------ proposals (MCP)

/// Q9:\Docs\a.pdf and Q9:\Backup\Docs\a.pdf (20 MB, same name: a quick-mode
/// duplicate), a Temp folder, an installer in Downloads and a protected copy.
fn proposal_server(protect: &[&str]) -> crate::mcp::Server {
    let mut b = Builder::new();
    let mb = 1024 * 1024;
    b.add(true, "", "Q9:", None, 1);
    b.add(false, "Q9:\\Docs", "a.pdf", Some(20 * mb), 5);
    b.add(false, "Q9:\\Backup\\Docs", "a.pdf", Some(20 * mb), 3);
    b.add(false, "Q9:\\Safe", "a.pdf", Some(20 * mb), 1);
    b.add(false, "Q9:\\Users\\me\\AppData\\Local\\Temp", "junk.tmp", Some(30 * mb), 1);
    b.add(false, "Q9:\\Users\\me\\Downloads", "setup.exe", Some(40 * mb), 1);
    b.add(false, "Q9:\\Users\\me\\Documents", "small.log", Some(mb), 1);
    let tree = Arc::new(b.finish(&[], Instant::now(), String::new()));
    crate::mcp::Server::new(Box::new(move || Ok(tree.clone()))).with_rules(rules_with(protect, "")).with_exclusions(Vec::new())
}

fn tool(s: &mut crate::mcp::Server, name: &str, args: Value) -> Value {
    let r = s.handle(&json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":name,"arguments":args}})).unwrap();
    assert_ne!(r["result"]["isError"], true, "{r}");
    serde_json::from_str(r["result"]["content"][0]["text"].as_str().unwrap()).unwrap()
}

#[test]
fn propose_duplicate_marks_is_data_only() {
    let mut s = proposal_server(&["Q9:\\Safe"]);
    let v = tool(&mut s, "propose_duplicate_marks", json!({"path":"Q9:\\","verify":false}));
    assert_eq!(v["proposal_only"], true);
    assert!(v["note"].as_str().unwrap().contains("nothing was marked"));
    assert_eq!(v["rule"], "smart");
    assert_eq!(v["groups"], 1);
    assert_eq!(v["all_verified_by_content"], false);
    let sug = &v["suggestions"][0];
    // The protected copy is kept (score bonus) and never marked; both others are.
    assert_eq!(sug["keep"], "Q9:\\Safe\\a.pdf");
    assert!(!sug["why"].as_str().unwrap().is_empty());
    let marks: Vec<&str> = sug["mark"].as_array().unwrap().iter().map(|m| m.as_str().unwrap()).collect();
    assert_eq!(marks.len(), 2);
    assert!(!marks.contains(&"Q9:\\Safe\\a.pdf"));
    assert_eq!(v["copies_to_mark"], 2);
    assert_eq!(v["bytes_freed"], 40 * 1024 * 1024);

    // The app gets the full proposal to show as a card.
    let p = s.take_proposals();
    assert_eq!(p.len(), 1);
    assert_eq!(p[0].kind, "duplicate_marks");
    assert_eq!(p[0].marks.marked.len(), 2);
    assert!(s.take_proposals().is_empty(), "taken once");

    // Other rules, and a bad one.
    let v = tool(&mut s, "propose_duplicate_marks", json!({"path":"Q9:\\","verify":false,"keep":"oldest","protect":["Q9:\\Docs"]}));
    let marks: Vec<&str> = v["suggestions"][0]["mark"].as_array().unwrap().iter().map(|m| m.as_str().unwrap()).collect();
    assert_eq!(marks, ["Q9:\\Backup\\Docs\\a.pdf"], "Docs and Safe are protected");
    assert_eq!(v["protected_copies_left_unmarked"], 1);
    let bad = s.handle(&json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"propose_duplicate_marks","arguments":{"path":"Q9:\\","keep":"biggest"}}})).unwrap();
    assert_eq!(bad["result"]["isError"], true);
    let guard = s.handle(&json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"propose_cleanup","arguments":{}}})).unwrap();
    assert_eq!(guard["result"]["isError"], true, "whole-PC scans need an explicit opt-in");
}

#[test]
fn propose_cleanup_lists_candidates() {
    let mut s = proposal_server(&[]);
    let v = tool(&mut s, "propose_cleanup", json!({"path":"Q9:\\","verify":false}));
    assert_eq!(v["proposal_only"], true);
    let items: Vec<&str> = v["other_candidates"].as_array().unwrap().iter().map(|i| i["path"].as_str().unwrap()).collect();
    assert_eq!(items, ["Q9:\\Users\\me\\Downloads\\setup.exe", "Q9:\\Users\\me\\AppData\\Local\\Temp"], "largest first; small log under 10 MB skipped");
    assert_eq!(v["duplicates"]["copies_to_mark"], 2);
    assert_eq!(v["total_bytes"], (40 + 40 + 30) * 1024 * 1024);
    let p = s.take_proposals();
    assert_eq!(p[0].kind, "cleanup");
    assert_eq!(p[0].items.len(), 2);
    assert_eq!(p[0].item_bytes(), 70 * 1024 * 1024);
    // Protected folders are left out of the candidates.
    let mut s = proposal_server(&["Q9:\\Users\\me\\Downloads"]);
    let v = tool(&mut s, "propose_cleanup", json!({"path":"Q9:\\","verify":false}));
    assert_eq!(v["other_candidates"].as_array().unwrap().len(), 1);
}

#[test]
fn proposal_tools_are_read_only() {
    let tools = crate::mcp::tools();
    for name in ["propose_duplicate_marks", "propose_cleanup"] {
        let t = tools.as_array().unwrap().iter().find(|t| t["name"] == name).unwrap_or_else(|| panic!("{name} listed"));
        assert_eq!(t["annotations"]["readOnlyHint"], true);
        assert_eq!(t["annotations"]["destructiveHint"], false);
    }
}

/// The assistant's proposal reaches the app; nothing on disk changes.
#[test]
fn groq_proposal_is_collected_not_applied() {
    struct Fake;
    impl crate::ai::Transport for Fake {
        fn get(&self, _: &str) -> Result<(u16, Value), String> {
            Ok((200, json!({"data":[{"id":"llama-3.3-70b-versatile"}]})))
        }
        fn post(&self, _: &str, body: &Value) -> Result<(u16, Value), String> {
            let msgs = body["messages"].as_array().unwrap();
            if let Some(t) = msgs.iter().rev().find(|m| m["role"] == "tool") {
                let v: Value = serde_json::from_str(t["content"].as_str().unwrap()).unwrap();
                return Ok((200, json!({"choices":[{"message":{"content": format!("marks {}", v["copies_to_mark"])}}]})));
            }
            Ok((200, json!({"choices":[{"message":{"content":null,"tool_calls":[{"id":"c1","type":"function","function":{"name":"propose_duplicate_marks","arguments":"{\"path\":\"Q9:\\\\\"}"}}]}}]})))
        }
    }
    let mut s = proposal_server(&[]);
    let (_, answer) = crate::ai::ask_auto(&Fake, None, "¿qué borro?", &mut s, true, &|_| {}).unwrap();
    assert_eq!(answer, "marks 2", "verify defaults to quick mode for the assistant (the index paths don't exist on disk)");
    assert_eq!(s.take_proposals().len(), 1);
}
