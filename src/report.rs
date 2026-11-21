//! Reporte HTML autocontenido de cualquier análisis (JSON -> tabla legible).
//! Se guarda en `%LOCALAPPDATA%\WizEverything\reports`; no sale de la máquina.

use std::fmt::Write as _;
use std::path::PathBuf;

use serde_json::Value;

fn esc(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(c),
        }
    }
    out
}

fn render(v: &Value, out: &mut String, depth: usize) {
    if depth > 6 {
        out.push_str("<em>…</em>");
        return;
    }
    match v {
        Value::Null => out.push_str("<span class=null>—</span>"),
        Value::Bool(b) => {
            let _ = write!(out, "<code>{b}</code>");
        }
        Value::Number(n) => {
            let _ = write!(out, "<code>{n}</code>");
        }
        Value::String(s) => out.push_str(&esc(s)),
        Value::Array(a) => {
            if a.is_empty() {
                out.push_str("<span class=null>—</span>");
            } else if a.iter().all(|x| x.is_object()) {
                let mut keys: Vec<String> = Vec::new();
                for x in a.iter().take(20) {
                    if let Some(o) = x.as_object() {
                        for k in o.keys() {
                            if !keys.contains(k) {
                                keys.push(k.clone());
                            }
                        }
                    }
                }
                out.push_str("<table><thead><tr>");
                for k in &keys {
                    let _ = write!(out, "<th>{}</th>", esc(k));
                }
                out.push_str("</tr></thead><tbody>");
                for x in a.iter().take(500) {
                    out.push_str("<tr>");
                    for k in &keys {
                        out.push_str("<td>");
                        render(x.get(k).unwrap_or(&Value::Null), out, depth + 1);
                        out.push_str("</td>");
                    }
                    out.push_str("</tr>");
                }
                out.push_str("</tbody></table>");
                if a.len() > 500 {
                    let _ = write!(out, "<p class=null>… {} filas más</p>", a.len() - 500);
                }
            } else {
                out.push_str("<ul>");
                for x in a.iter().take(500) {
                    out.push_str("<li>");
                    render(x, out, depth + 1);
                    out.push_str("</li>");
                }
                out.push_str("</ul>");
                if a.len() > 500 {
                    let _ = write!(out, "<p class=null>… {} más</p>", a.len() - 500);
                }
            }
        }
        Value::Object(o) => {
            out.push_str("<table class=kv><tbody>");
            for (k, x) in o {
                let _ = write!(out, "<tr><th>{}</th><td>", esc(k));
                render(x, out, depth + 1);
                out.push_str("</td></tr>");
            }
            out.push_str("</tbody></table>");
        }
    }
}

/// Documento HTML completo para un reporte ya calculado.
pub fn html(title: &str, v: &Value) -> String {
    let mut body = String::new();
    render(v, &mut body, 0);
    format!(
        r#"<!doctype html>
<html lang="es"><head><meta charset="utf-8">
<meta name="viewport" content="width=device-width,initial-scale=1">
<title>{title}</title>
<style>
:root{{color-scheme:light dark}}
body{{font:14px/1.45 system-ui,Segoe UI,sans-serif;max-width:1200px;margin:2rem auto;padding:0 1rem;color:#1d1d1f;background:#fff}}
h1{{font-size:1.45rem;margin:0 0 .2rem}} .meta{{color:#666;margin-bottom:1.2rem}}
table{{border-collapse:collapse;width:100%;margin:.6rem 0}} th,td{{border:1px solid #d5d5dc;padding:.35rem .5rem;text-align:left;vertical-align:top}}
th{{background:#f2f2f6;font-weight:600}} tr:nth-child(even) td{{background:#fafafd}}
code{{font-family:ui-monospace,Consolas,monospace;font-size:.92em}} .null{{color:#888}}
details{{margin:.4rem 0}} summary{{cursor:pointer;font-weight:600}}
@media(prefers-color-scheme:dark){{body{{background:#18181c;color:#eee}}th{{background:#26262c}}th,td{{border-color:#3a3a44}}tr:nth-child(even) td{{background:#1f1f25}}.meta{{color:#aaa}}}}
@media print{{body{{max-width:none;margin:0}}}}
</style></head><body>
<h1>{title}</h1>
<p class="meta">WizEverything · generado {stamp}</p>
{body}
</body></html>"#,
        title = esc(title),
        stamp = chrono_like_now(),
        body = body,
    )
}

fn chrono_like_now() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    // Fecha UTC aproximada sin dependencias.
    let days = secs / 86_400;
    let (y, m, d) = civil_from_days(days as i64);
    let rem = secs % 86_400;
    format!(
        "{y:04}-{m:02}-{d:02} {:02}:{:02}:{:02} UTC",
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}

fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = mp + if mp < 10 { 3 } else { -9 };
    (y + if m <= 2 { 1 } else { 0 }, m as u32, d as u32)
}

fn reports_dir() -> PathBuf {
    let base = std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    base.join("WizEverything").join("reports")
}

/// Guarda el reporte y devuelve la ruta del HTML.
pub fn save(title: &str, v: &Value) -> Result<PathBuf, String> {
    let dir = reports_dir();
    std::fs::create_dir_all(&dir).map_err(|e| format!("No pude crear {}: {e}", dir.display()))?;
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let file = dir.join(format!("wize-{stamp}.html"));
    std::fs::write(&file, html(title, v)).map_err(|e| format!("No pude escribir {}: {e}", file.display()))?;
    Ok(file)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escapes_and_renders_tables() {
        let v = serde_json::json!({"a":"<b>","rows":[{"x":1},{"x":2}]});
        let h = html("T <x>", &v);
        assert!(h.contains("&lt;b&gt;"));
        assert!(h.contains("<table"));
        assert!(h.contains("T &lt;x&gt;"));
    }
}
