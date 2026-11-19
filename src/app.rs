use std::collections::HashSet;
use std::sync::atomic::Ordering;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::Arc;
use std::time::{Duration, Instant};

use eframe::egui::{self, Align, Color32, Layout as ELayout, RichText, Sense, Stroke};
use egui_extras::{Column, TableBuilder};

use crate::tree::{self, Progress, ROOT, Tree};
use crate::treemap::{self, Layout};
use crate::i18n::{self, Lang, Settings, Theme, tr};
use crate::{autostart, everything};

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
}

/// Columnas ordenables.
#[derive(PartialEq, Eq, Clone, Copy, Hash, Debug)]
enum Col {
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

/// `None` = orden por defecto (tamaño descendente, como WizTree).
type Sort = Option<(Col, bool)>;

fn natural_asc(c: Col) -> bool {
    matches!(c, Col::Name | Col::Path)
}

/// Encabezado clicable: 1er clic ordena, 2do invierte, 3ro quita el orden.
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
    let key = |a: &tree::Node, b: &tree::Node| -> Ordering {
        match col {
            Col::Name | Col::Path => cmp_ci(&a.name, &b.name),
            Col::Pct | Col::Size | Col::Count => a.size.cmp(&b.size),
            Col::Alloc => a.alloc.cmp(&b.alloc),
            Col::Items => (a.files + a.folders).cmp(&(b.files + b.folders)),
            Col::Files => a.files.cmp(&b.files),
            Col::Folders => a.folders.cmp(&b.folders),
            Col::Modified => a.modified.cmp(&b.modified),
        }
    };
    ids.sort_by(|&x, &y| {
        let (a, b) = (&tree.nodes[x as usize], &tree.nodes[y as usize]);
        let o = key(a, b);
        let o = if asc { o } else { o.reverse() };
        o.then_with(|| b.size.cmp(&a.size))
    });
}

struct Loading {
    rx: Receiver<Result<Tree, String>>,
    progress: Arc<Progress>,
    started: Instant,
}

pub struct App {
    tree: Option<Arc<Tree>>,
    loading: Option<Loading>,
    error: Option<String>,

    view_root: u32,
    selected: Option<u32>,
    expanded: HashSet<u32>,
    rows: Vec<(u32, u16)>,
    rows_dirty: bool,
    scroll_to_selected: bool,

    layout: Option<Layout>,
    texture: Option<egui::TextureHandle>,
    layout_key: (u32, usize, usize, Option<u32>, usize),
    pending_size: Option<((usize, usize), Instant)>,
    hovered: Option<u32>,

    ext_rows: Vec<(u32, u64, u64)>,
    ext_key: (u32, usize, Sort),
    highlight_ext: Option<u32>,

    tab: Tab,
    file_filter: String,
    files: Vec<u32>,
    files_key: (u32, usize, String, Sort),

    tree_sort: Sort,
    files_sort: Sort,
    ext_sort: Sort,

    settings: Settings,

    autostart: bool,
    tray_rx: Receiver<TrayCmd>,
    has_tray: bool,
    visible: bool,
    pub tray_items: Option<TrayItems>,
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

impl App {
    pub fn new(ctx: &egui::Context, tray_rx: Receiver<TrayCmd>, has_tray: bool, start_hidden: bool, settings: Settings) -> Self {
        setup_fonts(ctx);
        i18n::set_lang(settings.lang);
        apply_theme(ctx, settings.theme);
        let mut style = (*ctx.global_style()).clone();
        style.spacing.item_spacing = egui::vec2(6.0, 3.0);
        ctx.set_global_style(style);
        let mut app = Self {
            tree: None,
            loading: None,
            error: None,
            view_root: ROOT,
            selected: None,
            expanded: HashSet::new(),
            rows: Vec::new(),
            rows_dirty: true,
            scroll_to_selected: false,
            layout: None,
            texture: None,
            layout_key: (u32::MAX, 0, 0, None, 0),
            pending_size: None,
            hovered: None,
            ext_rows: Vec::new(),
            ext_key: (u32::MAX, 0, None),
            highlight_ext: None,
            tab: Tab::Tree,
            file_filter: String::new(),
            files: Vec::new(),
            files_key: (u32::MAX, 0, String::new(), None),
            tree_sort: None,
            files_sort: None,
            ext_sort: None,
            settings,
            autostart: autostart::is_enabled(),
            tray_rx,
            has_tray,
            visible: !start_hidden,
            tray_items: None,
        };
        app.start_load(ctx);
        app
    }

