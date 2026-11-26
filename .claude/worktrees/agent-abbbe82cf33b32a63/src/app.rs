use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, channel};
use std::time::{Duration, Instant};

use eframe::egui::{self, Align, Color32, Key, Layout as ELayout, RichText, Sense, Stroke};
use egui_extras::{Column, TableBuilder};

use crate::dup::{self, DupGroup, DupOptions, DupProgress};
use crate::i18n::{self, Lang, Settings, Theme, tr};
use crate::tree::{self, Change, Progress, ROOT, Tree};
use crate::treemap::{self, Layout};
use crate::sunburst;
use crate::{autostart, everything};

/// Incremental sync interval while the window is visible.
const SYNC_EVERY: Duration = Duration::from_secs(30);
/// Full reload (catches deletions/renames) when the data is older than this.
const FULL_RELOAD_AFTER: Duration = Duration::from_secs(30 * 60);
/// Real on-disk size is computed automatically for folders up to this many files.
const AUTO_REAL_MAX_FILES: u32 = 300_000;

pub enum TrayCmd {
    Show,
    Refresh,
    ToggleAutostart,
    Quit,
}

#[derive(PartialEq, Clone, Copy)]
enum Tab {
    Tree,
    Files,
    Dups,
    Ai,
    Insights,
}

struct AiAnswer {
    question: String,
    model: String,
    answer: Option<Result<String, String>>,
}

enum AiMsg {
    Model(Result<String, String>),
    Answer(usize, Result<String, String>),
}

/// Natural-language questions (Groq).
#[derive(Default)]
struct Ai {
    question: String,
    answers: Vec<AiAnswer>,
    /// Best model with quota, chosen once per session (shown next to the button).
    model: Option<String>,
    picking: bool,
    rx: Option<Receiver<AiMsg>>,
    key_input: String,
    key_editing: bool,
}

/// Folders that rarely hold user duplicates worth deleting (system, programs, app data).
pub(crate) fn suggested_exclusions() -> Vec<String> {
    let sys = std::env::var("SystemDrive").unwrap_or_else(|_| "C:".into());
    let mut v: Vec<String> = ["Windows", "Program Files", "Program Files (x86)", "ProgramData", "$Recycle.Bin", "System Volume Information", "Recovery"]
        .iter()
        .map(|d| format!("{sys}\\{d}"))
        .collect();
    if let Ok(home) = std::env::var("USERPROFILE") {
        v.push(format!("{home}\\AppData"));
    }
    v
}

struct DupJob {
    /// Groups stream in as they are confirmed.
    rx: Receiver<DupGroup>,
    done_rx: Receiver<()>,
    prog: Arc<DupProgress>,
    started: Instant,
    finished: Option<f64>,
}

#[derive(Default)]
struct DupView {
    sort: Sort,
    /// Display order of the groups of this kind (recomputed when the key changes).
    order: Vec<usize>,
    key: (usize, Sort, u64),
}

/// State of the duplicates tab.
struct Dups {
    min_mb: f64,
    same_name: bool,
    /// Advanced mode (hash the content) vs quick mode (index only).
    verify: bool,
    /// Versions mode: same name + folder, content may differ (overrides verify).
    versions: bool,
    scope_all: bool,
    job: Option<DupJob>,
    groups: Vec<DupGroup>,
    searched: bool,
    last_secs: f64,
    last_read: u64,
    last_hits: u64,
    marked: HashSet<String>,
    confirm: bool,
    /// Sort/order of the files (0) and folders (1) tables.
    views: [DupView; 2],
    recycle_rx: Option<Receiver<(Vec<String>, Result<bool, String>)>>,
    export_rx: Option<Receiver<Result<String, String>>>,
    /// Excluded-folders editor open, its text (one path per line), and the
    /// "pick in tree" mode that shows checkboxes on folders.
    show_excl: bool,
    excl_text: String,
    pick: bool,
    /// Smart marking: kept copy → why it was chosen.
    keep_reason: std::collections::HashMap<String, String>,
}

impl Default for Dups {
    fn default() -> Self {
        Self {
            min_mb: 1.0,
            same_name: true,
            verify: true,
            versions: false,
            scope_all: false,
            job: None,
            groups: Vec::new(),
            searched: false,
            last_secs: 0.0,
            last_read: 0,
            last_hits: 0,
            marked: HashSet::new(),
            confirm: false,
            views: Default::default(),
            recycle_rx: None,
            export_rx: None,
            show_excl: false,
            excl_text: String::new(),
            pick: false,
            keep_reason: Default::default(),
        }
    }
}

/// What a right-click menu was opened on.
#[derive(Clone)]
enum MenuTarget {
    Node(u32),
    Path(String),
}

/// Sortable columns.
#[derive(PartialEq, Eq, Clone, Copy, Hash, Debug)]
pub(crate) enum Col {
    Name,
    Pct,
    Size,
    Alloc,
    Items,
    Files,
    Folders,
    Modified,
    Path,
    Count,
}

/// `None` = default order (size descending, like WizTree).
pub(crate) type Sort = Option<(Col, bool)>;

/// File type categories for the "File types" panel. "Others" catches the rest.
/// (Spanish name, English name, extensions.) Extensions without a dot are matched
/// lowercase; the empty string is "no extension".
pub(crate) const CATEGORIES: &[(&str, &str, &[&str])] = &[
    ("Video", "Video", &[
        "mp4", "mkv", "avi", "mov", "wmv", "flv", "webm", "m4v", "mpg", "mpeg", "3gp", "ts", "m2ts", "mts", "vob",
        "mxf", "divx", "rm", "rmvb", "ogv", "f4v", "asf", "m2v",
    ]),
    ("Audio", "Audio", &[
        "mp3", "wav", "flac", "aac", "ogg", "m4a", "wma", "aif", "aiff", "opus", "mid", "midi", "ape", "alac", "mka",
        "caf", "dsf", "dff", "m4b", "amr",
    ]),
    ("Imágenes", "Images", &[
        "jpg", "jpeg", "png", "gif", "bmp", "tif", "tiff", "webp", "heic", "heif", "avif", "jxl", "raw", "cr2", "cr3",
        "nef", "arw", "dng", "orf", "rw2", "raf", "ico", "svg", "emf", "wmf", "jfif",
    ]),
    ("Documentos", "Documents", &[
        "pdf", "doc", "docx", "docm", "dot", "dotx", "odt", "rtf", "txt", "md", "epub", "mobi", "azw3", "pages",
        "xps", "oxps", "tex", "gdoc", "one", "pub", "vsd", "vsdx", "djvu",
    ]),
    ("Planillas", "Spreadsheets", &["xls", "xlsx", "xlsm", "xlsb", "xlt", "xltx", "ods", "csv", "tsv", "numbers", "gsheet"]),
    ("Presentaciones", "Presentations", &["ppt", "pptx", "pptm", "pps", "ppsx", "pot", "potx", "odp", "key", "gslides"]),
    ("Correo", "Mail", &["pst", "ost", "msg", "eml", "mbox", "emlx", "ics", "vcf"]),
    ("Comprimidos", "Archives", &[
        "zip", "rar", "7z", "tar", "gz", "bz2", "xz", "zst", "cab", "tgz", "tbz2", "txz", "lz", "lzma", "lz4", "arj",
        "sit", "sitx",
    ]),
    ("Imágenes de disco y VMs", "Disk images & VMs", &[
        "iso", "img", "vhd", "vhdx", "avhdx", "vmdk", "vdi", "wim", "esd", "dmg", "qcow2", "ova", "ovf", "vmem", "vmsn",
        "hdd", "sav",
    ]),
    ("Programas y sistema", "Programs & system", &[
        "exe", "dll", "sys", "msi", "msp", "msix", "appx", "appxbundle", "msixbundle", "apk", "drv", "ocx", "cpl",
        "mui", "cat", "efi", "pak", "bin", "dat", "inf", "ini", "reg", "lnk", "url", "manifest", "config", "xbf",
        "pri", "nls", "tlb", "bat", "cmd", "ps1", "vbs", "scr", "com", "ax", "winmd", "nupkg",
    ]),
    ("Código y desarrollo", "Code & development", &[
        "rs", "js", "mjs", "cjs", "ts", "tsx", "jsx", "vue", "svelte", "py", "pyi", "pyx", "ipynb", "java", "kt",
        "swift", "dart", "scala", "go", "rb", "php", "lua", "r", "pl", "sh", "c", "cc", "cpp", "h", "hpp", "cs",
        "html", "htm", "css", "scss", "sass", "less", "json", "xml", "yml", "yaml", "toml", "lock", "sql", "gradle",
        "cmake", "mk", "sln", "csproj", "vcxproj", "props", "targets", "rlib", "rmeta", "pdb", "o", "obj", "lib",
        "a", "so", "dylib", "ilk", "exp", "class", "jar", "wasm", "map", "pyc", "pyd", "node", "d", "pack", "idx",
        "rev", "tsbuildinfo",
    ]),
    ("Bases de datos", "Databases", &[
        "db", "sqlite", "sqlite3", "db3", "mdb", "accdb", "ldf", "mdf", "ndf", "dbo", "wal", "shm", "ldb", "sst",
        "frm", "ibd", "myd", "myi", "realm", "kdbx",
    ]),
    ("Fuentes", "Fonts", &["ttf", "otf", "ttc", "woff", "woff2", "fon", "eot", "pfb"]),
    ("Diseño y 3D", "Design & 3D", &[
        "psd", "psb", "ai", "eps", "indd", "xcf", "kra", "sketch", "fig", "xd", "cdr", "afphoto", "afdesign", "stl",
        "3mf", "fbx", "blend", "gcode", "step", "stp", "iges", "dwg", "dxf", "skp", "max", "c4d", "glb", "gltf", "usdz",
    ]),
    ("Logs y temporales", "Logs & temporary", &[
        "log", "tmp", "temp", "bak", "old", "dmp", "mdmp", "etl", "evtx", "cache", "crdownload", "part", "partial",
        "download", "swp", "~", "blf", "regtrans-ms",
    ]),
    ("Sin extensión", "No extension", &[""]),
    ("Otros", "Others", &[]),
];

/// highlight_ext values with this bit set mean a whole category (low bits = index).
const CAT_FLAG: u32 = 0x8000_0000;

pub(crate) fn category_of(ext: &str) -> usize {
    CATEGORIES.iter().position(|(_, _, exts)| exts.contains(&ext)).unwrap_or(CATEGORIES.len() - 1)
}

pub(crate) fn category_name(c: usize) -> &'static str {
    let (es, en, _) = CATEGORIES[c];
    tr(es, en)
}

/// True if path or one of its parent folders is in marked.
fn under_marked(marked: &HashSet<String>, path: &str) -> bool {
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

/// How worth keeping a copy looks, from its path alone (higher is better),
/// with the main reason. Used by the smart marking of duplicates.
pub(crate) fn keep_score(path: &str, prefer: &str) -> (i32, &'static str) {
    let p = path.to_lowercase();
    let name = p.rsplit('\\').next().unwrap_or("");
    let has = |words: &[&str]| words.iter().any(|w| p.contains(w));
    let mut score = 0;
    let mut why = "";
    let mut note = |s: i32, w: &'static str| {
        if s.abs() > 0 && why.is_empty() {
            why = w;
        }
        score += s;
    };
    if !prefer.is_empty() && p.starts_with(prefer) {
        note(100, "unidad preferida");
    }
    if has(&["\\$recycle.bin\\", "\\trash", "\\papelera", "\\temp\\", "\\tmp\\", "\\cache", "\\caché"]) {
        note(-60, "las otras están en temporales/papelera");
    }
    if has(&["backup", "respaldo", "copia de seguridad", "\\old\\", "\\viejo", "\\archive", "\\archivo viejo", ".bak"]) {
        note(-40, "las otras parecen respaldos viejos");
    }
    let copy_name = [" (1)", " (2)", " (3)", " - copia", " - copy", "copy of ", "copia de ", "_copy", "_copia"].iter().any(|w| name.contains(w));
    if copy_name {
        note(-30, "las otras tienen nombre de copia");
    }
    if has(&["\\downloads\\", "\\descargas\\"]) {
        note(-20, "las otras están en Descargas");
    }
    if has(&["\\documents\\", "\\documentos\\", "\\pictures\\", "\\imágenes\\", "\\music\\", "\\música\\", "\\videos\\", "\\vídeos\\", "\\mi unidad\\", "\\my drive\\"]) {
        note(15, "carpeta personal organizada");
    }
    (score, why)
}

fn natural_asc(c: Col) -> bool {
    matches!(c, Col::Name | Col::Path)
}

/// Clickable header: 1st click sorts, 2nd reverses, 3rd clears the sort.
pub(crate) fn sort_header(ui: &mut egui::Ui, label: &str, col: Col, sort: &mut Sort) -> bool {
    let arrow = match *sort {
        Some((c, asc)) if c == col => {
            if asc {
                "  ▲"
            } else {
                "  ▼"
            }
        }
        _ => "",
    };
    let resp = ui
        .add(egui::Button::new(RichText::new(format!("{label}{arrow}")).strong()).frame(false))
        .on_hover_text(tr(
            "Clic: ordenar · 2º clic: invertir · 3er clic: quitar orden",
            "Click: sort · 2nd click: reverse · 3rd click: clear sort",
        ));
    if resp.clicked() {
        *sort = match *sort {
            Some((c, asc)) if c == col => {
                if asc == natural_asc(col) { Some((col, !asc)) } else { None }
            }
            _ => Some((col, natural_asc(col))),
        };
        return true;
    }
    false
}

fn cmp_ci(a: &str, b: &str) -> std::cmp::Ordering {
    a.chars().flat_map(char::to_lowercase).cmp(b.chars().flat_map(char::to_lowercase))
}

fn sort_nodes(tree: &Tree, ids: &mut [u32], col: Col, asc: bool) {
    use std::cmp::Ordering;
    ids.sort_by(|&x, &y| {
        let (a, b) = (&tree.nodes[x as usize], &tree.nodes[y as usize]);
        let o: Ordering = match col {
            Col::Name | Col::Path => cmp_ci(tree.name(x), tree.name(y)),
            Col::Pct | Col::Size | Col::Count => a.size.cmp(&b.size),
            Col::Alloc => a.alloc.cmp(&b.alloc),
            Col::Items => (a.files + a.folders).cmp(&(b.files + b.folders)),
            Col::Files => a.files.cmp(&b.files),
            Col::Folders => a.folders.cmp(&b.folders),
            Col::Modified => a.modified.cmp(&b.modified),
        };
        let o = if asc { o } else { o.reverse() };
        o.then_with(|| b.size.cmp(&a.size))
    });
}

/// Visible tree rows (node, depth): `root` first, then the children of every
/// expanded folder — every level, "This PC" included — in the requested order.
pub(crate) fn flatten_rows(tree: &Tree, root: u32, expanded: &HashSet<u32>, sort: Sort) -> Vec<(u32, u16)> {
    let mut rows = Vec::new();
    let mut stack = vec![(root, 0u16)];
    while let Some((id, d)) = stack.pop() {
        rows.push((id, d));
        if expanded.contains(&id) {
            let mut v = tree.children(id).to_vec();
            if let Some((col, asc)) = sort {
                sort_nodes(tree, &mut v, col, asc);
            }
            // Folders first, loose files after — unless sorting by the Folder (name) column.
            if !matches!(sort, Some((Col::Name, _))) {
                v.sort_by_key(|&c| !tree.nodes[c as usize].is_dir()); // stable: keeps the size/column order
            }
            stack.extend(v.into_iter().rev().map(|c| (c, d + 1)));
        }
    }
    rows
}

struct Loading {
    rx: Receiver<Result<Tree, String>>,
    progress: Arc<Progress>,
    started: Instant,
}

struct RealJob {
    rx: Receiver<(u64, Vec<(u32, u64)>)>,
    done: Arc<AtomicU64>,
    total: u64,
    root: u32,
}

struct ExportJob {
    rx: Receiver<Result<(String, u64), String>>,
    done: Arc<AtomicU64>,
}

/// Tray menu entries, kept so their labels can follow the UI language.
pub struct TrayItems {
    pub open: tray_icon::menu::MenuItem,
    pub refresh: tray_icon::menu::MenuItem,
    pub auto: tray_icon::menu::CheckMenuItem,
    pub quit: tray_icon::menu::MenuItem,
}

impl TrayItems {
    pub fn relabel(&self) {
        self.open.set_text(tr("Abrir", "Open"));
        self.refresh.set_text(tr("Actualizar", "Refresh"));
        self.auto.set_text(tr("Iniciar con Windows", "Start with Windows"));
        self.quit.set_text(tr("Salir", "Quit"));
    }
}

fn apply_theme(ctx: &egui::Context, t: Theme) {
    ctx.set_theme(match t {
        Theme::System => egui::ThemePreference::System,
        Theme::Light => egui::ThemePreference::Light,
        Theme::Dark => egui::ThemePreference::Dark,
    });
}

