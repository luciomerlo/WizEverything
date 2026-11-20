//! "Similares" — parecidos que no son byte-idénticos.
//!
//! Imágenes: hash perceptual (dHash + aHash) sobre una miniatura, agrupado con
//! bandas LSH para no comparar todos contra todos. Detecta la misma foto
//! reescalada, recomprimida o exportada dos veces.
//!
//! Audio: etiquetas (artista/título) + duración, con caída a nombre de archivo
//! cuando faltan tags. Separa versiones por duración (live/remix) y puntúa la
//! calidad para proponer cuál conservar.
//!
//! El motor solo lee: devuelve grupos con un "conservar" sugerido, candidatos a
//! mover y alternativas. Nunca borra ni mueve nada.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde_json::{Value, json};

pub struct FileInfo {
    pub path: PathBuf,
    pub bytes: u64,
}

#[derive(Clone)]
pub struct Item {
    pub path: String,
    pub bytes: u64,
    pub score: f64,
    pub reasons: Vec<String>,
}

pub struct Group {
    pub key: String,
    pub note: Option<String>,
    pub keeper: Item,
    pub others: Vec<Item>,
    pub reclaim: u64,
}

pub struct Report {
    pub kind: &'static str,
    pub files: usize,
    pub analyzed: usize,
    pub skipped: usize,
    pub groups: Vec<Group>,
    pub reclaim: u64,
    pub alternatives: Vec<&'static str>,
    pub notes: Vec<String>,
}

impl Report {
    pub fn to_json(&self, limit: usize) -> Value {
        let top: Vec<Value> = self
            .groups
            .iter()
            .take(limit)
            .map(|g| {
                let item = |i: &Item| {
                    json!({
                        "path": i.path,
                        "bytes": i.bytes,
                        "score": (i.score * 10.0).round() / 10.0,
                        "reasons": i.reasons,
                    })
                };
                json!({
                    "key": g.key,
                    "note": g.note,
                    "reclaim_bytes": g.reclaim,
                    "keep": item(&g.keeper),
                    "others": g.others.iter().map(item).collect::<Vec<_>>(),
                })
            })
            .collect();
        json!({
            "kind": self.kind,
            "files": self.files,
            "analyzed": self.analyzed,
            "skipped": self.skipped,
            "groups": self.groups.len(),
            "reclaim_bytes": self.reclaim,
            "alternatives": self.alternatives,
            "notes": self.notes,
            "top": top,
        })
    }
}


fn ext_of(p: &Path) -> String {
    p.extension().and_then(|e| e.to_str()).unwrap_or("").to_ascii_lowercase()
}

// ---------------------------------------------------------------- imágenes

const IMAGE_SCAN_EXT: &[&str] = &["jpg", "jpeg", "png", "webp", "gif", "bmp", "tif", "tiff"];

#[derive(Clone)]
struct ImgSig {
    path: PathBuf,
    bytes: u64,
    w: u32,
    h: u32,
    fmt: &'static str,
    dhash: u64,
    ahash: u64,
    flat: bool,
}

fn format_of(ext: &str) -> &'static str {
    match ext {
        "tif" | "tiff" => "TIFF",
        "png" => "PNG",
        "webp" => "WebP",
        "gif" => "GIF",
        "bmp" => "BMP",
        "jpg" | "jpeg" => "JPEG",
        _ => "?",
    }
}

fn format_score(fmt: &str) -> f64 {
    match fmt {
        "TIFF" => 40.0,
        "PNG" => 30.0,
        "WebP" => 22.0,
        "JPEG" => 12.0,
        "GIF" => 6.0,
        _ => 0.0,
    }
}

/// dHash de 64 bits: 8 filas x 8 comparaciones horizontales.
fn dhash(img: &image::DynamicImage) -> u64 {
    let g = img.resize_exact(9, 8, image::imageops::FilterType::Triangle).to_luma8();
    let mut h = 0u64;
    let mut bit = 0;
    for y in 0..8u32 {
        for x in 0..8u32 {
            let a = g.get_pixel(x, y)[0];
            let b = g.get_pixel(x + 1, y)[0];
            if a > b {
                h |= 1 << bit;
            }
            bit += 1;
        }
    }
    h
}

