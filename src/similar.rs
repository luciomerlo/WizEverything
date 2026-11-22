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

use crate::noise;

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
    /// Signals that the file was generated or edited by an AI tool.
    pub ai: Vec<String>,
    pub camera: Option<String>,
    pub captured: Option<String>,
    /// Acoustic profile (audio only): noise floor, SNR, artifacts.
    pub noise: Option<Value>,
    /// Fast similarity to the reference copy in the group (0–100).
    pub similarity: Option<f64>,
}

pub struct Group {
    pub key: String,
    pub note: Option<String>,
    pub keeper: Item,
    pub others: Vec<Item>,
    pub reclaim: u64,
    /// True when some copy looks AI-edited: keep both as variants, don't reclaim.
    pub ai_modified: bool,
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
    pub ai_images: Vec<Value>,
    /// Wall-clock time of the analysis, for the "fast first" contract.
    pub elapsed_ms: u64,
    /// True when a time budget stopped the analysis before covering every file.
    pub partial: bool,
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
                        "ai": i.ai,
                        "camera": i.camera,
                        "captured": i.captured,
                        "noise": i.noise,
                        "similarity_pct": i.similarity.map(|v| (v * 10.0).round() / 10.0),
                    })
                };
                json!({
                    "key": g.key,
                    "note": g.note,
                    "ai_modified": g.ai_modified,
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
            "ai_images": self.ai_images,
            "elapsed_ms": self.elapsed_ms,
            "partial": self.partial,
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
    exif: ExifInfo,
}

#[derive(Clone, Default)]
struct ExifInfo {
    camera: Option<String>,
    captured: Option<String>,
    ai: Vec<String>,
}

const AI_MARKERS: &[&str] = &[
    "midjourney",
    "dall-e",
    "dall e",
    "stable diffusion",
    "firefly",
    "generative",
    "ai generated",
    "ai-generated",
    "comfyui",
    "automatic1111",
    "novelai",
    "leonardo ai",
    "imagen",
    "flux",
];

fn read_exif(path: &Path) -> ExifInfo {
    let mut out = ExifInfo::default();
    let ext = ext_of(path);
    // Filename signals survive metadata stripping.
    let low = path.to_string_lossy().to_ascii_lowercase();
    for k in AI_MARKERS {
        if low.contains(k) {
            out.ai.push((*k).to_string());
        }
    }
    if !matches!(ext.as_str(), "jpg" | "jpeg" | "tif" | "tiff") {
        return out;
    }
    let Ok(file) = std::fs::File::open(path) else { return out };
    let mut buf = std::io::BufReader::new(file);
    let Ok(exif) = exif::Reader::new().read_from_container(&mut buf) else { return out };
    let get = |tag: exif::Tag| {
        exif.get_field(tag, exif::In::PRIMARY)
            .map(|f| f.display_value().with_unit(&exif).to_string())
            .filter(|s| !s.trim().is_empty())
    };
    let make = get(exif::Tag::Make);
    let model = get(exif::Tag::Model);
    out.camera = match (make, model) {
        (Some(a), Some(b)) if !a.contains(&b) => Some(format!("{a} {b}")),
        (_, Some(b)) => Some(b),
        (Some(a), None) => Some(a),
        _ => None,
    };
    out.captured = get(exif::Tag::DateTimeOriginal)
        .or_else(|| get(exif::Tag::DateTimeDigitized))
        .or_else(|| get(exif::Tag::DateTime));
    let mut meta = String::new();
    for tag in [
        exif::Tag::Software,
        exif::Tag::Artist,
        exif::Tag::ImageDescription,
        exif::Tag::UserComment,
        exif::Tag::Copyright,
    ] {
        if let Some(v) = get(tag) {
            meta.push_str(&v);
            meta.push(' ');
        }
    }
    let meta = meta.to_ascii_lowercase();
    for k in AI_MARKERS {
        if meta.contains(k) && !out.ai.iter().any(|x| x.as_str() == *k) {
            out.ai.push((*k).to_string());
        }
    }
    out
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

/// Perceptual similarity 0–100 from the two hashes (cheap, no pixel diff).
fn image_similarity(a: &ImgSig, b: &ImgSig) -> f64 {
    let dh = (a.dhash ^ b.dhash).count_ones() as f64;
    let ah = (a.ahash ^ b.ahash).count_ones() as f64;
    (100.0 - ((dh + ah) / 2.0) * 100.0 / 64.0).clamp(0.0, 100.0)
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
    if let Some(cam) = &s.exif.camera {
        reasons.push(cam.clone());
    }
    if let Some(dt) = &s.exif.captured {
        reasons.push(dt.clone());
    }
    if !s.exif.ai.is_empty() {
        score -= 12.0;
        reasons.push(format!("posible IA: {}", s.exif.ai.join(", ")));
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

/// Cloud placeholders must never be opened by the similarity scanner: opening
/// them hydrates the file and downloads it from Google Drive/OneDrive. Checking
/// attributes is metadata-only and keeps the "no hydration" contract.
fn is_cloud_placeholder(path: &Path) -> bool {
    crate::winphys::is_cloud_placeholder(path)
}

pub fn scan_images(files: Vec<FileInfo>, max_files: usize, threshold: u32, budget_ms: u64) -> Report {
    let scan_start = std::time::Instant::now();
    let budget = std::time::Duration::from_millis(budget_ms.clamp(200, 120_000));
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

    // Decoding full-resolution photos is the expensive part: run it in parallel and
    // stop at a wall-clock budget, reporting how much was actually analysed.
    let workers = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
        .clamp(1, 8);
    let cursor = std::sync::atomic::AtomicUsize::new(0);
    let skipped_n = std::sync::atomic::AtomicUsize::new(0);
    let cloud_n = std::sync::atomic::AtomicUsize::new(0);
    let partial_n = std::sync::atomic::AtomicBool::new(false);
    let collected: std::sync::Mutex<Vec<(usize, ImgSig, Option<Value>)>> =
        std::sync::Mutex::new(Vec::with_capacity(files.len()));
    std::thread::scope(|scope| {
        for _ in 0..workers {
            scope.spawn(|| loop {
                if scan_start.elapsed() >= budget {
                    partial_n.store(true, std::sync::atomic::Ordering::Relaxed);
                    break;
                }
                let i = cursor.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                if i >= files.len() {
                    break;
                }
                let f = &files[i];
                let ext = ext_of(&f.path);
                if is_cloud_placeholder(&f.path) {
                    // Metadata-only: never open a cloud placeholder here.
                    cloud_n.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    skipped_n.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    continue;
                }
                let decoded = image::ImageReader::open(&f.path)
                    .ok()
                    .and_then(|r| r.with_guessed_format().ok())
                    .and_then(|r| r.decode().ok());
                let Some(img) = decoded else {
                    skipped_n.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    continue;
                };
                let (w, h) = (img.width(), img.height());
                if w < 64 || h < 64 {
                    skipped_n.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    continue;
                }
                let (ah, flat) = ahash(&img);
                let exif = read_exif(&f.path);
                let ai = (!exif.ai.is_empty()).then(|| {
                    json!({
                        "path": f.path.to_string_lossy(),
                        "bytes": f.bytes,
                        "ai": exif.ai.clone(),
                        "camera": exif.camera.clone(),
                        "captured": exif.captured.clone(),
                    })
                });
                collected.lock().unwrap().push((
                    i,
                    ImgSig {
                        path: f.path.clone(),
                        bytes: f.bytes,
                        w,
                        h,
                        fmt: format_of(&ext),
                        dhash: dhash(&img),
                        ahash: ah,
                        flat,
                        exif,
                    },
                    ai,
                ));
            });
        }
    });
    let mut got = collected.into_inner().unwrap();
    got.sort_by_key(|(i, _, _)| *i);
    let mut sigs = Vec::with_capacity(got.len());
    let mut ai_images: Vec<Value> = Vec::new();
    for (_, s, ai) in got {
        if let Some(ai) = ai {
            ai_images.push(ai);
        }
        sigs.push(s);
    }
    let skipped = skipped_n.load(std::sync::atomic::Ordering::Relaxed);
    let cloud = cloud_n.load(std::sync::atomic::Ordering::Relaxed);
    let partial = partial_n.load(std::sync::atomic::Ordering::Relaxed)
        || cursor.load(std::sync::atomic::Ordering::Relaxed) < files.len();
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
                    ai: s.exif.ai.clone(),
                    camera: s.exif.camera.clone(),
                    captured: s.exif.captured.clone(),
                    noise: None,
                    similarity: Some(image_similarity(s, &sigs[first])),
                }
            })
            .collect();
        items.sort_by(|a, b| b.score.total_cmp(&a.score).then(b.bytes.cmp(&a.bytes)));
        let ai_modified = items.iter().any(|i| !i.ai.is_empty());
        let mut keeper = items.remove(0);
        keeper.similarity = Some(100.0);
        // Mismas dimensiones pero bytes distintos: ráfaga o edición, no duplicado.
        let mut note = (same_dims && bytes_differ)
            .then(|| "Mismas dimensiones y bytes distintos: podrían ser ráfaga o edición, revisar.".to_string());
        let low = keeper.path.to_ascii_lowercase();
        if low.contains("screenshot") || low.contains("captura de pantalla") {
            note = Some("Capturas de pantalla: confirmar que no sean ventanas distintas.".into());
        }
        if ai_modified {
            note = Some("Alguna copia parece generada/editada con IA: conservar ambas como variantes.".into());
        }
        // AI variants are kept, not reclaimed.
        let r: u64 = if ai_modified { 0 } else { items.iter().map(|o| o.bytes).sum() };
        reclaim += r;
        let p = Path::new(&keeper.path);
        let key = match (p.parent().and_then(|x| x.file_name()), p.file_name()) {
            (Some(d), Some(f)) => format!("{}\\{}", d.to_string_lossy(), f.to_string_lossy()),
            _ => keeper.path.clone(),
        };
        groups.push(Group {
            key,
            note,
            keeper,
            others: items,
            reclaim: r,
            ai_modified,
        });
    }
    groups.sort_by(|a, b| b.reclaim.cmp(&a.reclaim));

    let mut notes = Vec::new();
    if total > files.len() {
        notes.push(format!("Se analizaron las {} imágenes más grandes de {total}.", files.len()));
    }
    notes.push("Puntaje = megapíxeles + formato + bytes por píxel + biblioteca de fotos.".into());
    if cloud > 0 {
        notes.push(format!(
            "{cloud} imágenes son placeholders de nube y no se abrieron (no se descargaron)."
        ));
    }
    if partial {
        let pct = if files.is_empty() {
            100.0
        } else {
            analyzed as f64 * 100.0 / files.len() as f64
        };
        notes.push(format!(
            "Presupuesto rápido: se analizaron {analyzed} de {} imágenes ({pct:.0}%) en {} ms con {workers} hilos; subí budget_ms para una pasada completa.",
            files.len(),
            scan_start.elapsed().as_millis()
        ));
    } else if !files.is_empty() {
        notes.push(format!(
            "Análisis completo en {} ms con {workers} hilos.",
            scan_start.elapsed().as_millis()
        ));
    }
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
        ai_images,
        elapsed_ms: scan_start.elapsed().as_millis() as u64,
        partial,
    }
}

