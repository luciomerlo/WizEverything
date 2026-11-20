//! Buscar dentro de archivos, con extracción de texto y OCR opcional.
//!
//! **Fast** (por defecto): archivos de texto/código y la capa de texto de PDFs
//! con `pdftotext` si está instalado. Presupuesto corto y resultado `partial`.
//! **Deep**: además OCR de imágenes y PDFs escaneados con `tesseract`, y más
//! tiempo por archivo.
//!
//! No agrega dependencias: usa las herramientas externas si existen y, si no,
//! lo dice en `notes` en vez de fallar. El índice es efímero: se recorre el
//! árbol de Everything y se devuelven coincidencias con fragmentos.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Instant;

use crate::tree::Tree;

/// Extensiones que se leen como texto plano.
const TEXT_EXT: &[&str] = &[
    "txt", "md", "markdown", "log", "csv", "tsv", "json", "jsonl", "xml", "html", "htm", "css",
    "js", "mjs", "ts", "tsx", "jsx", "rs", "py", "rb", "go", "java", "c", "h", "cpp", "hpp", "cs",
    "php", "sh", "ps1", "bat", "cmd", "sql", "yaml", "yml", "toml", "ini", "cfg", "conf", "tex",
];

const IMAGE_EXT: &[&str] = &["png", "jpg", "jpeg", "tif", "tiff", "bmp", "webp", "gif"];

fn ext_of(p: &Path) -> String {
    p.extension().and_then(|e| e.to_str()).unwrap_or("").to_ascii_lowercase()
}

fn has_tool(name: &str, args: &[&str]) -> bool {
    Command::new(name)
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Herramientas externas disponibles (se detectan una vez).
pub struct Tools {
    pub pdftotext: bool,
    pub tesseract: bool,
}

impl Tools {
    pub fn detect() -> Self {
        // Poppler usa -v; Tesseract usa --version.
        Tools { pdftotext: has_tool("pdftotext", &["-v"]), tesseract: has_tool("tesseract", &["--version"]) }
    }
}

/// Texto extraído de un archivo.
pub struct Extracted {
    pub text: String,
    /// `text`, `pdf` o `ocr`.
    pub kind: &'static str,
    pub note: String,
}

fn run_stdout(mut cmd: Command) -> Option<String> {
    let out = cmd.output().ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Extrae texto de `path`. En fast no hace OCR ni abre imágenes.
pub fn extract(path: &Path, deep: bool, tools: &Tools) -> Option<Extracted> {
    let ext = ext_of(path);
    if TEXT_EXT.contains(&ext.as_str()) {
        use std::io::Read;
        let cap = if deep { 4 * 1024 * 1024 } else { 1024 * 1024 };
        let mut bytes = Vec::with_capacity(cap.min(256 * 1024));
        std::fs::File::open(path).ok()?.take(cap as u64).read_to_end(&mut bytes).ok()?;
        return Some(Extracted {
            text: String::from_utf8_lossy(&bytes).into_owned(),
            kind: "text",
            note: String::new(),
        });
    }
    if ext == "pdf" {
        if !tools.pdftotext {
            return Some(Extracted { text: String::new(), kind: "pdf", note: "Falta pdftotext (poppler) para leer PDFs.".into() });
        }
        let mut cmd = Command::new("pdftotext");
        cmd.arg("-q");
        if !deep {
            cmd.args(["-l", "3"]);
        }
        cmd.arg(path).arg("-");
        let text = run_stdout(cmd).unwrap_or_default();
        if text.trim().is_empty() {
            if deep && tools.tesseract {
                // PDF escaneado: se rasteriza con pdftoppm y se pasa a tesseract.
                return ocr_pdf(path, tools, deep);
            }
            return Some(Extracted { text, kind: "pdf", note: "Sin capa de texto; probá profundo con OCR.".into() });
        }
        return Some(Extracted { text, kind: "pdf", note: String::new() });
    }
    if deep && IMAGE_EXT.contains(&ext.as_str()) && tools.tesseract {
        let mut cmd = Command::new("tesseract");
        cmd.arg(path).arg("stdout").args(["-l", "spa+eng", "--psm", "3"]);
        if let Some(text) = run_stdout(cmd) {
            return Some(Extracted { text, kind: "ocr", note: String::new() });
        }
    }
    None
}

fn ocr_pdf(path: &Path, tools: &Tools, deep: bool) -> Option<Extracted> {
    if !tools.pdftotext || !tools.tesseract {
        return None;
    }
    // pdftoppm genera una imagen temporal por página; se OCR-ea la primera.
    let dir = std::env::temp_dir().join(format!("wzocr-{}", std::process::id()));
    std::fs::create_dir_all(&dir).ok()?;
    let out = dir.join("page");
    let max_pages = if deep { 20 } else { 1 };
    let ok = Command::new("pdftoppm")
        .args(["-f", "1", "-l", &max_pages.to_string(), "-r", "200", "-png"])
        .arg(path)
        .arg(&out)
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if !ok {
        let _ = std::fs::remove_dir_all(&dir);
        return None;
    }
    let mut imgs: Vec<PathBuf> = std::fs::read_dir(&dir)
        .ok()?
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|x| x.to_str()) == Some("png"))
        .collect();
    imgs.sort();
    let mut text = String::new();
    for img in imgs.iter().take(max_pages) {
        let mut cmd = Command::new("tesseract");
        cmd.arg(img).arg("stdout").args(["-l", "spa+eng"]);
        text.push_str(&run_stdout(cmd).unwrap_or_default());
        text.push('\n');
    }
    let _ = std::fs::remove_dir_all(&dir);
    Some(Extracted {
        text,
        kind: "ocr",
        note: format!("PDF sin texto: OCR de hasta {max_pages} páginas."),
    })
}