/// aHash de 64 bits: píxel por encima/debajo de la media.
fn ahash(img: &image::DynamicImage) -> (u64, bool) {
    let g = img.resize_exact(8, 8, image::imageops::FilterType::Triangle).to_luma8();
    let vals: Vec<u8> = g.pixels().map(|p| p[0]).collect();
    let mean = vals.iter().map(|&v| v as f64).sum::<f64>() / 64.0;
    let var = vals.iter().map(|&v| (v as f64 - mean).powi(2)).sum::<f64>() / 64.0;
    let mut h = 0u64;
    for (i, &v) in vals.iter().enumerate() {
        if v as f64 > mean {
            h |= 1 << i;
        }
    }
    (h, var < 8.0)
}

fn aspect_ok(a: &ImgSig, b: &ImgSig) -> bool {
    let ra = a.w as f64 / a.h.max(1) as f64;
    let rb = b.w as f64 / b.h.max(1) as f64;
    ((ra - rb).abs() / ra.max(rb).max(0.001)) <= 0.08
}

/// Dos firmas son la misma imagen si, además de parecerse, escalan de forma
/// consistente: un reescalado puro mantiene w/h; un recorte no.
fn pair_ok(a: &ImgSig, b: &ImgSig, threshold: u32) -> bool {
    if !aspect_ok(a, b) {
        return false;
    }
    let dh = (a.dhash ^ b.dhash).count_ones();
    let ah = (a.ahash ^ b.ahash).count_ones();
    if a.w == b.w && a.h == b.h {
        return dh <= threshold && ah <= threshold + 2;
    }
    let sw = a.w as f64 / b.w as f64;
    let sh = a.h as f64 / b.h as f64;
    let consistent = ((sw - sh).abs() / sw.max(sh).max(0.001)) <= 0.02;
    consistent && dh <= threshold.saturating_sub(2) && ah <= threshold
}

fn image_score(s: &ImgSig) -> (f64, Vec<String>) {
    let mp = (s.w as f64 * s.h as f64) / 1e6;
    let bpp = s.bytes as f64 / (s.w as f64 * s.h as f64).max(1.0);
    let mut score = mp * 8.0 + format_score(s.fmt) + (bpp * 12.0).min(30.0);
    let canonical = s.path.to_string_lossy().to_ascii_lowercase();
    let in_library = canonical.contains("\\pictures\\")
        || canonical.contains("\\photos\\")
        || canonical.contains("\\imágenes\\")
        || canonical.contains("\\imagenes\\");
    if in_library {
        score += 8.0;
    }
    let mut reasons = vec![
        format!("{}×{} ({:.1} MP)", s.w, s.h, mp),
        s.fmt.to_string(),
        format!("{:.2} B/px", bpp),
    ];
    if in_library {
        reasons.push("en la biblioteca de fotos".into());
    }
    (score, reasons)
}

/// Une índices por similitud usando bandas de 16 bits del dHash (LSH simple).
fn cluster_images(sigs: &[ImgSig], threshold: u32) -> Vec<Vec<usize>> {
    let n = sigs.len();
    let mut parent: Vec<usize> = (0..n).collect();
    fn find(p: &mut [usize], mut x: usize) -> usize {
        while p[x] != x {
            p[x] = p[p[x]];
            x = p[x];
        }
        x
    }
    fn union(p: &mut [usize], a: usize, b: usize) {
        let (ra, rb) = (find(p, a), find(p, b));
        if ra != rb {
            p[ra] = rb;
        }
    }

    let mut buckets: HashMap<u16, Vec<usize>> = HashMap::new();
    for (i, s) in sigs.iter().enumerate() {
        for band in 0..4 {
            let key = ((s.dhash >> (16 * band)) & 0xFFFF) as u16;
            buckets.entry(key).or_default().push(i);
        }
    }
    for ids in buckets.values() {
        // Una banda muy poblada suele ser ruido (imágenes planas): se omite.
        if ids.len() < 2 || ids.len() > 256 {
            continue;
        }
        for a in 0..ids.len() {
            for b in a + 1..ids.len() {
                let (i, j) = (ids[a], ids[b]);
                if pair_ok(&sigs[i], &sigs[j], threshold) {
                    union(&mut parent, i, j);
                }
            }
        }
    }

    let mut groups: HashMap<usize, Vec<usize>> = HashMap::new();
    for i in 0..n {
        let r = find(&mut parent, i);
        groups.entry(r).or_default().push(i);
    }
    groups.into_values().filter(|g| g.len() >= 2).collect()
}

