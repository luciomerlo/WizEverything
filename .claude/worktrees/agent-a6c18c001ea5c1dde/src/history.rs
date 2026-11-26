//! Size history: one compact snapshot per day of the sizes of drives and
//! their larger folders (up to 3 levels deep), in
//! `%LOCALAPPDATA%\WizEverything\history\YYYY-MM-DD.bin`. Kept for a year.
//!
//! File format: b"WZH1", u32 count, then per entry: u16 path length,
//! UTF-8 path, u64 size (little endian).

use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, channel};

use eframe::egui::{self, Color32, RichText, Sense, Stroke};

use crate::app::{fmt_size, fmt_int};
use crate::i18n::tr;
use crate::tree::Tree;

const MAGIC: &[u8; 4] = b"WZH1";
/// Folder levels below each drive that are recorded.
pub const MAX_DEPTH: usize = 3;
/// Folders below this size (bytes) are not recorded (drives and their top level always are).
pub const MIN_SIZE: u64 = 16 << 20;
/// At most this many entries per snapshot (the largest win).
pub const MAX_ENTRIES: usize = 30_000;
pub const KEEP_DAYS: i64 = 365;

pub fn dir() -> Option<PathBuf> {
    Some(PathBuf::from(std::env::var_os("LOCALAPPDATA")?).join("WizEverything").join("history"))
}

/// Today's local date as YYYY-MM-DD.
pub fn today() -> String {
    crate::tree::local_stamp()[..10].to_string()
}

/// Days since 1970-01-01 of a civil date (proleptic Gregorian).
pub fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

pub fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (yoe + era * 400 + i64::from(m <= 2), m, d)
}

/// "YYYY-MM-DD" → day number.
pub fn parse_date(s: &str) -> Option<i64> {
    let mut it = s.splitn(3, '-');
    let y: i64 = it.next()?.parse().ok()?;
    let m: i64 = it.next()?.parse().ok()?;
    let d: i64 = it.next()?.get(..2)?.parse().ok()?;
    ((1..=12).contains(&m) && (1..=31).contains(&d)).then(|| days_from_civil(y, m, d))
}

pub fn date_str(day: i64) -> String {
    let (y, m, d) = civil_from_days(day);
    format!("{y:04}-{m:02}-{d:02}")
}

/// Drives and their folders up to `max_depth` levels down (drives and their
/// first level always, deeper folders only from `min_size`), largest first.
pub fn collect(tree: &Tree, max_depth: usize, min_size: u64, max_entries: usize) -> Vec<(String, u64)> {
    let mut out: Vec<(u32, u64)> = Vec::new();
    let mut stack: Vec<(u32, usize)> = tree.drives.iter().map(|&d| (d, 0)).collect();
    while let Some((id, depth)) = stack.pop() {
        let n = &tree.nodes[id as usize];
        if !n.is_dir() || (depth > 1 && n.size < min_size) {
            continue;
        }
        out.push((id, n.size));
        if depth < max_depth {
            // Children are sorted by size: stop at the first small one past level 1.
            for &c in tree.children(id) {
                let cn = &tree.nodes[c as usize];
                if depth + 1 > 1 && cn.size < min_size {
                    break;
                }
                if cn.is_dir() {
                    stack.push((c, depth + 1));
                }
            }
        }
    }
    out.sort_by(|a, b| b.1.cmp(&a.1));
    out.truncate(max_entries.max(tree.drives.len()));
    out.into_iter().map(|(id, s)| (tree.full_path(id), s)).collect()
}

pub fn encode(entries: &[(String, u64)]) -> Vec<u8> {
    let mut v = Vec::with_capacity(8 + entries.len() * 48);
    v.extend_from_slice(MAGIC);
    v.extend_from_slice(&(entries.len() as u32).to_le_bytes());
    for (p, s) in entries {
        let b = &p.as_bytes()[..p.len().min(u16::MAX as usize)];
        v.extend_from_slice(&(b.len() as u16).to_le_bytes());
        v.extend_from_slice(b);
        v.extend_from_slice(&s.to_le_bytes());
    }
    v
}

