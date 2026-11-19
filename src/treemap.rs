//! Treemap "squarified" con sombreado cushion (van Wijk & van de Wetering),
//! rasterizado en CPU como hace WizTree/WinDirStat.

use crate::tree::Tree;

#[derive(Clone, Copy)]
pub struct R {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

pub struct Layout {
    pub width: usize,
    pub height: usize,
    pub root: u32,
    pub pixels: Vec<[u8; 4]>,
    /// Nodo hoja dibujado en cada píxel (u32::MAX = nada).
    pub owner: Vec<u32>,
    /// Rectángulo de cada nodo dibujado (carpetas incluidas), para resaltar selección.
    pub rects: std::collections::HashMap<u32, R>,
}

const HEIGHT: f32 = 0.38;
const SCALE: f32 = 0.91;
const AMBIENT: f32 = 0.15;
const MIN_SIDE: f32 = 1.0;

struct Ctx<'a> {
    tree: &'a Tree,
    width: usize,
    height: usize,
    pixels: Vec<[u8; 4]>,
    owner: Vec<u32>,
    rects: std::collections::HashMap<u32, R>,
    light: [f32; 3],
    highlight_ext: Option<u32>,
}

/// Agrega una "cresta" parabólica en un eje a los coeficientes de superficie.
fn ridge(s: &mut [f32; 4], r: R, h: f32) {
    if r.w > 0.0 {
        let (x1, x2) = (r.x, r.x + r.w);
        s[0] += 4.0 * h * (x2 + x1) / (x2 - x1);
        s[1] -= 4.0 * h / (x2 - x1);
    }
    if r.h > 0.0 {
        let (y1, y2) = (r.y, r.y + r.h);
        s[2] += 4.0 * h * (y2 + y1) / (y2 - y1);
        s[3] -= 4.0 * h / (y2 - y1);
    }
}

impl Ctx<'_> {
    fn paint(&mut self, id: u32, r: R, s: &[f32; 4], color: [u8; 3]) {
        let x0 = r.x.round().max(0.0) as usize;
        let x1 = ((r.x + r.w).round() as usize).min(self.width);
        let y0 = r.y.round().max(0.0) as usize;
        let y1 = ((r.y + r.h).round() as usize).min(self.height);
        let [lx, ly, lz] = self.light;
        let (cr, cg, cb) = (color[0] as f32, color[1] as f32, color[2] as f32);
        for y in y0..y1 {
            let ny = -(2.0 * s[3] * (y as f32 + 0.5) + s[2]);
            let row = y * self.width;
            for x in x0..x1 {
                let nx = -(2.0 * s[1] * (x as f32 + 0.5) + s[0]);
                let cos = (nx * lx + ny * ly + lz) / (nx * nx + ny * ny + 1.0).sqrt();
                let i = (AMBIENT + (1.0 - AMBIENT) * cos.max(0.0)) * 1.25;
                self.pixels[row + x] = [
                    (cr * i).min(255.0) as u8,
                    (cg * i).min(255.0) as u8,
                    (cb * i).min(255.0) as u8,
                    255,
                ];
                self.owner[row + x] = id;
            }
        }
    }

    fn node(&mut self, id: u32, r: R, mut s: [f32; 4], h: f32) {
        let tree = self.tree;
        let n = &tree.nodes[id as usize];
        self.rects.insert(id, r);
        ridge(&mut s, r, h);
        if !n.is_dir {
            let mut c = tree.exts[n.ext as usize].color;
            if let Some(e) = self.highlight_ext
                && e != n.ext
            {
                let g = ((c[0] as u32 + c[1] as u32 + c[2] as u32) / 3 / 3) as u8;
                c = [g, g, g];
            }
            self.paint(id, r, &s, c);
            return;
        }
        if r.w < MIN_SIDE * 2.0 || r.h < MIN_SIDE * 2.0 || n.size == 0 {
            let c = if n.size == 0 { [40, 40, 40] } else { tree.dominant_color(id) };
            self.paint(id, r, &s, c);
            return;
        }
        let kids: Vec<(u32, f64)> = n
            .children
            .iter()
            .map(|&c| (c, tree.nodes[c as usize].size as f64))
            .filter(|c| c.1 > 0.0)
            .collect();
        let mut out = Vec::with_capacity(kids.len());
        squarify(&kids, r, n.size as f64, &mut out);
        for (c, cr) in out {
            if cr.w * cr.h < 0.25 {
                continue;
            }
            self.node(c, cr, s, h * SCALE);
        }
    }
}

/// Algoritmo squarified de Bruls, Huizing y van Wijk. `items` viene ordenado desc.
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
            for k in i..j {
                let hk = if last && k + 1 == j { y + h - cy } else { items[k].1 * scale / thick };
                out.push((items[k].0, R { x: x as f32, y: cy as f32, w: thick as f32, h: hk as f32 }));
                cy += hk;
            }
            x += thick;
            w -= thick;
        } else {
            let mut cx = x;
            for k in i..j {
                let wk = if last && k + 1 == j { x + w - cx } else { items[k].1 * scale / thick };
                out.push((items[k].0, R { x: cx as f32, y: y as f32, w: wk as f32, h: thick as f32 }));
                cx += wk;
            }
            y += thick;
            h -= thick;
        }
        i = j;
    }
}

pub fn build(tree: &Tree, root: u32, width: usize, height: usize, highlight_ext: Option<u32>) -> Layout {
    let (lx, ly, lz) = (-1.0f32, -1.0f32, 10.0f32);
    let len = (lx * lx + ly * ly + lz * lz).sqrt();
    let mut ctx = Ctx {
        tree,
        width,
        height,
        pixels: vec![[30, 30, 30, 255]; width * height],
        owner: vec![u32::MAX; width * height],
        rects: Default::default(),
        light: [lx / len, ly / len, lz / len],
        highlight_ext,
    };
    if width > 0 && height > 0 {
        let r = R { x: 0.0, y: 0.0, w: width as f32, h: height as f32 };
        ctx.node(root, r, [0.0; 4], HEIGHT);
    }
    Layout { width, height, root, pixels: ctx.pixels, owner: ctx.owner, rects: ctx.rects }
}
