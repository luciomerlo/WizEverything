//! Flat squarified treemap: one color per top-level group, white 1 px
//! separators, group header tags and "name (size)" cell labels.
//! Rasterized on the CPU in parallel horizontal bands.

use crate::tree::Tree;

#[derive(Clone, Copy)]
pub struct R {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

/// A label to draw over the chart (in layout pixels).
pub struct Label {
    pub id: u32,
    pub r: R,
    /// Group header tag (top-left, dark background) vs. cell label (bottom-left).
    pub header: bool,
}

pub struct Layout {
    pub width: usize,
    pub height: usize,
    pub root: u32,
    pub pixels: Vec<[u8; 4]>,
    /// Leaf node drawn at each pixel (u32::MAX = nothing).
    pub owner: Vec<u32>,
    /// Rectangle of every drawn node (folders included), for selection highlights.
    pub rects: std::collections::HashMap<u32, R>,
    /// Treemap only: group headers and cell labels that fit.
    pub labels: Vec<Label>,
    /// Ring chart only: (start angle, end angle, depth) of each drawn arc.
    pub arcs: std::collections::HashMap<u32, [f32; 3]>,
    /// Ring chart only: (center x, center y, inner radius, ring width) in pixels.
    pub geom: Option<[f32; 4]>,
}

/// Folders smaller than this (px) are drawn as one block instead of subdivided.
const MIN_SIDE: f32 = 6.0;
/// Levels drawn below the view root.
const MAX_DEPTH: usize = 2;
/// Cells need at least this many pixels per side to get a separator line.
const SEP_MIN: usize = 10;
/// Cells bigger than this share of the chart show their content even below MAX_DEPTH.
const BIG_CELL: f32 = 0.12;
/// Cells at least this big (px) get a "name (size)" label.
const LABEL_MIN_W: f32 = 46.0;
const LABEL_MIN_H: f32 = 18.0;
const MAX_LABELS: usize = 800;

/// Group colors (largest group first), close to common BI treemaps.
pub const GROUP_COLORS: [[u8; 3]; 12] = [
    [91, 139, 201],
    [154, 205, 95],
    [255, 138, 92],
    [167, 123, 232],
    [255, 181, 71],
    [233, 87, 155],
    [72, 190, 200],
    [96, 125, 170],
    [200, 95, 95],
    [120, 175, 120],
    [190, 150, 95],
    [140, 140, 140],
];

/// Extension highlight: bytes of that extension inside every node.
pub struct Highlight {
    pub bytes: Vec<u64>,
}


/// A rectangle to paint: node, rect, flat color.
struct Leaf {
    id: u32,
    r: R,
    color: [u8; 3],
}

struct Ctx<'a> {
    tree: &'a Tree,
    leaves: Vec<Leaf>,
    rects: std::collections::HashMap<u32, R>,
    labels: Vec<Label>,
    highlight: Option<&'a Highlight>,
    /// Total chart area in px.
    area: f32,
}

impl Ctx<'_> {
    /// Size used for the layout: the highlighted extension's bytes, or the real size.
    fn size_of(&self, id: u32) -> u64 {
        match self.highlight {
            Some(h) => h.bytes.get(id as usize).copied().unwrap_or(0),
            None => self.tree.nodes[id as usize].size,
        }
    }

    fn leaf(&mut self, id: u32, r: R, color: [u8; 3]) {
        if r.w >= LABEL_MIN_W && r.h >= LABEL_MIN_H && self.labels.len() < MAX_LABELS {
            self.labels.push(Label { id, r, header: false });
        }
        self.leaves.push(Leaf { id, r, color });
    }

    fn node(&mut self, id: u32, r: R, depth: usize, color: [u8; 3]) {
        let tree = self.tree;
        let n = &tree.nodes[id as usize];
        let n_size = self.size_of(id);
        self.rects.insert(id, r);
        // Two levels, like BI treemaps: groups (children of the view root) and their items.
        // Deeper content is reached by double-clicking a cell.
        let big = r.w * r.h >= BIG_CELL * self.area;
        if !n.is_dir() || n_size == 0 || (depth >= MAX_DEPTH && !big) || r.w < MIN_SIDE * 2.0 || r.h < MIN_SIDE * 2.0 {
            self.leaf(id, r, if n_size == 0 { [70, 70, 70] } else { color });
            return;
        }
        // A group with a single sub-folder shows that sub-folder's content instead.
        if depth == 1
            && let [only] = tree.children(id)
            && self.highlight.is_none()
            && tree.nodes[*only as usize].is_dir()
        {
            self.rects.insert(*only, r);
            return self.node(*only, r, depth, color);
        }
        if depth > 0 {
            // Solid background so cells too small to draw don't leave holes.
            self.leaves.push(Leaf { id, r, color });
        }
        let mut kids: Vec<(u32, f64)> =
            tree.children(id).iter().map(|&c| (c, self.size_of(c) as f64)).filter(|c| c.1 > 0.0).collect();
        if self.highlight.is_some() {
            kids.sort_by(|a, b| b.1.total_cmp(&a.1)); // children are ordered by total size
        }
        let mut out = Vec::with_capacity(kids.len());
        squarify(&kids, r, n_size as f64, &mut out);
        for (i, (c, cr)) in out.into_iter().enumerate() {
            if cr.w * cr.h < 0.25 {
                continue;
            }
            // Each top-level group gets its own color; deeper levels inherit it.
            let col = if depth == 0 { GROUP_COLORS[i % GROUP_COLORS.len()] } else { color };
            if depth == 0 && tree.nodes[c as usize].is_dir() && cr.w >= 30.0 && cr.h >= 18.0 {
                self.labels.push(Label { id: c, r: cr, header: true });
            }
            self.node(c, cr, depth + 1, col);
        }
    }
}

