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
use std::time::{Duration, Instant};

use crate::tree::Tree;

/// Extensiones que se leen como texto plano.
const TEXT_EXT: &[&str] = &[
    "txt", "md", "markdown", "log", "csv", "tsv", "json", "jsonl", "xml", "html", "htm", "css",
    "js", "mjs", "ts", "tsx", "jsx", "rs", "py", "rb", "go", "java", "c", "h", "cpp", "hpp", "cs",
    "php", "sh", "ps1", "bat", "cmd", "sql", "yaml", "yml", "toml", "ini", "cfg", "conf", "tex",
];

const IMAGE_EXT: &[&str] = &["png", "jpg", "jpeg", "tif", "tiff", "bmp", "webp", "gif"];
const DOC_EXT: &[&str] = &["docx", "odt", "rtf", "epub"];
const PPT_EXT: &[&str] = &["pptx", "odp"];
const SHEET_EXT: &[&str] = &["xlsx", "ods"];
const LEGACY_EXT: &[&str] = &["doc", "ppt", "xls"];

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

/// Herramientas externas disponibles (se detectan una vez por proceso).
#[derive(Clone, Copy, Debug)]
pub struct Tools {
    pub pdftotext: bool,
    pub tesseract: bool,
}

impl Tools {
    pub fn detect() -> Self {
        // Cacheado: detectar cuesta dos procesos (`pdftotext -v`, `tesseract --version`)
        // y antes se repetía en cada búsqueda y en cada estimación.
        static CACHE: std::sync::OnceLock<Tools> = std::sync::OnceLock::new();
        *CACHE.get_or_init(|| Tools {
            // Poppler usa -v; Tesseract usa --version.
            pdftotext: has_tool("pdftotext", &["-v"]),
            tesseract: has_tool("tesseract", &["--version"]),
        })
    }

    /// Sin herramientas externas: sólo formatos nativos. Útil para tests y para
    /// razonar sobre lo que la app puede hacer por sí sola (no lanza procesos).
    pub const fn none() -> Self {
        Tools { pdftotext: false, tesseract: false }
    }

    fn ocr_pdf_ready(&self) -> bool {
        self.pdftotext && self.tesseract
    }
}

/// Texto extraído de un archivo.
pub struct Extracted {
    pub text: String,
    /// `text`, `pdf` o `ocr`.
    pub kind: &'static str,
    pub note: String,
    /// `true` si el texto es el mejor resultado que estas herramientas y este
    /// modo pueden dar (vale la pena guardarlo en el índice). `false` cuando
    /// conviene reintentar: falta `pdftotext`, falta OCR, o el PDF sólo tenía
    /// una capa de texto insignificante.
    pub cacheable: bool,
}

impl Extracted {
    fn new(text: String, kind: &'static str, note: String) -> Self {
        Extracted { text, kind, note, cacheable: true }
    }

    fn retry(text: String, kind: &'static str, note: String) -> Self {
        Extracted { text, kind, note, cacheable: false }
    }
}

/// Caracteres no blancos a partir de los cuales se considera que un PDF ya
/// tiene capa de texto útil y no vale la pena rasterizar con `pdftoppm`+OCR.
/// Muchos PDFs escaneados emiten un par de caracteres (números de página,
/// sellos), así que "no vacío" no alcanza como criterio.
const PDF_TEXT_MIN_CHARS: usize = 48;

/// `true` si el texto del PDF ya es suficientemente bueno para no hacer OCR.
fn pdf_text_is_enough(text: &str) -> bool {
    text.chars().filter(|c| !c.is_whitespace() && !c.is_control()).take(PDF_TEXT_MIN_CHARS).count()
        == PDF_TEXT_MIN_CHARS
}

fn temp_file(tag: &str) -> PathBuf {
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    std::env::temp_dir().join(format!("wz-{tag}-{}-{stamp}", std::process::id()))
}