// ------------------------------------------------------------------- audio

const AUDIO_SCAN_EXT: &[&str] = &[
    "mp3", "flac", "m4a", "aac", "ogg", "opus", "wav", "aiff", "aif", "wma", "ape", "wv", "mka",
];

/// Very short clips (UI sounds, notifications) are not "songs".
const MIN_TRACK_SECS: f64 = 10.0;

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
    channels: Option<u8>,
    lossless: bool,
    cover: bool,
    fmt: &'static str,
}

fn norm(s: &str) -> String {
    let mut out = String::new();
    let mut last_space = true;
    for c in s.to_lowercase().chars() {
        // Fold diacritics so "Acústico" matches "acustico".
        let c = match c {
            'á' | 'à' | 'ä' | 'â' => 'a',
            'é' | 'è' | 'ë' | 'ê' => 'e',
            'í' | 'ì' | 'ï' | 'î' => 'i',
            'ó' | 'ò' | 'ö' | 'ô' => 'o',
            'ú' | 'ù' | 'ü' | 'û' => 'u',
            'ñ' => 'n',
            _ => c,
        };
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
    ext == "m4a" && bit_depth.is_some() && bitrate.map(|b| b > 500).unwrap_or(false)
}

/// Words that describe a version/recording, not the song itself.
const VERSION_WORDS: &[&str] = &[
    "live", "vivo", "directo", "concierto", "concert", "television", "tv", "grabado", "grabacion",
    "bootleg", "audience", "radio", "remix", "mix", "remixed", "acoustic", "acustico", "unplugged",
    "sesion", "session", "karaoke", "instrumental", "cover", "demo",
];

/// Stopwords + version words: tokens that should *not* identify a song.
fn is_stop(w: &str) -> bool {
    matches!(
        w,
        "the" | "a" | "an" | "and" | "of" | "de" | "la" | "el" | "los" | "las" | "y" | "o" | "en"
            | "con" | "for" | "feat" | "ft" | "official" | "video" | "audio" | "lyrics" | "lyric"
            | "hd" | "hq" | "remaster" | "remastered" | "version" | "album" | "single" | "edit"
            | "denoised" | "denoise" | "denoising" | "denoiser" | "dehiss" | "hiss" | "restored"
            | "enhanced" | "reduced" | "noise" | "declick" | "decrackle" | "dsp"
    ) || VERSION_WORDS.contains(&w)
        || (w.len() > 1
            && w.starts_with('v')
            && w[1..].chars().all(|c| c.is_ascii_digit()))
}

fn version_label(text: &str) -> Option<&'static str> {
    let t = format!(" {} ", norm(text));
    let any = |ws: &[&str]| ws.iter().any(|w| t.contains(&format!(" {w} ")));
    if any(&[
        "live", "vivo", "directo", "concierto", "concert", "television", "tv", "grabado",
        "grabacion", "bootleg", "audience", "radio",
    ]) {
        return Some("live");
    }
    if any(&["remix", "mix", "remixed"]) {
        return Some("remix");
    }
    if any(&["acoustic", "acustico", "unplugged", "sesion", "session"]) {
        return Some("acoustic");
    }
    if any(&["karaoke"]) {
        return Some("karaoke");
    }
    if any(&["instrumental"]) {
        return Some("instrumental");
    }
    if any(&["cover"]) {
        return Some("cover");
    }
    if any(&["demo"]) {
        return Some("demo");
    }
    None
}

