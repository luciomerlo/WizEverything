//! Free-space alerts: when a local drive drops below a free-space threshold
//! (global or per drive), a tray notification is shown, at most once per day
//! per drive. Clicking it opens the app on that drive.
//!
//! Settings and the "already notified" state live in
//! `%APPDATA%\WizEverything\alerts.ini`:
//! `enabled=1`, `pct=10`, `drive=D:,5` (own threshold), `notified=C:,2026-11-19`.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use eframe::egui::{self, RichText};

use crate::app::fmt_size;
use crate::i18n::tr;

#[derive(Clone, Debug, PartialEq)]
pub struct Config {
    pub enabled: bool,
    /// Global threshold: % of the drive that must stay free.
    pub pct: f32,
    /// Per-drive thresholds that override the global one (e.g. ("D:", 5.0)).
    pub drives: Vec<(String, f32)>,
    /// Drive → last date (YYYY-MM-DD) a notification was shown for it.
    pub notified: Vec<(String, String)>,
}

impl Default for Config {
    fn default() -> Self {
        Self { enabled: true, pct: 10.0, drives: Vec::new(), notified: Vec::new() }
    }
}

fn path() -> Option<std::path::PathBuf> {
    Some(std::path::PathBuf::from(std::env::var_os("APPDATA")?).join("WizEverything").join("alerts.ini"))
}

impl Config {
    pub fn parse(text: &str) -> Self {
        let mut c = Config::default();
        for line in text.lines() {
            let Some((k, v)) = line.split_once('=') else { continue };
            let v = v.trim();
            let pair = || v.split_once(',').map(|(a, b)| (a.trim().to_string(), b.trim().to_string()));
            match k.trim() {
                "enabled" => c.enabled = v != "0",
                "pct" => {
                    if let Ok(p) = v.parse::<f32>() {
                        c.pct = p.clamp(0.0, 100.0);
                    }
                }
                "drive" => {
                    if let Some((d, p)) = pair()
                        && let Ok(p) = p.parse::<f32>()
                        && !d.is_empty()
                    {
                        c.drives.push((d, p.clamp(0.0, 100.0)));
                    }
                }
                "notified" => {
                    if let Some((d, date)) = pair()
                        && !d.is_empty()
                    {
                        c.notified.push((d, date));
                    }
                }
                _ => {}
            }
        }
        c
    }

    pub fn to_text(&self) -> String {
        let mut s = format!("enabled={}\npct={}\n", self.enabled as u8, self.pct);
        for (d, p) in &self.drives {
            s += &format!("drive={d},{p}\n");
        }
        for (d, date) in &self.notified {
            s += &format!("notified={d},{date}\n");
        }
        s
    }

    pub fn load() -> Self {
        Self::parse(&path().and_then(|p| std::fs::read_to_string(p).ok()).unwrap_or_default())
    }

    pub fn save(&self) {
        let Some(p) = path() else { return };
        if let Some(d) = p.parent() {
            let _ = std::fs::create_dir_all(d);
        }
        let _ = std::fs::write(p, self.to_text());
    }

    /// Threshold (% free) that applies to `drive`.
    pub fn threshold(&self, drive: &str) -> f32 {
        self.drives.iter().find(|(d, _)| d.eq_ignore_ascii_case(drive)).map_or(self.pct, |x| x.1)
    }

    fn notified_on(&self, drive: &str) -> Option<&str> {
        self.notified.iter().find(|(d, _)| d.eq_ignore_ascii_case(drive)).map(|x| x.1.as_str())
    }

    fn mark(&mut self, drive: &str, today: &str) {
        self.notified.retain(|(d, _)| !d.eq_ignore_ascii_case(drive));
        self.notified.push((drive.to_string(), today.to_string()));
    }
}

/// True if less than `pct` % of `total` is free.
pub fn below(total: u64, free: u64, pct: f32) -> bool {
    total > 0 && pct > 0.0 && (free.min(total) as f64) < total as f64 * pct as f64 / 100.0
}

