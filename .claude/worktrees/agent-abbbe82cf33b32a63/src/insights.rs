//! "Cleanup" tab: cleanup suggestions, what grew recently, and large old files.
//!
//! Everything works on the in-memory tree (no disk scan). The passes over the
//! tree run on a worker thread with a snapshot (`Arc`) of it that is dropped as
//! soon as the pass ends, so the app's in-place tree updates are only delayed
//! for that long. Results hold node ids, which stay valid until the next full
//! load (the epoch changes and they are discarded).

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::mpsc::{Receiver, TryRecvError, channel};
use std::time::{SystemTime, UNIX_EPOCH};

use eframe::egui::{self, Align, Layout as ELayout, RichText, Sense};
use egui_extras::{Column, TableBuilder};

use crate::app::{Col, Sort, file_icon, fmt_date, fmt_int, fmt_size, folder_icon, open_in_explorer, sort_header};
use crate::dup;
use crate::i18n::tr;
use crate::tree::{ROOT, Tree};

#[cfg(test)]
#[path = "insights_tests.rs"]
mod tests;

/// FILETIME ticks (100 ns) per day.
pub const DAY: u64 = 24 * 3600 * 10_000_000;
const MB: u64 = 1024 * 1024;
/// How many folders / files "What grew" keeps.
const GROWTH_FOLDERS: usize = 300;
const GROWTH_FILES: usize = 2000;

// ================================================================ analysis

/// Kinds of reclaimable space.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, PartialOrd, Ord)]
pub enum Kind {
    NodeModules,
    RustTarget,
    PyCache,
    Gradle,
    BuildOutput,
    BrowserCache,
    UserTemp,
    WindowsTemp,
    WindowsUpdate,
    RecycleBin,
    CrashDump,
    TempFile,
}

impl Kind {
    pub fn label(self) -> &'static str {
        match self {
            Kind::NodeModules => "node_modules",
            Kind::RustTarget => tr("target de Rust", "Rust target"),
            Kind::PyCache => "__pycache__",
            Kind::Gradle => tr("Cachés de Gradle", "Gradle caches"),
            Kind::BuildOutput => tr("Build/dist y cachés de desarrollo", "Build/dist & dev caches"),
            Kind::BrowserCache => tr("Caché de navegadores", "Browser caches"),
            Kind::UserTemp => tr("Temporales del usuario", "User temp folder"),
            Kind::WindowsTemp => tr("Temporales de Windows", "Windows temp folder"),
            Kind::WindowsUpdate => tr("Descargas de Windows Update", "Windows Update downloads"),
            Kind::RecycleBin => tr("Papelera de reciclaje", "Recycle Bin"),
            Kind::CrashDump => tr("Volcados de memoria (.dmp)", "Crash dumps (.dmp)"),
            Kind::TempFile => tr("Archivos .tmp", ".tmp files"),
        }
    }

    pub fn hint(self) -> &'static str {
        match self {
            Kind::NodeModules => tr(
                "Dependencias de Node.js; se reinstalan con npm/yarn/pnpm install.",
                "Node.js dependencies; reinstalled with npm/yarn/pnpm install.",
            ),
            Kind::RustTarget => tr(
                "Compilaciones de Rust (junto a un Cargo.toml); cargo las regenera.",
                "Rust build output (next to a Cargo.toml); cargo rebuilds it.",
            ),
            Kind::PyCache => tr("Bytecode de Python; se regenera solo.", "Python bytecode; regenerated automatically."),
            Kind::Gradle => tr(
                "Cachés de Gradle (del proyecto o ~/.gradle/caches); se vuelven a descargar.",
                "Gradle caches (project or ~/.gradle/caches); downloaded again when needed.",
            ),
            Kind::BuildOutput => tr(
                "build/dist junto a un archivo de proyecto, y cachés como .next, .turbo, .pytest_cache.",
                "build/dist next to a project file, plus caches such as .next, .turbo, .pytest_cache.",
            ),
            Kind::BrowserCache => tr(
                "Caché de Chrome, Edge, Brave, Firefox, Vivaldi y Opera. Cerrá el navegador antes.",
                "Chrome, Edge, Brave, Firefox, Vivaldi and Opera cache. Close the browser first.",
            ),
            Kind::UserTemp => tr(
                "AppData\\Local\\Temp: se envía el contenido, la carpeta queda. Lo que esté en uso se saltea.",
                "AppData\\Local\\Temp: its contents are removed, the folder stays. Files in use are skipped.",
            ),
            Kind::WindowsTemp => tr(
                "Windows\\Temp: se envía el contenido. Suele requerir permisos de administrador.",
                "Windows\\Temp: its contents are removed. Usually needs administrator rights.",
            ),
            Kind::WindowsUpdate => tr(
                "SoftwareDistribution\\Download: actualizaciones ya descargadas. Requiere administrador.",
                "SoftwareDistribution\\Download: already downloaded updates. Needs administrator rights.",
            ),
            Kind::RecycleBin => tr(
                "Lo que ya está en la papelera. Se vacía con el botón «Vaciar papelera».",
                "What is already in the Recycle Bin. Use the \"Empty Recycle Bin\" button.",
            ),
            Kind::CrashDump => tr("Volcados de errores de programas o de Windows.", "Program or Windows crash dumps."),
            Kind::TempFile => tr("Archivos temporales sueltos.", "Loose temporary files."),
        }
    }

    /// Only the folder's contents go to the Recycle Bin: Windows and the
    /// browsers expect the folder itself to exist.
    pub fn contents_only(self) -> bool {
        matches!(self, Kind::UserTemp | Kind::WindowsTemp | Kind::WindowsUpdate | Kind::BrowserCache)
    }

    /// Can be sent to the Recycle Bin from here ($Recycle.Bin is emptied instead).
    pub fn recyclable(self) -> bool {
        self != Kind::RecycleBin
    }
}

/// A reclaimable folder or file of the tree.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Candidate {
    pub kind: Kind,
    pub id: u32,
    pub size: u64,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct KindTotal {
    pub kind: Kind,
    pub count: u64,
    pub size: u64,
}