fn tokens(s: &str) -> Vec<String> {
    let n = norm(s);
    let mut v: Vec<String> = n
        .split_whitespace()
        .filter(|w| w.len() >= 2 && !is_stop(w) && !w.chars().all(|c| c.is_ascii_digit()))
        .map(|w| w.to_string())
        .collect();
    v.sort();
    v.dedup();
    v
}

struct AudKey {
    tokens: Vec<String>,
    label: &'static str,
    raw: String,
}

fn audio_key(s: &AudSig) -> AudKey {
    let (raw, tagged) = match (&s.artist, &s.title) {
        (Some(a), Some(t)) if !norm(a).is_empty() && !norm(t).is_empty() => {
            (format!("{a} - {t}"), true)
        }
        _ => {
            let stem = s.path.file_stem().and_then(|x| x.to_str()).unwrap_or("");
            (stem.to_string(), false)
        }
    };
    let mut label = if tagged && s.album.is_some() { "album" } else { "unknown" };
    if let Some(v) = version_label(&raw) {
        label = v;
    }
    if let Some(a) = &s.album
        && let Some(v) = version_label(a)
    {
        label = v;
    }
    AudKey { tokens: tokens(&raw), label, raw }
}

fn dice(a: &[String], b: &[String]) -> (f64, usize) {
    let (mut i, mut j, mut inter) = (0, 0, 0);
    while i < a.len() && j < b.len() {
        match a[i].cmp(&b[j]) {
            std::cmp::Ordering::Less => i += 1,
            std::cmp::Ordering::Greater => j += 1,
            std::cmp::Ordering::Equal => {
                i += 1;
                j += 1;
                inter += 1;
            }
        }
    }
    let union = a.len() + b.len();
    (if union > 0 { 2.0 * inter as f64 / union as f64 } else { 0.0 }, inter)
}