/// Calls `f(path, size)` for every entry; false if the data is malformed.
pub fn decode_each(data: &[u8], mut f: impl FnMut(&str, u64)) -> bool {
    if data.len() < 8 || &data[..4] != MAGIC {
        return false;
    }
    let count = u32::from_le_bytes(data[4..8].try_into().unwrap()) as usize;
    let mut at = 8;
    for _ in 0..count {
        let Some(l) = data.get(at..at + 2) else { return false };
        let len = u16::from_le_bytes([l[0], l[1]]) as usize;
        at += 2;
        let (Some(p), Some(s)) = (data.get(at..at + len), data.get(at + len..at + len + 8)) else { return false };
        let Ok(p) = std::str::from_utf8(p) else { return false };
        f(p, u64::from_le_bytes(s.try_into().unwrap()));
        at += len + 8;
    }
    true
}

pub fn decode(data: &[u8]) -> Option<Vec<(String, u64)>> {
    let mut v = Vec::new();
    decode_each(data, |p, s| v.push((p.to_string(), s))).then_some(v)
}

pub fn snapshot_path(dir: &Path, date: &str) -> PathBuf {
    dir.join(format!("{date}.bin"))
}

pub fn has_snapshot(dir: &Path, date: &str) -> bool {
    snapshot_path(dir, date).is_file()
}

/// Write a snapshot atomically (temp file + rename).
pub fn write_snapshot(dir: &Path, date: &str, entries: &[(String, u64)]) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    let tmp = dir.join(format!("{date}.tmp"));
    std::fs::write(&tmp, encode(entries))?;
    std::fs::rename(&tmp, snapshot_path(dir, date))
}

/// Snapshot files as (day, path), oldest first.
pub fn list(dir: &Path) -> Vec<(i64, PathBuf)> {
    let mut v: Vec<(i64, PathBuf)> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            let p = e.path();
            let stem = p.file_stem()?.to_str()?;
            (p.extension()? == "bin" && stem.len() == 10).then_some(())?;
            Some((parse_date(stem)?, p))
        })
        .collect();
    v.sort_by_key(|x| x.0);
    v
}

/// Delete snapshots older than `keep_days` before `today`. Returns how many.
pub fn prune(dir: &Path, today: &str, keep_days: i64) -> usize {
    let Some(t) = parse_date(today) else { return 0 };
    list(dir).into_iter().filter(|(d, p)| t - d > keep_days && std::fs::remove_file(p).is_ok()).count()
}

/// Size of `path` in every snapshot that has it: (day, size), oldest first.
pub fn series(dir: &Path, path: &str) -> Vec<(i64, u64)> {
    let want = path.trim_end_matches('\\');
    let mut out = Vec::new();
    for (day, file) in list(dir) {
        let Ok(data) = std::fs::read(&file) else { continue };
        let mut hit = None;
        decode_each(&data, |p, s| {
            if hit.is_none() && p.trim_end_matches('\\').eq_ignore_ascii_case(want) {
                hit = Some(s);
            }
        });
        if let Some(s) = hit {
            out.push((day, s));
        }
    }
    out
}

/// Change over the last `days`: (bytes delta, day it is measured from).
/// Uses the latest snapshot at least `days` old, or the oldest one.
pub fn growth(series: &[(i64, u64)], days: i64) -> Option<(i64, i64)> {
    let &(last_day, last) = series.last()?;
    if series.len() < 2 {
        return None;
    }
    let base = series.iter().rev().find(|(d, _)| last_day - d >= days).unwrap_or(&series[0]);
    if base.0 == last_day {
        return None;
    }
    Some((last as i64 - base.1 as i64, base.0))
}

