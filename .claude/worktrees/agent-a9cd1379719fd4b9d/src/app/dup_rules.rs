//! Duplicates tab extras, kept out of app.rs: selection rules (protected and
//! preferred folders, keep shortest/longest path), replacing copies with hard
//! links, and the assistant's proposals (cards that only mark copies).

use std::sync::atomic::AtomicUsize;

use super::*;
use crate::hardlink;
use crate::rules::{self, Keep, Proposal, Rules};

/// State of the extras (one field in [`Dups`]).
#[derive(Default)]
pub(super) struct DupExtra {
    /// Rules panel open, and the protected-folders text (one per line).
    show_rules: bool,
    protect_text: String,
    /// Hard links: confirmation open, running job and its progress.
    link_confirm: bool,
    link_rx: Option<Receiver<hardlink::Report>>,
    link_done: Arc<AtomicUsize>,
    link_total: usize,
}

impl App {
    pub(super) fn dup_rules(&self) -> Rules {
        Rules::from_settings(&self.settings)
    }

    /// Mark with a rule; protected copies are never marked.
    pub(super) fn dups_apply_rule(&mut self, keep: Keep) {
        let m = rules::plan_marks(&self.dups.groups, &self.dup_rules(), keep);
        self.dups.marked = m.marked;
        self.dups.keep_reason = m.reasons;
        if !m.protected.is_empty() {
            self.set_info(format!(
                "{} {}",
                fmt_int(m.protected.len() as u64),
                tr("copias en carpetas protegidas quedaron sin marcar", "copies in protected folders were left unmarked")
            ));
        }
    }

    /// Marked copies that are protected (blocks the Recycle Bin and hard links).
    pub(super) fn dups_protected_marked(&self) -> usize {
        let r = self.dup_rules();
        if r.protect.is_empty() {
            return 0;
        }
        self.dups.marked.iter().filter(|p| r.is_protected(p)).count()
    }

    /// Is this folder protected? Some(true) = itself, Some(false) = inside one.
    pub(super) fn dup_protected(&self, path: &str) -> Option<bool> {
        let p = rules::norm(path);
        if self.settings.dup_protect.iter().any(|d| rules::norm(d) == p) {
            return Some(true);
        }
        self.settings
            .dup_protect
            .iter()
            .map(|d| rules::norm(d))
            .any(|d| !d.is_empty() && p.starts_with(&d) && p[d.len()..].starts_with('\\'))
            .then_some(false)
    }

    /// Add or remove a protected folder (saved immediately).
    pub(super) fn toggle_dup_protect(&mut self, path: &str) {
        let p = rules::norm(path);
        let before = self.settings.dup_protect.len();
        self.settings.dup_protect.retain(|d| rules::norm(d) != p);
        let now = self.settings.dup_protect.len() == before;
        if now {
            self.settings.dup_protect.push(path.trim_end_matches('\\').to_string());
            // A protected copy can't stay marked.
            let r = self.dup_rules();
            self.dups.marked.retain(|m| !r.is_protected(m));
        }
        self.dups.extra.protect_text = self.settings.dup_protect.join("\n");
        self.settings.save();
        let msg = if now { tr("Protegida:", "Protected:") } else { tr("Ya no está protegida:", "No longer protected:") };
        self.set_info(format!("{msg} {path}"));
    }

    /// Extra marking buttons (after keep oldest/newest).
    pub(super) fn dups_more_mark_buttons(&mut self, ui: &mut egui::Ui) {
        if ui.button(tr("ruta más corta", "shortest path")).on_hover_text(tr("Conserva la copia con la ruta más corta", "Keeps the copy with the shortest path")).clicked() {
            self.dups_apply_rule(Keep::ShortestPath);
        }
        if ui.button(tr("ruta más larga", "longest path")).on_hover_text(tr("Conserva la copia con la ruta más larga", "Keeps the copy with the longest path")).clicked() {
            self.dups_apply_rule(Keep::LongestPath);
        }
    }