const BUILD_MARKERS: &[&str] = &[
    "package.json",
    "build.gradle",
    "build.gradle.kts",
    "settings.gradle",
    "settings.gradle.kts",
    "CMakeLists.txt",
    "pyproject.toml",
    "setup.py",
    "pom.xml",
    "angular.json",
    "vite.config.js",
    "vite.config.ts",
];
const GRADLE_MARKERS: &[&str] = &["build.gradle", "build.gradle.kts", "settings.gradle", "settings.gradle.kts", "gradlew"];
const DEV_CACHES: &[&str] = &[
    ".next",
    ".nuxt",
    ".parcel-cache",
    ".turbo",
    ".pytest_cache",
    ".mypy_cache",
    ".ruff_cache",
    ".svelte-kit",
    ".angular",
];
const BROWSER_CACHE_DIRS: &[&str] =
    &["cache", "code cache", "gpucache", "cache2", "shadercache", "grshadercache", "cachestorage", "scriptcache", "dawncache"];
const BROWSER_ROOTS: &[&str] = &[
    "\\google\\chrome\\user data\\",
    "\\microsoft\\edge\\user data\\",
    "\\bravesoftware\\brave-browser\\user data\\",
    "\\vivaldi\\user data\\",
    "\\opera software\\",
    "\\mozilla\\firefox\\profiles\\",
];

fn up(tree: &Tree, id: u32) -> Option<u32> {
    if id == ROOT { None } else { Some(tree.nodes[id as usize].parent) }
}

fn name_is(tree: &Tree, id: Option<u32>, name: &str) -> bool {
    id.is_some_and(|i| i != ROOT && tree.name(i).eq_ignore_ascii_case(name))
}

fn is_drive(tree: &Tree, id: Option<u32>) -> bool {
    id.is_some_and(|i| i != ROOT && tree.nodes[i as usize].parent == ROOT)
}

/// A file named like one of names sits directly inside folder.
fn has_file(tree: &Tree, folder: Option<u32>, names: &[&str]) -> bool {
    let Some(f) = folder else { return false };
    tree.children(f)
        .iter()
        .any(|&c| !tree.nodes[c as usize].is_dir() && names.iter().any(|n| tree.name(c).eq_ignore_ascii_case(n)))
}

/// Kind of a reclaimable folder, from its name and its surroundings.
pub fn classify_dir(tree: &Tree, id: u32) -> Option<Kind> {
    if id == ROOT {
        return None;
    }
    let name = tree.name(id);
    let is = |s: &str| name.eq_ignore_ascii_case(s);
    let parent = up(tree, id);
    let grand = parent.and_then(|p| up(tree, p));
    if is("node_modules") {
        return Some(Kind::NodeModules);
    }
    if is("__pycache__") {
        return Some(Kind::PyCache);
    }
    if is("target") {
        if has_file(tree, parent, &["Cargo.toml"]) {
            return Some(Kind::RustTarget);
        }
        if has_file(tree, parent, &["pom.xml"]) {
            return Some(Kind::BuildOutput);
        }
        return None;
    }
    if is(".gradle") {
        // ~/.gradle also holds settings (gradle.properties): only its caches are suggested.
        return has_file(tree, parent, GRADLE_MARKERS).then_some(Kind::Gradle);
    }
    if is("caches") && name_is(tree, parent, ".gradle") {
        return Some(Kind::Gradle);
    }
    if (is("build") || is("dist")) && has_file(tree, parent, BUILD_MARKERS) {
        return Some(Kind::BuildOutput);
    }
    if DEV_CACHES.iter().any(|c| is(c)) {
        return Some(Kind::BuildOutput);
    }
    if is("Temp") {
        if name_is(tree, parent, "Local") && name_is(tree, grand, "AppData") {
            return Some(Kind::UserTemp);
        }
        if name_is(tree, parent, "Windows") && is_drive(tree, grand) {
            return Some(Kind::WindowsTemp);
        }
        return None;
    }
    if is("Download") && name_is(tree, parent, "SoftwareDistribution") && name_is(tree, grand, "Windows") {
        return Some(Kind::WindowsUpdate);
    }
    if is("$Recycle.Bin") && is_drive(tree, parent) {
        return Some(Kind::RecycleBin);
    }
    if BROWSER_CACHE_DIRS.iter().any(|c| is(c)) {
        let path = tree.full_path(id).to_lowercase();
        if path.contains("\\appdata\\local\\") && BROWSER_ROOTS.iter().any(|r| path.contains(r)) {
            return Some(Kind::BrowserCache);
        }
    }
    None
}

/// Every reclaimable item under root, biggest first. Matched folders are not
/// looked into, so nothing is counted twice.
pub fn find_cleanup(tree: &Tree, root: u32) -> Vec<Candidate> {
    let ext_kind: Vec<Option<Kind>> = tree
        .exts
        .iter()
        .map(|e| match e.name.as_str() {
            "dmp" | "mdmp" | "hdmp" => Some(Kind::CrashDump),
            "tmp" => Some(Kind::TempFile),
            _ => None,
        })
        .collect();
    let mut out = Vec::new();
    let mut stack = vec![root];
    while let Some(id) = stack.pop() {
        let n = &tree.nodes[id as usize];
        if n.is_dir() {
            if let Some(kind) = classify_dir(tree, id) {
                if n.size > 0 {
                    out.push(Candidate { kind, id, size: n.size });
                }
                continue;
            }
            stack.extend_from_slice(tree.children(id));
        } else if let Some(kind) = ext_kind.get(n.ext as usize).copied().flatten()
            && n.size > 0
        {
            out.push(Candidate { kind, id, size: n.size });
        }
    }
    out.sort_by(|a, b| b.size.cmp(&a.size).then(a.id.cmp(&b.id)));
    out
}

/// Totals per kind, biggest first.
pub fn summarize(cands: &[Candidate]) -> Vec<KindTotal> {
    let mut acc: HashMap<Kind, (u64, u64)> = HashMap::new();
    for c in cands {
        let e = acc.entry(c.kind).or_default();
        e.0 += 1;
        e.1 += c.size;
    }
    let mut v: Vec<KindTotal> = acc.into_iter().map(|(kind, (count, size))| KindTotal { kind, count, size }).collect();
    v.sort_by(|a, b| b.size.cmp(&a.size).then(a.kind.cmp(&b.kind)));
    v
}

/// Paths to send to the Recycle Bin for these candidates: the item itself, or
/// its contents for system folders. The Recycle Bin itself is never included.
pub fn recycle_paths(tree: &Tree, cands: &[Candidate]) -> Vec<String> {
    let mut out = Vec::new();
    for c in cands.iter().filter(|c| c.kind.recyclable()) {
        if c.kind.contents_only() && tree.nodes[c.id as usize].is_dir() {
            out.extend(tree.children(c.id).iter().map(|&k| tree.full_path(k)));
        } else {
            out.push(tree.full_path(c.id));
        }
    }
    out
}