pub struct App {
    tree: Option<Arc<Tree>>,
    /// Bumped on every full load: results computed for an older tree are discarded.
    epoch: u64,
    /// Bumped on every change to the tree (load or mutation): invalidates caches.
    revision: u64,
    loading: Option<Loading>,
    error: Option<String>,
    info: Option<(String, Instant)>,
    error_at: Instant,
    /// Drive free-space cache: (drive, when, (total, free)).
    disk_cache: Option<(u32, Instant, Option<(u64, u64)>)>,

    // incremental sync
    sync_rx: Option<Receiver<(u64, String, Result<Vec<Change>, String>)>>,
    pending_changes: Option<(u64, String, Vec<Change>)>,
    sync_stamp: String,
    last_sync: Instant,

    // real on-disk size
    real_job: Option<RealJob>,
    pending_real: Option<(u64, Vec<(u32, u64)>)>,
    real_done: HashSet<u32>,
    real_queue: Option<u32>,

    export_job: Option<ExportJob>,
    exclusion_dirty: bool,
    dups: Dups,
    ctx_menu: Option<(MenuTarget, egui::Pos2)>,
    ai: Ai,
    insights: crate::insights::Insights,
    /// Dark/light state last applied to the native title bar.
    title_dark: Option<bool>,
    highlight_cache: Option<(u32, u64, Arc<treemap::Highlight>)>,

    view_root: u32,
    selected: Option<u32>,
    expanded: HashSet<u32>,
    rows: Vec<(u32, u16)>,
    rows_dirty: bool,
    scroll_to_selected: bool,

    layout: Option<Layout>,
    texture: Option<egui::TextureHandle>,
    layout_key: (u32, usize, usize, Option<u32>, u64, u8),
    pending_size: Option<((usize, usize), Instant)>,
    hovered: Option<u32>,

    ext_rows: Vec<(u32, u64, u64)>,
    ext_key: (u32, u64, Sort),
    /// Categories in display order: (category, size, files).
    ext_cats: Vec<(usize, u64, u64)>,
    ext_open: HashSet<usize>,
    highlight_ext: Option<u32>,

    tab: Tab,
    file_filter: String,
    files: Vec<u32>,
    files_key: (u32, u64, String, Sort),

    tree_sort: Sort,
    files_sort: Sort,
    ext_sort: Sort,

    settings: Settings,

    autostart: bool,
    tray_rx: Receiver<TrayCmd>,
    has_tray: bool,
    visible: bool,
    pub tray_items: Option<TrayItems>,
    pub tray_icon: Option<tray_icon::TrayIcon>,
}

impl App {
    pub fn new(ctx: &egui::Context, tray_rx: Receiver<TrayCmd>, has_tray: bool, start_hidden: bool, settings: Settings) -> Self {
        setup_fonts(ctx);
        let mut settings = settings;
        if !settings.dup_exclude_init {
            settings.dup_exclude = suggested_exclusions();
            settings.dup_exclude_init = true;
            settings.save();
        }
        i18n::set_lang(settings.lang);
        apply_theme(ctx, settings.theme);
        let mut style = (*ctx.global_style()).clone();
        style.spacing.item_spacing = egui::vec2(6.0, 3.0);
        // Selectable labels swallow clicks (incl. right-click) before the table
        // row sees them; rows must get them for selection and the context menu.
        style.interaction.selectable_labels = false;
        ctx.set_global_style(style);
        let mut app = Self {
            tree: None,
            epoch: 0,
            revision: 0,
            loading: None,
            error: None,
            info: None,
            error_at: Instant::now(),
            disk_cache: None,
            sync_rx: None,
            pending_changes: None,
            sync_stamp: String::new(),
            last_sync: Instant::now(),
            real_job: None,
            pending_real: None,
            real_done: HashSet::new(),
            real_queue: None,
            export_job: None,
            exclusion_dirty: false,
            dups: Dups::default(),
            ctx_menu: None,
            ai: Ai::default(),
            insights: Default::default(),
            title_dark: None,
            highlight_cache: None,
            view_root: ROOT,
            selected: None,
            expanded: HashSet::new(),
            rows: Vec::new(),
            rows_dirty: true,
            scroll_to_selected: false,
            layout: None,
            texture: None,
            layout_key: (u32::MAX, 0, 0, None, u64::MAX, 0),
            pending_size: None,
            hovered: None,
            ext_rows: Vec::new(),
            ext_key: (u32::MAX, u64::MAX, None),
            ext_cats: Vec::new(),
            ext_open: HashSet::new(),
            highlight_ext: None,
            tab: Tab::Tree,
            file_filter: String::new(),
            files: Vec::new(),
            files_key: (u32::MAX, u64::MAX, String::new(), None),
            tree_sort: None,
            files_sort: None,
            ext_sort: None,
            settings,
            autostart: autostart::is_enabled(),
            tray_rx,
            has_tray,
            visible: !start_hidden,
            tray_items: None,
            tray_icon: None,
        };
        app.start_load(ctx);
        app
    }

    fn set_error(&mut self, e: String) {
        self.error = Some(e);
        self.error_at = Instant::now();
    }

    /// Windows title bar follows the app theme (black in dark mode).
    fn sync_title_bar(&mut self, ctx: &egui::Context, frame: &eframe::Frame) {
        use raw_window_handle::{HasWindowHandle, RawWindowHandle};
        use windows_sys::Win32::Graphics::Dwm::DwmSetWindowAttribute;
        let dark = ctx.global_style().visuals.dark_mode;
        if self.title_dark == Some(dark) {
            return;
        }
        let Ok(handle) = frame.window_handle() else { return };
        if let RawWindowHandle::Win32(w) = handle.as_raw() {
            const DWMWA_USE_IMMERSIVE_DARK_MODE: u32 = 20;
            let value: i32 = dark.into();
            unsafe {
                DwmSetWindowAttribute(w.hwnd.get() as _, DWMWA_USE_IMMERSIVE_DARK_MODE, &value as *const i32 as _, 4);
            }
            self.title_dark = Some(dark);
        }
    }

    fn set_info(&mut self, s: String) {
        self.info = Some((s, Instant::now()));
    }

    // ------------------------------------------------------------ background work

    fn start_load(&mut self, ctx: &egui::Context) {
        if self.loading.is_some() {
            return;
        }
        let (tx, rx) = channel();
        let progress = Arc::new(Progress::default());
        let p = progress.clone();
        let ctx = ctx.clone();
        let excluded = self.settings.excluded_drives.clone();
        std::thread::Builder::new()
            .name("everything-load".into())
            .spawn(move || {
                // At Windows logon Everything may not be ready yet.
                while !matches!(everything::status(), everything::Status::Ready) {
                    p.waiting.store(true, Ordering::Relaxed);
                    ctx.request_repaint();
                    std::thread::sleep(Duration::from_secs(1));
                }
                p.waiting.store(false, Ordering::Relaxed);
                let pc = p.clone();
                let ctx2 = ctx.clone();
                let watcher = std::thread::spawn(move || {
                    while Arc::strong_count(&pc) > 2 {
                        ctx2.request_repaint();
                        std::thread::sleep(Duration::from_millis(150));
                    }
                });
                let r = tree::load(p.clone(), &excluded);
                drop(p);
                let _ = tx.send(r);
                ctx.request_repaint();
                let _ = watcher.join();
            })
            .expect("thread");
        self.loading = Some(Loading { rx, progress, started: Instant::now() });
    }

    fn poll_load(&mut self) {
        let Some(l) = &self.loading else { return };
        let Ok(res) = l.rx.try_recv() else { return };
        self.loading = None;
        match res {
            Err(e) => self.set_error(e),
            Ok(new) => {
                self.error = None;
                self.sync_stamp = new.sync_stamp.clone();
                self.last_sync = Instant::now();
                let new = Arc::new(new);
                // Keep view, selection and expanded folders by path.
                if let Some(old) = &self.tree {
                    let root_path = old.full_path(self.view_root);
                    let sel_path = self.selected.map(|s| old.full_path(s));
                    let exp: Vec<String> = self.expanded.iter().map(|&e| old.full_path(e)).collect();
                    self.view_root = if self.view_root == ROOT { ROOT } else { new.find_path(&root_path).unwrap_or(ROOT) };
                    self.selected = sel_path.and_then(|p| new.find_path(&p));
                    self.expanded = exp.iter().filter_map(|p| new.find_path(p)).collect();
                    self.expanded.insert(self.view_root);
                    // Extension ids are reassigned on every load: re-resolve by name.
                    self.highlight_ext = self.highlight_ext.and_then(|e| {
                        if e & CAT_FLAG != 0 { Some(e) } else { old.exts.get(e as usize).and_then(|x| new.find_ext(&x.name)) }
                    });
                    // An open right-click menu keeps pointing at the same path.
                    if let Some((MenuTarget::Node(id), pos)) = self.ctx_menu.clone() {
                        self.ctx_menu = ((id as usize) < old.nodes.len()).then(|| (MenuTarget::Path(old.full_path(id)), pos));
                    }
                } else {
                    self.view_root = default_drive(&new);
                    self.expanded.insert(self.view_root);
                }
                self.tree = Some(new);
                self.epoch += 1;
                self.hovered = None;
                self.real_queue = None;
                self.highlight_cache = None;
                self.disk_cache = None;
                self.revision += 1;
                self.real_done.clear();
                self.pending_changes = None;
                self.pending_real = None;
                self.rows_dirty = true;
            }
        }
    }

    fn maybe_sync(&mut self, ctx: &egui::Context) {
        if !self.visible || self.tree.is_none() || self.loading.is_some() || self.sync_rx.is_some() || self.pending_changes.is_some() {
            return;
        }
        if self.tree.as_ref().is_some_and(|t| t.built_at.elapsed() > FULL_RELOAD_AFTER) {
            self.start_load(ctx);
            return;
        }
        if self.last_sync.elapsed() < SYNC_EVERY {
            return;
        }
        self.last_sync = Instant::now();
        let (tx, rx) = channel();
        let since = self.sync_stamp.clone();
        let epoch = self.epoch;
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let stamp = tree::local_stamp();
            let r = tree::query_changes(&since);
            let _ = tx.send((epoch, stamp, r));
            ctx.request_repaint();
        });
        self.sync_rx = Some(rx);
    }

    fn start_real(&mut self, ctx: &egui::Context, root: u32) {
        if self.real_job.is_some() {
            self.real_queue = Some(root);
            return;
        }
        let Some(tree) = self.tree.clone() else { return };
        let (tx, rx) = channel();
        let done = Arc::new(AtomicU64::new(0));
        let d = done.clone();
        let epoch = self.epoch;
        let total = tree.nodes[root as usize].files as u64;
        let ctx = ctx.clone();
        std::thread::Builder::new()
            .name("real-size".into())
            .spawn(move || {
                let files = tree.files_under(root);
                let cluster = tree.cluster_of(root);
                // Metadata queries in parallel: one chunk per core.
                let threads = std::thread::available_parallelism().map_or(4, |n| n.get()).max(1);
                let chunk = files.len().div_ceil(threads).max(1);
                let out: Vec<(u32, u64)> = std::thread::scope(|s| {
                    let handles: Vec<_> = files
                        .chunks(chunk)
                        .map(|part| {
                            let (tree, d, ctx) = (&tree, &d, &ctx);
                            s.spawn(move || {
                                let mut v = Vec::with_capacity(part.len());
                                for (i, &id) in part.iter().enumerate() {
                                    if let Some(real) = tree::real_file_alloc(&tree.full_path(id), cluster) {
                                        v.push((id, real));
                                    }
                                    if i % 2000 == 1999 {
                                        d.fetch_add(2000, Ordering::Relaxed);
                                        ctx.request_repaint();
                                    }
                                }
                                v
                            })
                        })
                        .collect();
                    handles.into_iter().flat_map(|h| h.join().unwrap_or_default()).collect()
                });
                drop(tree);
                let _ = tx.send((epoch, out));
                ctx.request_repaint();
            })
            .expect("thread");
        self.real_job = Some(RealJob { rx, done, total, root });
    }

    fn start_export(&mut self, ctx: &egui::Context) {
        if self.export_job.is_some() {
            return;
        }
        let Some(tree) = self.tree.clone() else { return };
        let root = self.view_root;
        let default_name = {
            let p = tree.full_path(root);
            let p = p.replace([':', '\\'], "_").trim_matches('_').to_string();
            format!("WizEverything_{}.csv", if p.is_empty() { "All".into() } else { p })
        };
        let (tx, rx) = channel();
        let done = Arc::new(AtomicU64::new(0));
        let d = done.clone();
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let r = match save_dialog(&default_name) {
                None => Err(String::new()),
                Some(path) => tree
                    .write_csv(root, std::path::Path::new(&path), &d)
                    .map(|rows| (path, rows))
                    .map_err(|e| e.to_string()),
            };
            drop(tree);
            let _ = tx.send(r);
            ctx.request_repaint();
        });
        self.export_job = Some(ExportJob { rx, done });
    }

    /// Collect finished background work and apply mutations when the tree isn't shared.
    fn poll_jobs(&mut self, ctx: &egui::Context) {
        if let Some(rx) = &self.sync_rx
            && let Ok((epoch, stamp, r)) = rx.try_recv()
        {
            self.sync_rx = None;
            match r {
                Ok(ch) if epoch == self.epoch => self.pending_changes = Some((epoch, stamp, ch)),
                Ok(_) => {}
                Err(e) => self.set_error(e),
            }
        }
        if let Some(job) = &self.real_job
            && let Ok((epoch, vals)) = job.rx.try_recv()
        {
            let root = job.root;
            self.real_job = None;
            if epoch == self.epoch {
                self.real_done.insert(root);
                self.pending_real = Some((epoch, vals));
            }
            if let Some(next) = self.real_queue.take() {
                self.start_real(ctx, next);
            }
        }
        if let Some(job) = &self.export_job
            && let Ok(r) = job.rx.try_recv()
        {
            self.export_job = None;
            match r {
                Ok((path, rows)) => self.set_info(format!("{} {} → {path}", fmt_int(rows), tr("filas exportadas", "rows exported"))),
                Err(e) if e.is_empty() => {}
                Err(e) => self.set_error(e),
            }
        }

        // Duplicates: search, recycle and export results.
        if let Some(job) = &mut self.dups.job {
            self.dups.groups.extend(job.rx.try_iter());
            if job.done_rx.try_recv().is_ok() {
                self.dups.groups.extend(job.rx.try_iter());
                self.dups.last_secs = job.started.elapsed().as_secs_f64();
                self.dups.last_read = job.prog.bytes_read.load(Ordering::Relaxed);
                self.dups.last_hits = job.prog.cache_hits.load(Ordering::Relaxed);
                job.finished = Some(self.dups.last_secs);
            }
            if job.finished.is_some() {
                self.dups.job = None;
            }
        }
        if let Some(rx) = &self.dups.recycle_rx
            && let Ok((paths, r)) = rx.try_recv()
        {
            self.dups.recycle_rx = None;
            if let Err(e) = &r {
                self.set_error(e.clone());
            }
            // Even after an error some files may already be gone: always prune.
            {
                {
                    // Drop what is really gone; keep groups that still have duplicates.
                    let gone: HashSet<&String> = paths.iter().filter(|p| !std::path::Path::new(p.as_str()).exists()).collect();
                    for g in &mut self.dups.groups {
                        g.files.retain(|f| !gone.contains(&f.path));
                    }
                    self.dups.groups.retain(|g| g.files.len() > 1);
                    self.dups.marked.retain(|p| !gone.contains(p));
                    self.set_info(format!("{} {}", fmt_int(gone.len() as u64), tr("archivos enviados a la papelera", "files sent to the Recycle Bin")));
                    if !gone.is_empty() {
                        self.start_load(ctx);
                    }
                }
            }
        }
        if let Some(rx) = &self.dups.export_rx
            && let Ok(r) = rx.try_recv()
        {
            self.dups.export_rx = None;
            match r {
                Ok(p) => self.set_info(format!("{} → {p}", tr("Duplicados exportados", "Duplicates exported"))),
                Err(e) if e.is_empty() => {}
                Err(e) => self.set_error(e),
            }
        }

        // Mutations need exclusive access to the tree.
        let wants = self.pending_changes.is_some() || self.pending_real.is_some() || self.exclusion_dirty;
        if !wants {
            return;
        }
        let Some(arc) = self.tree.as_mut() else { return };
        let Some(tree) = Arc::get_mut(arc) else { return };
        let mut changed = false;
        if let Some((epoch, stamp, ch)) = self.pending_changes.take()
            && epoch == self.epoch
        {
            if tree.apply_changes(&ch) > 0 {
                changed = true;
            }
            self.sync_stamp = stamp;
        }
        if let Some((epoch, vals)) = self.pending_real.take()
            && epoch == self.epoch
        {
            tree.set_real_alloc(&vals);
            changed = true;
        }
        if self.exclusion_dirty {
            tree.set_excluded(&self.settings.excluded_drives);
            self.exclusion_dirty = false;
            changed = true;
        }
        if changed {
            self.revision += 1;
            self.rows_dirty = true;
        }
    }

    // ------------------------------------------------------------ view state

    fn set_root(&mut self, ctx: &egui::Context, id: u32) {
        self.view_root = id;
        self.expanded.insert(id);
        self.rows_dirty = true;
        self.highlight_ext = None;
        // Real on-disk size for a folder view (not drives / "All"), if reasonably small.
        if let Some(t) = &self.tree {
            let n = &t.nodes[id as usize];
            let is_folder = id != ROOT && n.parent != ROOT && n.is_dir();
            let covered = t.ancestors(id).iter().any(|a| self.real_done.contains(a));
            if is_folder && !covered && n.files <= AUTO_REAL_MAX_FILES {
                self.start_real(ctx, id);
            }
        }
    }

    fn select(&mut self, ctx: &egui::Context, tree: &Tree, id: u32, reveal: bool) {
        self.selected = Some(id);
        if reveal {
            let anc = tree.ancestors(id);
            if !anc.contains(&self.view_root) {
                self.set_root(ctx, *anc.get(1).unwrap_or(&ROOT));
            }
            let mut inside = false;
            for &a in &anc {
                if a == self.view_root {
                    inside = true;
                }
                if inside && a != id {
                    self.expanded.insert(a);
                }
            }
            self.rows_dirty = true;
            self.scroll_to_selected = true;
        }
    }

    fn rebuild_rows(&mut self, tree: &Tree) {
        self.rows = flatten_rows(tree, self.view_root, &self.expanded, self.tree_sort);
        self.rows_dirty = false;
    }

    fn handle_tray(&mut self, ctx: &egui::Context) {
        while let Ok(cmd) = self.tray_rx.try_recv() {
            match cmd {
                TrayCmd::Show => self.show_window(ctx),
                TrayCmd::Refresh => self.start_load(ctx),
                TrayCmd::ToggleAutostart => self.toggle_autostart(),
                TrayCmd::Quit => self.quit(),
            }
        }
    }

    fn quit(&mut self) -> ! {
        // Drop the tray icon first so no ghost icon stays in the notification area.
        self.tray_icon.take();
        std::process::exit(0)
    }

    fn show_window(&mut self, ctx: &egui::Context) {
        ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
        ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
        ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
        self.visible = true;
        // Catch up right away with whatever changed while hidden.
        self.last_sync = Instant::now() - SYNC_EVERY;
    }

    fn toggle_autostart(&mut self) {
        let want = !self.autostart;
        match autostart::set(want) {
            Ok(()) => self.autostart = want,
            Err(e) => self.set_error(e),
        }
        if let Some(items) = &self.tray_items {
            items.auto.set_checked(self.autostart);
        }
    }

    /// Keyboard navigation in the tree (when no text field has focus).
    fn tree_keys(&mut self, ctx: &egui::Context, tree: &Tree) {
        if ctx.egui_wants_keyboard_input() || self.tab != Tab::Tree {
            return;
        }
        let pressed = |k: Key| ctx.input(|i| i.key_pressed(k));
        let pos = self.selected.and_then(|s| self.rows.iter().position(|r| r.0 == s));
        let go = |app: &mut Self, idx: usize| {
            if let Some(&(id, _)) = app.rows.get(idx) {
                app.selected = Some(id);
                app.scroll_to_selected = true;
            }
        };
        let last = self.rows.len().saturating_sub(1);
        if pressed(Key::ArrowDown) {
            go(self, pos.map_or(0, |p| (p + 1).min(last)));
        } else if pressed(Key::ArrowUp) {
            go(self, pos.map_or(0, |p| p.saturating_sub(1)));
        } else if pressed(Key::PageDown) {
            go(self, pos.map_or(0, |p| (p + 20).min(last)));
        } else if pressed(Key::PageUp) {
            go(self, pos.map_or(0, |p| p.saturating_sub(20)));
        } else if pressed(Key::Home) {
            go(self, 0);
        } else if pressed(Key::End) {
            go(self, last);
        } else if let Some(sel) = self.selected {
            let n = &tree.nodes[sel as usize];
            if pressed(Key::ArrowRight) && n.is_dir() {
                if self.expanded.insert(sel) {
                    self.rows_dirty = true;
                } else if let Some(&c) = tree.children(sel).first() {
                    self.selected = Some(c);
                    self.scroll_to_selected = true;
                }
            } else if pressed(Key::ArrowLeft) {
                if self.expanded.contains(&sel) && sel != self.view_root {
                    self.expanded.remove(&sel);
                    self.rows_dirty = true;
                } else if sel != self.view_root {
                    self.selected = Some(n.parent);
                    self.scroll_to_selected = true;
                }
            } else if pressed(Key::Enter) && n.is_dir() && sel != self.view_root {
                self.set_root(ctx, sel);
            }
        }
        if pressed(Key::Backspace) && self.view_root != ROOT {
            let p = tree.nodes[self.view_root as usize].parent;
            let from = self.view_root;
            self.set_root(ctx, p);
            self.select(ctx, tree, from, true);
        }
    }
}

