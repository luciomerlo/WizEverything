//! Discos guardados: registro de unidades que se conectan y desaparecen.
//!
//! Mientras un disco está conectado se guarda lo que Windows y el índice de
//! Everything permiten saber de él: etiqueta, marca, modelo, número de serie,
//! bus, tamaño, y un inventario de rutas con tamaños y fechas. Cuando el disco
//! desaparece, ese inventario queda disponible como una unidad más ("offline"),
//! con la fecha de la última actualización a la vista.
//!
//! Dos modos de retención:
//!
//! - [`Retention::Full`] — todas las rutas + tamaños + fechas + huella rápida.
//!   Permite navegar el disco guardado, analizar carpetas y buscar duplicados
//!   contra los discos conectados.
//! - [`Retention::Digest`] — sólo totales, carpetas principales, tipos de
//!   archivo y los archivos más grandes. Ocupa poco y sirve para recordar qué
//!   había, pero no para navegar ni comparar archivo por archivo.
//!
//! La huella (`semihash`) nunca es un hash completo: lee 8 KiB (rápido) o
//! 48 KiB (profundo) por archivo —cabeza, medio y cola— y mezcla el tamaño.
//! Alcanza para reconocer la misma copia renombrada sin leer el disco entero.
//! Se calcula con presupuesto y cobertura parcial declarada.

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use crate::tree::Tree;

const MAGIC: &[u8; 8] = b"WZVOL1\0\0";
const ZSTD_LEVEL: i32 = 12;
const MAX_DIGEST_FOLDERS: usize = 2_000;
const MAX_DIGEST_FILES: usize = 5_000;

/// Cómo se guarda un disco cuando se desconecta.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Retention {
    /// Inventario completo de rutas (navegable y comparable).
    Full,
    /// Sólo resumen: totales, carpetas, tipos y archivos más grandes.
    Digest,
}

impl Retention {
    pub fn as_str(self) -> &'static str {
        match self {
            Retention::Full => "full",
            Retention::Digest => "digest",
        }
    }

    pub fn parse(s: &str) -> Retention {
        if s.eq_ignore_ascii_case("digest") || s.eq_ignore_ascii_case("resumen") {
            Retention::Digest
        } else {
            Retention::Full
        }
    }

    pub fn label(self) -> (&'static str, &'static str) {
        match self {
            Retention::Full => ("Completa", "Full"),
            Retention::Digest => ("Resumen", "Digest"),
        }
    }
}

/// Una fila del inventario guardado.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct VolEntry {
    /// Ruta relativa a la raíz del disco (`Users\ana\foto.jpg`).
    pub rel: String,
    pub is_dir: bool,
    pub size: u64,
    pub modified: i64,
    /// Huella de muestra; 0 = no calculada para este archivo.
    pub semihash: u64,
}

/// Metadatos + inventario de un disco guardado.
#[derive(Clone, Debug, Default)]
pub struct VolumeRecord {
    pub id: String,
    /// Nombre del nodo cuando se muestra como unidad: `E: (guardado)`.
    pub name: String,
    pub letter: String,
    pub label: String,
    pub fs: String,
    pub volume_serial: u64,
    pub vendor: String,
    pub model: String,
    pub serial_number: String,
    pub bus: String,
    pub removable: bool,
    pub total_bytes: u64,
    pub free_bytes: u64,
    pub first_seen: i64,
    pub last_seen: i64,
    /// Última vez que se guardó/actualizó el inventario.
    pub updated: i64,
    pub retention: Retention,
    /// Mostrar como unidad aunque esté desconectado.
    pub enabled: bool,
    pub entries: Vec<VolEntry>,
    /// Resumen por carpeta (sólo digest).
    pub folders: Vec<(String, u64)>,
    /// Resumen por extensión (sólo digest): (ext, bytes, count).
    pub by_ext: Vec<(String, u64, u64)>,
    pub file_count: u64,
    pub dir_count: u64,
    pub total_logical: u64,
    /// Archivos con huella calculada y si quedó trabajo pendiente.
    pub hashed: u64,
    pub hash_partial: bool,
    pub elapsed_ms: u64,
    pub note: String,
}

impl Default for Retention {
    fn default() -> Self {
        Retention::Full
    }
}

impl VolumeRecord {
    /// Etiqueta legible para la UI y el MCP.
    pub fn display(&self) -> String {
        let mut s = if self.label.is_empty() { self.letter.clone() } else { self.label.clone() };
        if !self.vendor.is_empty() {
            s.push_str(&format!(" · {}", self.vendor));
        }
        if !self.model.is_empty() {
            s.push_str(&format!(" {}", self.model));
        }
        if !self.serial_number.is_empty() {
            s.push_str(&format!(" · S/N {}", self.serial_number));
        }
        s
    }