/// A folder with recently modified files inside.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct FolderGrowth {
    pub id: u32,
    pub bytes: u64,
    pub files: u32,
}

#[derive(Default, Debug)]
pub struct Growth {
    /// Bytes of all files modified since the threshold.
    pub total: u64,
    pub count: u64,
    /// Folders by recent bytes, biggest first. Folders whose recent bytes sit
    /// almost entirely (≥ 90 %) in one subfolder are left out in favor of it.
    pub folders: Vec<FolderGrowth>,
    /// Recent files, biggest first.
    pub files: Vec<u32>,
}

/// Files under root modified at or after since (FILETIME), aggregated up the tree.
pub fn growth(tree: &Tree, root: u32, since: u64, folder_limit: usize, file_limit: usize) -> Growth {
    let mut recent: Vec<(u64, u32)> = Vec::new();
    let mut stack = vec![root];
    while let Some(id) = stack.pop() {
        let n = &tree.nodes[id as usize];
        if n.is_dir() {
            stack.extend_from_slice(tree.children(id));
        } else if n.modified >= since && n.modified != 0 {
            recent.push((n.size, id));
        }
    }
    let mut g = Growth { total: recent.iter().map(|r| r.0).sum(), count: recent.len() as u64, ..Default::default() };

    // Recent bytes and files of every folder between the files and root.
    let mut agg: HashMap<u32, (u64, u32)> = HashMap::new();
    for &(size, id) in &recent {
        if id == root {
            continue;
        }
        let mut p = tree.nodes[id as usize].parent;
        loop {
            let e = agg.entry(p).or_default();
            e.0 += size;
            e.1 += 1;
            if p == root || p == ROOT {
                break;
            }
            p = tree.nodes[p as usize].parent;
        }
    }
    let mut max_child: HashMap<u32, u64> = HashMap::new();
    for (&f, &(b, _)) in &agg {
        if f != root && f != ROOT {
            let m = max_child.entry(tree.nodes[f as usize].parent).or_default();
            *m = (*m).max(b);
        }
    }
    g.folders = agg
        .iter()
        .filter(|&(&f, &(b, _))| {
            let m = max_child.get(&f).copied().unwrap_or(0) as u128;
            f != ROOT && b > 0 && m * 10 < b as u128 * 9
        })
        .map(|(&id, &(bytes, files))| FolderGrowth { id, bytes, files })
        .collect();
    g.folders.sort_by(|a, b| b.bytes.cmp(&a.bytes).then(a.id.cmp(&b.id)));
    g.folders.truncate(folder_limit);
    recent.sort_unstable_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
    g.files = recent.into_iter().take(file_limit).map(|r| r.1).collect();
    g
}

/// Files under root of at least min_size bytes last modified before before
/// (FILETIME), biggest first. Files without a date are left out.
pub fn large_old(tree: &Tree, root: u32, min_size: u64, before: u64) -> Vec<u32> {
    let mut out: Vec<(u64, u32)> = Vec::new();
    let mut stack = vec![root];
    while let Some(id) = stack.pop() {
        let n = &tree.nodes[id as usize];
        if n.is_dir() {
            stack.extend_from_slice(tree.children(id));
        } else if n.size >= min_size && n.modified != 0 && n.modified < before {
            out.push((n.size, id));
        }
    }
    out.sort_unstable_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
    out.into_iter().map(|r| r.1).collect()
}

// ---------------------------------------------------------------- time

/// Current time as a FILETIME (UTC, 100 ns since 1601).
pub fn now_ft() -> u64 {
    let d = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default();
    (d.as_nanos() / 100) as u64 + 116_444_736_000_000_000
}

