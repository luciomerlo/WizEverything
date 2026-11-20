//! Módulo 6 — Validador de integridad por *Magic Bytes*.
//!
//! Detecta archivos corruptos, truncados o descargados mal:
//! - preasignados vacíos (los primeros bytes son todo `0x00`),
//! - páginas de error HTML guardadas con extensión binaria,
//! - firma binaria que no coincide con la extensión declarada.
//!
//! Todo el análisis trabaja sobre los primeros [`SNIFF_LEN`] bytes, sin leer
//! el archivo completo. La lógica de decisión ([`validate`]) es pura para
//! poder testearla sin tocar disco; [`sniff`] es el envoltorio de I/O.

use std::io::Read;
use std::path::Path;

/// Cantidad de bytes que hay que leer al inicio de cada archivo.
pub const SNIFF_LEN: usize = 32;

/// Una firma binaria esperada para una familia de extensiones.
pub struct Sig {
    /// Extensiones (en minúsculas, sin punto) que deberían tener esta firma.
    pub exts: &'static [&'static str],
    /// Bytes esperados.
    pub magic: &'static [u8],
    /// Desplazamiento dentro del archivo donde arranca `magic`.
    pub offset: usize,
}

/// Tabla de firmas para las extensiones estructuradas que nos interesan.
pub static SIGNATURES: &[Sig] = &[
    Sig { exts: &["pdf"], magic: b"%PDF-", offset: 0 },
    // ZIP y sus derivados (OOXML, JAR, APK, EPUB, y también CBZ/KRITA-like).
    Sig {
        exts: &["zip", "docx", "xlsx", "pptx", "jar", "apk", "epub", "odt", "ods", "odp"],
        magic: b"PK\x03\x04",
        offset: 0,
    },
    Sig { exts: &["7z"], magic: b"7z\xbc\xaf\x27\x1c", offset: 0 },
    Sig { exts: &["rar"], magic: b"Rar!\x1a\x07", offset: 0 },
    // MP4/MOV/M4A: la caja `ftyp` arranca en el byte 4.
    Sig {
        exts: &["mp4", "m4v", "mov", "m4a", "3gp"],
        magic: b"ftyp",
        offset: 4,
    },
    // Matroska / WebM comparten el mismo EBML header.
    Sig { exts: &["mkv", "webm", "mka", "mks"], magic: b"\x1a\x45\xdf\xa3", offset: 0 },
    Sig { exts: &["jpg", "jpeg", "jpe", "jfif"], magic: b"\xff\xd8\xff", offset: 0 },
    Sig { exts: &["png"], magic: b"\x89PNG\r\n\x1a\n", offset: 0 },
    Sig { exts: &["gif"], magic: b"GIF8", offset: 0 },
    Sig { exts: &["webp"], magic: b"WEBP", offset: 8 },
    Sig { exts: &["sqlite", "sqlite3", "db"], magic: b"SQLite format 3\x00", offset: 0 },
    Sig {
        exts: &["exe", "dll", "sys", "ocx", "cpl", "scr", "efi"],
        magic: b"MZ",
        offset: 0,
    },
];

/// Resultado de validar un archivo.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// La firma coincide con la extensión (o no hay nada que validar).
    Okay,
    /// La extensión no está en la tabla: no es un formato estructurado conocido.
    NotStructuredExt,
    /// Los primeros bytes son todos `0x00`: descarga fallida / preasignación vacía.
    AllZero,
    /// El archivo arranca con una página HTML (`<!DOCTYPE`, `<html`, …) pese a
    /// tener una extensión binaria: típico de errores de descarga 404/502.
    HtmlError,
    /// La firma binaria no coincide con la que corresponde a la extensión.
    Mismatch,
    /// El archivo es más corto que lo necesario para verificar la firma.
    TooShort,
}

impl Verdict {
    /// `true` si el archivo merece que el usuario lo revise.
    pub fn is_suspicious(self) -> bool {
        matches!(self, Verdict::AllZero | Verdict::HtmlError | Verdict::Mismatch)
    }
}

/// Extensiones que la tabla considera estructuradas (útil para filtrar candidatos).
pub fn is_structured_ext(ext: &str) -> bool {
    let needle = ext.trim_start_matches('.').to_ascii_lowercase();
    SIGNATURES.iter().any(|s| s.exts.contains(&needle.as_str()))
}

/// Devuelve las firmas esperadas para una extensión (sin punto, case-insensitive).
fn expected_for_ext(ext: &str) -> Vec<&'static Sig> {
    let needle = ext.trim_start_matches('.').to_ascii_lowercase();
    SIGNATURES
        .iter()
        .filter(|s| s.exts.contains(&needle.as_str()))
        .collect()
}

/// `true` si todos los bytes son 0.
fn all_zero(head: &[u8]) -> bool {
    !head.is_empty() && head.iter().all(|&b| b == 0)
}