    pub fn semihash_pct(&self) -> f64 {
        if self.file_count == 0 {
            return 0.0;
        }
        (self.hashed as f64 * 100.0 / self.file_count as f64).min(100.0)
    }

    pub fn to_json(&self, with_entries: bool) -> serde_json::Value {
        let mut v = serde_json::json!({
            "id": self.id,
            "name": self.name,
            "letter": self.letter,
            "label": self.label,
            "fs": self.fs,
            "volume_serial": self.volume_serial,
            "vendor": self.vendor,
            "model": self.model,
            "serial_number": self.serial_number,
            "bus": self.bus,
            "removable": self.removable,
            "total_bytes": self.total_bytes,
            "free_bytes": self.free_bytes,
            "first_seen": self.first_seen,
            "last_seen": self.last_seen,
            "updated": self.updated,
            "updated_date": fmt_date(self.updated),
            "retention": self.retention.as_str(),
            "enabled": self.enabled,
            "file_count": self.file_count,
            "dir_count": self.dir_count,
            "total_logical": self.total_logical,
            "hashed": self.hashed,
            "semihash_pct": (self.semihash_pct() * 10.0).round() / 10.0,
            "hash_partial": self.hash_partial,
            "elapsed_ms": self.elapsed_ms,
            "note": self.note,
            "display": self.display(),
        });
        if !self.folders.is_empty() {
            v["folders"] = serde_json::json!(self.folders);
        }
        if !self.by_ext.is_empty() {
            v["by_ext"] = serde_json::json!(self.by_ext);
        }
        if with_entries {
            v["entries"] = serde_json::json!(self.entries.len());
        }
        v
    }
}

// ---------------------------------------------------------------- fechas

/// Convierte segundos unix a fecha local `YYYY-MM-DD` (o `—` si no hay dato).
pub fn fmt_date(unix: i64) -> String {
    if unix <= 0 {
        return "—".into();
    }
    #[cfg(windows)]
    if let Some(s) = windows_local_date(unix) {
        return s;
    }
    let (y, m, d) = civil_from_days(unix.div_euclid(86_400));
    format!("{y:04}-{m:02}-{d:02}")
}

#[cfg(windows)]
fn windows_local_date(unix: i64) -> Option<String> {
    use windows_sys::Win32::Foundation::SYSTEMTIME;
    use windows_sys::Win32::System::Time::{FileTimeToSystemTime, SystemTimeToTzSpecificLocalTime};
    let ft = (unix as u64 + 11_644_473_600) * 10_000_000;
    let file = windows_sys::Win32::Foundation::FILETIME {
        dwLowDateTime: (ft & 0xffff_ffff) as u32,
        dwHighDateTime: (ft >> 32) as u32,
    };
    let mut utc: SYSTEMTIME = unsafe { std::mem::zeroed() };
    if unsafe { FileTimeToSystemTime(&file, &mut utc) } == 0 {
        return None;
    }
    let mut local: SYSTEMTIME = unsafe { std::mem::zeroed() };
    if unsafe { SystemTimeToTzSpecificLocalTime(std::ptr::null(), &utc, &mut local) } == 0 {
        return None;
    }
    Some(format!("{:04}-{:02}-{:02}", local.wYear, local.wMonth, local.wDay))
}

/// Howard Hinnant's civil_from_days, sin dependencias.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = mp + if mp < 10 { 3 } else { -9 };
    (y + i64::from(m <= 2), m as u32, d as u32)
}

pub fn now_unix() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64)
}

// ---------------------------------------------------------------- huella rápida

/// Huella de muestra: tamaño + cabeza + (medio) + cola. Nunca lee el archivo
/// completo. `deep` sube de 8 KiB a 48 KiB por archivo.
pub fn semihash(path: &Path, size: u64, deep: bool) -> Option<u64> {
    if size == 0 {
        return None;
    }
    let (chunk, mid) = if deep { (16 * 1024u64, true) } else { (8 * 1024u64, false) };
    let mut f = File::open(path).ok()?;
    let mut h = xxhash_rust::xxh3::Xxh3::new();
    h.update(&size.to_le_bytes());
    let mut buf = Vec::with_capacity(chunk as usize);
    read_at(&mut f, 0, chunk.min(size), &mut buf)?;
    h.update(&buf);
    if size > chunk {
        if mid && size > chunk * 2 {
            buf.clear();
            read_at(&mut f, size / 2 - chunk / 2, chunk, &mut buf)?;
            h.update(&buf);
        }
        buf.clear();
        read_at(&mut f, size.saturating_sub(chunk), chunk, &mut buf)?;
        h.update(&buf);
    }
    Some(h.digest())
}

fn read_at(f: &mut File, off: u64, len: u64, out: &mut Vec<u8>) -> Option<()> {
    f.seek(SeekFrom::Start(off)).ok()?;
    f.take(len).read_to_end(out).ok()?;
    Some(())
}