/// Daily snapshot + the growth chart window.
#[derive(Default)]
pub struct History {
    /// Date of the last snapshot taken (or found) by this process.
    done_for: String,
    view: Option<View>,
}

struct View {
    path: String,
    rx: Option<Receiver<Vec<(i64, u64)>>>,
    series: Vec<(i64, u64)>,
}

impl History {
    /// Take today's snapshot if there is none yet (call after a full load).
    pub fn maybe_snapshot(&mut self, tree: &Tree) {
        let date = today();
        if self.done_for == date {
            return;
        }
        self.done_for = date.clone();
        let Some(dir) = dir() else { return };
        if has_snapshot(&dir, &date) {
            return;
        }
        let entries = collect(tree, MAX_DEPTH, MIN_SIZE, MAX_ENTRIES);
        std::thread::spawn(move || {
            let _ = write_snapshot(&dir, &date, &entries);
            prune(&dir, &date, KEEP_DAYS);
        });
    }

    pub fn open(&mut self, path: String) {
        let (tx, rx) = channel();
        let p = path.clone();
        std::thread::spawn(move || {
            let _ = tx.send(dir().map(|d| series(&d, &p)).unwrap_or_default());
        });
        self.view = Some(View { path, rx: Some(rx), series: Vec::new() });
    }

    /// The history window (if open). `current`: the folder's size right now.
    pub fn window(&mut self, ctx: &egui::Context, current: impl Fn(&str) -> Option<u64>) {
        let Some(v) = &mut self.view else { return };
        if let Some(rx) = &v.rx {
            match rx.try_recv() {
                Ok(s) => {
                    v.series = s;
                    v.rx = None;
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => ctx.request_repaint_after(std::time::Duration::from_millis(100)),
                Err(_) => v.rx = None,
            }
        }
        let mut open = true;
        let now = current(&v.path);
        egui::Window::new(format!("📈 {}", tr("Historial de tamaño", "Size history")))
            .id(egui::Id::unique("size_history"))
            .open(&mut open)
            .collapsible(false)
            .resizable(true)
            .default_size([560.0, 330.0])
            .show(ctx, |ui| {
                ui.label(RichText::new(&v.path).strong());
                if v.rx.is_some() {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label(tr("Leyendo instantáneas…", "Reading snapshots…"));
                    });
                    return;
                }
                if let Some(n) = now {
                    ui.label(format!("{} {}", tr("Tamaño actual:", "Current size:"), fmt_size(n)));
                }
                if v.series.len() < 2 {
                    ui.label(RichText::new(tr(
                        "Todavía no hay historial suficiente: se guarda una instantánea por día (unidades y carpetas grandes hasta 3 niveles). Volvé en unos días.",
                        "Not enough history yet: one snapshot is saved per day (drives and large folders up to 3 levels deep). Come back in a few days.",
                    )).weak());
                    return;
                }
                growth_line(ui, &v.series);
                chart(ui, &v.series);
                ui.label(RichText::new(format!(
                    "{} {} · {} → {}",
                    fmt_int(v.series.len() as u64),
                    tr("instantáneas", "snapshots"),
                    date_str(v.series[0].0),
                    date_str(v.series[v.series.len() - 1].0)
                )).weak().small());
            });
        if !open {
            self.view = None;
        }
    }
}

fn fmt_delta(d: i64) -> String {
    let s = fmt_size(d.unsigned_abs());
    if d >= 0 { format!("+{s}") } else { format!("−{s}") }
}

fn growth_line(ui: &mut egui::Ui, series: &[(i64, u64)]) {
    ui.horizontal(|ui| {
        for days in [7, 30] {
            if let Some((delta, from)) = growth(series, days) {
                let last = series[series.len() - 1].0;
                let color = if delta > 0 { Color32::from_rgb(220, 90, 70) } else { Color32::from_rgb(60, 170, 90) };
                let span = if last - from == days {
                    format!("{} {days} {}", tr("en", "in"), tr("días", "days"))
                } else {
                    format!("{} {} ({} {})", tr("desde", "since"), date_str(from), last - from, tr("días", "days"))
                };
                ui.label(RichText::new(format!("{} {span}", fmt_delta(delta))).color(color).strong());
                ui.separator();
            }
        }
    });
}

