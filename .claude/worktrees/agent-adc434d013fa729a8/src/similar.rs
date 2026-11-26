//! "Similar" duplicates: files that look or sound the same without being
//! byte-identical.
//!
//! * Images (jpg/jpeg/jfif, png, webp, bmp, gif, tif/tiff): 64-bit difference
//!   hash (dHash) of a 9×8 area-averaged grayscale thumbnail. JPEGs are decoded
//!   at 1/2…1/8 scale straight from the DCT, so big photos are cheap. Images
//!   whose hashes are within a Hamming distance (the "sensitivity") are grouped,
//!   using a BK-tree so large collections are not compared pairwise. HEIC/AVIF
//!   are not supported (no practical pure-Rust decoder).
//! * Music (mp3, flac, m4a, ogg, opus, wav): normalized artist + title from the
//!   tags, with durations within ±2 s. Acoustic fingerprinting is out of scope.
//!
//! Like the byte-identical search, cloud placeholders are skipped (nothing is
//! downloaded), excluded folders and the minimum size are respected, the work
//! runs on a small thread pool with progress and cancel, and every result
//! (including "unreadable") is cached by path + size + mtime in a sibling of
//! the hash cache, so a second search reads nothing.

use std::collections::HashMap;
use std::io::BufReader;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use xxhash_rust::xxh3::xxh3_64;

use crate::dup::{self, DupFile, DupGroup, DupOptions, DupProgress};
use crate::hashcache;
use crate::tree::Tree;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    Image,
    Music,
}

/// Per-copy details shown next to each path (0 = unknown / not applicable).
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub struct CopyInfo {
    pub width: u32,
    pub height: u32,
    /// Audio bitrate in kbps.
    pub bitrate: u32,
    pub duration_ms: u32,
}