// ---------------------------------------------------------------- captura

#[derive(Clone, Debug)]
pub struct CaptureOpts {
    pub retention: Retention,
    /// Huella de 48 KiB por archivo (si no, 8 KiB).
    pub deep: bool,
    /// Tiempo máximo para calcular huellas (el inventario se guarda igual).
    pub budget_ms: u128,
    /// No calcular huella a archivos menores a esto (rápido).
    pub min_hash_bytes: u64,
    /// Tope de archivos a los que se les calcula huella.
    pub max_hash_files: usize,
}

impl CaptureOpts {
    pub fn fast() -> Self {
        CaptureOpts { retention: Retention::Full, deep: false, budget_ms: 4_000, min_hash_bytes: 64 * 1024, max_hash_files: 20_000 }
    }
    pub fn deep() -> Self {
        CaptureOpts { retention: Retention::Full, deep: true, budget_ms: 60_000, min_hash_bytes: 0, max_hash_files: 200_000 }
    }
}

#[derive(Default)]
pub struct CaptureProgress {
    pub files: std::sync::atomic::AtomicU64,
    pub hashed: std::sync::atomic::AtomicU64,
    pub total: std::sync::atomic::AtomicU64,
}

/// Guarda lo que se puede del disco `drive` del índice actual.
pub fn capture(t: &Tree, drive: u32, opts: &CaptureOpts, progress: &CaptureProgress) -> Result<VolumeRecord, String> {
    use std::sync::atomic::Ordering;
    let started = Instant::now();
    let letter = t.name(drive).to_string();
    let ident = crate::volinfo::query(&letter);
    let disk = crate::tree::disk_space(&letter).unwrap_or((0, 0));
    let now = now_unix();
    let mut rec = VolumeRecord {
        letter: letter.clone(),
        label: ident.label.clone(),
        fs: ident.fs.clone(),
        volume_serial: ident.volume_serial,
        vendor: ident.vendor.clone(),
        model: ident.model.clone(),
        serial_number: ident.serial_number.clone(),
        bus: ident.bus.clone(),
        removable: ident.removable,
        total_bytes: disk.0,
        free_bytes: disk.1,
        first_seen: now,
        last_seen: now,
        updated: now,
        retention: opts.retention,
        enabled: true,
        ..Default::default()
    };
    rec.id = make_id(rec.volume_serial, &rec.label, &rec.model, rec.total_bytes);
    rec.name = offline_name(&letter);
    rec.note = if ident.vendor.is_empty() && ident.model.is_empty() {
        "El disco no reportó marca/modelo; se identifica por etiqueta y serial de volumen.".into()
    } else {
        String::new()
    };

    let prefix = format!("{}\\", letter.trim_end_matches('\\'));
    let mut entries: Vec<VolEntry> = Vec::new();
    let mut folders: HashMap<String, u64> = HashMap::new();
    let mut by_ext: HashMap<String, (u64, u64)> = HashMap::new();

    let mut stack = vec![drive];
    while let Some(id) = stack.pop() {
        for &c in t.children(id) {
            let full = t.full_path(c);
            let rel = full.strip_prefix(&prefix).unwrap_or(&full).to_string();
            let n = &t.nodes[c as usize];
            if n.is_dir() {
                stack.push(c);
                entries.push(VolEntry { rel: rel.clone(), is_dir: true, size: 0, modified: n.modified as i64, semihash: 0 });
                if opts.retention == Retention::Digest {
                    *folders.entry(rel).or_default() += 0;
                }
            } else {
                let ext = t.exts.get(n.ext as usize).map(|e| e.name.clone()).unwrap_or_default();
                let e = by_ext.entry(ext).or_default();
                e.0 += n.size;
                e.1 += 1;
                if opts.retention == Retention::Digest {
                    let dir = rel.rsplit_once('\\').map(|(d, _)| d.to_string()).unwrap_or_default();
                    *folders.entry(dir).or_default() += n.size;
                }
                entries.push(VolEntry { rel, is_dir: false, size: n.size, modified: n.modified as i64, semihash: 0 });
            }
        }
    }
    progress.files.store(entries.len() as u64, Ordering::Relaxed);
    let total_files = entries.iter().filter(|e| !e.is_dir).count() as u64;
    rec.file_count = total_files;
    rec.dir_count = entries.len() as u64 - total_files;
    rec.total_logical = t.nodes[drive as usize].size;
    progress.total.store(total_files, Ordering::Relaxed);

    // Huellas: primero los archivos más grandes, que son los que importan para duplicados.
    let mut order: Vec<usize> = entries
        .iter()
        .enumerate()
        .filter(|(_, e)| !e.is_dir && e.size >= opts.min_hash_bytes && e.size > 0)
        .map(|(i, _)| i)
        .collect();
    order.sort_by(|&a, &b| entries[b].size.cmp(&entries[a].size));
    order.truncate(opts.max_hash_files);
    let max_hash = order.len() as u64;
    let mut hashed = 0u64;
    for i in order {
        if started.elapsed().as_millis() > opts.budget_ms {
            rec.hash_partial = true;
            break;
        }
        let path = format!("{}{}", prefix, entries[i].rel);
        if let Some(h) = semihash(Path::new(&path), entries[i].size, opts.deep) {
            entries[i].semihash = h;
            hashed += 1;
            progress.hashed.store(hashed, Ordering::Relaxed);
        }
    }
    rec.hashed = hashed;
    if hashed < max_hash && !rec.hash_partial {
        // Algunos archivos no se pudieron abrir, pero no fue por presupuesto.
        rec.hash_partial = hashed < max_hash;
    }

    if opts.retention == Retention::Digest {
        let mut f: Vec<(String, u64)> = folders.into_iter().collect();
        f.sort_by(|a, b| b.1.cmp(&a.1));
        f.truncate(MAX_DIGEST_FOLDERS);
        rec.folders = f;
        let mut e: Vec<(String, u64, u64)> = by_ext.into_iter().map(|(k, (b, c))| (k, b, c)).collect();
        e.sort_by(|a, b| b.1.cmp(&a.1));
        rec.by_ext = e;
        entries.retain(|x| !x.is_dir);
        entries.sort_by(|a, b| b.size.cmp(&a.size));
        entries.truncate(MAX_DIGEST_FILES);
        rec.entries = entries;
    } else {
        rec.entries = entries;
    }
    rec.elapsed_ms = started.elapsed().as_millis() as u64;
    Ok(rec)
}