    /// Rules row: protected folders (one per line) and the preferred folder.
    pub(super) fn dups_rules_ui(&mut self, ui: &mut egui::Ui, tree: &Tree) {
        let n = self.settings.dup_protect.len();
        let pref = !self.settings.dup_prefer_folder.is_empty();
        ui.horizontal(|ui| {
            let label = format!(
                "🔒 {} ({n}){}",
                tr("Carpetas protegidas y preferida", "Protected and preferred folders"),
                if pref { " ★" } else { "" }
            );
            if ui
                .selectable_label(self.dups.extra.show_rules, label)
                .on_hover_text(tr(
                    "Protegidas: sus copias nunca se marcan ni se envían a la papelera.\nPreferida: al marcar se conserva la copia que está ahí.",
                    "Protected: their copies are never marked or sent to the Recycle Bin.\nPreferred: when marking, the copy inside it is kept.",
                ))
                .clicked()
            {
                self.dups.extra.show_rules = !self.dups.extra.show_rules;
                self.dups.extra.protect_text = self.settings.dup_protect.join("\n");
            }
        });
        if !self.dups.extra.show_rules {
            return;
        }
        ui.label(RichText::new(tr(
            "Carpetas protegidas, una por renglón. También: clic derecho en una carpeta → Proteger.",
            "Protected folders, one per line. Also: right-click a folder → Protect.",
        )).weak());
        let edit = ui.add(
            egui::TextEdit::multiline(&mut self.dups.extra.protect_text)
                .desired_rows(3)
                .desired_width(f32::INFINITY)
                .hint_text("D:\\Fotos\\Originales\nH:\\Mi unidad\\Trabajo"),
        );
        if edit.changed() {
            self.settings.dup_protect = self
                .dups
                .extra
                .protect_text
                .lines()
                .map(|l| l.trim().replace('/', "\\").trim_end_matches('\\').to_string())
                .filter(|l| !l.is_empty())
                .collect();
            let r = self.dup_rules();
            self.dups.marked.retain(|m| !r.is_protected(m));
            self.settings.save();
        }
        ui.horizontal(|ui| {
            ui.label(tr("Conservar la copia en:", "Keep the copy in:")).on_hover_text(tr(
                "Al marcar (cualquier botón), si un grupo tiene una copia dentro de esta carpeta, esa se conserva.",
                "When marking (any button), if a group has a copy inside this folder, that one is kept.",
            ));
            let mut f = self.settings.dup_prefer_folder.clone();
            if ui.add(egui::TextEdit::singleline(&mut f).desired_width(320.0).hint_text("H:\\Mi unidad")).changed() {
                self.settings.dup_prefer_folder = f.trim().replace('/', "\\").trim_end_matches('\\').to_string();
                self.settings.save();
            }
            if pref && ui.small_button("✖").clicked() {
                self.settings.dup_prefer_folder.clear();
                self.settings.save();
            }
        });
        let missing: Vec<&String> = self
            .settings
            .dup_protect
            .iter()
            .chain(std::iter::once(&self.settings.dup_prefer_folder).filter(|s| !s.is_empty()))
            .filter(|p| tree.find_path(p).is_none())
            .collect();
        if !missing.is_empty() {
            ui.colored_label(
                ui.visuals().warn_fg_color,
                format!("{} {}", tr("No están en el índice:", "Not in the index:"), missing.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(" · ")),
            );
        }
    }

    // ------------------------------------------------------------ hard links

    /// "Replace with hard links" button (next to the Recycle Bin one).
    pub(super) fn dups_link_button(&mut self, ui: &mut egui::Ui) {
        let busy = self.dups.extra.link_rx.is_some();
        if busy {
            ui.spinner();
            ui.label(format!(
                "{} {}/{}",
                tr("Enlazando", "Linking"),
                fmt_int(self.dups.extra.link_done.load(Ordering::Relaxed) as u64),
                fmt_int(self.dups.extra.link_total as u64)
            ));
            return;
        }
        let n = self.dups.marked.len();
        if ui
            .add_enabled(n > 0 && self.dups.recycle_rx.is_none(), egui::Button::new(tr("⇄ Reemplazar por enlaces duros", "⇄ Replace with hard links")))
            .on_hover_text(tr(
                "Libera espacio sin borrar rutas: cada copia marcada pasa a ser un enlace duro a la copia conservada (solo grupos verificados, misma unidad NTFS).",
                "Frees space without removing paths: each marked copy becomes a hard link to the kept copy (verified groups only, same NTFS volume).",
            ))
            .clicked()
        {
            self.dups.extra.link_confirm = true;
        }
    }