/// `true` si el inicio parece una página/diagnóstico HTML (ignora espacios,
/// BOM UTF-8 y mayúsculas).
fn looks_like_html(head: &[u8]) -> bool {
    let mut i = 0;
    // Saltar BOM UTF-8 si está presente.
    if head.starts_with(&[0xEF, 0xBB, 0xBF]) {
        i = 3;
    }
    while i < head.len() && matches!(head[i], b' ' | b'\t' | b'\r' | b'\n') {
        i += 1;
    }
    let rest = &head[i..];
    let lower: Vec<u8> = rest.iter().take(16).map(u8::to_ascii_lowercase).collect();
    lower.starts_with(b"<!doctype")
        || lower.starts_with(b"<html")
        || lower.starts_with(b"<?xml")
        || lower.starts_with(b"<head")
        || lower.starts_with(b"<body")
}

/// Lógica pura de validación sobre los primeros bytes de un archivo.
pub fn validate(ext: &str, head: &[u8]) -> Verdict {
    let sigs = expected_for_ext(ext);
    if sigs.is_empty() {
        return Verdict::NotStructuredExt;
    }
    if all_zero(head) {
        return Verdict::AllZero;
    }
    if looks_like_html(head) {
        return Verdict::HtmlError;
    }
    // Si el archivo es más corto de lo que exige cualquier firma, no se puede validar.
    let max_needed = sigs.iter().map(|s| s.offset + s.magic.len()).max().unwrap_or(0);
    if head.len() < max_needed {
        return Verdict::TooShort;
    }
    let matched = sigs.iter().any(|s| {
        let end = s.offset + s.magic.len();
        head.len() >= end && &head[s.offset..end] == s.magic
    });
    if matched { Verdict::Okay } else { Verdict::Mismatch }
}

/// Lee los primeros [`SNIFF_LEN`] bytes de `path` y valida contra su extensión.
pub fn sniff(path: &Path) -> std::io::Result<Verdict> {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("");
    let mut buf = [0u8; SNIFF_LEN];
    let mut f = std::fs::File::open(path)?;
    let mut read = 0usize;
    loop {
        match f.read(&mut buf[read..])? {
            0 => break,
            n => {
                read += n;
                if read == SNIFF_LEN {
                    break;
                }
            }
        }
    }
    Ok(validate(ext, &buf[..read]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pdf_ok() {
        assert_eq!(validate("pdf", b"%PDF-1.7\n...."), Verdict::Okay);
    }

    #[test]
    fn zip_family_ok() {
        assert_eq!(validate("docx", b"PK\x03\x04resto"), Verdict::Okay);
        assert_eq!(validate("ZIP", b"PK\x03\x04resto"), Verdict::Okay);
        assert_eq!(validate(".epub", b"PK\x03\x04resto"), Verdict::Okay);
    }

    #[test]
    fn mp4_ftyp_at_offset_4() {
        let mut head = vec![0u8; 12];
        head[4..8].copy_from_slice(b"ftyp");
        assert_eq!(validate("mp4", &head), Verdict::Okay);
    }

    #[test]
    fn all_zero_detected() {
        assert_eq!(validate("pdf", &[0u8; 32]), Verdict::AllZero);
        assert_eq!(validate("exe", &[0u8; 16]), Verdict::AllZero);
    }

    #[test]
    fn html_error_detected() {
        assert_eq!(
            validate("png", b"<!DOCTYPE html><html><body>404</body></html>"),
            Verdict::HtmlError
        );
        assert_eq!(
            validate("mp4", b"  <html><head></head>"),
            Verdict::HtmlError
        );
        // Con BOM UTF-8.
        assert_eq!(
            validate("zip", b"\xEF\xBB\xBF<html>err"),
            Verdict::HtmlError
        );
    }

    #[test]
    fn mismatch_detected() {
        assert_eq!(validate("png", b"%PDF-1.7 gibberish...."), Verdict::Mismatch);
    }

    #[test]
    fn unknown_ext_not_structured() {
        assert_eq!(validate("txt", b"hola mundo"), Verdict::NotStructuredExt);
        assert!(!is_structured_ext("txt"));
        assert!(is_structured_ext("pdf"));
    }

    #[test]
    fn too_short_for_signature() {
        // Menos de 8 bytes: no alcanza para validar PNG.
        assert_eq!(validate("png", b"\x89PNG"), Verdict::TooShort);
    }

    #[test]
    fn sqlite_and_exe() {
        assert_eq!(
            validate("db", b"SQLite format 3\x00\x10\x00"),
            Verdict::Okay
        );
        assert_eq!(validate("dll", b"MZ\x90\x00\x03\x00"), Verdict::Okay);
    }

    #[test]
    fn suspicious_classification() {
        assert!(Verdict::AllZero.is_suspicious());
        assert!(Verdict::HtmlError.is_suspicious());
        assert!(Verdict::Mismatch.is_suspicious());
        assert!(!Verdict::Okay.is_suspicious());
        assert!(!Verdict::NotStructuredExt.is_suspicious());
        assert!(!Verdict::TooShort.is_suspicious());
    }
}
