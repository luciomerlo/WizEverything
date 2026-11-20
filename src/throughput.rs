//! Velocidad real y estimación de tiempos.
//!
//! Antes de prometer cuánto tarda una tarea, se mide:
//! - **clase de disco**: NVMe/SSD/HDD/USB/red, por bus y penalidad de seek;
//! - **lectura/escritura secuencial** con un archivo real y un tope de bytes;
//! - **Google Drive**: si la unidad está en modo streaming o clone, qué parte
//!   de los bytes ya está local/fijada y, opcionalmente, la velocidad real de
//!   descarga leyendo un placeholder (eso lo hidrata: sólo en modo profundo).
//!
//! El ETA usa la velocidad efectiva de la cadena (`1 / (1/origen + 1/destino)`)
//! más un costo fijo por archivo, para no ignorar millones de archivos chicos.

use std::path::{Path, PathBuf};
use std::time::Instant;

use crate::tree::Tree;

/// Tipo de almacenamiento, para elegir expectativas y paralelismo.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DiskClass {
    Nvme,
    Ssd,
    Hdd,
    Usb,
    Network,
    Unknown,
}

impl DiskClass {
    pub fn as_str(self) -> &'static str {
        match self {
            DiskClass::Nvme => "nvme",
            DiskClass::Ssd => "ssd",
            DiskClass::Hdd => "hdd",
            DiskClass::Usb => "usb",
            DiskClass::Network => "network",
            DiskClass::Unknown => "unknown",
        }
    }

    pub fn label(self) -> (&'static str, &'static str) {
        match self {
            DiskClass::Nvme => ("NVMe", "NVMe"),
            DiskClass::Ssd => ("SSD", "SSD"),
            DiskClass::Hdd => ("disco mecánico", "hard drive"),
            DiskClass::Usb => ("USB", "USB"),
            DiskClass::Network => ("red", "network"),
            DiskClass::Unknown => ("desconocido", "unknown"),
        }
    }

    /// Velocidad típica de referencia (MB/s) cuando no se puede medir.
    pub fn fallback_mbps(self) -> (f64, f64) {
        match self {
            DiskClass::Nvme => (2_500.0, 1_500.0),
            DiskClass::Ssd => (500.0, 400.0),
            DiskClass::Hdd => (150.0, 120.0),
            DiskClass::Usb => (100.0, 60.0),
            DiskClass::Network => (50.0, 50.0),
            DiskClass::Unknown => (120.0, 100.0),
        }
    }
}

/// Clase de la unidad de una ruta.
pub fn class_of(path: &Path) -> DiskClass {
    let s = path.to_string_lossy();
    if s.starts_with("\\\\") {
        return DiskClass::Network;
    }
    let Some(letter) = crate::winphys::drive_letter(path) else { return DiskClass::Unknown };
    let bus = crate::volinfo::query(&format!("{letter}:")).bus.to_ascii_lowercase();
    if bus.contains("usb") {
        return DiskClass::Usb;
    }
    if bus.contains("nvme") {
        return DiskClass::Nvme;
    }
    match crate::winphys::incurs_seek_penalty(letter) {
        Some(true) => DiskClass::Hdd,
        Some(false) => DiskClass::Ssd,
        None => DiskClass::Unknown,
    }
}

/// Google Drive: streaming (placeholders) o clone (archivos locales).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GoogleMode {
    Stream,
    Clone,
}

impl GoogleMode {
    pub fn as_str(self) -> &'static str {
        match self {
            GoogleMode::Stream => "stream",
            GoogleMode::Clone => "clone",
        }
    }
}

/// Detecta si la ruta está en una unidad de Google Drive y en qué modo.
pub fn google_mode(path: &Path) -> Option<GoogleMode> {
    let letter = crate::winphys::drive_letter(path)?;
    let label = crate::tree::volume_label(&format!("{letter}:"));
    if !label.contains("Google Drive") {
        return None;
    }
    if label.contains("(clone)") { Some(GoogleMode::Clone) } else { Some(GoogleMode::Stream) }
}

/// Resultado de un benchmark de lectura o escritura.
#[derive(Clone, Debug)]
pub struct Bench {
    pub mbps: f64,
    pub bytes: u64,
    pub ms: u128,
    pub path: String,
}