/// Squarified layout (Bruls, Huizing & van Wijk). `items` sorted by size, descending.
pub fn squarify(items: &[(u32, f64)], rect: R, total: f64, out: &mut Vec<(u32, R)>) {
    if items.is_empty() || total <= 0.0 {
        return;
    }
    let (mut x, mut y, mut w, mut h) = (rect.x as f64, rect.y as f64, rect.w as f64, rect.h as f64);
    let scale = (w * h) / total;
    let mut i = 0;
    while i < items.len() {
        let short = w.min(h).max(1e-9);
        let mut row_sum = 0.0;
        let mut worst_prev = f64::INFINITY;
        let mut j = i;
        let rmax = items[i].1 * scale;
        while j < items.len() {
            let s = items[j].1 * scale;
            let sum = row_sum + s;
            let worst = (short * short * rmax / (sum * sum)).max(sum * sum / (short * short * s));
            if j > i && worst > worst_prev {
                break;
            }
            worst_prev = worst;
            row_sum = sum;
            j += 1;
        }
        let last = j == items.len();
        let thick = (row_sum / short).min(if w >= h { w } else { h });
        if w >= h {
            let mut cy = y;
            for (k, item) in items.iter().enumerate().take(j).skip(i) {
                let hk = if last && k + 1 == j { y + h - cy } else { item.1 * scale / thick };
                out.push((item.0, R { x: x as f32, y: cy as f32, w: thick as f32, h: hk as f32 }));
                cy += hk;
            }
            x += thick;
            w -= thick;
        } else {
            let mut cx = x;
            for (k, item) in items.iter().enumerate().take(j).skip(i) {
                let wk = if last && k + 1 == j { x + w - cx } else { item.1 * scale / thick };
                out.push((item.0, R { x: cx as f32, y: y as f32, w: wk as f32, h: thick as f32 }));
                cx += wk;
            }
            y += thick;
            h -= thick;
        }
        i = j;
    }
}

/// Pixel range [start, end) covered by a span, with consistent rounding so
/// neighbouring rectangles share edges without gaps or overlaps.
fn span(a: f32, len: f32, max: usize) -> (usize, usize) {
    ((a.round().max(0.0) as usize).min(max), (((a + len).round().max(0.0)) as usize).min(max))
}

/// Run `f(first_row, rows_of_pixels, rows_of_owner)` over horizontal bands in parallel.
pub fn par_bands(
    width: usize,
    pixels: &mut [[u8; 4]],
    owner: &mut [u32],
    f: impl Fn(usize, &mut [[u8; 4]], &mut [u32]) + Sync,
) {
    let height = if width == 0 { 0 } else { pixels.len() / width };
    let threads = std::thread::available_parallelism().map_or(4, |n| n.get()).max(1);
    let rows = height.div_ceil(threads).max(1);
    std::thread::scope(|s| {
        for (i, (px, ow)) in pixels.chunks_mut(rows * width).zip(owner.chunks_mut(rows * width)).enumerate() {
            let f = &f;
            s.spawn(move || f(i * rows, px, ow));
        }
    });
}

/// `line`: separator color (white on light themes, dark on dark ones).
pub fn build(
    tree: &Tree,
    root: u32,
    width: usize,
    height: usize,
    highlight: Option<&Highlight>,
    line: [u8; 3],
) -> Layout {
    let mut ctx = Ctx {
        tree,
        leaves: Vec::new(),
        rects: Default::default(),
        labels: Vec::new(),
        highlight,
        area: (width * height) as f32,
    };
    if width > 0 && height > 0 {
        let r = R { x: 0.0, y: 0.0, w: width as f32, h: height as f32 };
        ctx.node(root, r, 0, GROUP_COLORS[0]);
    }

    let mut pixels = vec![[line[0], line[1], line[2], 255]; width * height];
    let mut owner = vec![u32::MAX; width * height];
    let leaves = &ctx.leaves;
    let sep_color = [line[0], line[1], line[2], 255];
    par_bands(width, &mut pixels, &mut owner, |y_start, px, ow| {
        let y_end = y_start + px.len() / width.max(1);
        for leaf in leaves {
            let (x0, x1) = span(leaf.r.x, leaf.r.w, width);
            let (y0, y1) = span(leaf.r.y, leaf.r.h, height);
            // 1 px separator on the right/bottom edge of cells that are big enough.
            let sep = x1 - x0 >= SEP_MIN && y1 - y0 >= SEP_MIN;
            let (fx1, fy1) = if sep { (x1 - 1, y1 - 1) } else { (x1, y1) };
            let (ya, yb) = (y0.max(y_start), y1.min(y_end));
            if x0 >= x1 || ya >= yb {
                continue;
            }
            let c = [leaf.color[0], leaf.color[1], leaf.color[2], 255];
            // Soft separator: the line color blended 65% over the cell color.
            let sep_color = [0, 1, 2, 3].map(|i| if i == 3 { 255 } else { ((leaf.color[i] as u16 * 35 + sep_color[i] as u16 * 65) / 100) as u8 });
            for y in ya..yb {
                let row = (y - y_start) * width;
                for x in x0..x1 {
                    px[row + x] = if x < fx1 && y < fy1 { c } else { sep_color };
                    ow[row + x] = leaf.id;
                }
            }
        }
    });
    Layout { width, height, root, pixels, owner, rects: ctx.rects, labels: ctx.labels, arcs: Default::default(), geom: None }
}