/// Una coincidencia con contexto.
#[derive(Clone, Debug)]
pub struct Hit {
    pub path: String,
    pub size: u64,
    pub kind: &'static str,
    pub snippet: String,
}

fn snippet(text: &str, q: &str) -> Option<String> {
    // Se trabaja sobre la versión en minúsculas: el mapeo de mayúsculas puede
    // cambiar longitudes (İ, ß) y cortar en un límite inválido.
    let lower = text.to_lowercase();
    let ql = q.to_lowercase();
    let at = lower.find(&ql)?;
    let start = lower[..at].char_indices().rev().nth(60).map(|(i, _)| i).unwrap_or(0);
    let end = lower[at..].char_indices().nth(ql.chars().count() + 120).map(|(i, _)| at + i).unwrap_or(lower.len());
    let mut s = lower[start..end].split_whitespace().collect::<Vec<_>>().join(" ");
    if start > 0 { s.insert_str(0, "… "); }
    if end < text.len() { s.push_str(" …"); }
    Some(s)
}

/// Resultado de una búsqueda.
pub struct Report {
    pub hits: Vec<Hit>,
    pub scanned: u64,
    pub bytes: u64,
    pub partial: bool,
    pub elapsed_ms: u64,
    pub notes: Vec<String>,
    pub tools: Tools,
}

/// Busca `query` dentro de los archivos bajo `root`.
pub fn search(t: &Tree, root: u32, query: &str, deep: bool, max_files: usize, budget_ms: u128, limit: usize) -> Report {
    let started = Instant::now();
    let tools = Tools::detect();
    let mut notes = Vec::new();
    if !tools.pdftotext {
        notes.push("PDFs: instalá poppler (`pdftotext`) para buscar en su texto.".into());
    }
    if deep && !tools.tesseract {
        notes.push("OCR: instalá Tesseract para imágenes y PDFs escaneados.".into());
    }
    if !deep {
        notes.push("Modo rápido: sin OCR; PDFs sólo primeras 3 páginas.".into());
    }
    let mut hits = Vec::new();
    let mut scanned = 0u64;
    let mut bytes = 0u64;
    let mut partial = false;
    for id in t.files_under(root) {
        if hits.len() >= limit || scanned as usize >= max_files || started.elapsed().as_millis() > budget_ms {
            partial = true;
            break;
        }
        let p = t.full_path(id);
        let path = Path::new(&p);
        let ext = ext_of(path);
        let supported = TEXT_EXT.contains(&ext.as_str()) || ext == "pdf" || (deep && IMAGE_EXT.contains(&ext.as_str()));
        if !supported {
            continue;
        }
        scanned += 1;
        bytes += t.nodes[id as usize].size;
        let Some(ex) = extract(path, deep, &tools) else { continue };
        if let Some(sn) = snippet(&ex.text, query) {
            hits.push(Hit { path: p, size: t.nodes[id as usize].size, kind: ex.kind, snippet: sn });
        }
    }
    Report { hits, scanned, bytes, partial, elapsed_ms: started.elapsed().as_millis() as u64, notes, tools }
}