pub fn scan_images(files: Vec<FileInfo>, max_files: usize, threshold: u32) -> Report {
    let mut files: Vec<FileInfo> = files
        .into_iter()
        .filter(|f| {
            let e = ext_of(&f.path);
            IMAGE_SCAN_EXT.contains(&e.as_str()) && f.bytes > 0 && f.bytes <= 96 * 1024 * 1024
        })
        .collect();
    let total = files.len();
    files.sort_by(|a, b| b.bytes.cmp(&a.bytes));
    files.truncate(max_files);

    let mut sigs = Vec::with_capacity(files.len());
    let mut skipped = 0usize;
    for f in &files {
        let ext = ext_of(&f.path);
        let decoded = image::ImageReader::open(&f.path)
            .ok()
            .and_then(|r| r.with_guessed_format().ok())
            .and_then(|r| r.decode().ok());
        let Some(img) = decoded else {
            skipped += 1;
            continue;
        };
        let (w, h) = (img.width(), img.height());
        if w < 64 || h < 64 {
            skipped += 1;
            continue;
        }
        let (ah, flat) = ahash(&img);
        sigs.push(ImgSig {
            path: f.path.clone(),
            bytes: f.bytes,
            w,
            h,
            fmt: format_of(&ext),
            dhash: dhash(&img),
            ahash: ah,
            flat,
        });
    }
    // Las imágenes planas (capturas de un color, iconos) generan falsos positivos.
    let analyzed = sigs.len();
    sigs.retain(|s| !s.flat);

    let mut groups = Vec::new();
    let mut reclaim = 0u64;
    for ids in cluster_images(&sigs, threshold.clamp(2, 20)) {
        let first = ids[0];
        let same_dims = ids.iter().all(|&i| sigs[i].w == sigs[first].w && sigs[i].h == sigs[first].h);
        let bytes_differ = ids.iter().all(|&i| sigs[i].bytes != sigs[first].bytes);
        let mut items: Vec<Item> = ids
            .iter()
            .map(|&i| {
                let s = &sigs[i];
                let (score, reasons) = image_score(s);
                Item {
                    path: s.path.to_string_lossy().to_string(),
                    bytes: s.bytes,
                    score,
                    reasons,
                }
            })
            .collect();
        items.sort_by(|a, b| b.score.total_cmp(&a.score).then(b.bytes.cmp(&a.bytes)));
        let keeper = items.remove(0);
        // Mismas dimensiones pero bytes distintos: ráfaga o edición, no duplicado.
        let mut note = (same_dims && bytes_differ)
            .then(|| "Mismas dimensiones y bytes distintos: podrían ser ráfaga o edición, revisar.".to_string());
        let low = keeper.path.to_ascii_lowercase();
        if low.contains("screenshot") || low.contains("captura de pantalla") {
            note = Some("Capturas de pantalla: confirmar que no sean ventanas distintas.".into());
        }
        let r: u64 = items.iter().map(|o| o.bytes).sum();
        reclaim += r;
        let p = Path::new(&keeper.path);
        let key = match (p.parent().and_then(|x| x.file_name()), p.file_name()) {
            (Some(d), Some(f)) => format!("{}\\{}", d.to_string_lossy(), f.to_string_lossy()),
            _ => keeper.path.clone(),
        };
        groups.push(Group { key, note, keeper, others: items, reclaim: r });
    }
    groups.sort_by(|a, b| b.reclaim.cmp(&a.reclaim));

    let mut notes = Vec::new();
    if total > files.len() {
        notes.push(format!("Se analizaron las {} imágenes más grandes de {total}.", files.len()));
    }
    notes.push("Puntaje = megapíxeles + formato + bytes por píxel + biblioteca de fotos.".into());
    Report {
        kind: "images",
        files: total,
        analyzed,
        skipped,
        groups,
        reclaim,
        alternatives: vec![
            "Mover los descartados a una carpeta _Cuarentena (reversible) antes de borrar.",
            "Si son ediciones distintas (recorte, revelado), conservar ambas.",
            "Hardlink solo si el hash exacto coincide (bytes idénticos).",
            "Comprimir/archivar las descartadas si son frías, en vez de borrarlas.",
            "Conservar la copia de la biblioteca de fotos (~/Pictures) sobre Downloads.",
        ],
        notes,
    }
}

