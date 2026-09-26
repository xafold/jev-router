//! Jev call, secret redaction, decision tracking and terminal views.
//! Port of the non-proxy half of router/cli.py (the reference).

use crate::router::{
    questions_v2, rank_of, requested_rung, route_v2, route_with, Answers, CLEAR_YES, QUESTIONS,
};
use crate::util::{data_dir, random_id, utc_iso};
use regex::Regex;
use serde_json::{json, Map, Value};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

const JEV_URL: &str = "https://api.typesafe.ai/v1/systemone";
/// Thresholds were tuned on this release; don't float on jev-latest.
const JEV_MODEL: &str = "jev-1.13.0";
/// Send only recent turns (rule 7: only the context the questions need).
const MAX_MESSAGES: usize = 8;
/// Per message; long pastes are cut in the middle.
const MAX_CHARS: usize = 2000;
/// Seconds per attempt; a slow Jev must not stall the turn for long.
const JEV_TIMEOUT: Duration = Duration::from_secs(8);
const JEV_RETRIES: usize = 1;
/// Repo summary sent as background (the opening of CLAUDE.md or README.md).
const SUMMARY_CHARS: usize = 800;

pub fn log_path() -> PathBuf {
    std::env::var_os("CLAUDE_ROUTER_LOG")
        .map_or_else(|| data_dir().join("decisions.jsonl"), PathBuf::from)
}

// --- Jev input -----------------------------------------------------------------------

/// ponytail: regex redaction catches common key shapes only; use a real secret scanner
/// (e.g. detect-secrets) before sharing the log or pointing this at a team.
fn secret() -> &'static Regex {
    static SECRET: OnceLock<Regex> = OnceLock::new();
    SECRET.get_or_init(|| {
        Regex::new(concat!(
            r"(?i)(\b\w*(?:api[_-]?key|secret|token|password|passwd))(\s*[:=]\s*)\S+",
            r"|\b(?:sk|pk|rk|ghp|gho|ghs|xox[abp]|AKIA|AIza|apikey)[-_A-Za-z0-9]{12,}",
            r"|\b[A-Za-z0-9_\-]{40,}\b",
        ))
        .unwrap()
    })
}

pub fn redact(text: &str) -> String {
    secret()
        .replace_all(text, |c: &regex::Captures| match (c.get(1), c.get(2)) {
            (Some(key), Some(sep)) => format!("{}{}[redacted]", key.as_str(), sep.as_str()),
            _ => "[redacted]".to_string(),
        })
        .into_owned()
}

pub fn clip(text: &str) -> String {
    let text = redact(text);
    let chars: Vec<char> = text.chars().collect();
    if chars.len() <= MAX_CHARS {
        return text;
    }
    let half = MAX_CHARS / 2;
    let head: String = chars[..half].iter().collect();
    let tail: String = chars[chars.len() - half..].iter().collect();
    format!(
        "{head}\n[... {} chars cut ...]\n{tail}",
        chars.len() - MAX_CHARS
    )
}

pub fn build_state(prompt: &str, history: &[Value], cwd: &Path) -> Value {
    let start = history.len().saturating_sub(MAX_MESSAGES - 1);
    let history: Vec<Value> = history[start..]
        .iter()
        .map(|m| json!({"role": m["role"], "text": clip(m["text"].as_str().unwrap_or(""))}))
        .collect();
    let mut environment = json!({
        "assistant": "a coding agent with the user's repository open; it can read any file, search, run commands and edit code",
        "repository": cwd.file_name().map(|n| n.to_string_lossy().into_owned()),
        "first_turn": history.is_empty(),
    });
    if let Some(summary) = repo_summary(cwd) {
        environment["repository_summary"] = json!(summary);
    }
    json!({"environment": environment, "history": history, "latest": clip(prompt)})
}

