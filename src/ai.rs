//! Natural-language questions answered by a Groq model that uses the read-only
//! MCP tools (drives, folder sizes, largest files, file types, duplicates) over
//! the in-memory index. The API key is stored encrypted with Windows DPAPI.

use serde_json::{Value, json};

use crate::mcp;

pub const API: &str = "https://api.groq.com/openai/v1";

/// Best first. Only models that the account lists are tried; unknown future
/// models are ranked after these by their parameter count.
const PREFERRED: &[&str] = &[
    "openai/gpt-oss-120b",
    "moonshotai/kimi-k2-instruct-0905",
    "moonshotai/kimi-k2-instruct",
    "llama-3.3-70b-versatile",
    "qwen/qwen3-32b",
    "meta-llama/llama-4-maverick-17b-128e-instruct",
    "openai/gpt-oss-20b",
    "meta-llama/llama-4-scout-17b-16e-instruct",
    "llama-3.1-8b-instant",
];

/// Model families that are not general chat models.
const NOT_CHAT: &[&str] = &["whisper", "tts", "guard", "playai", "orpheus", "embed", "distil", "compound"];

const MAX_STEPS: usize = 8;
const MAX_TOOL_CHARS: usize = 12_000;

/// HTTP access, abstracted so the agent can be tested without the network.
pub trait Transport {
    fn get(&self, path: &str) -> Result<(u16, Value), String>;
    fn post(&self, path: &str, body: &Value) -> Result<(u16, Value), String>;
}

pub struct Groq {
    key: String,
}

impl Groq {
    pub fn new(key: String) -> Self {
        Self { key }
    }

    fn agent() -> ureq::Agent {
        ureq::AgentBuilder::new().timeout(std::time::Duration::from_secs(90)).build()
    }

    fn reply(r: Result<ureq::Response, ureq::Error>) -> Result<(u16, Value), String> {
        match r {
            Ok(resp) => {
                let code = resp.status();
                resp.into_json::<Value>().map(|v| (code, v)).map_err(|e| e.to_string())
            }
            Err(ureq::Error::Status(code, resp)) => Ok((code, resp.into_json::<Value>().unwrap_or(Value::Null))),
            Err(e) => Err(e.to_string()),
        }
    }
}

impl Transport for Groq {
    fn get(&self, path: &str) -> Result<(u16, Value), String> {
        Self::reply(Self::agent().get(&format!("{API}{path}")).set("Authorization", &format!("Bearer {}", self.key)).call())
    }
    fn post(&self, path: &str, body: &Value) -> Result<(u16, Value), String> {
        Self::reply(
            Self::agent()
                .post(&format!("{API}{path}"))
                .set("Authorization", &format!("Bearer {}", self.key))
                .send_json(body.clone()),
        )
    }
}

/// Approximate size in billions of parameters from a model id ("…-70b-…" → 70).
fn params_b(id: &str) -> f32 {
    id.split(|c: char| c == '-' || c == '/' || c == '_')
        .filter_map(|p| p.strip_suffix('b').and_then(|n| n.parse::<f32>().ok()))
        .fold(0.0, f32::max)
}

/// Chat models from the account's list, best first.
pub fn rank_models(available: &[String]) -> Vec<String> {
    let chat: Vec<&String> = available.iter().filter(|m| !NOT_CHAT.iter().any(|x| m.to_lowercase().contains(x))).collect();
    let mut out: Vec<String> = PREFERRED.iter().filter(|p| chat.iter().any(|m| m == p)).map(|s| s.to_string()).collect();
    let mut rest: Vec<&String> = chat.into_iter().filter(|m| !out.contains(m)).collect();
    rest.sort_by(|a, b| params_b(b).total_cmp(&params_b(a)));
    out.extend(rest.into_iter().cloned());
    out
}

fn api_error(code: u16, v: &Value) -> String {
    let msg = v.pointer("/error/message").and_then(Value::as_str).unwrap_or("");
    match code {
        401 => "La clave de Groq no es válida / Invalid Groq API key".into(),
        429 => format!("Sin cuota / Rate limited: {msg}"),
        _ => format!("Groq HTTP {code}: {msg}"),
    }
}

/// Pick the best listed model that currently answers (has quota).
pub fn pick_model(t: &dyn Transport) -> Result<String, String> {
    let (code, v) = t.get("/models")?;
    if code != 200 {
        return Err(api_error(code, &v));
    }
    let ids: Vec<String> = v
        .get("data")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|m| m.get("active").and_then(Value::as_bool).unwrap_or(true))
        .filter_map(|m| m.get("id").and_then(Value::as_str).map(String::from))
        .collect();
    let mut last = String::from("No hay modelos de chat disponibles / No chat models available");
    for m in rank_models(&ids) {
        let probe = json!({ "model": m, "messages": [{ "role": "user", "content": "ok" }], "max_completion_tokens": 1 });
        let (code, v) = t.post("/chat/completions", &probe)?;
        match code {
            200 => return Ok(m),
            401 => return Err(api_error(code, &v)),
            _ => last = api_error(code, &v), // 429 no quota, 400/404 not usable: try the next one
        }
    }
    Err(last)
}

