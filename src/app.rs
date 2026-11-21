use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, channel};
use std::time::{Duration, Instant};

use eframe::egui::{self, Align, Color32, Key, Layout as ELayout, RichText, Sense, Stroke};
use egui_extras::{Column, TableBuilder};

use crate::dup::{self, DupGroup, DupOptions, DupProgress};
use crate::i18n::{self, IndexEngine, Lang, Settings, Theme, tr};
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
fn sort_header(ui: &mut egui::Ui, label: &str, col: Col, sort: &mut Sort) -> bool {
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

struct VolumeJob {
    rx: Receiver<Result<crate::volumes::VolumeRecord, String>>,
    progress: Arc<crate::volumes::CaptureProgress>,
    label: String,
    started: Instant,
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

/// Oscuro legible: texto casi blanco, paneles bien separados y acentos vivos.
/// El oscuro por defecto de egui deja el texto gris sobre gris; esto lo evita.
fn dark_visuals() -> egui::Visuals {
    use egui::{Color32, Stroke};
    let mut v = egui::Visuals::dark();
    v.dark_mode = true;
    v.override_text_color = None;
    v.weak_text_color = Some(Color32::from_rgb(205, 205, 212));
    v.panel_fill = Color32::from_rgb(20, 20, 23);
    v.window_fill = Color32::from_rgb(30, 30, 34);
    v.faint_bg_color = Color32::from_rgb(42, 42, 48);
    v.extreme_bg_color = Color32::from_rgb(12, 12, 14);
    v.text_edit_bg_color = Some(Color32::from_rgb(12, 12, 14));
    v.code_bg_color = Color32::from_rgb(38, 38, 44);
    v.hyperlink_color = Color32::from_rgb(130, 185, 255);
    v.warn_fg_color = Color32::from_rgb(255, 195, 95);
    v.error_fg_color = Color32::from_rgb(255, 125, 125);
    v.window_stroke = Stroke::new(1.0, Color32::from_gray(95));
    v.selection = egui::style::Selection {
        bg_fill: Color32::from_rgb(40, 95, 175),
        stroke: Stroke::new(1.0, Color32::WHITE),
    };
    v.widgets.noninteractive.bg_stroke = Stroke::new(1.0, Color32::from_gray(85));
    v.widgets.noninteractive.fg_stroke = Stroke::new(1.0, Color32::from_gray(220));
    v.widgets.inactive.bg_fill = Color32::from_rgb(52, 52, 60);
    v.widgets.inactive.weak_bg_fill = Color32::from_rgb(44, 44, 51);
    v.widgets.inactive.bg_stroke = Stroke::new(1.0, Color32::from_gray(115));
    v.widgets.inactive.fg_stroke = Stroke::new(1.0, Color32::from_gray(238));
    v.widgets.hovered.bg_fill = Color32::from_rgb(66, 78, 100);
    v.widgets.hovered.weak_bg_fill = Color32::from_rgb(58, 70, 92);
    v.widgets.hovered.bg_stroke = Stroke::new(1.5, Color32::from_rgb(125, 175, 255));
    v.widgets.hovered.fg_stroke = Stroke::new(1.5, Color32::WHITE);
    v.widgets.active.bg_fill = Color32::from_rgb(78, 98, 132);
    v.widgets.active.weak_bg_fill = Color32::from_rgb(70, 90, 124);
    v.widgets.active.bg_stroke = Stroke::new(2.0, Color32::from_rgb(160, 200, 255));
    v.widgets.active.fg_stroke = Stroke::new(2.0, Color32::WHITE);
    v
}

/// Claro de alto contraste: fondo papel, tinta negra y bordes gruesos.
/// Pensado para pantallas con brillo bajo, monitores viejos o vista cansada.
fn contrast_visuals() -> egui::Visuals {
    use egui::{Color32, Stroke};
    let mut v = egui::Visuals::light();
    v.dark_mode = false;
    v.override_text_color = Some(Color32::from_rgb(8, 8, 8));
    v.panel_fill = Color32::from_rgb(252, 252, 247);
    v.window_fill = Color32::WHITE;
    v.faint_bg_color = Color32::from_rgb(238, 238, 230);
    v.extreme_bg_color = Color32::WHITE;
    v.text_edit_bg_color = Some(Color32::WHITE);
    v.code_bg_color = Color32::from_rgb(240, 240, 232);
    v.hyperlink_color = Color32::from_rgb(0, 60, 150);
    v.warn_fg_color = Color32::from_rgb(150, 70, 0);
    v.error_fg_color = Color32::from_rgb(170, 0, 0);
    v.window_stroke = Stroke::new(1.5, Color32::from_gray(55));
    v.selection = egui::style::Selection {
        bg_fill: Color32::from_rgb(0, 70, 160),
        stroke: Stroke::new(1.0, Color32::WHITE),
    };
    v.widgets.noninteractive.bg_stroke = Stroke::new(1.0, Color32::from_gray(85));
    v.widgets.noninteractive.fg_stroke = Stroke::new(1.0, Color32::from_gray(10));
    v.widgets.inactive.bg_fill = Color32::from_rgb(232, 232, 224);
    v.widgets.inactive.weak_bg_fill = Color32::from_rgb(244, 244, 236);
    v.widgets.inactive.bg_stroke = Stroke::new(1.0, Color32::from_gray(90));
    v.widgets.inactive.fg_stroke = Stroke::new(1.0, Color32::from_gray(10));
    v.widgets.hovered.bg_fill = Color32::from_rgb(214, 228, 248);
    v.widgets.hovered.weak_bg_fill = Color32::from_rgb(224, 235, 250);
    v.widgets.hovered.bg_stroke = Stroke::new(1.5, Color32::from_rgb(0, 70, 160));
    v.widgets.hovered.fg_stroke = Stroke::new(1.5, Color32::BLACK);
    v.widgets.active.bg_fill = Color32::from_rgb(190, 212, 242);
    v.widgets.active.weak_bg_fill = Color32::from_rgb(200, 220, 245);
    v.widgets.active.bg_stroke = Stroke::new(2.0, Color32::from_rgb(0, 55, 130));
    v.widgets.active.fg_stroke = Stroke::new(2.0, Color32::BLACK);
    v
}

fn apply_theme(ctx: &egui::Context, t: Theme) {
    match t {
        Theme::System => {
            ctx.set_visuals_of(egui::Theme::Light, egui::Visuals::light());
            ctx.set_visuals_of(egui::Theme::Dark, dark_visuals());
            ctx.set_theme(egui::ThemePreference::System);
        }
        Theme::Light => {
            ctx.set_visuals_of(egui::Theme::Light, egui::Visuals::light());
            ctx.set_theme(egui::ThemePreference::Light);
        }
        Theme::Dark => {
            ctx.set_visuals_of(egui::Theme::Dark, dark_visuals());
            ctx.set_theme(egui::ThemePreference::Dark);
        }
        Theme::Contrast => {
            ctx.set_visuals_of(egui::Theme::Light, contrast_visuals());
            ctx.set_theme(egui::ThemePreference::Light);
        }
    }
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
    /// Cambió el motor mientras cargaba: recargar al terminar.
    reload_pending: bool,
    /// Diálogo de instalación cuando no hay ningún motor.
    setup_open: bool,
    setup_msg: Option<String>,
    setup_det: Option<crate::setup::Detection>,
    setup_det_at: Instant,
    /// Objetivo del planificador de espacio.
    plan_target_gb: u32,
    /// Snapshots/catálogos para comparar dos máquinas.
    machine_a: String,
    machine_b: String,

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
    /// Extension/category filter applied to the left panel.
    type_filter: Option<u32>,
    /// Report shown in the Analysis window (title, text).
    analysis: Option<(String, String)>,
    /// Structured report (similar files / GitHub mode) rendered as tables, not JSON.
    report: Option<(String, serde_json::Value)>,
    /// Thumbnail textures for the similar-images panel.
    thumbs: HashMap<String, Option<egui::TextureHandle>>,
    /// Action basket (mutating ops staged for dry-run + confirmation).
    basket: Vec<crate::executor::Op>,
    basket_bytes: u64,
    basket_report: Option<crate::executor::Report>,
    basket_open: bool,
    basket_armed: bool,
    basket_note: Option<String>,
    backup_open: bool,
    backup_source: String,
    backup_target: String,
    backup_deep: bool,
    /// Last fast run per analysis: (fast_ms, estimated_deep_ms), for the deep hover.
    deep_eta: HashMap<&'static str, (u64, u64)>,
    /// Saved disconnected disks (headers only; inventories load on demand).
    volumes: crate::volumes::Store,
    volume_metas: Vec<crate::volumes::VolumeRecord>,
    volume_job: Option<VolumeJob>,
    capture_open: bool,
    capture_drive: Option<(String, u32)>,
    capture_retention: crate::volumes::Retention,
    capture_deep: bool,
    volume_delete_armed: Option<String>,
    quarantine_open: bool,
    quarantine_entries: Vec<crate::quarantine::QEntry>,
    quarantine_armed: Option<std::path::PathBuf>,
    timeline_open: bool,
    timeline_frames: Vec<crate::timeline::Meta>,
    timeline_sel: usize,
    timeline_deep: bool,
    content_open: bool,
    content_query: String,
    content_deep: bool,
    ab: Option<crate::audio_ab::AbPlayer>,
    ab_error: Option<String>,
    ab_blind: bool,
    ab_swapped: bool,
    img_ab: Option<crate::img_ab::ImgAb>,
    img_ab_tex: Option<(egui::TextureHandle, egui::TextureHandle)>,

    tab: Tab,
    file_filter: String,
    files: Vec<u32>,
    files_key: (u32, u64, String, Sort, Option<u32>),

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
        crate::everything::set_engine(settings.engine);
        apply_theme(ctx, settings.theme);
        let mut style = (*ctx.global_style()).clone();
        style.spacing.item_spacing = egui::vec2(6.0, 3.0);
        // Selectable labels swallow clicks (incl. right-click) before the table
        // row sees them; rows must get them for selection and the context menu.
        style.interaction.selectable_labels = false;
        ctx.set_global_style(style);
        let volumes = crate::volumes::Store::default();
        let volume_metas = volumes.scan();
        let mut app = Self {
            tree: None,
            epoch: 0,
            revision: 0,
            loading: None,
            error: None,
            info: None,
            error_at: Instant::now(),
            disk_cache: None,
            reload_pending: false,
            setup_open: matches!(crate::everything::status(), crate::everything::Status::NotRunning),
            setup_msg: None,
            setup_det: None,
            setup_det_at: Instant::now() - Duration::from_secs(10),
            plan_target_gb: 100,
            machine_a: String::new(),
            machine_b: String::new(),
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
            type_filter: None,
            analysis: None,
            report: None,
            thumbs: HashMap::new(),
            basket: Vec::new(),
            basket_bytes: 0,
            basket_report: None,
            basket_open: false,
            basket_armed: false,
            basket_note: None,
            backup_open: false,
            backup_source: String::new(),
            backup_target: String::new(),
            backup_deep: false,
            deep_eta: HashMap::new(),
            volumes,
            volume_metas,
            volume_job: None,
            capture_open: false,
            capture_drive: None,
            capture_retention: crate::volumes::Retention::Full,
            capture_deep: false,
            volume_delete_armed: None,
            quarantine_open: false,
            quarantine_entries: Vec::new(),
            quarantine_armed: None,
            timeline_open: false,
            timeline_frames: Vec::new(),
            timeline_sel: 0,
            timeline_deep: false,
            content_open: false,
            content_query: String::new(),
            content_deep: false,
            ab: None,
            ab_error: None,
            ab_blind: false,
            ab_swapped: false,
            img_ab: None,
            img_ab_tex: None,
            tab: Tab::Tree,
            file_filter: String::new(),
            files: Vec::new(),
            files_key: (u32::MAX, u64::MAX, String::new(), None, None),
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
        // Una carga completa puede volver a elegir Auto; los deltas de sync
        // quedan fijados al motor que produjo este árbol.
        crate::everything::reset_active();
        let (tx, rx) = channel();
        let progress = Arc::new(Progress::default());
        let p = progress.clone();
        let ctx = ctx.clone();
        let excluded = self.settings.excluded_drives.clone();
        std::thread::Builder::new()
            .name("everything-load".into())
            .spawn(move || {
                // Al iniciar Windows el motor puede tardar; no esperar para siempre.
                let wait_from = Instant::now();
                while !matches!(everything::status(), everything::Status::Ready) {
                    if wait_from.elapsed() > Duration::from_secs(45) {
                        let _ = tx.send(Err(format!(
                            "No se pudo cargar el índice ({}). Iniciá Everything o `census serve` y reintentá con F5.",
                            everything::engine_label()
                        )));
                        ctx.request_repaint();
                        return;
                    }
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
                // Los discos guardados habilitados se suman como unidades offline.
                let saved = crate::volumes::Store::default().enabled_full();
                let offline = crate::volumes::offline_drives(&saved);
                let r = tree::load_with_offline(p.clone(), &excluded, &offline);
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
                self.volume_metas = self.volumes.scan();
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
                    self.type_filter = self.type_filter.and_then(|e| {
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
                // Debug hook for screenshots/tests: WIZE_HL=cat:<name>|<ext> opens highlighted.
                if let Ok(v) = std::env::var("WIZE_HL") {
                    let v = v.trim();
                    self.highlight_ext = if let Some(c) = v.strip_prefix("cat:") {
                        CATEGORIES
                            .iter()
                            .position(|(es, en, _)| es.eq_ignore_ascii_case(c) || en.eq_ignore_ascii_case(c))
                            .map(|i| CAT_FLAG | i as u32)
                    } else {
                        new.find_ext(v.trim_start_matches('.'))
                    };
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

        if let Some(job) = &self.volume_job
            && let Ok(r) = job.rx.try_recv()
        {
            let label = job.label.clone();
            self.volume_job = None;
            match r {
                Ok(rec) => {
                    let id = rec.id.clone();
                    let show = rec.retention == crate::volumes::Retention::Full && rec.enabled;
                    match self.volumes.save(rec) {
                        Ok(()) => {
                            self.volume_metas = self.volumes.scan();
                            self.set_info(format!("{} {label} → {id}", tr("Copia guardada:", "Saved copy:")));
                            if show {
                                self.start_load(ctx);
                            }
                        }
                        Err(e) => self.set_error(format!("{label}: {e}")),
                    }
                }
                Err(e) => self.set_error(format!("{label}: {e}")),
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
        // No perder una ruta de census.exe escrita y todavía no confirmada.
        self.settings.save();
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
        if self.reload_pending && self.loading.is_none() {
            self.reload_pending = false;
            self.start_load(ctx);
        }
        self.setup_ui(ctx);
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
                if let Some(code) = self.type_filter {
                    let label = if code & CAT_FLAG != 0 {
                        category_name((code & !CAT_FLAG) as usize).to_string()
                    } else if tree.exts.get(code as usize).is_some_and(|x| x.name.is_empty()) {
                        tree.ext_label(code).to_string()
                    } else {
                        format!(".{}", tree.ext_label(code))
                    };
                    let h = self.ext_bytes_for(&tree, code);
                    let total = h.bytes.get(self.view_root as usize).copied().unwrap_or(0);
                    let text = format!("{} {label} · {} ✕", tr("Filtro:", "Filter:"), fmt_size(total));
                    if ui
                        .small_button(text)
                        .on_hover_text(tr("Quitar filtro de tipo", "Clear type filter"))
                        .clicked()
                    {
                        self.type_filter = None;
                    }
                }
            });
            ui.separator();
            match self.tab {
                Tab::Tree => self.tree_ui(ui, &ctx, &tree),
                Tab::Files => self.files_ui(ui, &ctx, &tree),
                Tab::Dups => self.dups_ui(ui, &ctx, &tree),
                Tab::Ai => self.ai_ui(ui),
            }
        });
        self.menu_ui(&ctx);
        // Storage-analysis report window (read-only engines).
        if let Some((title, text)) = self.analysis.clone() {
            let mut open = true;
            egui::Window::new(title)
                .open(&mut open)
                .resizable(true)
                .default_width(760.0)
                .default_height(420.0)
                .show(&ctx, |ui| {
                    egui::ScrollArea::both().show(ui, |ui| {
                        ui.add(egui::Label::new(RichText::new(text).monospace()).wrap());
                    });
                });
            if !open {
                self.analysis = None;
            }
        }
        if let Some((title, v)) = self.report.clone() {
            let mut open = true;
            egui::Window::new(title.clone())
                .open(&mut open)
                .resizable(true)
                .default_size([820.0, 560.0])
                .show(&ctx, |ui| {
                    ui.horizontal(|ui| {
                        if ui
                            .button(tr("Guardar HTML", "Save HTML"))
                            .on_hover_text(tr(
                                "Guarda este análisis como un HTML autocontenido en la carpeta de datos de la app.",
                                "Saves this analysis as a self-contained HTML file in the app data folder.",
                            ))
                            .clicked()
                        {
                            match crate::report::save(&title, &v) {
                                Ok(p) => self.info = Some((format!("HTML: {}", p.display()), Instant::now())),
                                Err(e) => self.error = Some(e),
                            }
                        }
                    });
                    ui.separator();
                    egui::ScrollArea::vertical().show(ui, |ui| {
                        self.report_body(ui, &v);
                    });
                });
            if !open {
                self.report = None;
            }
        }
        if self.basket_open {
            let mut open = true;
            egui::Window::new(tr("Canasta de acciones (exp)", "Action basket (exp)"))
                .open(&mut open)
                .resizable(true)
                .default_size([780.0, 560.0])
                .show(&ctx, |ui| {
                    self.basket_ui(ui, &tree);
                });
            self.basket_open = open;
        }
        if self.backup_open {
            let mut open = true;
            egui::Window::new(tr("¿Está respaldado? (exp)", "Is it backed up? (exp)"))
                .open(&mut open)
                .resizable(true)
                .default_size([640.0, 300.0])
                .show(&ctx, |ui| {
                    self.backup_ui(ui, &tree);
                });
            self.backup_open = open;
        }
        if self.content_open {
            let mut open = true;
            egui::Window::new(tr("Buscar dentro de archivos (exp)", "Search inside files (exp)"))
                .open(&mut open)
                .resizable(true)
                .default_size([640.0, 220.0])
                .show(&ctx, |ui| {
                    self.content_ui(ui, &tree);
                });
            self.content_open = open;
        }
        if self.timeline_open {
            let mut open = true;
            egui::Window::new(tr("Línea de tiempo (exp)", "Timeline (exp)"))
                .open(&mut open)
                .resizable(true)
                .default_size([680.0, 460.0])
                .show(&ctx, |ui| {
                    self.timeline_ui(ui, &tree);
                });
            self.timeline_open = open;
        }
        if self.quarantine_open {
            let mut open = true;
            egui::Window::new(tr("Cuarentena (exp)", "Quarantine (exp)"))
                .open(&mut open)
                .resizable(true)
                .default_size([760.0, 520.0])
                .show(&ctx, |ui| {
                    self.quarantine_ui(ui);
                });
            self.quarantine_open = open;
        }
        if self.capture_open {
            let mut open = true;
            egui::Window::new(tr("Guardar disco para uso offline", "Save disk for offline use"))
                .open(&mut open)
                .resizable(false)
                .default_size([560.0, 280.0])
                .show(&ctx, |ui| {
                    self.capture_ui(ui, &ctx);
                });
            self.capture_open = open;
        }
        if self.ab.is_some() || self.ab_error.is_some() {
            let mut open = true;
            egui::Window::new(tr("Comparar audio A/B (exp)", "A/B audio compare (exp)"))
                .open(&mut open)
                .resizable(true)
                .default_size([640.0, 240.0])
                .show(&ctx, |ui| {
                    self.audio_ab_ui(ui);
                });
            if !open {
                self.ab = None;
                self.ab_error = None;
            }
        }
        if self.img_ab.is_some() {
            let mut open = true;
            egui::Window::new(tr("Comparar imágenes en zoom (exp)", "Zoom image compare (exp)"))
                .open(&mut open)
                .resizable(true)
                .default_size([900.0, 460.0])
                .show(&ctx, |ui| {
                    self.img_ab_ui(ui);
                });
            if !open {
                self.img_ab = None;
                self.img_ab_tex = None;
            }
        }
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
        if drive != ROOT
            && let Some(rec) = self.offline_record(tree.name(drive))
        {
            ui.colored_label(
                Color32::from_rgb(255, 176, 32),
                format!("🔌 {} · {}", tr("sin conexión", "offline"), crate::volumes::fmt_date(rec.updated)),
            )
            .on_hover_text(tr(
                "Estos datos son una copia guardada; el disco no está conectado. La fecha es la última actualización.",
                "This is a saved copy; the disk is not connected. The date is the last update.",
            ));
        }
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
                ui.selectable_value(&mut theme, Theme::Contrast, tr("◑ Alto contraste", "◑ High contrast"))
                    .on_hover_text(tr(
                        "Fondo claro, texto negro y bordes marcados. Más fácil de ver con brillo bajo o vista cansada.",
                        "Light background, black text and strong borders. Easier on low brightness or tired eyes.",
                    ));
            });
            if theme != self.settings.theme {
                self.settings.theme = theme;
                apply_theme(ctx, theme);
                self.settings.save();
            }
            ui.add_space(4.0);
            // Motor del índice
            ui.label(RichText::new(tr("Motor de índice", "Index engine")).strong());
            let mut engine = self.settings.engine;
            ui.horizontal(|ui| {
                ui.selectable_value(&mut engine, IndexEngine::Auto, tr("Auto", "Auto"))
                    .on_hover_text(tr(
                        "Usa Everything si está listo; si no, Census.",
                        "Uses Everything when ready; otherwise Census.",
                    ));
                ui.selectable_value(&mut engine, IndexEngine::Everything, "Everything");
                ui.selectable_value(&mut engine, IndexEngine::Census, "Census")
                    .on_hover_text(tr(
                        "Requiere `census serve` en ejecución (como administrador). Census indexa un volumen por vez.",
                        "Requires `census serve` running (as administrator). Census indexes one volume at a time.",
                    ));
            });
            ui.label(
                RichText::new(format!(
                    "{}: {}",
                    tr("Activo", "Active"),
                    crate::everything::active_engine()
                ))
                .weak(),
            );
            if engine == IndexEngine::Census
                && matches!(crate::everything::status(), crate::everything::Status::NotRunning)
            {
                ui.label(
                    RichText::new(tr(
                        "Ejecutá `census serve` como administrador para que Census aparezca.",
                        "Run `census serve` as administrator so Census shows up.",
                    ))
                    .weak(),
                );
            }
            if engine != self.settings.engine {
                self.settings.engine = engine;
                crate::everything::set_engine(engine);
                self.settings.save();
                if matches!(crate::everything::status(), crate::everything::Status::NotRunning) {
                    self.setup_open = true;
                }
                if self.loading.is_some() {
                    self.reload_pending = true;
                } else {
                    self.start_load(ctx);
                }
            }
            if ui
                .button(tr("Instalar o elegir motor…", "Install or choose an engine…"))
                .on_hover_text(tr(
                    "Detecta Everything/Census y ofrece instalarlos o indicar dónde está census.exe.",
                    "Detects Everything/Census and offers installation or a path to census.exe.",
                ))
                .clicked()
            {
                self.setup_open = true;
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
                    ui.horizontal(|ui| {
                        if ui.checkbox(&mut inc, text).changed() {
                            self.settings.excluded_drives.retain(|x| !x.eq_ignore_ascii_case(&name));
                            if !inc {
                                self.settings.excluded_drives.push(name.clone());
                            }
                            self.exclusion_dirty = true;
                            self.settings.save();
                        }
                        if self.offline_record(&name).is_none()
                            && ui
                                .small_button("💾")
                                .on_hover_text(tr(
                                    "Guardar este disco para consultarlo cuando esté desconectado (marca, modelo, serie, rutas y huella rápida).",
                                    "Save this disk to consult it while disconnected (brand, model, serial, paths and fast semihash).",
                                ))
                                .clicked()
                        {
                            self.capture_drive = Some((name.clone(), d));
                            self.capture_deep = false;
                            self.capture_retention = crate::volumes::Retention::Full;
                            self.capture_open = true;
                        }
                    });
                }
                self.saved_disks_ui(ui, ctx);
                if ui
                    .button(tr("Abrir cuarentena (exp)", "Open quarantine (exp)"))
                    .on_hover_text(tr(
                        "Archivos que la app movió a un lugar seguro: se pueden restaurar a su ruta original o borrar definitivamente. (Experimental)",
                        "Files the app moved to a safe place: restore them to their original path or delete them for good. (Experimental)",
                    ))
                    .clicked()
                {
                    self.quarantine_entries = crate::quarantine::scan();
                    self.quarantine_armed = None;
                    self.quarantine_open = true;
                }
            }
            // Storage analysis (read-only engines).
            ui.separator();
            if let Some(tree) = tree {
                ui.label(RichText::new(tr("Análisis del espacio", "Space analysis")).strong());
                let fmt = |r: Result<serde_json::Value, String>| match r {
                    Ok(v) => serde_json::to_string_pretty(&v).unwrap_or_default(),
                    Err(e) => format!("Error: {e}"),
                };
                let root = self.view_root;
                let caps = serde_json::json!({
                    "all_drives": true,
                    "max_scan": 2000,
                    "max_files": 5000,
                    "min_score": 40,
                    "max_entries": 20000
                });
                if ui
                    .button(tr("Modo GitHub (carpeta actual) (exp)", "GitHub mode (current folder) (exp)"))
                    .on_hover_text(tr(
                        "Analiza la carpeta actual como repo: dice si está al día con la web, si el local es más nuevo (commits sin subir), o si está atrasado/divergido; y explica las diferencias por commits, archivos y tamaño. (Experimental)",
                        "Treats the current folder as a repo: says whether it is up to date with the web, newer than it (unpushed commits), behind or diverged; then explains differences by commits, files and size. (Experimental)",
                    ))
                    .clicked()
                {
                    let res = crate::mcp_tools::github_mode(tree, root, &caps, 30);
                    let title = match &res {
                        Ok(v) => v.get("title_es").and_then(|x| x.as_str()).unwrap_or("Modo GitHub").to_string(),
                        Err(_) => "Modo GitHub".to_string(),
                    };
                    self.set_report(&format!("GitHub (exp) · {title}"), res);
                }
                if ui
                    .button(tr("Auditoría cloud / sparse", "Cloud / sparse audit"))
                    .on_hover_text(tr(
                        "Detecta archivos que no ocupan en disco lo que pesan (nube, sparse, comprimidos, junctions). Muestra tamaño lógico vs real y cuánto podrías liberar.",
                        "Finds files that use less disk than their size (cloud, sparse, compressed, junctions). Shows logical vs real size and how much you could free.",
                    ))
                    .clicked()
                {
                    self.analysis = Some(("Cloud / sparse audit".into(), fmt(crate::mcp_tools::cloud_audit(tree, root, &caps, 30))));
                }
                if ui
                    .button(tr("Archivos fríos", "Cold files"))
                    .on_hover_text(tr(
                        "Puntúa cada archivo por cuán poco lo usás (modificación, último acceso, veces abierto). Para elegir qué archivar o borrar.",
                        "Scores each file by how rarely you use it (modified, last access, run count). Helps choose what to archive or delete.",
                    ))
                    .clicked()
                {
                    self.analysis = Some(("Cold files".into(), fmt(crate::mcp_tools::cold_files(tree, root, &caps, 30))));
                }
                if ui
                    .button(tr("Carpetas frías", "Cold folders"))
                    .on_hover_text(tr(
                        "Igual que Archivos fríos, pero agrupado por carpeta: qué carpetas concentran más datos sin usar.",
                        "Same as Cold files but grouped by folder: which folders hold the most unused data.",
                    ))
                    .clicked()
                {
                    self.analysis = Some(("Cold folders".into(), fmt(crate::mcp_tools::cold_folders(tree, root, &caps, 30))));
                }
                if let Some(deep) = analysis_pair(
                    ui,
                    tr("Imágenes parecidas (exp)", "Similar images (exp)"),
                    tr(
                        "Agrupa la misma foto en distintas resoluciones o recompresiones (hash perceptual) y sugiere cuál conservar por calidad. Rápido usa un presupuesto; profundo analiza todas. (Experimental)",
                        "Groups the same photo at different resolutions or recompressions (perceptual hash) and suggests which copy to keep by quality. Fast uses a budget; deep analyses all. (Experimental)",
                    ),
                    self.deep_eta.get("images").copied(),
                ) {
                    let mut a = caps.clone();
                    a["mode"] = serde_json::json!(if deep { "deep" } else { "fast" });
                    let t0 = Instant::now();
                    let res = crate::mcp_tools::similar_images(tree, root, &a, 30);
                    if !deep {
                        self.remember_eta("images", t0.elapsed().as_millis() as u64, &res);
                    }
                    self.set_report(tr("Imágenes parecidas (exp)", "Similar images (exp)"), res);
                }
                if let Some(deep) = analysis_pair(
                    ui,
                    tr("Audio parecido (exp)", "Similar audio (exp)"),
                    tr(
                        "Agrupa el mismo tema aunque las etiquetas difieran (álbum vs grabación de TV/YouTube), separa versiones y sugiere cuál conservar. Rápido mide 20 s por copia; profundo, hasta 2 min. (Experimental)",
                        "Groups the same track even when tags differ (album vs TV/YouTube), splits versions and suggests which copy to keep. Fast measures 20 s per copy; deep up to 2 min. (Experimental)",
                    ),
                    self.deep_eta.get("audio").copied(),
                ) {
                    let mut a = caps.clone();
                    a["mode"] = serde_json::json!(if deep { "deep" } else { "fast" });
                    let t0 = Instant::now();
                    let res = crate::mcp_tools::similar_audio(tree, root, &a, 30);
                    if !deep {
                        self.remember_eta("audio", t0.elapsed().as_millis() as u64, &res);
                    }
                    self.set_report(tr("Audio parecido (exp)", "Similar audio (exp)"), res);
                }
                if let Some(deep) = analysis_pair(
                    ui,
                    tr("Velocidad y ETA (exp)", "Speed and ETA (exp)"),
                    tr(
                        "Mide el disco (NVMe/SSD/HDD/USB), detecta Google Drive streaming/clone y qué parte ya está local, y estima cuánto tarda una tarea. Profundo mide la descarga real leyendo un archivo de la nube (lo hidrata). (Experimental)",
                        "Measures the disk (NVMe/SSD/HDD/USB), detects Google Drive stream/clone and how much is already local, and estimates a task. Deep measures the real download by reading a cloud file (hydrates it). (Experimental)",
                    ),
                    None,
                ) {
                    let mut a = caps.clone();
                    a["path"] = serde_json::json!(tree.full_path(root));
                    a["mode"] = serde_json::json!(if deep { "deep" } else { "fast" });
                    self.set_report(
                        tr("Velocidad y ETA (exp)", "Speed and ETA (exp)"),
                        crate::mcp_tools::speed_test(tree, root, &a),
                    );
                }
                if ui
                    .button(tr("Addons / Google Photos (exp)", "Addons / Google Photos (exp)"))
                    .on_hover_text(tr(
                        "Detecta rclone, gphotos-sync, Tesseract y pdftotext, y muestra cómo usarlos. La app no pide credenciales ni sube nada. (Experimental)",
                        "Detects rclone, gphotos-sync, Tesseract and pdftotext, and shows how to use them. The app asks for no credentials and uploads nothing. (Experimental)",
                    ))
                    .clicked()
                {
                    self.set_report(
                        tr("Addons / Google Photos (exp)", "Addons / Google Photos (exp)"),
                        Ok(crate::addons::to_json()),
                    );
                }
                if ui
                    .button(tr("Buscar dentro de archivos (exp)", "Search inside files (exp)"))
                    .on_hover_text(tr(
                        "Busca texto en archivos de texto/código y PDFs (con pdftotext). Profundo agrega OCR con Tesseract. Antes podés ver cuánto tardaría. (Experimental)",
                        "Searches text in text/code files and PDFs (via pdftotext). Deep adds OCR with Tesseract. You can estimate the time first. (Experimental)",
                    ))
                    .clicked()
                {
                    self.content_open = true;
                }
                if ui
                    .button(tr("Candidatos a compresión", "Compression candidates"))
                    .on_hover_text(tr(
                        "Archivos fríos que no están ya comprimidos y ganarían espacio con compresión LZX de Windows. Solo plan; no comprime todavía.",
                        "Cold files not already compressed that would gain space from Windows LZX compression. Plan only; does not compress yet.",
                    ))
                    .clicked()
                {
                    self.analysis = Some(("Compression candidates".into(), fmt(crate::mcp_tools::compression_candidates(tree, root, &caps, 30))));
                }
                if ui
                    .button(tr("Plan de hardlinks", "Hardlink plan"))
                    .on_hover_text(tr(
                        "Duplicados exactos que podrían ser hardlinks (mismo archivo físico, varias rutas) dentro del mismo disco. Dry-run: no modifica nada.",
                        "Exact duplicates that could be hardlinks (one physical file, several paths) on the same volume. Dry-run: changes nothing.",
                    ))
                    .clicked()
                {
                    self.analysis = Some(("Hardlink plan".into(), fmt(crate::mcp_tools::hardlink_plan(tree, root, &caps, 30))));
                }
                if ui
                    .button(tr("Canasta de acciones (exp)", "Action basket (exp)"))
                    .on_hover_text(tr(
                        "Prepara operaciones (hardlinks, compresión LZX, cuarentena) y muéstralas en dry-run antes de confirmar. (Experimental)",
                        "Stages operations (hardlinks, LZX compression, quarantine) and shows them in dry-run before confirming. (Experimental)",
                    ))
                    .clicked()
                {
                    self.basket_open = true;
                }
                if ui
                    .button(tr("¿Por qué está lleno?", "Why is it full?"))
                    .on_hover_text(tr(
                        "Carpetas y archivos más grandes, el camino que explica el espacio y el espacio libre del disco.",
                        "Largest folders and files, the chain that explains the space and the drive's free space.",
                    ))
                    .clicked()
                {
                    self.set_report(
                        tr("¿Por qué está lleno?", "Why is it full?"),
                        crate::mcp_tools::why_full(tree, root, &caps, 10),
                    );
                }
                if let Some(deep) = analysis_pair(
                    ui,
                    tr("¿Qué creció?", "What grew?"),
                    tr(
                        "Compara los dos snapshots más nuevos y atribuye el crecimiento a carpetas. Rápido resuelve los cambios más grandes; profundo, todos.",
                        "Compares the two newest snapshots and attributes growth to folders. Fast resolves the biggest changes; deep resolves all.",
                    ),
                    self.deep_eta.get("growth").copied(),
                ) {
                    let mut a = caps.clone();
                    a["mode"] = serde_json::json!(if deep { "deep" } else { "fast" });
                    let t0 = Instant::now();
                    let res = crate::mcp_tools::growth_report(tree, root, &a, 12);
                    if !deep {
                        self.remember_eta("growth", t0.elapsed().as_millis() as u64, &res);
                    }
                    self.set_report(tr("¿Qué creció?", "What grew?"), res);
                }
                if let Some(deep) = analysis_pair(
                    ui,
                    tr("¿Está respaldado? (exp)", "Is it backed up? (exp)"),
                    tr(
                        "Compara esta carpeta contra otra copia por ruta relativa y tamaño. Rápido cubre hasta 50.000 archivos; profundo, 250.000. (Experimental)",
                        "Compares this folder against another copy by relative path and size. Fast covers up to 50,000 files; deep up to 250,000. (Experimental)",
                    ),
                    self.deep_eta.get("backup").copied(),
                ) {
                    self.backup_source = self
                        .selected
                        .map(|id| tree.full_path(id))
                        .unwrap_or_else(|| tree.full_path(root));
                    self.backup_target.clear();
                    self.backup_deep = deep;
                    self.backup_open = true;
                }
                if ui
                    .button(tr("Guardar snapshot", "Save snapshot"))
                    .on_hover_text(tr(
                        "Guarda una foto compacta del índice para comparar después y ver qué creció o desapareció. Se guarda en la carpeta de datos de la app.",
                        "Saves a compact snapshot of the index to compare later and see what grew or disappeared. Stored in the app's data folder.",
                    ))
                    .clicked()
                {
                    self.analysis = Some(("Snapshot".into(), fmt(crate::mcp_tools::snapshot_save(tree, root, &caps))));
                }
                if ui
                    .button(tr("Línea de tiempo (exp)", "Timeline (exp)"))
                    .on_hover_text(tr(
                        "Snapshots guardados en orden: elegí un intervalo y compará qué apareció, qué desapareció y qué carpeta creció. (Experimental)",
                        "Saved snapshots in order: pick an interval and compare what appeared, disappeared and which folder grew. (Experimental)",
                    ))
                    .clicked()
                {
                    self.timeline_frames = crate::timeline::list();
                    self.timeline_sel = self.timeline_frames.len().saturating_sub(2);
                    self.timeline_open = true;
                }
                ui.collapsing(tr("Planificación (exp)", "Planning (exp)"), |ui| {
                    ui.horizontal(|ui| {
                        ui.label(tr("Objetivo:", "Target:"));
                        ui.add(
                            egui::DragValue::new(&mut self.plan_target_gb)
                                .range(1..=100_000)
                                .suffix(" GB"),
                        );
                        if ui
                            .button(tr("Planificar liberación", "Plan reclaim"))
                            .on_hover_text(tr(
                                "Elige archivos grandes hasta llegar al objetivo, con riesgo y edad. Dry-run: no borra nada. (Experimental)",
                                "Picks large files until the target, with risk and age. Dry-run: deletes nothing. (Experimental)",
                            ))
                            .clicked()
                        {
                            let mut a = caps.clone();
                            a["target_gb"] = serde_json::json!(self.plan_target_gb);
                            self.set_report(
                                tr("Planificador de espacio", "Space planner"),
                                crate::mcp_tools::plan_100gb(tree, root, &a, 40),
                            );
                        }
                    });
                    ui.horizontal(|ui| {
                        if ui
                            .button(tr("Mapa de calor por edad", "Age heatmap"))
                            .on_hover_text(tr(
                                "Bytes y archivos por antigüedad; los más viejos y grandes primero. (Experimental)",
                                "Bytes and files by age; oldest large files first. (Experimental)",
                            ))
                            .clicked()
                        {
                            self.set_report(
                                tr("Mapa de calor por edad", "Age heatmap"),
                                crate::mcp_tools::age_heatmap(tree, root, &caps, 20),
                            );
                        }
                        if ui
                            .button(tr("WSL / Docker", "WSL / Docker"))
                            .on_hover_text(tr(
                                "Discos virtuales de WSL y Docker: espacio recuperable por compactación, no borrando. (Experimental)",
                                "WSL and Docker virtual disks: space recoverable by compaction, not deletion. (Experimental)",
                            ))
                            .clicked()
                        {
                            self.set_report(
                                tr("WSL / Docker", "WSL / Docker"),
                                crate::mcp_tools::wsl_docker(tree, root, &caps, 20),
                            );
                        }
                        if ui
                            .button(tr("Carpetas duplicadas", "Duplicate folders"))
                            .on_hover_text(tr(
                                "Agrupa carpetas con la misma estructura y tamaños de archivo. Comparación estructural: revisá antes de borrar. (Experimental)",
                                "Groups folders with the same structure and file sizes. Structural comparison: review before deleting. (Experimental)",
                            ))
                            .clicked()
                        {
                            self.set_report(
                                tr("Carpetas duplicadas (exp)", "Duplicate folders (exp)"),
                                crate::mcp_tools::duplicate_folders(tree, root, &caps, 20),
                            );
                        }
                        if ui
                            .button(tr("GitHub multicopia", "GitHub clones"))
                            .on_hover_text(tr(
                                "Agrupa clones del mismo repositorio por remoto y estima bytes redundantes. No hace fetch ni borra. (Experimental)",
                                "Groups clones of the same repo by remote and estimates redundant bytes. No fetch, no delete. (Experimental)",
                            ))
                            .clicked()
                        {
                            self.set_report(
                                tr("GitHub multicopia", "GitHub clones"),
                                crate::mcp_tools::github_clones(tree, root, &caps, 20),
                            );
                        }
                        if ui
                            .button(tr("Pronóstico de disco", "Disk forecast"))
                            .on_hover_text(tr(
                                "Días hasta llenarse según los snapshots guardados y el espacio libre actual. (Experimental)",
                                "Days until full from saved snapshots and current free space. (Experimental)",
                            ))
                            .clicked()
                        {
                            self.set_report(tr("Pronóstico de disco", "Disk forecast"), crate::mcp_tools::forecast(tree, root, &caps));
                        }
                    });
                    ui.separator();
                    ui.label(RichText::new(tr(
                        "Comparar dos máquinas o volúmenes (.wzsnap/.ecat)",
                        "Compare two machines or volumes (.wzsnap/.ecat)",
                    )).strong());
                    ui.horizontal(|ui| {
                        ui.label("A:");
                        ui.add(egui::TextEdit::singleline(&mut self.machine_a).desired_width(260.0).hint_text(tr("snapshot o .ecat", "snapshot or .ecat")));
                        ui.label("B:");
                        ui.add(egui::TextEdit::singleline(&mut self.machine_b).desired_width(260.0));
                        if ui
                            .button(tr("Comparar", "Compare"))
                            .on_hover_text(tr(
                                "Cruza dos snapshots por ruta. Si las máquinas usan distinta unidad o usuario, usá el MCP con strip_a/strip_b. (Experimental)",
                                "Crosses two snapshots by path. If the machines use a different drive/user, use MCP with strip_a/strip_b. (Experimental)",
                            ))
                            .clicked()
                        {
                            let args = serde_json::json!({
                                "a": self.machine_a.trim(),
                                "b": self.machine_b.trim(),
                            });
                            self.set_report(
                                tr("Comparar máquinas (exp)", "Compare machines (exp)"),
                                crate::mcp_tools::machine_compare(&args, 20),
                            );
                        }
                    });
                });
            }
        });
    }

    /// Diálogo de arranque: no hay ningún motor de índice disponible.
    fn setup_ui(&mut self, ctx: &egui::Context) {
        if !self.setup_open {
            return;
        }
        if matches!(crate::everything::status(), crate::everything::Status::Ready) {
            self.setup_open = false;
            return;
        }
        // `detect` ejecuta `where.exe`: no lo hacer 60 veces por segundo.
        if self.setup_det.is_none() || self.setup_det_at.elapsed() > Duration::from_secs(2) {
            self.setup_det = Some(crate::setup::detect(&self.settings.census_path));
            self.setup_det_at = Instant::now();
        }
        let det = self.setup_det.clone().unwrap_or_default();
        let mut open = self.setup_open;
        egui::Window::new(tr("Falta un motor de índice", "No index engine found"))
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .default_width(640.0)
            .show(ctx, |ui| {
                ui.label(tr(
                    "WizEverything no recorre el disco: necesita Everything o Census para leer un índice. Elegí uno.",
                    "WizEverything does not scan the disk: it needs Everything or Census to read an index. Choose one.",
                ));
                ui.add_space(8.0);
                ui.heading("Everything");
                ui.label(tr(
                    "La opción completa: todas las unidades, actualización en vivo y menos configuración.",
                    "The complete option: all drives, live updates and less setup.",
                ));
                ui.label(crate::setup::status_text(det.everything_running, det.everything_installed.as_deref()));
                ui.horizontal(|ui| {
                    if det.everything_ready {
                        ui.colored_label(Color32::from_rgb(90, 180, 100), tr("Todo listo", "Ready"));
                        if ui.button(tr("Usar Everything", "Use Everything")).clicked() {
                            self.settings.engine = IndexEngine::Everything;
                            crate::everything::set_engine(IndexEngine::Everything);
                            self.settings.save();
                            self.setup_open = false;
                            if self.loading.is_some() {
                                self.reload_pending = true;
                            } else {
                                self.start_load(ctx);
                            }
                        }
                    } else if det.everything_running {
                        ui.label(RichText::new(tr("Cargando índice…", "Loading index…")).weak());
                    } else if let Some(p) = &det.everything_installed {
                        if ui.button(tr("Iniciar Everything", "Start Everything")).clicked() {
                            match crate::setup::start_everything(p) {
                                Ok(()) => self.setup_msg = Some(tr(
                                    "Everything se está iniciando. Esperá unos segundos y reintentá.",
                                    "Everything is starting. Wait a few seconds and retry.",
                                ).into()),
                                Err(e) => self.setup_msg = Some(e),
                            }
                        }
                    } else {
                        if det.winget
                            && ui
                                .button(tr("Instalar con winget", "Install with winget"))
                                .on_hover_text("winget install --id voidtools.Everything -e")
                                .clicked()
                        {
                            match crate::setup::install_everything() {
                                Ok(()) => self.setup_msg = Some(tr(
                                    "Instalación iniciada. Completala en la ventana de winget y después tocá Reintentar.",
                                    "Installation started. Finish it in the winget window, then press Retry.",
                                ).into()),
                                Err(e) => self.setup_msg = Some(e),
                            }
                        }
                        if ui.button(tr("Abrir descarga oficial", "Open official download")).clicked() {
                            crate::setup::open_everything_download();
                        }
                    }
                });
                ui.add_space(10.0);
                ui.separator();
                ui.heading("Census");
                ui.label(tr(
                    "Alternativa reducida: un volumen por vez. Requiere `census serve` (elevado para leer la MFT).",
                    "Reduced alternative: one volume at a time. Requires `census serve` (elevated to read the MFT).",
                ));
                ui.label(crate::setup::status_text(det.census_running, det.census_installed.as_deref()));
                ui.horizontal(|ui| {
                    ui.label(tr("census.exe:", "census.exe:"));
                    let r = ui.add(
                        egui::TextEdit::singleline(&mut self.settings.census_path)
                            .hint_text(r"C:\ruta\a\census.exe")
                            .desired_width(360.0),
                    );
                    if r.changed() {
                        self.settings.save();
                        self.setup_det = None; // redetectar con la ruta nueva
                    }
                    if r.lost_focus() {
                        self.settings.save();
                        self.setup_det = None;
                    }
                    if let Some(p) = &det.census_installed {
                        if det.census_ready {
                            ui.colored_label(Color32::from_rgb(90, 180, 100), tr("En ejecución", "Running"));
                            if ui.button(tr("Usar Census", "Use Census")).clicked() {
                                self.settings.engine = IndexEngine::Census;
                                crate::everything::set_engine(IndexEngine::Census);
                                self.settings.save();
                                self.setup_open = false;
                                if self.loading.is_some() {
                                    self.reload_pending = true;
                                } else {
                                    self.start_load(ctx);
                                }
                            }
                        } else if det.census_running {
                            ui.label(RichText::new(tr("Cargando índice…", "Loading index…")).weak());
                        } else if ui.button(tr("Iniciar Census", "Start Census")).clicked() {
                            match crate::setup::start_census(p) {
                                Ok(()) => self.setup_msg = Some(tr(
                                    "Census se está iniciando. Si Windows pide administrador, aceptalo; después tocá Reintentar.",
                                    "Census is starting. If Windows asks for administrator, accept it; then press Retry.",
                                ).into()),
                                Err(e) => self.setup_msg = Some(e),
                            }
                        }
                        if ui.button(tr("Abrir carpeta", "Open folder")).clicked() {
                            if let Some(dir) = p.parent() {
                                crate::setup::open_folder(dir);
                            }
                        }
                    }
                });
                if det.census_installed.is_none() {
                    ui.label(RichText::new(tr(
                        "No encontrado. Compilá Census (`cargo build --release`) y pegá la ruta de census.exe, o agregalo al PATH.",
                        "Not found. Build Census (`cargo build --release`) and paste the census.exe path, or add it to PATH.",
                    )).weak());
                }
                if let Some(m) = &self.setup_msg {
                    ui.add_space(6.0);
                    ui.label(RichText::new(m).color(Color32::from_rgb(220, 170, 70)));
                }
                ui.add_space(10.0);
                ui.separator();
                ui.horizontal(|ui| {
                    if ui.button(tr("Reintentar", "Retry")).clicked() {
                        match crate::everything::status() {
                            crate::everything::Status::Ready => self.setup_open = false,
                            crate::everything::Status::Loading => {
                                self.setup_msg = Some(tr("El motor está cargando; esperá un momento.", "The engine is loading; wait a moment.").into());
                            }
                            crate::everything::Status::NotRunning => {
                                self.setup_msg = Some(tr("Todavía no hay motor. Iniciá o instalá uno.", "Still no engine. Start or install one.").into());
                                if self.loading.is_none() {
                                    self.start_load(ctx);
                                }
                            }
                        }
                    }
                    if ui.button(tr("Seguir sin índice", "Continue without an index")).clicked() {
                        self.setup_open = false;
                    }
                });
            });
        // Si un botón lo cerró, no reabrirlo al final del frame.
        self.setup_open = self.setup_open && open;
    }

    /// Ventana para buscar dentro de archivos, con estimación previa.
    fn content_ui(&mut self, ui: &mut egui::Ui, tree: &Tree) {
        ui.label(tr("Texto a buscar (sin distinguir mayúsculas)", "Text to find (case-insensitive)"));
        ui.text_edit_singleline(&mut self.content_query);
        ui.checkbox(&mut self.content_deep, tr(
            "Profundo: OCR de imágenes y PDFs escaneados (más lento)",
            "Deep: OCR for images and scanned PDFs (slower)",
        ));
        ui.horizontal(|ui| {
            if ui
                .add_enabled(!self.content_query.trim().is_empty(), egui::Button::new(tr("Buscar", "Search")))
                .clicked()
            {
                let q = self.content_query.trim().to_string();
                let budget = if self.content_deep { 15_000 } else { 2_500 };
                let rep = crate::contents::search(tree, self.view_root, &q, self.content_deep, 20_000, budget, 100);
                let mut v = rep.to_json(&q, 100);
                v["mode"] = serde_json::json!(if self.content_deep { "deep" } else { "fast" });
                v["root"] = serde_json::json!(tree.full_path(self.view_root));
                self.set_report(tr("Buscar dentro de archivos (exp)", "Search inside files (exp)"), Ok(v));
            }
            if ui
                .button(tr("Estimar antes", "Estimate first"))
                .on_hover_text(tr(
                    "Cuenta archivos de texto, PDF e imágenes y estima Fast/Deep sin leer el contenido.",
                    "Counts text/PDF/image files and estimates Fast/Deep without reading content.",
                ))
                .clicked()
            {
                let mut v = crate::contents::estimate(tree, self.view_root);
                v["root"] = serde_json::json!(tree.full_path(self.view_root));
                self.set_report(tr("Estimación de búsqueda (exp)", "Search estimate (exp)"), Ok(v));
            }
        });
        ui.separator();
        ui.label(RichText::new(tr(
            "Fast: texto/código y primeras páginas de PDF. Deep: OCR con Tesseract (si está instalado) e imágenes.",
            "Fast: text/code and first PDF pages. Deep: OCR with Tesseract (if installed) and images.",
        )).weak().size(10.0));
    }

    fn content_report_ui(&mut self, ui: &mut egui::Ui, v: &serde_json::Value) {
        ui.heading(tr("Resultados", "Results"));
        ui.label(RichText::new(format!(
            "«{}»  ·  {} {}  ·  {} ms{}",
            v["query"].as_str().unwrap_or(""),
            v["matches"],
            tr("coincidencias", "matches"),
            v["elapsed_ms"].as_u64().unwrap_or(0),
            if v["partial"].as_bool().unwrap_or(false) { tr(" · parcial", " · partial") } else { "" }
        )).strong());
        ui.label(RichText::new(format!(
            "{} {}  ·  {} {}",
            fmt_int(v["scanned"].as_u64().unwrap_or(0)),
            tr("archivos revisados", "files scanned"),
            fmt_size(v["bytes_scanned"].as_u64().unwrap_or(0)),
            tr("leídos", "read")
        )).weak().size(10.0));
        ui.separator();
        if let Some(top) = v["top"].as_array() {
            egui::ScrollArea::vertical().max_height(420.0).show(ui, |ui| {
                for h in top {
                    ui.label(RichText::new(h["path"].as_str().unwrap_or("")).monospace().size(11.0));
                    ui.label(RichText::new(h["snippet"].as_str().unwrap_or("")).size(11.0));
                    ui.label(RichText::new(format!(
                        "{} · {}",
                        fmt_size(h["size"].as_u64().unwrap_or(0)),
                        h["kind"].as_str().unwrap_or("")
                    )).weak().size(9.0));
                    ui.separator();
                }
            });
        }
        if let Some(notes) = v["notes"].as_array() {
            for n in notes {
                if let Some(n) = n.as_str() { ui.label(RichText::new(format!("• {n}")).weak().size(10.0)); }
            }
        }
    }

    fn content_estimate_report_ui(&mut self, ui: &mut egui::Ui, v: &serde_json::Value) {
        ui.heading(tr("Cuánto tardaría", "How long it would take"));
        ui.label(RichText::new(v["root"].as_str().unwrap_or("")).monospace().size(10.0));
        egui::Grid::new("content_est").num_columns(3).spacing([16.0, 4.0]).show(ui, |ui| {
            ui.label(RichText::new(tr("Tipo", "Type")).strong());
            ui.label(RichText::new(tr("Archivos", "Files")).strong());
            ui.label(RichText::new(tr("Tamaño", "Size")).strong());
            ui.end_row();
            for k in ["text", "pdf", "images", "email"] {
                if let Some(x) = v.get(k) {
                    ui.label(k);
                    ui.label(fmt_int(x["files"].as_u64().unwrap_or(0)));
                    ui.label(fmt_size(x["bytes"].as_u64().unwrap_or(0)));
                    ui.end_row();
                }
            }
        });
        ui.add_space(6.0);
        ui.label(RichText::new(format!(
            "{}: {} s",
            tr("Fast estimado", "Estimated fast"),
            v["estimated_seconds_fast"]
        )).strong());
        ui.label(RichText::new(format!(
            "{}: {} s",
            tr("Deep estimado", "Estimated deep"),
            v["estimated_seconds_deep"]
        )).strong());
        ui.label(RichText::new(format!(
            "pdftotext: {}  ·  tesseract: {}",
            if v["pdftotext"].as_bool().unwrap_or(false) { "sí" } else { "no" },
            if v["tesseract"].as_bool().unwrap_or(false) { "sí" } else { "no" }
        )).weak().size(10.0));
        if let Some(notes) = v["notes"].as_array() {
            for n in notes {
                if let Some(n) = n.as_str() { ui.label(RichText::new(format!("• {n}")).weak().size(10.0)); }
            }
        }
    }

    /// Línea de tiempo: elegir dos snapshots y compararlos.
    fn timeline_ui(&mut self, ui: &mut egui::Ui, tree: &Tree) {
        let frames = self.timeline_frames.clone();
        if frames.is_empty() {
            ui.label(tr(
                "Todavía no hay snapshots. Usá «Guardar snapshot» y repetilo en otro momento para ver el cambio.",
                "No snapshots yet. Use Save snapshot and repeat later to see the change.",
            ));
            return;
        }
        ui.label(RichText::new(format!("{} snapshots", frames.len())).strong());
        if frames.len() < 2 {
            ui.label(tr("Hace falta al menos un segundo snapshot.", "At least a second snapshot is needed."));
            return;
        }
        let max = frames.len() - 2;
        self.timeline_sel = self.timeline_sel.min(max);
        ui.add(egui::Slider::new(&mut self.timeline_sel, 0..=max).text(tr("intervalo", "interval")));
        let (old, new) = (frames[self.timeline_sel].clone(), frames[self.timeline_sel + 1].clone());
        ui.label(format!(
            "{}  →  {}   ({} {})",
            crate::volumes::fmt_date(old.created),
            crate::volumes::fmt_date(new.created),
            (((new.created - old.created) as f64 / 86_400.0) * 10.0).round() / 10.0,
            tr("días", "days")
        ));
        ui.horizontal(|ui| {
            if ui.button(tr("Comparar este intervalo", "Compare this interval")).clicked() {
                let res = crate::timeline::compare(tree, &old, &new, 20, self.timeline_deep).map(|mut v| {
                    v["mode"] = serde_json::json!(if self.timeline_deep { "deep" } else { "fast" });
                    v
                });
                self.set_report(tr("Línea de tiempo (exp)", "Timeline (exp)"), res);
            }
            ui.checkbox(&mut self.timeline_deep, tr("Profundo", "Deep"));
        });
        ui.separator();
        egui::ScrollArea::vertical().max_height(360.0).show(ui, |ui| {
            for (i, f) in frames.iter().enumerate() {
                ui.horizontal(|ui| {
                    ui.label(RichText::new(crate::volumes::fmt_date(f.created)).monospace().size(11.0));
                    ui.label(RichText::new(format!("{} {}", fmt_int(f.entries as u64), tr("entradas", "entries"))).weak().size(10.0));
                    if i > 0 && ui.small_button(tr("vs anterior", "vs previous")).clicked() {
                        let prev = frames[i - 1].clone();
                        let res = crate::timeline::compare(tree, &prev, f, 20, false).map(|mut v| {
                            v["mode"] = serde_json::json!("fast");
                            v
                        });
                        self.set_report(tr("Línea de tiempo (exp)", "Timeline (exp)"), res);
                    }
                });
            }
        });
    }

    /// Cuarentena: restaurar o borrar copias guardadas por la app.
    fn quarantine_ui(&mut self, ui: &mut egui::Ui) {
        let total: u64 = self.quarantine_entries.iter().map(|e| e.bytes).sum();
        ui.horizontal(|ui| {
            ui.label(RichText::new(format!(
                "{} {}  ·  {}",
                self.quarantine_entries.len(),
                tr("archivos", "files"),
                fmt_size(total)
            )).strong());
            if ui.button(tr("Actualizar", "Refresh")).clicked() {
                self.quarantine_entries = crate::quarantine::scan();
                self.quarantine_armed = None;
            }
        });
        ui.label(RichText::new(format!(
            "{}: {}",
            tr("Ubicación", "Location"),
            crate::quarantine::root().display()
        )).weak().size(10.0));
        ui.label(RichText::new(tr(
            "Restaurar devuelve el archivo a su ruta original (no sobrescribe). Borrar elimina sólo la copia guardada.",
            "Restore puts the file back at its original path (never overwrites). Delete removes only the saved copy.",
        )).weak());
        ui.separator();
        if self.quarantine_entries.is_empty() {
            ui.label(tr("No hay archivos en cuarentena.", "No files in quarantine."));
            return;
        }
        egui::ScrollArea::vertical().max_height(420.0).show(ui, |ui| {
            let mut restore: Option<usize> = None;
            let mut purge: Option<usize> = None;
            for (i, e) in self.quarantine_entries.iter().enumerate() {
                ui.horizontal(|ui| {
                    ui.label(RichText::new(crate::volumes::fmt_date(e.at)).weak().size(10.0));
                    ui.label(RichText::new(fmt_size(e.bytes)).monospace().size(10.0));
                    ui.label(RichText::new(e.original.to_string_lossy()).monospace().size(11.0));
                });
                ui.horizontal(|ui| {
                    if ui
                        .small_button(tr("Restaurar", "Restore"))
                        .on_hover_text(tr("Vuelve a la ruta original", "Back to the original path"))
                        .clicked()
                    {
                        restore = Some(i);
                    }
                    let armed = self.quarantine_armed.as_ref() == Some(&e.original);
                    let label = if armed { tr("¿Borrar copia?", "Delete copy?") } else { tr("Borrar copia", "Delete copy") };
                    if ui
                        .small_button(label)
                        .on_hover_text(tr(
                            "Borra la copia en cuarentena; el archivo original ya no existe. No se puede deshacer.",
                            "Deletes the quarantined copy; the original file no longer exists. Cannot be undone.",
                        ))
                        .clicked()
                    {
                        if armed {
                            purge = Some(i);
                        } else {
                            self.quarantine_armed = Some(e.original.clone());
                        }
                    }
                    if !e.exists() {
                        ui.colored_label(Color32::from_rgb(220, 160, 60), tr("copia ausente", "copy missing"));
                    }
                });
                ui.separator();
            }
            if let Some(i) = restore {
                let e = self.quarantine_entries[i].clone();
                match crate::quarantine::restore(&e) {
                    Ok(()) => {
                        self.quarantine_entries.remove(i);
                        self.set_info(format!("{} {}", tr("Restaurado:", "Restored:"), e.original.display()));
                    }
                    Err(err) => self.set_error(err),
                }
            }
            if let Some(i) = purge {
                let e = self.quarantine_entries[i].clone();
                match crate::quarantine::purge(&e) {
                    Ok(()) => {
                        self.quarantine_entries.remove(i);
                        self.quarantine_armed = None;
                        self.set_info(format!("{} {}", tr("Copia borrada:", "Copy deleted:"), e.stored.display()));
                    }
                    Err(err) => self.set_error(err),
                }
            }
        });
    }

    /// Lista de discos guardados: mostrar, abrir, borrar y ver la fecha.
    fn saved_disks_ui(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        if self.volume_metas.is_empty() {
            return;
        }
        ui.add_space(8.0);
        ui.label(RichText::new(tr("Discos guardados (offline)", "Saved disks (offline)")).strong());
        let metas = self.volume_metas.clone();
        for m in metas {
            ui.horizontal(|ui| {
                let mut en = m.enabled;
                if ui
                    .checkbox(&mut en, "")
                    .on_hover_text(tr("Mostrar como unidad aunque esté desconectado", "Show as a drive even while disconnected"))
                    .changed()
                {
                    if let Err(e) = self.volumes.set_enabled(&m.id, en) {
                        self.set_error(e);
                    } else {
                        self.volume_metas = self.volumes.scan();
                        self.start_load(ctx);
                    }
                }
                ui.label(format!(
                    "{}  ·  {}  ·  {}",
                    m.display(),
                    m.retention.label().0,
                    crate::volumes::fmt_date(m.updated)
                ));
                if m.retention == crate::volumes::Retention::Full {
                    if ui.small_button(tr("Abrir", "Open")).clicked()
                        && let Some(t) = self.tree.clone()
                        && let Some(id) = t.drives.iter().copied().find(|&d| t.name(d) == m.name)
                    {
                        self.set_root(ctx, id);
                    }
                } else if ui
                    .small_button(tr("Resumen", "Summary"))
                    .on_hover_text(tr(
                        "La retención Resumen guarda totales, carpetas y archivos grandes, pero no se puede navegar.",
                        "Digest retention keeps totals, folders and largest files, but is not browsable.",
                    ))
                    .clicked()
                {
                    self.set_report(tr("Disco guardado (resumen)", "Saved disk (digest)"), Ok(m.to_json(false)));
                }
                let armed = self.volume_delete_armed.as_deref() == Some(m.id.as_str());
                let label = if armed { tr("¿Borrar datos?", "Delete data?") } else { tr("Borrar", "Delete") };
                if ui
                    .small_button(label)
                    .on_hover_text(tr(
                        "Borra el inventario guardado de la app. No toca el disco real.",
                        "Deletes the app's saved inventory. Does not touch the real disk.",
                    ))
                    .clicked()
                {
                    if armed {
                        match self.volumes.delete(&m.id) {
                            Ok(()) => {
                                self.volume_delete_armed = None;
                                self.volume_metas = self.volumes.scan();
                                self.start_load(ctx);
                            }
                            Err(e) => self.set_error(e),
                        }
                    } else {
                        self.volume_delete_armed = Some(m.id.clone());
                    }
                }
            });
            ui.label(RichText::new(format!(
                "{} {}  ·  {}  ·  {} {:.0}% ({})",
                tr("actualizado", "updated"),
                crate::volumes::fmt_date(m.updated),
                fmt_size(m.total_logical),
                tr("huella", "semihash"),
                m.semihash_pct(),
                tr("archivos con huella", "files hashed"),
            )).weak().size(10.0));
        }
    }

    /// Guarda una copia del disco conectado en segundo plano.
    fn start_volume_capture(&mut self, ctx: &egui::Context, drive: u32) {
        let Some(tree) = self.tree.clone() else { return };
        let name = tree.name(drive).to_string();
        let retention = self.capture_retention;
        let deep = self.capture_deep;
        let progress = Arc::new(crate::volumes::CaptureProgress::default());
        let p = progress.clone();
        let (tx, rx) = channel();
        let ctx2 = ctx.clone();
        std::thread::Builder::new()
            .name("volume-capture".into())
            .spawn(move || {
                let mut opts = if deep { crate::volumes::CaptureOpts::deep() } else { crate::volumes::CaptureOpts::fast() };
                opts.retention = retention;
                let r = crate::volumes::capture(&tree, drive, &opts, &p);
                let _ = tx.send(r);
                ctx2.request_repaint();
            })
            .expect("thread");
        self.volume_job = Some(VolumeJob { rx, progress, label: name, started: Instant::now() });
        self.capture_open = false;
        self.set_info(tr(
            "Guardando el disco… podés seguir usando la app.",
            "Saving the disk… you can keep using the app.",
        ).into());
    }

    /// Contenido de la ventana "Guardar disco".
    fn capture_ui(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        let Some((name, drive)) = self.capture_drive.clone() else { return };
        ui.label(RichText::new(format!("{} {name}", tr("Guardar copia de", "Save a copy of"))).strong());
        ui.label(RichText::new(tr(
            "Se guarda en la carpeta de datos de la app, no en el disco. Después de desconectarlo aparece como unidad offline.",
            "Stored in the app's data folder, not on the disk. After disconnecting it it appears as an offline drive.",
        )).weak());
        ui.add_space(4.0);
        ui.radio_value(&mut self.capture_retention, crate::volumes::Retention::Full, tr(
            "Retención completa — todas las rutas, tamaños y fechas; navegable y comparable",
            "Full retention — every path, size and date; browsable and comparable",
        ));
        ui.radio_value(&mut self.capture_retention, crate::volumes::Retention::Digest, tr(
            "Retención resumen — totales, carpetas y archivos más grandes; ocupa poco, no navega",
            "Digest retention — totals, folders and largest files; small, not browsable",
        ));
        ui.add_space(4.0);
        ui.checkbox(&mut self.capture_deep, tr(
            "Huella profunda (48 KiB por archivo; el rápido usa 8 KiB)",
            "Deep semihash (48 KiB per file; fast uses 8 KiB)",
        ));
        ui.label(RichText::new(tr(
            "La huella es una muestra, no un hash completo: reconoce copias y busca duplicados sin leer el disco entero.",
            "The semihash is a sample, not a full hash: it recognizes copies and finds duplicates without reading the whole disk.",
        )).weak());
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            if ui.button(tr("Guardar copia", "Save copy")).clicked() {
                self.start_volume_capture(ctx, drive);
            }
            if ui.button(tr("Cancelar", "Cancel")).clicked() {
                self.capture_open = false;
            }
        });
    }

    /// Registro de un disco guardado cuyo nombre coincide con el nodo del árbol.
    fn offline_record(&self, drive_name: &str) -> Option<&crate::volumes::VolumeRecord> {
        self.volume_metas.iter().find(|r| r.name == drive_name)
    }

    /// Learns the deep estimate from a completed fast run, for the next deep hover.
    fn remember_eta(
        &mut self,
        kind: &'static str,
        fast_ms: u64,
        res: &Result<serde_json::Value, String>,
    ) {
        if let Ok(v) = res {
            let deep = estimate_deep(kind, fast_ms, v);
            self.deep_eta.insert(kind, (fast_ms.max(1), deep.max(1)));
        }
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
            if let Some(j) = &self.volume_job {
                ui.spinner();
                let hashed = j.progress.hashed.load(Ordering::Relaxed);
                let total = j.progress.total.load(Ordering::Relaxed);
                ui.label(format!(
                    "{} {}  ·  {} {}/{}  ({} s)",
                    tr("Guardando disco", "Saving disk"),
                    j.label,
                    tr("huellas", "semihashes"),
                    fmt_int(hashed),
                    fmt_int(total),
                    fmt_dec(j.started.elapsed().as_secs_f64())
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
            MenuAction::Advice(path, deep) => {
                if let Some(tree) = self.tree.clone() {
                    let a = serde_json::json!({
                        "path": path,
                        "mode": if deep { "deep" } else { "fast" },
                    });
                    let t0 = Instant::now();
                    let res = crate::mcp_tools::advice(&tree, self.view_root, &a);
                    if !deep {
                        self.remember_eta("advice", t0.elapsed().as_millis() as u64, &res);
                    }
                    self.set_report(
                        if deep {
                            tr("¿Puedo borrar esto? (profundo)", "Can I delete this? (deep)")
                        } else {
                            tr("¿Puedo borrar esto? (rápido)", "Can I delete this? (fast)")
                        },
                        res,
                    );
                }
            }
            MenuAction::Provenance(path, deep) => {
                if let Some(tree) = self.tree.clone() {
                    let a = serde_json::json!({
                        "path": path,
                        "mode": if deep { "deep" } else { "fast" },
                    });
                    let res = crate::mcp_tools::provenance(&tree, self.view_root, &a);
                    self.set_report(
                        if deep {
                            tr("Procedencia (profundo)", "Provenance (deep)")
                        } else {
                            tr("Procedencia (rápido)", "Provenance (fast)")
                        },
                        res,
                    );
                }
            }
            MenuAction::Quarantine(path) => {
                self.basket_push(crate::executor::Op::Quarantine {
                    path: std::path::PathBuf::from(path),
                });
                self.basket_open = true;
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
            .show(ctx, |ui| egui::Frame::popup(ui.style()).show(ui, |ui| context_menu(ui, tree.as_deref(), &target, excluded, self.deep_eta.get("advice").copied())).inner);
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
        let key = (self.view_root, self.revision, self.file_filter.clone(), self.files_sort, self.type_filter);
        if key != self.files_key {
            if (key.0, key.1, &key.2, key.4) != (self.files_key.0, self.files_key.1, &self.files_key.2, self.files_key.4) {
                let cap = if self.type_filter.is_some() { 20_000 } else { 1000 };
                self.files = tree.largest_files(self.view_root, &self.file_filter, cap);
                if let Some(code) = self.type_filter {
                    let h = self.ext_bytes_for(tree, code);
                    self.files.retain(|&id| h.bytes.get(id as usize).copied().unwrap_or(0) > 0);
                    self.files.truncate(1000);
                }
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

    fn basket_caps(&self) -> serde_json::Value {
        serde_json::json!({
            "all_drives": true,
            "max_scan": 2000,
            "max_files": 5000,
            "min_score": 40,
            "max_entries": 20000
        })
    }

    fn op_bytes(op: &crate::executor::Op) -> u64 {
        match op {
            crate::executor::Op::HardlinkMerge { duplicate, .. }
            | crate::executor::Op::Quarantine { path: duplicate } => {
                crate::winphys::physical_size(duplicate).unwrap_or(0)
            }
            crate::executor::Op::Compress { path } => {
                std::fs::metadata(path).map(|m| m.len() / 2).unwrap_or(0)
            }
            _ => 0,
        }
    }

    fn basket_push(&mut self, op: crate::executor::Op) {
        let d = op.describe();
        if self.basket.iter().any(|o| o.describe() == d) {
            return;
        }
        self.basket_bytes += Self::op_bytes(&op);
        self.basket.push(op);
    }

    fn basket_recount(&mut self) {
        self.basket_bytes = self.basket.iter().map(Self::op_bytes).sum();
    }

    fn basket_load_hardlinks(&mut self, tree: &Tree) {
        let caps = self.basket_caps();
        match crate::mcp_tools::hardlink_plan(tree, self.view_root, &caps, 200) {
            Ok(v) => {
                let mut n = 0;
                let empty: Vec<serde_json::Value> = Vec::new();
                for g in v["top"].as_array().unwrap_or(&empty) {
                    let paths: Vec<String> = g["paths"]
                        .as_array()
                        .map(|a| a.iter().filter_map(|x| x.as_str()).map(String::from).collect())
                        .unwrap_or_default();
                    if paths.len() < 2 {
                        continue;
                    }
                    let master = PathBuf::from(&paths[0]);
                    let drive: String = paths[0].chars().take(2).collect();
                    for d in &paths[1..] {
                        let dd: String = d.chars().take(2).collect();
                        if !dd.eq_ignore_ascii_case(&drive) {
                            continue;
                        }
                        self.basket_push(crate::executor::Op::HardlinkMerge {
                            master: master.clone(),
                            duplicate: PathBuf::from(d),
                        });
                        n += 1;
                    }
                }
                self.basket_note = Some(format!("{n} hardlinks agregados / added"));
            }
            Err(e) => self.basket_note = Some(format!("Error: {e}")),
        }
    }

    fn basket_load_compression(&mut self, tree: &Tree) {
        let caps = self.basket_caps();
        match crate::mcp_tools::compression_candidates(tree, self.view_root, &caps, 100) {
            Ok(v) => {
                let empty: Vec<serde_json::Value> = Vec::new();
                for x in v["top"].as_array().unwrap_or(&empty) {
                    if let Some(p) = x["path"].as_str() {
                        self.basket_push(crate::executor::Op::Compress { path: PathBuf::from(p) });
                    }
                }
                self.basket_note = Some(tr("Candidatos a compresión agregados", "Compression candidates added").to_string());
            }
            Err(e) => self.basket_note = Some(format!("Error: {e}")),
        }
    }

    fn basket_load_similar(&mut self, tree: &Tree) {
        let caps = self.basket_caps();
        let mut n = 0;
        for kind in ["images", "audio"] {
            let r = if kind == "images" {
                crate::mcp_tools::similar_images(tree, self.view_root, &caps, 50)
            } else {
                crate::mcp_tools::similar_audio(tree, self.view_root, &caps, 50)
            };
            let Ok(v) = r else { continue };
            let empty: Vec<serde_json::Value> = Vec::new();
            for g in v["top"].as_array().unwrap_or(&empty) {
                // Only clean groups: no review note and no AI-modified variant.
                if !g["note"].is_null() || g["ai_modified"].as_bool().unwrap_or(false) {
                    continue;
                }
                let others: Vec<serde_json::Value> = Vec::new();
                for o in g["others"].as_array().unwrap_or(&others) {
                    if let Some(p) = o["path"].as_str() {
                        self.basket_push(crate::executor::Op::Quarantine { path: PathBuf::from(p) });
                        n += 1;
                    }
                }
            }
        }
        self.basket_note = Some(format!("{n} candidatos a cuarentena agregados / added"));
    }

    fn basket_ui(&mut self, ui: &mut egui::Ui, tree: &Tree) {
        ui.label(
            RichText::new(tr(
                "Nada se toca hasta que simules y confirmes. La cuarentena mueve a la carpeta de datos de la app (reversible); los hardlinks verifican contenido idéntico; LZX es reversible.",
                "Nothing is touched until you simulate and confirm. Quarantine moves files to the app data folder (reversible); hardlinks verify identical content; LZX is reversible.",
            ))
            .weak(),
        );
        ui.horizontal(|ui| {
            if ui
                .button(tr("Cargar hardlinks", "Load hardlinks"))
                .on_hover_text(tr(
                    "Agrega pares duplicados exactos del mismo disco listos para fusionar como hardlink.",
                    "Adds exact duplicate pairs from the same volume ready to merge as hardlinks.",
                ))
                .clicked()
            {
                self.basket_load_hardlinks(tree);
            }
            if ui
                .button(tr("Cargar compresión", "Load compression"))
                .on_hover_text(tr(
                    "Agrega archivos fríos candidatos a compresión LZX de Windows.",
                    "Adds cold files that are LZX compression candidates.",
                ))
                .clicked()
            {
                self.basket_load_compression(tree);
            }
            if ui
                .button(tr("Cargar similares", "Load similar files"))
                .on_hover_text(tr(
                    "Agrega a cuarentena los candidatos de grupos limpios (sin nota de revisión ni variantes IA).",
                    "Adds quarantine entries for candidates in clean groups (no review note, no AI variants).",
                ))
                .clicked()
            {
                self.basket_load_similar(tree);
            }
            if ui.button(tr("Vaciar", "Clear")).clicked() {
                self.basket.clear();
                self.basket_bytes = 0;
                self.basket_report = None;
                self.basket_armed = false;
                self.basket_note = None;
            }
        });
        if let Some(n) = &self.basket_note {
            ui.label(RichText::new(n).weak());
        }
        ui.separator();
        let (mut hl, mut cp, mut qu) = (0, 0, 0);
        for op in &self.basket {
            match op {
                crate::executor::Op::HardlinkMerge { .. } => hl += 1,
                crate::executor::Op::Compress { .. } => cp += 1,
                crate::executor::Op::Quarantine { .. } => qu += 1,
                _ => {}
            }
        }
        ui.label(format!(
            "{} operaciones · {} recuperables · {hl} hardlinks · {cp} LZX · {qu} cuarentena",
            self.basket.len(),
            fmt_size(self.basket_bytes)
        ));
        let mut remove: Option<usize> = None;
        egui::ScrollArea::vertical().max_height(240.0).show(ui, |ui| {
            for (i, op) in self.basket.iter().enumerate() {
                ui.horizontal(|ui| {
                    if ui.small_button("✕").on_hover_text(tr("Quitar", "Remove")).clicked() {
                        remove = Some(i);
                    }
                    ui.label(RichText::new(op.describe()).monospace().size(11.0));
                });
            }
        });
        if let Some(i) = remove {
            let b = Self::op_bytes(&self.basket[i]);
            self.basket.remove(i);
            self.basket_bytes = self.basket_bytes.saturating_sub(b);
        }
        ui.separator();
        ui.horizontal(|ui| {
            if ui
                .add_enabled(!self.basket.is_empty(), egui::Button::new(tr("Simular (dry-run)", "Simulate (dry-run)")))
                .on_hover_text(tr(
                    "Calcula qué se recuperaría sin tocar nada y sin escribir auditoría.",
                    "Estimates what would be reclaimed without touching anything and without writing audit entries.",
                ))
                .clicked()
            {
                match crate::basket::RealBackend::new() {
                    Ok(mut be) => {
                        let plan = crate::executor::ExecutionPlan::dry_run(self.basket.clone());
                        self.basket_report = Some(crate::executor::run(&plan, &mut be, None));
                    }
                    Err(e) => self.basket_note = Some(format!("Error: {e}")),
                }
            }
            if let Some(r) = &self.basket_report {
                ui.label(format!(
                    "{} · {} operaciones · {} · {} fallos",
                    if r.dry_run { tr("Simulación", "Simulation") } else { tr("Ejecutado", "Executed") },
                    r.outcomes.len(),
                    fmt_size(if r.dry_run { r.potential } else { r.recovered }),
                    r.failures
                ));
            }
        });
        if let Some(r) = &self.basket_report {
            let failures: Vec<&crate::executor::OpOutcome> =
                r.outcomes.iter().filter(|o| o.status.is_failure()).collect();
            if !failures.is_empty() {
                egui::CollapsingHeader::new(format!("Fallos ({})", failures.len())).show(ui, |ui| {
                    for o in failures.iter().take(50) {
                        if let crate::executor::OpStatus::Failed(why) = &o.status {
                            ui.colored_label(
                                Color32::from_rgb(210, 90, 90),
                                format!("{} — {why}", o.description),
                            );
                        }
                    }
                });
            }
        }
        ui.add_space(4.0);
        ui.checkbox(
            &mut self.basket_armed,
            tr(
                "Entiendo que esto modifica archivos (fusiona hardlinks, mueve a cuarentena o comprime).",
                "I understand this modifies files (merges hardlinks, moves to quarantine or compresses).",
            ),
        );
        let can_run = self.basket_armed && !self.basket.is_empty();
        if ui
            .add_enabled(can_run, egui::Button::new(tr("Ejecutar de verdad", "Execute for real")))
            .on_hover_text(tr(
                "Ejecuta las operaciones y deja una línea por operación en %LOCALAPPDATA%\\WizEverything\\audit.jsonl.",
                "Runs the operations and writes one line per operation to %LOCALAPPDATA%\\WizEverything\\audit.jsonl.",
            ))
            .clicked()
        {
            match crate::basket::RealBackend::new() {
                Ok(mut be) => {
                    let log_path = be.log.clone();
                    let mut audit = std::fs::OpenOptions::new()
                        .create(true)
                        .append(true)
                        .open(&log_path)
                        .ok();
                    let plan = crate::executor::ExecutionPlan::live(self.basket.clone());
                    let rep = crate::executor::run(
                        &plan,
                        &mut be,
                        audit.as_mut().map(|f| f as &mut dyn std::io::Write),
                    );
                    let failed: Vec<crate::executor::Op> = rep
                        .outcomes
                        .iter()
                        .zip(self.basket.iter())
                        .filter(|(o, _)| o.status.is_failure())
                        .map(|(_, op)| op.clone())
                        .collect();
                    self.basket = failed;
                    self.basket_armed = false;
                    self.basket_report = Some(rep);
                    self.basket_recount();
                }
                Err(e) => self.basket_note = Some(format!("Error: {e}")),
            }
        }
    }

    fn why_report_ui(&mut self, ui: &mut egui::Ui, v: &serde_json::Value) {
        let total = v["total_bytes"].as_u64().unwrap_or(0);
        ui.heading(tr("¿Por qué está lleno?", "Why is it full?"));
        ui.label(format!(
            "{} · {} archivos · {} carpetas",
            fmt_size(total),
            v["files"],
            v["folders"]
        ));
        if let Some(d) = v.get("drive").and_then(|x| x.as_object()) {
            let free = d["free"].as_u64().unwrap_or(0);
            let tot = d["total"].as_u64().unwrap_or(0);
            let frac = if tot > 0 { free as f32 / tot as f32 } else { 0.0 };
            ui.add(
                egui::ProgressBar::new(frac)
                    .desired_width(280.0)
                    .text(format!("libre {}", fmt_size(free))),
            );
        }
        ui.separator();
        ui.label(RichText::new(tr("Carpetas más grandes", "Largest folders")).strong());
        let empty: Vec<serde_json::Value> = Vec::new();
        for f in v["top_folders"].as_array().unwrap_or(&empty) {
            let p = f["pct"].as_f64().unwrap_or(0.0);
            ui.horizontal(|ui| {
                ui.label(f["name"].as_str().unwrap_or(""));
                ui.add(egui::ProgressBar::new((p / 100.0) as f32).desired_width(140.0));
                ui.label(format!("{} · {p:.1}%", fmt_size(f["bytes"].as_u64().unwrap_or(0))));
                if let Some(path) = f["path"].as_str() {
                    ui.label(RichText::new(path).weak().size(10.0));
                }
            });
        }
        if let Some(chain) = v["chain"].as_array()
            && !chain.is_empty()
        {
            ui.separator();
            ui.label(RichText::new(tr("El camino al fondo", "The path to the bottom")).strong());
            let names: Vec<&str> = chain.iter().filter_map(|c| c["name"].as_str()).collect();
            ui.label(RichText::new(names.join("  →  ")).monospace());
            for c in chain {
                ui.label(format!(
                    "{}  {} · {:.1}%",
                    if c["is_dir"].as_bool().unwrap_or(false) { "📁" } else { "📄" },
                    fmt_size(c["bytes"].as_u64().unwrap_or(0)),
                    c["pct"].as_f64().unwrap_or(0.0)
                ));
            }
        }
        if let Some(files) = v["top_files"].as_array()
            && !files.is_empty()
        {
            ui.separator();
            ui.label(RichText::new(tr("Archivos más grandes", "Largest files")).strong());
            for f in files {
                ui.label(format!(
                    "{} · {:.1}%  {}",
                    fmt_size(f["bytes"].as_u64().unwrap_or(0)),
                    f["pct"].as_f64().unwrap_or(0.0),
                    f["path"].as_str().unwrap_or("")
                ));
            }
        }
    }

    fn reclaim_report_ui(&mut self, ui: &mut egui::Ui, v: &serde_json::Value) {
        ui.heading(tr("Planificador de espacio", "Space planner"));
        ui.label(format!(
            "{} / {} ({}%) · modo {} · {}",
            fmt_size(v["selected_bytes"].as_u64().unwrap_or(0)),
            fmt_size(v["target_bytes"].as_u64().unwrap_or(0)),
            v["percent"].as_f64().unwrap_or(0.0),
            mode_es(v),
            if v["reached"].as_bool().unwrap_or(false) { tr("objetivo alcanzado", "target reached") } else { tr("parcial", "partial") },
        ));
        ui.label(RichText::new(v["note"].as_str().unwrap_or("")).weak());
        ui.separator();
        let empty: Vec<serde_json::Value> = Vec::new();
        egui::ScrollArea::vertical().max_height(360.0).show(ui, |ui| {
            for it in v["items"].as_array().unwrap_or(&empty) {
                let risk = it["risk"].as_str().unwrap_or("review");
                let color = match risk {
                    "safe" => Color32::from_rgb(90, 180, 100),
                    "keep" => Color32::from_rgb(220, 90, 90),
                    _ => Color32::from_rgb(220, 170, 70),
                };
                ui.horizontal(|ui| {
                    ui.colored_label(color, format!("{risk:>6}"));
                    ui.label(format!("{:>9}", fmt_size(it["bytes"].as_u64().unwrap_or(0))));
                    ui.label(RichText::new(it["path"].as_str().unwrap_or("")).size(10.0));
                });
            }
        });
    }

    fn age_report_ui(&mut self, ui: &mut egui::Ui, v: &serde_json::Value) {
        ui.heading(tr("Mapa de calor por edad", "Age heatmap"));
        ui.label(format!("{} · {}", fmt_size(v["total_bytes"].as_u64().unwrap_or(0)), v["note"].as_str().unwrap_or("")));
        let empty: Vec<serde_json::Value> = Vec::new();
        for b in v["buckets"].as_array().unwrap_or(&empty) {
            let label = b["bucket"].as_str().unwrap_or("?");
            let frac = (b["pct"].as_f64().unwrap_or(0.0) / 100.0) as f32;
            ui.horizontal(|ui| {
                ui.add(egui::ProgressBar::new(frac).desired_width(180.0));
                ui.label(format!("{label:>9}  {:>9}  {}", fmt_size(b["bytes"].as_u64().unwrap_or(0)), b["files"]));
            });
        }
    }

    fn wsl_report_ui(&mut self, ui: &mut egui::Ui, v: &serde_json::Value) {
        ui.heading(tr("WSL / Docker", "WSL / Docker"));
        ui.label(format!(
            "{} · {}",
            tr("recuperable por compactación", "recoverable by compaction"),
            fmt_size(v["reclaimable_candidate"].as_u64().unwrap_or(0))
        ));
        let empty: Vec<serde_json::Value> = Vec::new();
        for it in v["items"].as_array().unwrap_or(&empty) {
            ui.horizontal(|ui| {
                ui.label(format!("{:>9}  {:>8}", fmt_size(it["bytes"].as_u64().unwrap_or(0)), it["kind"].as_str().unwrap_or("")));
                ui.label(RichText::new(it["path"].as_str().unwrap_or("")).size(10.0));
            });
        }
        for w in v["warnings"].as_array().unwrap_or(&empty) {
            ui.colored_label(Color32::from_rgb(220, 170, 70), w.as_str().unwrap_or(""));
        }
    }

    fn machine_compare_report_ui(&mut self, ui: &mut egui::Ui, v: &serde_json::Value) {
        ui.heading(tr("Comparar máquinas", "Compare machines"));
        ui.label(format!(
            "A: {} ({} archivos)  ·  B: {} ({} archivos)",
            v["a"]["label"].as_str().unwrap_or("?"),
            v["a"]["entries"],
            v["b"]["label"].as_str().unwrap_or("?"),
            v["b"]["entries"]
        ));
        ui.label(format!(
            "{} neto · {}% de rutas en común",
            fmt_size(v["net_bytes"].as_i64().unwrap_or(0).unsigned_abs()),
            v["overlap_pct"].as_f64().unwrap_or(0.0)
        ));
        let empty: Vec<serde_json::Value> = Vec::new();
        for w in v["warnings"].as_array().unwrap_or(&empty) {
            ui.colored_label(Color32::from_rgb(220, 160, 70), w.as_str().unwrap_or(""));
        }
        for (key, title) in [
            ("only_a", tr("Sólo en A", "Only in A")),
            ("only_b", tr("Sólo en B", "Only in B")),
            ("grown", tr("Crecieron", "Grew")),
            ("shrunk", tr("Se achicaron", "Shrank")),
        ] {
            let sec = &v[key];
            if sec["files"].as_u64().unwrap_or(0) == 0 {
                continue;
            }
            ui.separator();
            ui.label(RichText::new(format!("{title}: {} archivos · {}", sec["files"], fmt_size(sec["bytes"].as_u64().unwrap_or(0)))).strong());
            for it in sec["top"].as_array().unwrap_or(&empty) {
                ui.horizontal(|ui| {
                    let b = it["bytes"].as_u64().unwrap_or(0);
                    ui.label(format!("{:>9}", fmt_size(b)));
                    ui.label(RichText::new(it["path"].as_str().unwrap_or("")).size(10.0));
                });
            }
        }
    }

    fn duplicate_folders_report_ui(&mut self, ui: &mut egui::Ui, v: &serde_json::Value) {
        ui.heading(tr("Carpetas duplicadas", "Duplicate folders"));
        ui.label(format!(
            "{} carpetas · {} grupos · {} redundantes · modo {}",
            v["folders"],
            v["groups"],
            fmt_size(v["redundant_bytes"].as_u64().unwrap_or(0)),
            mode_es(v)
        ));
        ui.label(RichText::new(v["note"].as_str().unwrap_or("")).weak());
        if v["partial"].as_bool().unwrap_or(false) {
            ui.colored_label(
                Color32::from_rgb(220, 160, 70),
                tr(
                    "Parcial: se alcanzó el tope de carpetas analizadas.",
                    "Partial: the folder-analysis cap was reached.",
                ),
            );
        }
        if v["truncated"].as_bool().unwrap_or(false) {
            ui.label(RichText::new(format!(
                "Mostrando {} de {} grupos.",
                v["shown"],
                v["groups"]
            )).weak());
        }
        let empty: Vec<serde_json::Value> = Vec::new();
        for g in v["items"].as_array().unwrap_or(&empty) {
            ui.separator();
            if g["nested"].as_bool().unwrap_or(false) {
                ui.colored_label(
                    Color32::from_rgb(220, 160, 70),
                    tr("Anidada dentro de otro grupo: no suma bytes.", "Nested inside another group: not counted."),
                );
            }
            ui.label(RichText::new(format!(
                "{} copias · {} redundantes · conservar: {}",
                g["copies"],
                fmt_size(g["redundant_bytes"].as_u64().unwrap_or(0)),
                g["keep_suggestion"].as_str().unwrap_or("")
            )).strong());
            for c in g["items"].as_array().unwrap_or(&empty) {
                ui.horizontal(|ui| {
                    ui.label(format!("{:>9}", fmt_size(c["bytes"].as_u64().unwrap_or(0))));
                    ui.label(RichText::new(c["path"].as_str().unwrap_or("")).size(10.0));
                });
            }
        }
        if let Some(notes) = v["notes"].as_array() {
            for n in notes {
                if let Some(n) = n.as_str() {
                    ui.label(RichText::new(format!("• {n}")).weak().size(10.0));
                }
            }
        }
    }

    fn github_clones_report_ui(&mut self, ui: &mut egui::Ui, v: &serde_json::Value) {
        ui.heading(tr("GitHub multicopia", "GitHub clones"));
        ui.label(format!(
            "{} repos · {} grupos · {} redundantes · modo {}",
            v["repos"],
            v["groups"],
            fmt_size(v["redundant_bytes"].as_u64().unwrap_or(0)),
            mode_es(v)
        ));
        let empty: Vec<serde_json::Value> = Vec::new();
        for g in v["items"].as_array().unwrap_or(&empty) {
            ui.separator();
            ui.label(RichText::new(g["remote"].as_str().unwrap_or("")).strong());
            for c in g["copies"].as_array().unwrap_or(&empty) {
                ui.horizontal(|ui| {
                    ui.label(format!("{:>9}", fmt_size(c["bytes"].as_u64().unwrap_or(0))));
                    ui.label(RichText::new(c["path"].as_str().unwrap_or("")).size(10.0));
                });
            }
        }
    }

    fn forecast_report_ui(&mut self, ui: &mut egui::Ui, v: &serde_json::Value) {
        ui.heading(tr("Pronóstico de disco", "Disk forecast"));
        if let Some(d) = v["days_to_full"].as_f64() {
            ui.label(RichText::new(format!("≈ {d:.0} {} ", tr("días hasta llenarse", "days until full"))).size(22.0).strong());
        }
        ui.label(format!(
            "{} / día · libre {} · {} snapshots · confianza {}",
            fmt_size(v["per_day"].as_f64().unwrap_or(0.0).max(0.0) as u64),
            fmt_size(v["free_bytes"].as_u64().unwrap_or(0)),
            v["samples"],
            v["confidence"].as_str().unwrap_or("?")
        ));
    }

    fn growth_report_ui(&mut self, ui: &mut egui::Ui, v: &serde_json::Value) {
        let net = v["net_bytes"].as_i64().unwrap_or(0);
        ui.heading(tr("¿Qué creció?", "What grew?"));
        let (sign, color) = if net >= 0 {
            ("+", Color32::from_rgb(210, 90, 90))
        } else {
            ("", Color32::from_rgb(90, 170, 100))
        };
        ui.colored_label(
            color,
            format!(
                "{sign}{} neto · {} nuevos · {} borrados · {} crecieron · {} se achicaron",
                fmt_size(net.unsigned_abs()),
                v["added"],
                v["removed"],
                v["grown"],
                v["shrunk"]
            ),
        );
        if let Some(a) = v["attributed_pct"].as_f64() {
            ui.label(RichText::new(format!(
                "atribuido {a:.0}% del cambio · modo {} · {} ms",
                mode_es(v),
                v["elapsed_ms"].as_u64().unwrap_or(0)
            )).weak());
        }
        ui.separator();
        ui.label(RichText::new(tr("Carpetas que más cambiaron", "Folders that changed most")).strong());
        let empty: Vec<serde_json::Value> = Vec::new();
        let folders = v["folders"].as_array().unwrap_or(&empty);
        if folders.is_empty() {
            ui.label(RichText::new(tr(
                "Sin cambios entre los dos snapshots (o son consecutivos sin actividad).",
                "No changes between the two snapshots (or they are consecutive with no activity).",
            )).weak());
        }
        let max = folders
            .iter()
            .map(|f| f["bytes"].as_i64().unwrap_or(0).unsigned_abs())
            .max()
            .unwrap_or(1)
            .max(1);
        for f in folders {
            let b = f["bytes"].as_i64().unwrap_or(0);
            let frac = b.unsigned_abs() as f32 / max as f32;
            let col = if b >= 0 {
                Color32::from_rgb(210, 90, 90)
            } else {
                Color32::from_rgb(90, 170, 100)
            };
            ui.horizontal(|ui| {
                ui.add(egui::ProgressBar::new(frac).desired_width(120.0).fill(col));
                ui.colored_label(
                    col,
                    format!(
                        "{}{}",
                        if b >= 0 { "+" } else { "-" },
                        fmt_size(b.unsigned_abs())
                    ),
                );
                ui.label(RichText::new(f["folder"].as_str().unwrap_or("")).size(10.0));
            });
        }
        for (key, title) in [
            ("new_files", tr("Archivos nuevos más grandes", "Largest new files")),
            ("grown_files", tr("Archivos que más crecieron", "Files that grew most")),
        ] {
            if let Some(arr) = v[key].as_array()
                && !arr.is_empty()
            {
                ui.separator();
                ui.label(RichText::new(title).strong());
                for f in arr {
                    let extra = if key == "grown_files" {
                        format!(" (+{})", fmt_size(f["delta"].as_i64().unwrap_or(0).unsigned_abs()))
                    } else {
                        format!(" ({})", fmt_size(f["bytes"].as_u64().unwrap_or(0)))
                    };
                    ui.label(RichText::new(format!("{}{extra}", f["path"].as_str().unwrap_or(""))).size(10.0));
                }
            }
        }
    }

    fn advice_report_ui(&mut self, ui: &mut egui::Ui, v: &serde_json::Value) {
        let verdict = v["verdict"].as_str().unwrap_or("caution");
        let (label, color) = match verdict {
            "safe" => (tr("SEGURO DE BORRAR", "SAFE TO DELETE"), Color32::from_rgb(90, 170, 100)),
            "keep" => (tr("MEJOR NO BORRAR", "BETTER KEEP IT"), Color32::from_rgb(210, 90, 90)),
            _ => (tr("PRECAUCIÓN", "CAUTION"), Color32::from_rgb(220, 160, 70)),
        };
        ui.heading(RichText::new(label).color(color));
        ui.label(RichText::new(v["path"].as_str().unwrap_or("")).monospace().size(11.0));
        ui.label(RichText::new(format!(
            "modo {} · confianza {}",
            mode_es(v),
            v["confidence"].as_str().unwrap_or("media")
        )).weak().size(10.0));
        ui.label(format!(
            "{} · {} archivos · modificado hace {} días · se recuperarían {}",
            fmt_size(v["bytes"].as_u64().unwrap_or(0)),
            v["files"],
            v["modified_days"],
            fmt_size(v["reclaim_bytes"].as_u64().unwrap_or(0))
        ));
        ui.separator();
        let empty: Vec<serde_json::Value> = Vec::new();
        for r in v["reasons"].as_array().unwrap_or(&empty) {
            ui.label(format!("• {}", r.as_str().unwrap_or("")));
        }
        if let Some(tags) = v.get("tags").and_then(|t| t.as_object()) {
            let mut chips: Vec<String> = Vec::new();
            if tags.get("git_tracked").and_then(|x| x.as_bool()).unwrap_or(false) {
                chips.push("git".into());
            }
            if let Some(a) = tags.get("artifact").and_then(|x| x.as_str()) {
                chips.push(a.to_string());
            }
            if tags.get("temp").and_then(|x| x.as_bool()).unwrap_or(false) {
                chips.push("temp".into());
            }
            if tags.get("system").and_then(|x| x.as_bool()).unwrap_or(false) {
                chips.push("sistema".into());
            }
            if tags.get("personal").and_then(|x| x.as_bool()).unwrap_or(false) {
                chips.push("personal".into());
            }
            let hl = tags.get("hard_links").and_then(|x| x.as_u64()).unwrap_or(0);
            if hl > 1 {
                chips.push(format!("{hl} hard links"));
            }
            if !chips.is_empty() {
                ui.label(RichText::new(chips.join(" · ")).weak());
            }
        }
        if let Some(actions) = v["actions"].as_array()
            && !actions.is_empty()
        {
            ui.separator();
            ui.label(RichText::new(tr("Qué hacer", "What to do")).strong());
            for a in actions {
                ui.label(format!("→ {}", a.as_str().unwrap_or("")));
            }
        }
    }

    fn backup_report_ui(&mut self, ui: &mut egui::Ui, v: &serde_json::Value) {
        let cov = v["coverage_pct"].as_f64().unwrap_or(0.0);
        let color = if cov >= 99.9 {
            Color32::from_rgb(90, 170, 100)
        } else if cov >= 80.0 {
            Color32::from_rgb(220, 160, 70)
        } else {
            Color32::from_rgb(210, 90, 90)
        };
        ui.heading(tr("¿Está respaldado?", "Is it backed up?"));
        ui.colored_label(color, format!("{cov:.1}% cubierto por tamaño · modo {}", mode_es(v)));
        ui.add(
            egui::ProgressBar::new((cov / 100.0) as f32)
                .desired_width(320.0)
                .fill(color)
                .text(format!(
                    "{} de {}",
                    fmt_size(v["covered_bytes"].as_u64().unwrap_or(0)),
                    fmt_size(v["source_bytes"].as_u64().unwrap_or(0))
                )),
        );
        ui.label(format!(
            "{} archivos en origen · {} faltan ({} ) · {} difieren ({} ) · {} solo en el backup ({})",
            v["source_files"],
            v["missing"],
            fmt_size(v["missing_bytes"].as_u64().unwrap_or(0)),
            v["differs"],
            fmt_size(v["differs_bytes"].as_u64().unwrap_or(0)),
            v["extra"],
            fmt_size(v["extra_bytes"].as_u64().unwrap_or(0))
        ));
        if let (Some(s), Some(t)) = (v["source"].as_str(), v["target"].as_str()) {
            ui.label(RichText::new(format!("{s}  →  {t}")).weak().size(10.0));
        }
        if v["truncated"].as_bool().unwrap_or(false) {
            ui.colored_label(
                Color32::from_rgb(220, 160, 70),
                tr(
                    "Muestra limitada (250.000 archivos): la cobertura es sobre esa muestra.",
                    "Sample capped (250,000 files): coverage is over that sample.",
                ),
            );
        }
        if let Some(arr) = v["top_missing"].as_array()
            && !arr.is_empty()
        {
            ui.separator();
            ui.label(RichText::new(tr("Faltan en el backup", "Missing from the backup")).strong());
            egui::Grid::new("bk_missing").striped(true).num_columns(2).show(ui, |ui| {
                for f in arr.iter().take(25) {
                    ui.label(RichText::new(f["rel"].as_str().unwrap_or("")).size(10.0));
                    ui.label(fmt_size(f["bytes"].as_u64().unwrap_or(0)));
                    ui.end_row();
                }
            });
        }
        if let Some(arr) = v["top_differs"].as_array()
            && !arr.is_empty()
        {
            ui.separator();
            ui.label(RichText::new(tr("Mismo nombre, distinto tamaño", "Same name, different size")).strong());
            egui::Grid::new("bk_differs").striped(true).num_columns(3).show(ui, |ui| {
                for f in arr.iter().take(25) {
                    ui.label(RichText::new(f["rel"].as_str().unwrap_or("")).size(10.0));
                    ui.label(fmt_size(f["source"].as_u64().unwrap_or(0)));
                    ui.label(fmt_size(f["target"].as_u64().unwrap_or(0)));
                    ui.end_row();
                }
            });
        }
    }

    fn backup_ui(&mut self, ui: &mut egui::Ui, tree: &Tree) {
        ui.label(RichText::new(tr(
            "Compara el origen contra una copia por ruta relativa y tamaño (no por fecha). Las dos carpetas tienen que estar en el índice.",
            "Compares the source against a copy by relative path and size (not by date). Both folders must be in the index.",
        )).weak());
        ui.add_space(4.0);
        ui.label(tr("Origen", "Source"));
        ui.text_edit_singleline(&mut self.backup_source);
        ui.label(tr("Copia / backup", "Copy / backup"));
        ui.text_edit_singleline(&mut self.backup_target);
        ui.checkbox(
            &mut self.backup_deep,
            tr("Profundo (250.000 archivos)", "Deep (250,000 files)"),
        );
        ui.add_space(4.0);
        if ui
            .add_enabled(!self.backup_target.trim().is_empty(), egui::Button::new(tr("Comparar", "Compare")))
            .clicked()
        {
            let a = serde_json::json!({
                "source": self.backup_source,
                "target": self.backup_target,
                "mode": if self.backup_deep { "deep" } else { "fast" },
            });
            let t0 = Instant::now();
            let res = crate::mcp_tools::backup_check(tree, self.view_root, &a, 30);
            if !self.backup_deep {
                self.remember_eta("backup", t0.elapsed().as_millis() as u64, &res);
            }
            self.set_report(tr("¿Está respaldado? (exp)", "Is it backed up? (exp)"), res);
        }
        if ui.button(tr("Usar la carpeta actual como origen", "Use current folder as source")).clicked() {
            self.backup_source = tree.full_path(self.view_root);
        }
    }

    fn open_audio_ab(&mut self, a: &str, b: &str) {
        self.ab_error = None;
        self.ab = None;
        self.ab_blind = false;
        self.ab_swapped = false;
        match crate::audio_ab::AbPlayer::open(std::path::Path::new(a), std::path::Path::new(b)) {
            Ok(p) => self.ab = Some(p),
            Err(e) => self.ab_error = Some(e),
        }
    }

    fn audio_ab_ui(&mut self, ui: &mut egui::Ui) {
        if let Some(err) = self.ab_error.clone() {
            ui.colored_label(Color32::from_rgb(210, 90, 90), err);
            if ui.button(tr("Cerrar", "Close")).clicked() {
                self.ab_error = None;
            }
            return;
        }
        let Some(p) = self.ab.as_ref() else { return };
        let mut blind = self.ab_blind;
        let mut swap = self.ab_swapped;
        let (name_a, name_b) = if swap {
            (&p.b_name, &p.a_name)
        } else {
            (&p.a_name, &p.b_name)
        };
        if blind {
            ui.label(RichText::new("A / B (ciego)").strong());
        } else {
            ui.label(format!("A: {name_a}"));
            ui.label(format!("B: {name_b}"));
        }
        ui.label(RichText::new(tr(
            "Misma posición en ambos; solo cambia cuál se oye. Se decodifican hasta 2 minutos desde el inicio. Ideal con auriculares.",
            "Same position in both; only which one is heard changes. Up to two minutes are decoded from the start. Best with headphones.",
        )).weak());
        ui.separator();
        let mut ms = p.interval_ms();
        let mut close = false;
        ui.horizontal(|ui| {
            if ui
                .button(if p.is_playing() {
                    tr("⏸ Pausa", "⏸ Pause")
                } else {
                    tr("▶ Reproducir", "▶ Play")
                })
                .clicked()
            {
                p.toggle_play();
            }
            let side = p.side();
            if ui.selectable_label(side == 0, "A").clicked() {
                p.set_side(0);
            }
            if ui.selectable_label(side == 1, "B").clicked() {
                p.set_side(1);
            }
            let mut auto = p.auto();
            if ui.checkbox(&mut auto, tr("Auto", "Auto")).changed() {
                p.set_auto(auto);
            }
            egui::ComboBox::from_id_salt("ab_interval")
                .selected_text(format!("{:.1}s", ms as f32 / 1000.0))
                .show_ui(ui, |ui| {
                    for v in [500u32, 1000, 1500, 2000, 4000] {
                        if ui
                            .selectable_label(ms == v, format!("{:.1}s", v as f32 / 1000.0))
                            .clicked()
                        {
                            ms = v;
                        }
                    }
                });
            if ui.checkbox(&mut blind, tr("Ciego", "Blind")).changed() {
                self.ab_blind = blind;
            }
            if ui
                .button(tr("Permutar A/B", "Swap A/B"))
                .on_hover_text(tr(
                    "Intercambia qué archivo es A y cuál B, para no dejarte llevar por el nombre.",
                    "Swaps which file is A and which is B, so the name does not bias you.",
                ))
                .clicked()
            {
                swap = !swap;
                p.set_swapped(swap);
                self.ab_swapped = swap;
            }
            if ui.button(tr("Cerrar", "Close")).clicked() {
                close = true;
            }
        });
        if ms != p.interval_ms() {
            p.set_interval_ms(ms);
        }
        let dur = p.duration();
        let mut pos = p.pos_secs();
        if ui
            .add(egui::Slider::new(&mut pos, 0.0..=dur).text(tr("posición", "position")))
            .changed()
        {
            p.seek_secs(pos);
        }
        ui.label(format!(
            "{:.1} / {:.1} s · {:.0} kHz · escuchando {}",
            p.pos_secs(),
            dur,
            p.rate as f32 / 1000.0,
            if p.side() == 0 { "A" } else { "B" }
        ));
        if close {
            self.ab = None;
        }
    }

    fn open_img_ab(&mut self, a: &str, b: &str) {
        let dim = |p: &str| image::image_dimensions(p).unwrap_or((0, 0));
        let bytes = |p: &str| std::fs::metadata(p).map(|m| m.len()).unwrap_or(0);
        self.img_ab = Some(crate::img_ab::ImgAb::new(
            a,
            b,
            dim(a),
            dim(b),
            bytes(a),
            bytes(b),
        ));
        self.img_ab_tex = None;
    }

    fn img_ab_ui(&mut self, ui: &mut egui::Ui) {
        let (ap, bp) = match &self.img_ab {
            Some(s) => (s.a_path.clone(), s.b_path.clone()),
            None => return,
        };
        if self.img_ab_tex.is_none() {
            let ctx = ui.ctx().clone();
            if let (Some((ta, da)), Some((tb, db))) = (load_full(&ctx, &ap), load_full(&ctx, &bp)) {
                if let Some(st) = &mut self.img_ab {
                    st.a_dim = da;
                    st.b_dim = db;
                }
                self.img_ab_tex = Some((ta, tb));
            } else {
                ui.colored_label(
                    Color32::from_rgb(210, 90, 90),
                    tr("No se pudieron cargar las imágenes.", "The images could not be loaded."),
                );
                return;
            }
        }
        let Some(st) = self.img_ab.as_mut() else { return };
        let Some((ta, tb)) = self.img_ab_tex.as_ref() else { return };

        if ui.rect_contains_pointer(ui.max_rect()) {
            let scroll = ui.input(|i| i.smooth_scroll_delta.y);
            if scroll.abs() > 0.01 {
                st.zoom = (st.zoom * (1.0 + scroll * 0.002)).clamp(1.0, 16.0);
            }
        }
        ui.horizontal(|ui| {
            ui.label(format!("Zoom {:.1}×", st.zoom));
            for z in [1.0f32, 2.0, 4.0, 8.0] {
                if ui.selectable_label((st.zoom - z).abs() < 0.01, format!("{z:.0}×")).clicked() {
                    st.zoom = z;
                }
            }
            if ui
                .button("1:1")
                .on_hover_text(tr(
                    "Un píxel de la imagen por píxel de pantalla.",
                    "One image pixel per screen pixel.",
                ))
                .clicked()
            {
                st.zoom = 1.0;
            }
            let mut z = st.zoom;
            if ui.add(egui::Slider::new(&mut z, 1.0..=16.0).show_value(false)).changed() {
                st.zoom = z;
            }
        });

        let size = egui::vec2((ui.available_width() - 16.0).max(120.0) / 2.0, 320.0);
        let mp = |d: (u32, u32)| d.0 as f64 * d.1 as f64 / 1_000_000.0;
        let size_a = st.a_dim;
        let size_b = st.b_dim;
        let zoom = st.zoom;
        let mut center = st.center;
        ui.horizontal(|ui| {
            ui.vertical(|ui| {
                ui.label(RichText::new(format!(
                    "A · {}×{} ({:.1} MP) · {}",
                    size_a.0,
                    size_a.1,
                    mp(size_a),
                    fmt_size(st.a_bytes)
                ))
                .strong());
                paint_crop(ui, ta, size_a, &mut center, zoom, size);
            });
            ui.add_space(8.0);
            ui.vertical(|ui| {
                ui.label(RichText::new(format!(
                    "B · {}×{} ({:.1} MP) · {}",
                    size_b.0,
                    size_b.1,
                    mp(size_b),
                    fmt_size(st.b_bytes)
                ))
                .strong());
                paint_crop(ui, tb, size_b, &mut center, zoom, size);
            });
        });
        st.center = center;
        ui.label(RichText::new(format!(
            "Misma zona de las dos: {:.1}% del ancho · mové el mouse sobre cualquiera para recorrerla · rueda para zoom",
            100.0 / zoom
        )).weak().size(10.0));
        if size_a != size_b {
            let (hi, lo) = if size_a.0 * size_a.1 >= size_b.0 * size_b.1 {
                ("A", "B")
            } else {
                ("B", "A")
            };
            ui.label(RichText::new(format!(
                "{hi} tiene más resolución: a este zoom {lo} se ve más blanda (esa es la diferencia real)."
            )).weak());
        }
    }

    fn set_report(&mut self, title: &str, r: Result<serde_json::Value, String>) {
        let v = match r {
            Ok(v) => v,
            Err(e) => serde_json::json!({ "error": e }),
        };
        self.report = Some((title.to_string(), v));
    }

    /// Small cached thumbnail for the similar-images panel.
    fn thumb(&mut self, ctx: &egui::Context, path: &str) -> Option<egui::TextureHandle> {
        if let Some(t) = self.thumbs.get(path) {
            return t.clone();
        }
        let img = image::ImageReader::open(path)
            .ok()
            .and_then(|r| r.with_guessed_format().ok())
            .and_then(|r| r.decode().ok())
            .map(|im| im.thumbnail(96, 96));
        let tex = img.map(|im| {
            let rgba = im.to_rgba8();
            let size = [rgba.width() as usize, rgba.height() as usize];
            let color = egui::ColorImage::from_rgba_unmultiplied(size, rgba.as_raw());
            ctx.load_texture(format!("thumb:{path}"), color, egui::TextureOptions::LINEAR)
        });
        self.thumbs.insert(path.to_string(), tex.clone());
        tex
    }

    fn report_body(&mut self, ui: &mut egui::Ui, v: &serde_json::Value) {
        if let Some(err) = v.get("error").and_then(|x| x.as_str()) {
            ui.colored_label(Color32::from_rgb(200, 80, 80), err);
            return;
        }
        if v.get("status").is_some() {
            self.github_report_ui(ui, v);
            return;
        }
        match v.get("kind").and_then(|x| x.as_str()) {
            Some("images") | Some("audio") => self.similar_report_ui(ui, v),
            Some("why") => self.why_report_ui(ui, v),
            Some("growth") => self.growth_report_ui(ui, v),
            Some("advice") => self.advice_report_ui(ui, v),
            Some("backup") => self.backup_report_ui(ui, v),
            Some("provenance") => self.provenance_report_ui(ui, v),
            Some("speed") => self.speed_report_ui(ui, v),
            Some("addons") => self.addons_report_ui(ui, v),
            Some("content") => self.content_report_ui(ui, v),
            Some("content_estimate") => self.content_estimate_report_ui(ui, v),
            Some("reclaim_plan") => self.reclaim_report_ui(ui, v),
            Some("age_heatmap") => self.age_report_ui(ui, v),
            Some("wsl_docker") => self.wsl_report_ui(ui, v),
            Some("forecast") => self.forecast_report_ui(ui, v),
            Some("github_clones") => self.github_clones_report_ui(ui, v),
            Some("duplicate_folders") => self.duplicate_folders_report_ui(ui, v),
            Some("machine_compare") => self.machine_compare_report_ui(ui, v),
            _ => {
                ui.add(
                    egui::Label::new(
                        RichText::new(serde_json::to_string_pretty(v).unwrap_or_default()).monospace(),
                    )
                    .wrap(),
                );
            }
        }
    }

    fn addons_report_ui(&mut self, ui: &mut egui::Ui, v: &serde_json::Value) {
        ui.heading(tr("Addons externos", "External addons"));
        ui.label(RichText::new(tr(
            "Herramientas que ya resuelven Google Photos u OCR. WizEverything sólo las detecta y muestra cómo usarlas; no maneja credenciales.",
            "Tools that already solve Google Photos or OCR. WizEverything only detects them and shows how to use them; it handles no credentials.",
        )).weak());
        ui.separator();
        if let Some(list) = v["addons"].as_array() {
            for a in list {
                let ok = a["installed"].as_bool().unwrap_or(false);
                ui.horizontal(|ui| {
                    ui.colored_label(
                        if ok { Color32::from_rgb(90, 170, 100) } else { Color32::from_rgb(200, 120, 90) },
                        if ok { "●" } else { "○" },
                    );
                    ui.label(RichText::new(a["name"].as_str().unwrap_or("")).strong());
                    if ok {
                        ui.label(RichText::new(a["version"].as_str().unwrap_or("")).weak().size(10.0));
                    } else {
                        ui.label(RichText::new(tr("no instalado", "not installed")).weak().size(10.0));
                    }
                });
                ui.label(RichText::new(a["purpose"].as_str().unwrap_or("")).size(11.0));
                if let Some(setup) = a["setup"].as_array() {
                    for (i, s) in setup.iter().enumerate() {
                        if let Some(s) = s.as_str() {
                            ui.label(RichText::new(format!("{}. {s}", i + 1)).weak().size(10.0));
                        }
                    }
                }
                if let Some(cmds) = a["commands"].as_array() {
                    for c in cmds {
                        if let Some(c) = c.as_str() {
                            ui.label(RichText::new(c).monospace().size(10.0));
                        }
                    }
                }
                ui.add_space(6.0);
            }
        }
        if let Some(dirs) = v["local_photo_dirs"].as_array().filter(|d| !d.is_empty()) {
            ui.separator();
            ui.label(RichText::new(tr("Carpetas locales detectadas", "Detected local folders")).strong());
            for d in dirs {
                ui.label(RichText::new(d["path"].as_str().unwrap_or("")).monospace().size(10.0));
            }
        }
        ui.separator();
        if let Some(notes) = v["notes"].as_array() {
            for n in notes {
                if let Some(n) = n.as_str() {
                    ui.label(RichText::new(format!("• {n}")).weak().size(10.0));
                }
            }
        }
    }

    fn speed_report_ui(&mut self, ui: &mut egui::Ui, v: &serde_json::Value) {
        ui.heading(tr("Velocidad y tiempo estimado", "Speed and estimated time"));
        ui.label(RichText::new(v["path"].as_str().unwrap_or("")).monospace().size(10.0));
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            ui.label(RichText::new(format!(
                "{}: {}",
                tr("Disco", "Disk"),
                v["disk_class_label"].as_str().unwrap_or("?")
            )).strong());
            if let Some(g) = v["google_drive"].as_str() {
                ui.colored_label(
                    Color32::from_rgb(90, 160, 220),
                    format!("Google Drive · {g}"),
                );
            }
        });
        egui::Grid::new("speed_grid").num_columns(2).spacing([12.0, 4.0]).show(ui, |ui| {
            let row = |ui: &mut egui::Ui, k: &str, val: Option<&serde_json::Value>| {
                if let Some(b) = val {
                    ui.label(k);
                    ui.label(format!("{} MB/s  ({} MB, {} ms)", b["mbps"], b["bytes"].as_u64().unwrap_or(0) / 1_000_000, b["ms"]));
                    ui.end_row();
                }
            };
            row(ui, tr("Descarga", "Download"), v.get("download"));
            row(ui, tr("Lectura", "Read"), v.get("read"));
            row(ui, tr("Escritura", "Write"), v.get("write"));
        });
        if let Some(c) = v.get("cloud").filter(|c| !c.is_null()) {
            ui.add_space(4.0);
            ui.label(format!(
                "{} {:.1}%  ·  {} / {}",
                tr("Ya local:", "Already local:"),
                c["local_pct"].as_f64().unwrap_or(0.0),
                fmt_size(c["local_bytes"].as_u64().unwrap_or(0)),
                fmt_size(c["bytes"].as_u64().unwrap_or(0)),
            ));
            if c["partial"].as_bool().unwrap_or(false) {
                ui.colored_label(Color32::from_rgb(220, 160, 60), tr("muestra parcial", "partial sample"));
            }
        }
        ui.add_space(6.0);
        if let Some(e) = v.get("eta") {
            ui.label(RichText::new(format!(
                "{} {}  ({})",
                tr("ETA:", "ETA:"),
                e["human"].as_str().unwrap_or("?"),
                tr("velocidad efectiva", "effective speed")
            )).strong());
            ui.label(RichText::new(format!(
                "{} {} MB/s · {} MB/s → {} MB/s",
                tr("efectiva", "effective"),
                e["effective_mbps"], e["source_mbps"], e["target_mbps"]
            )).weak().size(10.0));
        }
        if let Some(notes) = v["notes"].as_array().filter(|a| !a.is_empty()) {
            ui.add_space(4.0);
            for n in notes {
                if let Some(n) = n.as_str() {
                    ui.label(RichText::new(format!("• {n}")).weak().size(10.0));
                }
            }
        }
    }

    fn provenance_report_ui(&mut self, ui: &mut egui::Ui, v: &serde_json::Value) {
        ui.heading(tr("Procedencia", "Provenance"));
        ui.label(RichText::new(v["path"].as_str().unwrap_or("")).monospace().size(11.0));
        ui.add_space(6.0);
        if let Some(z) = v.get("zone").filter(|z| !z.is_null()) {
            let id = z["zone_id"].as_u64().unwrap_or(0);
            let (txt, color) = if id == 3 {
                (tr("Descargado de Internet", "Downloaded from the Internet"), Color32::from_rgb(220, 150, 60))
            } else {
                (tr("Zona de Internet", "Internet zone"), Color32::from_rgb(140, 160, 190))
            };
            ui.colored_label(color, txt);
            if let Some(u) = z["host_url"].as_str().filter(|u| !u.is_empty()) {
                ui.label(RichText::new(u).monospace().size(10.0));
            }
            if let Some(r) = z["referrer_url"].as_str().filter(|u| !u.is_empty()) {
                ui.label(RichText::new(format!("{} {r}", tr("Referencia:", "Referrer:"))).weak().size(10.0));
            }
            if let Some(p) = z["package"].as_str().filter(|u| !u.is_empty()) {
                ui.label(RichText::new(format!("{} {p}", tr("Paquete:", "Package:"))).monospace().size(10.0));
            }
        } else {
            ui.label(RichText::new(tr(
                "Sin Zone.Identifier: no vino de Internet o la marca se perdió al copiarlo.",
                "No Zone.Identifier: it did not come from the Internet or the mark was lost when copied.",
            )).weak());
        }
        ui.separator();
        if let Some(i) = v.get("install").filter(|i| !i.is_null()) {
            ui.label(RichText::new(format!(
                "{} {}",
                tr("Instalado por", "Installed by"),
                i["name"].as_str().unwrap_or("")
            )).strong());
            let mut line = String::new();
            if let Some(x) = i["version"].as_str().filter(|x| !x.is_empty()) { line.push_str(&format!("v{x}  ")); }
            if let Some(x) = i["publisher"].as_str().filter(|x| !x.is_empty()) { line.push_str(x); }
            if !line.is_empty() { ui.label(RichText::new(line).weak()); }
            ui.label(RichText::new(i["location"].as_str().unwrap_or("")).monospace().size(10.0));
        } else {
            ui.label(RichText::new(tr("No pertenece a una aplicación instalada conocida.", "Not inside a known installed application.")).weak());
        }
        if let Some(s) = v.get("steam_library").and_then(|x| x.as_str()) {
            ui.label(RichText::new(format!("{} {s}", tr("Biblioteca de Steam:", "Steam library:"))).monospace().size(10.0));
        }
        if let Some(n) = v.get("run_count").and_then(|x| x.as_u64()) {
            ui.label(format!("{} {}", tr("Veces abierto:", "Times opened:"), n));
        }
        if let Some(src) = v.get("sources").and_then(|x| x.as_array()).filter(|a| !a.is_empty()) {
            ui.add_space(4.0);
            ui.label(RichText::new(tr("Origen", "Source")).strong());
            for s in src {
                if let Some(s) = s.as_str() { ui.label(format!("• {s}")); }
            }
        }
    }

    fn similar_report_ui(&mut self, ui: &mut egui::Ui, v: &serde_json::Value) {
        let kind = v["kind"].as_str().unwrap_or("");
        let reclaim = v["reclaim_bytes"].as_u64().unwrap_or(0);
        ui.horizontal(|ui| {
            ui.heading(if kind == "images" { "Imágenes parecidas" } else { "Audio parecido" });
            ui.label(format!(
                "{} archivos · {} grupos · {} recuperables",
                v["analyzed"],
                v["groups"],
                fmt_size(reclaim)
            ));
        });
        ui.label(RichText::new(format!(
            "modo {} · {} ms{}",
            mode_es(v),
            v["elapsed_ms"].as_u64().unwrap_or(0),
            if v["partial"].as_bool().unwrap_or(false) { " · parcial" } else { "" }
        )).weak().size(10.0));
        if let Some(notes) = v["notes"].as_array() {
            for n in notes {
                ui.label(RichText::new(n.as_str().unwrap_or("")).weak());
            }
        }
        ui.separator();
        let empty: Vec<serde_json::Value> = Vec::new();
        for g in v["top"].as_array().unwrap_or(&empty) {
            egui::Frame::group(ui.style()).show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.strong(g["key"].as_str().unwrap_or(""));
                    let r = g["reclaim_bytes"].as_u64().unwrap_or(0);
                    if r > 0 {
                        ui.label(format!("· {} recuperables", fmt_size(r)));
                    }
                });
                if let Some(note) = g["note"].as_str() {
                    ui.colored_label(Color32::from_rgb(210, 150, 60), note);
                }
                self.similar_item_ui(ui, kind, &g["keep"], true);
                if let Some(others) = g["others"].as_array() {
                    for o in others {
                        self.similar_item_ui(ui, kind, o, false);
                    }
                }
                let kp = g["keep"]["path"].as_str();
                let op = g["others"]
                    .as_array()
                    .and_then(|a| a.first())
                    .and_then(|x| x["path"].as_str());
                if let (Some(kp), Some(op)) = (kp, op) {
                    if kind == "audio" {
                        if ui
                            .button(tr("▶ Escuchar A/B", "▶ A/B listen"))
                            .on_hover_text(tr(
                                "Reproduce las dos copias en la misma posición y alterna cuál se oye (se puede fijar A o B). (Experimental)",
                                "Plays both copies at the same position and alternates which one is heard (A or B can be pinned). (Experimental)",
                            ))
                            .clicked()
                        {
                            self.open_audio_ab(kp, op);
                        }
                    } else if kind == "images" {
                        if ui
                            .button(tr("🔍 Comparar zoom (A/B)", "🔍 Zoom compare (A/B)"))
                            .on_hover_text(tr(
                                "Muestra la misma zona de las dos imágenes al mismo zoom; mové el mouse para recorrerlas. (Experimental)",
                                "Shows the same area of both images at the same zoom; move the mouse to inspect. (Experimental)",
                            ))
                            .clicked()
                        {
                            self.open_img_ab(kp, op);
                        }
                    }
                }
            });
            ui.add_space(4.0);
        }
        if let Some(alts) = v["alternatives"].as_array() {
            ui.separator();
            ui.label(RichText::new("Alternativas").strong());
            for a in alts {
                ui.label(format!("· {}", a.as_str().unwrap_or("")));
            }
        }
    }

    fn similar_item_ui(&mut self, ui: &mut egui::Ui, kind: &str, item: &serde_json::Value, keep: bool) {
        ui.horizontal(|ui| {
            if keep
                && kind == "images"
                && let Some(path) = item["path"].as_str()
                && let Some(tex) = self.thumb(ui.ctx(), path)
            {
                ui.add(egui::Image::new(egui::load::SizedTexture::new(
                    tex.id(),
                    egui::vec2(72.0, 72.0),
                )));
            }
            ui.vertical(|ui| {
                let (label, color) = if keep {
                    ("CONSERVAR", Color32::from_rgb(90, 170, 100))
                } else {
                    ("CANDIDATO", Color32::from_rgb(200, 150, 60))
                };
                ui.horizontal(|ui| {
                    ui.colored_label(color, label);
                    ui.label(format!(
                        "{} · score {:.1}",
                        fmt_size(item["bytes"].as_u64().unwrap_or(0)),
                        item["score"].as_f64().unwrap_or(0.0)
                    ));
                    if let Some(sim) = item["similarity_pct"].as_f64() {
                        ui.label(format!("· similitud {sim:.0}%"));
                    }
                });
                ui.label(item["path"].as_str().unwrap_or(""));
                if let Some(rs) = item["reasons"].as_array() {
                    let s: Vec<&str> = rs.iter().filter_map(|x| x.as_str()).collect();
                    ui.label(RichText::new(s.join(" · ")).weak());
                }
                if let Some(ai) = item["ai"].as_array() {
                    let s: Vec<&str> = ai.iter().filter_map(|x| x.as_str()).collect();
                    if !s.is_empty() {
                        ui.colored_label(Color32::from_rgb(200, 120, 220), format!("IA: {}", s.join(", ")));
                    }
                }
                if let Some(n) = item["noise"].as_object() {
                    let summary = n.get("summary").and_then(|x| x.as_str()).unwrap_or("");
                    if !summary.is_empty() {
                        let bad = n.get("artifacts").and_then(|x| x.as_bool()).unwrap_or(false)
                            || n.get("dull").and_then(|x| x.as_bool()).unwrap_or(false);
                        ui.colored_label(
                            if bad {
                                Color32::from_rgb(210, 120, 70)
                            } else {
                                Color32::from_gray(150)
                            },
                            format!("ruido: {summary}"),
                        );
                    }
                }
            });
        });
    }

    fn github_report_ui(&mut self, ui: &mut egui::Ui, v: &serde_json::Value) {
        let status = v["status"].as_str().unwrap_or("");
        let title = v["title_es"].as_str().or_else(|| v["title"].as_str()).unwrap_or("");
        let color = match status {
            "up_to_date" => Color32::from_rgb(90, 170, 100),
            "local_newer" => Color32::from_rgb(90, 150, 220),
            "behind" => Color32::from_rgb(220, 160, 70),
            "diverged" => Color32::from_rgb(210, 90, 90),
            _ => Color32::from_gray(150),
        };
        ui.heading(RichText::new(title).color(color).size(20.0));
        if status == "not_repo" {
            ui.label(v["path"].as_str().unwrap_or(""));
            return;
        }
        ui.horizontal_wrapped(|ui| {
            ui.label(format!("Rama: {}", v["branch"].as_str().unwrap_or("")));
            ui.label(format!("· HEAD {}", v["head"].as_str().unwrap_or("")));
            ui.label(format!("· {}", v["upstream"].as_str().unwrap_or("—")));
            ui.label(format!("· {} adelante / {} atrás", v["ahead"], v["behind"]));
            if v["dirty"].as_u64().unwrap_or(0) > 0 {
                ui.colored_label(
                    Color32::from_rgb(210, 150, 60),
                    format!("· {} sin commitear", v["dirty"]),
                );
            }
        });
        if let Some(notes) = v["notes"].as_array() {
            for n in notes {
                ui.label(RichText::new(n.as_str().unwrap_or("")).weak());
            }
        }
        ui.separator();
        if let Some(s) = v.get("size") {
            ui.horizontal_wrapped(|ui| {
                for (key, label) in [
                    ("local_alloc", "Disco"),
                    ("tracked", "Versionado"),
                    ("git_loose", ".git suelto"),
                    ("git_pack", ".git pack"),
                ] {
                    ui.label(format!("{label}: {}", fmt_size(s[key].as_u64().unwrap_or(0))));
                    ui.label("·");
                }
                if let Some(kb) = s["github_kb"].as_u64() {
                    ui.label(format!("GitHub: {kb} KB"));
                }
            });
            if let Some(e) = s["explanation"].as_str() {
                ui.label(RichText::new(e).weak());
            }
        }
        ui.add_space(6.0);
        if let Some(folders) = v["folders"].as_array()
            && !folders.is_empty()
        {
            ui.label(RichText::new("Tamaño por carpeta").strong());
            egui::Grid::new("gh_folders").striped(true).num_columns(5).show(ui, |ui| {
                for h in ["Carpeta", "Disco", "Versionado", "Delta", "%"] {
                    ui.strong(h);
                }
                ui.end_row();
                for f in folders {
                    ui.label(f["folder"].as_str().unwrap_or(""));
                    ui.label(fmt_size(f["local_bytes"].as_u64().unwrap_or(0)));
                    ui.label(fmt_size(f["tracked_bytes"].as_u64().unwrap_or(0)));
                    let d = f["delta_bytes"].as_i64().unwrap_or(0);
                    ui.label(if d >= 0 {
                        format!("+{}", fmt_size(d as u64))
                    } else {
                        format!("-{}", fmt_size((-d) as u64))
                    });
                    ui.label(format!("{:.1}%", f["percent"].as_f64().unwrap_or(0.0)));
                    ui.end_row();
                }
            });
        }
        for (key, label) in [
            ("ahead", "Commits locales sin subir"),
            ("behind", "Commits de la web que te faltan"),
        ] {
            if let Some(arr) = v["commits"][key].as_array()
                && !arr.is_empty()
            {
                egui::CollapsingHeader::new(format!("{label} ({})", arr.len()))
                    .default_open(true)
                    .show(ui, |ui| {
                        for c in arr {
                            ui.label(format!(
                                "{}  {}  {}",
                                c["hash"].as_str().unwrap_or(""),
                                c["date"].as_str().unwrap_or(""),
                                c["subject"].as_str().unwrap_or("")
                            ));
                        }
                    });
            }
        }
        if let Some(arr) = v["changes"]["upstream_files"].as_array()
            && !arr.is_empty()
        {
            egui::CollapsingHeader::new(format!("Archivos que cambiaron arriba ({})", arr.len())).show(
                ui,
                |ui| {
                    for f in arr.iter().take(100) {
                        ui.label(format!(
                            "{}  {}",
                            f["status"].as_str().unwrap_or(""),
                            f["path"].as_str().unwrap_or("")
                        ));
                    }
                },
            );
        }
        if let Some(arr) = v["dirty_files"].as_array()
            && !arr.is_empty()
        {
            egui::CollapsingHeader::new(format!("Cambios locales sin commitear ({})", v["dirty"]))
                .show(ui, |ui| {
                    for f in arr {
                        ui.label(f.as_str().unwrap_or(""));
                    }
                });
        }
        if let Some(alts) = v["alternatives"].as_array() {
            ui.separator();
            ui.label(RichText::new("Alternativas").strong());
            for a in alts {
                ui.label(format!("· {}", a.as_str().unwrap_or("")));
            }
        }
    }

    /// Bytes of an extension/category per node, cached per code + revision.
    fn ext_bytes_for(&mut self, tree: &Tree, code: u32) -> Arc<treemap::Highlight> {
        match &self.highlight_cache {
            Some((ce, rev, h)) if *ce == code && *rev == self.revision => h.clone(),
            _ => {
                let bytes = if code & CAT_FLAG != 0 {
                    let c = (code & !CAT_FLAG) as usize;
                    tree.ext_bytes_where(|x| category_of(&tree.exts[x as usize].name) == c)
                } else {
                    tree.ext_bytes(code)
                };
                let h = Arc::new(treemap::Highlight { bytes });
                self.highlight_cache = Some((code, self.revision, h.clone()));
                h
            }
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
            let hl_code = self.highlight_ext;
            let hl = hl_code.map(|e| self.ext_bytes_for(tree, e));
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

        // Rings + extension highlight: a small arrow sends the filter to the left panel.
        if self.settings.rings
            && let Some(g) = layout.geom
            && let Some(code) = self.highlight_ext
        {
            let rmax = (layout.width.min(layout.height) as f32 / 2.0 - 6.0).max(10.0);
            let cx = rect.min.x + g[0] * kx;
            let rx = rmax * kx;
            let pos = egui::pos2((cx + rx + 16.0).max(rect.min.x + 8.0), rect.min.y + 20.0);
            let resp = ui.put(
                egui::Rect::from_min_size(pos, egui::vec2(126.0, 22.0)),
                egui::Button::new(tr("↙ Explorador", "↙ Explorer")).small(),
            );
            if resp
                .on_hover_text(tr(
                    "Llevar este filtro al panel izquierdo",
                    "Send this filter to the left panel",
                ))
                .clicked()
            {
                self.type_filter = Some(code);
                self.tab = Tab::Files;
            }
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
    Advice(String, bool),
    Provenance(String, bool),
    Quarantine(String),
}

/// The mode a report was produced with, in UI language.
fn mode_es(v: &serde_json::Value) -> &'static str {
    if v.get("mode").and_then(|m| m.as_str()) == Some("deep") {
        tr("profundo", "deep")
    } else {
        tr("rápido", "fast")
    }
}

/// Human duration for estimates and measurements.
fn fmt_ms(ms: u64) -> String {
    if ms < 1_000 {
        format!("{ms} ms")
    } else if ms < 60_000 {
        format!("{} s", fmt_dec(ms as f64 / 1000.0))
    } else {
        let m = ms / 60_000;
        format!("{m} min {} s", fmt_dec((ms % 60_000) as f64 / 1000.0))
    }
}

/// Deep hover: generic warning plus the estimate learned from the last fast run.
fn deep_hover(label: &str, eta: Option<(u64, u64)>) -> String {
    let base = format!(
        "{label}: {}",
        tr(
            "sin presupuesto, analiza todo; puede tardar más.",
            "no budget, analyses everything; may take longer.",
        )
    );
    match eta {
        Some((fast, deep)) => format!(
            "{base}\n{} {} → {} ≈ {}.",
            tr("Última corrida rápida:", "Last fast run:"),
            fmt_ms(fast),
            tr("profundo", "deep"),
            fmt_ms(deep)
        ),
        None => base,
    }
}

/// Estimate deep time from the fast result: remaining work × the known deep/fast factor.
pub(crate) fn estimate_deep(kind: &str, fast_ms: u64, v: &serde_json::Value) -> u64 {
    let u = |k: &str| v.get(k).and_then(|x| x.as_u64()).unwrap_or(0) as f64;
    let partial = v.get("partial").and_then(|x| x.as_bool()).unwrap_or(false);
    let factor = match kind {
        // Deep only raises the budget: it re-decodes the images the fast run could not finish.
        "images" => {
            if partial && u("analyzed") > 0.0 {
                (u("files") / u("analyzed")).clamp(1.0, 24.0)
            } else {
                1.0
            }
        }
        // Deep decodes a 120 s window instead of 20 s: ~6× the decode work.
        "audio" => {
            let remaining = if partial && u("analyzed") > 0.0 {
                ((u("files") - u("skipped")) / u("analyzed")).clamp(1.0, 20.0)
            } else {
                1.0
            };
            remaining * 6.0
        }
        // Deep resolves every change instead of the 2,000 largest.
        "growth" => {
            let total = ["added", "removed", "grown", "shrunk"]
                .iter()
                .map(|k| u(k))
                .sum::<f64>();
            let pct = v
                .get("attributed_pct")
                .and_then(|x| x.as_f64())
                .unwrap_or(100.0);
            let resolved = total * pct / 100.0;
            if resolved > 0.0 && total > resolved {
                (total / resolved).clamp(1.0, 10.0)
            } else {
                1.0
            }
        }
        // Deep compares 250,000 files instead of 50,000.
        "backup" => {
            if v.get("truncated").and_then(|x| x.as_bool()).unwrap_or(false) {
                5.0
            } else {
                1.0
            }
        }
        // Deep adds the git check.
        "advice" => 1.5,
        _ => 1.0,
    };
    let factor = if factor.is_finite() && factor >= 1.0 { factor } else { 1.0 };
    (fast_ms.max(1) as f64 * factor).min(1_800_000.0) as u64
}

/// Two explicit answers for heavy analyses: fast (default budget) and deep.
fn analysis_pair(
    ui: &mut egui::Ui,
    label: &str,
    hover: &str,
    eta: Option<(u64, u64)>,
) -> Option<bool> {
    let mut out = None;
    ui.horizontal_wrapped(|ui| {
        if ui
            .button(format!("{label} · {}", tr("rápido", "fast")))
            .on_hover_text(hover)
            .clicked()
        {
            out = Some(false);
        }
        if ui
            .button(tr("profundo", "deep"))
            .on_hover_text(deep_hover(label, eta))
            .clicked()
        {
            out = Some(true);
        }
    });
    out
}

/// Right-click menu body. Returns an action for the app, and whether to close.
/// excluded: the folder's state in the duplicate exclusions (None = not excluded,
/// Some(true) = excluded itself, Some(false) = inside an excluded folder).
fn context_menu(
    ui: &mut egui::Ui,
    tree: Option<&Tree>,
    target: &MenuTarget,
    excluded: Option<bool>,
    advice_eta: Option<(u64, u64)>,
) -> (Option<MenuAction>, bool) {
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
    if ui
        .button(tr("¿Puedo borrar esto? (rápido)", "Can I delete this? (fast)"))
        .on_hover_text(tr(
            "Segunda opinión sin consultar git: artefacto regenerable, nube, carpeta del sistema, dato personal, antigüedad.",
            "Second opinion without asking git: regenerable artifact, cloud, system folder, personal data, age.",
        ))
        .clicked()
    {
        action = Some(MenuAction::Advice(path.clone(), false));
        close = true;
    }
    if ui
        .button(tr("Procedencia (rápido)", "Provenance (fast)"))
        .on_hover_text(tr(
            "Zona de Internet, URL de descarga, aplicación que lo instaló y biblioteca de Steam.",
            "Internet zone, download URL, the app that installed it and Steam library.",
        ))
        .clicked()
    {
        action = Some(MenuAction::Provenance(path.clone(), false));
        close = true;
    }
    if ui
        .button(tr("Procedencia (profundo)", "Provenance (deep)"))
        .on_hover_text(tr(
            "Igual que el rápido y además consulta a Everything cuántas veces se abrió.",
            "Same as fast and also asks Everything how many times it was opened.",
        ))
        .clicked()
    {
        action = Some(MenuAction::Provenance(path.clone(), true));
        close = true;
    }
    if ui
        .button(tr("¿Puedo borrar esto? (profundo)", "Can I delete this? (deep)"))
        .on_hover_text(deep_hover(
            tr("¿Puedo borrar esto?", "Can I delete this?"),
            advice_eta,
        ))
        .clicked()
    {
        action = Some(MenuAction::Advice(path.clone(), true));
        close = true;
    }
    if ui
        .button(tr("Mover a cuarentena", "Move to quarantine"))
        .on_hover_text(tr(
            "Lo agrega a la canasta con dry-run; nada se mueve hasta confirmar.",
            "Adds it to the basket with dry-run; nothing moves until you confirm.",
        ))
        .clicked()
    {
        action = Some(MenuAction::Quarantine(path.clone()));
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
fn open_in_explorer(path: &str) {
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

fn folder_icon(ui: &mut egui::Ui) {
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

fn file_icon(ui: &mut egui::Ui, c: [u8; 3]) {
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

fn load_full(ctx: &egui::Context, path: &str) -> Option<(egui::TextureHandle, (u32, u32))> {
    let im = image::ImageReader::open(path)
        .ok()?
        .with_guessed_format()
        .ok()?
        .decode()
        .ok()?;
    // Cap GPU memory while keeping enough detail for 8x zoom.
    let im = im.thumbnail(4096, 4096);
    let rgba = im.to_rgba8();
    let dims = (rgba.width(), rgba.height());
    let color = egui::ColorImage::from_rgba_unmultiplied(
        [dims.0 as usize, dims.1 as usize],
        rgba.as_raw(),
    );
    Some((
        ctx.load_texture(format!("full:{path}"), color, egui::TextureOptions::LINEAR),
        dims,
    ))
}

/// Paint the same normalised region of one image; moving the mouse pans it.
fn paint_crop(
    ui: &mut egui::Ui,
    tex: &egui::TextureHandle,
    dim: (u32, u32),
    center: &mut (f32, f32),
    zoom: f32,
    size: egui::Vec2,
) -> egui::Response {
    let resp = ui.allocate_response(size, egui::Sense::hover());
    if dim.0 == 0 || dim.1 == 0 {
        return resp;
    }
    let fx = 1.0 / zoom;
    // Keep pixels square: the crop's normalised height follows the widget aspect.
    let fy = fx * (size.y / size.x) * (dim.0 as f32 / dim.1.max(1) as f32);
    if resp.hovered()
        && let Some(p) = ui.ctx().pointer_hover_pos()
    {
        let (x0, y0, w, h) = crate::img_ab::crop_frac(dim, *center, (fx, fy));
        let local = p - resp.rect.min;
        center.0 = ((x0 + local.x / size.x * w) / dim.0 as f32).clamp(0.0, 1.0);
        center.1 = ((y0 + local.y / size.y * h) / dim.1 as f32).clamp(0.0, 1.0);
    }
    let (x0, y0, w, h) = crate::img_ab::crop_frac(dim, *center, (fx, fy));
    let uv = egui::Rect::from_min_max(
        egui::pos2(x0 / dim.0 as f32, y0 / dim.1 as f32),
        egui::pos2((x0 + w) / dim.0 as f32, (y0 + h) / dim.1 as f32),
    );
    ui.painter()
        .image(tex.id(), resp.rect, uv, egui::Color32::WHITE);
    resp
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