/// What the repository is, so "this project" isn't read literally: the opening of
/// CLAUDE.md (else README.md) up to the first `##` heading, without headings, clipped and
/// redacted. Off with JEV_ROUTER_REPO_SUMMARY=off (it is sent to api.typesafe.ai).
pub fn repo_summary(cwd: &Path) -> Option<String> {
    if std::env::var("JEV_ROUTER_REPO_SUMMARY").is_ok_and(|v| v == "off") {
        return None;
    }
    let text = ["CLAUDE.md", "README.md"]
        .iter()
        .find_map(|f| fs::read_to_string(cwd.join(f)).ok())?;
    let intro: Vec<&str> = text
        .lines()
        .skip_while(|l| l.trim().is_empty() || l.starts_with("# "))
        .take_while(|l| !l.starts_with("## "))
        .filter(|l| !l.starts_with('#') && !l.contains("provides guidance to Claude Code"))
        .collect();
    let intro = intro
        .join(" ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    let intro: String = redact(&intro).chars().take(SUMMARY_CHARS).collect();
    (!intro.is_empty()).then_some(intro)
}

/// Load .env without overriding the real environment: ~/.config/jev-router/.env (for a
/// downloaded binary), then the repo's .env next to the crate (for a source build).
pub fn load_env() {
    let config = crate::util::home().join(".config/jev-router/.env");
    let repo = std::path::PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/.env"));
    let text: String = [config, repo]
        .iter()
        .filter_map(|p| fs::read_to_string(p).ok())
        .collect::<Vec<_>>()
        .join("\n");
    for line in text.lines() {
        if line.trim_start().starts_with('#') {
            continue;
        }
        if let Some((key, value)) = line.split_once('=') {
            let key = key.trim();
            if std::env::var_os(key).is_none() {
                std::env::set_var(key, value.trim().trim_matches(|c| c == '"' || c == '\''));
            }
        }
    }
}

/// v1's Nouls plus v2's questions, all in one request (speculative fan-out).
pub fn all_questions() -> Map<String, Value> {
    QUESTIONS
        .iter()
        .map(|(k, q)| ((*k).to_string(), json!({"type": "noul", "instructions": q})))
        .chain(questions_v2().into_iter().map(|(k, q)| (k.to_string(), q)))
        .collect()
}

/// One Jev call. Returns model, request_id, latency_ms, usage, `answers` (every Noul as a
/// number) and `structured` (Choice/Score answers as Jev returned them).
pub fn ask_jev(state: &Value) -> Result<Map<String, Value>, String> {
    let key = std::env::var("TYPESAFE_API_KEY")
        .map_err(|_| "TypeSafeError: TYPESAFE_API_KEY is not set")?;
    let body = json!({"state": state, "model": JEV_MODEL, "questions": all_questions()});
    let agent = ureq::AgentBuilder::new().timeout(JEV_TIMEOUT).build();
    let start = Instant::now();
    let mut attempt = 0;
    let response = loop {
        let sent = agent
            .post(JEV_URL)
            .set("Authorization", &format!("Bearer {key}"))
            .set("User-Agent", "jev-router-rs/0.1")
            .send_json(&body);
        // Same retryable set as the Python SDK: 408, 429, 5xx, and transport errors.
        let retry = match sent.as_ref().map_err(|e| match e {
            ureq::Error::Status(c, _) => Some(*c),
            ureq::Error::Transport(_) => None,
        }) {
            Ok(_) => false,
            Err(Some(code)) => code == 408 || code == 429 || code >= 500,
            Err(None) => true, // transport error
        };
        if !retry || attempt >= JEV_RETRIES {
            break sent;
        }
        attempt += 1;
        std::thread::sleep(Duration::from_millis(500));
    };
    let response = response.map_err(|error| match error {
        ureq::Error::Status(code, r) => {
            format!(
                "TypeSafeAPIError: HTTP {code}: {}",
                r.into_string()
                    .unwrap_or_default()
                    .chars()
                    .take(300)
                    .collect::<String>()
            )
        }
        ureq::Error::Transport(t) => format!("TypeSafeAPIConnectionError: {t}"),
    })?;
    let request_id = response.header("x-typesafe-request-id").map(str::to_string);
    let reply: Value = response
        .into_json()
        .map_err(|e| format!("TypeSafeAPIResponseValidationError: {e}"))?;
    let mut answers = Answers::new();
    let mut structured = Map::new();
    for (k, q) in all_questions() {
        let a = &reply["answers"][&k];
        let bad = || {
            format!(
                "TypeSafeAPIResponseValidationError: no {} answer for {k}",
                q["type"]
            )
        };
        match q["type"].as_str() {
            Some("noul") => {
                answers.insert(k.clone(), json!(a["noul"].as_f64().ok_or_else(bad)?));
            }
            Some("choice") if a["probabilities"].is_object() => {
                structured.insert(k.clone(), json!({"choice": a["choice"], "probabilities": a["probabilities"], "confidence": a["confidence"]}));
            }
            Some("score") if a["score"].is_f64() => {
                structured.insert(k.clone(), json!({"score": a["score"], "probabilities": a["probabilities"], "confidence": a["confidence"]}));
            }
            _ => return Err(bad()),
        }
    }
    let mut out = Map::new();
    out.insert("model".into(), reply["model"].clone());
    out.insert("request_id".into(), json!(request_id));
    out.insert(
        "latency_ms".into(),
        json!(start.elapsed().as_millis() as u64),
    );
    out.insert("usage".into(), reply["usage"].clone());
    out.insert("answers".into(), Value::Object(answers));
    out.insert("structured".into(), Value::Object(structured));
    Ok(out)
}

// --- Tracking ------------------------------------------------------------------------

/// Ask Jev, route, and append the full record to the log. On Jev failure the error record
/// is logged and the error returned. `facts` are code-known: previous_rung, context_tokens.
pub fn decide(
    prompt: &str,
    history: &[Value],
    facts: Map<String, Value>,
    source: &str,
    session: Option<&str>,
) -> Result<Value, String> {
    let mut all_facts = Map::new();
    all_facts.insert("previous_rung".into(), Value::Null);
    all_facts.insert("context_tokens".into(), json!(0));
    all_facts.extend(facts);
    let dir = std::env::current_dir().unwrap_or_default();
    let state = build_state(prompt, history, &dir);
    let questions = all_questions();
    let cwd = dir.display().to_string();
    let mut record = json!({
        "id": random_id(),
        "ts": utc_iso(),
        "source": source,
        "session_id": session,
        "cwd": cwd,
        "facts": all_facts,
        "jev": {"state": state, "questions": questions},
        "error": null,
    });
    match ask_jev(&state) {
        Ok(result) => record["jev"].as_object_mut().unwrap().extend(result),
        Err(error) => {
            record["error"] = json!(error);
            append(&record);
            return Err(error);
        }
    }
    let answers = record["jev"]["answers"].as_object().unwrap().clone();
    let previous = record["facts"]["previous_rung"].as_str().and_then(rank_of);
    let context = record["facts"]["context_tokens"].as_u64().unwrap_or(0);
    let structured = record["jev"]["structured"].as_object().unwrap().clone();
    let requested = requested_rung(prompt);
    // Both rule sets run on the same answers; one serves, the other is logged as `shadow`.
    let v2 = route_v2(&answers, &structured, previous, context, requested);
    // Auto-tuned soft knobs (defaults until enough feedback) apply to v1 only.
    let tuning = crate::autotune::current();
    let v1 = route_with(&answers, previous, context, &tuning);
    let (served, shadow) = if std::env::var("JEV_ROUTER_RULES").is_ok_and(|v| v == "v1") {
        record["tuning"] = tuning.to_json();
        (v1, v2)
    } else {
        (v2, v1)
    };
    record["shadow"] = json!({"rules": shadow.get("rules").cloned().unwrap_or(json!("v1")),
                              "final": shadow["final"], "why": shadow["why"]});
    record
        .as_object_mut()
        .unwrap()
        .extend(served.as_object().unwrap().clone());
    append(&record);
    Ok(record)
}

fn append(record: &Value) {
    let path = log_path();
    if let Some(dir) = path.parent() {
        let _ = fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir);
    }
    // Prompts are private: 0600.
    if let Ok(mut f) = OpenOptions::new()
        .append(true)
        .create(true)
        .mode(0o600)
        .open(&path)
    {
        let _ = writeln!(f, "{record}");
    }
}