    fn start_load(&mut self, ctx: &egui::Context) {
        if self.loading.is_some() {
            return;
        }
        let (tx, rx): (Sender<Result<Tree, String>>, _) = channel();
        let progress = Arc::new(Progress::default());
        let p = progress.clone();
        let ctx = ctx.clone();
        std::thread::Builder::new()
            .name("everything-load".into())
            .spawn(move || {
                // Al iniciar Windows, Everything puede tardar en estar listo.
                loop {
                    match everything::status() {
                        everything::Status::Ready => break,
                        _ => {
                            p.waiting.store(true, Ordering::Relaxed);
                            ctx.request_repaint();
                            std::thread::sleep(Duration::from_secs(1));
                        }
                    }
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
                let r = tree::load(p.clone());
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
            Err(e) => self.error = Some(e),
            Ok(new) => {
                self.error = None;
                let new = Arc::new(new);
                // Conservar vista, selección y carpetas expandidas por ruta.
                if let Some(old) = &self.tree {
                    let root_path = old.full_path(self.view_root);
                    let sel_path = self.selected.map(|s| old.full_path(s));
                    let exp: Vec<String> = self.expanded.iter().map(|&e| old.full_path(e)).collect();
                    self.view_root = find_path(&new, &root_path).unwrap_or(ROOT);
                    self.selected = sel_path.and_then(|p| find_path(&new, &p));
                    self.expanded = exp.iter().filter_map(|p| find_path(&new, p)).collect();
                } else {
                    self.view_root = default_drive(&new);
                    self.expanded.insert(self.view_root);
                }
                self.tree = Some(new);
                self.rows_dirty = true;
                self.layout_key.0 = u32::MAX;
            }
        }
    }

    fn set_root(&mut self, id: u32) {
        self.view_root = id;
        self.expanded.insert(id);
        self.rows_dirty = true;
        self.highlight_ext = None;
    }

    fn select(&mut self, tree: &Tree, id: u32, reveal: bool) {
        self.selected = Some(id);
        if reveal {
            let anc = tree.ancestors(id);
            if !anc.contains(&self.view_root) {
                // fuera de la vista: subir hasta la unidad
                self.set_root(*anc.get(1).unwrap_or(&ROOT));
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
        self.rows.clear();
        let mut stack = vec![(self.view_root, 0u16)];
        while let Some((id, d)) = stack.pop() {
            self.rows.push((id, d));
            if self.expanded.contains(&id) {
                let ch = &tree.nodes[id as usize].children;
                if let Some((col, asc)) = self.tree_sort {
                    let mut v = ch.clone();
                    sort_nodes(tree, &mut v, col, asc);
                    stack.extend(v.into_iter().rev().map(|c| (c, d + 1)));
                } else {
                    stack.extend(ch.iter().rev().map(|&c| (c, d + 1)));
                }
            }
        }
        self.rows_dirty = false;
    }

    fn handle_tray(&mut self, ctx: &egui::Context) {
        while let Ok(cmd) = self.tray_rx.try_recv() {
            match cmd {
                TrayCmd::Show => self.show_window(ctx),
                TrayCmd::Refresh => self.start_load(ctx),
                TrayCmd::ToggleAutostart => self.toggle_autostart(),
                TrayCmd::Quit => std::process::exit(0),
            }
        }
    }

    fn show_window(&mut self, ctx: &egui::Context) {
        ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
        ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
        ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
        self.visible = true;
        // Everything se actualiza en vivo: si el árbol tiene más de 1 minuto, refrescar.
        if self.tree.as_ref().is_some_and(|t| t.built_at.elapsed() > Duration::from_secs(60)) {
            self.start_load(ctx);
        }
    }

    fn toggle_autostart(&mut self) {
        let want = !self.autostart;
        match autostart::set(want) {
            Ok(()) => self.autostart = want,
            Err(e) => self.error = Some(e),
        }
        if let Some(items) = &self.tray_items {
            items.auto.set_checked(self.autostart);
        }
    }
}

impl eframe::App for App {
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.handle_tray(ctx);
        self.poll_load();
        if ctx.input(|i| i.viewport().close_requested()) && self.has_tray {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
            self.visible = false;
            // liberar la textura grande mientras está oculto
            self.texture = None;
            self.layout = None;
            self.layout_key.0 = u32::MAX;
        }
        if ctx.input(|i| i.key_pressed(egui::Key::F5)) {
            self.start_load(ctx);
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        let tree = self.tree.clone();

        egui::Panel::top("toolbar").show(ui, |ui| self.toolbar(ui, &ctx, tree.as_deref()));
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

        let h = ui.available_height();
        if !self.settings.show_tree {
            // Sin árbol: el treemap ocupa el centro (y las extensiones, si están, a la derecha).
            if self.settings.show_exts {
                egui::Panel::right("exts")
                    .resizable(true)
                    .default_size(360.0)
                    .show(ui, |ui| self.ext_ui(ui, &tree));
            }
            egui::CentralPanel::no_frame().show(ui, |ui| self.treemap_ui(ui, &tree));
            return;
        }
        egui::Panel::bottom("treemap")
            .resizable(true)
            .default_size(h * 0.55)
            .min_size(80.0)
            .frame(egui::Frame::NONE)
            .show(ui, |ui| self.treemap_ui(ui, &tree));
        if self.settings.show_exts {
            egui::Panel::right("exts")
                .resizable(true)
                .default_size(360.0)
                .show(ui, |ui| self.ext_ui(ui, &tree));
        }
        egui::CentralPanel::default_margins().show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.selectable_value(&mut self.tab, Tab::Tree, tr("Vista de árbol", "Tree view"));
                ui.selectable_value(&mut self.tab, Tab::Files, tr("Vista de archivos", "File view"));
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
                Tab::Tree => self.tree_ui(ui, &tree),
                Tab::Files => self.files_ui(ui, &tree),
            }
        });
    }
}

// ---------------------------------------------------------------- paneles

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
                ui.separator();
                ui.label(tr("Unidad:", "Drive:"));
                let drives: Vec<u32> = tree.nodes[ROOT as usize].children.clone();
                let drive = tree.ancestors(self.view_root).get(1).copied().unwrap_or(ROOT);
                let label = |id: u32| {
                    let n = &tree.nodes[id as usize];
                    format!("{}  {}", tree.full_path(id), fmt_size(n.size))
                };
                let mut chosen = None;
                egui::ComboBox::from_id_salt("drive")
                    .selected_text(if drive == ROOT { tr("Todas", "All").to_string() } else { label(drive) })
                    .width(220.0)
                    .show_ui(ui, |ui| {
                        if ui.selectable_label(drive == ROOT, format!("{}  {}", tr("Todas", "All"), fmt_size(tree.nodes[0].size))).clicked() {
                            chosen = Some(ROOT);
                        }
                        for d in drives {
                            if ui.selectable_label(d == drive, label(d)).clicked() {
                                chosen = Some(d);
                            }
                        }
                    });
                if let Some(d) = chosen {
                    self.expanded.clear();
                    self.set_root(d);
                    self.selected = None;
                }
                if self.view_root != drive {
                    ui.separator();
                    if ui.button(tr("⬆ Subir", "⬆ Up")).clicked() {
                        let p = tree.nodes[self.view_root as usize].parent;
                        self.set_root(p);
                    }
                    ui.label(RichText::new(tree.full_path(self.view_root)).strong());
                }
            }
            ui.with_layout(ELayout::right_to_left(Align::Center), |ui| {
                let mut a = self.autostart;
                if ui.checkbox(&mut a, tr("Iniciar con Windows", "Start with Windows")).changed() {
                    self.toggle_autostart();
                }
                ui.separator();

                // Idioma
                let mut lang = self.settings.lang;
                egui::ComboBox::from_id_salt("lang")
                    .selected_text(if lang == Lang::Es { "Español" } else { "English" })
                    .width(80.0)
                    .show_ui(ui, |ui| {
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

                // Tema
                let mut theme = self.settings.theme;
                let theme_label = |t: Theme| match t {
                    Theme::System => tr("Sistema", "System"),
                    Theme::Light => tr("☀ Claro", "☀ Light"),
                    Theme::Dark => tr("☾ Oscuro", "☾ Dark"),
                };
                egui::ComboBox::from_id_salt("theme")
                    .selected_text(theme_label(theme))
                    .width(90.0)
                    .show_ui(ui, |ui| {
                        for t in [Theme::System, Theme::Light, Theme::Dark] {
                            ui.selectable_value(&mut theme, t, theme_label(t));
                        }
                    });
                if theme != self.settings.theme {
                    self.settings.theme = theme;
                    apply_theme(ctx, theme);
                    self.settings.save();
                }
                ui.separator();

                let (t0, e0) = (self.settings.show_tree, self.settings.show_exts);
                ui.toggle_value(&mut self.settings.show_exts, tr("Extensiones", "Extensions"))
                    .on_hover_text(tr("Mostrar u ocultar el panel de tipos de archivo", "Show or hide the file types panel"));
                ui.toggle_value(&mut self.settings.show_tree, tr("Árbol", "Tree"))
                    .on_hover_text(tr("Mostrar u ocultar la vista de árbol / archivos", "Show or hide the tree / file view"));
                ui.label(tr("Paneles:", "Panels:"));
                if (t0, e0) != (self.settings.show_tree, self.settings.show_exts) {
                    self.settings.save();
                }
            });
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
                        "{} {} / {}  ({:.1} s)",
                        tr("Leyendo índice de Everything…", "Reading Everything index…"),
                        fmt_int(d as u64),
                        fmt_int(t as u64),
                        l.started.elapsed().as_secs_f32()
                    ));
                }
                ui.separator();
            }
            if let Some(e) = &self.error {
                ui.colored_label(Color32::LIGHT_RED, e);
                ui.separator();
            }
            if let Some(tree) = tree {
                let drive = tree.ancestors(self.view_root).get(1).copied();
                if let Some(d) = drive
                    && let Some((total, free)) = tree::disk_space(&tree.nodes[d as usize].name)
                {
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
                    "{} {} · {} {} s · {}",
                    fmt_int(tree.nodes.len() as u64 - 1),
                    tr("elementos", "items"),
                    tr("cargado en", "loaded in"),
                    fmt_dec(tree.load_ms as f64 / 1000.0),
                    fmt_age(tree.built_at.elapsed())
                ));
                if let Some(h) = self.hovered {
                    ui.separator();
                    let n = &tree.nodes[h as usize];
                    ui.label(format!("{}  ({})", tree.full_path(h), fmt_size(n.size)));
                }
            }
        });
    }

    fn tree_ui(&mut self, ui: &mut egui::Ui, tree: &Tree) {
        if self.rows_dirty {
            self.rebuild_rows(tree);
        }
        let row_h = 20.0;
        let mut table = TableBuilder::new(ui)
            .striped(true)
            .resizable(true)
            .sense(Sense::click())
            .cell_layout(ELayout::left_to_right(Align::Center))
            .column(Column::initial(340.0).at_least(120.0).clip(true))
            .column(Column::initial(130.0).at_least(60.0))
            .column(Column::initial(85.0))
            .column(Column::initial(85.0))
            .column(Column::initial(80.0))
            .column(Column::initial(80.0))
            .column(Column::initial(70.0))
            .column(Column::remainder().at_least(110.0));
        if self.scroll_to_selected {
            if let Some(i) = self.selected.and_then(|s| self.rows.iter().position(|r| r.0 == s)) {
                table = table.scroll_to_row(i, Some(Align::Center));
            }
            self.scroll_to_selected = false;
        }
        let mut toggle = None;
        let mut select = None;
        let mut zoom = None;
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
                        sort_header(ui, t, c, &mut sort);
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
                        if n.is_dir && !n.children.is_empty() {
                            if toggle_arrow(ui, self.expanded.contains(&id)).clicked() {
                                toggle = Some(id);
                            }
                        } else {
                            ui.add_space(14.0);
                        }
                        if id == ROOT || n.parent == ROOT {
                            drive_icon(ui);
                        } else if n.is_dir {
                            folder_icon(ui);
                        } else {
                            file_icon(ui, tree.exts[n.ext as usize].color);
                        }
                        let name = if id == self.view_root { tree.full_path(id) } else { n.name.to_string() };
                        let name = if id == ROOT { tr("Este equipo", "This PC").to_string() } else { name };
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
                    row.col(|ui| right(ui, if n.is_dir { fmt_int((n.files + n.folders) as u64) } else { String::new() }));
                    row.col(|ui| right(ui, if n.is_dir { fmt_int(n.files as u64) } else { String::new() }));
                    row.col(|ui| right(ui, if n.is_dir { fmt_int(n.folders as u64) } else { String::new() }));
                    row.col(|ui| {
                        ui.label(fmt_date(n.modified));
                    });
                    let resp = row.response();
                    if resp.clicked() {
                        select = Some(id);
                    }
                    if resp.double_clicked() && n.is_dir {
                        toggle = Some(id);
                    }
                    resp.context_menu(|ui| {
                        if let Some(z) = context_menu(ui, tree, id) {
                            zoom = Some(z);
                        }
                    });
                });
            });
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
        }
        if let Some(z) = zoom {
            self.set_root(z);
        }
    }

    fn files_ui(&mut self, ui: &mut egui::Ui, tree: &Tree) {
        ui.horizontal(|ui| {
            ui.label(tr("Filtrar:", "Filter:"));
            ui.text_edit_singleline(&mut self.file_filter);
            ui.label(RichText::new(tr("los 1000 archivos más grandes de la vista", "the 1000 largest files in view")).weak());
        });
        let key = (self.view_root, Arc::as_ptr(self.tree.as_ref().unwrap()) as usize, self.file_filter.clone(), self.files_sort);
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
        let mut zoom = None;
        let mut sort = self.files_sort;
        TableBuilder::new(ui)
            .striped(true)
            .resizable(true)
            .sense(Sense::click())
            .cell_layout(ELayout::left_to_right(Align::Center))
            .column(Column::initial(260.0).clip(true))
            .column(Column::initial(85.0))
            .column(Column::initial(85.0))
            .column(Column::initial(115.0))
            .column(Column::remainder().clip(true))
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
                        ui.label(&*n.name);
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
                    resp.context_menu(|ui| {
                        if let Some(z) = context_menu(ui, tree, id) {
                            zoom = Some(z);
                        }
                    });
                });
            });
        self.files_sort = sort;
        if let Some(s) = select {
            self.selected = Some(s);
        }
        if let Some(z) = zoom {
            self.set_root(z);
        }
    }

    fn ext_ui(&mut self, ui: &mut egui::Ui, tree: &Tree) {
        let key = (self.view_root, Arc::as_ptr(self.tree.as_ref().unwrap()) as usize, self.ext_sort);
        if key != self.ext_key {
            if (key.0, key.1) != (self.ext_key.0, self.ext_key.1) {
                self.ext_rows = tree.ext_stats(self.view_root);
            }
            let (col, asc) = self.ext_sort.unwrap_or((Col::Size, false));
            self.ext_rows.sort_by(|a, b| {
                let o = match col {
                    Col::Name => cmp_ci(tree.ext_label(a.0), tree.ext_label(b.0)),
                    Col::Count => a.2.cmp(&b.2),
                    _ => a.1.cmp(&b.1),
                };
                (if asc { o } else { o.reverse() }).then_with(|| b.1.cmp(&a.1))
            });
            self.ext_key = key;
        }
        let mut sort = self.ext_sort;
        let total = tree.nodes[self.view_root as usize].size.max(1) as f32;
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
        let mut toggle = None;
        TableBuilder::new(ui)
            .striped(true)
            .resizable(true)
            .sense(Sense::click())
            .cell_layout(ELayout::left_to_right(Align::Center))
            .column(Column::initial(120.0).clip(true))
            .column(Column::initial(55.0))
            .column(Column::initial(80.0))
            .column(Column::remainder())
            .header(22.0, |mut h| {
                for (t, c) in [
                    (tr("Extensión", "Extension"), Col::Name),
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
                body.rows(20.0, self.ext_rows.len(), |mut row| {
                    let (e, size, count) = self.ext_rows[row.index()];
                    row.set_selected(self.highlight_ext == Some(e));
                    row.col(|ui| {
                        swatch(ui, tree.exts[e as usize].color);
                        ui.label(tree.ext_label(e));
                    });
                    row.col(|ui| right(ui, format!("{} %", fmt_dec(size as f64 / total as f64 * 100.0))));
                    row.col(|ui| right(ui, fmt_size(size)));
                    row.col(|ui| right(ui, fmt_int(count)));
                    if row.response().clicked() {
                        toggle = Some(e);
                    }
                });
            });
        self.ext_sort = sort;
        if let Some(e) = toggle {
            self.highlight_ext = if self.highlight_ext == Some(e) { None } else { Some(e) };
        }
    }

    fn treemap_ui(&mut self, ui: &mut egui::Ui, tree: &Tree) {
        let (rect, resp) = ui.allocate_exact_size(ui.available_size(), Sense::click());
        let ppp = ui.ctx().pixels_per_point();
        let w = (rect.width() * ppp).round().max(1.0) as usize;
        let h = (rect.height() * ppp).round().max(1.0) as usize;
        let tree_ptr = Arc::as_ptr(self.tree.as_ref().unwrap()) as usize;

        // Debounce: al redimensionar, esperar a que el tamaño se estabilice.
        let size_changed = self.layout.as_ref().is_some_and(|l| (l.width, l.height) != (w, h));
        let mut rebuild = (self.view_root, w, h, self.highlight_ext, tree_ptr) != self.layout_key;
        if size_changed && self.layout_key.0 == self.view_root && self.layout_key.4 == tree_ptr && self.layout_key.3 == self.highlight_ext {
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
            let layout = treemap::build(tree, self.view_root, w, h, self.highlight_ext);
            let img = egui::ColorImage::from_rgba_unmultiplied([w, h], layout.pixels.as_flattened());
            match &mut self.texture {
                Some(t) => t.set(img, egui::TextureOptions::NEAREST),
                None => self.texture = Some(ui.ctx().load_texture("treemap", img, egui::TextureOptions::NEAREST)),
            }
            self.layout = Some(layout);
            self.layout_key = (self.view_root, w, h, self.highlight_ext, tree_ptr);
        }
        let painter = ui.painter_at(rect);
        if let Some(t) = &self.texture {
            painter.image(t.id(), rect, egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)), Color32::WHITE);
        }
        let Some(layout) = &self.layout else { return };
        let (kx, ky) = (rect.width() / layout.width as f32, rect.height() / layout.height as f32);
        let to_screen = |r: treemap::R| {
            egui::Rect::from_min_size(rect.min + egui::vec2(r.x * kx, r.y * ky), egui::vec2(r.w * kx, r.h * ky))
        };

        // Hover
        self.hovered = None;
        if let Some(pos) = resp.hover_pos() {
            let sx = layout.width as f32 / rect.width();
            let sy = layout.height as f32 / rect.height();
            let px = ((pos.x - rect.min.x) * sx) as usize;
            let py = ((pos.y - rect.min.y) * sy) as usize;
            if px < layout.width && py < layout.height {
                let id = layout.owner[py * layout.width + px];
                if id != u32::MAX {
                    self.hovered = Some(id);
                    if let Some(r) = layout.rects.get(&id) {
                        painter.rect_stroke(to_screen(*r), 0.0, Stroke::new(1.0, Color32::from_white_alpha(180)), egui::StrokeKind::Inside);
                    }
                    let n = &tree.nodes[id as usize];
                    resp.clone().on_hover_ui_at_pointer(|ui| {
                        ui.strong(&*n.name);
                        ui.label(tree.full_path(n.parent));
                        ui.label(format!("{}  ·  {}", fmt_size(n.size), if n.is_dir { tr("carpeta", "folder").into() } else { tree.ext_label(n.ext).to_string() }));
                    });
                }
            }
        }

        // Selección (o su ancestro visible más cercano)
        if let Some(sel) = self.selected {
            let mut id = sel;
            loop {
                if let Some(r) = layout.rects.get(&id) {
                    let sr = to_screen(*r);
                    painter.rect_stroke(sr, 0.0, Stroke::new(2.0, Color32::BLACK), egui::StrokeKind::Inside);
                    painter.rect_stroke(sr.shrink(1.0), 0.0, Stroke::new(1.5, Color32::WHITE), egui::StrokeKind::Inside);
                    break;
                }
                if id == ROOT || id == layout.root {
                    break;
                }
                id = tree.nodes[id as usize].parent;
            }
        }

        if resp.clicked()
            && let Some(h) = self.hovered
        {
            self.select(tree, h, true);
        }
        if let Some(h) = self.hovered {
            let mut zoom = None;
            resp.context_menu(|ui| {
                if let Some(z) = context_menu(ui, tree, h) {
                    zoom = Some(z);
                }
            });
            if let Some(z) = zoom {
                self.set_root(z);
            }
        }
    }
}