#[derive(Debug, PartialEq)]
pub struct Alert {
    pub drive: String,
    pub total: u64,
    pub free: u64,
    pub pct: f32,
}

/// Drives (name, total, free) under their threshold that weren't notified
/// today. Marks them as notified in `cfg`.
pub fn evaluate(cfg: &mut Config, readings: &[(String, u64, u64)], today: &str) -> Vec<Alert> {
    if !cfg.enabled {
        return Vec::new();
    }
    let mut out = Vec::new();
    for (drive, total, free) in readings {
        let pct = cfg.threshold(drive);
        if below(*total, *free, pct) && cfg.notified_on(drive) != Some(today) {
            cfg.mark(drive, today);
            out.push(Alert { drive: drive.clone(), total: *total, free: (*free).min(*total), pct });
        }
    }
    out
}

/// Local fixed and removable drives ("C:", "D:", …): network and optical
/// drives are skipped (they can block or are always full).
pub fn local_drives() -> Vec<String> {
    use windows_sys::Win32::Storage::FileSystem::{GetDriveTypeW, GetLogicalDrives};
    const DRIVE_REMOVABLE: u32 = 2;
    const DRIVE_FIXED: u32 = 3;
    let mask = unsafe { GetLogicalDrives() };
    (0..26u8)
        .filter(|i| mask & (1 << i) != 0)
        .map(|i| format!("{}:", (b'A' + i) as char))
        .filter(|d| {
            let root = crate::everything::wide(&format!("{d}\\"));
            matches!(unsafe { GetDriveTypeW(root.as_ptr()) }, DRIVE_FIXED | DRIVE_REMOVABLE)
        })
        .collect()
}

// ---------------------------------------------------------------- notifications

/// Drive of the last notification (what a click on it opens).
static LAST: Mutex<Option<String>> = Mutex::new(None);
static CLICKED: AtomicBool = AtomicBool::new(false);
static CTX: OnceLock<egui::Context> = OnceLock::new();

const NIN_BALLOONUSERCLICK: u32 = 0x0405;
/// tray-icon's callback message for its notification icon.
const WM_USER_TRAYICON: u32 = 6002;

unsafe extern "system" fn subclass_proc(
    hwnd: windows_sys::Win32::Foundation::HWND,
    msg: u32,
    wparam: usize,
    lparam: isize,
    _id: usize,
    _data: usize,
) -> isize {
    use windows_sys::Win32::UI::Shell::DefSubclassProc;
    if msg == WM_USER_TRAYICON && (lparam as u32 & 0xffff) == NIN_BALLOONUSERCLICK {
        CLICKED.store(true, Ordering::Relaxed);
        if let Some(ctx) = CTX.get() {
            ctx.request_repaint();
        }
    }
    unsafe { DefSubclassProc(hwnd, msg, wparam, lparam) }
}

/// The drive whose notification was clicked since the last call.
pub fn take_clicked() -> Option<String> {
    if CLICKED.swap(false, Ordering::Relaxed) { LAST.lock().ok()?.clone() } else { None }
}

fn copy_wide(dst: &mut [u16], s: &str) {
    let n = dst.len() - 1;
    for (i, c) in s.encode_utf16().take(n).enumerate() {
        dst[i] = c;
    }
}

/// Show a balloon / toast on the app's tray icon (owned by tray-icon: its id is
/// private, so the few low ids it hands out are probed with NIM_MODIFY).
fn balloon(hwnd: isize, title: &str, text: &str) -> bool {
    use windows_sys::Win32::UI::Shell::{NIF_INFO, NIIF_WARNING, NIM_MODIFY, NOTIFYICONDATAW, Shell_NotifyIconW};
    let mut nid: NOTIFYICONDATAW = unsafe { std::mem::zeroed() };
    nid.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
    nid.hWnd = hwnd as _;
    nid.uFlags = NIF_INFO;
    nid.dwInfoFlags = NIIF_WARNING;
    copy_wide(&mut nid.szInfoTitle, title);
    copy_wide(&mut nid.szInfo, text);
    for id in 0..64 {
        nid.uID = id;
        if unsafe { Shell_NotifyIconW(NIM_MODIFY, &nid) } != 0 {
            return true;
        }
    }
    false
}