/// Today's local midnight as a FILETIME.
fn local_midnight_ft() -> Option<u64> {
    use windows_sys::Win32::Foundation::{FILETIME, SYSTEMTIME};
    use windows_sys::Win32::System::SystemInformation::GetLocalTime;
    use windows_sys::Win32::System::Time::{SystemTimeToFileTime, TzSpecificLocalTimeToSystemTime};
    unsafe {
        let mut loc: SYSTEMTIME = std::mem::zeroed();
        GetLocalTime(&mut loc);
        loc.wHour = 0;
        loc.wMinute = 0;
        loc.wSecond = 0;
        loc.wMilliseconds = 0;
        let mut utc: SYSTEMTIME = std::mem::zeroed();
        if TzSpecificLocalTimeToSystemTime(std::ptr::null(), &loc, &mut utc) == 0 {
            return None;
        }
        let mut ft: FILETIME = std::mem::zeroed();
        if SystemTimeToFileTime(&utc, &mut ft) == 0 {
            return None;
        }
        Some(((ft.dwHighDateTime as u64) << 32) | ft.dwLowDateTime as u64)
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Period {
    Today,
    Week,
    Month,
}

/// Start of a period ending now.
pub fn period_start(p: Period, now: u64) -> u64 {
    match p {
        Period::Today => local_midnight_ft().filter(|&m| m <= now).unwrap_or(now.saturating_sub(DAY)),
        Period::Week => now.saturating_sub(7 * DAY),
        Period::Month => now.saturating_sub(30 * DAY),
    }
}

/// now minus years (of 365.25 days).
pub fn years_before(now: u64, years: u32) -> u64 {
    now.saturating_sub(years as u64 * 36525 * DAY / 100)
}

// ================================================================ UI

/// What the app must do after a frame of this tab.
pub enum Action {
    /// Select the node in the tree view (switching to it).
    Reveal(u32),
    /// Open the app's right-click menu for the node.
    Menu(u32),
    /// The disk changed: reload the index.
    Reload,
    Info(String),
    Error(String),
}

#[derive(PartialEq, Eq, Clone, Copy, Debug)]
enum Section {
    Cleanup,
    Growth,
    Old,
}

#[derive(PartialEq, Eq, Clone, Copy, Debug)]
struct Key {
    section: Section,
    root: u32,
    epoch: u64,
    revision: u64,
    a: u64,
    b: u64,
}

struct CleanupData {
    cands: Vec<Candidate>,
    totals: Vec<KindTotal>,
}

enum Payload {
    Cleanup(CleanupData),
    Growth(Growth),
    Old(Vec<u32>),
}

struct Done<T> {
    key: Key,
    data: T,
}

enum Confirm {
    Recycle { paths: Vec<String>, size: u64, totals: Vec<KindTotal> },
    EmptyBin { drives: Vec<String>, size: u64 },
}

enum OpDone {
    Recycled(Vec<String>, Result<bool, String>),
    Emptied(Result<(), String>),
}

#[derive(PartialEq, Eq, PartialOrd, Ord)]
enum SortVal {
    Num(u64),
    Text(String),
}

/// Display order of a table, re-sorted only when the data or the sort changes.
#[derive(Default)]
struct View {
    sort: Sort,
    order: Vec<usize>,
    key: Option<(Sort, u64, u32)>,
}

impl View {
    fn update(&mut self, gen_: u64, filter: u32, base: impl FnOnce() -> Vec<usize>, val: impl Fn(usize, Col) -> SortVal) {
        let k = (self.sort, gen_, filter);
        if self.key == Some(k) {
            return;
        }
        let mut order = base();
        if let Some((col, asc)) = self.sort {
            order.sort_by_cached_key(|&i| val(i, col));
            if !asc {
                order.reverse();
            }
        }
        self.order = order;
        self.key = Some(k);
    }
}

fn node_val(tree: &Tree, id: u32, col: Col) -> SortVal {
    let n = &tree.nodes[id as usize];
    match col {
        Col::Name => SortVal::Text(tree.name(id).to_lowercase()),
        Col::Modified => SortVal::Num(n.modified),
        Col::Path => SortVal::Text(tree.full_path(n.parent).to_lowercase()),
        _ => SortVal::Num(n.size),
    }
}

pub struct Insights {
    section: Section,
    period: Period,
    min_mb: u64,
    years: u32,
    epoch: u64,
    job: Option<(Key, Receiver<Payload>)>,
    /// Bumped whenever results change (invalidates the table orders).
    gen_: u64,
    cleanup: Option<Done<CleanupData>>,
    growth: Option<Done<Growth>>,
    old: Option<Done<Vec<u32>>>,
    filter: Option<Kind>,
    marked: HashSet<u32>,
    sel: Option<u32>,
    kinds_sort: Sort,
    /// 0 cleanup items, 1 growth folders, 2 growth files, 3 large & old.
    views: [View; 4],
    confirm: Option<Confirm>,
    op_rx: Option<Receiver<OpDone>>,
}

impl Default for Insights {
    fn default() -> Self {
        Self {
            section: Section::Cleanup,
            period: Period::Week,
            min_mb: 100,
            years: 2,
            epoch: u64::MAX,
            job: None,
            gen_: 0,
            cleanup: None,
            growth: None,
            old: None,
            filter: None,
            marked: HashSet::new(),
            sel: None,
            kinds_sort: None,
            views: Default::default(),
            confirm: None,
            op_rx: None,
        }
    }
}

fn right(ui: &mut egui::Ui, s: String) {
    ui.with_layout(ELayout::right_to_left(Align::Center), |ui| {
        ui.label(s);
    });
}

fn waiting(ui: &mut egui::Ui) {
    ui.horizontal(|ui| {
        ui.spinner();
        ui.label(tr("Analizando el índice…", "Analyzing the index…"));
    });
}

impl Insights {
    /// Draw the tab. tree is the current tree, root the folder in view; epoch
    /// and revision come from the app (full loads / any change to the tree).
    pub fn ui(&mut self, ui: &mut egui::Ui, ctx: &egui::Context, tree: &Arc<Tree>, root: u32, epoch: u64, revision: u64) -> Vec<Action> {
        let mut acts = Vec::new();
        self.poll(epoch, &mut acts);

        ui.horizontal(|ui| {
            ui.selectable_value(&mut self.section, Section::Cleanup, tr("Sugerencias de limpieza", "Cleanup suggestions"));
            ui.selectable_value(&mut self.section, Section::Growth, tr("Qué creció", "What grew"));
            ui.selectable_value(&mut self.section, Section::Old, tr("Grandes y viejos", "Large & old"));
            ui.separator();
            let place = if root == ROOT { tr("Este equipo", "This PC").to_string() } else { tree.full_path(root) };
            ui.label(RichText::new(format!("{} {place}", tr("En:", "In:"))).weak());
            if ui.small_button("⟳").on_hover_text(tr("Volver a analizar", "Analyze again")).clicked() {
                match self.section {
                    Section::Cleanup => self.cleanup = None,
                    Section::Growth => self.growth = None,
                    Section::Old => self.old = None,
                }
            }
            if self.job.is_some() || self.op_rx.is_some() {
                ui.spinner();
            }
        });
        self.selection_bar(ui, tree, &mut acts);
        ui.separator();

        let key = self.wanted(root, epoch, revision);
        self.ensure(ctx, tree, key);
        match self.section {
            Section::Cleanup => self.cleanup_ui(ui, tree, &mut acts),
            Section::Growth => self.growth_ui(ui, tree, &mut acts),
            Section::Old => self.old_ui(ui, tree, &mut acts),
        }
        self.confirm_ui(ctx, &mut acts);
        acts
    }

    fn wanted(&self, root: u32, epoch: u64, revision: u64) -> Key {
        let (a, b) = match self.section {
            Section::Cleanup => (0, 0),
            Section::Growth => (self.period as u64, 0),
            Section::Old => (self.min_mb, self.years as u64),
        };
        Key { section: self.section, root, epoch, revision, a, b }
    }

    /// Start a pass for key unless its result is current or a pass is running.
    fn ensure(&mut self, ctx: &egui::Context, tree: &Arc<Tree>, key: Key) {
        let have = match key.section {
            Section::Cleanup => self.cleanup.as_ref().map(|d| d.key),
            Section::Growth => self.growth.as_ref().map(|d| d.key),
            Section::Old => self.old.as_ref().map(|d| d.key),
        };
        if have == Some(key) || self.job.is_some() || key.root as usize >= tree.nodes.len() {
            return;
        }
        let (tx, rx) = channel();
        let t = Arc::clone(tree);
        let ctx = ctx.clone();
        let (period, min_size, years) = (self.period, self.min_mb.saturating_mul(MB), self.years);
        std::thread::spawn(move || {
            let p = match key.section {
                Section::Cleanup => {
                    let cands = find_cleanup(&t, key.root);
                    let totals = summarize(&cands);
                    Payload::Cleanup(CleanupData { cands, totals })
                }
                Section::Growth => {
                    let since = period_start(period, now_ft());
                    Payload::Growth(growth(&t, key.root, since, GROWTH_FOLDERS, GROWTH_FILES))
                }
                Section::Old => Payload::Old(large_old(&t, key.root, min_size, years_before(now_ft(), years))),
            };
            drop(t); // release the snapshot before the UI sees the result
            let _ = tx.send(p);
            ctx.request_repaint();
        });
        self.job = Some((key, rx));
    }

    fn poll(&mut self, epoch: u64, acts: &mut Vec<Action>) {
        if epoch != self.epoch {
            // A full load renumbers the nodes: drop everything tied to old ids.
            self.epoch = epoch;
            self.cleanup = None;
            self.growth = None;
            self.old = None;
            self.marked.clear();
            self.sel = None;
            self.confirm = None;
        }
        if let Some((key, rx)) = &self.job {
            let key = *key;
            match rx.try_recv() {
                Ok(p) => {
                    self.job = None;
                    if key.epoch == epoch {
                        self.gen_ += 1;
                        match p {
                            Payload::Cleanup(data) => {
                                let ids: HashSet<u32> = data.cands.iter().map(|c| c.id).collect();
                                self.marked.retain(|id| ids.contains(id));
                                self.cleanup = Some(Done { key, data });
                            }
                            Payload::Growth(data) => self.growth = Some(Done { key, data }),
                            Payload::Old(data) => self.old = Some(Done { key, data }),
                        }
                    }
                }
                Err(TryRecvError::Disconnected) => self.job = None,
                Err(TryRecvError::Empty) => {}
            }
        }
        if let Some(rx) = &self.op_rx
            && let Ok(r) = rx.try_recv()
        {
            self.op_rx = None;
            match r {
                OpDone::Recycled(paths, res) => {
                    if let Err(e) = res {
                        acts.push(Action::Error(e));
                    }
                    let gone = paths.iter().filter(|p| !std::path::Path::new(p.as_str()).exists()).count();
                    acts.push(Action::Info(format!(
                        "{} {}",
                        fmt_int(gone as u64),
                        tr("elementos enviados a la papelera", "items sent to the Recycle Bin")
                    )));
                    if gone > 0 {
                        self.marked.clear();
                        acts.push(Action::Reload);
                    }
                }
                OpDone::Emptied(res) => {
                    match res {
                        Ok(()) => acts.push(Action::Info(tr("Papelera de reciclaje vaciada", "Recycle Bin emptied").into())),
                        Err(e) => acts.push(Action::Error(e)),
                    }
                    acts.push(Action::Reload);
                }
            }
        }
    }

    /// Selected item with its actions (or a usage hint).
    fn selection_bar(&mut self, ui: &mut egui::Ui, tree: &Tree, acts: &mut Vec<Action>) {
        if self.sel.is_some_and(|id| id as usize >= tree.nodes.len()) {
            self.sel = None;
        }
        ui.horizontal(|ui| match self.sel {
            Some(id) => {
                let path = tree.full_path(id);
                if ui.button(tr("Abrir en el Explorador", "Open in Explorer")).clicked() {
                    open_in_explorer(&path);
                }
                if ui.button(tr("Mostrar en el árbol", "Show in tree")).clicked() {
                    acts.push(Action::Reveal(id));
                }
                ui.label(RichText::new(path).strong());
            }
            None => {
                ui.label(
                    RichText::new(tr(
                        "Clic: seleccionar · doble clic: mostrar en el árbol · clic derecho: menú",
                        "Click: select · double-click: show in tree · right-click: menu",
                    ))
                    .weak(),
                );
            }
        });
    }

    // ------------------------------------------------------------ cleanup

    fn cleanup_ui(&mut self, ui: &mut egui::Ui, tree: &Tree, acts: &mut Vec<Action>) {
        let Some(done) = self.cleanup.take() else {
            waiting(ui);
            return;
        };
        let data = &done.data;
        let total: u64 = data.totals.iter().map(|t| t.size).sum();
        let count: u64 = data.totals.iter().map(|t| t.count).sum();
        // Marked count/size per kind.
        let mut marked_by: HashMap<Kind, (u64, u64)> = HashMap::new();
        if !self.marked.is_empty() {
            for c in data.cands.iter().filter(|c| self.marked.contains(&c.id)) {
                let e = marked_by.entry(c.kind).or_default();
                e.0 += 1;
                e.1 += c.size;
            }
        }
        let (marked_n, marked_size) = marked_by.values().fold((0, 0), |a, v| (a.0 + v.0, a.1 + v.1));

        ui.label(format!(
            "{} {} {} {}",
            tr("Recuperable:", "Reclaimable:"),
            fmt_size(total),
            tr("en", "in"),
            format_args!("{} {}", fmt_int(count), tr("elementos", "items")),
        ));
        if data.cands.is_empty() {
            ui.label(RichText::new(tr("No se encontró nada para limpiar en esta vista.", "Nothing to clean up in this view.")).weak());
        }

        // --- per-kind totals
        let mut sort = self.kinds_sort;
        let mut order: Vec<usize> = (0..data.totals.len()).collect();
        if let Some((col, asc)) = sort {
            order.sort_by(|&a, &b| {
                let (x, y) = (&data.totals[a], &data.totals[b]);
                let o = match col {
                    Col::Name => x.kind.label().to_lowercase().cmp(&y.kind.label().to_lowercase()),
                    Col::Count => x.count.cmp(&y.count),
                    _ => x.size.cmp(&y.size),
                };
                if asc { o } else { o.reverse() }
            });
        }
        let mut pick_kind = None;
        let mut mark_kind: Option<(Kind, bool)> = None;
        ui.push_id("ins_kinds", |ui| {
            TableBuilder::new(ui)
                .striped(true)
                .resizable(true)
                .vscroll(false)
                .sense(Sense::click())
                .cell_layout(ELayout::left_to_right(Align::Center))
                .column(Column::initial(270.0).range(140.0..=500.0).clip(true))
                .column(Column::initial(80.0).range(50.0..=140.0).clip(true))
                .column(Column::initial(90.0).range(60.0..=160.0).clip(true))
                .column(Column::remainder().at_least(100.0).clip(true))
                .header(22.0, |mut h| {
                    h.col(|ui| {
                        sort_header(ui, tr("Tipo", "Kind"), Col::Name, &mut sort);
                    });
                    h.col(|ui| {
                        sort_header(ui, tr("Elementos", "Items"), Col::Count, &mut sort);
                    });
                    h.col(|ui| {
                        sort_header(ui, tr("Tamaño", "Size"), Col::Size, &mut sort);
                    });
                    h.col(|ui| {
                        ui.strong(tr("Qué es", "What it is"));
                    });
                })
                .body(|body| {
                    body.rows(20.0, order.len(), |mut row| {
                        let t = data.totals[order[row.index()]];
                        row.set_selected(self.filter == Some(t.kind));
                        row.col(|ui| {
                            let m = marked_by.get(&t.kind).map_or(0, |v| v.0);
                            let mut all = m == t.count;
                            let cb = egui::Checkbox::new(&mut all, "").indeterminate(m > 0 && m < t.count);
                            if t.kind.recyclable() {
                                if ui.add(cb).on_hover_text(tr("Marcar / desmarcar todos", "Mark / unmark all")).changed() {
                                    mark_kind = Some((t.kind, m < t.count));
                                }
                            } else {
                                ui.add_enabled(false, cb);
                            }
                            ui.label(t.kind.label());
                        });
                        row.col(|ui| right(ui, fmt_int(t.count)));
                        row.col(|ui| right(ui, fmt_size(t.size)));
                        row.col(|ui| {
                            ui.label(RichText::new(t.kind.hint()).weak()).on_hover_text(t.kind.hint());
                        });
                        if row.response().clicked() {
                            pick_kind = Some(t.kind);
                        }
                    });
                });
        });
        self.kinds_sort = sort;
        if let Some(k) = pick_kind {
            self.filter = if self.filter == Some(k) { None } else { Some(k) };
        }
        if let Some((k, on)) = mark_kind {
            for c in data.cands.iter().filter(|c| c.kind == k) {
                if on {
                    self.marked.insert(c.id);
                } else {
                    self.marked.remove(&c.id);
                }
            }
        }

        // --- actions
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            match self.filter {
                Some(k) => {
                    ui.label(format!("{} {}", tr("Mostrando:", "Showing:"), k.label()));
                    if ui.small_button(tr("✖ ver todos", "✖ show all")).clicked() {
                        self.filter = None;
                    }
                }
                None => {
                    ui.label(tr("Elementos más grandes", "Biggest items"));
                }
            }
            ui.separator();
            let label = format!("{} ({}, {})", tr("Enviar a la papelera", "Send to Recycle Bin"), fmt_int(marked_n), fmt_size(marked_size));
            if ui.add_enabled(marked_n > 0 && self.op_rx.is_none(), egui::Button::new(label)).clicked() {
                let picked: Vec<Candidate> = data.cands.iter().copied().filter(|c| self.marked.contains(&c.id)).collect();
                self.confirm = Some(Confirm::Recycle {
                    paths: recycle_paths(tree, &picked),
                    size: marked_size,
                    totals: summarize(&picked),
                });
            }
            if marked_n > 0 && ui.small_button(tr("Desmarcar todo", "Unmark all")).clicked() {
                self.marked.clear();
            }
            let bins: Vec<&Candidate> = data.cands.iter().filter(|c| c.kind == Kind::RecycleBin).collect();
            if !bins.is_empty() {
                let size: u64 = bins.iter().map(|c| c.size).sum();
                let label = format!("{} ({})…", tr("Vaciar papelera", "Empty Recycle Bin"), fmt_size(size));
                if ui.add_enabled(self.op_rx.is_none(), egui::Button::new(label)).clicked() {
                    let drives = bins.iter().filter_map(|c| tree.drive_of(c.id)).map(|d| tree.name(d).to_string()).collect();
                    self.confirm = Some(Confirm::EmptyBin { drives, size });
                }
            }
        });

        // --- items
        let filter = self.filter;
        let fcode = filter.map_or(u32::MAX, |k| k as u32);
        let mut view = std::mem::take(&mut self.views[0]);
        view.update(
            self.gen_,
            fcode,
            || (0..data.cands.len()).filter(|&i| filter.is_none_or(|k| data.cands[i].kind == k)).collect(),
            |i, col| node_val(tree, data.cands[i].id, col),
        );
        let mut sort = view.sort;
        let mut toggle = None;
        let mut click = None;
        ui.push_id("ins_items", |ui| {
            TableBuilder::new(ui)
                .striped(true)
                .resizable(true)
                .sense(Sense::click())
                .cell_layout(ELayout::left_to_right(Align::Center))
                .column(Column::initial(260.0).range(120.0..=600.0).clip(true))
                .column(Column::initial(150.0).range(60.0..=300.0).clip(true))
                .column(Column::initial(85.0).range(55.0..=140.0).clip(true))
                .column(Column::initial(112.0).range(70.0..=160.0).clip(true))
                .column(Column::remainder().at_least(100.0).clip(true))
                .header(22.0, |mut h| {
                    h.col(|ui| {
                        sort_header(ui, tr("Nombre", "Name"), Col::Name, &mut sort);
                    });
                    h.col(|ui| {
                        ui.strong(tr("Tipo", "Kind"));
                    });
                    h.col(|ui| {
                        sort_header(ui, tr("Tamaño", "Size"), Col::Size, &mut sort);
                    });
                    h.col(|ui| {
                        sort_header(ui, tr("Modificado", "Modified"), Col::Modified, &mut sort);
                    });
                    h.col(|ui| {
                        sort_header(ui, tr("Carpeta", "Folder"), Col::Path, &mut sort);
                    });
                })
                .body(|body| {
                    body.rows(20.0, view.order.len(), |mut row| {
                        let c = data.cands[view.order[row.index()]];
                        let n = &tree.nodes[c.id as usize];
                        row.set_selected(self.sel == Some(c.id));
                        row.col(|ui| {
                            let mut m = self.marked.contains(&c.id);
                            if c.kind.recyclable() {
                                if ui
                                    .checkbox(&mut m, "")
                                    .on_hover_text(tr("Marcar para enviar a la papelera", "Mark to send to the Recycle Bin"))
                                    .changed()
                                {
                                    toggle = Some(c.id);
                                }
                            } else {
                                ui.add_enabled(false, egui::Checkbox::new(&mut m, ""));
                            }
                            if n.is_dir() {
                                folder_icon(ui);
                            } else {
                                file_icon(ui, tree.exts[n.ext as usize].color);
                            }
                            ui.label(tree.name(c.id));
                        });
                        row.col(|ui| {
                            ui.label(c.kind.label());
                        });
                        row.col(|ui| right(ui, fmt_size(c.size)));
                        row.col(|ui| {
                            ui.label(fmt_date(n.modified));
                        });
                        row.col(|ui| {
                            ui.label(tree.full_path(n.parent));
                        });
                        click = row_click(&row.response(), c.id).or(click);
                    });
                });
        });
        view.sort = sort;
        self.views[0] = view;
        if let Some(id) = toggle
            && !self.marked.remove(&id)
        {
            self.marked.insert(id);
        }
        self.handle_click(click, acts);
        self.cleanup = Some(done);
    }

    fn handle_click(&mut self, click: Option<(u32, Click)>, acts: &mut Vec<Action>) {
        match click {
            Some((id, Click::Select)) => self.sel = Some(id),
            Some((id, Click::Reveal)) => {
                self.sel = Some(id);
                acts.push(Action::Reveal(id));
            }
            Some((id, Click::Menu)) => {
                self.sel = Some(id);
                acts.push(Action::Menu(id));
            }
            None => {}
        }
    }

    fn confirm_ui(&mut self, ctx: &egui::Context, _acts: &mut Vec<Action>) {
        let Some(confirm) = &self.confirm else { return };
        let mut go = false;
        let mut cancel = false;
        let resp = egui::Modal::new(egui::Id::unique("insights_confirm")).show(ctx, |ui| {
            ui.set_max_width(480.0);
            match confirm {
                Confirm::Recycle { paths, size, totals } => {
                    ui.heading(tr("Enviar a la papelera", "Send to Recycle Bin"));
                    ui.label(format!(
                        "{} {} ({}).",
                        fmt_int(paths.len() as u64),
                        tr("elementos irán a la papelera de reciclaje", "items will be moved to the Recycle Bin"),
                        fmt_size(*size)
                    ));
                    for t in totals {
                        ui.label(format!("• {}: {} ({})", t.kind.label(), fmt_int(t.count), fmt_size(t.size)));
                    }
                    ui.label(
                        RichText::new(tr(
                            "De Temp, cachés de navegador y Windows Update se envía solo el contenido. Lo que esté en uso o requiera administrador puede fallar. Si una unidad no tiene papelera, Windows pedirá confirmación antes de borrar definitivamente.",
                            "For Temp, browser caches and Windows Update only the contents are moved. Files in use or needing administrator rights may fail. If a drive has no Recycle Bin, Windows will ask before deleting permanently.",
                        ))
                        .weak(),
                    );
                    if totals.iter().any(|t| t.kind == Kind::BrowserCache) {
                        ui.colored_label(ui.visuals().warn_fg_color, tr("Cerrá los navegadores antes de continuar.", "Close your browsers before continuing."));
                    }
                }
                Confirm::EmptyBin { drives, size } => {
                    ui.heading(tr("Vaciar papelera de reciclaje", "Empty Recycle Bin"));
                    ui.label(format!(
                        "{} {} ({}).",
                        tr("Se borrará DEFINITIVAMENTE el contenido de la papelera en", "This PERMANENTLY deletes the Recycle Bin contents on"),
                        drives.join(", "),
                        fmt_size(*size)
                    ));
                    ui.colored_label(ui.visuals().error_fg_color, tr("No se puede deshacer.", "This cannot be undone."));
                }
            }
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                let label = match confirm {
                    Confirm::Recycle { .. } => tr("Enviar a la papelera", "Send to Recycle Bin"),
                    Confirm::EmptyBin { .. } => tr("Vaciar papelera", "Empty Recycle Bin"),
                };
                if ui.button(label).clicked() {
                    go = true;
                }
                if ui.button(tr("Cancelar", "Cancel")).clicked() {
                    cancel = true;
                }
            });
        });
        if resp.should_close() || cancel {
            self.confirm = None;
        }
        if !go {
            return;
        }
        let Some(confirm) = self.confirm.take() else { return };
        let (tx, rx) = channel();
        let ctx = ctx.clone();
        match confirm {
            Confirm::Recycle { paths, .. } => {
                std::thread::spawn(move || {
                    let r = dup::recycle(&paths);
                    let _ = tx.send(OpDone::Recycled(paths, r));
                    ctx.request_repaint();
                });
            }
            Confirm::EmptyBin { drives, .. } => {
                std::thread::spawn(move || {
                    let _ = tx.send(OpDone::Emptied(empty_recycle_bins(&drives)));
                    ctx.request_repaint();
                });
            }
        }
        self.op_rx = Some(rx);
    }

    // ------------------------------------------------------------ what grew

    fn growth_ui(&mut self, ui: &mut egui::Ui, tree: &Tree, acts: &mut Vec<Action>) {
        ui.horizontal(|ui| {
            ui.label(tr("Período:", "Period:"));
            ui.selectable_value(&mut self.period, Period::Today, tr("Hoy", "Today"));
            ui.selectable_value(&mut self.period, Period::Week, tr("Últimos 7 días", "Last 7 days"));
            ui.selectable_value(&mut self.period, Period::Month, tr("Últimos 30 días", "Last 30 days"));
            if let Some(d) = &self.growth {
                ui.separator();
                ui.label(RichText::new(format!(
                    "{} {} · {} {}",
                    fmt_size(d.data.total),
                    tr("nuevos o modificados", "new or modified"),
                    fmt_int(d.data.count),
                    tr("archivos", "files")
                ))
                .strong());
            }
        });
        ui.label(
            RichText::new(tr(
                "Según la fecha de modificación: las copias que conservan una fecha vieja no aparecen y lo borrado no se descuenta. Una carpeta cuyo crecimiento está casi todo en una subcarpeta se muestra como esa subcarpeta.",
                "Based on the modified date: copies that keep an old date don't show up and deleted files aren't subtracted. A folder whose growth is almost all in one subfolder is shown as that subfolder.",
            ))
            .weak(),
        );
        let Some(done) = self.growth.take() else {
            waiting(ui);
            return;
        };
        let g = &done.data;
        let mut click = None;
        let w = ui.available_width().max(200.0);
        let gen_ = self.gen_;
        let sel = self.sel;
        let mut fview = std::mem::take(&mut self.views[1]);
        let mut files_view = std::mem::take(&mut self.views[2]);
        egui::Panel::left("ins_growth_folders")
            .resizable(true)
            .default_size(w * 0.45)
            .size_range(w * 0.15..=w * 0.85)
            .show(ui, |ui| {
                fview.update(
                    gen_,
                    0,
                    || (0..g.folders.len()).collect(),
                    |i, col| {
                        let f = g.folders[i];
                        match col {
                            Col::Path => SortVal::Text(tree.full_path(f.id).to_lowercase()),
                            Col::Files => SortVal::Num(f.files as u64),
                            _ => SortVal::Num(f.bytes),
                        }
                    },
                );
                let mut sort = fview.sort;
                ui.push_id("ins_growth_folders_t", |ui| {
                    TableBuilder::new(ui)
                        .striped(true)
                        .resizable(true)
                        .sense(Sense::click())
                        .cell_layout(ELayout::left_to_right(Align::Center))
                        .column(Column::remainder().at_least(140.0).clip(true))
                        .column(Column::initial(85.0).range(55.0..=140.0).clip(true))
                        .column(Column::initial(70.0).range(45.0..=120.0).clip(true))
                        .header(22.0, |mut h| {
                            h.col(|ui| {
                                sort_header(ui, tr("Carpeta", "Folder"), Col::Path, &mut sort);
                            });
                            h.col(|ui| {
                                sort_header(ui, tr("Reciente", "Recent"), Col::Size, &mut sort);
                            });
                            h.col(|ui| {
                                sort_header(ui, tr("Archivos", "Files"), Col::Files, &mut sort);
                            });
                        })
                        .body(|body| {
                            body.rows(20.0, fview.order.len(), |mut row| {
                                let f = g.folders[fview.order[row.index()]];
                                row.set_selected(sel == Some(f.id));
                                row.col(|ui| {
                                    folder_icon(ui);
                                    ui.label(tree.full_path(f.id));
                                });
                                row.col(|ui| right(ui, fmt_size(f.bytes)));
                                row.col(|ui| right(ui, fmt_int(f.files as u64)));
                                click = row_click(&row.response(), f.id).or(click);
                            });
                        });
                });
                fview.sort = sort;
            });
        egui::CentralPanel::no_frame().show(ui, |ui| {
            if let Some(c) = files_table(ui, "ins_growth_files", tree, &g.files, &mut files_view, gen_, sel) {
                click = Some(c);
            }
        });
        self.views[1] = fview;
        self.views[2] = files_view;
        self.handle_click(click, acts);
        self.growth = Some(done);
    }

    // ------------------------------------------------------------ large & old

    fn old_ui(&mut self, ui: &mut egui::Ui, tree: &Tree, acts: &mut Vec<Action>) {
        ui.horizontal(|ui| {
            ui.label(tr("Archivos de más de", "Files bigger than"));
            ui.add(egui::DragValue::new(&mut self.min_mb).range(1..=10_000_000).speed(5.0).suffix(" MB"));
            ui.label(tr("sin modificar hace más de", "not modified for more than"));
            ui.add(egui::DragValue::new(&mut self.years).range(1..=50).speed(0.05).suffix(tr(" años", " years")));
            if let Some(d) = &self.old {
                let total: u64 = d.data.iter().map(|&id| tree.nodes[id as usize].size).sum();
                ui.separator();
                ui.label(RichText::new(format!("{} {} · {}", fmt_int(d.data.len() as u64), tr("archivos", "files"), fmt_size(total))).strong());
            }
        });
        let Some(done) = self.old.take() else {
            waiting(ui);
            return;
        };
        let mut view = std::mem::take(&mut self.views[3]);
        let click = files_table(ui, "ins_old", tree, &done.data, &mut view, self.gen_, self.sel);
        self.views[3] = view;
        self.handle_click(click, acts);
        self.old = Some(done);
    }
}