// ---------------------------------------------------------------- helpers

/// Usa Segoe UI (la fuente de Windows) y Segoe UI Symbol como respaldo de glifos.
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

/// Flecha de expandir/contraer dibujada (no depende de fuentes).
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

/// Menú contextual común. Devuelve una carpeta si se pidió hacer zoom.
fn context_menu(ui: &mut egui::Ui, tree: &Tree, id: u32) -> Option<u32> {
    let n = &tree.nodes[id as usize];
    let path = tree.full_path(id);
    let mut zoom = None;
    ui.label(RichText::new(&*n.name).strong());
    ui.separator();
    if ui.button(tr("Abrir", "Open")).clicked() {
        shell_open(&path, None);
        ui.close();
    }
    if ui.button(tr("Mostrar en el Explorador", "Show in Explorer")).clicked() {
        shell_open("explorer.exe", Some(&format!("/select,\"{path}\"")));
        ui.close();
    }
    if ui.button(tr("Copiar ruta", "Copy path")).clicked() {
        ui.ctx().copy_text(path.clone());
        ui.close();
    }
    let dir = if n.is_dir { id } else { n.parent };
    if ui.button(tr("Hacer zoom en esta carpeta", "Zoom into this folder")).clicked() {
        zoom = Some(dir);
        ui.close();
    }
    zoom
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

fn find_path(tree: &Tree, path: &str) -> Option<u32> {
    if path.is_empty() {
        return Some(ROOT);
    }
    let mut id = ROOT;
    for part in path.split('\\').filter(|p| !p.is_empty()) {
        id = *tree.nodes[id as usize]
            .children
            .iter()
            .find(|&&c| tree.nodes[c as usize].name.eq_ignore_ascii_case(part))?;
    }
    Some(id)
}

fn default_drive(tree: &Tree) -> u32 {
    let drives = &tree.nodes[ROOT as usize].children;
    drives
        .iter()
        .copied()
        .find(|&d| tree.nodes[d as usize].name.eq_ignore_ascii_case("C:"))
        .or_else(|| drives.first().copied())
        .unwrap_or(ROOT)
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

/// Barra de porcentaje estilo WizTree.
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

/// Un decimal con el separador del idioma actual.
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
        if i > 0 && (s.len() - i) % 3 == 0 {
            out.push(thousands);
        }
        out.push(c);
    }
    out
}

fn fmt_age(d: Duration) -> String {
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

fn fmt_date(ft: u64) -> String {
    use windows_sys::Win32::Foundation::{FILETIME, SYSTEMTIME};
    use windows_sys::Win32::System::Time::{FileTimeToSystemTime, SystemTimeToTzSpecificLocalTime};
    if ft == 0 || ft == u64::MAX {
        return String::new();
    }
    let f = FILETIME { dwLowDateTime: ft as u32, dwHighDateTime: (ft >> 32) as u32 };
    unsafe {
        let mut utc: SYSTEMTIME = std::mem::zeroed();
        let mut loc: SYSTEMTIME = std::mem::zeroed();
        if FileTimeToSystemTime(&f, &mut utc) == 0 || SystemTimeToTzSpecificLocalTime(std::ptr::null(), &utc, &mut loc) == 0 {
            return String::new();
        }
        if i18n::lang() == Lang::Es {
            format!("{:02}/{:02}/{:04} {:02}:{:02}", loc.wDay, loc.wMonth, loc.wYear, loc.wHour, loc.wMinute)
        } else {
            format!("{:04}-{:02}-{:02} {:02}:{:02}", loc.wYear, loc.wMonth, loc.wDay, loc.wHour, loc.wMinute)
        }
    }
}
