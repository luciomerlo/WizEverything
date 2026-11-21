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
const MAIL_EXT: &[&str] = &["eml", "msg"];

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

/// Texto aproximado de un `.msg` de Outlook (contenedor OLE binario).
/// No intenta interpretar el formato: extrae las cadenas UTF-16LE/ASCII, que
/// incluyen asunto y cuerpo en la mayoría de los mensajes.
fn msg_text(bytes: &[u8]) -> String {
    let mut out = String::new();
    let mut cur = String::new();
    fn flush(cur: &mut String, out: &mut String) {
        if cur.chars().filter(|c| c.is_alphanumeric()).count() >= 4 {
            out.push_str(cur);
            out.push('\n');
        }
        cur.clear();
    }
    for w in bytes.chunks_exact(2) {
        let u = u16::from_le_bytes([w[0], w[1]]);
        let ch = char::from_u32(u as u32).filter(|c| !c.is_control() || c.is_whitespace());
        match ch {
            Some(c) if c.is_ascii_graphic() || c == ' ' || c == '\n' || c == '\r' || c == '\t' || u >= 0xA0 => {
                if c == '\r' || c == '\n' {
                    flush(&mut cur, &mut out);
                } else {
                    cur.push(c);
                }
            }
            _ => flush(&mut cur, &mut out),
        }
    }
    flush(&mut cur, &mut out);
    let ascii = String::from_utf8_lossy(bytes);
    for part in ascii.split(|c: char| c.is_control() && c != '\n' && c != '\r') {
        let t = part.trim();
        if t.len() >= 8 && t.chars().filter(|c| c.is_ascii_graphic() || *c == ' ').count() == t.chars().count() {
            out.push_str(t);
            out.push('\n');
        }
    }
    out
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

fn decode_qp(s: &str) -> Vec<u8> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'=' {
            if i + 1 < b.len() && (b[i + 1] == b'\n' || b[i + 1] == b'\r') {
                i += if b.get(i + 1) == Some(&b'\r') && b.get(i + 2) == Some(&b'\n') { 3 } else { 2 };
                continue;
            }
            if let (Some(h), Some(l)) = (b.get(i + 1).and_then(|&x| hex_val(x)), b.get(i + 2).and_then(|&x| hex_val(x))) {
                out.push(h * 16 + l);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    out
}

fn b64_val(c: u8) -> Option<u8> {
    match c {
        b'A'..=b'Z' => Some(c - b'A'),
        b'a'..=b'z' => Some(c - b'a' + 26),
        b'0'..=b'9' => Some(c - b'0' + 52),
        b'+' => Some(62),
        b'/' => Some(63),
        _ => None,
    }
}

fn decode_base64(s: &str) -> Vec<u8> {
    let mut out = Vec::new();
    let mut acc = 0u32;
    let mut bits = 0u8;
    for &c in s.as_bytes() {
        let Some(v) = b64_val(c) else { continue };
        acc = (acc << 6) | v as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    out
}

fn strip_html(s: &str) -> String {
    // Quita script/style completos y después las etiquetas, dejando un espacio
    // para no pegar palabras que estaban en líneas o celdas distintas.
    let mut cleaned = s.to_string();
    for tag in ["script", "style"] {
        loop {
            let lower = cleaned.to_ascii_lowercase();
            let Some(start) = lower.find(&format!("<{tag}")) else { break };
            let Some(rel_end) = lower[start..].find(&format!("</{tag}>")) else { break };
            let end = start + rel_end + tag.len() + 3;
            cleaned.replace_range(start..end, " ");
        }
    }
    let mut out = String::with_capacity(cleaned.len());
    let mut in_tag = false;
    for c in cleaned.chars() {
        match c {
            '<' => in_tag = true,
            '>' => {
                in_tag = false;
                // Cualquier etiqueta separa palabras: <b>mundo</b> no es "Holamundo".
                out.push(' ');
            }
            _ if in_tag => {}
            _ => out.push(c),
        }
    }
    unescape(&out.split_whitespace().collect::<Vec<_>>().join(" "))
}

fn header(body: &str, name: &str) -> Option<String> {
    let head = body.split("\n\n").next().unwrap_or(body);
    for line in head.lines() {
        if let Some((k, v)) = line.split_once(':') {
            if k.trim().eq_ignore_ascii_case(name) {
                return Some(v.trim().to_string());
            }
        }
    }
    None
}

/// Decodifica `=?charset?B|Q?texto?=` de un encabezado MIME.
fn decode_words(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find("=?") {
        out.push_str(&rest[..i]);
        let tail = &rest[i + 2..];
        let Some(j) = tail.find("?=") else {
            out.push_str("=?");
            rest = tail;
            continue;
        };
        let inner = &tail[..j];
        let mut parts = inner.splitn(3, '?');
        let charset = parts.next().unwrap_or("");
        let enc = parts.next().unwrap_or("").to_ascii_uppercase();
        let text = parts.next().unwrap_or("");
        let bytes = if enc == "B" {
            decode_base64(text)
        } else {
            decode_qp(&text.replace('_', " "))
        };
        out.push_str(&decode_charset(&bytes, charset));
        rest = &tail[j + 2..];
    }
    out.push_str(rest);
    out
}

fn cp1252(b: u8) -> char {
    match b {
        0x80 => '€', 0x82 => '‚', 0x83 => 'ƒ', 0x84 => '„', 0x85 => '…', 0x86 => '†',
        0x87 => '‡', 0x88 => 'ˆ', 0x89 => '‰', 0x8A => 'Š', 0x8B => '‹', 0x8C => 'Œ',
        0x8E => 'Ž', 0x91 => '‘', 0x92 => '’', 0x93 => '“', 0x94 => '”', 0x95 => '•',
        0x96 => '–', 0x97 => '—', 0x98 => '˜', 0x99 => '™', 0x9A => 'š', 0x9B => '›',
        0x9C => 'œ', 0x9E => 'ž', 0x9F => 'Ÿ',
        _ => char::from_u32(b as u32).unwrap_or('\u{FFFD}'),
    }
}

fn decode_charset(bytes: &[u8], charset: &str) -> String {
    let cs = charset.to_ascii_lowercase();
    if cs.contains("utf-16") || bytes.starts_with(&[0xFF, 0xFE]) || bytes.starts_with(&[0xFE, 0xFF]) {
        let be = cs.contains("be") || bytes.starts_with(&[0xFE, 0xFF]);
        let units: Vec<u16> = bytes
            .chunks_exact(2)
            .map(|w| if be { u16::from_be_bytes([w[0], w[1]]) } else { u16::from_le_bytes([w[0], w[1]]) })
            .collect();
        return String::from_utf16_lossy(&units);
    }
    if cs.contains("iso-8859-1") || cs.contains("latin1") || cs.contains("windows-1252") || cs.contains("cp1252") {
        return bytes.iter().map(|&b| cp1252(b)).collect();
    }
    String::from_utf8_lossy(bytes).into_owned()
}

fn boundary_of(ctype: &str) -> Option<String> {
    ctype
        .split(';')
        .find_map(|p| p.trim().strip_prefix("boundary="))
        .map(|b| b.trim_matches('"').to_string())
}

fn eml_multipart(raw: &str, ctype: &str, depth: usize) -> String {
    if depth > 4 {
        return String::new();
    }
    let Some(b) = boundary_of(ctype) else { return String::new() };
    let alternative = ctype.contains("multipart/alternative");
    let mut out = String::new();
    let mut html = String::new();
    for part in raw.split(&format!("--{b}")) {
        let p = part.trim_start_matches('\n');
        if p.starts_with("--") || p.trim().is_empty() {
            continue;
        }
        let pctype = header(p, "content-type").unwrap_or_default().to_ascii_lowercase();
        let t = eml_part(p, depth + 1);
        if t.trim().is_empty() {
            continue;
        }
        // En alternative, texto plano y HTML son el mismo mensaje: quedarse con uno.
        if alternative && pctype.contains("text/html") {
            if html.is_empty() {
                html = t;
            }
        } else {
            out.push_str(&t);
            out.push('\n');
        }
    }
    if out.trim().is_empty() { html } else { out }
}

fn eml_part(raw: &str, depth: usize) -> String {
    let (head, body) = raw.split_once("\n\n").unwrap_or((raw, ""));
    let disp = header(head, "content-disposition").unwrap_or_default().to_ascii_lowercase();
    if disp.contains("attachment") || disp.contains("filename=") {
        return String::new();
    }
    let ctype = header(head, "content-type").unwrap_or_default().to_ascii_lowercase();
    if !ctype.is_empty() && !ctype.contains("text/") && !ctype.contains("multipart/") {
        return String::new();
    }
    let enc = header(head, "content-transfer-encoding").unwrap_or_default().to_ascii_lowercase();
    let bytes = if enc.contains("quoted-printable") {
        decode_qp(body)
    } else if enc.contains("base64") {
        decode_base64(body)
    } else {
        body.as_bytes().to_vec()
    };
    let text = decode_charset(&bytes, &ctype);
    if ctype.contains("multipart/") {
        return eml_multipart(&text, &ctype, depth);
    }
    if ctype.contains("text/html") { strip_html(&text) } else { text }
}

fn eml_text(raw: &str) -> String {
    let norm = raw.replace("\r\n", "\n").replace('\r', "\n");
    let mut out = String::new();
    for h in ["Subject", "From", "To", "Date"] {
        if let Some(v) = header(&norm, h) {
            out.push_str(h);
            out.push_str(": ");
            out.push_str(&decode_words(&v));
            out.push('\n');
        }
    }
    out.push('\n');
    out.push_str(&eml_part(&norm, 0));
    out
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
        return Some(Extracted {
            text: String::from_utf8_lossy(&bytes).into_owned(),
            kind: "text",
            note: String::new(),
        });
    }
    if MAIL_EXT.contains(&ext.as_str()) {
        let cap = if deep { 16 * 1024 * 1024 } else { 8 * 1024 * 1024 };
        let mut bytes = Vec::new();
        use std::io::Read;
        std::fs::File::open(path).ok()?.take(cap as u64).read_to_end(&mut bytes).ok()?;
        if ext == "msg" {
            let text = msg_text(&bytes);
            return Some(Extracted {
                text,
                kind: "email",
                note: "Outlook .msg: extracción aproximada de cadenas; puede incluir ruido binario.".into(),
            });
        }
        let raw = String::from_utf8_lossy(&bytes).into_owned();
        return Some(Extracted { text: eml_text(&raw), kind: "email", note: String::new() });
    }
    if DOC_EXT.contains(&ext.as_str()) {
        if ext == "rtf" {
            let raw = std::fs::read_to_string(path).ok()?;
            return Some(Extracted { text: rtf_text(&raw), kind: "document", note: String::new() });
        }
        if ext == "epub" {
            let text = read_zip(path, |n| n.ends_with(".xhtml") || n.ends_with(".html"), &["p"])?;
            return Some(Extracted { text, kind: "document", note: String::new() });
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
        return Some(Extracted { text, kind: "document", note: String::new() });
    }
    if PPT_EXT.contains(&ext.as_str()) {
        let text = if ext == "pptx" {
            read_zip(path, |n| (n.starts_with("ppt/slides/slide") || n.starts_with("ppt/notesSlides/")) && n.ends_with(".xml"), &["t"])
        } else {
            read_zip(path, |n| n == "content.xml", &["p"])
        }?;
        return Some(Extracted { text, kind: "presentation", note: String::new() });
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
        return Some(Extracted { text, kind: "sheet", note: String::new() });
    }
    if LEGACY_EXT.contains(&ext.as_str()) {
        return Some(Extracted {
            text: String::new(),
            kind: "document",
            note: format!(".{ext} es un formato binario antiguo: convertí a .{} para buscar dentro.", if ext == "doc" { "docx" } else if ext == "ppt" { "pptx" } else { "xlsx" }),
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
        let left = budget_secs(started, budget_ms);
        if left == 0 {
            return None;
        }
        let text = run_stdout_timeout(cmd, left.min(if deep { 60 } else { 20 })).unwrap_or_default();
        if text.trim().is_empty() {
            if deep && tools.tesseract {
                // PDF escaneado: se rasteriza con pdftoppm y se pasa a tesseract.
                return ocr_pdf(path, tools, deep, budget_ms, started);
            }
            return Some(Extracted { text, kind: "pdf", note: "Sin capa de texto; probá profundo con OCR.".into() });
        }
        return Some(Extracted { text, kind: "pdf", note: String::new() });
    }
    if deep && IMAGE_EXT.contains(&ext.as_str()) && tools.tesseract {
        let left = budget_secs(started, budget_ms);
        if left == 0 {
            return None;
        }
        let mut cmd = Command::new("tesseract");
        cmd.arg(path).arg("stdout").args(["-l", "spa+eng", "--psm", "3"]);
        if let Some(text) = run_stdout_timeout(cmd, left.min(25)) {
            return Some(Extracted { text, kind: "ocr", note: String::new() });
        }
    }
    None
}

fn ocr_pdf(path: &Path, tools: &Tools, deep: bool, budget_ms: u128, started: Instant) -> Option<Extracted> {
    if !tools.pdftotext || !tools.tesseract {
        return None;
    }
    // Directorio único por llamada: dos búsquedas simultáneas no comparten PNGs.
    let dir = temp_file("ocr");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).ok()?;
    let out = dir.join("page");
    let max_pages = if deep { 20 } else { 1 };
    let mut raster = Command::new("pdftoppm");
    raster.args(["-f", "1", "-l", &max_pages.to_string(), "-r", "200", "-png"]).arg(path).arg(&out);
    let left = budget_secs(started, budget_ms);
    if left == 0 {
        let _ = std::fs::remove_dir_all(&dir);
        return None;
    }
    let ok = status_timeout(raster, left.min(180));
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
        text.push_str(&run_stdout_timeout(cmd, 25).unwrap_or_default());
        text.push('\n');
    }
    let _ = std::fs::remove_dir_all(&dir);
    Some(Extracted {
        text,
        kind: "ocr",
        note: format!("PDF sin texto: OCR de hasta {max_pages} páginas."),
    })
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
            || MAIL_EXT.contains(&ext.as_str())
            || ext == "pdf"
            || (deep && IMAGE_EXT.contains(&ext.as_str()));
        if !supported {
            continue;
        }
        scanned += 1;
        let size = t.nodes[id as usize].size;
        let modified = t.nodes[id as usize].modified as i64;
        bytes += size;
        let cached = index
            .as_ref()
            .and_then(|c| crate::content_index::get(c, &p, size, modified));
        let ex = match cached {
            Some((kind, text)) => {
                index_hits += 1;
                Extracted {
                    text,
                    kind: match kind.as_str() {
                        "text" => "text",
                        "email" => "email",
                        "ocr" => "ocr",
                        "pdf" => "pdf",
                        "presentation" => "presentation",
                        "sheet" => "sheet",
                        _ => "document",
                    },
                    note: String::new(),
                }
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
                if owner
                    && let Some(c) = &index
                    && !ex.text.is_empty()
                    && ex.note.is_empty()
                    && crate::content_index::put(c, &p, size, modified, ex.kind, &ex.text).is_ok()
                {
                    index_writes += 1;
                }
                ex
            }
        };
        if let Some(sn) = snippet(&ex.text, query) {
            hits.push(Hit { path: p, size, kind: ex.kind, snippet: sn });
        }
    }
    if owner
        && let Some(c) = &index
    {
        let _ = crate::content_index::release(c, crate::content_index::APP_WIZ);
    }
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
    let mut email = (0u64, 0u64);
    for id in t.files_under(root) {
        let p = PathBuf::from(t.full_path(id));
        let ext = ext_of(&p);
        let size = t.nodes[id as usize].size;
        if MAIL_EXT.contains(&ext.as_str()) {
            email.0 += 1;
            email.1 += size;
        } else if TEXT_EXT.contains(&ext.as_str()) {
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
    let fast = (text.1 + email.1) as f64 / 20e6 + pdf.0 as f64 * 0.2;
    let deep = (text.1 + email.1) as f64 / 20e6 + pdf.0 as f64 * 1.2 + images.0 as f64 * 1.2;
    let tools = Tools::detect();
    serde_json::json!({
        "kind": "content_estimate",
        "text": { "files": text.0, "bytes": text.1 },
        "pdf": { "files": pdf.0, "bytes": pdf.1 },
        "images": { "files": images.0, "bytes": images.1 },
        "email": { "files": email.0, "bytes": email.1 },
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
        let tools = Tools::detect();
        let d = extract(&docx, false, &tools).unwrap();
        assert_eq!(d.kind, "document");
        assert!(d.text.contains("Hola documento"));

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
    fn eml_headers_and_quoted_printable_are_read() {
        let raw = "Subject: Factura =?UTF-8?B?YWJyaWw=?=
From: a@b.c
Content-Type: text/plain; charset=utf-8
Content-Transfer-Encoding: quoted-printable

Hola=20mundo=
continuaci=C3=B3n
";
        let text = eml_text(raw);
        assert!(text.contains("Subject:"));
        assert!(text.contains("abril"), "RFC 2047 subject is decoded");
        assert!(text.contains("Hola mundo"));
        assert!(text.contains("continuación"));
    }

    #[test]
    fn eml_multipart_skips_attachments() {
        let raw = r#"Content-Type: multipart/mixed; boundary="b"

--b
Content-Type: text/plain; charset=utf-8

Hola mundo
--b
Content-Type: application/pdf; name=a.pdf
Content-Disposition: attachment; filename=a.pdf
Content-Transfer-Encoding: base64

JVBERi0xLjQ=
--b--
"#;
        let text = eml_text(raw);
        assert!(text.contains("Hola mundo"));
        assert!(!text.contains("JVBERi"), "el adjunto no debe indexarse");
    }

    #[test]
    fn html_tags_do_not_glue_words() {
        let text = strip_html("<p>Hola<b>mundo</b><br>otra</p><script>x()</script>");
        assert!(text.contains("Hola mundo"));
        assert!(text.contains("otra"));
        assert!(!text.contains("x()"));
    }

    #[test]
    fn text_extensions_are_recognized() {
        assert_eq!(ext_of(Path::new("a.MD")), "md");
        assert!(TEXT_EXT.contains(&"rs"));
    }
}
