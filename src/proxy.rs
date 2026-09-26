//! Loopback proxy between Claude Code and the Anthropic API that swaps in the routed model.
//! Port of router/proxy.py (the reference); approach from github.com/gargpratyush/jev-router.
//!
//! Claude Code runs with ANTHROPIC_BASE_URL pointed here and the sentinel model "jev-router".
//! Requests carrying the sentinel are rewritten to the routed rung; any other model is the
//! user's explicit choice and passes through. Only the first request of a user turn is
//! routed; its tool loop reuses that rung. HTTP/1.1 is served by hand so every SSE chunk is
//! flushed as it arrives; upstream is ureq (rustls, connection pool).

use crate::router::{rank_of, H, LADDER, S_MED};
use crate::util::{local_hms, utc_iso};
use regex::Regex;
use serde_json::{json, Map, Value};
use std::collections::hash_map::DefaultHasher;
use std::collections::{HashMap, VecDeque};
use std::fs::{self, OpenOptions};
use std::hash::{Hash, Hasher};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub const AUTO_MODEL: &str = "jev-router";
/// Tool-loop request with no rung yet (Jev failed on the turn's first request).
const AGENT_FALLBACK: usize = S_MED;
/// Claude Code's own tool-less calls: titles, summaries.
const AUX_RUNG: usize = H;
const MAX_CONVERSATIONS: usize = 50;
const HOP_HEADERS: [&str; 9] = [
    "connection",
    "keep-alive",
    "proxy-connection",
    "transfer-encoding",
    "upgrade",
    "te",
    "trailer",
    "host",
    "content-length",
];
/// Models that accept mid-conversation `role: "system"` messages and tool_addition/removal
/// blocks. Claude Code sends both to a model id it doesn't know; the others return 400.
const SYSTEM_MESSAGE_MODELS: [&str; 1] = ["claude-opus-5-5"];

fn reminder() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?s)<system-reminder>.*?</system-reminder>").unwrap())
}

// --- Request inspection and rewriting ------------------------------------------------

/// Claude Code converts draft-04 boolean exclusiveMinimum/Maximum in MCP tool schemas only
/// when talking to Anthropic directly; behind a base URL the API would reject them.
pub fn sanitize_schema(node: &mut Value) {
    match node {
        Value::Array(items) => items.iter_mut().for_each(sanitize_schema),
        Value::Object(map) => {
            for (key, bound) in [
                ("exclusiveMinimum", "minimum"),
                ("exclusiveMaximum", "maximum"),
            ] {
                if let Some(Value::Bool(flag)) = map.get(key).cloned() {
                    if flag && map.get(bound).is_some_and(Value::is_number) {
                        let value = map.shift_remove(bound).unwrap();
                        map.insert(key.into(), value);
                    } else {
                        map.shift_remove(key);
                    }
                }
            }
            map.values_mut().for_each(sanitize_schema);
        }
        _ => {}
    }
}

pub fn text_of(content: &Value) -> String {
    let text = match content {
        Value::String(s) => s.clone(),
        Value::Array(blocks) => blocks
            .iter()
            .filter(|b| b["type"] == "text")
            .map(|b| b["text"].as_str().unwrap_or(""))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    };
    // Claude Code's <system-reminder> blocks are noise to a router.
    reminder().replace_all(&text, "").trim().to_string()
}

fn messages_of(body: &Value) -> &[Value] {
    body["messages"].as_array().map_or(&[], Vec::as_slice)
}

fn has_tools(body: &Value) -> bool {
    body["tools"].as_array().is_some_and(|t| !t.is_empty())
}

/// Prompts Claude Code writes itself (prompt suggestions, the away recap); they arrive with
/// tools like a user turn. Re-check these markers when Claude Code changes version.
const INTERNAL_MARKERS: [&str; 2] = [
    "[SUGGESTION MODE:",
    "The user stepped away and is coming back.",
];

pub fn is_internal(text: &str) -> bool {
    INTERNAL_MARKERS
        .iter()
        .any(|m| text.trim_start().starts_with(m))
}