    /// Confirmation for the hard-link replacement, and starting the job.
    pub(super) fn dups_link_modal(&mut self, ctx: &egui::Context) {
        if !self.dups.extra.link_confirm {
            return;
        }
        let (jobs, refused) = hardlink::plan(&self.dups.groups, &self.dups.marked, &self.dup_rules());
        let bytes: u64 = jobs.iter().map(|j| j.size).sum();
        let resp = egui::Modal::new(egui::Id::unique("dup_link_confirm")).show(ctx, |ui| {
            ui.set_max_width(480.0);
            ui.heading(tr("Reemplazar por enlaces duros", "Replace with hard links"));
            ui.label(RichText::new(format!(
                "{} {} · {} {}",
                fmt_int(jobs.len() as u64),
                tr("copias marcadas se reemplazan", "marked copies will be replaced"),
                fmt_size(bytes),
                tr("liberados", "freed")
            ))
            .strong());
            for line in [
                tr(
                    "• Cada archivo sigue en su ruta: programas, accesos directos y enlaces siguen funcionando.",
                    "• Every file keeps its path: programs, shortcuts and links keep working.",
                ),
                tr(
                    "• Antes de enlazar se vuelven a comparar tamaño y contenido de ambas copias; si algo falla, el original queda intacto.",
                    "• Size and content of both copies are compared again right before linking; if anything fails the original is kept.",
                ),
                tr(
                    "• Fechas, atributos y permisos pasan a ser los de la copia conservada.",
                    "• Dates, attributes and permissions become those of the kept copy.",
                ),
            ] {
                ui.label(line);
            }
            ui.colored_label(
                ui.visuals().warn_fg_color,
                tr(
                    "• Todas las rutas comparten un único contenido: editar una edita todas. No lo uses con archivos que vas a modificar por separado.",
                    "• All paths share one content: editing one edits all. Don't use it for files you will change separately.",
                ),
            );
            if !refused.is_empty() {
                ui.add_space(4.0);
                ui.label(RichText::new(format!("{} {}:", fmt_int(refused.len() as u64), tr("copias marcadas no se pueden enlazar", "marked copies can't be linked"))).strong());
                for (why, n) in hardlink::summarize(&refused).into_iter().take(6) {
                    ui.label(format!("  {} × {why}", fmt_int(n as u64)));
                }
                ui.label(RichText::new(tr(
                    "También se rechazan al enlazar: archivos de solo lectura, de sistema o en la nube, y volúmenes que no son NTFS.",
                    "Also refused when linking: read-only, system or cloud files, and non-NTFS volumes.",
                ))
                .weak());
            }
            ui.add_space(8.0);
            let mut go = false;
            ui.horizontal(|ui| {
                let label = format!("{} ({})", tr("Reemplazar", "Replace"), fmt_int(jobs.len() as u64));
                if ui.add_enabled(!jobs.is_empty(), egui::Button::new(label)).clicked() {
                    go = true;
                }
                if ui.button(tr("Cancelar", "Cancel")).clicked() {
                    self.dups.extra.link_confirm = false;
                }
            });
            go
        });
        if resp.should_close() {
            self.dups.extra.link_confirm = false;
        }
        if resp.inner && !jobs.is_empty() {
            self.dups.extra.link_confirm = false;
            let done = Arc::new(AtomicUsize::new(0));
            self.dups.extra.link_done = done.clone();
            self.dups.extra.link_total = jobs.len();
            let (tx, rx) = channel();
            let ctx = ctx.clone();
            std::thread::spawn(move || {
                let r = hardlink::run(&jobs, &done);
                let _ = tx.send(r);
                ctx.request_repaint();
            });
            self.dups.extra.link_rx = Some(rx);
        }
    }

    /// Collect the hard-link result: drop linked copies from the results.
    pub(super) fn poll_dup_links(&mut self, ctx: &egui::Context) {
        let Some(rx) = &self.dups.extra.link_rx else { return };
        let Ok(r) = rx.try_recv() else {
            ctx.request_repaint_after(Duration::from_millis(250));
            return;
        };
        self.dups.extra.link_rx = None;
        let done: Vec<String> = r.linked.iter().chain(&r.already).cloned().collect();
        hardlink::prune(&mut self.dups.groups, &done);
        self.dups.marked.retain(|p| !done.contains(p));
        self.dups.views = Default::default();
        if r.failed.is_empty() {
            self.set_info(format!(
                "{} {} · {} {}",
                fmt_int(r.linked.len() as u64),
                tr("copias reemplazadas por enlaces duros", "copies replaced with hard links"),
                fmt_size(r.freed),
                tr("liberados", "freed")
            ));
        } else {
            let first: Vec<String> = r.failed.iter().take(3).map(|(p, e)| format!("{p}: {e}")).collect();
            self.set_error(format!(
                "{} {} ({} {}); {} {}:\n{}",
                fmt_int(r.linked.len() as u64),
                tr("enlazadas", "linked"),
                fmt_size(r.freed),
                tr("liberados", "freed"),
                fmt_int(r.failed.len() as u64),
                tr("sin cambios por error", "left unchanged after an error"),
                first.join("\n")
            ));
        }
    }

