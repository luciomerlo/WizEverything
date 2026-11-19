//! Baobab-style ring chart (sunburst), rasterized on the CPU.
//! The view root is the center; each ring is one level deeper. Folders are
//! colored by angle (hue) and get lighter with depth; files use their
//! extension color.

use std::f32::consts::{PI, TAU};

use crate::tree::Tree;
use crate::treemap::{GROUP_COLORS, Layout};

const LEVELS: usize = 7;

struct Arc {
    a0: f32,
    a1: f32,
    id: u32,
    color: [u8; 3],
}

pub struct Params<'a> {
    pub highlight: Option<&'a crate::treemap::Highlight>,
    pub bg: [u8; 3],
}

pub fn build(tree: &Tree, root: u32, width: usize, height: usize, p: &Params<'_>) -> Layout {
    let mut pixels = vec![[p.bg[0], p.bg[1], p.bg[2], 255]; width * height];
    let mut owner = vec![u32::MAX; width * height];
    let (cx, cy) = (width as f32 / 2.0, height as f32 / 2.0);
    let rmax = (width.min(height) as f32 / 2.0 - 6.0).max(10.0);
    let r0 = rmax * 0.2;
    let ring = (rmax - r0) / LEVELS as f32;

    // Arcs per level, in angular order.
    let mut levels: Vec<Vec<Arc>> = (0..LEVELS).map(|_| Vec::new()).collect();
    // With an extension highlighted, arcs are sized by that extension's bytes
    // only: the chart shows where those files are and how much they weigh.
    let size_of = |id: u32| match p.highlight {
        Some(h) => h.bytes.get(id as usize).copied().unwrap_or(0),
        None => tree.nodes[id as usize].size,
    };
    let mut stack = vec![(root, 0.0f32, TAU, 0usize, 0usize)];
    while let Some((id, a0, a1, depth, group)) = stack.pop() {
        let total = size_of(id) as f32;
        if depth >= LEVELS || total <= 0.0 {
            continue;
        }
        let outer = r0 + ring * (depth + 1) as f32;
        let mut a = a0;
        for (i, &c) in tree.children(id).iter().enumerate() {
            let n = &tree.nodes[c as usize];
            let span = (a1 - a0) * size_of(c) as f32 / total;
            if span * outer < 0.6 {
                if p.highlight.is_some() {
                    a += span; // order is by total size, not by highlighted bytes
                    continue;
                }
                break; // children are sorted by size: the rest are smaller still
            }
            // Same palette as the treemap: one color per first-level group,
            // getting lighter towards the outside.
            let g = if depth == 0 { i % GROUP_COLORS.len() } else { group };
            let base = GROUP_COLORS[g];
            let k = (depth as f32 * 0.09).min(0.5);
            let mut color = [0, 1, 2].map(|j| (base[j] as f32 + (255.0 - base[j] as f32) * k) as u8);
            if !n.is_dir() && p.highlight.is_none() {
                color = [0, 1, 2].map(|j| (color[j] as f32 * 0.88) as u8); // files a bit darker
            }
            levels[depth].push(Arc { a0: a, a1: a + span, id: c, color });
            if n.is_dir() {
                stack.push((c, a, a + span, depth + 1, g));
            }
            a += span;
        }
    }
    let mut arcs = std::collections::HashMap::new();
    for (d, l) in levels.iter_mut().enumerate() {
        l.sort_by(|x, y| x.a0.total_cmp(&y.a0));
        for a in l.iter() {
            arcs.insert(a.id, [a.a0, a.a1, d as f32]);
        }
    }

    let center = [200, 200, 200];
    let levels = &levels;
    // Rasterize in horizontal bands on every core.
    crate::treemap::par_bands(width, &mut pixels, &mut owner, |y_start, pixels, owner| {
    for y in y_start..y_start + pixels.len() / width.max(1) {
        let dy = y as f32 + 0.5 - cy;
        for x in 0..width {
            let dx = x as f32 + 0.5 - cx;
            let r = (dx * dx + dy * dy).sqrt();
            if r > rmax {
                continue;
            }
            let i = (y - y_start) * width + x;
            if r < r0 {
                let k = 1.0 - 0.25 * r / r0;
                pixels[i] = [(center[0] as f32 * k) as u8, (center[1] as f32 * k) as u8, (center[2] as f32 * k) as u8, 255];
                owner[i] = root;
                continue;
            }
            let rel = r - r0;
            let d = (rel / ring) as usize;
            if d >= LEVELS {
                continue;
            }
            // Angle: 0 at the top, clockwise.
            let t = (dy.atan2(dx) + PI / 2.0).rem_euclid(TAU);
            let lvl = &levels[d];
            let k = lvl.partition_point(|a| a.a0 <= t);
            if k == 0 {
                continue;
            }
            let arc = &lvl[k - 1];
            if t >= arc.a1 {
                continue;
            }
            // Separators between rings and between arcs.
            let within = rel - d as f32 * ring;
            let edge = within < 1.0 || (t - arc.a0) * r < 1.0 || (arc.a1 - t) * r < 0.6;
            let c = arc.color;
            let shade = if edge { 0.55 } else { 1.05 - 0.2 * within / ring };
            pixels[i] = [
                (c[0] as f32 * shade).min(255.0) as u8,
                (c[1] as f32 * shade).min(255.0) as u8,
                (c[2] as f32 * shade).min(255.0) as u8,
                255,
            ];
            owner[i] = arc.id;
        }
    }
    });
    Layout { width, height, root, pixels, owner, rects: Default::default(), labels: Vec::new(), arcs, geom: Some([cx, cy, r0, ring]) }
}

/// Outline polygon of an arc (in layout pixels), for hover/selection highlights.
pub fn arc_outline(geom: [f32; 4], arc: [f32; 3]) -> Vec<(f32, f32)> {
    let [cx, cy, r0, ring] = geom;
    let [a0, a1, d] = arc;
    let (ri, ro) = (r0 + ring * d, r0 + ring * (d + 1.0));
    let steps = (((a1 - a0) * ro / 4.0).ceil() as usize).clamp(2, 256);
    let pt = |a: f32, r: f32| {
        let t = a - PI / 2.0;
        (cx + r * t.cos(), cy + r * t.sin())
    };
    let mut v = Vec::with_capacity(steps * 2 + 2);
    for s in 0..=steps {
        v.push(pt(a0 + (a1 - a0) * s as f32 / steps as f32, ro));
    }
    for s in (0..=steps).rev() {
        v.push(pt(a0 + (a1 - a0) * s as f32 / steps as f32, ri));
    }
    v
}