impl eframe::App for App {
    fn logic(&mut self, ctx: &egui::Context, frame: &mut eframe::Frame) {
        self.sync_title_bar(ctx, frame);
        self.handle_tray(ctx);
        self.poll_load();
        self.poll_jobs(ctx);
        self.maybe_sync(ctx);
        if ctx.input(|i| i.viewport().close_requested()) {
            if self.has_tray && self.settings.close_to_tray {
                ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
                ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
                self.visible = false;
                // free the big texture while hidden
                self.texture = None;
                self.layout = None;
                self.layout_key.0 = u32::MAX;
            } else {
                self.quit();
            }
        }
        if ctx.input(|i| i.key_pressed(Key::F5)) {
            self.start_load(ctx);
        }
        // keep the sync timer ticking while visible
        if self.visible {
            ctx.request_repaint_after(Duration::from_secs(5));
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        // ui() only runs while the window is shown (also when another instance showed it).
        self.visible = true;
        let ctx = ui.ctx().clone();
        let tree = self.tree.clone();

        egui::Panel::top("toolbar").show(ui, |ui| self.toolbar(ui, &ctx, tree.as_deref()));
        egui::Panel::top("ask").show(ui, |ui| self.ask_bar(ui, &ctx));
        self.groq_key_dialog(&ctx);
        egui::Panel::bottom("status").show(ui, |ui| self.status_bar(ui, tree.as_deref()));

        let Some(tree) = tree else {
            egui::CentralPanel::default_margins().show(ui, |ui| {
                ui.centered_and_justified(|ui| {
                    if let Some(e) = &self.error {
                        ui.colored_label(Color32::LIGHT_RED, e);
                    } else {
                        ui.spinner();
                    }
                });
            });
            return;
        };

        if self.rows_dirty {
            self.rebuild_rows(&tree);
        }
        self.tree_keys(&ctx, &tree);

        let h = ui.available_height();
        if self.settings.show_treemap {
            egui::Panel::bottom("treemap")
                .resizable(true)
                .default_size(h * 0.55)
                .min_size(80.0)
                .frame(egui::Frame::NONE)
                .show(ui, |ui| self.treemap_ui(ui, &tree));
        } else {
            self.hovered = None;
        }
        if self.settings.show_exts {
            egui::Panel::right("exts").resizable(true).default_size(360.0).show(ui, |ui| self.ext_ui(ui, &tree));
        }
        egui::CentralPanel::default_margins().show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.selectable_value(&mut self.tab, Tab::Tree, tr("Vista de árbol", "Tree view"));
                ui.selectable_value(&mut self.tab, Tab::Files, tr("Vista de archivos", "File view"));
                ui.selectable_value(&mut self.tab, Tab::Dups, tr("Duplicados", "Duplicates"));
                ui.selectable_value(&mut self.tab, Tab::Insights, tr("Limpieza", "Cleanup"));
                if !self.ai.answers.is_empty() {
                    ui.selectable_value(&mut self.tab, Tab::Ai, tr("Respuestas IA", "AI answers"));
                }
                let clear = tr("✖ quitar orden", "✖ clear sort");
                if self.tab == Tab::Tree && self.tree_sort.is_some() && ui.small_button(clear).clicked() {
                    self.tree_sort = None;
                    self.rows_dirty = true;
                }
                if self.tab == Tab::Files && self.files_sort.is_some() && ui.small_button(clear).clicked() {
                    self.files_sort = None;
                }
            });
            ui.separator();
            match self.tab {
                Tab::Tree => self.tree_ui(ui, &ctx, &tree),
                Tab::Files => self.files_ui(ui, &ctx, &tree),
                Tab::Dups => self.dups_ui(ui, &ctx, &tree),
                Tab::Ai => self.ai_ui(ui),
                Tab::Insights => {
                    use crate::insights::Action;
                    for a in self.insights.ui(ui, &ctx, &tree, self.view_root, self.epoch, self.revision) {
                        match a {
                            Action::Reveal(id) => {
                                self.tab = Tab::Tree;
                                self.select(&ctx, &tree, id, true);
                            }
                            Action::Menu(id) => self.open_menu(&ctx, MenuTarget::Node(id)),
                            Action::Reload => self.start_load(&ctx),
                            Action::Info(s) => self.set_info(s),
                            Action::Error(e) => self.set_error(e),
                        }
                    }
                }
            }
        });
        self.menu_ui(&ctx);
        // Sorting / expanding marks the rows dirty during this frame: draw the
        // result right away instead of waiting for the next mouse move.
        if self.rows_dirty {
            ctx.request_repaint();
        }
    }
}

// ---------------------------------------------------------------- panels

impl App {
    fn toolbar(&mut self, ui: &mut egui::Ui, ctx: &egui::Context, tree: Option<&Tree>) {
        ui.horizontal(|ui| {
            let busy = self.loading.is_some();
            if ui
                .add_enabled(!busy, egui::Button::new(tr("⟳ Actualizar", "⟳ Refresh")))
                .on_hover_text(tr("F5 — vuelve a leer el índice de Everything", "F5 — re-read the Everything index"))
                .clicked()
            {
                self.start_load(ctx);
            }
            if let Some(tree) = tree {
                if ui
                    .add_enabled(self.export_job.is_none(), egui::Button::new(tr("⇩ Exportar CSV", "⇩ Export CSV")))
                    .on_hover_text(tr("Exporta la vista actual (carpetas y archivos)", "Export the current view (folders and files)"))
                    .clicked()
                {
                    self.start_export(ctx);
                }
                ui.separator();
                self.breadcrumbs(ui, ctx, tree);
            }
            ui.with_layout(ELayout::right_to_left(Align::Center), |ui| {
                self.options_menu(ui, ctx, tree);
                ui.separator();
                let (t0, e0, r0) = (self.settings.show_treemap, self.settings.show_exts, self.settings.rings);
                ui.selectable_value(&mut self.settings.rings, true, tr("◎ Anillos", "◎ Rings"))
                    .on_hover_text(tr("Gráfico de anillos tipo Baobab: clic en un anillo para entrar, clic en el centro para subir", "Baobab-style ring chart: click a ring to zoom in, click the center to go up"));
                ui.selectable_value(&mut self.settings.rings, false, "▦ Treemap");
                ui.separator();
                ui.toggle_value(&mut self.settings.show_exts, tr("Extensiones", "Extensions"))
                    .on_hover_text(tr("Mostrar u ocultar el panel de tipos de archivo", "Show or hide the file types panel"));
                ui.toggle_value(&mut self.settings.show_treemap, tr("Gráfico", "Chart"))
                    .on_hover_text(tr("Mostrar u ocultar el gráfico (treemap / anillos)", "Show or hide the chart (treemap / rings)"));
                ui.label(tr("Paneles:", "Panels:"));
                if (t0, e0, r0) != (self.settings.show_treemap, self.settings.show_exts, self.settings.rings) {
                    self.settings.save();
                }
            });
        });
    }

    /// Drive selector + clickable path, like WizTree's address bar.
    fn breadcrumbs(&mut self, ui: &mut egui::Ui, ctx: &egui::Context, tree: &Tree) {
        let anc = tree.ancestors(self.view_root);
        let drive = anc.get(1).copied().unwrap_or(ROOT);
        let label = |id: u32| format!("{}  ·  {}", tree.drive_label(id), fmt_size(tree.nodes[id as usize].size));
        let mut target = None;
        egui::ComboBox::from_id_salt("drive")
            .selected_text(if drive == ROOT { tr("Todas", "All").to_string() } else { tree.drive_label(drive) })
            .width(260.0)
            .show_ui(ui, |ui| {
                let all = format!("{}  {}", tr("Todas", "All"), fmt_size(tree.nodes[ROOT as usize].size));
                if ui.selectable_label(drive == ROOT, all).clicked() {
                    target = Some(ROOT);
                }
                for &d in &tree.drives {
                    let mut text = RichText::new(label(d));
                    if tree.is_excluded(d) {
                        text = text.weak().italics();
                    }
                    if ui.selectable_label(d == drive, text).clicked() {
                        target = Some(d);
                    }
                }
            });
        if self.view_root != ROOT
            && ui.small_button("⬆").on_hover_text(tr("Subir un nivel (Retroceso)", "Up one level (Backspace)")).clicked()
        {
            target = Some(tree.nodes[self.view_root as usize].parent);
        }
        for &a in anc.iter().skip(2) {
            ui.label(RichText::new("›").weak());
            let current = a == self.view_root;
            let text = RichText::new(tree.name(a));
            if ui.add(egui::Button::new(if current { text.strong() } else { text }).frame(false)).clicked() && !current {
                target = Some(a);
            }
        }
        if let Some(t) = target {
            if t == ROOT || tree.nodes[t as usize].parent == ROOT {
                self.expanded.clear();
                self.selected = None;
            }
            self.set_root(ctx, t);
        }
    }

    fn options_menu(&mut self, ui: &mut egui::Ui, ctx: &egui::Context, tree: Option<&Tree>) {
        ui.menu_button(tr("⚙ Opciones", "⚙ Options"), |ui| {
            ui.set_min_width(230.0);
            // Language
            ui.label(RichText::new(tr("Idioma", "Language")).strong());
            let mut lang = self.settings.lang;
            ui.horizontal(|ui| {
                ui.selectable_value(&mut lang, Lang::Es, "Español");
                ui.selectable_value(&mut lang, Lang::En, "English");
            });
            if lang != self.settings.lang {
                self.settings.lang = lang;
                i18n::set_lang(lang);
                if let Some(items) = &self.tray_items {
                    items.relabel();
                }
                self.settings.save();
            }
            ui.separator();
            // Theme
            ui.label(RichText::new(tr("Tema", "Theme")).strong());
            let mut theme = self.settings.theme;
            ui.horizontal(|ui| {
                ui.selectable_value(&mut theme, Theme::System, tr("Sistema", "System"));
                ui.selectable_value(&mut theme, Theme::Light, tr("☀ Claro", "☀ Light"));
                ui.selectable_value(&mut theme, Theme::Dark, tr("☾ Oscuro", "☾ Dark"));
            });
            if theme != self.settings.theme {
                self.settings.theme = theme;
                apply_theme(ctx, theme);
                self.settings.save();
            }
            ui.separator();
            // Behaviour
            let mut a = self.autostart;
            if ui.checkbox(&mut a, tr("Iniciar con Windows", "Start with Windows")).changed() {
                self.toggle_autostart();
            }
            if ui
                .add_enabled(self.has_tray, egui::Checkbox::new(&mut self.settings.close_to_tray, tr("Cerrar a la bandeja", "Close to tray")))
                .on_hover_text(tr(
                    "Al cerrar la ventana queda en la bandeja del sistema con el índice cargado",
                    "Closing the window keeps the app in the system tray with the index loaded",
                ))
                .changed()
            {
                self.settings.save();
            }
            self.groq_options(ui);
            // Drives counted in "All"
            if let Some(tree) = tree {
                ui.separator();
                ui.label(RichText::new(tr("Unidades incluidas en \"Todas\"", "Drives included in \"All\"")).strong());
                for &d in &tree.drives {
                    let name = tree.name(d).to_string();
                    let mut inc = !self.settings.excluded_drives.iter().any(|x| x.eq_ignore_ascii_case(&name));
                    let text = format!("{}  ·  {}", tree.drive_label(d), fmt_size(tree.nodes[d as usize].size));
                    if ui.checkbox(&mut inc, text).changed() {
                        self.settings.excluded_drives.retain(|x| !x.eq_ignore_ascii_case(&name));
                        if !inc {
                            self.settings.excluded_drives.push(name);
                        }
                        self.exclusion_dirty = true;
                        self.settings.save();
                    }
                }
            }
        });
    }