/// Ejecuta con un tope de tiempo. Sin esto, un PDF dañado o una imagen enorme
/// puede colgar el OCR para siempre.
fn run_stdout_timeout(mut cmd: Command, secs: u64) -> Option<String> {
    let out_path = temp_file("out");
    let file = std::fs::File::create(&out_path).ok()?;
    cmd.stdout(Stdio::from(file)).stderr(Stdio::null());
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(_) => {
            let _ = std::fs::remove_file(&out_path);
            return None;
        }
    };
    let deadline = Instant::now() + Duration::from_secs(secs);
    let status = loop {
        match child.try_wait() {
            Ok(Some(st)) => break Some(st),
            Ok(None) => {}
            Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            break None;
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    let text = std::fs::read(&out_path).ok().map(|b| String::from_utf8_lossy(&b).into_owned());
    let _ = std::fs::remove_file(&out_path);
    match status {
        Some(st) if st.success() => text,
        _ => None,
    }
}

fn status_timeout(mut cmd: Command, secs: u64) -> bool {
    cmd.stdout(Stdio::null()).stderr(Stdio::null());
    let Ok(mut child) = cmd.spawn() else { return false };
    let deadline = Instant::now() + Duration::from_secs(secs);
    loop {
        match child.try_wait() {
            Ok(Some(st)) => return st.success(),
            Ok(None) => {}
            Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                return false;
            }
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return false;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Extrae texto de `path`. En fast no hace OCR ni abre imágenes.
pub fn extract(path: &Path, deep: bool, tools: &Tools) -> Option<Extracted> {
    extract_with_budget(path, deep, tools, 120_000)
}

fn budget_secs(started: Instant, budget_ms: u128) -> u64 {
    let left = budget_ms.saturating_sub(started.elapsed().as_millis());
    if left == 0 { 0 } else { ((left + 999) / 1000).max(1) as u64 }
}

fn extract_with_budget(path: &Path, deep: bool, tools: &Tools, budget_ms: u128) -> Option<Extracted> {
    let started = Instant::now();
    let ext = ext_of(path);
    if TEXT_EXT.contains(&ext.as_str()) {
        use std::io::Read;
        let cap = if deep { 4 * 1024 * 1024 } else { 1024 * 1024 };
        let mut bytes = Vec::with_capacity(cap.min(256 * 1024));
        std::fs::File::open(path).ok()?.take(cap as u64).read_to_end(&mut bytes).ok()?;
        return Some(Extracted::new(String::from_utf8_lossy(&bytes).into_owned(), "text", String::new()));
    }
    if DOC_EXT.contains(&ext.as_str()) {
        if ext == "rtf" {
            let raw = std::fs::read_to_string(path).ok()?;
            return Some(Extracted::new(rtf_text(&raw), "document", String::new()));
        }
        if ext == "epub" {
            let text = read_zip(path, |n| n.ends_with(".xhtml") || n.ends_with(".html"), &["p"])?;
            return Some(Extracted::new(text, "document", String::new()));
        }
        let text = read_zip(
            path,
            |n| {
                n.starts_with("word/")
                    && n.ends_with(".xml")
                    && ["document", "header", "footer", "footnotes", "endnotes", "comments"]
                        .iter()
                        .any(|k| n.contains(k))
            },
            &["t"],
        )?;
        return Some(Extracted::new(text, "document", String::new()));
    }
    if PPT_EXT.contains(&ext.as_str()) {
        let text = if ext == "pptx" {
            read_zip(path, |n| (n.starts_with("ppt/slides/slide") || n.starts_with("ppt/notesSlides/")) && n.ends_with(".xml"), &["t"])
        } else {
            read_zip(path, |n| n == "content.xml", &["p"])
        }?;
        return Some(Extracted::new(text, "presentation", String::new()));
    }
    if SHEET_EXT.contains(&ext.as_str()) {
        let text = if ext == "xlsx" {
            read_zip(
                path,
                |n| n.starts_with("xl/sharedStrings") || n.starts_with("xl/worksheets/") || n.starts_with("xl/comments"),
                &["t"],
            )
        } else {
            read_zip(path, |n| n == "content.xml", &["p"])
        }?;
        return Some(Extracted::new(text, "sheet", String::new()));
    }
    if LEGACY_EXT.contains(&ext.as_str()) {
        return Some(Extracted::retry(
            String::new(),
            "document",
            format!(".{ext} es un formato binario antiguo: convertí a .{} para buscar dentro.", if ext == "doc" { "docx" } else if ext == "ppt" { "pptx" } else { "xlsx" }),
        ));
    }
    if ext == "pdf" {
        if !tools.pdftotext {
            return Some(Extracted::retry(String::new(), "pdf", "Falta pdftotext (poppler) para leer PDFs.".into()));
        }
        let mut cmd = Command::new("pdftotext");
        cmd.arg("-q");
        if !deep {
            cmd.args(["-l", "3"]);
        }
        cmd.arg(path).arg("-");
        let left = budget_secs(started, budget_ms);
        if left == 0 {
            return None;
        }
        let text = run_stdout_timeout(cmd, left.min(if deep { 60 } else { 20 })).unwrap_or_default();
        // Camino rápido: si la capa de texto alcanza, se devuelve ya. No se
        // crea directorio temporal, no se lanza `pdftoppm` ni `tesseract`.
        if pdf_text_is_enough(&text) {
            return Some(Extracted::new(text, "pdf", String::new()));
        }
        if deep && tools.tesseract {
            // PDF escaneado (o con capa casi vacía): se rasteriza con pdftoppm
            // y se pasa a tesseract, dentro del presupuesto restante.
            if let Some(ex) = ocr_pdf(path, tools, deep, budget_ms, started) {
                return Some(ex);
            }
        }
        if text.trim().is_empty() {
            return Some(Extracted::retry(text, "pdf", "Sin capa de texto; probá profundo con OCR.".into()));
        }
        // Capa de texto mínima: se devuelve (puede haber coincidencias) pero no
        // se cachea, para que un modo profundo posterior sí intente OCR.
        return Some(Extracted::retry(text, "pdf", "Capa de texto mínima; probá profundo con OCR.".into()));
    }
    if deep && IMAGE_EXT.contains(&ext.as_str()) && tools.tesseract {
        let left = budget_secs(started, budget_ms);
        if left == 0 {
            return None;
        }
        let mut cmd = Command::new("tesseract");
        cmd.arg(path).arg("stdout").args(["-l", "spa+eng", "--psm", "3"]);
        if let Some(text) = run_stdout_timeout(cmd, left.min(25)) {
            return Some(Extracted::new(text, "ocr", String::new()));
        }
    }
    None
}

fn ocr_pdf(path: &Path, tools: &Tools, deep: bool, budget_ms: u128, started: Instant) -> Option<Extracted> {
    if !tools.ocr_pdf_ready() {
        return None;
    }
    // Directorio único por llamada: dos búsquedas simultáneas no comparten PNGs.
    // Se limpia siempre al salir, incluso si algo falla a mitad de camino.
    let dir = temp_file("ocr");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).ok()?;
    let result = ocr_pdf_inner(path, deep, budget_ms, started, &dir);
    let _ = std::fs::remove_dir_all(&dir);
    result
}

fn ocr_pdf_inner(path: &Path, deep: bool, budget_ms: u128, started: Instant, dir: &Path) -> Option<Extracted> {
    let out = dir.join("page");
    let max_pages = if deep { 20 } else { 1 };
    let mut raster = Command::new("pdftoppm");
    raster.args(["-f", "1", "-l", &max_pages.to_string(), "-r", "200", "-png"]).arg(path).arg(&out);
    let left = budget_secs(started, budget_ms);
    if left == 0 {
        return None;
    }
    if !status_timeout(raster, left.min(180)) {
        return None;
    }
    let mut imgs: Vec<PathBuf> = std::fs::read_dir(dir)
        .ok()?
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|x| x.to_str()) == Some("png"))
        .collect();
    imgs.sort();
    let mut text = String::new();
    for img in imgs.iter().take(max_pages) {
        // Cada página respeta el presupuesto global, no sólo el tope por página.
        let left = budget_secs(started, budget_ms);
        if left == 0 {
            break;
        }
        let mut cmd = Command::new("tesseract");
        cmd.arg(img).arg("stdout").args(["-l", "spa+eng"]);
        text.push_str(&run_stdout_timeout(cmd, left.min(25)).unwrap_or_default());
        text.push('\n');
    }
    Some(Extracted::new(text, "ocr", format!("PDF sin texto: OCR de hasta {max_pages} páginas.")))
}