impl Report {
    pub fn to_json(&self, query: &str, limit: usize) -> serde_json::Value {
        serde_json::json!({
            "kind": "content",
            "query": query,
            "matches": self.hits.len(),
            "scanned": self.scanned,
            "bytes_scanned": self.bytes,
            "partial": self.partial,
            "elapsed_ms": self.elapsed_ms,
            "pdftotext": self.tools.pdftotext,
            "tesseract": self.tools.tesseract,
            "notes": self.notes,
            "top": self.hits.iter().take(limit).map(|h| serde_json::json!({
                "path": h.path,
                "size": h.size,
                "kind": h.kind,
                "snippet": h.snippet,
            })).collect::<Vec<_>>(),
        })
    }
}

/// Estimación antes de indexar: archivos por tipo y tiempos aproximados.
pub fn estimate(t: &Tree, root: u32) -> serde_json::Value {
    let mut text = (0u64, 0u64);
    let mut pdf = (0u64, 0u64);
    let mut images = (0u64, 0u64);
    for id in t.files_under(root) {
        let p = PathBuf::from(t.full_path(id));
        let ext = ext_of(&p);
        let size = t.nodes[id as usize].size;
        if TEXT_EXT.contains(&ext.as_str()) {
            text.0 += 1;
            text.1 += size;
        } else if ext == "pdf" {
            pdf.0 += 1;
            pdf.1 += size;
        } else if IMAGE_EXT.contains(&ext.as_str()) {
            images.0 += 1;
            images.1 += size;
        }
    }
    // Órdenes de magnitud medidos en esta app: texto ~20 MB/s, PDF ~1 pág/0,2 s,
    // OCR ~1 pág/1,2 s. Se declaran como estimación, no como promesa.
    let fast = text.1 as f64 / 20e6 + pdf.0 as f64 * 0.2;
    let deep = text.1 as f64 / 20e6 + pdf.0 as f64 * 1.2 + images.0 as f64 * 1.2;
    let tools = Tools::detect();
    serde_json::json!({
        "kind": "content_estimate",
        "text": { "files": text.0, "bytes": text.1 },
        "pdf": { "files": pdf.0, "bytes": pdf.1 },
        "images": { "files": images.0, "bytes": images.1 },
        "estimated_seconds_fast": (fast * 10.0).round() / 10.0,
        "estimated_seconds_deep": (deep * 10.0).round() / 10.0,
        "pdftotext": tools.pdftotext,
        "tesseract": tools.tesseract,
        "notes": [
            "Fast lee texto/código y las primeras páginas de PDFs; no hace OCR.",
            "Deep agrega OCR de imágenes y PDFs escaneados (mucho más lento).",
            "La estimación supone 1 página cada ~100 KB de PDF y 1,2 s por página de OCR.",
        ],
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snippets_keep_context() {
        let s = snippet("hola mundo esto es una prueba de texto", "prueba").unwrap();
        assert!(s.contains("prueba"));
        assert!(snippet("nada", "zzz").is_none());
    }

    #[test]
    fn text_extensions_are_recognized() {
        assert_eq!(ext_of(Path::new("a.MD")), "md");
        assert!(TEXT_EXT.contains(&"rs"));
    }
}