    // ------------------------------------------------------------ AI proposals

    /// Load a proposal's marks into the Duplicates tab (marks only, nothing is deleted).
    pub(super) fn apply_proposal(&mut self, answer: usize, k: usize) {
        let Some(p) = self.ai.answers.get(answer).and_then(|a| a.proposals.get(k)).cloned() else { return };
        let known: HashSet<&str> = self.dups.groups.iter().flat_map(|g| g.files.iter().map(|f| f.path.as_str())).collect();
        let covered = p.groups.iter().all(|g| g.files.iter().all(|f| known.contains(f.path.as_str())));
        if !covered {
            // Show the proposal's own groups instead of the current results.
            if let Some(job) = self.dups.job.take() {
                job.prog.cancel.store(true, Ordering::Relaxed);
            }
            self.dups.groups = p.groups.clone();
            self.dups.views = Default::default();
            let smallest = p.groups.iter().map(|g| g.size).min().unwrap_or(0) as f64 / (1024.0 * 1024.0);
            self.dups.min_mb = self.dups.min_mb.min(smallest);
        }
        let r = self.dup_rules();
        self.dups.marked = p.marks.marked.iter().filter(|m| !r.is_protected(m)).cloned().collect();
        self.dups.keep_reason = p.marks.reasons.clone();
        self.dups.searched = true;
        self.tab = Tab::Dups;
        self.set_info(format!(
            "{} {} ({}). {}",
            fmt_int(self.dups.marked.len() as u64),
            tr("copias marcadas según la propuesta", "copies marked from the proposal"),
            fmt_size(rules::marked_bytes(&self.dups.groups, &self.dups.marked)),
            tr("Revisalas: nada se borra sin tu confirmación.", "Review them: nothing is deleted without your confirmation."),
        ));
    }
}

/// A proposal card under an AI answer. Returns true when "Apply marks" is clicked.
pub(super) fn proposal_card(ui: &mut egui::Ui, p: &Proposal) -> bool {
    let mut apply = false;
    egui::Frame::group(ui.style()).show(ui, |ui| {
        ui.set_width(ui.available_width());
        let title = if p.kind == "cleanup" { tr("Propuesta de limpieza", "Cleanup proposal") } else { tr("Propuesta de marcas", "Marks proposal") };
        ui.horizontal(|ui| {
            ui.strong(format!("💡 {title}"));
            ui.label(RichText::new(format!("· {} {}", tr("regla", "rule"), p.rule.id())).weak());
        });
        let n = p.marks.marked.len();
        ui.label(format!(
            "{} {} {} · {} {}",
            fmt_int(n as u64),
            tr("copias duplicadas para marcar en", "duplicate copies to mark in"),
            fmt_int(p.groups.len() as u64),
            fmt_size(p.mark_bytes()),
            tr("liberables", "reclaimable")
        ));
        if p.groups.iter().any(|g| !g.verified) {
            ui.colored_label(ui.visuals().warn_fg_color, tr("Incluye grupos sin verificar por contenido (modo rápido).", "Includes groups not verified by content (quick mode)."));
        }
        if !p.marks.protected.is_empty() {
            ui.label(RichText::new(format!("🔒 {} {}", fmt_int(p.marks.protected.len() as u64), tr("copias protegidas quedan sin marcar", "protected copies stay unmarked"))).weak());
        }
        if !p.items.is_empty() {
            ui.collapsing(
                format!("{} {} · {}", fmt_int(p.items.len() as u64), tr("otros elementos para revisar", "other items to review"), fmt_size(p.item_bytes())),
                |ui| {
                    for (path, size, why) in p.items.iter().take(30) {
                        ui.label(format!("{}  {path}  ({why})", fmt_size(*size)));
                    }
                },
            );
        }
        ui.horizontal(|ui| {
            if ui
                .add_enabled(n > 0, egui::Button::new(RichText::new(tr("Aplicar marcas", "Apply marks")).strong()))
                .on_hover_text(tr(
                    "Solo marca las copias en la pestaña Duplicados. Enviar a la papelera o enlazar se confirma aparte.",
                    "Only marks the copies in the Duplicates tab. Recycle Bin or hard links are confirmed separately.",
                ))
                .clicked()
            {
                apply = true;
            }
            ui.label(RichText::new(tr("Nada se borra ni se mueve.", "Nothing is deleted or moved.")).weak());
        });
    });
    apply
}