    fn status_bar(&mut self, ui: &mut egui::Ui, tree: Option<&Tree>) {
        ui.horizontal(|ui| {
            if let Some(l) = &self.loading {
                ui.spinner();
                if l.progress.waiting.load(Ordering::Relaxed) {
                    ui.label(tr("Esperando a que Everything esté listo…", "Waiting for Everything to be ready…"));
                } else {
                    let (d, t) = (l.progress.done.load(Ordering::Relaxed), l.progress.total.load(Ordering::Relaxed));
                    ui.label(format!(
                        "{} {} / {}  ({} s)",
                        tr("Leyendo índice de Everything…", "Reading Everything index…"),
                        fmt_int(d as u64),
                        fmt_int(t as u64),
                        fmt_dec(l.started.elapsed().as_secs_f64())
                    ));
                }
                ui.separator();
            }
            if let Some(j) = &self.real_job {
                ui.spinner();
                ui.label(format!(
                    "{} {} / {}",
                    tr("Tamaño en disco real…", "Real on-disk size…"),
                    fmt_int(j.done.load(Ordering::Relaxed)),
                    fmt_int(j.total)
                ));
                ui.separator();
            }
            if let Some(j) = &self.export_job {
                ui.spinner();
                ui.label(format!("{} {}", tr("Exportando CSV…", "Exporting CSV…"), fmt_int(j.done.load(Ordering::Relaxed))));
                ui.separator();
            }
            if let Some(e) = &self.error {
                ui.colored_label(Color32::LIGHT_RED, e);
                ui.separator();
            }
            if let Some((msg, t)) = &self.info
                && t.elapsed() < Duration::from_secs(15) {
                    ui.label(RichText::new(msg).color(ui.visuals().warn_fg_color));
                    ui.separator();
                }
            if let Some(tree) = tree {
                let space = tree.drive_of(self.view_root).and_then(|d| {
                    let fresh = matches!(self.disk_cache, Some((cd, at, _)) if cd == d && at.elapsed() < Duration::from_secs(10));
                    if !fresh {
                        self.disk_cache = Some((d, Instant::now(), tree::disk_space(tree.name(d))));
                    }
                    self.disk_cache.and_then(|c| c.2).map(|s| (d, s))
                });
                if let Some((d, (total, free))) = space {
                    let free = free.min(total);
                    ui.label(format!(
                        "{}  {} {} {} {} · {} {}",
                        tree.full_path(d),
                        tr("Usado", "Used"),
                        fmt_size(total - free),
                        tr("de", "of"),
                        fmt_size(total),
                        tr("Libre", "Free"),
                        fmt_size(free)
                    ));
                    ui.separator();
                }
                ui.label(format!(
                    "{} {} · {} {} s · {} · {} MB",
                    fmt_int(tree.nodes.len() as u64 - 1),
                    tr("elementos", "items"),
                    tr("cargado en", "loaded in"),
                    fmt_dec(tree.load_ms as f64 / 1000.0),
                    fmt_age(tree.built_at.elapsed()),
                    fmt_int(tree.memory() as u64 / (1024 * 1024)),
                ))
                .on_hover_text(tr(
                    "Los cambios se sincronizan cada 30 s; los borrados se reflejan al actualizar (F5, o solo cada 30 min)",
                    "Changes sync every 30 s; deletions show up on refresh (F5, or automatically every 30 min)",
                ));
                if let Some(h) = self.hovered {
                    ui.separator();
                    ui.label(format!("{}  ({})", tree.full_path(h), fmt_size(tree.nodes[h as usize].size)));
                }
            }
        });
    }

    fn tree_ui(&mut self, ui: &mut egui::Ui, ctx: &egui::Context, tree: &Tree) {
        if self.dups.pick {
            ui.horizontal(|ui| {
                ui.label(RichText::new(tr("⊘ Tildá las carpetas que la búsqueda de duplicados debe ignorar.", "⊘ Tick the folders the duplicate search must ignore.")).strong());
                if ui.button(tr("Listo", "Done")).clicked() {
                    self.dups.pick = false;
                    self.tab = Tab::Dups;
                }
            });
        }
        let mut toggle_excl: Option<String> = None;
        let row_h = 20.0;
        let mut table = TableBuilder::new(ui)
            .striped(true)
            .resizable(true)
            .sense(Sense::click())
            .cell_layout(ELayout::left_to_right(Align::Center))
            .column(Column::remainder().at_least(140.0).clip(true))
            .column(Column::initial(110.0).range(50.0..=220.0).clip(true))
            .column(Column::initial(80.0).range(55.0..=140.0).clip(true))
            .column(Column::initial(80.0).range(55.0..=140.0).clip(true))
            .column(Column::initial(76.0).range(50.0..=130.0).clip(true))
            .column(Column::initial(76.0).range(50.0..=130.0).clip(true))
            .column(Column::initial(66.0).range(45.0..=120.0).clip(true))
            .column(Column::initial(112.0).range(70.0..=160.0).clip(true));
        if self.scroll_to_selected {
            if let Some(i) = self.selected.and_then(|s| self.rows.iter().position(|r| r.0 == s)) {
                table = table.scroll_to_row(i, Some(Align::Center));
            }
            self.scroll_to_selected = false;
        }
        let mut toggle = None;
        let mut select = None;
        let mut action = None;
        let mut sort = self.tree_sort;
        table
            .header(22.0, |mut h| {
                for (t, c) in [
                    (tr("Carpeta", "Folder"), Col::Name),
                    (tr("% del padre", "% of parent"), Col::Pct),
                    (tr("Tamaño", "Size"), Col::Size),
                    (tr("Asignado*", "Allocated*"), Col::Alloc),
                    (tr("Elementos", "Items"), Col::Items),
                    (tr("Archivos", "Files"), Col::Files),
                    (tr("Carpetas", "Folders"), Col::Folders),
                    (tr("Modificado", "Modified"), Col::Modified),
                ] {
                    h.col(|ui| {
                        let r = sort_header(ui, t, c, &mut sort);
                        if c == Col::Alloc && !r {
                            ui.response().on_hover_text(tr(
                                "* Estimado (redondeado al clúster). Al entrar a una carpeta se calcula el tamaño real en disco.",
                                "* Estimated (rounded to the cluster). Entering a folder computes the real on-disk size.",
                            ));
                        }
                    });
                }
            })
            .body(|body| {
                body.rows(row_h, self.rows.len(), |mut row| {
                    let (id, depth) = self.rows[row.index()];
                    let n = &tree.nodes[id as usize];
                    row.set_selected(self.selected == Some(id));
                    row.col(|ui| {
                        ui.spacing_mut().item_spacing.x = 4.0;
                        ui.add_space(depth as f32 * 14.0);
                        if n.is_dir() && !tree.children(id).is_empty() {
                            if toggle_arrow(ui, self.expanded.contains(&id)).clicked() {
                                toggle = Some(id);
                            }
                        } else {
                            ui.add_space(14.0);
                        }
                        if self.dups.pick && n.is_dir() && id != ROOT {
                            let path = tree.full_path(id);
                            match self.dup_excluded(&path) {
                                Some(false) => {
                                    // Inside an excluded folder: shown ticked, not editable here.
                                    ui.add_enabled(false, egui::Checkbox::without_text(&mut true));
                                }
                                state => {
                                    let mut on = state == Some(true);
                                    if ui.checkbox(&mut on, "").on_hover_text(tr("Excluir de duplicados", "Exclude from duplicates")).changed() {
                                        toggle_excl = Some(path);
                                    }
                                }
                            }
                        }
                        if id == ROOT || n.parent == ROOT {
                            drive_icon(ui);
                        } else if n.is_dir() {
                            folder_icon(ui);
                        } else {
                            file_icon(ui, tree.exts[n.ext as usize].color);
                        }
                        let name = if id == ROOT {
                            tr("Este equipo", "This PC").to_string()
                        } else if n.parent == ROOT {
                            tree.drive_label(id)
                        } else if id == self.view_root {
                            tree.full_path(id)
                        } else {
                            tree.name(id).to_string()
                        };
                        ui.label(name);
                    });
                    row.col(|ui| {
                        let parent = if id == self.view_root { None } else { Some(n.parent) };
                        let pct = parent
                            .map(|p| tree.nodes[p as usize].size)
                            .filter(|&ps| ps > 0)
                            .map(|ps| n.size as f32 / ps as f32)
                            .unwrap_or(1.0);
                        pct_bar(ui, pct);
                    });
                    row.col(|ui| right(ui, fmt_size(n.size)));
                    row.col(|ui| right(ui, fmt_size(n.alloc)));
                    row.col(|ui| right(ui, if n.is_dir() { fmt_int((n.files + n.folders) as u64) } else { String::new() }));
                    row.col(|ui| right(ui, if n.is_dir() { fmt_int(n.files as u64) } else { String::new() }));
                    row.col(|ui| right(ui, if n.is_dir() { fmt_int(n.folders as u64) } else { String::new() }));
                    row.col(|ui| {
                        ui.label(fmt_date(n.modified));
                    });
                    let resp = row.response();
                    if resp.clicked() {
                        select = Some(id);
                    }
                    if resp.double_clicked() && n.is_dir() {
                        toggle = Some(id);
                    }
                    if resp.secondary_clicked() {
                        action = Some(MenuAction::Menu(id));
                    }
                });
            });
        if let Some(p) = toggle_excl {
            self.toggle_dup_exclude(&p);
        }
        if sort != self.tree_sort {
            self.tree_sort = sort;
            self.rows_dirty = true;
        }
        if let Some(t) = toggle {
            if !self.expanded.remove(&t) {
                self.expanded.insert(t);
            }
            self.rows_dirty = true;
        }
        if let Some(s) = select {
            self.selected = Some(s);
            if self.view_root == ROOT && tree.nodes[s as usize].parent == ROOT {
                self.set_root(ctx, s);
                self.selected = Some(s);
            }
        }
        if let Some(a) = action {
            self.run_action(ctx, a);
        }
    }

    fn run_action(&mut self, ctx: &egui::Context, a: MenuAction) {
        match a {
            MenuAction::Menu(id) => self.open_menu(ctx, MenuTarget::Node(id)),
            MenuAction::ToggleExclude(path) => {
                self.toggle_dup_exclude(&path);
                let msg = if self.dup_excluded(&path) == Some(true) {
                    tr("Excluida de duplicados:", "Excluded from duplicates:")
                } else {
                    tr("Incluida de nuevo en duplicados:", "Included in duplicates again:")
                };
                self.set_info(format!("{msg} {path}"));
            }
            MenuAction::Reveal(path) => {
                if let Some(tree) = self.tree.clone()
                    && let Some(id) = tree.find_path(&path)
                {
                    self.tab = Tab::Tree;
                    self.select(ctx, &tree, id, true);
                }
            }
        }
    }

    fn open_menu(&mut self, ctx: &egui::Context, target: MenuTarget) {
        let pos = ctx.pointer_latest_pos().unwrap_or_default();
        self.ctx_menu = Some((target, pos));
    }

    /// Draw the open right-click menu (if any) on top of everything.
    fn menu_ui(&mut self, ctx: &egui::Context) {
        let Some((target, pos)) = self.ctx_menu.clone() else { return };
        let tree = self.tree.clone();
        let path = match &target {
            MenuTarget::Node(id) => tree.as_ref().map(|t| t.full_path(*id)).unwrap_or_default(),
            MenuTarget::Path(p) => p.clone(),
        };
        let excluded = self.dup_excluded(&path);
        let area = egui::Area::new(egui::Id::unique("wiz_ctx_menu"))
            .order(egui::Order::Foreground)
            .fixed_pos(pos)
            .constrain(true)
            .show(ctx, |ui| egui::Frame::popup(ui.style()).show(ui, |ui| context_menu(ui, tree.as_deref(), &target, excluded)).inner);
        let (action, mut close) = area.inner;
        let clicked_outside = ctx.input(|i| i.pointer.any_pressed())
            && ctx.pointer_latest_pos().is_some_and(|p| !area.response.rect.contains(p));
        if clicked_outside || ctx.input(|i| i.key_pressed(Key::Escape)) {
            close = true;
        }
        if close {
            self.ctx_menu = None;
        }
        if let Some(a) = action {
            self.run_action(ctx, a);
        }
    }

    fn files_ui(&mut self, ui: &mut egui::Ui, ctx: &egui::Context, tree: &Tree) {
        ui.horizontal(|ui| {
            ui.label(tr("Filtrar:", "Filter:"));
            ui.text_edit_singleline(&mut self.file_filter);
            ui.label(RichText::new(tr("los 1000 archivos más grandes de la vista", "the 1000 largest files in view")).weak());
        });
        let key = (self.view_root, self.revision, self.file_filter.clone(), self.files_sort);
        if key != self.files_key {
            if (key.0, key.1, &key.2) != (self.files_key.0, self.files_key.1, &self.files_key.2) {
                self.files = tree.largest_files(self.view_root, &self.file_filter, 1000);
            }
            match self.files_sort {
                Some((Col::Path, asc)) => {
                    self.files.sort_by_cached_key(|&id| tree.full_path(tree.nodes[id as usize].parent).to_lowercase());
                    if !asc {
                        self.files.reverse();
                    }
                }
                Some((col, asc)) => sort_nodes(tree, &mut self.files, col, asc),
                None => sort_nodes(tree, &mut self.files, Col::Size, false),
            }
            self.files_key = key;
        }
        let mut select = None;
        let mut action = None;
        let mut sort = self.files_sort;
        TableBuilder::new(ui)
            .striped(true)
            .resizable(true)
            .sense(Sense::click())
            .cell_layout(ELayout::left_to_right(Align::Center))
            .column(Column::initial(240.0).range(100.0..=600.0).clip(true))
            .column(Column::initial(80.0).range(55.0..=140.0).clip(true))
            .column(Column::initial(80.0).range(55.0..=140.0).clip(true))
            .column(Column::initial(112.0).range(70.0..=160.0).clip(true))
            .column(Column::remainder().at_least(100.0).clip(true))
            .header(22.0, |mut h| {
                for (t, c) in [
                    (tr("Nombre", "Name"), Col::Name),
                    (tr("Tamaño", "Size"), Col::Size),
                    (tr("Asignado*", "Allocated*"), Col::Alloc),
                    (tr("Modificado", "Modified"), Col::Modified),
                    (tr("Carpeta", "Folder"), Col::Path),
                ] {
                    h.col(|ui| {
                        sort_header(ui, t, c, &mut sort);
                    });
                }
            })
            .body(|body| {
                body.rows(20.0, self.files.len(), |mut row| {
                    let id = self.files[row.index()];
                    let n = &tree.nodes[id as usize];
                    row.set_selected(self.selected == Some(id));
                    row.col(|ui| {
                        file_icon(ui, tree.exts[n.ext as usize].color);
                        ui.label(tree.name(id));
                    });
                    row.col(|ui| right(ui, fmt_size(n.size)));
                    row.col(|ui| right(ui, fmt_size(n.alloc)));
                    row.col(|ui| {
                        ui.label(fmt_date(n.modified));
                    });
                    row.col(|ui| {
                        ui.label(tree.full_path(n.parent));
                    });
                    let resp = row.response();
                    if resp.clicked() {
                        select = Some(id);
                    }
                    if resp.secondary_clicked() {
                        action = Some(MenuAction::Menu(id));
                    }
                });
            });
        self.files_sort = sort;
        if let Some(s) = select {
            self.selected = Some(s);
        }
        if let Some(a) = action {
            self.run_action(ctx, a);
        }
    }

    // ------------------------------------------------------------ AI questions (Groq)

    fn groq_key(&self) -> Option<String> {
        crate::ai::unprotect(&self.settings.groq_key_enc).filter(|k| !k.is_empty())
    }