/// Background checker: runs after each load/sync (`kick`) and every 10 minutes,
/// also while the window is hidden in the tray.
pub struct Alerts {
    pub cfg: Arc<Mutex<Config>>,
    wake: Option<Sender<()>>,
    /// Alerts to show in the status bar (always, and when there is no tray icon).
    pub rx: Option<Receiver<Alert>>,
    last_kick: Option<Instant>,
}

impl Default for Alerts {
    fn default() -> Self {
        Self { cfg: Arc::new(Mutex::new(Config::load())), wake: None, rx: None, last_kick: None }
    }
}

impl Alerts {
    pub fn started(&self) -> bool {
        self.wake.is_some()
    }

    /// Start the checker. `tray_hwnd`: tray-icon's window (balloons + click hook).
    pub fn start(&mut self, ctx: &egui::Context, tray_hwnd: Option<isize>) {
        if self.wake.is_some() {
            return;
        }
        let _ = CTX.set(ctx.clone());
        if let Some(h) = tray_hwnd {
            use windows_sys::Win32::UI::Shell::SetWindowSubclass;
            // Runs on the UI thread, which owns the tray window.
            unsafe { SetWindowSubclass(h as _, Some(subclass_proc), 0x5749_5A41, 0) };
        }
        let (wake_tx, wake_rx) = channel::<()>();
        let (tx, rx) = channel();
        let cfg = self.cfg.clone();
        let ctx = ctx.clone();
        std::thread::Builder::new()
            .name("free-space-alerts".into())
            .spawn(move || {
                loop {
                    match wake_rx.recv_timeout(Duration::from_secs(600)) {
                        Ok(()) | Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                        Err(_) => return,
                    }
                    while wake_rx.try_recv().is_ok() {}
                    if !cfg.lock().map(|c| c.enabled).unwrap_or(false) {
                        continue;
                    }
                    let readings: Vec<(String, u64, u64)> = local_drives()
                        .into_iter()
                        .filter_map(|d| crate::tree::disk_space(&d).map(|(t, f)| (d, t, f)))
                        .collect();
                    let today = crate::history::today();
                    let alerts = {
                        let Ok(mut c) = cfg.lock() else { return };
                        let a = evaluate(&mut c, &readings, &today);
                        if !a.is_empty() {
                            c.save();
                        }
                        a
                    };
                    for a in alerts {
                        let title = format!("{} {}", tr("Poco espacio libre en", "Low free space on"), a.drive);
                        let text = format!(
                            "{} {} ({:.1} %) {} {} · {} {} %. {}",
                            tr("Quedan", "Only"),
                            fmt_size(a.free),
                            a.free as f64 / a.total as f64 * 100.0,
                            tr("de", "of"),
                            fmt_size(a.total),
                            tr("umbral", "threshold"),
                            a.pct,
                            tr("Clic para ver qué ocupa espacio.", "Click to see what uses the space."),
                        );
                        if let Some(h) = tray_hwnd {
                            if let Ok(mut l) = LAST.lock() {
                                *l = Some(a.drive.clone());
                            }
                            balloon(h, &title, &text);
                        }
                        let _ = tx.send(a);
                        ctx.request_repaint();
                    }
                }
            })
            .expect("thread");
        self.wake = Some(wake_tx);
        self.rx = Some(rx);
        self.kick(true);
    }

    /// Ask for a check (after a load or sync). Throttled to once a minute.
    pub fn kick(&mut self, force: bool) {
        if !force && self.last_kick.is_some_and(|t| t.elapsed() < Duration::from_secs(60)) {
            return;
        }
        if let Some(w) = &self.wake {
            self.last_kick = Some(Instant::now());
            let _ = w.send(());
        }
    }