pub fn read_log() -> Vec<Value> {
    fs::read_to_string(log_path())
        .unwrap_or_default()
        .lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect()
}

// --- Terminal views ------------------------------------------------------------------

fn words(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn take(text: &str, n: usize) -> String {
    text.chars().take(n).collect()
}

/// The prompt of a record; older records stored `state.messages` instead of `latest`.
fn latest_of(state: &Value) -> String {
    state["latest"]
        .as_str()
        .or_else(|| state["messages"].as_array()?.last()?["text"].as_str())
        .unwrap_or("")
        .to_string()
}

pub fn render(record: &Value) -> String {
    let mut out = vec![format!(
        "decision {}  {}  source={}  session={}",
        record["id"].as_str().unwrap_or(""),
        record["ts"].as_str().unwrap_or(""),
        record["source"].as_str().unwrap_or(""),
        record["session_id"].as_str().unwrap_or("-"),
    )];
    let jev = &record["jev"];
    let history = jev["state"]["history"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    out.push(format!(
        "\nJev input ({} history turns + latest):",
        history.len()
    ));
    let latest = json!({"role": "latest", "text": latest_of(&jev["state"])});
    for m in history.iter().chain(std::iter::once(&latest)) {
        let text = words(m["text"].as_str().unwrap_or(""));
        let more = if text.chars().count() > 160 {
            "..."
        } else {
            ""
        };
        out.push(format!(
            "  [{}] {}{more}",
            m["role"].as_str().unwrap_or(""),
            take(&text, 160)
        ));
    }
    if let Some(error) = record["error"].as_str() {
        out.push(format!("\nJev failed: {error}"));
        return out.join("\n");
    }
    out.push(format!(
        "\nJev answers ({}, {} ms, request {}):",
        jev["model"].as_str().unwrap_or("?"),
        jev["latency_ms"],
        jev["request_id"].as_str().unwrap_or("-"),
    ));
    for (key, p) in jev["answers"].as_object().into_iter().flatten() {
        let p = p.as_f64().unwrap_or(0.0);
        let label = if p > CLEAR_YES {
            "yes"
        } else if p < 1.0 - CLEAR_YES {
            "no"
        } else {
            "unsure"
        };
        let bar = "#".repeat((p * 20.0).round_ties_even() as usize);
        out.push(format!("  {key:<16}{p:5.2}  {bar:<20}  {label}"));
    }
    if record["rules"] == "v2" {
        let st = &jev["structured"];
        let i = &record["intent"];
        out.push(format!(
            "\nRequest type: {} (confidence {:.2}, runner-up {})",
            i["choice"].as_str().unwrap_or("?"),
            i["confidence"].as_f64().unwrap_or(0.0),
            i["runner_up"].as_str().unwrap_or("-"),
        ));
        for k in ["depth", "breadth"] {
            out.push(format!(
                "  {k:<16}{:5.2} of 3",
                st[k]["score"].as_f64().unwrap_or(0.0)
            ));
        }
        out.push("\nRules (v2):".into());
        for reason in record["why"].as_array().into_iter().flatten() {
            out.push(format!("  - {}", reason.as_str().unwrap_or("")));
        }
    } else {
        render_trees(record, &mut out);
    }
    if let Some(shadow) = record["shadow"]["final"]["rung"].as_str() {
        out.push(format!(
            "\n(shadow {} rules would pick {shadow})",
            record["shadow"]["rules"].as_str().unwrap_or("v1")
        ));
    }
    let f = &record["final"];
    let effort = f["effort"].as_str().unwrap_or("no effort param");
    out.push(format!(
        "\nFINAL: {}   ({}, {effort})",
        f["rung"].as_str().unwrap_or(""),
        f["model"].as_str().unwrap_or("")
    ));
    out.join("\n")
}

fn render_trees(record: &Value, out: &mut Vec<String>) {
    out.push("\nTrees:".into());
    for (name, tree) in record["trees"].as_object().into_iter().flatten() {
        out.push(format!("  {name}"));
        let path = tree["path"].as_array().cloned().unwrap_or_default();
        for (depth, step) in path.iter().enumerate() {
            out.push(format!(
                "{}└─ {}={:.2} -> {}",
                "  ".repeat(depth + 2),
                step["key"].as_str().unwrap_or(""),
                step["noul"].as_f64().unwrap_or(0.0),
                step["branch"].as_str().unwrap_or(""),
            ));
        }
        out.push(format!(
            "{}└─ vote: {}",
            "  ".repeat(path.len() + 2),
            tree["vote"].as_str().unwrap_or("")
        ));
    }
    let votes: Vec<&str> = record["votes"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect();
    out.push(format!("\nVotes (low -> high): {}", votes.join(", ")));
    for reason in record["why"].as_array().into_iter().flatten() {
        out.push(format!("  - {}", reason.as_str().unwrap_or("")));
    }
}

pub fn one_line(record: &Value) -> String {
    let rung = record["final"]["rung"].as_str().unwrap_or("ERROR");
    format!(
        "{}  {}  {:<6}  {rung:<16}  {}",
        record["id"].as_str().unwrap_or(""),
        take(record["ts"].as_str().unwrap_or(""), 19),
        record["source"].as_str().unwrap_or(""),
        take(&words(&latest_of(&record["jev"]["state"])), 60),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redaction_matches_python_reference() {
        assert_eq!(
            redact("here is the api_key please store it apikey_abc123DEF456ghi789"),
            "here is the api_key please store it [redacted]"
        );
        assert_eq!(
            redact("TYPESAFE_API_KEY=ts_live_9f8e7d6c5b4a3210fedcba98"),
            "TYPESAFE_API_KEY=[redacted]"
        );
        assert_eq!(
            redact("export GITHUB_TOKEN = \"ghp_x\""),
            "export GITHUB_TOKEN = [redacted]"
        );
        assert_eq!(redact("token: abc.def"), "token: [redacted]");
        assert_eq!(
            redact("normal text about tokens and passwords"),
            "normal text about tokens and passwords"
        );
        assert_eq!(redact("sk-ant-api03-AbCdEfGhIjKlMnOpQrSt"), "[redacted]");
    }

    #[test]
    fn clip_cuts_the_middle_by_chars() {
        let long = "é".repeat(2500);
        let clipped = clip(&long);
        assert!(clipped.starts_with(&"é".repeat(1000)));
        assert!(clipped.contains("[... 500 chars cut ...]"));
        assert!(clipped.ends_with(&"é".repeat(1000)));
    }
}