#[derive(Clone, Copy)]
enum Click {
    Select,
    Reveal,
    Menu,
}

fn row_click(resp: &egui::Response, id: u32) -> Option<(u32, Click)> {
    if resp.double_clicked() {
        Some((id, Click::Reveal))
    } else if resp.secondary_clicked() {
        Some((id, Click::Menu))
    } else if resp.clicked() {
        Some((id, Click::Select))
    } else {
        None
    }
}

/// Sortable table of files: name, size, modified, folder.
fn files_table(
    ui: &mut egui::Ui,
    salt: &str,
    tree: &Tree,
    ids: &[u32],
    view: &mut View,
    gen_: u64,
    sel: Option<u32>,
) -> Option<(u32, Click)> {
    view.update(gen_, 0, || (0..ids.len()).collect(), |i, col| node_val(tree, ids[i], col));
    let mut sort = view.sort;
    let mut click = None;
    ui.push_id(salt, |ui| {
        TableBuilder::new(ui)
            .striped(true)
            .resizable(true)
            .sense(Sense::click())
            .cell_layout(ELayout::left_to_right(Align::Center))
            .column(Column::initial(240.0).range(100.0..=600.0).clip(true))
            .column(Column::initial(85.0).range(55.0..=140.0).clip(true))
            .column(Column::initial(112.0).range(70.0..=160.0).clip(true))
            .column(Column::remainder().at_least(100.0).clip(true))
            .header(22.0, |mut h| {
                for (t, c) in [
                    (tr("Nombre", "Name"), Col::Name),
                    (tr("Tamaño", "Size"), Col::Size),
                    (tr("Modificado", "Modified"), Col::Modified),
                    (tr("Carpeta", "Folder"), Col::Path),
                ] {
                    h.col(|ui| {
                        sort_header(ui, t, c, &mut sort);
                    });
                }
            })
            .body(|body| {
                body.rows(20.0, view.order.len(), |mut row| {
                    let id = ids[view.order[row.index()]];
                    let n = &tree.nodes[id as usize];
                    row.set_selected(sel == Some(id));
                    row.col(|ui| {
                        file_icon(ui, tree.exts[n.ext as usize].color);
                        ui.label(tree.name(id));
                    });
                    row.col(|ui| right(ui, fmt_size(n.size)));
                    row.col(|ui| {
                        ui.label(fmt_date(n.modified));
                    });
                    row.col(|ui| {
                        ui.label(tree.full_path(n.parent));
                    });
                    click = row_click(&row.response(), id).or(click);
                });
            });
    });
    view.sort = sort;
    click
}

/// Permanently empty the Recycle Bin of each drive ("C:"). The confirmation
/// was already shown by the app.
fn empty_recycle_bins(drives: &[String]) -> Result<(), String> {
    use windows_sys::Win32::UI::Shell::{SHERB_NOCONFIRMATION, SHEmptyRecycleBinW};
    let mut errors = Vec::new();
    for d in drives {
        let root = crate::everything::wide(&format!("{}\\", d.trim_end_matches('\\')));
        let hr = unsafe { SHEmptyRecycleBinW(std::ptr::null_mut(), root.as_ptr(), SHERB_NOCONFIRMATION) };
        // E_UNEXPECTED (0x8000FFFF) is returned when the bin is already empty.
        if hr != 0 && hr as u32 != 0x8000_FFFF {
            errors.push(format!("{d}: 0x{:08X}", hr as u32));
        }
    }
    if errors.is_empty() { Ok(()) } else { Err(format!("SHEmptyRecycleBin: {}", errors.join(", "))) }
}