/// Lee hasta `max_bytes` de un archivo y devuelve MB/s. Para un placeholder de
/// Google Drive esto mide la descarga real (y lo hidrata).
pub fn bench_read(path: &Path, max_bytes: u64) -> Result<Bench, String> {
    use std::io::Read;
    let mut f = std::fs::File::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let size = f.metadata().map(|m| m.len()).unwrap_or(0);
    let want = max_bytes.min(size).max(1);
    let mut buf = vec![0u8; 1024 * 1024];
    let mut read = 0u64;
    let start = Instant::now();
    while read < want {
        let n = f.read(&mut buf).map_err(|e| e.to_string())?;
        if n == 0 {
            break;
        }
        read += n as u64;
    }
    let secs = start.elapsed().as_secs_f64().max(0.001);
    Ok(Bench {
        mbps: read as f64 / 1_000_000.0 / secs,
        bytes: read,
        ms: start.elapsed().as_millis(),
        path: path.to_string_lossy().into(),
    })
}

/// Escritura secuencial en un archivo temporal dentro de `dir` (se borra).
pub fn bench_write(dir: &Path, max_bytes: u64) -> Result<Bench, String> {
    use std::io::Write;
    std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    let path = dir.join(format!("wize-speed-{}.tmp", std::process::id()));
    let mut f = std::fs::File::create(&path).map_err(|e| e.to_string())?;
    let chunk = vec![0xA5u8; 1024 * 1024];
    let mut written = 0u64;
    let start = Instant::now();
    while written < max_bytes {
        let n = (max_bytes - written).min(chunk.len() as u64) as usize;
        f.write_all(&chunk[..n]).map_err(|e| e.to_string())?;
        written += n as u64;
    }
    f.sync_all().ok();
    let ms = start.elapsed().as_millis();
    drop(f);
    let _ = std::fs::remove_file(&path);
    let secs = ms as f64 / 1000.0;
    Ok(Bench { mbps: written as f64 / 1_000_000.0 / secs.max(0.001), bytes: written, ms, path: path.to_string_lossy().into() })
}

const ATTR_OFFLINE: u32 = 0x0000_1000;
const ATTR_PINNED: u32 = 0x0008_0000;
const ATTR_UNPINNED: u32 = 0x0010_0000;
const ATTR_RECALL_DATA: u32 = 0x0040_0000;
const ATTR_RECALL_OPEN: u32 = 0x0004_0000;

/// Qué parte de una carpeta está local vs en la nube, y cuánto está fijado.
#[derive(Clone, Debug, Default)]
pub struct CloudBreakdown {
    pub files: u64,
    pub scanned: u64,
    pub bytes: u64,
    pub local_bytes: u64,
    pub cloud_bytes: u64,
    pub pinned_bytes: u64,
    pub partial: bool,
}

impl CloudBreakdown {
    /// Porcentaje de bytes que no habría que descargar.
    pub fn local_pct(&self) -> f64 {
        if self.bytes == 0 { 0.0 } else { self.local_bytes as f64 * 100.0 / self.bytes as f64 }
    }

    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "files": self.files,
            "scanned": self.scanned,
            "bytes": self.bytes,
            "local_bytes": self.local_bytes,
            "cloud_bytes": self.cloud_bytes,
            "pinned_bytes": self.pinned_bytes,
            "local_pct": (self.local_pct() * 10.0).round() / 10.0,
            "partial": self.partial,
        })
    }
}

/// Recorre hasta `max_files` archivos y clasifica local/nube/fijado por tamaño.
pub fn cloud_breakdown(t: &Tree, root: u32, budget_ms: u128, max_files: usize) -> CloudBreakdown {
    let started = Instant::now();
    let mut b = CloudBreakdown::default();
    for id in t.files_under(root) {
        if b.scanned as usize >= max_files || started.elapsed().as_millis() > budget_ms {
            b.partial = true;
            break;
        }
        let p = t.full_path(id);
        let Ok(attr) = crate::winphys::attributes(Path::new(&p)) else { continue };
        let size = t.nodes[id as usize].size;
        b.scanned += 1;
        b.files += 1;
        b.bytes += size;
        let cloud = attr & (ATTR_OFFLINE | ATTR_RECALL_DATA | ATTR_RECALL_OPEN) != 0;
        if cloud {
            b.cloud_bytes += size;
        } else {
            b.local_bytes += size;
        }
        if attr & ATTR_PINNED != 0 && attr & ATTR_UNPINNED == 0 {
            b.pinned_bytes += size;
            if cloud {
                b.local_bytes += size;
                b.cloud_bytes = b.cloud_bytes.saturating_sub(size);
            }
        }
    }
    b
}