fn unescape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find('&') {
        out.push_str(&rest[..i]);
        rest = &rest[i..];
        let Some(end) = rest.find(';').filter(|&e| e <= 10) else {
            out.push('&');
            rest = &rest[1..];
            continue;
        };
        let ent = &rest[1..end];
        let ch = match ent {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" => Some('\''),
            "nbsp" => Some(' '),
            _ => ent
                .strip_prefix("#x")
                .and_then(|h| u32::from_str_radix(h, 16).ok())
                .or_else(|| ent.strip_prefix('#').and_then(|d| d.parse().ok()))
                .and_then(char::from_u32),
        };
        match ch {
            Some(c) => {
                out.push(c);
                rest = &rest[end + 1..];
            }
            None => {
                out.push_str(&rest[..end + 1]);
                rest = &rest[end + 1..];
            }
        }
    }
    out.push_str(rest);
    out
}

/// Texto de un XML de Office/ODF, capturando sólo los tags de texto.
fn xml_text(xml: &str, tags: &[&str]) -> String {
    let mut out = String::new();
    let mut depth = 0usize;
    let mut i = 0usize;
    while i < xml.len() {
        if xml.as_bytes()[i] == b'<' {
            let end = xml[i..].find('>').map(|x| i + x).unwrap_or(xml.len());
            let raw = &xml[i + 1..end];
            let closing = raw.starts_with('/');
            let self_closing = raw.ends_with('/');
            let name = raw
                .trim_start_matches('/')
                .split(|c: char| c.is_whitespace() || c == '/')
                .next()
                .unwrap_or("");
            let local = name.rsplit(':').next().unwrap_or(name);
            if tags.contains(&local) {
                if closing {
                    depth = depth.saturating_sub(1);
                    if depth == 0 {
                        out.push('\n');
                    }
                } else if !self_closing {
                    depth += 1;
                }
            } else if depth > 0 && local == "tab" {
                out.push('\t');
            } else if depth > 0 && local == "br" && !closing {
                out.push('\n');
            }
            i = end + 1;
        } else {
            let end = xml[i..].find('<').map(|x| i + x).unwrap_or(xml.len());
            if depth > 0 {
                out.push_str(&unescape(&xml[i..end]));
            }
            i = end;
        }
    }
    out
}