/// "continue", "yes, do it", "ok go ahead": the user is carrying on the current task.
/// ponytail: fixed English vocabulary; anything else goes to Jev as usual.
pub fn is_continuation(text: &str) -> bool {
    const WORDS: [&str; 26] = [
        "yes", "y", "yep", "yeah", "ok", "okay", "sure", "please", "continue", "go", "on", "ahead",
        "do", "it", "proceed", "keep", "going", "carry", "sounds", "good", "lgtm", "that", "then",
        "now", "and", "thanks",
    ];
    let lower = text.to_lowercase();
    let words: Vec<&str> = lower
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .collect();
    !words.is_empty() && text.len() <= 40 && words.iter().all(|w| WORDS.contains(w))
}

/// Index of the user message this request answers. Claude Code appends mid-conversation
/// `role: "system"` messages (hook output, reminders) after it, so skip those.
pub fn latest_user(messages: &[Value]) -> Option<usize> {
    let i = messages.iter().rposition(|m| m["role"] != "system")?;
    (messages[i]["role"] == "user").then_some(i)
}

/// Text of a genuinely new user turn, or None for tool-loop continuations and Claude
/// Code's auxiliary (tool-less) calls.
pub fn fresh_prompt(body: &Value) -> Option<String> {
    let messages = messages_of(body);
    let i = latest_user(messages)?;
    if !has_tools(body) {
        return None;
    }
    let content = &messages[i]["content"];
    if content
        .as_array()
        .is_some_and(|blocks| blocks.iter().any(|b| b["type"] == "tool_result"))
    {
        return None;
    }
    Some(text_of(content)).filter(|t| !t.is_empty())
}

pub fn history_of(body: &Value) -> Vec<Value> {
    let messages = messages_of(body);
    let end = latest_user(messages).unwrap_or(messages.len());
    messages[..end]
        .iter()
        .filter_map(|m| {
            let role = m["role"].as_str()?;
            let text = text_of(&m["content"]);
            (matches!(role, "user" | "assistant") && !text.is_empty())
                .then(|| json!({"role": role, "text": text}))
        })
        .collect()
}

pub fn session_of(body: &Value) -> Option<String> {
    let user_id: Value = serde_json::from_str(body["metadata"]["user_id"].as_str()?).ok()?;
    user_id["session_id"].as_str().map(str::to_string)
}

/// Session id + first message text: stable across a conversation, and different for each
/// sub-agent, so a sub-agent's rung never leaks into the main conversation.
pub fn conversation_key(body: &Value) -> String {
    let first = messages_of(body)
        .first()
        .map_or(Value::Null, |m| m["content"].clone());
    let mut h = DefaultHasher::new();
    format!(
        "{}|{}",
        session_of(body).unwrap_or_default(),
        text_of(&first)
    )
    .hash(&mut h);
    format!("{:016x}", h.finish())[..12].to_string()
}

/// For Sonnet 5 / Haiku 4.5, rewrite what Claude Code would have sent them natively: system
/// messages become <system-reminder> user text, and a tool_addition becomes the tool no
/// longer being deferred. The API merges the resulting consecutive user turns.
pub fn fold_system_messages(body: &mut Value) {
    let mut undefer = Vec::new();
    let mut folded = Vec::new();
    for mut message in messages_of(body).to_vec() {
        let system = message["role"] == "system";
        let content = match &message["content"] {
            Value::String(s) => vec![json!({"type": "text", "text": s})],
            Value::Array(blocks) => blocks.clone(),
            _ => Vec::new(),
        };
        let mut kept = Vec::new();
        for mut block in content {
            match block["type"].as_str() {
                Some(kind @ ("tool_addition" | "tool_removal")) => {
                    // ponytail: removals keep the tool loaded.
                    if kind == "tool_addition" {
                        if let Some(name) = block["tool"]["name"].as_str() {
                            undefer.push(name.to_string());
                        }
                    }
                    continue;
                }
                Some("text") if system => {
                    let text = block["text"].as_str().unwrap_or("").to_string();
                    block["text"] = json!(format!("<system-reminder>\n{text}\n</system-reminder>"));
                }
                _ => {}
            }
            kept.push(block);
        }
        if system {
            if kept.is_empty() {
                continue;
            }
            message["role"] = json!("user");
        }
        if let Some(m) = message.as_object_mut() {
            m.insert("content".into(), Value::Array(kept));
        }
        folded.push(message);
    }
    body["messages"] = Value::Array(folded);
    for tool in body
        .get_mut("tools")
        .and_then(Value::as_array_mut)
        .into_iter()
        .flatten()
    {
        if tool["name"]
            .as_str()
            .is_some_and(|n| undefer.iter().any(|u| u == n))
        {
            if let Some(t) = tool.as_object_mut() {
                t.shift_remove("defer_loading");
            }
        }
    }
}