    fn ask_ai(&mut self, ctx: &egui::Context) {
        let (Some(key), Some(tree)) = (self.groq_key(), self.tree.clone()) else { return };
        let question = self.ai.question.trim().to_string();
        if question.is_empty() || self.ai.rx.is_some() {
            return;
        }
        let idx = self.ai.answers.len();
        self.ai.answers.push(AiAnswer { question: question.clone(), model: self.ai.model.clone().unwrap_or_default(), answer: None });
        self.tab = Tab::Ai;
        let known = self.ai.model.clone();
        self.ai.picking = known.is_none();
        let spanish = i18n::lang() == Lang::Es;
        let (tx, rx) = channel();
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let groq = crate::ai::Groq::new(key);
            let mut server = crate::mcp::Server::new(Box::new(move || Ok(tree.clone())));
            let r = crate::ai::ask_auto(&groq, known.as_deref(), &question, &mut server, spanish, &|m: &str| {
                let _ = tx.send(AiMsg::Model(Ok(m.to_string())));
            })
            .map(|(_, answer)| answer);
            let _ = tx.send(AiMsg::Answer(idx, r));
            ctx.request_repaint();
        });
        self.ai.rx = Some(rx);
    }

    fn poll_ai(&mut self) {
        let Some(rx) = &self.ai.rx else { return };
        let mut done = false;
        for msg in rx.try_iter() {
            match msg {
                AiMsg::Model(r) => {
                    self.ai.picking = false;
                    if let Ok(m) = r {
                        self.ai.model = Some(m);
                    }
                }
                AiMsg::Answer(i, r) => {
                    if let Some(a) = self.ai.answers.get_mut(i) {
                        a.model = self.ai.model.clone().unwrap_or_default();
                        a.answer = Some(r);
                    }
                    done = true;
                }
            }
        }
        if done {
            self.ai.rx = None;
            self.ai.picking = false;
        }
    }

    /// Question bar under the toolbar; the chosen model is shown next to the button.
    fn ask_bar(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        self.poll_ai();
        let has_key = !self.settings.groq_key_enc.is_empty();
        ui.horizontal(|ui| {
            ui.label(tr("Preguntar:", "Ask:"));
            let w = (ui.available_width() - 330.0).max(160.0);
            let edit = ui.add(
                egui::TextEdit::singleline(&mut self.ai.question)
                    .desired_width(w)
                    .hint_text(tr("¿Qué carpetas ocupan más en C:? ¿Cuántos GB de video tengo?", "Which folders use the most space on C:? How many GB of video do I have?")),
            );
            let enter = edit.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter));
            let busy = self.ai.rx.is_some();
            let can = has_key && !busy && self.tree.is_some() && !self.ai.question.trim().is_empty();
            if ui.add_enabled(can, egui::Button::new(tr("Buscar", "Search"))).clicked() || (enter && can) {
                self.ask_ai(ctx);
            }
            if busy {
                ui.spinner();
            }
            let note = if !has_key {
                tr("(cargá la clave de Groq en ⚙ Opciones)", "(add your Groq key in ⚙ Options)").to_string()
            } else if self.ai.picking {
                tr("(eligiendo modelo…)", "(choosing model…)").to_string()
            } else {
                match self.ai.model.as_deref() {
                    None => format!("({})", tr("modelo automático", "automatic model")),
                    Some(m) => match crate::ai::quota(m) {
                        Some(q) => format!(
                            "({m} · {} {} · {} {})",
                            q.req_left.map_or("?".into(), fmt_int),
                            tr("consultas/día", "requests/day"),
                            q.tok_left.map_or("?".into(), fmt_int),
                            tr("tokens/min", "tokens/min")
                        ),
                        None => format!("({m})"),
                    },
                }
            };
            ui.label(RichText::new(note).weak());
        });
    }

    fn ai_ui(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.strong(tr("Respuestas IA", "AI answers"));
            if self.ai.rx.is_none() && ui.small_button(tr("✖ Limpiar", "✖ Clear")).clicked() {
                self.ai.answers.clear();
                self.tab = Tab::Tree;
            }
        });
        ui.label(RichText::new(tr(
            "Responde con datos de tu índice usando herramientas de solo lectura; envía rutas y nombres de archivo a Groq.",
            "Answers from your index with read-only tools; paths and file names are sent to Groq.",
        )).weak());
        ui.separator();
        egui::ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
            for a in self.ai.answers.iter().rev() {
                ui.label(RichText::new(&a.question).strong());
                match &a.answer {
                    None => {
                        ui.horizontal(|ui| {
                            ui.spinner();
                            ui.label(tr("Pensando…", "Thinking…"));
                        });
                    }
                    Some(Ok(text)) => {
                        ui.add(egui::Label::new(text.as_str()).selectable(true).wrap());
                        if !a.model.is_empty() {
                            ui.label(RichText::new(format!("— {}", a.model)).weak().small());
                        }
                    }
                    Some(Err(e)) => {
                        ui.colored_label(ui.visuals().error_fg_color, e);
                    }
                }
                ui.separator();
            }
        });
    }

    fn groq_options(&mut self, ui: &mut egui::Ui) {
        ui.separator();
        ui.label(RichText::new(tr("Preguntas con IA (Groq)", "AI questions (Groq)")).strong());
        if !self.settings.groq_key_enc.is_empty() && !self.ai.key_editing {
            ui.label(format!("{} ••••••••••••  {}", tr("Clave:", "Key:"), tr("(cifrada con Windows)", "(encrypted by Windows)")));
            ui.horizontal(|ui| {
                if ui.small_button(tr("Cambiar", "Change")).clicked() {
                    self.ai.key_editing = true;
                    self.ai.key_input.clear();
                }
                if ui.small_button(tr("Borrar", "Remove")).clicked() {
                    self.settings.groq_key_enc.clear();
                    self.settings.save();
                    self.ai.model = None;
                }
            });
        } else if ui.button(tr("Cargar clave de Groq…", "Enter Groq key…")).clicked() {
            // Menus close on any click inside, so the key is typed in its own dialog.
            self.ai.key_editing = true;
            self.ai.key_input.clear();
            ui.close();
        }
    }

    /// Dialog to type the Groq key (masked); saved encrypted with DPAPI.
    fn groq_key_dialog(&mut self, ctx: &egui::Context) {
        if !self.ai.key_editing {
            return;
        }
        let resp = egui::Modal::new(egui::Id::unique("groq_key")).show(ctx, |ui| {
            ui.set_width(380.0);
            ui.heading(tr("Clave de Groq", "Groq key"));
            ui.label(tr("Se guarda cifrada con tu usuario de Windows (DPAPI).", "Stored encrypted with your Windows user (DPAPI)."));
            let edit = ui.add(egui::TextEdit::singleline(&mut self.ai.key_input).password(true).hint_text("gsk_…").desired_width(f32::INFINITY));
            edit.request_focus();
            let enter = edit.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter));
            if ui.link(tr("Obtener una clave en console.groq.com", "Get a key at console.groq.com")).clicked() {
                // eframe is built without its URL opener: use the Windows shell.
                shell_open("https://console.groq.com/keys", None);
            }
            ui.add_space(6.0);
            let mut close = false;
            ui.horizontal(|ui| {
                let ok = !self.ai.key_input.trim().is_empty();
                if (ui.add_enabled(ok, egui::Button::new(tr("Guardar", "Save"))).clicked() || (enter && ok))
                    && let Some(enc) = crate::ai::protect(self.ai.key_input.trim())
                {
                    self.settings.groq_key_enc = enc;
                    self.settings.save();
                    self.ai.model = None;
                    close = true;
                }
                if ui.button(tr("Cancelar", "Cancel")).clicked() {
                    close = true;
                }
            });
            close
        });
        if resp.inner || resp.should_close() {
            self.ai.key_editing = false;
            self.ai.key_input.clear();
        }
    }

    fn start_dups(&mut self, ctx: &egui::Context) {
        let Some(tree) = self.tree.clone() else { return };
        let opt = DupOptions {
            root: if self.dups.scope_all { ROOT } else { self.view_root },
            min_size: (self.dups.min_mb * 1024.0 * 1024.0) as u64,
            same_name: self.dups.same_name,
            verify: self.dups.verify && !self.dups.versions,
            versions: self.dups.versions,
            exclude: self.settings.dup_exclude.iter().filter_map(|p| tree.find_path(p)).collect(),
        };
        let prog = Arc::new(DupProgress::default());
        let p = prog.clone();
        let (tx, rx) = channel();
        let (done_tx, done_rx) = channel();
        let ctx = ctx.clone();
        std::thread::Builder::new()
            .name("duplicates".into())
            .spawn(move || {
                let pc = p.clone();
                let ctx2 = ctx.clone();
                let ticker = std::thread::spawn(move || {
                    while Arc::strong_count(&pc) > 2 {
                        ctx2.request_repaint();
                        std::thread::sleep(Duration::from_millis(250));
                    }
                });
                let tx = std::sync::Mutex::new(tx);
                dup::find_streaming(tree, opt, &p, &|g| {
                    let _ = tx.lock().unwrap().send(g);
                });
                drop(p);
                let _ = done_tx.send(());
                ctx.request_repaint();
                let _ = ticker.join();
            })
            .expect("thread");
        self.dups.groups.clear();
        self.dups.marked.clear();
        self.dups.searched = true;
        self.dups.job = Some(DupJob { rx, done_rx, prog, started: Instant::now(), finished: None });
    }

    /// Is this folder (or one of its parents) excluded from the duplicate search?
    fn dup_excluded(&self, path: &str) -> Option<bool> {
        let lower = path.trim_end_matches('\\').to_lowercase();
        let direct = self.settings.dup_exclude.iter().any(|e| e.trim_end_matches('\\').to_lowercase() == lower);
        if direct {
            return Some(true);
        }
        let inherited = self.settings.dup_exclude.iter().any(|e| {
            let e = e.trim_end_matches('\\').to_lowercase();
            lower.starts_with(&e) && lower[e.len()..].starts_with('\\')
        });
        inherited.then_some(false)
    }

    /// Add or remove a folder from the exclusions (saved immediately).
    fn toggle_dup_exclude(&mut self, path: &str) {
        let lower = path.trim_end_matches('\\').to_lowercase();
        let before = self.settings.dup_exclude.len();
        self.settings.dup_exclude.retain(|e| e.trim_end_matches('\\').to_lowercase() != lower);
        if self.settings.dup_exclude.len() == before {
            self.settings.dup_exclude.push(path.trim_end_matches('\\').to_string());
        }
        self.dups.excl_text = self.settings.dup_exclude.join("\n");
        self.settings.save();
    }

    /// Smart marking: in each group keep the copy that looks most "official"
    /// (preferred drive, organized personal folder, not a backup/temp/copy-named
    /// file), newest on ties; mark the rest. Works within a single drive too.
    fn dups_mark_smart(&mut self) {
        self.dups.marked.clear();
        self.dups.keep_reason.clear();
        let prefer = if self.settings.dup_prefer.is_empty() { String::new() } else { format!("{}\\", self.settings.dup_prefer.to_lowercase()) };
        for folders in [true, false] {
            let mut add = Vec::new();
            let mut reasons = Vec::new();
            for g in self.dups.groups.iter().filter(|g| g.folder == folders) {
                let ok: Vec<usize> = (0..g.files.len()).filter(|&i| !under_marked(&self.dups.marked, &g.files[i].path)).collect();
                let pool = if ok.is_empty() { (0..g.files.len()).collect() } else { ok };
                let scored: Vec<(i32, u64, usize, &'static str)> = pool
                    .iter()
                    .map(|&i| {
                        let (s, w) = keep_score(&g.files[i].path, &prefer);
                        (s, g.files[i].modified, i, w)
                    })
                    .collect();
                let Some(&(best, _, keep, why)) = scored.iter().max_by_key(|x| (x.0, x.1)) else { continue };
                // Explain with the winner's bonus, else with what was wrong with the others.
                let reason = if best > 0 && !why.is_empty() {
                    why.to_string()
                } else {
                    scored
                        .iter()
                        .filter(|x| x.2 != keep && x.0 < best)
                        .map(|x| x.3)
                        .find(|w| !w.is_empty())
                        .map_or_else(|| "la más nueva".to_string(), String::from)
                };
                reasons.push((g.files[keep].path.clone(), reason));
                for (i, f) in g.files.iter().enumerate() {
                    if i != keep {
                        add.push(f.path.clone());
                    }
                }
            }
            self.dups.marked.extend(add);
            self.dups.keep_reason.extend(reasons);
        }
    }

    fn dups_mark(&mut self, keep_oldest: bool) {
        self.dups.keep_reason.clear();
        self.dups.marked.clear();
        let prefer = if self.settings.dup_prefer.is_empty() { String::new() } else { format!("{}\\", self.settings.dup_prefer.to_lowercase()) };
        let pick = |g: &DupGroup, avoid: &HashSet<String>| -> Option<usize> {
            // Prefer a copy that is not inside an already-marked folder.
            let ok: Vec<usize> = (0..g.files.len()).filter(|&i| !under_marked(avoid, &g.files[i].path)).collect();
            let mut pool = if ok.is_empty() { (0..g.files.len()).collect() } else { ok };
            // The preferred drive always wins (e.g. Google Drive over old backups).
            if !prefer.is_empty() {
                let on_pref: Vec<usize> = pool.iter().copied().filter(|&i| g.files[i].path.to_lowercase().starts_with(&prefer)).collect();
                if !on_pref.is_empty() {
                    pool = on_pref;
                }
            }
            if keep_oldest {
                pool.into_iter().min_by_key(|&i| g.files[i].modified)
            } else {
                pool.into_iter().max_by_key(|&i| g.files[i].modified)
            }
        };
        // Folders first, then files, so a file's surviving copy never sits in a marked folder.
        for folders in [true, false] {
            let mut add = Vec::new();
            for g in self.dups.groups.iter().filter(|g| g.folder == folders) {
                let keep = pick(g, &self.dups.marked);
                for (i, f) in g.files.iter().enumerate() {
                    if Some(i) != keep {
                        add.push(f.path.clone());
                    }
                }
            }
            self.dups.marked.extend(add);
        }
    }
    fn dups_ui(&mut self, ui: &mut egui::Ui, ctx: &egui::Context, tree: &Tree) {
        // --- options
        ui.horizontal_wrapped(|ui| {
            let opts_before = (self.dups.min_mb, self.dups.same_name, self.dups.scope_all);
            ui.label(tr("Buscar en:", "Search in:"));
            let here = if self.view_root == ROOT { tr("Todas", "All").to_string() } else { tree.full_path(self.view_root) };
            ui.radio_value(&mut self.dups.scope_all, false, format!("{} ({here})", tr("vista actual", "current view")));
            ui.radio_value(&mut self.dups.scope_all, true, tr("todas las unidades incluidas", "all included drives"))
                .on_hover_text(tr(
                    "Las unidades excluidas en ⚙ Opciones no se analizan (útil para unidades en la nube)",
                    "Drives excluded in ⚙ Options are skipped (useful for cloud drives)",
                ));
            ui.separator();
            ui.label(tr("Tamaño mínimo", "Minimum size"));
            ui.add(egui::DragValue::new(&mut self.dups.min_mb).range(0.0..=1_000_000.0).speed(0.5).suffix(" MB"));
            ui.separator();
            ui.label(tr("Modo:", "Mode:"));
            let mode_before = (self.dups.verify, self.dups.versions);
            if ui.selectable_label(!self.dups.verify && !self.dups.versions, tr("Rápido", "Quick")).clicked() {
                (self.dups.verify, self.dups.versions) = (false, false);
            }
            ui.label(RichText::new("?").weak()).on_hover_text(tr(
                "Rápido: solo nombre y tamaño, desde el índice. No lee ningún archivo: instantáneo y seguro con Google Drive en streaming. Resultados sin verificar.",
                "Name and size only, from the index. Reads no file: instant and safe with streamed Google Drive. Results are unverified.",
            ));
            if ui
                .selectable_label(self.dups.verify && !self.dups.versions, tr("Avanzado", "Advanced"))
                .on_hover_text(tr(
                    "Confirma el contenido con hash (lee los archivos; omite los que están solo en la nube).",
                    "Confirms content with hashes (reads the files; skips cloud-only ones).",
                ))
                .clicked()
            {
                (self.dups.verify, self.dups.versions) = (true, false);
            }
            if ui
                .selectable_label(self.dups.versions, tr("Versiones", "Versions"))
                .on_hover_text(tr(
                    "Mismo nombre dentro de una carpeta del mismo nombre, en cualquier unidad, aunque cambien tamaño y contenido (copias viejas). No lee archivos.",
                    "Same name inside a same-named folder, on any drive, even if size and content differ (old copies). Reads no file.",
                ))
                .clicked()
            {
                self.dups.versions = true;
            }
            if mode_before != (self.dups.verify, self.dups.versions)
                && let Some(job) = self.dups.job.take()
            {
                job.prog.cancel.store(true, Ordering::Relaxed);
                self.start_dups(ctx);
            }
            ui.separator();
            ui.label(tr("Preferir:", "Prefer:")).on_hover_text(tr(
                "Al marcar, se conserva siempre la copia de esta unidad (por ejemplo tu Google Drive); si no hay, la más nueva.",
                "When marking, the copy on this drive always wins (e.g. your Google Drive); otherwise the newest.",
            ));
            let mut prefer = self.settings.dup_prefer.clone();
            egui::ComboBox::from_id_salt("dup_prefer")
                .selected_text(if prefer.is_empty() { tr("ninguna", "none").to_string() } else { tree.find_path(&prefer).map_or(prefer.clone(), |d| tree.drive_label(d)) })
                .width(170.0)
                .show_ui(ui, |ui| {
                    ui.selectable_value(&mut prefer, String::new(), tr("ninguna", "none"));
                    for &d in &tree.drives {
                        ui.selectable_value(&mut prefer, tree.name(d).to_string(), tree.drive_label(d));
                    }
                });
            if prefer != self.settings.dup_prefer {
                self.settings.dup_prefer = prefer;
                self.settings.save();
            }
            ui.add_enabled(self.dups.verify, egui::Checkbox::new(&mut self.dups.same_name, tr("Mismo nombre", "Same name"))).on_hover_text(tr(
                "Activado: solo compara archivos con igual nombre y tamaño (rápido).\nDesactivado: compara todos los de igual tamaño (encuentra copias renombradas).",
                "On: only compares files with the same name and size (fast).\nOff: compares every file of the same size (finds renamed copies).",
            ));
            // Changing an option while searching restarts the search with the new values.
            if opts_before != (self.dups.min_mb, self.dups.same_name, self.dups.scope_all)
                && let Some(job) = self.dups.job.take()
            {
                job.prog.cancel.store(true, Ordering::Relaxed);
                self.start_dups(ctx);
            }
            ui.separator();
            if let Some(job) = &self.dups.job {
                if ui.button(tr("✖ Cancelar", "✖ Cancel")).clicked() {
                    job.prog.cancel.store(true, Ordering::Relaxed);
                    // Keep what was found; the worker winds down on its own.
                    self.dups.groups.extend(job.rx.try_iter());
                    self.dups.last_secs = job.started.elapsed().as_secs_f64();
                    self.dups.job = None;
                }
            } else {
                if ui.button(RichText::new(tr("Buscar duplicados", "Find duplicates")).strong()).clicked() {
                    self.start_dups(ctx);
                }
                if ui
                    .small_button(tr("Vaciar caché", "Clear cache"))
                    .on_hover_text(tr(
                        "Borra los hashes guardados. La próxima búsqueda avanzada vuelve a leer todo.",
                        "Deletes stored hashes. The next advanced search reads everything again.",
                    ))
                    .clicked()
                {
                    crate::hashcache::global().lock().unwrap().clear();
                    self.set_info(tr("Caché de hashes vaciada", "Hash cache cleared").to_string());
                }
            }
        });

        // --- excluded folders: typed one per line, or ticked in the tree
        ui.horizontal(|ui| {
            let n = self.settings.dup_exclude.len();
            if ui
                .selectable_label(self.dups.show_excl, format!("⊘ {} ({n})", tr("Carpetas excluidas", "Excluded folders")))
                .on_hover_text(tr("Carpetas que la búsqueda nunca revisa. Se guardan entre sesiones.", "Folders the search never looks into. Kept between sessions."))
                .clicked()
            {
                self.dups.show_excl = !self.dups.show_excl;
                self.dups.excl_text = self.settings.dup_exclude.join("\n");
            }
            if ui
                .toggle_value(&mut self.dups.pick, tr("Marcar en el árbol", "Pick in tree"))
                .on_hover_text(tr("Muestra tildes junto a cada carpeta del árbol para excluirla", "Shows a checkbox next to every folder in the tree to exclude it"))
                .clicked()
                && self.dups.pick
            {
                self.tab = Tab::Tree;
            }
        });
        if self.dups.show_excl {
            ui.horizontal(|ui| {
                if ui.small_button(tr("✖ Vaciar", "✖ Clear")).on_hover_text(tr("Quita todas las carpetas excluidas", "Remove every excluded folder")).clicked() {
                    self.settings.dup_exclude.clear();
                    self.dups.excl_text.clear();
                    self.settings.save();
                }
                if ui
                    .small_button(tr("Sugeridas", "Suggested"))
                    .on_hover_text(suggested_exclusions().join("\n"))
                    .clicked()
                {
                    self.settings.dup_exclude = suggested_exclusions();
                    self.dups.excl_text = self.settings.dup_exclude.join("\n");
                    self.settings.save();
                }
            });
            ui.label(RichText::new(tr("Una ruta por renglón. También: clic derecho en una carpeta → Excluir de duplicados.", "One path per line. Also: right-click a folder → Exclude from duplicates.")).weak());
            let edit = ui.add(
                egui::TextEdit::multiline(&mut self.dups.excl_text)
                    .desired_rows(4)
                    .desired_width(f32::INFINITY)
                    .hint_text("C:\\Windows\nC:\\Program Files\nH:\\Mi unidad\\Backups"),
            );
            if edit.changed() {
                self.settings.dup_exclude =
                    self.dups.excl_text.lines().map(|l| l.trim().replace('/', "\\").trim_end_matches('\\').to_string()).filter(|l| !l.is_empty()).collect();
                self.settings.save();
            }
            let missing: Vec<&String> = self.settings.dup_exclude.iter().filter(|p| tree.find_path(p).is_none()).collect();
            if !missing.is_empty() {
                ui.colored_label(
                    ui.visuals().warn_fg_color,
                    format!("{} {}", tr("No están en el índice:", "Not in the index:"), missing.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(" · ")),
                );
            }
        }

        // --- progress
        if let Some(job) = &self.dups.job {
            let p = &job.prog;
            let stage = match p.stage.load(Ordering::Relaxed) {
                0 => tr("Agrupando candidatos con el índice…", "Grouping candidates from the index…"),
                _ => tr("Comparando contenido…", "Comparing contents…"),
            };
            let (bd, bt) = (p.bytes_done.load(Ordering::Relaxed), p.bytes_total.load(Ordering::Relaxed).max(1));
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label(format!(
                    "{stage}  {} / {} {} · {} / {} · {} s",
                    fmt_int(p.groups_done.load(Ordering::Relaxed)),
                    fmt_int(p.groups_total.load(Ordering::Relaxed)),
                    tr("grupos candidatos", "candidate groups"),
                    fmt_size(bd.min(bt)),
                    fmt_size(bt),
                    fmt_dec(job.started.elapsed().as_secs_f64())
                ));
                let hits = p.cache_hits.load(Ordering::Relaxed);
                ui.label(RichText::new(format!(
                    "· {} {} · {} {}",
                    fmt_size(p.bytes_read.load(Ordering::Relaxed)),
                    tr("leídos del disco", "read from disk"),
                    fmt_int(hits),
                    tr("desde caché", "from cache")
                )).weak());
            });
            ui.add(egui::ProgressBar::new((bd as f32 / bt as f32).min(1.0)).desired_height(6.0));
        }
        if !self.dups.searched {
            ui.add_space(8.0);
            ui.label(RichText::new(tr(
                "Busca archivos idénticos: primero por nombre y tamaño (instantáneo, con el índice de Everything), luego confirma por contenido con hash. Los archivos solo en la nube no se descargan.",
                "Finds identical files: first by name and size (instant, using Everything's index), then confirms by content hash. Cloud-only files are never downloaded.",
            )).weak());
            return;
        }

        // --- summary + actions
        let n_groups = self.dups.groups.len();
        let wasted: u64 = self.dups.groups.iter().map(|g| g.wasted()).sum();
        let files: usize = self.dups.groups.iter().map(|g| g.files.len()).sum();
        let marked_size: u64 = self
            .dups
            .groups
            .iter()
            .map(|g| g.size * g.files.iter().filter(|f| self.dups.marked.contains(&f.path)).count() as u64)
            .sum();
        ui.separator();
        ui.horizontal_wrapped(|ui| {
            ui.label(RichText::new(format!(
                "{} {} · {} {} · {} {}",
                fmt_int(n_groups as u64),
                tr("grupos", "groups"),
                fmt_int(files as u64),
                tr("archivos", "files"),
                fmt_size(wasted),
                tr("recuperables", "reclaimable")
            ))
            .strong());
            if self.dups.job.is_none() && self.dups.last_secs > 0.0 {
                ui.label(RichText::new(format!(
                    "({} s · {} {} · {} {})",
                    fmt_dec(self.dups.last_secs),
                    fmt_size(self.dups.last_read),
                    tr("leídos", "read"),
                    fmt_int(self.dups.last_hits),
                    tr("desde caché", "from cache")
                ))
                .weak());
            }
            ui.separator();
            ui.label(tr("Marcar:", "Mark:"));
            if ui
                .button(RichText::new(tr("✓ sugerir cuál conservar", "✓ suggest which to keep")).strong())
                .on_hover_text(tr(
                    "Conserva en cada grupo la copia más «oficial»: unidad preferida o carpeta personal (Documentos, Imágenes, Música…), no respaldos, temporales, Descargas ni nombres tipo «(1)»/«- copia»; si empatan, la más nueva. Pasá el mouse por ✓ para ver el motivo.",
                    "Keeps the most «official» copy in each group: preferred drive or personal folder (Documents, Pictures, Music…), not backups, temp, Downloads or «(1)»/«- copy» names; newest on ties. Hover ✓ to see why.",
                ))
                .clicked()
            {
                self.dups_mark_smart();
            }
            if ui.button(tr("conservar el más antiguo", "keep oldest")).clicked() {
                self.dups_mark(true);
            }
            if ui.button(tr("conservar el más nuevo", "keep newest")).clicked() {
                self.dups_mark(false);
            }
            if ui.button(tr("desmarcar", "clear")).clicked() {
                self.dups.marked.clear();
                self.dups.keep_reason.clear();
            }
            ui.separator();
            if ui.add_enabled(self.dups.export_rx.is_none(), egui::Button::new(tr("⇩ Exportar CSV", "⇩ Export CSV"))).clicked() {
                let groups: Vec<DupGroup> = self.dups.groups.clone();
                let (tx, rx) = channel();
                let ctx = ctx.clone();
                std::thread::spawn(move || {
                    let r = match save_dialog("WizEverything_duplicates.csv") {
                        None => Err(String::new()),
                        Some(p) => dup::write_csv(&groups, std::path::Path::new(&p)).map(|_| p).map_err(|e| e.to_string()),
                    };
                    let _ = tx.send(r);
                    ctx.request_repaint();
                });
                self.dups.export_rx = Some(rx);
            }
            let n = self.dups.marked.len();
            let label = format!("{} ({}, {})", tr("Enviar a la papelera", "Send to Recycle Bin"), fmt_int(n as u64), fmt_size(marked_size));
            if ui.add_enabled(n > 0 && self.dups.recycle_rx.is_none(), egui::Button::new(label)).clicked() {
                self.dups.confirm = true;
            }
        });

        // --- confirmation
        if self.dups.confirm {
            let all_marked = self
                .dups
                .groups
                .iter()
                .filter(|g| g.files.iter().all(|f| under_marked(&self.dups.marked, &f.path)))
                .count();
            let n = self.dups.marked.len();
            let resp = egui::Modal::new(egui::Id::unique("dup_confirm")).show(ctx, |ui| {
                ui.set_max_width(440.0);
                ui.heading(tr("Enviar a la papelera", "Send to Recycle Bin"));
                ui.label(format!(
                    "{} {} ({}).",
                    fmt_int(n as u64),
                    tr("archivos marcados irán a la papelera de reciclaje", "marked files will be moved to the Recycle Bin"),
                    fmt_size(marked_size)
                ));
                ui.label(RichText::new(tr(
                    "Si una unidad no tiene papelera, Windows pedirá confirmación antes de borrar definitivamente.",
                    "If a drive has no Recycle Bin, Windows will ask before deleting permanently.",
                ))
                .weak());
                let unverified = self.dups.groups.iter().any(|g| !g.verified && g.files.iter().any(|f| self.dups.marked.contains(&f.path)));
                if unverified {
                    ui.colored_label(
                        ui.visuals().warn_fg_color,
                        tr(
                            "Hay grupos sin verificar (modo rápido): comprobá el contenido antes de borrar, o usá el modo Avanzado.",
                            "Some groups are unverified (quick mode): check the content before deleting, or use Advanced mode.",
                        ),
                    );
                }
                if all_marked > 0 {
                    ui.colored_label(
                        ui.visuals().error_fg_color,
                        format!(
                            "{} {}",
                            fmt_int(all_marked as u64),
                            tr("grupos tienen TODAS sus copias marcadas. Desmarcá al menos una por grupo.", "groups have ALL copies marked. Unmark at least one per group.")
                        ),
                    );
                }
                ui.add_space(8.0);
                let mut go = false;
                ui.horizontal(|ui| {
                    if ui.add_enabled(all_marked == 0, egui::Button::new(tr("Enviar a la papelera", "Send to Recycle Bin"))).clicked() {
                        go = true;
                    }
                    if ui.button(tr("Cancelar", "Cancel")).clicked() {
                        self.dups.confirm = false;
                    }
                });
                go
            });
            if resp.should_close() {
                self.dups.confirm = false;
            }
            if resp.inner {
                self.dups.confirm = false;
                let paths: Vec<String> = self.dups.marked.iter().cloned().collect();
                let (tx, rx) = channel();
                let ctx = ctx.clone();
                std::thread::spawn(move || {
                    let r = dup::recycle(&paths);
                    let _ = tx.send((paths, r));
                    ctx.request_repaint();
                });
                self.dups.recycle_rx = Some(rx);
            }
        }

        // --- results: files on the left, whole folders on the right
        // Draggable split between files and folders, each side between 10% and 90%.
        let w = ui.available_width().max(200.0);
        egui::Panel::left("dup_files_pane")
            .resizable(true)
            .default_size(w * 0.5)
            .size_range(w * 0.1..=w * 0.9)
            .show(ui, |ui| self.dup_table(ui, ctx, tree, false));
        egui::CentralPanel::no_frame().show(ui, |ui| {
            ui.add_space(2.0);
            self.dup_table(ui, ctx, tree, true);
        });
    }

    /// One results table: a header row per duplicated item, then one row per path.
    fn dup_table(&mut self, ui: &mut egui::Ui, ctx: &egui::Context, tree: &Tree, folders: bool) {
        let k = usize::from(folders);
        let count = self.dups.groups.iter().filter(|g| g.folder == folders).count();
        let wasted: u64 = self.dups.groups.iter().filter(|g| g.folder == folders).map(|g| g.wasted()).sum();
        ui.horizontal(|ui| {
            if folders {
                folder_icon(ui);
                ui.strong(tr("Carpetas duplicadas", "Duplicate folders"));
            } else {
                file_icon(ui, [150, 150, 150]);
                ui.strong(tr("Archivos duplicados", "Duplicate files"));
            }
            ui.label(RichText::new(format!("{} · {} {}", fmt_int(count as u64), fmt_size(wasted), tr("recuperables", "reclaimable"))).weak());
        });
        let view = &mut self.dups.views[k];
        let order_key = (self.dups.groups.len(), view.sort, self.dups.min_mb.to_bits());
        if order_key != view.key {
            let groups = &self.dups.groups;
            let min = (self.dups.min_mb * 1024.0 * 1024.0) as u64;
            let mut order: Vec<usize> = (0..groups.len()).filter(|&i| groups[i].folder == folders && groups[i].size >= min).collect();
            if folders {
                // Hide nested matches: all copies sit inside the copies of another folder group.
                let mut owner: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
                for &i in &order {
                    for f in &groups[i].files {
                        owner.insert(f.path.to_lowercase(), i);
                    }
                }
                order.retain(|&i| {
                    let parents: HashSet<Option<usize>> = groups[i]
                        .files
                        .iter()
                        .map(|f| f.path.rsplit_once('\\').and_then(|(d, _)| owner.get(&d.to_lowercase()).copied()))
                        .collect();
                    !(parents.len() == 1 && parents.iter().next().is_some_and(|x| x.is_some_and(|g| g != i)))
                });
            }
            let (col, asc) = view.sort.unwrap_or((Col::Alloc, false));
            let newest = |g: &DupGroup| g.files.iter().map(|f| f.modified).max().unwrap_or(0);
            order.sort_by(|&a, &b| {
                let (x, y) = (&groups[a], &groups[b]);
                let o = match col {
                    Col::Name => cmp_ci(x.name(), y.name()),
                    Col::Count => x.files.len().cmp(&y.files.len()),
                    Col::Size => x.size.cmp(&y.size),
                    Col::Modified => newest(x).cmp(&newest(y)),
                    _ => x.wasted().cmp(&y.wasted()),
                };
                (if asc { o } else { o.reverse() }).then_with(|| y.wasted().cmp(&x.wasted()))
            });
            view.order = order;
            view.key = order_key;
        }
        let mut rows: Vec<(usize, Option<usize>)> = Vec::new();
        for &g in &self.dups.views[k].order {
            rows.push((g, None));
            rows.extend((0..self.dups.groups[g].files.len()).map(|i| (g, Some(i))));
        }
        let mut toggle: Option<String> = None;
        let mut reveal: Option<String> = None;
        let mut menu: Option<String> = None;
        let mut sort = self.dups.views[k].sort;
        ui.push_id(("dup_table", k), |ui| {
            TableBuilder::new(ui)
                .striped(true)
                .resizable(true)
                .sense(Sense::click())
                .cell_layout(ELayout::left_to_right(Align::Center))
                .column(Column::remainder().at_least(160.0).clip(true))
                .column(Column::initial(48.0))
                .column(Column::initial(72.0))
                .column(Column::initial(80.0))
                .column(Column::initial(105.0))
                .header(22.0, |mut h| {
                    let first = if folders { tr("Carpeta / ruta", "Folder / path") } else { tr("Archivo / ruta", "File / path") };
                    for (t, c) in [
                        (first, Col::Name),
                        (tr("Copias", "Copies"), Col::Count),
                        (tr("Tamaño", "Size"), Col::Size),
                        (tr("Recuperable", "Reclaimable"), Col::Alloc),
                        (tr("Modificado", "Modified"), Col::Modified),
                    ] {
                        h.col(|ui| {
                            sort_header(ui, t, c, &mut sort);
                        });
                    }
                })
                .body(|body| {
                    body.rows(20.0, rows.len(), |mut row| {
                        let (g, f) = rows[row.index()];
                        let grp = &self.dups.groups[g];
                        match f {
                            None => {
                                row.col(|ui| {
                                    if grp.folder {
                                        folder_icon(ui);
                                    } else {
                                        file_icon(ui, [150, 150, 150]);
                                    }
                                    let label = ui.label(RichText::new(grp.name()).strong());
                                    if grp.versions {
                                        ui.label(RichText::new("≠").weak()).on_hover_text(tr(
                                            "Versiones: mismo nombre y carpeta, el contenido puede diferir",
                                            "Versions: same name and folder, content may differ",
                                        ));
                                    } else if !grp.verified {
                                        ui.label(RichText::new("≈").weak()).on_hover_text(tr(
                                            "Sin verificar: coinciden nombre y tamaño (modo rápido)",
                                            "Unverified: name and size match (quick mode)",
                                        ));
                                    }
                                    if grp.folder {
                                        label.on_hover_text(format!("{} {}", fmt_int(grp.inner_files as u64), tr("archivos idénticos en cada copia", "identical files in each copy")));
                                    }
                                });
                                row.col(|ui| right(ui, fmt_int(grp.files.len() as u64)));
                                row.col(|ui| right(ui, fmt_size(grp.size)));
                                row.col(|ui| right(ui, fmt_size(grp.wasted())));
                                row.col(|_| {});
                            }
                            Some(i) => {
                                let file = &grp.files[i];
                                let shown = if grp.folder {
                                    file.path.as_str()
                                } else {
                                    file.path.rsplit_once('\\').map_or(file.path.as_str(), |(d, _)| d)
                                };
                                row.set_selected(self.dups.marked.contains(&file.path));
                                row.col(|ui| {
                                    ui.add_space(18.0);
                                    let mut m = self.dups.marked.contains(&file.path);
                                    if ui
                                        .checkbox(&mut m, "")
                                        .on_hover_text(tr("Marcar para enviar a la papelera", "Mark to send to the Recycle Bin"))
                                        .changed()
                                    {
                                        toggle = Some(file.path.clone());
                                    }
                                    if let Some(why) = self.dups.keep_reason.get(&file.path) {
                                        ui.label(RichText::new("✓").color(Color32::from_rgb(90, 190, 90)).strong())
                                            .on_hover_text(format!("{}: {why}", tr("Se conserva", "Kept")));
                                    }
                                    ui.label(shown).on_hover_text(&file.path);
                                });
                                row.col(|_| {});
                                row.col(|ui| right(ui, if grp.versions { fmt_size(file.size) } else { String::new() }));
                                row.col(|_| {});
                                row.col(|ui| {
                                    ui.label(fmt_date(file.modified));
                                });
                                let resp = row.response();
                                if resp.double_clicked() {
                                    reveal = Some(file.path.clone());
                                }
                                if resp.secondary_clicked() {
                                    menu = Some(file.path.clone());
                                }
                            }
                        }
                    });
                });
        });
        self.dups.views[k].sort = sort;
        if let Some(p) = toggle
            && !self.dups.marked.remove(&p)
        {
            self.dups.marked.insert(p);
        }
        if let Some(p) = menu {
            self.open_menu(ctx, MenuTarget::Path(p));
        }
        if let Some(p) = reveal
            && let Some(id) = tree.find_path(&p)
        {
            self.tab = Tab::Tree;
            self.select(ctx, tree, id, true);
        }
    }

    fn ext_ui(&mut self, ui: &mut egui::Ui, tree: &Tree) {
        let key = (self.view_root, self.revision, self.ext_sort);
        if key != self.ext_key {
            if (key.0, key.1) != (self.ext_key.0, self.ext_key.1) {
                self.ext_rows = tree.ext_stats(self.view_root);
            }
            let (col, asc) = self.ext_sort.unwrap_or((Col::Size, false));
            let cmp_ext = |a: &(u32, u64, u64), b: &(u32, u64, u64)| {
                let o = match col {
                    Col::Name => cmp_ci(tree.ext_label(a.0), tree.ext_label(b.0)),
                    Col::Count => a.2.cmp(&b.2),
                    _ => a.1.cmp(&b.1),
                };
                (if asc { o } else { o.reverse() }).then_with(|| b.1.cmp(&a.1))
            };
            self.ext_rows.sort_by(cmp_ext);
            // Categories: totals, same order rule, "Others" always last.
            let mut cats: Vec<(usize, u64, u64)> = (0..CATEGORIES.len()).map(|c| (c, 0, 0)).collect();
            for &(e, size, count) in &self.ext_rows {
                let c = category_of(&tree.exts[e as usize].name);
                cats[c].1 += size;
                cats[c].2 += count;
            }
            // Every category with files (Others included) follows the chosen order.
            let mut ordered: Vec<(usize, u64, u64)> = cats.iter().copied().filter(|c| c.2 > 0).collect();
            ordered.sort_by(|a, b| {
                let o = match col {
                    Col::Name => cmp_ci(category_name(a.0), category_name(b.0)),
                    Col::Count => a.2.cmp(&b.2),
                    _ => a.1.cmp(&b.1),
                };
                (if asc { o } else { o.reverse() }).then_with(|| b.1.cmp(&a.1))
            });
            self.ext_cats = ordered;
            self.ext_key = key;
        }
        let mut sort = self.ext_sort;
        let total = tree.nodes[self.view_root as usize].size.max(1) as f64;
        ui.horizontal(|ui| {
            ui.strong(tr("Tipos de archivo", "File types"));
            if self.highlight_ext.is_some() && ui.small_button(tr("✖ quitar resaltado", "✖ clear highlight")).clicked() {
                self.highlight_ext = None;
            }
            if self.ext_sort.is_some() && ui.small_button(tr("✖ quitar orden", "✖ clear sort")).clicked() {
                self.ext_sort = None;
                sort = None;
            }
        });

        // Rows: a category, then its extensions when expanded.
        enum Row {
            Cat(usize, u64, u64),
            Ext(u32, u64, u64),
        }
        let mut rows = Vec::new();
        for &(c, size, count) in &self.ext_cats {
            rows.push(Row::Cat(c, size, count));
            if self.ext_open.contains(&c) {
                for &(e, s, n) in &self.ext_rows {
                    if category_of(&tree.exts[e as usize].name) == c {
                        rows.push(Row::Ext(e, s, n));
                    }
                }
            }
        }
        let mut toggle_ext = None;
        let mut toggle_cat = None;
        // Fixed, non-resizable numeric columns; the name takes whatever is left,
        // so the table always fits the panel width.
        TableBuilder::new(ui)
            .striped(true)
            .resizable(false)
            .sense(Sense::click())
            .cell_layout(ELayout::left_to_right(Align::Center))
            .column(Column::remainder().at_least(70.0).clip(true))
            .column(Column::exact(50.0).clip(true))
            .column(Column::exact(68.0).clip(true))
            .column(Column::exact(62.0).clip(true))
            .header(22.0, |mut h| {
                for (t, c) in [
                    (tr("Tipo", "Type"), Col::Name),
                    ("%", Col::Pct),
                    (tr("Tamaño", "Size"), Col::Size),
                    (tr("Archivos", "Files"), Col::Count),
                ] {
                    h.col(|ui| {
                        sort_header(ui, t, c, &mut sort);
                    });
                }
            })
            .body(|body| {
                body.rows(20.0, rows.len(), |mut row| match rows[row.index()] {
                    Row::Cat(c, size, count) => {
                        let open = self.ext_open.contains(&c);
                        row.set_selected(self.highlight_ext == Some(CAT_FLAG | c as u32));
                        row.col(|ui| {
                            ui.spacing_mut().item_spacing.x = 4.0;
                            if toggle_arrow(ui, open).clicked() {
                                toggle_cat = Some(c);
                            }
                            ui.label(RichText::new(category_name(c)).strong());
                        });
                        row.col(|ui| right(ui, format!("{} %", fmt_dec(size as f64 / total * 100.0))));
                        row.col(|ui| right(ui, fmt_size(size)));
                        row.col(|ui| right(ui, fmt_int(count)));
                        if row.response().clicked() {
                            toggle_cat = Some(c);
                        }
                    }
                    Row::Ext(e, size, count) => {
                        row.set_selected(self.highlight_ext == Some(e));
                        row.col(|ui| {
                            ui.add_space(18.0);
                            swatch(ui, tree.exts[e as usize].color);
                            ui.label(tree.ext_label(e));
                        });
                        row.col(|ui| right(ui, format!("{} %", fmt_dec(size as f64 / total * 100.0))));
                        row.col(|ui| right(ui, fmt_size(size)));
                        row.col(|ui| right(ui, fmt_int(count)));
                        if row.response().clicked() {
                            toggle_ext = Some(e);
                        }
                    }
                });
            });
        self.ext_sort = sort;
        if let Some(c) = toggle_cat {
            if !self.ext_open.remove(&c) {
                self.ext_open.insert(c);
            }
            // Clicking a category also paints the chart with all its extensions.
            let code = CAT_FLAG | c as u32;
            self.highlight_ext = if self.highlight_ext == Some(code) { None } else { Some(code) };
        }
        if let Some(e) = toggle_ext {
            self.highlight_ext = if self.highlight_ext == Some(e) { None } else { Some(e) };
        }
    }


    fn treemap_ui(&mut self, ui: &mut egui::Ui, tree: &Tree) {
        let (rect, resp) = ui.allocate_exact_size(ui.available_size(), Sense::click());
        let ppp = ui.ctx().pixels_per_point();
        let w = (rect.width() * ppp).round().max(1.0) as usize;
        let h = (rect.height() * ppp).round().max(1.0) as usize;

        // Debounce resizes: wait until the size settles before re-rendering.
        let rings = self.settings.rings;
        let style = u8::from(rings) * 2 + u8::from(ui.visuals().dark_mode);
        let size_changed = self.layout.as_ref().is_some_and(|l| (l.width, l.height) != (w, h));
        let same_content = self.layout_key.0 == self.view_root
            && self.layout_key.4 == self.revision
            && self.layout_key.3 == self.highlight_ext
            && self.layout_key.5 == style;
        let mut rebuild = (self.view_root, w, h, self.highlight_ext, self.revision, style) != self.layout_key;
        if size_changed && same_content {
            match self.pending_size {
                Some((s, t)) if s == (w, h) && t.elapsed() > Duration::from_millis(120) => {}
                Some((s, _)) if s == (w, h) => {
                    rebuild = false;
                    ui.ctx().request_repaint_after(Duration::from_millis(60));
                }
                _ => {
                    self.pending_size = Some(((w, h), Instant::now()));
                    rebuild = false;
                    ui.ctx().request_repaint_after(Duration::from_millis(130));
                }
            }
        }
        if rebuild {
            self.pending_size = None;
            // Bytes of the highlighted extension per node (cached per extension + tree revision).
            let hl = self.highlight_ext.map(|e| match &self.highlight_cache {
                Some((ce, rev, h)) if *ce == e && *rev == self.revision => h.clone(),
                _ => {
                    let bytes = if e & CAT_FLAG != 0 {
                        let c = (e & !CAT_FLAG) as usize;
                        tree.ext_bytes_where(|x| category_of(&tree.exts[x as usize].name) == c)
                    } else {
                        tree.ext_bytes(e)
                    };
                    let h = Arc::new(treemap::Highlight { bytes });
                    self.highlight_cache = Some((e, self.revision, h.clone()));
                    h
                }
            });
            let layout = if rings {
                let bg = ui.visuals().panel_fill;
                let p = sunburst::Params { highlight: hl.as_deref(), bg: [bg.r(), bg.g(), bg.b()] };
                sunburst::build(tree, self.view_root, w, h, &p)
            } else {
                let line = if ui.visuals().dark_mode { [235, 235, 235] } else { [255, 255, 255] };
                treemap::build(tree, self.view_root, w, h, hl.as_deref(), line)
            };
            let img = egui::ColorImage::from_rgba_unmultiplied([w, h], layout.pixels.as_flattened());
            match &mut self.texture {
                Some(t) => t.set(img, egui::TextureOptions::NEAREST),
                None => self.texture = Some(ui.ctx().load_texture("treemap", img, egui::TextureOptions::NEAREST)),
            }
            self.layout = Some(layout);
            self.layout_key = (self.view_root, w, h, self.highlight_ext, self.revision, style);
        }
        let painter = ui.painter_at(rect);
        if let Some(t) = &self.texture {
            painter.image(t.id(), rect, egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)), Color32::WHITE);
        }
        let Some(layout) = &self.layout else { return };
        let (kx, ky) = (rect.width() / layout.width as f32, rect.height() / layout.height as f32);
        let to_screen =
            |r: treemap::R| egui::Rect::from_min_size(rect.min + egui::vec2(r.x * kx, r.y * ky), egui::vec2(r.w * kx, r.h * ky));
        // Treemap labels: group header tags (top-left) and "name (size)" cells (bottom-left).
        for l in &layout.labels {
            let sr = to_screen(l.r);
            let n = &tree.nodes[l.id as usize];
            // With an extension highlighted the chart is sized by that extension's bytes.
            let shown = match &self.highlight_cache {
                Some((e, _, h)) if self.highlight_ext == Some(*e) => h.bytes.get(l.id as usize).copied().unwrap_or(0),
                _ => n.size,
            };
            let text = format!("{} ({})", tree.name(l.id), fmt_size(shown));
            let job = |color: Color32, max_w: f32, size: f32| {
                let mut job = egui::text::LayoutJob::single_section(
                    text.clone(),
                    egui::TextFormat::simple(egui::FontId::proportional(size), color),
                );
                job.wrap = egui::text::TextWrapping {
                    max_width: max_w.max(1.0),
                    max_rows: 1,
                    break_anywhere: true,
                    overflow_character: Some('…'),
                };
                job
            };
            if l.header {
                let g = painter.layout_job(job(Color32::WHITE, sr.width() - 12.0, 12.5));
                let bg = egui::Rect::from_min_size(sr.min + egui::vec2(1.0, 1.0), g.size() + egui::vec2(10.0, 6.0));
                painter.rect_filled(bg.intersect(sr), 2.0, Color32::from_rgba_unmultiplied(30, 40, 55, 215));
                painter.galley(bg.min + egui::vec2(5.0, 3.0), g, Color32::WHITE);
            } else {
                let g = painter.layout_job(job(Color32::from_black_alpha(215), sr.width() - 8.0, 11.5));
                let pos = egui::pos2(sr.min.x + 4.0, sr.max.y - g.size().y - 3.0);
                if pos.y > sr.min.y {
                    painter.galley(pos, g, Color32::BLACK);
                }
            }
        }
        // Ring chart: outline of an arc in screen space.
        let arc_shape = |id: u32, stroke: Stroke| -> Option<egui::Shape> {
            let (geom, arc) = (layout.geom?, *layout.arcs.get(&id)?);
            let pts = sunburst::arc_outline(geom, arc).into_iter().map(|(x, y)| rect.min + egui::vec2(x * kx, y * ky)).collect();
            Some(egui::Shape::closed_line(pts, stroke))
        };
        if let Some(g) = layout.geom {
            // Center label: current folder and its size.
            let c = rect.min + egui::vec2(g[0] * kx, g[1] * ky);
            let n = &tree.nodes[layout.root as usize];
            let mut name = if layout.root == ROOT { tr("Este equipo", "This PC").to_string() } else { tree.name(layout.root).to_string() };
            let mut size = n.size;
            if let Some((e, _, h)) = &self.highlight_cache
                && self.highlight_ext == Some(*e)
            {
                name = if *e & CAT_FLAG != 0 {
                    category_name((*e & !CAT_FLAG) as usize).to_string()
                } else if tree.exts.get(*e as usize).is_some_and(|x| x.name.is_empty()) {
                    tree.ext_label(*e).to_string()
                } else {
                    format!(".{}", tree.ext_label(*e))
                };
                size = h.bytes.get(layout.root as usize).copied().unwrap_or(0);
            }
            let dark = Color32::from_gray(30);
            painter.text(c - egui::vec2(0.0, 8.0), egui::Align2::CENTER_CENTER, name, egui::FontId::proportional(13.0), dark);
            painter.text(c + egui::vec2(0.0, 9.0), egui::Align2::CENTER_CENTER, fmt_size(size), egui::FontId::proportional(12.0), dark);
        }

        self.hovered = None;
        if let Some(pos) = resp.hover_pos() {
            let px = ((pos.x - rect.min.x) / kx) as usize;
            let py = ((pos.y - rect.min.y) / ky) as usize;
            if px < layout.width && py < layout.height {
                let id = layout.owner[py * layout.width + px];
                if id != u32::MAX && (id as usize) < tree.nodes.len() {
                    self.hovered = Some(id);
                    if let Some(r) = layout.rects.get(&id) {
                        painter.rect_stroke(to_screen(*r), 0.0, Stroke::new(1.0, Color32::from_white_alpha(180)), egui::StrokeKind::Inside);
                    }
                    if let Some(s) = arc_shape(id, Stroke::new(2.0, Color32::WHITE)) {
                        painter.add(s);
                        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                    }
                    let n = &tree.nodes[id as usize];
                    resp.clone().on_hover_ui_at_pointer(|ui| {
                        ui.strong(tree.name(id));
                        ui.label(tree.full_path(n.parent));
                        let kind = if n.is_dir() { tr("carpeta", "folder").to_string() } else { tree.ext_label(n.ext).to_string() };
                        ui.label(format!("{}  ·  {}", fmt_size(n.size), kind));
                    });
                }
            }
        }

        // Selection (or its nearest visible ancestor).
        if let Some(sel) = self.selected {
            let mut id = sel;
            loop {
                if let Some(r) = layout.rects.get(&id) {
                    let sr = to_screen(*r);
                    painter.rect_stroke(sr, 0.0, Stroke::new(2.0, Color32::BLACK), egui::StrokeKind::Inside);
                    painter.rect_stroke(sr.shrink(1.0), 0.0, Stroke::new(1.5, Color32::WHITE), egui::StrokeKind::Inside);
                    break;
                }
                if let Some(s) = arc_shape(id, Stroke::new(2.5, Color32::BLACK)) {
                    painter.add(s);
                    break;
                }
                if id == ROOT || id == layout.root || id as usize >= tree.nodes.len() {
                    break;
                }
                id = tree.nodes[id as usize].parent;
            }
        }

        let ctx = ui.ctx().clone();
        let ring_mode = layout.geom.is_some();
        if resp.clicked()
            && let Some(h) = self.hovered
        {
            if ring_mode && h == self.view_root {
                // Baobab: clicking the center goes up one level.
                if h != ROOT {
                    let from = h;
                    self.set_root(&ctx, tree.nodes[h as usize].parent);
                    self.select(&ctx, tree, from, true);
                }
            } else if ring_mode && tree.nodes[h as usize].is_dir() {
                // Baobab: clicking a folder ring zooms into it.
                self.set_root(&ctx, h);
                self.select(&ctx, tree, h, true);
            } else {
                self.select(&ctx, tree, h, true);
            }
        }
        // Treemap: double-click a folder cell to go into it.
        if !ring_mode
            && resp.double_clicked()
            && let Some(h) = self.hovered
            && tree.nodes[h as usize].is_dir()
            && h != self.view_root
        {
            self.set_root(&ctx, h);
            self.select(&ctx, tree, h, true);
        }
        if let Some(h) = self.hovered
            && resp.secondary_clicked() {
                self.open_menu(&ctx, MenuTarget::Node(h));
            }
    }
}

