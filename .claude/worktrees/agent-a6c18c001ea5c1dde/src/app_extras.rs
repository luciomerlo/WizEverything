//! App glue for the Everything-syntax search filter, size history, free-space
//! alerts, chart legend and thumbnails. A child module of `app`, so it can use
//! the App's private state; the features themselves live in their own modules.

use std::sync::Arc;

use eframe::egui::{self, Key, RichText};

use super::{App, CAT_FLAG, Tab, category_name, category_of, fmt_int, fmt_size};
use crate::i18n::tr;
use crate::tree::{ROOT, Tree};
use crate::treemap::{Highlight, Layout};
use crate::{alerts, history, legend, search, thumbs};

#[derive(Default)]
pub(crate) struct Extras {
    pub search: search::Search,
    pub history: history::History,
    pub alerts: alerts::Alerts,
    pub thumbs: thumbs::Thumbs,
    /// Tree epoch / revision last seen (to react to loads and syncs).
    seen: Option<(u64, u64)>,
    /// Chart highlight while a search is active: ((ext, revision, search generation), bytes).
    hl_cache: Option<((Option<u32>, u64, u64), Arc<Highlight>)>,
    /// Highlight the current chart layout was built with (labels and legend use it).
    pub layout_hl: Option<Arc<Highlight>>,
    pub legend: legend::Legend,
}

impl App {
    /// Background results and reactions to loads/syncs. Called every frame from `logic`.
    pub(super) fn poll_extras(&mut self, ctx: &egui::Context) {
        if self.extras.search.poll() {
            if let Some(e) = self.extras.search.error.clone() {
                self.set_error(e);
            }
            self.invalidate_views();
        }
        if let Some(tree) = self.tree.clone() {
            let now = (self.epoch, self.revision);
            let prev = self.extras.seen.replace(now);
            if prev.map(|p| p.0) != Some(now.0) {
                // A full load: daily snapshot, re-run the search on the new tree.
                self.extras.history.maybe_snapshot(&tree);
                if let Some(r) = &self.extras.search.result
                    && r.epoch != self.epoch
                    && !self.extras.search.busy()
                {
                    let q = r.query.clone();
                    self.extras.search.start(ctx, tree.clone(), self.epoch, self.revision, q);
                }
                self.extras.alerts.kick(false);
            } else if prev != Some(now) {
                // Sync / real sizes: the matched files' sizes may have changed.
                let rev = self.revision;
                if let Some(r) = self.extras.search.result.as_mut()
                    && r.epoch == self.epoch
                    && self.extras.search.revision != rev
                {
                    r.refresh(&tree);
                    self.extras.search.revision = rev;
                    self.extras.search.generation += 1;
                    self.invalidate_views();
                }
                self.extras.alerts.kick(false);
            }
        }

        // Free-space alerts (started once the tray icon exists).
        if !self.extras.alerts.started() && (self.tray_icon.is_some() || !self.has_tray) {
            let hwnd = self.tray_icon.as_ref().map(|t| t.window_handle() as isize);
            self.extras.alerts.start(ctx, hwnd);
        }
        let fired: Vec<alerts::Alert> = self.extras.alerts.rx.as_ref().map(|rx| rx.try_iter().collect()).unwrap_or_default();
        for a in fired {
            self.set_info(format!(
                "⚠ {} {}: {} {} ({} {} %)",
                tr("Poco espacio libre en", "Low free space on"),
                a.drive,
                fmt_size(a.free),
                tr("libres", "free"),
                tr("umbral", "threshold"),
                a.pct
            ));
        }
        if let Some(drive) = alerts::take_clicked() {
            self.show_window(ctx);
            if let Some(tree) = self.tree.clone()
                && let Some(d) = tree.find_drive(&drive)
            {
                self.tab = Tab::Tree;
                self.expanded.clear();
                self.selected = None;
                self.set_root(ctx, d);
            }
        }
    }

    /// Force the chart, file list, file types panel and tree rows to rebuild.
    fn invalidate_views(&mut self) {
        self.layout_key.0 = u32::MAX;
        self.files_key.0 = u32::MAX;
        self.ext_key.0 = u32::MAX;
        self.rows_dirty = true;
    }

    fn search_active(&self) -> Option<&search::SearchResult> {
        self.extras.search.active(self.epoch)
    }

    /// Size shown for a node: only the matching bytes while a search is active.
    pub(super) fn view_size(&self, tree: &Tree, id: u32) -> u64 {
        match self.search_active() {
            Some(r) => r.bytes_of(id),
            None => tree.nodes[id as usize].size,
        }
    }

    /// Tree rows: with a search, only folders holding matches and matching files.
    pub(super) fn filter_rows(&mut self) {
        let Some(r) = self.extras.search.active(self.epoch) else { return };
        let root = self.view_root;
        self.rows.retain(|&(id, _)| id == root || r.bytes_of(id) > 0 || r.files.binary_search(&id).is_ok());
    }