/// Point the request at a rung. Claude Code composed it for "jev-router" (declared with
/// thinking + effort), so fields the target model rejects must go.
pub fn apply_rung(body: &mut Value, rank: usize) {
    let rung = &LADDER[rank];
    body["model"] = json!(rung.model);
    if !SYSTEM_MESSAGE_MODELS.contains(&rung.model) {
        fold_system_messages(body);
    }
    let obj = body.as_object_mut().expect("request body is an object");
    if let Some(effort) = rung.effort {
        match obj.get_mut("output_config") {
            Some(Value::Object(config)) => {
                config.insert("effort".into(), json!(effort));
            }
            _ => {
                obj.insert("output_config".into(), json!({"effort": effort}));
            }
        }
        return;
    }
    // Haiku 4.5: adaptive thinking and effort are both 400s.
    obj.shift_remove("thinking");
    let empty_config = match obj.get_mut("output_config") {
        Some(Value::Object(config)) => {
            config.shift_remove("effort");
            config.is_empty()
        }
        _ => false,
    };
    if empty_config {
        obj.shift_remove("output_config");
    }
    let mut drop_edits = false;
    if let Some(edits) = obj
        .get_mut("context_management")
        .and_then(|c| c.get_mut("edits"))
        .and_then(Value::as_array_mut)
    {
        edits.retain(|e| !e["type"].as_str().unwrap_or("").contains("thinking"));
        drop_edits = edits.is_empty();
    }
    if drop_edits {
        obj.shift_remove("context_management");
    }
}

// --- Proxy server --------------------------------------------------------------------

pub type DecideFn =
    fn(&str, &[Value], Map<String, Value>, &str, Option<&str>) -> Result<Value, String>;

#[derive(Clone, Default)]
struct Conversation {
    rung: Option<usize>,
    decision: Option<String>,
    turn: Option<(Option<usize>, String)>,
}

/// Per-conversation rung state plus the rewrite step. `decide` is jev::decide, passed in so
/// this module has no Jev or logging dependency.
pub struct Router {
    decide: DecideFn,
    status_dir: PathBuf,
    debug_log: PathBuf,
    usage_log: PathBuf,
    conversations: Mutex<(HashMap<String, Conversation>, VecDeque<String>)>,
    log_lock: Mutex<()>,
}

impl Router {
    pub fn new(
        decide: DecideFn,
        status_dir: PathBuf,
        debug_log: PathBuf,
        usage_log: PathBuf,
    ) -> Self {
        Router {
            decide,
            status_dir,
            debug_log,
            usage_log,
            conversations: Mutex::default(),
            log_lock: Mutex::default(),
        }
    }

    pub fn debug(&self, line: &str) {
        let _guard = self.log_lock.lock().unwrap_or_else(|e| e.into_inner());
        if let Ok(mut f) = OpenOptions::new()
            .append(true)
            .create(true)
            .open(&self.debug_log)
        {
            let _ = writeln!(f, "{} {line}", local_hms());
        }
    }

    /// One line per Claude request in usage.jsonl: the tag from `rewrite` plus the token
    /// usage the API reported. The dashboard prices it (actual vs. without the router).
    fn record_usage(&self, tag: &Value, usage: Value, served_model: Option<String>) {
        let mut record = json!({"ts": utc_iso()});
        if let (Some(r), Some(t)) = (record.as_object_mut(), tag.as_object()) {
            r.extend(t.clone());
            r.insert("served_model".into(), json!(served_model));
            r.insert("usage".into(), usage);
        }
        let _guard = self.log_lock.lock().unwrap_or_else(|e| e.into_inner());
        if let Ok(mut f) = OpenOptions::new()
            .append(true)
            .create(true)
            .mode(0o600)
            .open(&self.usage_log)
        {
            let _ = writeln!(f, "{record}");
        }
    }

    fn status(&self, session: Option<&str>, value: Value) {
        if let Some(session) = session {
            let _ = fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(&self.status_dir);
            let _ = fs::write(
                self.status_dir.join(format!("{session}.json")),
                value.to_string(),
            );
        }
    }

