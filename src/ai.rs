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

/// Last known quota per model (from response headers).
#[derive(Clone, Copy, Debug)]
pub struct Quota {
    pub req_left: Option<u64>,
    pub tok_left: Option<u64>,
    pub at: std::time::Instant,
}

static QUOTAS: std::sync::Mutex<Option<std::collections::HashMap<String, Quota>>> = std::sync::Mutex::new(None);

fn remember_quota(model: &str, v: &Value) {
    let rl = &v["_rl"];
    let q = Quota { req_left: rl["req_left"].as_u64(), tok_left: rl["tok_left"].as_u64(), at: std::time::Instant::now() };
    if q.req_left.is_some() || q.tok_left.is_some() {
        QUOTAS.lock().unwrap().get_or_insert_with(Default::default).insert(model.to_string(), q);
    }
}

pub fn quota(model: &str) -> Option<Quota> {
    QUOTAS.lock().unwrap().as_ref()?.get(model).copied()
}

/// Known to be out of quota: no requests left today, or almost no tokens left
/// in the current minute (token quotas refill within a minute).
fn exhausted(model: &str) -> bool {
    quota(model).is_some_and(|q| q.req_left == Some(0) || (q.tok_left.is_some_and(|t| t < 1500) && q.at.elapsed().as_secs() < 60))
}
const MAX_TOOL_CHARS: usize = 4_000;

/// HTTP access, abstracted so the agent can be tested without the network.
pub trait Transport {
    fn get(&self, path: &str) -> Result<(u16, Value), String>;
    fn post(&self, path: &str, body: &Value) -> Result<(u16, Value), String>;
}

/// OpenAI-compatible endpoint (Groq by default; any `base_url` for manual/local ones).
pub struct Groq {
    base: String,
    key: String,
}

impl Groq {
    pub fn new(key: String) -> Self {
        Self { base: API.to_string(), key }
    }

    /// Manual endpoint: any OpenAI-compatible `base_url` (e.g. `http://localhost:11434/v1`).
    /// An empty key sends no `Authorization` header (typical for local servers).
    pub fn custom(base: &str, key: String) -> Self {
        Self { base: base.trim().trim_end_matches('/').to_string(), key }
    }

    fn auth(&self, req: ureq::Request) -> ureq::Request {
        if self.key.is_empty() { req } else { req.set("Authorization", &format!("Bearer {}", self.key)) }
    }

    fn agent() -> ureq::Agent {
        ureq::AgentBuilder::new().timeout(std::time::Duration::from_secs(90)).build()
    }

    /// Groq reports the remaining quota in headers on every response; keep them
    /// next to the body as _rl.
    fn rate_headers(resp: &ureq::Response) -> Value {
        let h = |n: &str| resp.header(n).and_then(|v| v.parse::<u64>().ok());
        json!({
            "req_left": h("x-ratelimit-remaining-requests"),
            "tok_left": h("x-ratelimit-remaining-tokens"),
            "req_limit": h("x-ratelimit-limit-requests"),
            "tok_limit": h("x-ratelimit-limit-tokens"),
        })
    }

    fn reply(r: Result<ureq::Response, ureq::Error>) -> Result<(u16, Value), String> {
        let (code, resp) = match r {
            Ok(resp) => (resp.status(), resp),
            Err(ureq::Error::Status(code, resp)) => (code, resp),
            Err(e) => return Err(e.to_string()),
        };
        let rl = Self::rate_headers(&resp);
        let mut v = resp.into_json::<Value>().unwrap_or(Value::Null);
        if v.is_object() {
            v["_rl"] = rl;
        }
        Ok((code, v))
    }
}

impl Transport for Groq {
    fn get(&self, path: &str) -> Result<(u16, Value), String> {
        Self::reply(self.auth(Self::agent().get(&format!("{}{path}", self.base))).call())
    }
    fn post(&self, path: &str, body: &Value) -> Result<(u16, Value), String> {
        Self::reply(self.auth(Self::agent().post(&format!("{}{path}", self.base))).send_json(body.clone()))
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
        429 | 413 => format!("Rate limited: {msg}"),
        _ => format!("Groq HTTP {code}: {msg}"),
    }
}

/// Chat models the account can use, best first.
pub fn list_models(t: &dyn Transport) -> Result<Vec<String>, String> {
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
    Ok(rank_models(&ids))
}

/// Ask with automatic failover: start with `preferred` (if still listed), then
/// try every other model best-first until one answers. Only an invalid key
/// stops early. `on_model` is told which model is being tried.
pub fn ask_auto(
    t: &dyn Transport,
    preferred: Option<&str>,
    question: &str,
    server: &mut mcp::Server,
    spanish: bool,
    on_model: &dyn Fn(&str),
) -> Result<(String, String), String> {
    ask_with(t, preferred, false, question, server, spanish, on_model)
}