// ---------------------------------------------------------------- helpers

enum MenuAction {
    Menu(u32),
    Reveal(String),
    ToggleExclude(String),
}

/// Right-click menu body. Returns an action for the app, and whether to close.
/// excluded: the folder's state in the duplicate exclusions (None = not excluded,
/// Some(true) = excluded itself, Some(false) = inside an excluded folder).
fn context_menu(ui: &mut egui::Ui, tree: Option<&Tree>, target: &MenuTarget, excluded: Option<bool>) -> (Option<MenuAction>, bool) {
    let (path, from_tree) = match target {
        MenuTarget::Node(id) => (tree.map(|t| t.full_path(*id)).unwrap_or_default(), true),
        MenuTarget::Path(p) => (p.clone(), false),
    };
    let name = path.trim_end_matches('\\').rsplit('\\').next().unwrap_or(&path).to_string();
    let mut action = None;
    let mut close = false;
    ui.set_min_width(200.0);
    ui.label(RichText::new(&name).strong());
    ui.separator();
    if ui.button(tr("Abrir", "Open")).on_hover_text(tr(
        "Carpetas: se abren en el Explorador. Archivos: se muestran seleccionados en el Explorador.",
        "Folders open in Explorer. Files are shown selected in Explorer.",
    )).clicked() {
        open_in_explorer(&path);
        close = true;
    }
    if ui.button(tr("Copiar ruta completa", "Copy full path")).clicked() {
        ui.ctx().copy_text(path.clone());
        close = true;
    }
    if ui.button(tr("Copiar nombre", "Copy name")).clicked() {
        ui.ctx().copy_text(name.clone());
        close = true;
    }
    let is_dir = match target {
        MenuTarget::Node(id) => tree.is_some_and(|t| t.nodes[*id as usize].is_dir() && *id != ROOT),
        MenuTarget::Path(p) => std::path::Path::new(p).is_dir(),
    };
    if is_dir && excluded != Some(false) {
        let label = if excluded == Some(true) {
            tr("Volver a incluir en duplicados", "Include in duplicates again")
        } else {
            tr("Excluir de duplicados", "Exclude from duplicates")
        };
        if ui.button(format!("⊘ {label}")).clicked() {
            action = Some(MenuAction::ToggleExclude(path.clone()));
            close = true;
        }
    }
    if !from_tree && ui.button(tr("Mostrar en el árbol", "Show in tree")).clicked() {
        action = Some(MenuAction::Reveal(path.clone()));
        close = true;
    }
    (action, close)
}