    fn load(&self, key: &str) -> Conversation {
        let mut guard = self.conversations.lock().unwrap_or_else(|e| e.into_inner());
        let (map, order) = &mut *guard;
        if !map.contains_key(key) {
            map.insert(key.to_string(), Conversation::default());
            order.push_back(key.to_string());
            if map.len() > MAX_CONVERSATIONS {
                if let Some(oldest) = order.pop_front() {
                    map.remove(&oldest);
                }
            }
        }
        map.get(key).cloned().unwrap_or_default()
    }

    fn store(&self, key: &str, state: Conversation) {
        let mut guard = self.conversations.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(slot) = guard.0.get_mut(key) {
            *slot = state;
        }
    }

    /// Rewrite a /v1/messages body in place. Returns a proxy.log note and the usage tag
    /// (kind: routed | pinned | aux | manual, plus model, rung, decision, conversation).
    pub fn rewrite(&self, body: &mut Value) -> (String, Value) {
        if let Some(dump) = std::env::var_os("JEV_ROUTER_DUMP") {
            // Request shape is undocumented and moves.
            let dir = PathBuf::from(dump);
            let _ = fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(&dir);
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos();
            let _ = fs::write(
                dir.join(format!("{nanos}.json")),
                serde_json::to_string_pretty(body).unwrap_or_default(),
            );
        }
        for tool in body
            .get_mut("tools")
            .and_then(Value::as_array_mut)
            .into_iter()
            .flatten()
        {
            if let Some(schema) = tool.get_mut("input_schema") {
                sanitize_schema(schema);
            }
        }
        let session = session_of(body);
        let tools = has_tools(body);
        if body["model"] != AUTO_MODEL {
            if tools {
                // A real agent turn on a model the user picked.
                self.status(session.as_deref(), json!({"manual": body["model"]}));
            }
            let tag = json!({"kind": "manual", "model": body["model"], "rung": null, "decision": null,
                             "conversation": null, "session_id": session});
            return (
                format!("passthrough {}", body["model"].as_str().unwrap_or("None")),
                tag,
            );
        }
        let key = conversation_key(body);
        let mut state = self.load(&key);
        let mut prompt = fresh_prompt(body);
        // Claude Code's own side requests (suggestions, recaps) and "continue"-style replies
        // skip Jev and keep the pinned rung: no extra latency, and no cache rebuild.
        let internal = prompt.as_deref().is_some_and(is_internal);
        if internal || (state.rung.is_some() && prompt.as_deref().is_some_and(is_continuation)) {
            prompt = None;
        }
        // Claude Code retries a failed request with the same prompt: keep that turn's rung.
        let turn = (
            latest_user(messages_of(body)),
            prompt.clone().unwrap_or_default(),
        );
        if prompt.is_some() && state.turn.as_ref() == Some(&turn) && state.rung.is_some() {
            prompt = None;
        }
        // Debug: skip Jev and use this rung.
        let forced = std::env::var("JEV_ROUTER_FORCE_RUNG")
            .ok()
            .as_deref()
            .and_then(rank_of);
        if let Some(text) = &prompt {
            state.turn = Some(turn);
            if forced.is_some() {
                state.rung = forced;
                state.decision = None;
            } else {
                let mut facts = Map::new();
                facts.insert(
                    "previous_rung".into(),
                    json!(state.rung.map(|r| LADDER[r].name)),
                );
                let size = serde_json::to_string(&body["messages"]).map_or(0, |s| s.len());
                facts.insert("context_tokens".into(), json!(size / 4));
                facts.insert("conversation".into(), json!(key));
                match (self.decide)(text, &history_of(body), facts, "proxy", session.as_deref()) {
                    Ok(record) => {
                        state.rung = record["final"]["rung"].as_str().and_then(rank_of);
                        state.decision = record["id"].as_str().map(str::to_string);
                    }
                    Err(error) => self.debug(&format!("{key} routing failed: {error}")),
                }
            }
        }
        let rung = state.rung.unwrap_or(if tools && !internal {
            AGENT_FALLBACK
        } else {
            AUX_RUNG
        });
        apply_rung(body, rung);
        if prompt.is_some() {
            self.status(
                session.as_deref(),
                json!({"rung": LADDER[rung].name, "decision": state.decision}),
            );
        }
        let kind = match (prompt.is_some(), tools) {
            (true, _) => "routed",
            (false, true) => "pinned",
            (false, false) => "aux",
        };
        let tag = json!({"kind": kind, "model": LADDER[rung].model, "rung": LADDER[rung].name,
                         "decision": state.decision, "conversation": key, "session_id": session});
        self.store(&key, state);
        let note = format!(
            "{key} {} -> {}",
            if prompt.is_some() { "routed" } else { "pinned" },
            LADDER[rung].name
        );
        (note, tag)
    }
}