fn offline_name(letter: &str) -> String {
    let l = letter.trim_end_matches('\\');
    if l.is_empty() { "Disco guardado".into() } else { format!("{l} (guardado)") }
}

fn make_id(serial: u64, label: &str, model: &str, total: u64) -> String {
    if serial != 0 {
        return format!("vol-{serial:08x}");
    }
    let key = format!("{label}|{model}|{total}");
    format!("vol-{:016x}", xxhash_rust::xxh3::xxh3_64(key.as_bytes()))
}

// ---------------------------------------------------------------- almacenamiento

/// Carpeta de discos guardados: `%LOCALAPPDATA%\WizEverything\volumes`.
#[derive(Clone, Debug)]
pub struct Store {
    dir: PathBuf,
}

impl Default for Store {
    fn default() -> Self {
        let base = std::env::var_os("LOCALAPPDATA")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        Store { dir: base.join("WizEverything").join("volumes") }
    }
}

impl Store {
    pub fn with_dir(dir: PathBuf) -> Self {
        Store { dir }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    fn path_of(&self, id: &str) -> PathBuf {
        self.dir.join(format!("{id}.wzvol"))
    }

    /// Lee sólo las cabeceras: rápido para listar sin cargar inventarios.
    pub fn scan(&self) -> Vec<VolumeRecord> {
        let Ok(rd) = std::fs::read_dir(&self.dir) else { return Vec::new() };
        let mut out = Vec::new();
        for e in rd.flatten() {
            let p = e.path();
            if p.extension().and_then(|x| x.to_str()) != Some("wzvol") {
                continue;
            }
            if let Ok(rec) = read_header(&p) {
                out.push(rec);
            }
        }
        out.sort_by(|a, b| b.updated.cmp(&a.updated));
        out
    }

    /// Carga un disco completo (inventario incluido).
    pub fn load(&self, id: &str) -> Result<VolumeRecord, String> {
        let p = self.path_of(id);
        let bytes = std::fs::read(&p).map_err(|e| format!("{}: {e}", p.display()))?;
        decode(&bytes)
    }

    /// Guarda el registro; conserva `first_seen` si ya existía.
    pub fn save(&self, mut rec: VolumeRecord) -> Result<(), String> {
        std::fs::create_dir_all(&self.dir).map_err(|e| e.to_string())?;
        if let Ok(old) = read_header(&self.path_of(&rec.id))
            && old.first_seen > 0
        {
            rec.first_seen = old.first_seen;
        }
        let bytes = encode(&rec)?;
        let path = self.path_of(&rec.id);
        let tmp = path.with_extension("tmp");
        {
            let mut f = OpenOptions::new().create(true).write(true).truncate(true).open(&tmp).map_err(|e| e.to_string())?;
            f.write_all(&bytes).map_err(|e| e.to_string())?;
            f.sync_all().ok();
        }
        std::fs::rename(&tmp, &path).map_err(|e| e.to_string())
    }

    /// Borra el inventario guardado de un disco. No toca el disco real.
    pub fn delete(&self, id: &str) -> Result<(), String> {
        let p = self.path_of(id);
        if p.exists() { std::fs::remove_file(&p).map_err(|e| e.to_string()) } else { Ok(()) }
    }

    /// Cambia sólo metadatos (por ejemplo `enabled`) sin reescribir entradas.
    pub fn set_enabled(&self, id: &str, enabled: bool) -> Result<(), String> {
        let mut rec = self.load(id)?;
        rec.enabled = enabled;
        self.save(rec)
    }

    pub fn enabled_full(&self) -> Vec<VolumeRecord> {
        self.scan()
            .into_iter()
            .filter(|r| r.enabled && r.retention == Retention::Full)
            .filter_map(|r| self.load(&r.id).ok())
            .collect()
    }
}

fn encode(rec: &VolumeRecord) -> Result<Vec<u8>, String> {
    let header = serde_json::json!({
        "version": 1,
        "id": rec.id,
        "name": rec.name,
        "letter": rec.letter,
        "label": rec.label,
        "fs": rec.fs,
        "volume_serial": rec.volume_serial,
        "vendor": rec.vendor,
        "model": rec.model,
        "serial_number": rec.serial_number,
        "bus": rec.bus,
        "removable": rec.removable,
        "total_bytes": rec.total_bytes,
        "free_bytes": rec.free_bytes,
        "first_seen": rec.first_seen,
        "last_seen": rec.last_seen,
        "updated": rec.updated,
        "retention": rec.retention.as_str(),
        "enabled": rec.enabled,
        "file_count": rec.file_count,
        "dir_count": rec.dir_count,
        "total_logical": rec.total_logical,
        "hashed": rec.hashed,
        "hash_partial": rec.hash_partial,
        "elapsed_ms": rec.elapsed_ms,
        "note": rec.note,
        "folders": rec.folders,
        "by_ext": rec.by_ext,
    });
    let hdr = serde_json::to_vec(&header).map_err(|e| e.to_string())?;
    let mut payload = Vec::new();
    payload.extend_from_slice(&(rec.entries.len() as u32).to_le_bytes());
    for e in &rec.entries {
        payload.push(u8::from(e.is_dir));
        payload.extend_from_slice(&e.size.to_le_bytes());
        payload.extend_from_slice(&e.modified.to_le_bytes());
        payload.extend_from_slice(&e.semihash.to_le_bytes());
        let b = e.rel.as_bytes();
        payload.extend_from_slice(&(b.len() as u32).to_le_bytes());
        payload.extend_from_slice(b);
    }
    let compressed = zstd::encode_all(payload.as_slice(), ZSTD_LEVEL).map_err(|e| e.to_string())?;
    let mut out = Vec::with_capacity(16 + hdr.len() + compressed.len());
    out.extend_from_slice(MAGIC);
    out.extend_from_slice(&(hdr.len() as u32).to_le_bytes());
    out.extend_from_slice(&(compressed.len() as u32).to_le_bytes());
    out.extend_from_slice(&hdr);
    out.extend_from_slice(&compressed);
    Ok(out)
}

fn split(bytes: &[u8]) -> Result<(&[u8], &[u8]), String> {
    if bytes.len() < 16 || &bytes[..8] != MAGIC {
        return Err("archivo de disco guardado inválido".into());
    }
    let hlen = u32::from_le_bytes(bytes[8..12].try_into().unwrap()) as usize;
    let plen = u32::from_le_bytes(bytes[12..16].try_into().unwrap()) as usize;
    if 16 + hlen + plen > bytes.len() {
        return Err("archivo de disco guardado truncado".into());
    }
    Ok((&bytes[16..16 + hlen], &bytes[16 + hlen..16 + hlen + plen]))
}

fn read_header(path: &Path) -> Result<VolumeRecord, String> {
    let mut f = File::open(path).map_err(|e| e.to_string())?;
    let mut head = [0u8; 16];
    f.read_exact(&mut head).map_err(|e| e.to_string())?;
    if &head[..8] != MAGIC {
        return Err("magic inválido".into());
    }
    let hlen = u32::from_le_bytes(head[8..12].try_into().unwrap()) as usize;
    let mut hdr = vec![0u8; hlen];
    f.read_exact(&mut hdr).map_err(|e| e.to_string())?;
    let v: serde_json::Value = serde_json::from_slice(&hdr).map_err(|e| e.to_string())?;
    Ok(from_header(&v))
}

fn decode(bytes: &[u8]) -> Result<VolumeRecord, String> {
    let (hdr, payload) = split(bytes)?;
    let v: serde_json::Value = serde_json::from_slice(hdr).map_err(|e| e.to_string())?;
    let mut rec = from_header(&v);
    let raw = zstd::decode_all(payload).map_err(|e| e.to_string())?;
    let mut p = 0usize;
    let rd_u32 = |p: &mut usize| -> Result<u32, String> {
        if *p + 4 > raw.len() { return Err("payload truncado".into()); }
        let v = u32::from_le_bytes(raw[*p..*p + 4].try_into().unwrap());
        *p += 4;
        Ok(v)
    };
    let count = rd_u32(&mut p)? as usize;
    rec.entries.reserve(count);
    for _ in 0..count {
        if p + 25 > raw.len() { return Err("entrada truncada".into()); }
        let flags = raw[p];
        let size = u64::from_le_bytes(raw[p + 1..p + 9].try_into().unwrap());
        let modified = i64::from_le_bytes(raw[p + 9..p + 17].try_into().unwrap());
        let semihash = u64::from_le_bytes(raw[p + 17..p + 25].try_into().unwrap());
        p += 25;
        let len = rd_u32(&mut p)? as usize;
        if p + len > raw.len() { return Err("ruta truncada".into()); }
        let rel = String::from_utf8_lossy(&raw[p..p + len]).into_owned();
        p += len;
        rec.entries.push(VolEntry { rel, is_dir: flags & 1 != 0, size, modified, semihash });
    }
    Ok(rec)
}

fn from_header(v: &serde_json::Value) -> VolumeRecord {
    let s = |k: &str| v.get(k).and_then(|x| x.as_str()).unwrap_or("").to_string();
    let u = |k: &str| v.get(k).and_then(|x| x.as_u64()).unwrap_or(0);
    let i = |k: &str| v.get(k).and_then(|x| x.as_i64()).unwrap_or(0);
    let b = |k: &str| v.get(k).and_then(|x| x.as_bool()).unwrap_or(false);
    let mut rec = VolumeRecord {
        id: s("id"),
        name: s("name"),
        letter: s("letter"),
        label: s("label"),
        fs: s("fs"),
        volume_serial: u("volume_serial"),
        vendor: s("vendor"),
        model: s("model"),
        serial_number: s("serial_number"),
        bus: s("bus"),
        removable: b("removable"),
        total_bytes: u("total_bytes"),
        free_bytes: u("free_bytes"),
        first_seen: i("first_seen"),
        last_seen: i("last_seen"),
        updated: i("updated"),
        retention: Retention::parse(&s("retention")),
        enabled: v.get("enabled").and_then(|x| x.as_bool()).unwrap_or(true),
        file_count: u("file_count"),
        dir_count: u("dir_count"),
        total_logical: u("total_logical"),
        hashed: u("hashed"),
        hash_partial: b("hash_partial"),
        elapsed_ms: u("elapsed_ms"),
        note: s("note"),
        ..Default::default()
    };
    if let Some(fs) = v.get("folders").and_then(|x| x.as_array()) {
        rec.folders = fs
            .iter()
            .filter_map(|x| Some((x.get(0)?.as_str()?.to_string(), x.get(1)?.as_u64()?)))
            .collect();
    }
    if let Some(es) = v.get("by_ext").and_then(|x| x.as_array()) {
        rec.by_ext = es
            .iter()
            .filter_map(|x| Some((x.get(0)?.as_str()?.to_string(), x.get(1)?.as_u64()?, x.get(2)?.as_u64()?)))
            .collect();
    }
    rec
}

// ---------------------------------------------------------------- unidad offline

/// Una unidad guardada lista para inyectar en el árbol.
pub struct OfflineDrive {
    pub name: String,
    pub label: String,
    pub entries: Vec<OfflineEntry>,
}

pub struct OfflineEntry {
    pub rel: String,
    pub is_dir: bool,
    pub size: u64,
    pub modified: i64,
}

/// Convierte discos guardados (retención completa) en unidades del árbol.
pub fn offline_drives(records: &[VolumeRecord]) -> Vec<OfflineDrive> {
    records
        .iter()
        .filter(|r| r.retention == Retention::Full && !r.entries.is_empty())
        .map(|r| OfflineDrive {
            name: r.name.clone(),
            label: format!(
                "{} · {} · actualizado {}",
                r.display(),
                r.retention.label().0,
                fmt_date(r.updated)
            ),
            entries: r
                .entries
                .iter()
                .map(|e| OfflineEntry { rel: e.rel.clone(), is_dir: e.is_dir, size: e.size, modified: e.modified })
                .collect(),
        })
        .collect()
}

// ---------------------------------------------------------------- duplicados offline

pub struct DupOpts {
    pub deep: bool,
    pub budget_ms: u128,
    pub min_bytes: u64,
    pub max_files: usize,
}

impl Default for DupOpts {
    fn default() -> Self {
        DupOpts { deep: false, budget_ms: 3_000, min_bytes: 64 * 1024, max_files: 5_000 }
    }
}

/// Busca copias entre los archivos locales bajo `root` y los inventarios
/// guardados, usando tamaño + huella de muestra. Con `root = 0` solo compara
/// discos guardados entre sí.
pub fn duplicates(t: Option<&Tree>, root: u32, records: &[VolumeRecord], opts: &DupOpts) -> serde_json::Value {
    let started = Instant::now();
    let mut saved: HashMap<(u64, u64), Vec<(String, String)>> = HashMap::new();
    for r in records {
        if r.retention != Retention::Full {
            continue;
        }
        for e in &r.entries {
            if e.is_dir || e.semihash == 0 || e.size < opts.min_bytes {
                continue;
            }
            saved
                .entry((e.size, e.semihash))
                .or_default()
                .push((r.id.clone(), format!("{}\\{}", r.letter, e.rel)));
        }
    }

    // Duplicados entre discos guardados: no requiere que estén conectados.
    let mut saved_pairs = Vec::new();
    for ((size, _), paths) in &saved {
        if paths.len() < 2 {
            continue;
        }
        for w in paths.windows(2).take(3) {
            saved_pairs.push(serde_json::json!({
                "size": size,
                "a": { "volume": w[0].0, "path": w[0].1 },
                "b": { "volume": w[1].0, "path": w[1].1 },
                "match": "semihash",
            }));
        }
    }
    saved_pairs.sort_by(|a, b| b["size"].as_u64().cmp(&a["size"].as_u64()));

    let saved_sizes: std::collections::HashSet<u64> = saved.keys().map(|(s, _)| *s).collect();
    let mut matches = Vec::new();
    let mut analyzed = 0u64;
    let mut candidates = 0u64;
    let mut partial = false;
    if let Some(t) = t
        && root != 0
    {
        let files = t.files_under(root);
        let cand: Vec<u32> = files
            .into_iter()
            .filter(|&id| {
                let n = &t.nodes[id as usize];
                n.size >= opts.min_bytes && n.size > 0
            })
            .collect();
        candidates = cand.len() as u64;
        for id in cand {
            let n = &t.nodes[id as usize];
            let size = n.size;
            let bucket = saved_sizes.contains(&size);
            if !bucket {
                continue;
            }
            if analyzed as usize >= opts.max_files || started.elapsed().as_millis() > opts.budget_ms {
                partial = true;
                break;
            }
            let path = t.full_path(id);
            let Some(h) = semihash(Path::new(&path), size, opts.deep) else { continue };
            analyzed += 1;
            if let Some(hits) = saved.get(&(size, h)) {
                for (vol, saved_path) in hits {
                    matches.push(serde_json::json!({
                        "local": path,
                        "saved": saved_path,
                        "volume": vol,
                        "size": size,
                        "match": "semihash",
                    }));
                }
            }
        }
    }

    matches.sort_by(|a, b| b["size"].as_u64().cmp(&a["size"].as_u64()));
    let covered = if candidates == 0 { 100.0 } else { analyzed as f64 * 100.0 / candidates as f64 };
    serde_json::json!({
        "kind": "volume_duplicates",
        "mode": if opts.deep { "deep" } else { "fast" },
        "volumes": records.len(),
        "local_matches": matches.len(),
        "saved_pairs": saved_pairs.len(),
        "analyzed": analyzed,
        "candidates": candidates,
        "coverage_pct": (covered * 10.0).round() / 10.0,
        "partial": partial,
        "elapsed_ms": started.elapsed().as_millis() as u64,
        "matches": matches,
        "saved": saved_pairs,
        "notes": [
            "La huella lee una muestra (8 KiB rápido / 48 KiB profundo), no el archivo completo.",
            "Sólo se comparan archivos con huella guardada; semihash_pct indica la cobertura de cada disco.",
            "Un disco guardado con retención Resumen no participa en la búsqueda.",
        ],
    })
}

/// ¿Este archivo local está en algún disco guardado? Compara ruta relativa,
/// tamaño y huella. Sirve para responder "¿lo tengo respaldado en el disco
/// que no está conectado?".
pub fn lookup(records: &[VolumeRecord], path: &Path, deep: bool) -> serde_json::Value {
    let started = Instant::now();
    let size = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
    let hash = if size > 0 { semihash(path, size, deep) } else { None };
    let mut found = Vec::new();
    for r in records {
        if r.retention != Retention::Full {
            continue;
        }
        for e in &r.entries {
            if e.is_dir || e.size != size {
                continue;
            }
            let same_path = e.rel.rsplit(['\\', '/']).next() == Some(name);
            let same_hash = hash.is_some() && e.semihash == hash.unwrap_or(0) && e.semihash != 0;
            if same_hash || (same_path && e.size == size) {
                found.push(serde_json::json!({
                    "volume": r.id,
                    "volume_display": r.display(),
                    "saved_path": format!("{}\\{}", r.letter, e.rel),
                    "size": e.size,
                    "match": if same_hash { "semihash" } else { "path+size" },
                    "updated": r.updated,
                    "updated_date": fmt_date(r.updated),
                }));
            }
        }
    }
    found.sort_by(|a, b| a["match"].as_str().cmp(&b["match"].as_str()));
    serde_json::json!({
        "kind": "volume_lookup",
        "path": path.to_string_lossy(),
        "size": size,
        "semihash": hash,
        "found": found.len(),
        "partial": hash.is_none(),
        "elapsed_ms": started.elapsed().as_millis() as u64,
        "results": found,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(super) fn entry(rel: &str, size: u64, h: u64) -> VolEntry {
        VolEntry { rel: rel.into(), is_dir: false, size, modified: 0, semihash: h }
    }

    pub(super) fn record(id: &str, letter: &str, entries: Vec<VolEntry>) -> VolumeRecord {
        let files = entries.iter().filter(|e| !e.is_dir).count() as u64;
        VolumeRecord {
            id: id.into(),
            name: format!("{letter} (guardado)"),
            letter: letter.into(),
            label: "Prueba".into(),
            retention: Retention::Full,
            enabled: true,
            entries,
            file_count: files,
            updated: 1_700_000_000,
            ..Default::default()
        }
    }

    #[test]
    fn round_trip_keeps_entries_and_metadata() {
        let dir = std::env::temp_dir().join(format!("wzvol-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let store = Store::with_dir(dir.clone());
        let mut rec = record("vol-1", "E:", vec![entry("fotos\\a.jpg", 10, 7), VolEntry { rel: "fotos".into(), is_dir: true, ..Default::default() }]);
        rec.vendor = "WD".into();
        rec.model = "Passport".into();
        rec.serial_number = "ABC123".into();
        rec.retention = Retention::Full;
        store.save(rec.clone()).unwrap();
        let back = store.load("vol-1").unwrap();
        assert_eq!(back.vendor, "WD");
        assert_eq!(back.model, "Passport");
        assert_eq!(back.serial_number, "ABC123");
        assert_eq!(back.entries.len(), 2);
        assert_eq!(back.entries[0].rel, "fotos\\a.jpg");
        assert_eq!(back.name, "E: (guardado)");
        let listed = store.scan();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, "vol-1");
        store.delete("vol-1").unwrap();
        assert!(store.scan().is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn saved_duplicates_need_no_disk() {
        let a = record("a", "E:", vec![entry("x.bin", 100 * 1024, 42)]);
        let b = record("b", "F:", vec![entry("copia\\x.bin", 100 * 1024, 42)]);
        let mut opts = DupOpts::default();
        opts.min_bytes = 0;
        let v = duplicates(None, 0, &[a, b], &opts);
        assert_eq!(v["saved_pairs"], 1);
        assert_eq!(v["local_matches"], 0);
    }

    #[test]
    fn retained_digest_is_not_offered_as_a_drive() {
        let mut rec = record("d", "G:", vec![]);
        rec.retention = Retention::Digest;
        assert!(offline_drives(&[rec]).is_empty());
    }

    #[test]
    fn dates_are_stable() {
        assert_eq!(fmt_date(0), "—");
        assert_eq!(fmt_date(1_700_000_000).len(), 10);
    }
}

#[cfg(test)]
mod capture_tests {
    use super::*;
    use super::tests::{entry, record};

    #[test]
    fn captures_inventory_and_semihash() {
        let dir = std::env::temp_dir().join(format!("wzvol-cap-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("foto.bin");
        std::fs::write(&file, vec![7u8; 128 * 1024]).unwrap();
        let mut b = crate::tree::Builder::new();
        b.add(true, "", "C:", None, 0);
        let parent = dir.to_string_lossy().trim_end_matches('\\').to_string();
        b.add(false, &parent, "foto.bin", Some(128 * 1024), 1_700_000_000);
        let tree = b.finish(&[], std::time::Instant::now(), String::new());
        let drive = tree.drives[0];
        let prog = CaptureProgress::default();
        let rec = capture(&tree, drive, &CaptureOpts::fast(), &prog).unwrap();
        assert_eq!(rec.file_count, 1);
        assert_eq!(rec.hashed, 1);
        assert_ne!(rec.entries.iter().find(|e| !e.is_dir).unwrap().semihash, 0);
        assert_eq!(rec.retention, Retention::Full);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn toggling_enabled_preserves_the_inventory() {
        let dir = std::env::temp_dir().join(format!("wzvol-en-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let store = Store::with_dir(dir.clone());
        let rec = record("v", "E:", vec![entry("a", 1, 9)]);
        store.save(rec).unwrap();
        store.set_enabled("v", false).unwrap();
        let back = store.load("v").unwrap();
        assert!(!back.enabled);
        assert_eq!(back.entries.len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