fn read_zip(path: &Path, pick: impl Fn(&str) -> bool, tags: &[&str]) -> Option<String> {
    use std::io::Read;
    let file = std::fs::File::open(path).ok()?;
    let mut zip = zip::ZipArchive::new(file).ok()?;
    let mut out = String::new();
    let cap = 8 * 1024 * 1024;
    for i in 0..zip.len() {
        let Ok(mut f) = zip.by_index(i) else { continue };
        if !pick(f.name()) {
            continue;
        }
        let mut buf = Vec::new();
        (&mut f).take(cap as u64).read_to_end(&mut buf).ok()?;
        let xml = String::from_utf8_lossy(&buf);
        out.push_str(&xml_text(&xml, tags));
        out.push('\n');
        if out.len() >= cap {
            break;
        }
    }
    Some(out)
}

/// RTF básico: quita controles y conserva el texto visible.
fn rtf_text(raw: &str) -> String {
    let mut out = String::new();
    let mut chars = raw.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '{' | '}' => {}
            '\\' => match chars.next() {
                Some('\\') | Some('{') | Some('}') => out.push(chars.clone().next().map_or('\\', |_| '\\')),
                Some('\'') => {
                    let hex: String = (0..2).filter_map(|_| chars.next()).collect();
                    if let Ok(b) = u8::from_str_radix(&hex, 16) {
                        out.push(b as char);
                    }
                }
                Some('u') => {
                    let mut num = String::new();
                    while let Some(&d) = chars.peek() {
                        if d == '-' || d.is_ascii_digit() {
                            num.push(d);
                            chars.next();
                        } else {
                            break;
                        }
                    }
                    if let Ok(n) = num.parse::<i32>() {
                        out.push(char::from_u32(n as u32).unwrap_or('?'));
                    }
                    chars.next(); // delimiter
                }
                Some(d) if d.is_ascii_alphabetic() => {
                    while let Some(&d) = chars.peek() {
                        if d.is_ascii_alphabetic() {
                            chars.next();
                        } else {
                            break;
                        }
                    }
                    if matches!(chars.peek(), Some(' ')) {
                        chars.next();
                    }
                }
                _ => {}
            },
            _ => out.push(c),
        }
    }
    out
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
    /// Textos servidos por el índice compartido (evita releer el archivo).
    pub index_hits: u64,
    pub index_writes: u64,
    pub index_path: String,
}

