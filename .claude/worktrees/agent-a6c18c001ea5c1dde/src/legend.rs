//! Color legend of the charts: the first-level groups of the view with the
//! color each one gets in the treemap / ring chart, and their size (only the
//! highlighted or filtered bytes when a highlight or search is active).

use eframe::egui::{self, Color32, Sense, Stroke};

use crate::app::{fmt_dec, fmt_size};
use crate::i18n::tr;
use crate::tree::{ROOT, Tree};
use crate::treemap::{GROUP_COLORS, Highlight};

pub struct Entry {
    pub id: u32,
    pub color: [u8; 3],
    pub size: u64,
}

#[derive(Default)]
pub struct Legend {
    pub entries: Vec<Entry>,
    /// Groups beyond the listed ones: (bytes, how many).
    pub others: (u64, u64),
    /// Bytes of the whole view (highlighted bytes when highlighting).
    pub total: u64,
}

/// Groups in the order and colors the charts use: the treemap colors the
/// non-empty children by rank of (highlighted) size; the ring chart by
/// position among all children. At most `max` entries (colors repeat after 12).
pub fn build(tree: &Tree, root: u32, hl: Option<&Highlight>, rings: bool, max: usize) -> Legend {
    let size_of = |id: u32| match hl {
        Some(h) => h.bytes.get(id as usize).copied().unwrap_or(0),
        None => tree.nodes[id as usize].size,
    };
    let mut lg = Legend { total: size_of(root), ..Default::default() };
    if !tree.nodes[root as usize].is_dir() || lg.total == 0 {
        return lg;
    }
    // (color index, id, size)
    let mut groups: Vec<(usize, u32, u64)> = if rings {
        tree.children(root).iter().enumerate().map(|(i, &c)| (i, c, size_of(c))).filter(|g| g.2 > 0).collect()
    } else {
        let mut v: Vec<(u32, u64)> = tree.children(root).iter().map(|&c| (c, size_of(c))).filter(|c| c.1 > 0).collect();
        if hl.is_some() {
            v.sort_by(|a, b| b.1.cmp(&a.1));
        }
        v.into_iter().enumerate().map(|(i, (c, s))| (i, c, s)).collect()
    };
    if rings && hl.is_some() {
        groups.sort_by(|a, b| b.2.cmp(&a.2));
    }
    for (k, (i, id, size)) in groups.into_iter().enumerate() {
        if k < max {
            lg.entries.push(Entry { id, color: GROUP_COLORS[i % GROUP_COLORS.len()], size });
        } else {
            lg.others.0 += size;
            lg.others.1 += 1;
        }
    }
    lg
}

pub struct Response {
    /// Group under the pointer.
    pub hovered: Option<u32>,
    pub clicked: Option<u32>,
    /// The pointer is over the legend (the chart below must ignore it).
    pub over: bool,
    pub toggled: bool,
}

const W: f32 = 240.0;
const ROW: f32 = 18.0;