/// A TV/YouTube rip can add intro, applause or a longer outro.
fn duration_ok(a: &AudSig, b: &AudSig) -> bool {
    let (x, y) = (a.duration.max(1.0), b.duration.max(1.0));
    x.max(y) / x.min(y) <= 1.7
}

/// Fuzzy grouping: same song even when tags differ, one is an album track and the
/// other a broadcast/YouTube recording. Uses shared title tokens + duration.
fn group_audio(sigs: &[AudSig]) -> (Vec<Vec<usize>>, Vec<AudKey>) {
    let keys: Vec<AudKey> = sigs.iter().map(audio_key).collect();
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

    let mut buckets: HashMap<&str, Vec<usize>> = HashMap::new();
    for (i, k) in keys.iter().enumerate() {
        for t in &k.tokens {
            buckets.entry(t.as_str()).or_default().push(i);
        }
    }
    for ids in buckets.values() {
        if ids.len() < 2 || ids.len() > 200 {
            continue;
        }
        for a in 0..ids.len() {
            for b in a + 1..ids.len() {
                let (i, j) = (ids[a], ids[b]);
                if !duration_ok(&sigs[i], &sigs[j]) {
                    continue;
                }
                let (sim, shared) = dice(&keys[i].tokens, &keys[j].tokens);
                if sim >= 0.62 && (shared >= 2 || sim >= 0.85) {
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
    (groups.into_values().filter(|g| g.len() >= 2).collect(), keys)
}

fn audio_score(s: &AudSig, label: &str) -> (f64, Vec<String>) {
    // Effective bitrate when the container does not report one.
    let br = s
        .bitrate
        .unwrap_or_else(|| (s.bytes as f64 * 8.0 / s.duration.max(1.0) / 1000.0) as u32);
    let mut score = if s.lossless {
        150.0
    } else {
        (br as f64 / 320.0) * 70.0
    };
    score += s.sample_rate.map(|r| ((r as f64 / 44100.0).min(4.0)) * 8.0).unwrap_or(0.0);
    score += s.bit_depth.map(|b| b as f64 * 0.6).unwrap_or(0.0);
    score += if s.cover { 4.0 } else { 0.0 };
    score += if s.album.is_some() { 2.0 } else { 0.0 };

    let mut reasons = Vec::new();
    match (s.bitrate, s.sample_rate, s.bit_depth) {
        (Some(b), Some(sr), Some(bd)) => reasons.push(format!(
            "{} {b} kbps · {:.1} kHz · {bd} bit",
            s.fmt,
            sr as f64 / 1000.0
        )),
        (Some(b), Some(sr), None) => {
            reasons.push(format!("{} {b} kbps · {:.1} kHz", s.fmt, sr as f64 / 1000.0))
        }
        (Some(b), None, _) => reasons.push(format!("{} {b} kbps", s.fmt)),
        _ => reasons.push(format!("{} ~{br} kbps (estimado)", s.fmt)),
    }
    if s.lossless {
        reasons.push("sin pérdida".into());
    }
    match s.channels {
        Some(1) => {
            score -= 8.0;
            reasons.push("mono".into());
        }
        Some(2) => score += 2.0,
        _ => {}
    }
    match label {
        "album" => {
            score += 4.0;
            reasons.push("álbum".into());
        }
        "live" => {
            score -= 6.0;
            reasons.push("en vivo/TV".into());
        }
        "remix" | "acoustic" | "karaoke" | "instrumental" | "cover" | "demo" => {
            reasons.push(label.to_string());
        }
        _ => {}
    }
    if s.artist.is_none() || s.title.is_none() {
        reasons.push("sin etiquetas".into());
    }
    (score, reasons)
}

/// Filename markers that suggest a denoise/restoration pass.
fn noise_label(text: &str) -> Option<&'static str> {
    let t = format!(" {} ", norm(text));
    let hit = [
        " denois",
        " dehiss",
        " de hiss",
        " hiss",
        " noise reduction",
        " noise reduced",
        " restored",
        " enhanced",
        " remaster",
        " declip",
        " denoiser",
        " declick",
        " decrackle",
        " dsp",
        " dn ",
    ]
    .iter()
    .any(|w| t.contains(w));
    hit.then_some("denoised")
}

fn r1(x: f32) -> f32 {
    (x * 10.0).round() / 10.0
}

fn short_name(p: &str) -> String {
    let path = Path::new(p);
    let name = path.file_name().and_then(|x| x.to_str()).unwrap_or(p);
    match path.parent().and_then(|d| d.file_name()).and_then(|x| x.to_str()) {
        Some(dir) => format!("{dir}/{name}"),
        None => name.to_string(),
    }
}

pub fn scan_audio(files: Vec<FileInfo>, max_files: usize, deep: bool) -> Report {
    let scan_start = std::time::Instant::now();
    let window = if deep { 120.0 } else { 20.0 };
    let budget = std::time::Duration::from_millis(if deep { 60_000 } else { 1_200 });
    let decode_cap = if deep { usize::MAX } else { 120 };
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
    let mut cloud = 0usize;
    let mut meta_partial = false;
    for f in &files {
        // Tag reading can dominate the fast budget on slow/network volumes.
        if scan_start.elapsed() >= budget {
            meta_partial = true;
            break;
        }
        if is_cloud_placeholder(&f.path) {
            skipped += 1;
            cloud += 1;
            continue;
        }
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
        if duration < MIN_TRACK_SECS {
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
            channels: props.channels(),
            lossless: is_lossless(&ext, bit_depth, bitrate),
            cover: tag.map(|t| !t.pictures().is_empty()).unwrap_or(false),
            fmt: match ext.as_str() {
                "mp3" => "MP3",
                "flac" => "FLAC",
                "m4a" => "M4A",
                "aac" => "AAC",
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

    let (groups, keys) = group_audio(&sigs);
    let group_count = groups.len();
    let mut out_groups = Vec::new();
    let mut reclaim = 0u64;
    let mut decoded = 0usize;
    let mut noise_groups = 0usize;
    let mut noise_partial = false;
    for ids in groups {
        let mut labels: Vec<&str> = ids
            .iter()
            .map(|&i| keys[i].label)
            .filter(|l| *l != "unknown")
            .collect();
        labels.sort();
        labels.dedup();
        let mixed = labels.len() > 1;
        let mut durs: Vec<f64> = ids.iter().map(|&i| sigs[i].duration).collect();
        durs.sort_by(|a, b| a.total_cmp(b));
        let spread = durs.last().copied().unwrap_or(0.0) - durs.first().copied().unwrap_or(0.0);
        let mut note = if mixed {
            Some("Versiones distintas (álbum vs en vivo/TV/remix): conservá ambas si te interesan.".to_string())
        } else if spread > 8.0 {
            Some("Duraciones distintas: podría ser otra grabación (TV/en vivo).".to_string())
        } else {
            None
        };

        // Acoustic profile of each copy: this is what separates "denoised" from
        // the original and "clean" from "over-filtered". Bounded to 8 copies.
        // Fast first: at most 20 s per copy, 120 files and ~1.2 s of wall clock.
        let do_noise = !noise_partial && decoded < decode_cap && scan_start.elapsed() < budget;
        if !do_noise {
            noise_partial = true;
        }
        let prof: HashMap<usize, noise::Profile> = if do_noise {
            ids.iter()
                .take(8)
                .filter_map(|&i| noise::analyze_secs(&sigs[i].path, window).map(|p| (i, p)))
                .collect()
        } else {
            HashMap::new()
        };
        decoded += prof.len();
        if !prof.is_empty() {
            noise_groups += 1;
        }
        let bestp = prof
            .values()
            .max_by(|a, b| a.quality().total_cmp(&b.quality()));
        let qmax = bestp.map(|p| p.quality()).unwrap_or(f32::MIN);

        let mut items: Vec<(usize, Item)> = ids
            .iter()
            .map(|&i| {
                let s = &sigs[i];
                let (mut score, mut reasons) = audio_score(s, keys[i].label);
                let mut nz = None;
                let mut sim = None;
                let mut hint = noise_label(&keys[i].raw).is_some();
                if let Some(p) = prof.get(&i) {
                    score += (((p.quality() - qmax) as f64) * 0.5).clamp(-12.0, 0.0);
                    let rel_art = bestp.is_some_and(|bp| !std::ptr::eq(p, bp) && p.artifacts_vs(bp));
                    let rel_dull = bestp.is_some_and(|bp| !std::ptr::eq(p, bp) && p.dull_vs(bp));
                    if rel_art {
                        score -= 8.0;
                    }
                    if rel_dull {
                        score -= 4.0;
                    }
                    if let Some(bp) = bestp {
                        sim = Some(noise::similarity_pct(bp, p));
                    }
                    reasons.push(format!("ruido: {}", p.summary()));
                    hint |= p.denoised_hint();
                    if hint {
                        reasons.push("parece denoise".into());
                    }
                    if rel_art {
                        reasons.push("posible ruido musical del denoise".into());
                    }
                    if rel_dull {
                        reasons.push("agudos apagados vs la otra copia (sobre-denoise)".into());
                    }
                    nz = Some(json!({
                        "snr_db": r1(p.snr_db),
                        "noise_floor_db": r1(p.noise_floor_db),
                        "signal_db": r1(p.signal_db),
                        "silence_ratio": r1(p.silence_ratio * 100.0) / 100.0,
                        "digital_silence": r1(p.digital_silence * 100.0) / 100.0,
                        "hf_quiet_db": r1(p.hf_quiet_db),
                        "hf_loud_db": r1(p.hf_loud_db),
                        "flatness_quiet": r1(p.flatness_quiet * 1000.0) / 1000.0,
                        "flux_quiet": r1(p.flux_quiet * 1000.0) / 1000.0,
                        "crest_db": r1(p.crest_db),
                        "clipped": r1(p.clipped * 1000.0) / 1000.0,
                        "denoised": hint,
                        "artifacts": rel_art,
                        "dull": rel_dull,
                        "seconds": r1(p.seconds),
                        "summary": p.summary(),
                    }));
                }
                (
                    i,
                    Item {
                        path: s.path.to_string_lossy().to_string(),
                        bytes: s.bytes,
                        score,
                        reasons,
                        ai: Vec::new(),
                        camera: None,
                        captured: None,
                        noise: nz,
                        similarity: sim,
                    },
                )
            })
            .collect();
        items.sort_by(|a, b| b.1.score.total_cmp(&a.1.score).then(b.1.bytes.cmp(&a.1.bytes)));
        let (keep_idx, mut keeper) = items.remove(0);
        keeper.similarity = Some(100.0);

        // Explain the noise verdict against the best remaining copy.
        if let Some(kp) = prof.get(&keep_idx)
            && let Some((op, oname)) = items
                .iter()
                .find_map(|(j, it)| prof.get(j).map(|p| (p, short_name(&it.path))))
        {
            let kname = short_name(&keeper.path);
            let (ord, notes) = noise::compare(kp, op, &kname, &oname);
            if !notes.is_empty() {
                let mut seen: Vec<&str> = Vec::new();
                let mut unique: Vec<&str> = Vec::new();
                for n in &notes {
                    if !seen.contains(&n.as_str()) {
                        seen.push(n.as_str());
                        unique.push(n.as_str());
                    }
                }
                let mut text = format!(
                    "Similitud acústica {:.0}%. {}",
                    noise::similarity_pct(kp, op),
                    unique.join(" ")
                );
                if ord == std::cmp::Ordering::Less {
                    text = format!("Ojo: la copia conservada no es la de mejor ruido. {text}");
                }
                note = Some(match note {
                    Some(n) => format!("{n} {text}"),
                    None => text,
                });
            }
        }

        let r: u64 = items.iter().map(|(_, o)| o.bytes).sum();
        reclaim += r;
        let others: Vec<Item> = items.into_iter().map(|(_, i)| i).collect();
        out_groups.push(Group {
            key: keys[keep_idx].raw.clone(),
            note,
            keeper,
            others,
            reclaim: r,
            ai_modified: false,
        });
    }
    out_groups.sort_by(|a, b| b.reclaim.cmp(&a.reclaim));

    let mut notes = Vec::new();
    if total > files.len() {
        notes.push(format!("Se analizaron los {} audios más grandes de {total}.", files.len()));
    }
    notes.push("Agrupación difusa por tokens del título (o nombre de archivo) + duración; no exige etiquetas iguales.".into());
    notes.push("Puntaje = sin pérdida > bitrate > sample rate > bit depth > estéreo > carátula/álbum; en vivo/TV resta.".into());
    notes.push(format!("Se omiten audios de menos de {MIN_TRACK_SECS:.0} s (sonidos de sistema)."));
    if cloud > 0 {
        notes.push(format!(
            "{cloud} audios son placeholders de nube y no se abrieron (no se descargaron)."
        ));
    }
    if decoded > 0 {
        notes.push(format!(
            "Análisis de sonido: {noise_groups}/{group_count} grupos ({decoded} archivos) en {} ms, con ventanas de hasta {window:.0} s. El resto usa metadatos y porcentajes para no frenar.",
            scan_start.elapsed().as_millis()
        ));
    } else {
        notes.push("No se decodificó audio (WMA/APE/WV no soportados o presupuesto de tiempo): el puntaje usa solo metadatos.".into());
    }
    if meta_partial {
        notes.push("Presupuesto de tiempo alcanzado leyendo etiquetas: se analizaron sólo los primeros audios.".into());
    }
    if noise_partial {
        notes.push("Presupuesto de tiempo alcanzado: el análisis acústico es parcial; los porcentajes lo indican.".into());
    }
    Report {
        kind: "audio",
        files: total,
        analyzed: sigs.len(),
        skipped,
        groups: out_groups,
        reclaim,
        alternatives: vec![
            "Conservar el álbum (mejor master) y mover la grabación de TV/YouTube a _Cuarentena.",
            "Si la versión en vivo te interesa, conservar ambas: son grabaciones distintas.",
            "Conservar el archivo sin pérdida y generar un 320 kbps para el teléfono.",
            "Archivar los lossy en otro disco en vez de borrarlos.",
            "Completar etiquetas faltantes antes de decidir.",
            "Comparar a oído la copia denoised y la original: el perfil de ruido es una guía, no una verdad absoluta.",
        ],
        notes,
        ai_images: Vec::new(),
        elapsed_ms: scan_start.elapsed().as_millis() as u64,
        partial: noise_partial || meta_partial,
    }
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
            exif: ExifInfo::default(),
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
    fn image_similarity_uses_the_hashes() {
        let a = sig(0b1010, 0b0101, 4000, 3000);
        let b = sig(0b1010, 0b0101, 4000, 3000);
        let c = sig(0b1010 ^ 0xFFFF, 0b0101 ^ 0xFFFF, 4000, 3000);
        assert!((image_similarity(&a, &b) - 100.0).abs() < 0.01);
        assert!(image_similarity(&a, &c) < 80.0);
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

    fn aud(
        path: &str,
        artist: Option<&str>,
        title: Option<&str>,
        album: Option<&str>,
        duration: f64,
        bitrate: u32,
        lossless: bool,
    ) -> AudSig {
        AudSig {
            path: PathBuf::from(path),
            bytes: (bitrate as u64) * 1000 / 8 * duration as u64,
            artist: artist.map(|s| s.to_string()),
            title: title.map(|s| s.to_string()),
            album: album.map(|s| s.to_string()),
            duration,
            bitrate: Some(bitrate),
            sample_rate: Some(44100),
            bit_depth: lossless.then_some(16),
            channels: Some(2),
            lossless,
            cover: false,
            fmt: if lossless { "FLAC" } else { "MP3" },
        }
    }

    #[test]
    fn lossless_beats_mp3() {
        let flac = aud("a.flac", Some("A"), Some("T"), Some("X"), 200.0, 900, true);
        let mp3 = aud("a.mp3", Some("A"), Some("T"), None, 200.0, 320, false);
        assert!(audio_score(&flac, "album").0 > audio_score(&mp3, "unknown").0);
    }

    #[test]
    fn dice_similarity() {
        let a = vec!["artist".to_string(), "song".to_string()];
        let b = vec!["artist".to_string(), "song".to_string()];
        assert_eq!(dice(&a, &b).0, 1.0);
        let c = vec!["artist".to_string(), "other".to_string()];
        assert!(dice(&a, &c).0 < 0.62);
    }

    #[test]
    fn version_labels() {
        assert_eq!(version_label("Artist - Song (Live on TV)"), Some("live"));
        assert_eq!(version_label("Song (Remix)"), Some("remix"));
        assert_eq!(version_label("Song (Acústico)"), Some("acoustic"));
        assert_eq!(version_label("Artist - Song"), None);
    }

    #[test]
    fn duration_tolerance() {
        let a = aud("a.flac", Some("A"), Some("T"), Some("X"), 225.0, 900, true);
        let b = aud("b.mp3", None, None, None, 250.0, 128, false);
        let c = aud("c.mp3", None, None, None, 420.0, 128, false);
        assert!(duration_ok(&a, &b));
        assert!(!duration_ok(&a, &c));
    }

    #[test]
    fn fuzzy_groups_album_and_tv_rip() {
        let album = aud(
            "C:\\Music\\Album\\01 Song.flac",
            Some("Artist"),
            Some("Song"),
            Some("Album"),
            225.0,
            900,
            true,
        );
        let tv = aud(
            "C:\\Downloads\\Artist - Song (Live on TV).mp3",
            None,
            None,
            None,
            250.0,
            128,
            false,
        );
        let (groups, keys) = group_audio(&[album, tv]);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].len(), 2);
        assert_eq!(keys[0].label, "album");
        assert_eq!(keys[1].label, "live");
    }

    #[test]
    fn different_songs_are_not_grouped() {
        let a = aud("a.flac", Some("Artist"), Some("Alpha"), Some("X"), 200.0, 900, true);
        let b = aud("b.flac", Some("Artist"), Some("Gamma"), Some("X"), 200.0, 900, true);
        let (groups, _) = group_audio(&[a, b]);
        assert!(groups.is_empty());
    }

    #[test]
    fn album_beats_tv_recording() {
        let album = aud("a.flac", Some("A"), Some("T"), Some("X"), 225.0, 900, true);
        let tv = aud("b.mp3", Some("A"), Some("T (Live)"), None, 250.0, 192, false);
        assert!(audio_score(&album, "album").0 > audio_score(&tv, "live").0);
    }

    fn write_wav(path: &Path, rate: u32, secs: u32) {
        let data = rate * secs * 2; // 16-bit mono
        let mut v = Vec::with_capacity(44 + data as usize);
        v.extend_from_slice(b"RIFF");
        v.extend_from_slice(&(36 + data).to_le_bytes());
        v.extend_from_slice(b"WAVEfmt ");
        v.extend_from_slice(&16u32.to_le_bytes());
        v.extend_from_slice(&1u16.to_le_bytes());
        v.extend_from_slice(&1u16.to_le_bytes());
        v.extend_from_slice(&rate.to_le_bytes());
        v.extend_from_slice(&(rate * 2).to_le_bytes());
        v.extend_from_slice(&2u16.to_le_bytes());
        v.extend_from_slice(&16u16.to_le_bytes());
        v.extend_from_slice(b"data");
        v.extend_from_slice(&data.to_le_bytes());
        v.resize(44 + data as usize, 0);
        std::fs::write(path, &v).unwrap();
    }

    #[test]
    fn scan_audio_groups_album_and_tv_wav() {
        let dir = std::env::temp_dir().join(format!("wize-audio-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let album = dir.join("Artist - Song.wav");
        let tv = dir.join("Artist - Song (Live on TV).wav");
        write_wav(&album, 44100, 120);
        write_wav(&tv, 8000, 125);
        let fi = |p: &Path| FileInfo {
            path: p.to_path_buf(),
            bytes: std::fs::metadata(p).unwrap().len(),
        };
        let report = scan_audio(vec![fi(&album), fi(&tv)], 10, false);
        assert_eq!(report.groups.len(), 1);
        assert_eq!(report.groups[0].others.len(), 1);
        assert!(report.groups[0].keeper.path.ends_with("Artist - Song.wav"));
        std::fs::remove_dir_all(&dir).ok();
    }
    fn unit_noise(seed: &mut u64) -> f32 {
        *seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        ((*seed >> 33) as f32 / (1u64 << 31) as f32) * 2.0 - 1.0
    }

    /// 16-bit mono WAV: 440 Hz tone plus a controlled amount of broadband noise.
    fn write_tone_wav(path: &Path, rate: u32, secs: u32, noise: f32) {
        let n = rate * secs;
        let data = n * 2;
        let mut v = Vec::with_capacity(44 + data as usize);
        v.extend_from_slice(b"RIFF");
        v.extend_from_slice(&(36 + data).to_le_bytes());
        v.extend_from_slice(b"WAVEfmt ");
        v.extend_from_slice(&16u32.to_le_bytes());
        v.extend_from_slice(&1u16.to_le_bytes());
        v.extend_from_slice(&1u16.to_le_bytes());
        v.extend_from_slice(&rate.to_le_bytes());
        v.extend_from_slice(&(rate * 2).to_le_bytes());
        v.extend_from_slice(&2u16.to_le_bytes());
        v.extend_from_slice(&16u16.to_le_bytes());
        v.extend_from_slice(b"data");
        v.extend_from_slice(&data.to_le_bytes());
        let mut seed = 11u64;
        for i in 0..n {
            let t = i as f32 / rate as f32;
            let x = (0.3 * (2.0 * std::f32::consts::PI * 440.0 * t).sin() + noise * unit_noise(&mut seed))
                .clamp(-1.0, 1.0);
            v.extend_from_slice(&((x * 32767.0) as i16).to_le_bytes());
        }
        std::fs::write(path, &v).unwrap();
    }

    #[test]
    fn denoise_markers_are_recognized_and_ignored_for_grouping() {
        assert_eq!(noise_label("Artist - Song (denoised)"), Some("denoised"));
        assert_eq!(noise_label("Song (noise reduced)"), Some("denoised"));
        assert_eq!(noise_label("Artist - Song"), None);
        let plain = aud("a.wav", Some("A"), Some("Song"), Some("X"), 200.0, 900, true);
        let den = aud("b.wav", Some("A"), Some("Song (denoised)"), Some("X"), 200.0, 900, true);
        let (groups, _) = group_audio(&[plain, den]);
        assert_eq!(groups.len(), 1);
    }

    #[test]
    fn scan_audio_ranks_the_denoised_copy_as_cleaner() {
        let dir = std::env::temp_dir().join(format!("wize-denoise-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let original = dir.join("Artist - Song.wav");
        let denoised = dir.join("Artist - Song (denoised).wav");
        write_tone_wav(&original, 44100, 12, 0.06);
        write_tone_wav(&denoised, 44100, 12, 0.004);
        let fi = |p: &Path| FileInfo {
            path: p.to_path_buf(),
            bytes: std::fs::metadata(p).unwrap().len(),
        };
        let report = scan_audio(vec![fi(&original), fi(&denoised)], 10, false);
        assert_eq!(report.groups.len(), 1);
        let g = &report.groups[0];
        assert!(
            g.keeper.path.ends_with("(denoised).wav"),
            "keeper={}",
            g.keeper.path
        );
        let n = g.keeper.noise.as_ref().expect("profile on keeper");
        assert!(n["snr_db"].as_f64().unwrap() > 25.0, "snr={}", n["snr_db"]);
        let note = g.note.as_deref().unwrap_or("");
        assert!(note.contains("ruido"), "note={note}");
        std::fs::remove_dir_all(&dir).ok();
    }
}