/// Start the proxy on a random loopback port. Returns the port; serves until process exit.
pub fn start_proxy(router: Arc<Router>, upstream: String) -> io::Result<u16> {
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let port = listener.local_addr()?.port();
    let agent = ureq::AgentBuilder::new()
        .redirects(0)
        .timeout_connect(Duration::from_secs(30))
        .timeout_read(Duration::from_secs(600))
        .build();
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let (router, agent, upstream) = (router.clone(), agent.clone(), upstream.clone());
            std::thread::spawn(move || {
                let _ = serve(stream, &router, &agent, &upstream);
            });
        }
    });
    Ok(port)
}

fn read_chunked(reader: &mut impl BufRead) -> io::Result<Vec<u8>> {
    let mut body = Vec::new();
    loop {
        let mut line = String::new();
        reader.read_line(&mut line)?;
        let size = usize::from_str_radix(line.trim().split(';').next().unwrap_or("0"), 16)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        if size == 0 {
            loop {
                // Trailers, up to the blank line.
                let mut trailer = String::new();
                if reader.read_line(&mut trailer)? == 0 || trailer.trim().is_empty() {
                    return Ok(body);
                }
            }
        }
        let start = body.len();
        body.resize(start + size, 0);
        reader.read_exact(&mut body[start..])?;
        reader.read_line(&mut line)?;
    }
}