/// Draw the legend in the top-right corner of `chart`, with a show/hide tab.
/// `what`: what the sizes mean when not the plain sizes (e.g. ".mp4", "filter").
pub fn overlay(ui: &mut egui::Ui, chart: egui::Rect, tree: &Tree, lg: &Legend, show: bool, what: Option<&str>) -> Response {
    let mut out = Response { hovered: None, clicked: None, over: false, toggled: false };
    let font = egui::FontId::proportional(11.5);
    let painter = ui.painter_at(chart);
    let bg = Color32::from_rgba_unmultiplied(28, 32, 40, 215);
    let fg = Color32::from_gray(235);
    let weak = Color32::from_gray(170);

    // Tab: "Legend ▾" / "Legend ▸".
    let title = match what {
        Some(w) => format!("{} · {w}", tr("Leyenda", "Legend")),
        None => tr("Leyenda", "Legend").to_string(),
    };
    let tab_text = format!("{} {}", if show { "▾" } else { "▸" }, if show { title.as_str() } else { tr("Leyenda", "Legend") });
    let galley = painter.layout_no_wrap(tab_text, font.clone(), fg);
    let tab_w = if show { W } else { galley.size().x + 14.0 };
    let tab = egui::Rect::from_min_size(egui::pos2(chart.right() - tab_w - 6.0, chart.top() + 6.0), egui::vec2(tab_w, ROW + 2.0));
    if !chart.contains_rect(tab) {
        return out;
    }
    let tab_resp = ui.interact(tab, ui.scope_id().with("legend_tab"), Sense::click());
    let rows = if show { lg.entries.len() + usize::from(lg.others.1 > 0) } else { 0 };
    let body = egui::Rect::from_min_size(tab.left_bottom(), egui::vec2(W, rows as f32 * ROW + if rows > 0 { 6.0 } else { 0.0 }));
    let body = body.intersect(chart.shrink(2.0));
    painter.rect_filled(if rows > 0 { tab.union(body) } else { tab }, 4.0, bg);
    if tab_resp.hovered() {
        painter.rect_filled(tab, 4.0, Color32::from_white_alpha(18));
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    let tab_resp = tab_resp.on_hover_text(if show { tr("Ocultar la leyenda", "Hide the legend") } else { tr("Mostrar la leyenda de colores", "Show the color legend") });
    painter.galley(tab.left_center() + egui::vec2(7.0, -galley.size().y / 2.0), galley, fg);
    out.toggled = tab_resp.clicked();
    let pointer = ui.ctx().pointer_hover_pos();
    out.over = pointer.is_some_and(|p| tab.contains(p) || (rows > 0 && body.contains(p)));
    if rows == 0 {
        return out;
    }
    let total = lg.total.max(1) as f64;
    let body_resp = ui.interact(body, ui.scope_id().with("legend_body"), Sense::click());
    let mut y = body.top() + 3.0;
    let row = |y: f32| egui::Rect::from_min_size(egui::pos2(body.left(), y), egui::vec2(W, ROW));
    for e in &lg.entries {
        let r = row(y);
        if r.bottom() > body.bottom() + 0.5 {
            break;
        }
        let hot = pointer.is_some_and(|p| r.contains(p));
        if hot {
            painter.rect_filled(r.shrink2(egui::vec2(2.0, 0.0)), 2.0, Color32::from_white_alpha(28));
            out.hovered = Some(e.id);
            if body_resp.clicked() {
                out.clicked = Some(e.id);
            }
        }
        let sw = egui::Rect::from_center_size(egui::pos2(r.left() + 13.0, r.center().y), egui::vec2(11.0, 11.0));
        painter.rect_filled(sw, 2.0, Color32::from_rgb(e.color[0], e.color[1], e.color[2]));
        painter.rect_stroke(sw, 2.0, Stroke::new(1.0, Color32::from_black_alpha(140)), egui::StrokeKind::Inside);
        let pct = format!("{} %", fmt_dec(e.size as f64 / total * 100.0));
        let size = fmt_size(e.size);
        let right = painter.text(r.right_center() - egui::vec2(8.0, 0.0), egui::Align2::RIGHT_CENTER, &pct, font.clone(), weak);
        let mid = painter.text(egui::pos2(right.left() - 8.0, r.center().y), egui::Align2::RIGHT_CENTER, &size, font.clone(), fg);
        let name = if e.id == ROOT { tr("Este equipo", "This PC").to_string() } else if tree.nodes[e.id as usize].parent == ROOT { tree.drive_label(e.id) } else { tree.name(e.id).to_string() };
        let mut job = egui::text::LayoutJob::single_section(name, egui::TextFormat::simple(font.clone(), fg));
        job.wrap = egui::text::TextWrapping { max_width: (mid.left() - r.left() - 30.0).max(10.0), max_rows: 1, break_anywhere: true, overflow_character: Some('…') };
        let g = painter.layout_job(job);
        painter.galley(egui::pos2(r.left() + 23.0, r.center().y - g.size().y / 2.0), g, fg);
        y += ROW;
    }
    if lg.others.1 > 0 {
        let r = row(y);
        if r.bottom() <= body.bottom() + 0.5 {
            painter.text(
                r.left_center() + egui::vec2(23.0, 0.0),
                egui::Align2::LEFT_CENTER,
                format!("{} ({})", tr("Otros", "Others"), lg.others.1),
                font.clone(),
                weak,
            );
            painter.text(r.right_center() - egui::vec2(8.0, 0.0), egui::Align2::RIGHT_CENTER, fmt_size(lg.others.0), font, weak);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tree::Builder;

    /// Q9:\ a\ (300: x.mp4 200, y.txt 100) · b\ (50: z.mp4) · c.txt (500) · empty\
    fn sample() -> Tree {
        let mut b = Builder::new();
        b.add(true, "", "Q9:", None, 1);
        b.add(false, "Q9:\\a", "x.mp4", Some(200), 1);
        b.add(false, "Q9:\\a", "y.txt", Some(100), 1);
        b.add(false, "Q9:\\b", "z.mp4", Some(50), 1);
        b.add(false, "Q9:", "c.txt", Some(500), 1);
        b.add(true, "Q9:", "empty", None, 1);
        b.finish(&[], std::time::Instant::now(), String::new())
    }

    fn names(t: &Tree, lg: &Legend) -> Vec<String> {
        lg.entries.iter().map(|e| t.name(e.id).to_string()).collect()
    }

    #[test]
    fn legend_follows_chart_colors() {
        let t = sample();
        let q = t.find_path("Q9:").unwrap();
        let lg = build(&t, q, None, false, 12);
        assert_eq!(names(&t, &lg), vec!["c.txt", "a", "b"], "empty groups are left out");
        assert_eq!(lg.entries.iter().map(|e| e.color).collect::<Vec<_>>(), GROUP_COLORS[..3].to_vec());
        assert_eq!(lg.entries.iter().map(|e| e.size).collect::<Vec<_>>(), vec![500, 300, 50]);
        assert_eq!((lg.total, lg.others), (850, (0, 0)));
        // At most `max` rows; the rest is summed.
        let lg = build(&t, q, None, false, 1);
        assert_eq!((lg.entries.len(), lg.others), (1, (350, 2)));
    }

    #[test]
    fn legend_with_highlight_reorders_like_the_treemap() {
        let t = sample();
        let q = t.find_path("Q9:").unwrap();
        let mp4 = t.find_ext("mp4").unwrap();
        let hl = Highlight { bytes: t.ext_bytes(mp4) };
        let lg = build(&t, q, Some(&hl), false, 12);
        assert_eq!(names(&t, &lg), vec!["a", "b"]);
        assert_eq!(lg.entries.iter().map(|e| e.size).collect::<Vec<_>>(), vec![200, 50]);
        assert_eq!(lg.entries[0].color, GROUP_COLORS[0], "treemap: colors by highlighted rank");
        assert_eq!(lg.total, 250);
        // Ring chart: colors by position among all children (c.txt, a, b, empty).
        let lg = build(&t, q, Some(&hl), true, 12);
        assert_eq!(names(&t, &lg), vec!["a", "b"]);
        assert_eq!(lg.entries[0].color, GROUP_COLORS[1]);
        assert_eq!(lg.entries[1].color, GROUP_COLORS[2]);
    }
}