/// Folders open in Explorer; files are shown selected in their folder (never executed).
pub(crate) fn open_in_explorer(path: &str) {
    if std::path::Path::new(path).is_dir() {
        shell_open("explorer.exe", Some(&format!("\"{path}\"")));
    } else {
        shell_open("explorer.exe", Some(&format!("/select,\"{path}\"")));
    }
}

fn shell_open(file: &str, args: Option<&str>) {
    use windows_sys::Win32::UI::Shell::ShellExecuteW;
    use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
    let f = everything::wide(file);
    let a = args.map(everything::wide);
    let op = everything::wide("open");
    unsafe {
        ShellExecuteW(
            std::ptr::null_mut(),
            op.as_ptr(),
            f.as_ptr(),
            a.as_ref().map_or(std::ptr::null(), |a| a.as_ptr()),
            std::ptr::null(),
            SW_SHOWNORMAL,
        );
    }
}

/// Native "Save as" dialog for CSV files.
fn save_dialog(default_name: &str) -> Option<String> {
    use windows_sys::Win32::UI::Controls::Dialogs::*;
    let mut buf = vec![0u16; 1024];
    for (i, c) in default_name.encode_utf16().take(1000).enumerate() {
        buf[i] = c;
    }
    let filter: Vec<u16> = "CSV (*.csv)\0*.csv\0\0".encode_utf16().collect();
    let ext = everything::wide("csv");
    let mut ofn: OPENFILENAMEW = unsafe { std::mem::zeroed() };
    ofn.lStructSize = std::mem::size_of::<OPENFILENAMEW>() as u32;
    ofn.lpstrFilter = filter.as_ptr();
    ofn.lpstrFile = buf.as_mut_ptr();
    ofn.nMaxFile = buf.len() as u32;
    ofn.lpstrDefExt = ext.as_ptr();
    ofn.Flags = OFN_OVERWRITEPROMPT | OFN_PATHMUSTEXIST | OFN_NOCHANGEDIR;
    if unsafe { GetSaveFileNameW(&mut ofn) } == 0 {
        return None;
    }
    let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    Some(String::from_utf16_lossy(&buf[..len]))
}