/// Texto extraído pendiente de escribir en el índice compartido.
struct PendingDoc {
    path: String,
    size: u64,
    mtime: i64,
    kind: &'static str,
    text: String,
}

/// Documentos por transacción al indexar. Un commit por archivo costaba un
/// ciclo de WAL/fsync por cada texto; lotes de 32 lo amortizan sin retener el
/// lock de escritura durante toda la búsqueda.
const INDEX_BATCH: usize = 32;

/// Vuelca el lote pendiente en **una** transacción. Devuelve cuántos se
/// escribieron (0 si no hay índice o si falla: el índice es una caché).
fn flush_index(
    index: Option<&rusqlite::Connection>,
    pending: &mut Vec<PendingDoc>,
    notes: &mut Vec<String>,
) -> u64 {
    if pending.is_empty() {
        return 0;
    }
    let Some(c) = index else {
        pending.clear();
        return 0;
    };
    // Sin clonar textos: `Doc` sólo toma prestado de `pending`.
    let docs = pending.iter().map(|d| crate::content_index::Doc {
        path: &d.path,
        size: d.size,
        mtime: d.mtime,
        kind: d.kind,
        text: &d.text,
    });
    let n = match crate::content_index::put_many(c, docs) {
        Ok(n) => n as u64,
        Err(e) => {
            if notes.len() < 30 {
                notes.push(format!("Índice compartido: {e}"));
            }
            0
        }
    };
    pending.clear();
    n
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
    // Índice compartido con SortEverything: misma ruta, misma base.
    let index = crate::content_index::open().ok();
    let mut owner = false;
    if let Some(c) = &index {
        owner = crate::content_index::claim(c, crate::content_index::APP_WIZ).unwrap_or(false);
        notes.push(format!("Índice compartido: {}", crate::content_index::display_path()));
        if !owner {
            notes.push("La otra app está indexando ahora; se usa lo que ya hay en la base compartida.".into());
        }
    }
    let mut hits = Vec::new();
    let mut scanned = 0u64;
    let mut bytes = 0u64;
    let mut partial = false;
    let mut index_hits = 0u64;
    let mut index_writes = 0u64;
    let mut cloud = 0u64;
    let mut pending: Vec<PendingDoc> = Vec::with_capacity(INDEX_BATCH);
    for id in t.files_under(root) {
        if hits.len() >= limit || scanned as usize >= max_files || started.elapsed().as_millis() > budget_ms {
            partial = true;
            break;
        }
        let p = t.full_path(id);
        let path = Path::new(&p);
        let ext = ext_of(path);
        let supported = TEXT_EXT.contains(&ext.as_str())
            || DOC_EXT.contains(&ext.as_str())
            || PPT_EXT.contains(&ext.as_str())
            || SHEET_EXT.contains(&ext.as_str())
            || LEGACY_EXT.contains(&ext.as_str())
            || ext == "pdf"
            || (deep && IMAGE_EXT.contains(&ext.as_str()));
        if !supported {
            continue;
        }
        // Nunca hidratar un placeholder de nube desde una búsqueda de contenido.
        // Se consulta sólo metadata; el archivo cuenta como omitido.
        if crate::winphys::is_cloud_placeholder(path) {
            cloud += 1;
            continue;
        }
        scanned += 1;
        let size = t.nodes[id as usize].size;
        let modified = t.nodes[id as usize].modified as i64;
        bytes += size;
        let cached = index
            .as_ref()
            .and_then(|c| crate::content_index::get(c, &p, size, modified));
        // `cacheable` sólo importa para escribir el índice; un hit del índice no
        // se vuelve a escribir.
        let (kind, text, cacheable) = match cached {
            Some((kind, text)) => {
                index_hits += 1;
                let kind = match kind.as_str() {
                    "text" => "text",
                    "ocr" => "ocr",
                    "pdf" => "pdf",
                    "presentation" => "presentation",
                    "sheet" => "sheet",
                    _ => "document",
                };
                (kind, text, false)
            }
            None => {
                let left = budget_ms.saturating_sub(started.elapsed().as_millis());
                if left == 0 {
                    partial = true;
                    break;
                }
                let Some(ex) = extract_with_budget(path, deep, &tools, left) else { continue };
                if !ex.note.is_empty() && notes.len() < 30 {
                    notes.push(format!("{}: {}", p, ex.note));
                }
                (ex.kind, ex.text, ex.cacheable)
            }
        };
        let found = snippet(&text, query);
        // Se cachea también lo que no coincide: el índice sirve a futuras búsquedas.
        if owner && cacheable && !text.trim().is_empty() {
            pending.push(PendingDoc { path: p.clone(), size, mtime: modified, kind, text });
            if pending.len() >= INDEX_BATCH {
                index_writes += flush_index(index.as_ref(), &mut pending, &mut notes);
            }
        }
        if let Some(sn) = found {
            hits.push(Hit { path: p, size, kind, snippet: sn });
        }
    }
    if owner {
        index_writes += flush_index(index.as_ref(), &mut pending, &mut notes);
        if let Some(c) = &index {
            let _ = crate::content_index::release(c, crate::content_index::APP_WIZ);
        }
    }
    if cloud > 0 {
        notes.push(format!(
            "{cloud} archivos son placeholders de nube y no se abrieron (no se descargaron)."
        ));
    }
    // Si no somos dueños no se escribió nada: se descarta el lote parcial.
    pending.clear();
    Report {
        hits,
        scanned,
        bytes,
        partial,
        elapsed_ms: started.elapsed().as_millis() as u64,
        notes,
        tools,
        index_hits,
        index_writes,
        index_path: crate::content_index::display_path(),
    }
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
            "index_hits": self.index_hits,
            "index_writes": self.index_writes,
            "index_path": self.index_path,
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
        let n = &t.nodes[id as usize];
        // Extension ids are dense; no need to build the full path just to
        // classify a million files before estimating.
        let ext = t.exts.get(n.ext as usize).map(|e| e.name.as_str()).unwrap_or("");
        let size = n.size;
        if TEXT_EXT.contains(&ext) {
            text.0 += 1;
            text.1 += size;
        } else if ext == "pdf" {
            pdf.0 += 1;
            pdf.1 += size;
        } else if IMAGE_EXT.contains(&ext) {
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
    fn docx_and_xlsx_text_is_extracted() {
        use std::io::Write;
        let dir = std::env::temp_dir().join(format!("wzfmt-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let docx = dir.join("a.docx");
        let mut z = zip::ZipWriter::new(std::fs::File::create(&docx).unwrap());
        z.start_file("word/document.xml", zip::write::SimpleFileOptions::default()).unwrap();
        z.write_all(br#"<w:document><w:body><w:p><w:r><w:t>Hola documento</w:t></w:r></w:p></w:body></w:document>"#).unwrap();
        z.finish().unwrap();
        // Sin herramientas externas: estos formatos son nativos.
        let tools = Tools::none();
        let d = extract(&docx, false, &tools).unwrap();
        assert_eq!(d.kind, "document");
        assert!(d.text.contains("Hola documento"));
        assert!(d.cacheable);

        let xlsx = dir.join("a.xlsx");
        let mut z = zip::ZipWriter::new(std::fs::File::create(&xlsx).unwrap());
        z.start_file("xl/sharedStrings.xml", zip::write::SimpleFileOptions::default()).unwrap();
        z.write_all(br#"<sst><si><t>Factura enero</t></si></sst>"#).unwrap();
        z.finish().unwrap();
        let x = extract(&xlsx, false, &tools).unwrap();
        assert_eq!(x.kind, "sheet");
        assert!(x.text.contains("Factura enero"));
        let _ = std::fs::remove_dir_all(&dir);
    }
    #[test]
    fn text_extensions_are_recognized() {
        assert_eq!(ext_of(Path::new("a.MD")), "md");
        assert!(TEXT_EXT.contains(&"rs"));
    }

    #[test]
    fn pdf_text_layer_is_only_good_when_it_has_enough_content() {
        // Nada o casi nada (número de página, sello) → conviene OCR.
        assert!(!pdf_text_is_enough(""));
        assert!(!pdf_text_is_enough("  \n\t \n"));
        assert!(!pdf_text_is_enough("1"));
        assert!(!pdf_text_is_enough(&"x".repeat(PDF_TEXT_MIN_CHARS - 1)));
        // Los blancos no cuentan.
        assert!(!pdf_text_is_enough(&" x \n".repeat(PDF_TEXT_MIN_CHARS - 1)));
        assert!(pdf_text_is_enough(&"x".repeat(PDF_TEXT_MIN_CHARS)));
        assert!(pdf_text_is_enough("Factura de enero\nTotal: 1234\nCliente: ACME S.A.\nVencimiento: 30/01"));
    }

    #[test]
    fn pdf_without_pdftotext_does_not_spawn_anything_and_is_not_cached() {
        let dir = std::env::temp_dir().join(format!("wzpdf-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let pdf = dir.join("a.pdf");
        std::fs::write(&pdf, b"%PDF-1.4\n%%EOF").unwrap();
        let ex = extract(&pdf, true, &Tools::none()).unwrap();
        assert_eq!(ex.kind, "pdf");
        assert!(ex.text.is_empty());
        assert!(ex.note.contains("pdftotext"));
        // `cacheable = false`: cuando aparezca poppler se reintenta solo.
        assert!(!ex.cacheable);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn plain_text_and_legacy_formats_have_the_expected_cache_policy() {
        let dir = std::env::temp_dir().join(format!("wztxt-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let txt = dir.join("a.txt");
        std::fs::write(&txt, "hola índice compartido").unwrap();
        let ex = extract(&txt, false, &Tools::none()).unwrap();
        assert_eq!(ex.kind, "text");
        assert!(ex.text.contains("compartido"));
        assert!(ex.cacheable);

        let doc = dir.join("viejo.doc");
        std::fs::write(&doc, [0u8; 4]).unwrap();
        let ex = extract(&doc, false, &Tools::none()).unwrap();
        assert!(ex.text.is_empty());
        assert!(!ex.cacheable);

        // Formatos fuera de alcance: ni imagen en modo rápido ni correo.
        let img = dir.join("a.png");
        std::fs::write(&img, [0u8; 4]).unwrap();
        assert!(extract(&img, false, &Tools::none()).is_none());
        let mail = dir.join("a.eml");
        std::fs::write(&mail, "Subject: hola").unwrap();
        assert!(extract(&mail, true, &Tools::none()).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn flushing_without_an_index_is_a_no_op() {
        let mut pending = vec![PendingDoc {
            path: r"C:\a.txt".into(),
            size: 1,
            mtime: 1,
            kind: "text",
            text: "alfa".into(),
        }];
        let mut notes = Vec::new();
        assert_eq!(flush_index(None, &mut pending, &mut notes), 0);
        assert!(pending.is_empty());
        assert!(notes.is_empty());
    }
}