// ------------------------------------------------------------------- audio

const AUDIO_SCAN_EXT: &[&str] = &[
    "mp3", "flac", "m4a", "aac", "ogg", "opus", "wav", "aiff", "aif", "wma", "ape", "wv", "mka",
];

#[derive(Clone)]
struct AudSig {
    path: PathBuf,
    bytes: u64,
    artist: Option<String>,
    title: Option<String>,
    album: Option<String>,
    duration: f64,
    bitrate: Option<u32>,
    sample_rate: Option<u32>,
    bit_depth: Option<u8>,
    lossless: bool,
    cover: bool,
    fmt: &'static str,
}

fn norm(s: &str) -> String {
    let mut out = String::new();
    let mut last_space = true;
    for c in s.to_lowercase().chars() {
        if c.is_alphanumeric() {
            out.push(c);
            last_space = false;
        } else if !last_space {
            out.push(' ');
            last_space = true;
        }
    }
    out.trim().to_string()
}

/// Clave a partir del nombre: saca número de pista, corchetes y sufijos típicos.
pub fn norm_filename(stem: &str) -> String {
    let s = stem.to_lowercase();
    let t = s.trim_start_matches(|c: char| c.is_ascii_digit() || matches!(c, '-' | '.' | '_' | ' '));
    let mut out = String::new();
    let mut depth = 0i32;
    for c in t.chars() {
        match c {
            '[' | '(' => depth += 1,
            ']' | ')' => depth = (depth - 1).max(0),
            _ if depth == 0 => out.push(c),
            _ => {}
        }
    }
    let words: Vec<&str> = out
        .split_whitespace()
        .filter(|w| {
            !matches!(
                *w,
                "official" | "video" | "audio" | "lyrics" | "hd" | "hq" | "remastered" | "feat" | "ft"
            )
        })
        .collect();
    norm(&words.join(" "))
}

fn is_lossless(ext: &str, bit_depth: Option<u8>, bitrate: Option<u32>) -> bool {
    if matches!(ext, "flac" | "wav" | "aiff" | "aif" | "ape" | "wv") {
        return true;
    }
    // M4A puede ser ALAC: bit depth presente y bitrate alto.
    ext == "m4a" && bit_depth.is_some() && bitrate.map(|b| b > 500).unwrap_or(false)
}

fn audio_score(s: &AudSig) -> (f64, Vec<String>) {
    let mut score = if s.lossless {
        150.0
    } else {
        (s.bitrate.unwrap_or(128) as f64 / 320.0) * 70.0
    };
    score += s.sample_rate.map(|r| ((r as f64 / 44100.0).min(4.0)) * 8.0).unwrap_or(0.0);
    score += s.bit_depth.map(|b| b as f64 * 0.6).unwrap_or(0.0);
    score += if s.cover { 4.0 } else { 0.0 };
    score += if s.album.is_some() { 2.0 } else { 0.0 };

    let mut reasons = Vec::new();
    match (s.bitrate, s.sample_rate, s.bit_depth) {
        (Some(br), Some(sr), Some(bd)) => reasons.push(format!(
            "{} {br} kbps · {:.1} kHz · {bd} bit",
            s.fmt,
            sr as f64 / 1000.0
        )),
        (Some(br), Some(sr), None) => {
            reasons.push(format!("{} {br} kbps · {:.1} kHz", s.fmt, sr as f64 / 1000.0))
        }
        (Some(br), None, _) => reasons.push(format!("{} {br} kbps", s.fmt)),
        _ => reasons.push(s.fmt.to_string()),
    }
    if s.lossless {
        reasons.push("sin pérdida".into());
    }
    if s.artist.is_none() || s.title.is_none() {
        reasons.push("sin etiquetas".into());
    }
    (score, reasons)
}