/// One keep-alive client connection: parse each request, forward it, relay the response.
fn serve(
    stream: TcpStream,
    router: &Router,
    agent: &ureq::Agent,
    upstream: &str,
) -> io::Result<()> {
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut writer = stream;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line)? == 0 {
            return Ok(());
        }
        let line = line.trim_end();
        if line.is_empty() {
            continue;
        }
        let mut parts = line.split(' ');
        let (method, path) = (
            parts.next().unwrap_or("GET").to_string(),
            parts.next().unwrap_or("/").to_string(),
        );
        let mut headers: Vec<(String, String)> = Vec::new();
        loop {
            let mut header = String::new();
            if reader.read_line(&mut header)? == 0 {
                return Ok(());
            }
            let header = header.trim_end_matches(['\r', '\n']);
            if header.is_empty() {
                break;
            }
            if let Some((k, v)) = header.split_once(':') {
                headers.push((k.trim().to_string(), v.trim().to_string()));
            }
        }
        let get = |name: &str| {
            headers
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case(name))
                .map(|(_, v)| v.as_str())
        };
        let body = if get("transfer-encoding").is_some_and(|v| v.eq_ignore_ascii_case("chunked")) {
            read_chunked(&mut reader)?
        } else {
            let mut body = vec![
                0;
                get("content-length")
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(0)
            ];
            reader.read_exact(&mut body)?;
            body
        };
        let close = get("connection").is_some_and(|v| v.eq_ignore_ascii_case("close"));
        if method == "HEAD" {
            // Claude Code probes the base URL before its first request.
            writer.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n")?;
        } else {
            forward(
                &mut writer,
                router,
                agent,
                upstream,
                &method,
                &path,
                &headers,
                body,
            )?;
        }
        writer.flush()?;
        if close {
            return Ok(());
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn forward(
    writer: &mut TcpStream,
    router: &Router,
    agent: &ureq::Agent,
    upstream: &str,
    method: &str,
    path: &str,
    headers: &[(String, String)],
    mut body: Vec<u8>,
) -> io::Result<()> {
    let mut note = None;
    let mut tag = None;
    if path.starts_with("/v1/messages") && !body.is_empty() {
        note = Some(match serde_json::from_slice::<Value>(&body) {
            Ok(mut data) if data.is_object() => {
                // A bug in the rewrite must never take the session down: forward as-is.
                match catch_unwind(AssertUnwindSafe(|| router.rewrite(&mut data))) {
                    Ok((n, t)) => {
                        body = serde_json::to_vec(&data).unwrap_or(body);
                        tag = Some(t);
                        n
                    }
                    Err(_) => "could not rewrite: panic in rewrite".to_string(),
                }
            }
            Ok(_) => "could not rewrite: body is not a JSON object".to_string(),
            Err(e) => format!("could not rewrite: JSONDecodeError: {e}"),
        });
    }
    // Billed message requests are tapped for token usage, which needs an uncompressed reply;
    // with JEV_ROUTER_DUMP every reply is uncompressed so errors are readable in proxy.log.
    let tap = tag.is_some() && !path.starts_with("/v1/messages/count_tokens");
    let dump = tap || std::env::var_os("JEV_ROUTER_DUMP").is_some();
    let mut merged: Vec<(String, String)> = Vec::new(); // ureq `set` replaces, so join repeats
    for (k, v) in headers {
        let lower = k.to_ascii_lowercase();
        if HOP_HEADERS.contains(&lower.as_str()) || (dump && lower == "accept-encoding") {
            continue;
        }
        match merged.iter_mut().find(|(m, _)| m.eq_ignore_ascii_case(k)) {
            Some((_, existing)) => {
                existing.push_str(", ");
                existing.push_str(v);
            }
            None => merged.push((k.clone(), v.clone())),
        }
    }
    let url = format!("{}{path}", upstream.trim_end_matches('/'));
    let mut request = agent.request(method, &url);
    for (k, v) in &merged {
        request = request.set(k, v);
    }
    let result = if body.is_empty() && matches!(method, "GET" | "DELETE" | "OPTIONS") {
        request.call()
    } else {
        request.send_bytes(&body)
    };
    let response = match result {
        Ok(r) | Err(ureq::Error::Status(_, r)) => r,
        Err(ureq::Error::Transport(error)) => {
            router.debug(&format!("upstream error: {error}"));
            let payload =
                json!({"type": "error", "error": {"message": error.to_string()}}).to_string();
            let head = format!(
                "HTTP/1.1 502 Bad Gateway\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n",
                payload.len()
            );
            writer.write_all(head.as_bytes())?;
            return writer.write_all(payload.as_bytes());
        }
    };
    if let Some(note) = note {
        router.debug(&format!("{note} [{}]", response.status()));
    }
    relay(writer, response, router, tag.filter(|_| tap))
}

/// Pulls `usage` (and the served model) out of a Messages response while it is relayed:
/// SSE `message_start` / `message_delta` events, or a plain JSON body.
#[derive(Default)]
pub struct UsageTap {
    line: Vec<u8>,
    whole: Vec<u8>,
    sse: bool,
    usage: Map<String, Value>,
    model: Option<String>,
}

impl UsageTap {
    pub fn feed(&mut self, bytes: &[u8]) {
        if !self.sse && self.whole.len() + bytes.len() <= 8 << 20 {
            self.whole.extend_from_slice(bytes);
        }
        for &b in bytes {
            if b == b'\n' {
                let line = std::mem::take(&mut self.line);
                self.line_done(&line);
            } else if self.line.len() < 4 << 20 {
                self.line.push(b);
            }
        }
    }

    fn line_done(&mut self, line: &[u8]) {
        let Some(data) = line.strip_prefix(b"data:") else {
            return;
        };
        if !self.sse {
            self.sse = true;
            self.whole = Vec::new();
        }
        let Ok(event) = serde_json::from_slice::<Value>(data.trim_ascii()) else {
            return;
        };
        match event["type"].as_str() {
            Some("message_start") => {
                self.merge(&event["message"]["usage"]);
                self.model = event["message"]["model"].as_str().map(str::to_string);
            }
            Some("message_delta") => self.merge(&event["usage"]),
            _ => {}
        }
    }

    fn merge(&mut self, usage: &Value) {
        for (k, v) in usage.as_object().into_iter().flatten() {
            if !v.is_null() {
                self.usage.insert(k.clone(), v.clone());
            }
        }
    }

    pub fn finish(mut self) -> Option<(Value, Option<String>)> {
        if !self.sse {
            let body: Value = serde_json::from_slice(&self.whole).ok()?;
            self.merge(&body["usage"]);
            self.model = body["model"].as_str().map(str::to_string);
        }
        (!self.usage.is_empty()).then(|| (Value::Object(self.usage), self.model))
    }
}

/// Stream the upstream response back unchanged (SSE included).
fn relay(
    writer: &mut TcpStream,
    response: ureq::Response,
    router: &Router,
    tag: Option<Value>,
) -> io::Result<()> {
    let status = response.status();
    let mut head = format!("HTTP/1.1 {status} {}\r\n", response.status_text());
    for name in response.headers_names() {
        if HOP_HEADERS.contains(&name.to_ascii_lowercase().as_str()) {
            continue;
        }
        for value in response.all(&name) {
            head.push_str(&format!("{name}: {value}\r\n"));
        }
    }
    let encoded = response.header("content-encoding").is_some();
    let length = response
        .header("content-length")
        .and_then(|v| v.parse::<u64>().ok());
    let mut reader = response.into_reader();
    if status >= 400 {
        // A small JSON error: buffer it so it can be logged (readable when uncompressed).
        let mut data = Vec::new();
        reader.read_to_end(&mut data)?;
        if !encoded {
            router.debug(&format!(
                "  upstream {status}: {}",
                String::from_utf8_lossy(&data[..data.len().min(400)])
            ));
        }
        head.push_str(&format!("Content-Length: {}\r\n\r\n", data.len()));
        writer.write_all(head.as_bytes())?;
        return writer.write_all(&data);
    }
    let mut tap = (!encoded)
        .then_some(tag)
        .flatten()
        .map(|t| (t, UsageTap::default()));
    let mut buf = vec![0; 65536];
    if let Some(length) = length {
        head.push_str(&format!("Content-Length: {length}\r\n\r\n"));
        writer.write_all(head.as_bytes())?;
        let mut body = reader.take(length);
        loop {
            let n = body.read(&mut buf)?;
            if n == 0 {
                break;
            }
            if let Some((_, t)) = tap.as_mut() {
                t.feed(&buf[..n]);
            }
            writer.write_all(&buf[..n])?;
        }
    } else {
        head.push_str("Transfer-Encoding: chunked\r\n\r\n");
        writer.write_all(head.as_bytes())?;
        loop {
            let n = reader.read(&mut buf)?;
            if n == 0 {
                break;
            }
            if let Some((_, t)) = tap.as_mut() {
                t.feed(&buf[..n]);
            }
            write!(writer, "{n:x}\r\n")?;
            writer.write_all(&buf[..n])?;
            writer.write_all(b"\r\n")?;
            writer.flush()?;
        }
        writer.write_all(b"0\r\n\r\n")?;
    }
    if let Some((tag, t)) = tap {
        if let Some((usage, served)) = t.finish() {
            router.record_usage(&tag, usage, served);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn turn() -> Value {
        json!({
            "model": AUTO_MODEL,
            "tools": [{"name": "t", "input_schema": {"exclusiveMinimum": true, "minimum": 0}}],
            "metadata": {"user_id": json!({"session_id": "s1"}).to_string()},
            "thinking": {"type": "adaptive"},
            "output_config": {"effort": "high"},
            "context_management": {"edits": [{"type": "clear_thinking_20251015"}]},
            "messages": [
                {"role": "user", "content": "fix the bug"},
                {"role": "assistant", "content": [{"type": "text", "text": "which file?"}]},
                {"role": "user", "content": [{"type": "text", "text": "<system-reminder>x</system-reminder>utils.py"}]},
            ],
        })
    }

    #[test]
    fn self_check_matches_python_reference() {
        let mut t = turn();
        assert_eq!(fresh_prompt(&t).as_deref(), Some("utils.py"));
        assert_eq!(
            history_of(&t),
            vec![
                json!({"role": "user", "text": "fix the bug"}),
                json!({"role": "assistant", "text": "which file?"})
            ]
        );
        let mut hooked = t.clone();
        hooked["messages"]
            .as_array_mut()
            .unwrap()
            .push(json!({"role": "system", "content": "hook"}));
        assert_eq!(fresh_prompt(&hooked).as_deref(), Some("utils.py"));
        assert_eq!(history_of(&hooked), history_of(&t));

        let mut loop_ = t.clone();
        loop_["messages"]
            .as_array_mut()
            .unwrap()
            .push(json!({"role": "user", "content": [{"type": "tool_result"}]}));
        assert_eq!(fresh_prompt(&loop_), None);
        let mut aux = t.clone();
        aux["tools"] = json!([]);
        assert_eq!(fresh_prompt(&aux), None);
        assert_eq!(conversation_key(&t), conversation_key(&loop_));

        sanitize_schema(&mut t["tools"]);
        assert_eq!(
            t["tools"][0]["input_schema"],
            json!({"exclusiveMinimum": 0})
        );
        let mut haiku = t.clone();
        apply_rung(&mut haiku, H);
        assert_eq!(haiku["model"], "claude-haiku-4-5");
        for gone in ["thinking", "output_config", "context_management"] {
            assert!(haiku.get(gone).is_none(), "{gone} should be stripped");
        }
        let mut opus = t.clone();
        apply_rung(&mut opus, crate::router::O_XHIGH);
        assert_eq!(opus["model"], "claude-opus-5-5");
        assert_eq!(opus["output_config"]["effort"], "xhigh");
        assert_eq!(opus["thinking"], json!({"type": "adaptive"}));
    }

    #[test]
    fn usage_tap_reads_sse_split_across_chunks_and_plain_json() {
        let sse = concat!(
            "event: message_start\r\ndata: {\"type\":\"message_start\",\"message\":{\"model\":\"claude-haiku-4-5-20251001\",",
            "\"usage\":{\"input_tokens\":10,\"cache_read_input_tokens\":900,\"cache_creation_input_tokens\":50,\"output_tokens\":1}}}\r\n\r\n",
            "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"delta\":{\"text\":\"usage\"}}\n\n",
            "event: message_delta\ndata: {\"type\":\"message_delta\",\"usage\":{\"output_tokens\":42,\"input_tokens\":null}}\n\n",
        );
        let mut tap = UsageTap::default();
        for chunk in sse.as_bytes().chunks(7) {
            tap.feed(chunk);
        }
        let (usage, model) = tap.finish().unwrap();
        assert_eq!(model.as_deref(), Some("claude-haiku-4-5-20251001"));
        assert_eq!(
            usage,
            json!({"input_tokens": 10, "cache_read_input_tokens": 900, "cache_creation_input_tokens": 50, "output_tokens": 42})
        );

        let mut plain = UsageTap::default();
        plain.feed(br#"{"model":"claude-sonnet-5","usage":{"input_tokens":5,"output_tokens":7}}"#);
        assert_eq!(
            plain.finish().unwrap().0,
            json!({"input_tokens": 5, "output_tokens": 7})
        );
        assert!(UsageTap::default().finish().is_none());
    }

    #[test]
    fn side_requests_and_continuations_skip_jev() {
        assert!(is_internal(
            "[SUGGESTION MODE: Suggest what the user might type next.]"
        ));
        assert!(is_internal(
            "The user stepped away and is coming back. Recap in under 40 words"
        ));
        assert!(!is_internal("explain the suggestion mode"));
        for t in [
            "continue",
            "yes, do it",
            "ok go ahead",
            "Proceed.",
            "sounds good, thanks",
        ] {
            assert!(is_continuation(t), "{t}");
        }
        for t in [
            "no",
            "yes but use sqlite instead",
            "fix it",
            "continue with the auth refactor and add tests",
        ] {
            assert!(!is_continuation(t), "{t}");
        }
    }

    #[test]
    fn folds_system_messages_for_sonnet_and_haiku() {
        let real = json!({
            "model": AUTO_MODEL,
            "tools": [{"name": "Docs", "defer_loading": true}, {"name": "Bash"}],
            "messages": [
                {"role": "user", "content": "hi"},
                {"role": "system", "content": [
                    {"type": "text", "text": "hook said x"},
                    {"type": "tool_addition", "tool": {"type": "tool_reference", "name": "Docs"}},
                ]},
            ],
        });
        let mut sonnet = real.clone();
        apply_rung(&mut sonnet, crate::router::S_LOW);
        let roles: Vec<&str> = sonnet["messages"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m["role"].as_str().unwrap())
            .collect();
        assert_eq!(roles, ["user", "user"]);
        assert_eq!(
            sonnet["messages"][1]["content"],
            json!([{"type": "text", "text": "<system-reminder>\nhook said x\n</system-reminder>"}])
        );
        assert!(sonnet["tools"][0].get("defer_loading").is_none());
        let mut opus = real.clone();
        apply_rung(&mut opus, crate::router::O_HIGH);
        assert_eq!(opus["messages"][1]["role"], "system"); // Opus 5.5 takes it as-is
    }
}