    /// Files tab: the largest files of the view (matching ones while searching).
    pub(super) fn largest_files_view(&self, tree: &Tree, limit: usize) -> Vec<u32> {
        match self.search_active() {
            Some(r) => search::largest_under(tree, r, self.view_root, &self.file_filter, limit),
            None => tree.largest_files(self.view_root, &self.file_filter, limit),
        }
    }

    /// File types panel: per-extension totals of the view (matching files while searching).
    pub(super) fn ext_stats_view(&self, tree: &Tree) -> Vec<(u32, u64, u64)> {
        match self.search_active() {
            Some(r) => search::ext_stats(tree, r, self.view_root),
            None => tree.ext_stats(self.view_root),
        }
    }

    /// Bytes the chart is sized by: the highlighted extension/category, the
    /// search matches, or both (matching files of that extension). Cached.
    pub(super) fn chart_highlight(&mut self, tree: &Tree) -> Option<Arc<Highlight>> {
        let pick = |e: u32| -> Box<dyn Fn(u32) -> bool + '_> {
            if e & CAT_FLAG != 0 {
                let c = (e & !CAT_FLAG) as usize;
                Box::new(move |x: u32| category_of(&tree.exts[x as usize].name) == c)
            } else {
                Box::new(move |x: u32| x == e)
            }
        };
        if let Some(r) = self.extras.search.active(self.epoch) {
            let key = (self.highlight_ext, self.revision, self.extras.search.generation);
            if let Some((k, h)) = &self.extras.hl_cache
                && *k == key
            {
                return Some(h.clone());
            }
            let h = match self.highlight_ext {
                None => r.bytes.clone(),
                Some(e) => Arc::new(Highlight { bytes: search::bytes_for(tree, &r.files, Some(&*pick(e))) }),
            };
            self.extras.hl_cache = Some((key, h.clone()));
            return Some(h);
        }
        self.extras.hl_cache = None;
        let e = self.highlight_ext?;
        if let Some((ce, rev, h)) = &self.highlight_cache
            && *ce == e
            && *rev == self.revision
        {
            return Some(h.clone());
        }
        let bytes = if e & CAT_FLAG != 0 { tree.ext_bytes_where(&*pick(e)) } else { tree.ext_bytes(e) };
        let h = Arc::new(Highlight { bytes });
        self.highlight_cache = Some((e, self.revision, h.clone()));
        Some(h)
    }

    /// After a chart rebuild: remember its highlight and compute its legend.
    pub(super) fn after_layout(&mut self, tree: &Tree, layout: &Layout, hl: Option<Arc<Highlight>>) {
        self.extras.legend = legend::build(tree, layout.root, hl.as_deref(), layout.geom.is_some(), 12);
        self.extras.layout_hl = hl;
    }

    /// What the chart sizes mean when it isn't plain sizes (legend title / ring center).
    pub(super) fn chart_subject(&self, tree: &Tree) -> Option<String> {
        let ext = self.highlight_ext.map(|e| {
            if e & CAT_FLAG != 0 {
                category_name((e & !CAT_FLAG) as usize).to_string()
            } else if tree.exts.get(e as usize).is_some_and(|x| x.name.is_empty()) {
                tree.ext_label(e).to_string()
            } else {
                format!(".{}", tree.ext_label(e))
            }
        });
        match (ext, self.search_active()) {
            (Some(e), Some(_)) => Some(format!("{e} · {}", tr("filtro", "filter"))),
            (Some(e), None) => Some(e),
            (None, Some(_)) => Some(format!("🔍 {}", tr("filtro", "filter"))),
            (None, None) => None,
        }
    }

    /// Bytes of a node in the current chart (highlighted / matching, or its size).
    pub(super) fn chart_size(&self, tree: &Tree, id: u32) -> u64 {
        match &self.extras.layout_hl {
            Some(h) => h.bytes.get(id as usize).copied().unwrap_or(0),
            None => tree.nodes[id as usize].size,
        }
    }

    /// Thumbnail tooltip for image files (tree / files tables, chart).
    pub(super) fn thumb_tooltip(&self, resp: &egui::Response, tree: &Tree, id: u32) {
        let n = &tree.nodes[id as usize];
        if n.is_dir() || !resp.hovered() || !thumbs::is_image(&tree.exts[n.ext as usize].name) {
            return;
        }
        let path = tree.full_path(id);
        resp.clone().on_hover_ui(|ui| self.extras.thumbs.hover_ui(ui, &path));
    }

    /// Thumbnail inside an existing tooltip (the chart's hover card).
    pub(super) fn thumb_in_tooltip(&self, ui: &mut egui::Ui, tree: &Tree, id: u32) {
        let n = &tree.nodes[id as usize];
        if !n.is_dir() && thumbs::is_image(&tree.exts[n.ext as usize].name) {
            self.extras.thumbs.hover_ui(ui, &tree.full_path(id));
        }
    }

    /// The search filter bar (Everything syntax).
    pub(super) fn search_bar(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        ui.horizontal(|ui| {
            ui.label(tr("🔍 Filtrar:", "🔍 Filter:"));
            let w = (ui.available_width() - 420.0).clamp(160.0, 700.0);
            let edit = ui.add(
                egui::TextEdit::singleline(&mut self.extras.search.text)
                    .desired_width(w)
                    .hint_text(tr("Filtrar con sintaxis de Everything: ext:mp4 size:>1gb dm:lastyear", "Filter with Everything syntax: ext:mp4 size:>1gb dm:lastyear")),
            );
            let enter = edit.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter));
            let busy = self.extras.search.busy();
            let text = self.extras.search.text.trim().to_string();
            let apply = ui
                .add_enabled(self.tree.is_some() && !busy, egui::Button::new(tr("Aplicar", "Apply")))
                .on_hover_text(tr(
                    "Filtra árbol, archivos, tipos y gráficos a los archivos que encuentra Everything. Ej.: ext:jpg;png  size:>500mb  dm:thisyear  path:Descargas  !ext:tmp",
                    "Filters tree, files, types and charts to the files Everything finds. E.g.: ext:jpg;png  size:>500mb  dm:thisyear  path:Downloads  !ext:tmp",
                ))
                .clicked();
            if (apply || enter) && !busy {
                if text.is_empty() {
                    self.clear_search();
                } else if let Some(tree) = self.tree.clone() {
                    self.extras.search.start(ctx, tree, self.epoch, self.revision, text);
                }
            }
            let has = self.extras.search.result.is_some();
            if (has || busy) && ui.button(tr("✖ Limpiar", "✖ Clear")).clicked() {
                self.clear_search();
            }
            if busy {
                ui.spinner();
                ui.label(tr("Buscando en Everything…", "Searching Everything…"));
            } else if let (Some(r), Some(tree)) = (self.search_active(), self.tree.as_deref()) {
                let mut s = format!("{} {} · {}", fmt_int(r.count()), tr("archivos", "files"), fmt_size(r.total(tree)));
                if self.view_root != ROOT {
                    s += &format!("  ({} {})", tr("en la vista:", "in view:"), fmt_size(r.bytes_of(self.view_root)));
                }
                let resp = ui.label(RichText::new(s).strong());
                if r.unmapped > 0 || r.folders > 0 {
                    resp.on_hover_text(format!(
                        "{} {} · {} {}",
                        fmt_int(r.folders),
                        tr("carpetas coincidentes ignoradas (el filtro usa archivos)", "matching folders ignored (the filter uses files)"),
                        fmt_int(r.unmapped),
                        tr("archivos nuevos aún no cargados (F5)", "new files not loaded yet (F5)"),
                    ));
                }
            }
        });
    }

    fn clear_search(&mut self) {
        self.extras.search.clear();
        self.extras.hl_cache = None;
        self.invalidate_views();
    }

    /// Floating windows (size history).
    pub(super) fn extras_windows(&mut self, ctx: &egui::Context) {
        let tree = self.tree.clone();
        self.extras.history.window(ctx, |p| {
            let t = tree.as_deref()?;
            t.find_path(p).map(|id| t.nodes[id as usize].size)
        });
    }

    /// Options menu entries (free-space alerts).
    pub(super) fn extras_options(&mut self, ui: &mut egui::Ui) {
        let drives = alerts::local_drives();
        self.extras.alerts.options_ui(ui, &drives);
    }

    /// Legend over the chart (the caller handles toggles and clicks).
    pub(super) fn legend_ui(&self, ui: &mut egui::Ui, rect: egui::Rect, tree: &Tree) -> legend::Response {
        let what = self.chart_subject(tree);
        legend::overlay(ui, rect, tree, &self.extras.legend, self.settings.show_legend, what.as_deref())
    }
}

/// Outline of a chart group (treemap rect or ring arc) in screen space.
pub(super) fn group_outline(layout: &Layout, rect: egui::Rect, id: u32, stroke: egui::Stroke) -> Option<egui::Shape> {
    let (kx, ky) = (rect.width() / layout.width.max(1) as f32, rect.height() / layout.height.max(1) as f32);
    if let Some(r) = layout.rects.get(&id) {
        let sr = egui::Rect::from_min_size(rect.min + egui::vec2(r.x * kx, r.y * ky), egui::vec2(r.w * kx, r.h * ky));
        return Some(egui::Shape::rect_stroke(sr, 0.0, stroke, egui::StrokeKind::Inside));
    }
    let (geom, arc) = (layout.geom?, *layout.arcs.get(&id)?);
    let pts = crate::sunburst::arc_outline(geom, arc).into_iter().map(|(x, y)| rect.min + egui::vec2(x * kx, y * ky)).collect();
    Some(egui::Shape::closed_line(pts, stroke))
}