/// Primer archivo de la nube (placeholder) que se pueda usar para medir descarga.
pub fn first_cloud_file(t: &Tree, root: u32, min_bytes: u64) -> Option<u32> {
    for id in t.files_under(root) {
        let n = &t.nodes[id as usize];
        if n.size < min_bytes {
            continue;
        }
        let p = t.full_path(id);
        if let Ok(attr) = crate::winphys::attributes(Path::new(&p))
            && attr & (ATTR_OFFLINE | ATTR_RECALL_DATA | ATTR_RECALL_OPEN) != 0
        {
            return Some(id);
        }
    }
    None
}

/// Estimación de una transferencia.
#[derive(Clone, Debug)]
pub struct Eta {
    pub seconds: f64,
    pub effective_mbps: f64,
    pub source_mbps: f64,
    pub target_mbps: f64,
    pub overhead_ms_per_file: u64,
}

impl Eta {
    pub fn human(&self) -> String {
        let s = self.seconds.max(0.0);
        if s < 60.0 {
            format!("{s:.0} s")
        } else if s < 3_600.0 {
            format!("{:.1} min", s / 60.0)
        } else {
            format!("{:.1} h", s / 3_600.0)
        }
    }

    pub fn to_json(&self, bytes: u64, files: u64) -> serde_json::Value {
        serde_json::json!({
            "bytes": bytes,
            "files": files,
            "seconds": (self.seconds * 10.0).round() / 10.0,
            "human": self.human(),
            "effective_mbps": (self.effective_mbps * 10.0).round() / 10.0,
            "source_mbps": (self.source_mbps * 10.0).round() / 10.0,
            "target_mbps": (self.target_mbps * 10.0).round() / 10.0,
            "overhead_ms_per_file": self.overhead_ms_per_file,
        })
    }
}

/// ETA de `bytes` en `files` archivos con una cadena origen→destino.
pub fn estimate(bytes: u64, files: u64, source_mbps: f64, target_mbps: f64, overhead_ms_per_file: u64) -> Eta {
    let src = source_mbps.max(0.1);
    let dst = target_mbps.max(0.1);
    let effective = 1.0 / (1.0 / src + 1.0 / dst);
    let seconds = bytes as f64 / 1_000_000.0 / effective + (files * overhead_ms_per_file) as f64 / 1000.0;
    Eta { seconds, effective_mbps: effective, source_mbps: src, target_mbps: dst, overhead_ms_per_file }
}

/// Elige un archivo grande real para medir lectura (no hidrata si no es nube).
pub fn sample_file(t: &Tree, root: u32, min_bytes: u64) -> Option<u32> {
    let mut best = None;
    for id in t.files_under(root) {
        let n = &t.nodes[id as usize];
        if n.size >= min_bytes && best.is_none_or(|b: u32| n.size > t.nodes[b as usize].size) {
            best = Some(id);
        }
    }
    best
}

/// Ruta temporal en la misma unidad que `path`, para medir escritura.
pub fn write_probe_dir(path: &Path) -> PathBuf {
    match crate::winphys::drive_letter(path) {
        Some(c) if c.eq_ignore_ascii_case(&crate::winphys::drive_letter(&std::env::temp_dir()).unwrap_or('Z')) => std::env::temp_dir(),
        Some(c) => PathBuf::from(format!("{c}:\\")),
        None => std::env::temp_dir(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn eta_uses_the_slower_link_and_file_overhead() {
        let e = estimate(1_000_000_000, 0, 100.0, 100.0, 0);
        assert!((e.effective_mbps - 50.0).abs() < 0.01);
        assert!((e.seconds - 20.0).abs() < 0.1);
        let f = estimate(0, 1_000, 100.0, 100.0, 2);
        assert!((f.seconds - 2.0).abs() < 0.01);
    }

    #[test]
    fn classes_have_sane_fallbacks() {
        assert!(DiskClass::Nvme.fallback_mbps().0 > DiskClass::Hdd.fallback_mbps().0);
        assert_eq!(DiskClass::Usb.as_str(), "usb");
    }

    #[test]
    fn breakdown_percentages_are_safe() {
        let b = CloudBreakdown { bytes: 100, local_bytes: 25, ..Default::default() };
        assert_eq!(b.local_pct(), 25.0);
        assert_eq!(CloudBreakdown::default().local_pct(), 0.0);
    }

    #[test]
    fn read_benchmark_reads_and_reports() {
        let p = std::env::temp_dir().join(format!("wzsp-{}", std::process::id()));
        std::fs::write(&p, vec![1u8; 4 * 1024 * 1024]).unwrap();
        let b = bench_read(&p, 2 * 1024 * 1024).unwrap();
        assert!(b.bytes <= 2 * 1024 * 1024);
        assert!(b.mbps > 0.0);
        let _ = std::fs::remove_file(&p);
    }
}