fn system_prompt(spanish: bool) -> String {
    let lang = if spanish { "Respond in Spanish (Rioplatense is fine)." } else { "Respond in English." };
    format!(
        "You are the assistant of WizEverything, a Windows disk-space and duplicate-file analyzer. \
         Answer questions about this PC's files using the tools; never guess numbers. Tool sizes are bytes: \
         show them as KB/MB/GB/TB with one decimal. Paths use backslashes. Be brief and concrete; use short \
         lists when helpful. For duplicates prefer verify=false unless the user asks for certainty, and always \
         pass a path. The tools are read-only: you cannot delete or move files, but you can say what would \
         free space. {lang}"
    )
}

/// Ask a question; the model may call the read-only tools several times.
pub fn ask(t: &dyn Transport, model: &str, question: &str, server: &mut mcp::Server, spanish: bool) -> Result<String, String> {
    let tools: Vec<Value> = mcp::tools()
        .as_array()
        .into_iter()
        .flatten()
        .map(|tl| {
            json!({ "type": "function", "function": {
                "name": tl["name"], "description": tl["description"], "parameters": tl["inputSchema"],
            }})
        })
        .collect();
    let mut messages = vec![json!({ "role": "system", "content": system_prompt(spanish) }), json!({ "role": "user", "content": question })];
    for _ in 0..MAX_STEPS {
        let body = json!({ "model": model, "messages": messages, "tools": tools, "tool_choice": "auto", "temperature": 0.2 });
        let (code, v) = t.post("/chat/completions", &body)?;
        if code != 200 {
            return Err(api_error(code, &v));
        }
        let msg = v.pointer("/choices/0/message").cloned().unwrap_or(Value::Null);
        let calls = msg.get("tool_calls").and_then(Value::as_array).cloned().unwrap_or_default();
        if calls.is_empty() {
            return Ok(msg.get("content").and_then(Value::as_str).unwrap_or("").trim().to_string());
        }
        messages.push(json!({ "role": "assistant", "content": msg.get("content").cloned().unwrap_or(Value::Null), "tool_calls": calls }));
        for call in &calls {
            let name = call.pointer("/function/name").and_then(Value::as_str).unwrap_or("");
            let args: Value = call
                .pointer("/function/arguments")
                .and_then(Value::as_str)
                .and_then(|s| serde_json::from_str(s).ok())
                .unwrap_or(json!({}));
            let mut out = match server.call_tool(name, &args) {
                Ok(v) => v.to_string(),
                Err(e) => format!("{{\"error\":{}}}", Value::String(e)),
            };
            if out.len() > MAX_TOOL_CHARS {
                let cut = (0..=MAX_TOOL_CHARS).rev().find(|&i| out.is_char_boundary(i)).unwrap_or(0);
                out.truncate(cut);
                out.push_str("…(truncated)");
            }
            messages.push(json!({ "role": "tool", "tool_call_id": call.get("id").cloned().unwrap_or(Value::Null), "content": out }));
        }
    }
    Err("La pregunta necesitó demasiados pasos / Too many steps".into())
}

// ---------------------------------------------------------------- DPAPI

/// Encrypt for the current Windows user (DPAPI); returns hex.
pub fn protect(plain: &str) -> Option<String> {
    use windows_sys::Win32::Foundation::LocalFree;
    use windows_sys::Win32::Security::Cryptography::{CRYPT_INTEGER_BLOB, CryptProtectData};
    let mut data = plain.as_bytes().to_vec();
    let input = CRYPT_INTEGER_BLOB { cbData: data.len() as u32, pbData: data.as_mut_ptr() };
    let mut out = CRYPT_INTEGER_BLOB { cbData: 0, pbData: std::ptr::null_mut() };
    let ok = unsafe { CryptProtectData(&input, std::ptr::null(), std::ptr::null(), std::ptr::null(), std::ptr::null(), 0, &mut out) };
    if ok == 0 {
        return None;
    }
    let bytes = unsafe { std::slice::from_raw_parts(out.pbData, out.cbData as usize) }.to_vec();
    unsafe { LocalFree(out.pbData as _) };
    Some(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

/// Decrypt a value produced by [`protect`] (same Windows user only).
pub fn unprotect(hex: &str) -> Option<String> {
    use windows_sys::Win32::Foundation::LocalFree;
    use windows_sys::Win32::Security::Cryptography::{CRYPT_INTEGER_BLOB, CryptUnprotectData};
    let mut data: Vec<u8> = (0..hex.len() / 2).filter_map(|i| u8::from_str_radix(hex.get(i * 2..i * 2 + 2)?, 16).ok()).collect();
    if data.is_empty() {
        return None;
    }
    let input = CRYPT_INTEGER_BLOB { cbData: data.len() as u32, pbData: data.as_mut_ptr() };
    let mut out = CRYPT_INTEGER_BLOB { cbData: 0, pbData: std::ptr::null_mut() };
    let ok = unsafe {
        CryptUnprotectData(&input, std::ptr::null_mut(), std::ptr::null(), std::ptr::null(), std::ptr::null(), 0, &mut out)
    };
    if ok == 0 {
        return None;
    }
    let bytes = unsafe { std::slice::from_raw_parts(out.pbData, out.cbData as usize) }.to_vec();
    unsafe { LocalFree(out.pbData as _) };
    String::from_utf8(bytes).ok()
}