impl CopyInfo {
    pub fn label(&self) -> String {
        if self.width > 0 {
            format!("{}×{}", self.width, self.height)
        } else if self.bitrate > 0 || self.duration_ms > 0 {
            let s = self.duration_ms / 1000;
            format!("{} kbps · {}:{:02}", self.bitrate, s / 60, s % 60)
        } else {
            String::new()
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct SimOptions {
    pub kind: Kind,
    /// Images: maximum Hamming distance (of 64 bits) to count as similar.
    pub threshold: u32,
}

/// Default image sensitivity: ≤ 6 of 64 bits may differ.
pub const DEFAULT_THRESHOLD: u32 = 6;
/// Music: same artist + title and durations within this many milliseconds.
pub const MAX_DURATION_DIFF_MS: u32 = 2_000;

pub const IMAGE_EXT: &[&str] = &["jpg", "jpeg", "jfif", "png", "webp", "bmp", "gif", "tif", "tiff"];
pub const MUSIC_EXT: &[&str] = &["mp3", "flac", "m4a", "ogg", "opus", "wav"];

fn ext_of(name: &str) -> String {
    name.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase()).unwrap_or_default()
}

/// Index of the copy worth keeping: largest resolution (images) or highest
/// bitrate (music), then the biggest file.
pub fn best(g: &DupGroup) -> usize {
    let q = |f: &DupFile| {
        let i = f.info.unwrap_or_default();
        let main = if g.similar == Some(Kind::Music) { i.bitrate as u64 } else { i.width as u64 * i.height as u64 };
        (main, f.size)
    };
    (0..g.files.len()).max_by_key(|&i| q(&g.files[i])).unwrap_or(0)
}

// ---------------------------------------------------------------- images

/// 64-bit difference hash of an 8-bit grayscale image: area-average into 9×8
/// cells, then one bit per horizontally adjacent pair (left brighter → 1).
pub fn dhash(luma: &[u8], w: usize, h: usize) -> u64 {
    let mut sum = [0u64; 72];
    let mut cnt = [0u32; 72];
    let col: Vec<usize> = (0..w).map(|x| x * 9 / w).collect();
    for y in 0..h {
        let cy = y * 8 / h;
        let row = &luma[y * w..(y + 1) * w];
        for (x, &v) in row.iter().enumerate() {
            let c = cy * 9 + col[x];
            sum[c] += v as u64;
            cnt[c] += 1;
        }
    }
    let avg: Vec<u64> = (0..72).map(|i| if cnt[i] > 0 { sum[i] * 16 / cnt[i] as u64 } else { 0 }).collect();
    let mut hash = 0u64;
    for r in 0..8 {
        for c in 0..8 {
            hash <<= 1;
            if avg[r * 9 + c] > avg[r * 9 + c + 1] {
                hash |= 1;
            }
        }
    }
    hash
}

fn rgb_luma(r: u8, g: u8, b: u8) -> u8 {
    ((r as u32 * 299 + g as u32 * 587 + b as u32 * 114) / 1000) as u8
}

/// JPEG: read the header for the real size, then decode scaled down by the
/// DCT (1/8 for big photos), which is several times faster than a full decode.
fn decode_jpeg(path: &str) -> Option<(Vec<u8>, usize, usize, u32, u32)> {
    use jpeg_decoder::PixelFormat;
    let f = std::fs::File::open(path).ok()?;
    let mut d = jpeg_decoder::Decoder::new(BufReader::new(f));
    d.set_max_decoding_buffer_size(256 << 20);
    d.read_info().ok()?;
    let full = d.info()?;
    d.scale(64, 64).ok()?;
    let px = d.decode().ok()?;
    let info = d.info()?;
    let luma: Vec<u8> = match info.pixel_format {
        PixelFormat::L8 => px,
        PixelFormat::L16 => px.chunks_exact(2).map(|c| (u16::from_ne_bytes([c[0], c[1]]) >> 8) as u8).collect(),
        PixelFormat::RGB24 => px.chunks_exact(3).map(|c| rgb_luma(c[0], c[1], c[2])).collect(),
        PixelFormat::CMYK32 => px
            .chunks_exact(4)
            .map(|c| {
                let k = 255 - c[3] as u32;
                let ch = |v: u8| ((255 - v as u32) * k / 255) as u8;
                rgb_luma(ch(c[0]), ch(c[1]), ch(c[2]))
            })
            .collect(),
    };
    let (w, h) = (info.width as usize, info.height as usize);
    (luma.len() == w * h).then_some((luma, w, h, full.width as u32, full.height as u32))
}

fn decode_other(path: &str) -> Option<(Vec<u8>, usize, usize, u32, u32)> {
    let mut r = image::ImageReader::open(path).ok()?.with_guessed_format().ok()?;
    let mut limits = image::Limits::default();
    limits.max_alloc = Some(256 << 20);
    r.limits(limits);
    let img = r.decode().ok()?;
    let (w, h) = (img.width(), img.height());
    Some((img.into_luma8().into_raw(), w as usize, h as usize, w, h))
}

/// (perceptual hash, width, height), or None if it can't be decoded.
pub fn image_features(path: &str) -> Option<(u64, u32, u32)> {
    let ext = ext_of(path);
    let (luma, w, h, ow, oh) = if matches!(ext.as_str(), "jpg" | "jpeg" | "jfif") { decode_jpeg(path)? } else { decode_other(path)? };
    if w < 9 || h < 8 || ow == 0 || oh == 0 {
        return None;
    }
    Some((dhash(&luma, w, h), ow, oh))
}

/// BK-tree over 64-bit hashes with Hamming distance.
struct BkTree {
    /// (hash, items with this exact hash, children by distance)
    nodes: Vec<(u64, Vec<usize>, Vec<(u32, usize)>)>,
}

impl BkTree {
    fn new() -> Self {
        Self { nodes: Vec::new() }
    }
    fn insert(&mut self, hash: u64, item: usize) {
        if self.nodes.is_empty() {
            self.nodes.push((hash, vec![item], Vec::new()));
            return;
        }
        let mut at = 0;
        loop {
            let d = (self.nodes[at].0 ^ hash).count_ones();
            if d == 0 {
                self.nodes[at].1.push(item);
                return;
            }
            match self.nodes[at].2.iter().find(|c| c.0 == d) {
                Some(&(_, next)) => at = next,
                None => {
                    let id = self.nodes.len();
                    self.nodes.push((hash, vec![item], Vec::new()));
                    self.nodes[at].2.push((d, id));
                    return;
                }
            }
        }
    }
    /// Every item whose hash is within `max` of `hash`.
    fn query(&self, hash: u64, max: u32, out: &mut Vec<usize>) {
        if self.nodes.is_empty() {
            return;
        }
        let mut stack = vec![0usize];
        while let Some(at) = stack.pop() {
            let (h, items, children) = &self.nodes[at];
            let d = (h ^ hash).count_ones();
            if d <= max {
                out.extend_from_slice(items);
            }
            for &(cd, c) in children {
                if cd + max >= d && cd <= d + max {
                    stack.push(c);
                }
            }
        }
    }
}

/// Group images by perceptual hash. `items` = (hash, quality); the best copy
/// (highest quality) leads its group and every member is within `threshold`
/// of the leader (no chaining of merely "similar to a similar" images).
pub fn group_images(items: &[(u64, u64)], threshold: u32) -> Vec<Vec<usize>> {
    let mut bk = BkTree::new();
    for (i, &(h, _)) in items.iter().enumerate() {
        bk.insert(h, i);
    }
    let mut order: Vec<usize> = (0..items.len()).collect();
    order.sort_by_key(|&i| std::cmp::Reverse(items[i].1));
    let mut taken = vec![false; items.len()];
    let mut out = Vec::new();
    let mut near = Vec::new();
    for &lead in &order {
        if taken[lead] {
            continue;
        }
        near.clear();
        bk.query(items[lead].0, threshold, &mut near);
        let mut g = vec![lead];
        g.extend(near.iter().copied().filter(|&j| j != lead && !taken[j]));
        if g.len() > 1 {
            for &j in &g {
                taken[j] = true;
            }
            out.push(g);
        }
    }
    out
}

// ---------------------------------------------------------------- music

fn fold(c: char) -> char {
    match c {
        'á' | 'à' | 'ä' | 'â' | 'ã' | 'å' => 'a',
        'é' | 'è' | 'ë' | 'ê' => 'e',
        'í' | 'ì' | 'ï' | 'î' => 'i',
        'ó' | 'ò' | 'ö' | 'ô' | 'õ' | 'ø' => 'o',
        'ú' | 'ù' | 'ü' | 'û' => 'u',
        'ñ' => 'n',
        'ç' => 'c',
        'ý' | 'ÿ' => 'y',
        c => c,
    }
}

/// Lowercase, accents folded, "&" → "and", bracketed parts ("(Remastered)",
/// "[Live]") and "feat./ft." credits dropped, punctuation removed.
pub fn normalize(s: &str, drop_brackets: bool) -> String {
    let mut out = String::with_capacity(s.len());
    let mut depth = 0i32;
    for ch in s.chars().flat_map(char::to_lowercase) {
        match ch {
            '(' | '[' | '{' if drop_brackets => depth += 1,
            ')' | ']' | '}' if drop_brackets => depth = (depth - 1).max(0),
            _ if depth > 0 => {}
            '&' => out.push_str(" and "),
            c if c.is_alphanumeric() => out.push(fold(c)),
            _ => out.push(' '),
        }
    }
    let mut words: Vec<&str> = out.split_whitespace().collect();
    if let Some(p) = words.iter().position(|w| matches!(*w, "feat" | "ft" | "featuring")) {
        words.truncate(p);
    }
    words.join(" ")
}

/// Grouping key from artist + title (None if either is missing).
pub fn tag_key(artist: &str, title: &str) -> Option<u64> {
    let norm = |s: &str| {
        let n = normalize(s, true);
        if n.is_empty() { normalize(s, false) } else { n }
    };
    let a = norm(artist);
    let a = a.strip_prefix("the ").map(str::to_string).unwrap_or(a);
    let t = norm(title);
    if a.is_empty() || t.is_empty() {
        return None;
    }
    Some(xxh3_64(format!("{a}\u{1}{t}").as_bytes()) | 1)
}

/// (tag key, duration ms, bitrate kbps), or None without usable tags.
pub fn music_features(path: &str) -> Option<(u64, u32, u32)> {
    use lofty::prelude::*;
    let opts = lofty::config::ParseOptions::new().read_cover_art(false);
    let file = lofty::probe::Probe::open(path).ok()?.options(opts).read().ok()?;
    let mut artist = None;
    let mut title = None;
    for tag in file.primary_tag().into_iter().chain(file.tags().iter()) {
        artist = artist.or_else(|| tag.artist().map(|s| s.to_string()).filter(|s| !s.trim().is_empty()));
        title = title.or_else(|| tag.title().map(|s| s.to_string()).filter(|s| !s.trim().is_empty()));
    }
    let key = tag_key(&artist?, &title?)?;
    let p = file.properties();
    let dur = p.duration().as_millis().min(u32::MAX as u128) as u32;
    let br = p.audio_bitrate().or(p.overall_bitrate()).unwrap_or(0);
    Some((key, dur, br))
}

/// Group songs with the same tag key whose durations are within ±2 s of the
/// group's first (shortest) member. `items` = (key, duration ms).
pub fn group_music(items: &[(u64, u32)]) -> Vec<Vec<usize>> {
    let mut by: HashMap<u64, Vec<usize>> = HashMap::new();
    for (i, &(k, _)) in items.iter().enumerate() {
        by.entry(k).or_default().push(i);
    }
    let mut out = Vec::new();
    for mut v in by.into_values().filter(|v| v.len() > 1) {
        v.sort_by_key(|&i| items[i].1);
        let mut g: Vec<usize> = Vec::new();
        for i in v {
            if let Some(&first) = g.first()
                && items[i].1 - items[first].1 > MAX_DURATION_DIFF_MS
            {
                if g.len() > 1 {
                    out.push(std::mem::take(&mut g));
                }
                g.clear();
            }
            g.push(i);
        }
        if g.len() > 1 {
            out.push(g);
        }
    }
    out
}

// ---------------------------------------------------------------- search

fn pack(a: u64, b: u32, c: u32) -> u128 {
    a as u128 | (b as u128) << 64 | (c as u128) << 96
}
fn unpack(v: u128) -> (u64, u32, u32) {
    (v as u64, (v >> 64) as u32, (v >> 96) as u32)
}

struct Cand {
    path: String,
    modified: u64,
    size: u64,
}

/// Features of one file, from the cache or computed (0 = unusable, also cached).
fn analyze(c: &Cand, kind: Kind, p: &DupProgress) -> Option<u128> {
    if dup::is_not_local(&c.path) {
        p.skipped.fetch_add(1, Ordering::Relaxed);
        return None;
    }
    let key = hashcache::file_key(&c.path)?;
    if let Some(v) = hashcache::similar().lock().unwrap().get(&c.path, key) {
        p.cache_hits.fetch_add(1, Ordering::Relaxed);
        return Some(v);
    }
    let v = match kind {
        Kind::Image => {
            p.bytes_read.fetch_add(c.size, Ordering::Relaxed);
            image_features(&c.path)
        }
        Kind::Music => music_features(&c.path),
    }
    .map_or(0, |(a, b, c)| pack(a, b, c));
    hashcache::similar().lock().unwrap().put(&c.path, key, v);
    Some(v)
}

/// Streaming search for similar images or music. Progress: stage 2, with
/// `groups_*` counting files analyzed. Groups are emitted once every file has
/// been analyzed (grouping needs them all), biggest savings first.
pub fn find_streaming(tree: Arc<Tree>, opt: DupOptions, sim: SimOptions, p: &DupProgress, on_group: &(dyn Fn(DupGroup) + Sync)) {
    p.stage.store(0, Ordering::Relaxed);
    let exts = if sim.kind == Kind::Image { IMAGE_EXT } else { MUSIC_EXT };
    let mut cands = Vec::new();
    for (k, id) in tree.files_under_skipping(opt.root, &opt.exclude).into_iter().enumerate() {
        if k % 65_536 == 0 && p.cancel.load(Ordering::Relaxed) {
            return;
        }
        let n = &tree.nodes[id as usize];
        if n.size < opt.min_size.max(1) || !exts.contains(&ext_of(tree.name(id)).as_str()) {
            continue;
        }
        cands.push(Cand { path: tree.full_path(id), modified: n.modified, size: n.size });
    }
    drop(tree);
    p.groups_total.store(cands.len() as u64, Ordering::Relaxed);
    p.files_total.store(cands.len() as u64, Ordering::Relaxed);
    p.bytes_total.store(cands.iter().map(|c| c.size).sum(), Ordering::Relaxed);
    p.stage.store(2, Ordering::Relaxed);

    let next = AtomicUsize::new(0);
    let feats: Mutex<Vec<(usize, u128)>> = Mutex::new(Vec::with_capacity(cands.len()));
    std::thread::scope(|s| {
        for _ in 0..dup::threads() {
            s.spawn(|| {
                let mut local = Vec::new();
                loop {
                    if p.cancel.load(Ordering::Relaxed) {
                        break;
                    }
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    let Some(c) = cands.get(i) else { break };
                    if let Some(v) = analyze(c, sim.kind, p).filter(|&v| v != 0) {
                        local.push((i, v));
                    }
                    p.bytes_done.fetch_add(c.size, Ordering::Relaxed);
                    p.groups_done.fetch_add(1, Ordering::Relaxed);
                }
                feats.lock().unwrap().extend(local);
            });
        }
    });
    // Keep what was analyzed (also after a cancel) for the next search.
    hashcache::similar().lock().unwrap().save();
    if p.cancel.load(Ordering::Relaxed) {
        return;
    }
    let feats = feats.into_inner().unwrap();
    let decoded: Vec<(usize, (u64, u32, u32))> = feats.into_iter().map(|(i, v)| (i, unpack(v))).collect();
    let classes = match sim.kind {
        Kind::Image => {
            let items: Vec<(u64, u64)> =
                decoded.iter().map(|&(i, (h, w, ht))| (h, (w as u64 * ht as u64) << 20 | (cands[i].size >> 20).min(0xF_FFFF))).collect();
            group_images(&items, sim.threshold.min(64))
        }
        Kind::Music => group_music(&decoded.iter().map(|&(_, (k, d, _))| (k, d)).collect::<Vec<_>>()),
    };
    let mut groups: Vec<DupGroup> = classes
        .into_iter()
        .filter_map(|class| {
            // Hard links (same physical file) count once.
            let mut seen = std::collections::HashSet::new();
            let files: Vec<DupFile> = class
                .into_iter()
                .filter(|&j| {
                    let c = &cands[decoded[j].0];
                    std::fs::File::open(&c.path).ok().and_then(|f| dup::file_id(&f)).is_none_or(|id| seen.insert(id))
                })
                .map(|j| {
                    let (i, (_, b, x)) = decoded[j];
                    let c = &cands[i];
                    let info = match sim.kind {
                        Kind::Image => CopyInfo { width: b, height: x, ..Default::default() },
                        Kind::Music => CopyInfo { bitrate: x, duration_ms: b, ..Default::default() },
                    };
                    DupFile { path: c.path.clone(), modified: c.modified, size: c.size, info: Some(info) }
                })
                .collect();
            (files.len() > 1).then(|| DupGroup {
                size: files.iter().map(|f| f.size).max().unwrap_or(0),
                files,
                folder: false,
                inner_files: 0,
                verified: false,
                versions: false,
                similar: Some(sim.kind),
            })
        })
        .collect();
    groups.sort_by_key(|g| std::cmp::Reverse(g.wasted()));
    for g in groups {
        on_group(g);
    }
}

/// Blocking search returning every group (tests).
#[cfg_attr(not(test), allow(dead_code))]
pub fn find(tree: Arc<Tree>, opt: DupOptions, sim: SimOptions, p: &DupProgress) -> Vec<DupGroup> {
    let out = Mutex::new(Vec::new());
    find_streaming(tree, opt, sim, p, &|g| out.lock().unwrap().push(g));
    out.into_inner().unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tree::Builder;
    use std::time::Instant;

    struct TempDir(std::path::PathBuf);
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn temp(tag: &str) -> TempDir {
        // Same throwaway cache file as the other tests (never the user's real one).
        hashcache::set_path(std::env::temp_dir().join(format!("wiz_hash_cache_test_{}.bin", std::process::id())));
        let base = std::env::temp_dir().join(format!("wiz_sim_{tag}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        TempDir(base)
    }

    fn tree_of(dir: &std::path::Path) -> (Tree, u32) {
        let mut b = Builder::new();
        for e in std::fs::read_dir(dir).unwrap() {
            let p = e.unwrap().path();
            let meta = std::fs::metadata(&p).unwrap();
            b.add(false, &dir.to_string_lossy(), &p.file_name().unwrap().to_string_lossy(), Some(meta.len()), 1);
        }
        let tree = b.finish(&[], Instant::now(), String::new());
        let root = tree.find_path(&dir.to_string_lossy()).unwrap();
        (tree, root)
    }

    fn opts(root: u32) -> DupOptions {
        DupOptions { root, min_size: 1, same_name: false, verify: false, exclude: Default::default(), versions: false }
    }

    /// A "photo": soft background, a bright disc and a dark bar; `seed` changes the layout.
    fn scene(w: u32, h: u32, seed: u32) -> image::RgbImage {
        image::RgbImage::from_fn(w, h, |x, y| {
            let (fx, fy) = (x as f32 / w as f32, y as f32 / h as f32);
            let mut v = 60.0 + 80.0 * fx + 40.0 * (fy * 6.0 + seed as f32).sin();
            let (cx, cy) = if seed == 0 { (0.3, 0.4) } else { (0.75, 0.7) };
            if (fx - cx).powi(2) + (fy - cy).powi(2) < 0.04 {
                v = 235.0;
            }
            if seed == 0 && (0.6..0.7).contains(&fx) {
                v = 15.0;
            }
            if seed != 0 && (0.1..0.25).contains(&fy) {
                v = 20.0;
            }
            let v = v.clamp(0.0, 255.0) as u8;
            image::Rgb([v, v.saturating_sub(20), v / 2 + 40])
        })
    }

    fn write_jpeg(img: &image::RgbImage, path: &std::path::Path, quality: u8) {
        let enc = jpeg_encoder::Encoder::new_file(path, quality).unwrap();
        enc.encode(img.as_raw(), img.width() as u16, img.height() as u16, jpeg_encoder::ColorType::Rgb).unwrap();
    }

    #[test]
    fn dhash_tolerates_resize_and_recompression() {
        let a = scene(800, 600, 0);
        let small = image::imageops::resize(&a, 320, 240, image::imageops::FilterType::Triangle);
        let gray = |i: &image::RgbImage| image::DynamicImage::ImageRgb8(i.clone()).into_luma8();
        let (ga, gs, gb) = (gray(&a), gray(&small), gray(&scene(800, 600, 1)));
        let ha = dhash(ga.as_raw(), 800, 600);
        let hs = dhash(gs.as_raw(), 320, 240);
        let hb = dhash(gb.as_raw(), 800, 600);
        assert!((ha ^ hs).count_ones() <= 4, "resized copy: {}", (ha ^ hs).count_ones());
        assert!((ha ^ hb).count_ones() > 16, "different image: {}", (ha ^ hb).count_ones());
    }

    #[test]
    fn bk_tree_grouping_matches_brute_force() {
        // Pseudo-random hashes plus near copies; the BK-tree must find the same neighbours.
        let mut x = 0x9E37_79B9_7F4A_7C15u64;
        let mut rnd = || {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x
        };
        let mut items: Vec<(u64, u64)> = (0..400).map(|i| (rnd(), i)).collect();
        for i in 0..50 {
            let h = items[i].0 ^ (1 << (i % 64)) ^ (1 << ((i * 7) % 64));
            items.push((h, 1000 + i as u64));
        }
        let mut bk = BkTree::new();
        for (i, &(h, _)) in items.iter().enumerate() {
            bk.insert(h, i);
        }
        for probe in [0usize, 7, 49, 120, 420] {
            let mut got = Vec::new();
            bk.query(items[probe].0, 6, &mut got);
            got.sort();
            let want: Vec<usize> = (0..items.len()).filter(|&j| (items[j].0 ^ items[probe].0).count_ones() <= 6).collect();
            assert_eq!(got, want);
        }
        let groups = group_images(&items, 6);
        assert!(groups.len() >= 50);
        // The near copy (higher quality) leads, the original follows.
        assert!(groups.iter().any(|g| g[0] == 400 && g.contains(&0)));
    }

    #[test]
    fn similar_images_group_resized_and_reencoded_copies() {
        let tmp = temp("img");
        let a = scene(1024, 768, 0);
        a.save(tmp.0.join("orig.png")).unwrap();
        write_jpeg(&a, &tmp.0.join("orig_q90.jpg"), 90);
        let small = image::imageops::resize(&a, 400, 300, image::imageops::FilterType::Triangle);
        write_jpeg(&small, &tmp.0.join("small_q60.jpg"), 60);
        small.save(tmp.0.join("small.bmp")).unwrap();
        let other = scene(1024, 768, 1);
        write_jpeg(&other, &tmp.0.join("other.jpg"), 90);
        std::fs::write(tmp.0.join("notes.txt"), b"not an image").unwrap();
        std::fs::write(tmp.0.join("broken.jpg"), b"not really a jpeg").unwrap();

        let (tree, root) = tree_of(&tmp.0);
        let tree = Arc::new(tree);
        let sim = SimOptions { kind: Kind::Image, threshold: DEFAULT_THRESHOLD };
        let p = DupProgress::default();
        let groups = find(tree.clone(), opts(root), sim, &p);
        assert_eq!(groups.len(), 1, "one similar group");
        let g = &groups[0];
        let mut names: Vec<&str> = g.files.iter().map(|f| f.path.rsplit('\\').next().unwrap()).collect();
        names.sort();
        assert_eq!(names, ["orig.png", "orig_q90.jpg", "small.bmp", "small_q60.jpg"]);
        assert_eq!(g.similar, Some(Kind::Image));
        assert!(!g.verified);
        // Resolution per copy; the best copy is a full-size one.
        let b = &g.files[best(g)];
        assert_eq!(b.info.unwrap().label(), "1024×768");
        assert!(g.files.iter().any(|f| f.info.unwrap().label() == "400×300"));
        assert_eq!(g.wasted(), g.files.iter().map(|f| f.size).sum::<u64>() - b.size);
        assert_eq!(p.groups_done.load(Ordering::Relaxed), 6, "6 candidate images (txt skipped)");
        assert!(p.bytes_read.load(Ordering::Relaxed) > 0);

        // Second run: everything (including the broken file) comes from the cache.
        let p2 = DupProgress::default();
        let again = find(tree.clone(), opts(root), sim, &p2);
        assert_eq!(again.len(), 1);
        assert_eq!(p2.bytes_read.load(Ordering::Relaxed), 0, "nothing decoded again");
        assert_eq!(p2.cache_hits.load(Ordering::Relaxed), 6);

        // Min size and excluded folders apply.
        let big = DupOptions { min_size: 10_000_000, ..opts(root) };
        assert!(find(tree.clone(), big, sim, &DupProgress::default()).is_empty());
        let mut excl = opts(root);
        excl.exclude.insert(root);
        assert!(find(tree.clone(), excl, sim, &DupProgress::default()).is_empty());

        // Cancel: nothing is emitted.
        let pc = DupProgress::default();
        pc.cancel.store(true, Ordering::Relaxed);
        assert!(find(tree, opts(root), sim, &pc).is_empty());
    }

    #[test]
    fn music_tag_normalization() {
        let k = |a: &str, t: &str| tag_key(a, t).unwrap();
        assert_eq!(k("The Beatles", "Let It Be"), k("beatles", "let it be (Remastered 2009)"));
        assert_eq!(k("Beyoncé", "Halo"), k("BEYONCE", "Halo [Live]"));
        assert_eq!(k("Simon & Garfunkel", "The Boxer"), k("Simon and Garfunkel", "The Boxer!"));
        assert_eq!(k("Daft Punk feat. Pharrell", "Get Lucky"), k("Daft Punk", "Get Lucky (feat. Pharrell Williams)"));
        assert_ne!(k("Daft Punk", "Get Lucky"), k("Daft Punk", "Lose Yourself to Dance"));
        assert_eq!(k("X", "(Intro)"), k("x", "intro"), "title made only of brackets falls back");
        assert!(tag_key("", "Song").is_none() && tag_key("Artist", "  ").is_none());
    }

    #[test]
    fn music_grouping_by_key_and_duration() {
        let items = [(5, 180_000), (5, 181_500), (5, 240_000), (7, 180_000), (5, 242_000), (5, 179_000)];
        let mut g = group_music(&items);
        for v in &mut g {
            v.sort();
        }
        g.sort();
        assert_eq!(g, vec![vec![0, 1, 5], vec![2, 4]]);
    }

    /// Minimal PCM WAV of `secs` seconds of silence.
    fn wav(path: &std::path::Path, secs: u32, bits: u16) {
        let rate = 8000u32;
        let bytes = rate * secs * bits as u32 / 8;
        let mut d = Vec::new();
        d.extend_from_slice(b"RIFF");
        d.extend_from_slice(&(36 + bytes).to_le_bytes());
        d.extend_from_slice(b"WAVEfmt ");
        d.extend_from_slice(&16u32.to_le_bytes());
        d.extend_from_slice(&1u16.to_le_bytes());
        d.extend_from_slice(&1u16.to_le_bytes());
        d.extend_from_slice(&rate.to_le_bytes());
        d.extend_from_slice(&(rate * bits as u32 / 8).to_le_bytes());
        d.extend_from_slice(&(bits / 8).to_le_bytes());
        d.extend_from_slice(&bits.to_le_bytes());
        d.extend_from_slice(b"data");
        d.extend_from_slice(&bytes.to_le_bytes());
        d.resize(d.len() + bytes as usize, if bits == 8 { 128 } else { 0 });
        std::fs::write(path, d).unwrap();
    }

    fn tag(path: &std::path::Path, artist: &str, title: &str) {
        use lofty::prelude::*;
        let tt = lofty::read_from_path(path).unwrap().primary_tag_type();
        let mut t = lofty::tag::Tag::new(tt);
        t.set_artist(artist.to_string());
        t.set_title(title.to_string());
        t.save_to_path(path, lofty::config::WriteOptions::default()).unwrap();
    }

    #[test]
    fn similar_music_groups_by_tags_and_keeps_highest_bitrate() {
        let tmp = temp("music");
        let f = |n: &str| tmp.0.join(n);
        wav(&f("a_low.wav"), 3, 8);
        tag(&f("a_low.wav"), "Queen", "Bohemian Rhapsody");
        wav(&f("a_high.wav"), 4, 16);
        tag(&f("a_high.wav"), "QUEEN", "Bohemian Rhapsody (Remastered 2011)");
        wav(&f("a_long.wav"), 9, 8);
        tag(&f("a_long.wav"), "Queen", "Bohemian Rhapsody");
        wav(&f("b.wav"), 3, 8);
        tag(&f("b.wav"), "Queen", "Somebody to Love");
        wav(&f("untagged.wav"), 3, 8);

        let (tree, root) = tree_of(&tmp.0);
        let sim = SimOptions { kind: Kind::Music, threshold: 0 };
        let groups = find(Arc::new(tree), opts(root), sim, &DupProgress::default());
        assert_eq!(groups.len(), 1, "one similar song group");
        let g = &groups[0];
        let mut names: Vec<&str> = g.files.iter().map(|f| f.path.rsplit('\\').next().unwrap()).collect();
        names.sort();
        assert_eq!(names, ["a_high.wav", "a_low.wav"], "9 s copy is outside ±2 s");
        assert_eq!(g.similar, Some(Kind::Music));
        let keep = &g.files[best(g)];
        assert!(keep.path.ends_with("a_high.wav"), "highest bitrate kept");
        assert_eq!(keep.info.unwrap().bitrate, 128);
        assert!(keep.info.unwrap().label().starts_with("128 kbps · 0:04"));
    }
}