/// Like [`ask_auto`]. With `force`, `preferred` is tried first even if the
/// endpoint does not list it (or has no `/models` at all, as many local servers).
pub fn ask_with(
    t: &dyn Transport,
    preferred: Option<&str>,
    force: bool,
    question: &str,
    server: &mut mcp::Server,
    spanish: bool,
    on_model: &dyn Fn(&str),
) -> Result<(String, String), String> {
    let mut order = match list_models(t) {
        Ok(o) => o,
        Err(e) if force && preferred.is_some() => {
            let _ = e;
            Vec::new()
        }
        Err(e) => return Err(e),
    };
    if let Some(p) = preferred {
        match order.iter().position(|m| m == p) {
            Some(i) => {
                let m = order.remove(i);
                order.insert(0, m);
            }
            None if force => order.insert(0, p.to_string()),
            None => {}
        }
    }
    // Models known to be out of quota go last instead of wasting an attempt.
    let first = if force { preferred.map(String::from) } else { None };
    order.sort_by_key(|m| exhausted(m) && first.as_deref() != Some(m));
    if order.is_empty() {
        return Err("No hay modelos de chat disponibles / No chat models available".into());
    }
    let mut errors = Vec::new();
    for m in order {
        on_model(&m);
        match ask(t, &m, question, server, spanish) {
            Ok(answer) => return Ok((m, answer)),
            Err(e) if e.contains("Invalid Groq API key") => return Err(e),
            Err(e) => errors.push(format!("{m}: {e}")),
        }
    }
    Err(format!("Ningún modelo pudo responder / No model could answer:\n{}", errors.join("\n")))
}

/// One configured endpoint: its name, transport and an optional forced model
/// (`None` = choose automatically from the endpoint's list).
pub struct Provider {
    pub name: String,
    pub transport: Box<dyn Transport + Send + Sync>,
    pub model: Option<String>,
}

/// Endpoints from the settings, in priority order: the manual one (if any),
/// then Groq (if a key is saved). Falls over to the next one on any failure.
pub fn providers(s: &crate::i18n::Settings) -> Vec<Provider> {
    let mut v = Vec::new();
    if !s.ai_base.trim().is_empty() {
        let key = unprotect(&s.ai_key_enc).unwrap_or_default();
        let name = if s.ai_name.trim().is_empty() { s.ai_base.trim().to_string() } else { s.ai_name.trim().to_string() };
        let model = Some(s.ai_model.trim().to_string()).filter(|m| !m.is_empty());
        v.push(Provider { name, transport: Box::new(Groq::custom(&s.ai_base, key)), model });
    }
    if let Some(key) = unprotect(&s.groq_key_enc).filter(|k| !k.is_empty()) {
        v.push(Provider { name: "Groq".into(), transport: Box::new(Groq::new(key)), model: None });
    }
    v
}

/// Try each provider in order; the first that answers wins.
/// `known` is the model that worked last time (used for automatic providers).
pub fn ask_chain(
    providers: &[Provider],
    known: Option<&str>,
    question: &str,
    server: &mut mcp::Server,
    spanish: bool,
    on_model: &dyn Fn(&str),
) -> Result<(String, String), String> {
    if providers.is_empty() {
        return Err("No hay IA configurada / No AI endpoint configured".into());
    }
    let mut errors = Vec::new();
    for p in providers {
        let (pref, force) = match &p.model {
            Some(m) => (Some(m.as_str()), true),
            None => (known, false),
        };
        match ask_with(p.transport.as_ref(), pref, force, question, server, spanish, on_model) {
            Ok(r) => return Ok(r),
            Err(e) => errors.push(format!("[{}] {e}", p.name)),
        }
    }
    Err(errors.join("\n"))
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
        remember_quota(model, &v);
        if code != 200 {
            return Err(api_error(code, &v));
        }
        let msg = v.pointer("/choices/0/message").cloned().unwrap_or(Value::Null);
        let calls = msg.get("tool_calls").and_then(Value::as_array).cloned().unwrap_or_default();
        if calls.is_empty() {
            let text = msg.get("content").and_then(Value::as_str).unwrap_or("").trim().to_string();
            return if text.is_empty() { Err("Empty answer (the model stopped early); try again".into()) } else { Ok(text) };
        }
        messages.push(json!({ "role": "assistant", "content": msg.get("content").cloned().unwrap_or(Value::Null), "tool_calls": calls }));
        for call in &calls {
            let name = call.pointer("/function/name").and_then(Value::as_str).unwrap_or("");
            let mut args: Value = call
                .pointer("/function/arguments")
                .and_then(Value::as_str)
                .and_then(|s| serde_json::from_str(s).ok())
                .unwrap_or(json!({}));
            if name == "find_duplicates" && args.get("verify").is_none() {
                args["verify"] = json!(false); // quick by default: never read whole disks unasked
            }
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