/// Line chart of size over time, drawn with the painter.
fn chart(ui: &mut egui::Ui, series: &[(i64, u64)]) {
    let w = ui.available_width().max(200.0);
    let h = (ui.available_height() - 24.0).clamp(120.0, 400.0);
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(w, h), Sense::hover());
    let p = ui.painter_at(rect);
    let vis = ui.visuals();
    p.rect_filled(rect, 4.0, vis.extreme_bg_color);
    let plot = egui::Rect::from_min_max(rect.min + egui::vec2(70.0, 10.0), rect.max - egui::vec2(12.0, 22.0));
    let (d0, d1) = (series[0].0, series[series.len() - 1].0);
    let max = series.iter().map(|s| s.1).max().unwrap_or(1).max(1);
    let min = series.iter().map(|s| s.1).min().unwrap_or(0);
    // Y range with a margin; not from zero, so small changes are visible.
    let pad = ((max - min) / 8).max(max / 100).max(1);
    let (lo, hi) = (min.saturating_sub(pad), max + pad);
    let x = |d: i64| plot.left() + plot.width() * (d - d0) as f32 / (d1 - d0).max(1) as f32;
    let y = |s: u64| plot.bottom() - plot.height() * (s - lo) as f32 / (hi - lo).max(1) as f32;
    let grid = Stroke::new(1.0, vis.widgets.noninteractive.bg_stroke.color);
    let text = vis.weak_text_color();
    for k in 0..=3 {
        let v = lo + (hi - lo) / 3 * k;
        let yy = y(v);
        p.line_segment([egui::pos2(plot.left(), yy), egui::pos2(plot.right(), yy)], grid);
        p.text(egui::pos2(plot.left() - 6.0, yy), egui::Align2::RIGHT_CENTER, fmt_size(v), egui::FontId::proportional(11.0), text);
    }
    p.text(egui::pos2(plot.left(), rect.bottom() - 4.0), egui::Align2::LEFT_BOTTOM, date_str(d0), egui::FontId::proportional(11.0), text);
    p.text(egui::pos2(plot.right(), rect.bottom() - 4.0), egui::Align2::RIGHT_BOTTOM, date_str(d1), egui::FontId::proportional(11.0), text);
    let pts: Vec<egui::Pos2> = series.iter().map(|&(d, s)| egui::pos2(x(d), y(s))).collect();
    // Area under the line, then the line and the points.
    let accent = Color32::from_rgb(91, 139, 201);
    for w in pts.windows(2) {
        let quad = vec![w[0], w[1], egui::pos2(w[1].x, plot.bottom()), egui::pos2(w[0].x, plot.bottom())];
        p.add(egui::Shape::convex_polygon(quad, accent.gamma_multiply(0.18), Stroke::NONE));
    }
    p.add(egui::Shape::line(pts.clone(), Stroke::new(2.0, accent)));
    if pts.len() <= 120 {
        for &pt in &pts {
            p.circle_filled(pt, 2.5, accent);
        }
    }
    // Hover: the nearest snapshot.
    if let Some(pos) = resp.hover_pos()
        && let Some((i, pt)) = pts.iter().enumerate().min_by(|a, b| (a.1.x - pos.x).abs().total_cmp(&(b.1.x - pos.x).abs()))
    {
        p.line_segment([egui::pos2(pt.x, plot.top()), egui::pos2(pt.x, plot.bottom())], Stroke::new(1.0, text));
        p.circle_stroke(*pt, 4.5, Stroke::new(2.0, accent));
        let (d, s) = series[i];
        resp.on_hover_ui_at_pointer(|ui| {
            ui.label(format!("{}  ·  {}", date_str(d), fmt_size(s)));
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TempDir(PathBuf);
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    fn temp() -> TempDir {
        let p = std::env::temp_dir().join(format!("wiz_hist_{}_{}", std::process::id(), std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()));
        std::fs::create_dir_all(&p).unwrap();
        TempDir(p)
    }

    #[test]
    fn dates_roundtrip() {
        assert_eq!(days_from_civil(1970, 1, 1), 0);
        assert_eq!(parse_date("2026-11-19").map(date_str).as_deref(), Some("2026-11-19"));
        assert_eq!(parse_date("2024-03-01").unwrap() - parse_date("2024-02-28").unwrap(), 2); // leap year
        assert_eq!(parse_date("2026-13-01"), None);
        assert_eq!(parse_date("nope"), None);
        for d in [-1000, 0, 19_000, 20_500] {
            assert_eq!(parse_date(&date_str(d)), Some(d));
        }
    }

    #[test]
    fn encode_decode_roundtrip() {
        let e = vec![("C:\\".to_string(), 123), ("C:\\Usuarios\\Ñandú".to_string(), u64::MAX - 1)];
        assert_eq!(decode(&encode(&e)), Some(e));
        assert_eq!(decode(b"WZH1\x05\0\0\0"), None);
        assert_eq!(decode(b"junk"), None);
    }

    #[test]
    fn snapshot_write_read_prune() {
        let t = temp();
        let d = &t.0;
        write_snapshot(d, "2025-10-01", &[("C:\\".into(), 100), ("C:\\Data".into(), 40)]).unwrap();
        write_snapshot(d, "2026-11-10", &[("C:\\".into(), 200), ("C:\\Data".into(), 70)]).unwrap();
        write_snapshot(d, "2026-11-19", &[("C:\\".into(), 260), ("C:\\Data".into(), 90)]).unwrap();
        std::fs::write(d.join("notes.txt"), "x").unwrap();
        assert!(has_snapshot(d, "2026-11-19") && !has_snapshot(d, "2026-11-18"));
        let s = series(d, "c:\\data");
        assert_eq!(s.iter().map(|x| x.1).collect::<Vec<_>>(), vec![40, 70, 90]);
        assert_eq!(series(d, "C:").len(), 3, "drive root matches with or without the backslash");
        // 7 days: from 2026-11-10 (9 days before the last one).
        let (delta, from) = growth(&s, 7).unwrap();
        assert_eq!((delta, date_str(from).as_str()), (20, "2026-11-10"));
        assert_eq!(growth(&s, 30).unwrap().0, 50);
        assert_eq!(growth(&s[..1], 7), None);
        // Older than a year → deleted; other files untouched.
        assert_eq!(prune(d, "2026-11-19", KEEP_DAYS), 1);
        assert_eq!(list(d).len(), 2);
        assert!(d.join("notes.txt").exists());
    }

    #[test]
    fn collect_depth_and_size() {
        use crate::tree::Builder;
        let mut b = Builder::new();
        b.add(true, "", "Q9:", None, 1);
        b.add(false, "Q9:\\a\\b\\c\\d", "deep.bin", Some(MIN_SIZE * 2), 1);
        b.add(false, "Q9:\\small\\x", "tiny.bin", Some(10), 1);
        let t = b.finish(&[], std::time::Instant::now(), String::new());
        let paths: Vec<String> = collect(&t, MAX_DEPTH, MIN_SIZE, 100).into_iter().map(|e| e.0).collect();
        for want in ["Q9:\\", "Q9:\\a", "Q9:\\a\\b", "Q9:\\a\\b\\c", "Q9:\\small"] {
            assert!(paths.contains(&want.to_string()), "{want} missing from {paths:?}");
        }
        assert!(!paths.iter().any(|p| p.ends_with("\\d") || p.ends_with("\\x")), "{paths:?}");
    }
}