fn default_drive(tree: &Tree) -> u32 {
    tree.drives
        .iter()
        .copied()
        .find(|&d| tree.name(d).eq_ignore_ascii_case("C:"))
        .or_else(|| tree.drives.first().copied())
        .unwrap_or(ROOT)
}

/// Segoe UI (the Windows font) with Segoe UI Symbol as glyph fallback.
fn setup_fonts(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();
    let dir = std::path::PathBuf::from(std::env::var("WINDIR").unwrap_or_else(|_| "C:\\Windows".into())).join("Fonts");
    let mut primary = Vec::new();
    let mut fallback = Vec::new();
    for (name, file, is_primary) in [("segoe", "segoeui.ttf", true), ("segsym", "seguisym.ttf", false)] {
        if let Ok(bytes) = std::fs::read(dir.join(file)) {
            fonts.font_data.insert(name.into(), Arc::new(egui::FontData::from_owned(bytes)));
            if is_primary { primary.push(name) } else { fallback.push(name) }
        }
    }
    for fam in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
        let list = fonts.families.entry(fam.clone()).or_default();
        if fam == egui::FontFamily::Proportional {
            for (i, p) in primary.iter().enumerate() {
                list.insert(i, p.to_string());
            }
        }
        for f in &fallback {
            list.push(f.to_string());
        }
    }
    ctx.set_fonts(fonts);
}

/// Expand/collapse arrow drawn with shapes (no font dependency).
fn toggle_arrow(ui: &mut egui::Ui, open: bool) -> egui::Response {
    let (r, resp) = ui.allocate_exact_size(egui::vec2(14.0, 14.0), Sense::click());
    let c = r.center();
    let col = if resp.hovered() { ui.visuals().strong_text_color() } else { ui.visuals().text_color() };
    let pts = if open {
        vec![c + egui::vec2(-4.0, -2.0), c + egui::vec2(4.0, -2.0), c + egui::vec2(0.0, 3.0)]
    } else {
        vec![c + egui::vec2(-2.0, -4.0), c + egui::vec2(3.0, 0.0), c + egui::vec2(-2.0, 4.0)]
    };
    ui.painter().add(egui::Shape::convex_polygon(pts, col, Stroke::NONE));
    resp
}

pub(crate) fn folder_icon(ui: &mut egui::Ui) {
    let (r, _) = ui.allocate_exact_size(egui::vec2(16.0, 14.0), Sense::hover());
    let p = ui.painter();
    let fill = Color32::from_rgb(255, 206, 84);
    let edge = Stroke::new(1.0, Color32::from_rgb(196, 140, 30));
    let tab = egui::Rect::from_min_size(r.min + egui::vec2(1.0, 2.0), egui::vec2(6.0, 3.0));
    let body = egui::Rect::from_min_max(r.min + egui::vec2(1.0, 4.0), r.max - egui::vec2(1.0, 1.0));
    p.rect_filled(tab, 1.0, fill);
    p.rect_stroke(tab, 1.0, edge, egui::StrokeKind::Inside);
    p.rect_filled(body, 1.5, fill);
    p.rect_stroke(body, 1.5, edge, egui::StrokeKind::Inside);
}

pub(crate) fn file_icon(ui: &mut egui::Ui, c: [u8; 3]) {
    let (r, _) = ui.allocate_exact_size(egui::vec2(16.0, 14.0), Sense::hover());
    let page = egui::Rect::from_center_size(r.center(), egui::vec2(10.0, 13.0));
    let p = ui.painter();
    p.rect_filled(page, 1.0, Color32::from_gray(250));
    p.rect_stroke(page, 1.0, Stroke::new(1.0, Color32::from_gray(140)), egui::StrokeKind::Inside);
    let band = egui::Rect::from_min_size(page.min + egui::vec2(1.0, 7.0), egui::vec2(8.0, 4.0));
    p.rect_filled(band, 0.0, Color32::from_rgb(c[0], c[1], c[2]));
}

fn drive_icon(ui: &mut egui::Ui) {
    let (r, _) = ui.allocate_exact_size(egui::vec2(16.0, 14.0), Sense::hover());
    let body = egui::Rect::from_center_size(r.center(), egui::vec2(15.0, 8.0));
    let p = ui.painter();
    p.rect_filled(body, 2.0, Color32::from_gray(200));
    p.rect_stroke(body, 2.0, Stroke::new(1.0, Color32::from_gray(110)), egui::StrokeKind::Inside);
    p.circle_filled(body.right_center() - egui::vec2(3.5, 0.0), 1.5, Color32::from_rgb(40, 200, 60));
}

fn right(ui: &mut egui::Ui, s: String) {
    ui.with_layout(ELayout::right_to_left(Align::Center), |ui| {
        ui.label(s);
    });
}

fn swatch(ui: &mut egui::Ui, c: [u8; 3]) {
    let (r, _) = ui.allocate_exact_size(egui::vec2(12.0, 12.0), Sense::hover());
    ui.painter().rect_filled(r, 2.0, Color32::from_rgb(c[0], c[1], c[2]));
    ui.painter().rect_stroke(r, 2.0, Stroke::new(1.0, Color32::from_black_alpha(120)), egui::StrokeKind::Inside);
}

/// WizTree-style percentage bar.
fn pct_bar(ui: &mut egui::Ui, pct: f32) {
    let w = ui.available_width().max(20.0);
    let (r, _) = ui.allocate_exact_size(egui::vec2(w, 14.0), Sense::hover());
    let p = ui.painter();
    p.rect_filled(r, 1.0, ui.visuals().extreme_bg_color);
    let mut fill = r;
    fill.set_width(r.width() * pct.clamp(0.0, 1.0));
    p.rect_filled(fill, 1.0, Color32::from_rgb(170, 120, 230));
    p.text(
        r.center(),
        egui::Align2::CENTER_CENTER,
        format!("{} %", fmt_dec(pct as f64 * 100.0)),
        egui::FontId::proportional(11.0),
        ui.visuals().strong_text_color(),
    );
}

pub fn fmt_size(b: u64) -> String {
    const U: [&str; 6] = ["B", "KB", "MB", "GB", "TB", "PB"];
    if b < 1024 {
        return format!("{b} B");
    }
    let mut v = b as f64;
    let mut i = 0;
    while v >= 1024.0 && i < U.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    format!("{} {}", fmt_dec(v), U[i])
}

/// One decimal with the current language's separator.
pub fn fmt_dec(v: f64) -> String {
    let s = format!("{v:.1}");
    let (dec, _) = i18n::seps();
    if dec == '.' { s } else { s.replace('.', &dec.to_string()) }
}

pub fn fmt_int(n: u64) -> String {
    let (_, thousands) = i18n::seps();
    let s = n.to_string();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push(thousands);
        }
        out.push(c);
    }
    out
}

pub(crate) fn fmt_age(d: Duration) -> String {
    let s = d.as_secs();
    let amount = if s < 60 {
        format!("{s} s")
    } else if s < 3600 {
        format!("{} min", s / 60)
    } else {
        format!("{} h", s / 3600)
    };
    if i18n::lang() == Lang::Es { format!("hace {amount}") } else { format!("{amount} ago") }
}

fn local_time(ft: u64) -> Option<windows_sys::Win32::Foundation::SYSTEMTIME> {
    use windows_sys::Win32::Foundation::{FILETIME, SYSTEMTIME};
    use windows_sys::Win32::System::Time::{FileTimeToSystemTime, SystemTimeToTzSpecificLocalTime};
    if ft == 0 || ft == u64::MAX {
        return None;
    }
    let f = FILETIME { dwLowDateTime: ft as u32, dwHighDateTime: (ft >> 32) as u32 };
    unsafe {
        let mut utc: SYSTEMTIME = std::mem::zeroed();
        let mut loc: SYSTEMTIME = std::mem::zeroed();
        if FileTimeToSystemTime(&f, &mut utc) == 0 || SystemTimeToTzSpecificLocalTime(std::ptr::null(), &utc, &mut loc) == 0 {
            return None;
        }
        Some(loc)
    }
}

pub(crate) fn fmt_date(ft: u64) -> String {
    let Some(t) = local_time(ft) else { return String::new() };
    if i18n::lang() == Lang::Es {
        format!("{:02}/{:02}/{:04} {:02}:{:02}", t.wDay, t.wMonth, t.wYear, t.wHour, t.wMinute)
    } else {
        format!("{:04}-{:02}-{:02} {:02}:{:02}", t.wYear, t.wMonth, t.wDay, t.wHour, t.wMinute)
    }
}

/// ISO-8601 local time for CSV export.
pub fn fmt_iso(ft: u64) -> String {
    let Some(t) = local_time(ft) else { return String::new() };
    format!("{:04}-{:02}-{:02} {:02}:{:02}:{:02}", t.wYear, t.wMonth, t.wDay, t.wHour, t.wMinute, t.wSecond)
}