    /// Settings block for the Options menu. `drives`: names like "C:".
    pub fn options_ui(&mut self, ui: &mut egui::Ui, drives: &[String]) {
        let Ok(mut c) = self.cfg.lock() else { return };
        let before = c.clone();
        ui.separator();
        ui.label(RichText::new(tr("Alertas de espacio libre", "Free-space alerts")).strong());
        ui.checkbox(&mut c.enabled, tr("Avisar con poco espacio libre", "Warn when free space is low"))
            .on_hover_text(tr(
                "Notificación en la bandeja, como mucho una vez por día y por unidad; clic para abrir esa unidad",
                "Tray notification, at most once a day per drive; click it to open that drive",
            ));
        ui.add_enabled_ui(c.enabled, |ui| {
            ui.horizontal(|ui| {
                ui.label(tr("Umbral general:", "Global threshold:"));
                ui.add(egui::DragValue::new(&mut c.pct).range(1.0..=50.0).speed(0.25).suffix(tr(" % libre", " % free")));
            });
            ui.collapsing(tr("Umbral por unidad", "Per-drive threshold"), |ui| {
                for d in drives {
                    let own = c.drives.iter().position(|(x, _)| x.eq_ignore_ascii_case(d));
                    let mut on = own.is_some();
                    ui.horizontal(|ui| {
                        if ui.checkbox(&mut on, d.as_str()).changed() {
                            if on {
                                let p = c.pct;
                                c.drives.push((d.clone(), p));
                            } else {
                                c.drives.retain(|(x, _)| !x.eq_ignore_ascii_case(d));
                            }
                        }
                        if let Some(i) = c.drives.iter().position(|(x, _)| x.eq_ignore_ascii_case(d)) {
                            ui.add(egui::DragValue::new(&mut c.drives[i].1).range(0.0..=90.0).speed(0.25).suffix(" %"));
                        } else {
                            ui.label(RichText::new(format!("{} %", c.pct)).weak());
                        }
                    });
                }
            });
        });
        if *c != before {
            // A changed threshold may alert again today.
            if c.pct != before.pct || c.drives != before.drives || (c.enabled && !before.enabled) {
                c.notified.clear();
            }
            c.save();
            drop(c);
            self.kick(true);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn threshold_logic() {
        assert!(below(1000, 99, 10.0));
        assert!(!below(1000, 100, 10.0));
        assert!(!below(0, 0, 10.0), "unknown size never alerts");
        assert!(!below(1000, 0, 0.0), "0 % disables the drive");
        assert!(!below(1000, 5000, 100.0), "free is capped at total");
    }

    #[test]
    fn config_roundtrip_and_overrides() {
        let mut c = Config::parse("garbage\nenabled=0\npct=12.5\ndrive=D:,5\ndrive=bad\nnotified=C:,2026-11-18\n");
        assert!(!c.enabled);
        assert_eq!(c.pct, 12.5);
        assert_eq!(c.threshold("d:"), 5.0);
        assert_eq!(c.threshold("C:"), 12.5);
        c.enabled = true;
        assert_eq!(Config::parse(&c.to_text()), c);
        assert_eq!(Config::parse(""), Config::default());
        assert_eq!(Config::default().pct, 10.0);
    }

    #[test]
    fn alerts_once_per_day_per_drive() {
        let mut c = Config { drives: vec![("D:".into(), 2.0)], ..Config::default() };
        let r = vec![("C:".to_string(), 1000, 50), ("D:".to_string(), 1000, 50), ("E:".to_string(), 1000, 500)];
        let a = evaluate(&mut c, &r, "2026-11-19");
        assert_eq!(a.iter().map(|a| a.drive.as_str()).collect::<Vec<_>>(), vec!["C:"], "D: has its own 2 % threshold");
        assert!(evaluate(&mut c, &r, "2026-11-19").is_empty(), "same day: no repeat");
        assert_eq!(evaluate(&mut c, &r, "2026-11-20").len(), 1, "next day: again");
        c.enabled = false;
        assert!(evaluate(&mut c, &r, "2026-11-21").is_empty());
    }
}
