//! Módulo 5 (núcleo) — Matriz de obsolescencia (Cold Data).
//!
//! Combina `DateModified`, `DateAccessed` y `RunCount` en un *cold score*
//! (0 = caliente, 100 = congelado) para decidir qué conviene comprimir con WOF
//! LZX. La compresión real vive en la capa Win32; acá solo se decide.

/// Entradas temporales de un archivo.
#[derive(Clone, Copy, Debug, Default)]
pub struct ColdInput {
    /// Última modificación (unix segs).
    pub modified_unix: u64,
    /// Último acceso (unix segs), si se conoce.
    pub accessed_unix: Option<u64>,
    /// Veces que se ejecutó/abrió según el historial (`Run Count` de Everything).
    pub run_count: u32,
}

const DAY: u64 = 86_400;
/// Horizonte de saturación para "sin modificar".
const MOD_CAP_DAYS: u64 = 730;
/// Horizonte de saturación para "sin acceder".
const ACC_CAP_DAYS: u64 = 365;
/// Cap de ejecuciones para normalizar.
const RUN_CAP: u32 = 100;

fn days_since(now: u64, then: u64) -> u64 {
    if then != 0 && then <= now {
        (now - then) / DAY
    } else if then == 0 {
        // 0 = desconocido -> se trata como muy viejo.
        u64::MAX / 2
    } else {
        0
    }
}

/// Cold score 0..=100 (más alto = más frío/obsoleto).
pub fn cold_score(now_unix: u64, input: ColdInput) -> u8 {
    let dm = days_since(now_unix, input.modified_unix).min(MOD_CAP_DAYS) as f64 / MOD_CAP_DAYS as f64;
    let da = input
        .accessed_unix
        .map(|a| days_since(now_unix, a).min(ACC_CAP_DAYS) as f64 / ACC_CAP_DAYS as f64)
        .unwrap_or(1.0);
    let rc = (input.run_count.min(RUN_CAP) as f64) / RUN_CAP as f64;

    let score = dm * 50.0 + da * 35.0 + (1.0 - rc) * 15.0;
    score.round().clamp(0.0, 100.0) as u8
}

/// Extensiones que ya están comprimidas por entropía: comprimirlas con LZX
/// desperdicia CPU sin ganar espacio.
pub const ALREADY_COMPRESSED: &[&str] = &[
    "zip", "7z", "rar", "zst", "gz", "xz", "bz2", "lz4", "lzma", "cab", "iso", "vhdx",
    "mp4", "mkv", "avi", "mov", "webm", "mp3", "flac", "aac", "ogg", "opus", "m4a", "wav",
    "jpg", "jpeg", "png", "gif", "webp", "heic", "avif", "bmp", "tif", "tiff",
    // Formatos ya con estructura comprimida interna:
    "docx", "xlsx", "pptx", "odt", "ods", "odp", "jar", "apk", "epub",
];

/// `true` si el formato ya está comprimido (no conviene re-comprimir).
pub fn is_already_compressed(ext: &str) -> bool {
    let e = ext.trim_start_matches('.').to_ascii_lowercase();
    ALREADY_COMPRESSED.contains(&e.as_str())
}

/// Decide si un archivo es candidato a compresión LZX.
///
/// Requiere: no estar ya comprimido, y cold score por encima del umbral.
pub fn should_compress(ext: &str, cold: u8, threshold: u8) -> bool {
    !is_already_compressed(ext) && cold >= threshold
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: u64 = 1_700_000_000;

    fn days_ago(d: u64) -> u64 {
        NOW - d * DAY
    }

    #[test]
    fn fresh_file_is_hot() {
        let input = ColdInput {
            modified_unix: days_ago(1),
            accessed_unix: Some(days_ago(0)),
            run_count: 80,
        };
        assert!(cold_score(NOW, input) < 20);
    }

    #[test]
    fn old_untouched_file_is_cold() {
        let input = ColdInput {
            modified_unix: days_ago(900),
            accessed_unix: Some(days_ago(500)),
            run_count: 0,
        };
        assert!(cold_score(NOW, input) > 80);
    }

    #[test]
    fn unknown_timestamps_treated_as_old() {
        let input = ColdInput { modified_unix: 0, accessed_unix: None, run_count: 0 };
        assert_eq!(cold_score(NOW, input), 100);
    }

    #[test]
    fn monotonic_in_age() {
        let make = |d| ColdInput { modified_unix: days_ago(d), accessed_unix: Some(days_ago(d)), run_count: 10 };
        let a = cold_score(NOW, make(10));
        let b = cold_score(NOW, make(200));
        let c = cold_score(NOW, make(600));
        assert!(a <= b && b <= c, "{a} {b} {c}");
    }

    #[test]
    fn compressed_formats_skipped() {
        assert!(is_already_compressed("jpg"));
        assert!(is_already_compressed(".MP4"));
        assert!(is_already_compressed("docx"));
        assert!(!is_already_compressed("txt"));
        assert!(!is_already_compressed("log"));
        assert!(!should_compress("zip", 99, 70));
        assert!(should_compress("txt", 99, 70));
        assert!(!should_compress("txt", 40, 70));
    }
}