fn duration_cluster(sigs: &[(usize, f64)]) -> Vec<Vec<usize>> {
    let mut v = sigs.to_vec();
    v.sort_by(|a, b| a.1.total_cmp(&b.1));
    let mut out: Vec<Vec<usize>> = Vec::new();
    let mut cur: Vec<usize> = Vec::new();
    let mut last = f64::NAN;
    for (i, d) in v {
        let split = !last.is_nan() && (d - last).abs() > 4.0 && (d - last).abs() / last.max(1.0) > 0.03;
        if split && !cur.is_empty() {
            out.push(std::mem::take(&mut cur));
        }
        cur.push(i);
        last = d;
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

pub fn scan_audio(files: Vec<FileInfo>, max_files: usize) -> Report {
    let mut files: Vec<FileInfo> = files
        .into_iter()
        .filter(|f| AUDIO_SCAN_EXT.contains(&ext_of(&f.path).as_str()) && f.bytes > 0)
        .collect();
    let total = files.len();
    files.sort_by(|a, b| b.bytes.cmp(&a.bytes));
    files.truncate(max_files);

    use lofty::prelude::*;

    let mut sigs = Vec::with_capacity(files.len());
    let mut skipped = 0usize;
    for f in &files {
        let Ok(tagged) = lofty::read_from_path(&f.path) else {
            skipped += 1;
            continue;
        };
        let props = tagged.properties();
        let duration = props.duration().as_secs_f64();
        if duration <= 0.0 {
            skipped += 1;
            continue;
        }
        let tag = tagged.primary_tag().or_else(|| tagged.first_tag());
        let artist = tag.and_then(|t| t.artist()).map(|c| c.to_string()).filter(|s| !s.trim().is_empty());
        let title = tag.and_then(|t| t.title()).map(|c| c.to_string()).filter(|s| !s.trim().is_empty());
        let album = tag.and_then(|t| t.album()).map(|c| c.to_string()).filter(|s| !s.trim().is_empty());
        let ext = ext_of(&f.path);
        let bitrate = props.audio_bitrate();
        let bit_depth = props.bit_depth();
        sigs.push(AudSig {
            path: f.path.clone(),
            bytes: f.bytes,
            artist,
            title,
            album,
            duration,
            bitrate,
            sample_rate: props.sample_rate(),
            bit_depth,
            lossless: is_lossless(&ext, bit_depth, bitrate),
            cover: tag.map(|t| !t.pictures().is_empty()).unwrap_or(false),
            fmt: match ext.as_str() {
                "mp3" => "MP3",
                "flac" => "FLAC",
                "m4a" => "M4A",
                "ogg" => "OGG",
                "opus" => "Opus",
                "wav" => "WAV",
                "aiff" | "aif" => "AIFF",
                "wma" => "WMA",
                "ape" => "APE",
                "wv" => "WV",
                _ => "?",
            },
        });
    }

    // clave -> índices
    let mut by_key: HashMap<String, Vec<usize>> = HashMap::new();
    for (i, s) in sigs.iter().enumerate() {
        let key = match (&s.artist, &s.title) {
            (Some(a), Some(t)) if !norm(a).is_empty() && !norm(t).is_empty() => {
                format!("{}|{}", norm(a), norm(t))
            }
            _ => {
                let stem = s.path.file_stem().and_then(|x| x.to_str()).unwrap_or("");
                norm_filename(stem)
            }
        };
        if key.is_empty() {
            continue;
        }
        by_key.entry(key).or_default().push(i);
    }

    let mut groups = Vec::new();
    let mut reclaim = 0u64;
    for (key, ids) in by_key {
        if ids.len() < 2 {
            continue;
        }
        let durs: Vec<(usize, f64)> = ids.iter().map(|&i| (i, sigs[i].duration)).collect();
        for cluster in duration_cluster(&durs) {
            if cluster.len() < 2 {
                continue;
            }
            let mut items: Vec<Item> = cluster
                .iter()
                .map(|&i| {
                    let s = &sigs[i];
                    let (score, reasons) = audio_score(s);
                    Item {
                        path: s.path.to_string_lossy().to_string(),
                        bytes: s.bytes,
                        score,
                        reasons,
                    }
                })
                .collect();
            items.sort_by(|a, b| b.score.total_cmp(&a.score).then(b.bytes.cmp(&a.bytes)));
            let keeper = items.remove(0);
            let d0 = sigs[cluster[0]].duration;
            let spread = cluster
                .iter()
                .map(|&i| (sigs[i].duration - d0).abs())
                .fold(0.0f64, f64::max);
            let note = (spread > 4.0).then(|| "Duraciones distintas: podrían ser versiones (live/remix).".to_string());
            let r: u64 = items.iter().map(|o| o.bytes).sum();
            reclaim += r;
            let label = key.replace('|', " — ");
            groups.push(Group { key: label, note, keeper, others: items, reclaim: r });
        }
    }
    groups.sort_by(|a, b| b.reclaim.cmp(&a.reclaim));

    let mut notes = Vec::new();
    if total > files.len() {
        notes.push(format!("Se analizaron los {} audios más grandes de {total}.", files.len()));
    }
    notes.push("Clave = artista + título (o nombre de archivo), separando versiones por duración.".into());
    notes.push("Puntaje = sin pérdida > bitrate > sample rate > bit depth > carátula > álbum.".into());
    let report = Report {
        kind: "audio",
        files: total,
        analyzed: sigs.len(),
        skipped,
        groups,
        reclaim,
        alternatives: vec![
            "Conservar el archivo sin pérdida (FLAC) y generar un 320 kbps para el teléfono.",
            "Si son versiones distintas (live/remix), conservar ambas.",
            "Archivar los lossy en otro disco en vez de borrarlos.",
            "Mover los descartados a _Cuarentena (reversible).",
            "Completar etiquetas faltantes antes de decidir.",
        ],
        notes,
    };
    report
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sig(d: u64, a: u64, w: u32, h: u32) -> ImgSig {
        ImgSig {
            path: PathBuf::from(format!("C:\\x\\{w}x{h}_{d}.jpg")),
            bytes: (w as u64 * h as u64) / 2,
            w,
            h,
            fmt: "JPEG",
            dhash: d,
            ahash: a,
            flat: false,
        }
    }

    #[test]
    fn hamming_distance_basics() {
        assert_eq!((0u64 ^ 0u64).count_ones(), 0);
        assert_eq!((0u64 ^ u64::MAX).count_ones(), 64);
        assert_eq!((0b1010u64 ^ 0b0011u64).count_ones(), 2);
    }

    #[test]
    fn aspect_ratio_guard() {
        let a = sig(1, 1, 4000, 3000);
        let b = sig(1, 1, 1000, 750);
        let c = sig(1, 1, 1000, 1000);
        assert!(aspect_ok(&a, &b));
        assert!(!aspect_ok(&a, &c));
    }

    #[test]
    fn clusters_near_hashes_only() {
        let sigs = vec![
            sig(0x00FF, 0x0F0F, 4000, 3000),
            sig(0x00FE, 0x0F0E, 1000, 750),
            sig(0xFFFF_FFFF_FFFF_0000, 0xAAAA_AAAA_AAAA_0000, 800, 600),
        ];
        let groups = cluster_images(&sigs, 10);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].len(), 2);
    }

    #[test]
    fn image_score_prefers_resolution() {
        let big = sig(0, 0, 4000, 3000);
        let small = sig(0, 0, 1000, 750);
        assert!(image_score(&big).0 > image_score(&small).0);
    }

    #[test]
    fn filename_key_strips_noise() {
        assert_eq!(norm_filename("01 - Song Name (Official Video) [HD]"), "song name");
        assert_eq!(norm_filename("07. Other_Song"), "other song");
    }

    #[test]
    fn lossless_beats_mp3() {
        let flac = AudSig {
            path: PathBuf::from("a.flac"),
            bytes: 30_000_000,
            artist: Some("A".into()),
            title: Some("T".into()),
            album: Some("X".into()),
            duration: 200.0,
            bitrate: Some(900),
            sample_rate: Some(44100),
            bit_depth: Some(16),
            lossless: true,
            cover: true,
            fmt: "FLAC",
        };
        let mp3 = AudSig {
            path: PathBuf::from("a.mp3"),
            bytes: 8_000_000,
            artist: Some("A".into()),
            title: Some("T".into()),
            album: None,
            duration: 200.0,
            bitrate: Some(320),
            sample_rate: Some(44100),
            bit_depth: None,
            lossless: false,
            cover: false,
            fmt: "MP3",
        };
        assert!(audio_score(&flac).0 > audio_score(&mp3).0);
    }

    #[test]
    fn duration_clusters_split_versions() {
        let v = vec![(0, 200.0), (1, 201.5), (2, 260.0)];
        let c = duration_cluster(&v);
        assert_eq!(c.len(), 2);
    }
}
