//! Which concrete Claude model each family (haiku, sonnet, opus, fable) runs on.
//!
//! Rungs name a family, never a version. The newest model of each family comes from the
//! Anthropic Models API (GET /v1/models), fetched with the credentials Claude Code already
//! sends through the proxy, together with the effort levels and thinking modes it accepts.
//! The result is cached for a day in models.json. Until a lookup succeeds, or when
//! JEV_ROUTER_MODELS=builtin pins them, the built-in models below are used.

use crate::util::data_dir;
use serde_json::{json, Map, Value};
use std::fs;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{OnceLock, RwLock};
use std::time::{SystemTime, UNIX_EPOCH};

pub const FAMILIES: [&str; 4] = ["haiku", "sonnet", "opus", "fable"];
pub const EFFORTS: [&str; 5] = ["low", "medium", "high", "xhigh", "max"];
const REFRESH_AFTER_SECS: u64 = 24 * 3600;
/// After a failed lookup, try again in an hour rather than on every request.
const RETRY_AFTER_SECS: u64 = 3600;

#[derive(Clone, Debug, PartialEq)]
pub struct Model {
    pub id: String,
    /// Display name without the "Claude " prefix, e.g. "Opus 5.5".
    pub name: String,
    /// Effort levels the model accepts, low -> max. Empty: send no effort.
    pub efforts: Vec<String>,
    /// Accepts `thinking: {type: "adaptive"}`. False: thinking is stripped.
    pub adaptive: bool,
    /// Context window (max input tokens).
    pub context: u64,
}

impl Model {
    pub fn to_json(&self) -> Value {
        json!({"id": self.id, "name": self.name, "efforts": self.efforts,
               "adaptive": self.adaptive, "context": self.context})
    }

    fn from_json(v: &Value, fallback: &Model) -> Option<Model> {
        Some(Model {
            id: v["id"].as_str()?.to_string(),
            name: v["name"].as_str().unwrap_or(&fallback.name).to_string(),
            efforts: v["efforts"]
                .as_array()
                .map_or(fallback.efforts.clone(), |a| {
                    a.iter()
                        .filter_map(|e| e.as_str().map(str::to_string))
                        .collect()
                }),
            adaptive: v["adaptive"].as_bool().unwrap_or(fallback.adaptive),
            context: v["context"].as_u64().unwrap_or(fallback.context),
        })
    }
}

/// Models known when this build was made. Used until the Models API answers.
pub fn builtin(family: &str) -> Model {
    let all: Vec<String> = EFFORTS.iter().map(|e| e.to_string()).collect();
    let (id, name, efforts, adaptive, context) = match family {
        "haiku" => ("claude-haiku-4-5", "Haiku 4.5", Vec::new(), false, 200_000),
        "sonnet" => ("claude-sonnet-5", "Sonnet 5", all, true, 1_000_000),
        "opus" => ("claude-opus-5-5", "Opus 5.5", all, true, 1_000_000),
        _ => ("claude-fable-5-1", "Fable 5.1", all, true, 1_000_000),
    };
    Model {
        id: id.into(),
        name: name.into(),
        efforts,
        adaptive,
        context,
    }
}

struct State {
    models: Vec<Model>, // indexed like FAMILIES
    fetched_at: u64,
    error: Option<String>,
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// Built-in only: unit tests, and anyone who sets JEV_ROUTER_MODELS=builtin to opt out.
pub fn pinned() -> bool {
    cfg!(test) || std::env::var("JEV_ROUTER_MODELS").is_ok_and(|v| v == "builtin")
}

fn cache_path() -> std::path::PathBuf {
    data_dir().join("models.json")
}

fn state() -> &'static RwLock<State> {
    static STATE: OnceLock<RwLock<State>> = OnceLock::new();
    STATE.get_or_init(|| {
        let mut state = State {
            models: FAMILIES.iter().map(|f| builtin(f)).collect(),
            fetched_at: 0,
            error: None,
        };
        if !pinned() {
            if let Some(cache) = fs::read_to_string(cache_path())
                .ok()
                .and_then(|t| serde_json::from_str::<Value>(&t).ok())
            {
                for (i, family) in FAMILIES.iter().enumerate() {
                    if let Some(m) = Model::from_json(&cache["families"][family], &state.models[i])
                    {
                        state.models[i] = m;
                    }
                }
                state.fetched_at = cache["fetched_at"].as_u64().unwrap_or(0);
                state.error = cache["error"].as_str().map(str::to_string);
            }
        }
        RwLock::new(state)
    })
}

/// The model a family runs on right now.
pub fn model(family: &str) -> Model {
    let i = FAMILIES.iter().position(|f| *f == family).unwrap_or(0);
    state().read().unwrap_or_else(|e| e.into_inner()).models[i].clone()
}

/// Effort to send for a rung that wants `wanted`: the same level if the model has it,
/// else the highest level below it, else its lowest level. None if it takes no effort.
pub fn effort_for(model: &Model, wanted: &str) -> Option<String> {
    let rank = |e: &str| EFFORTS.iter().position(|x| *x == e);
    let want = rank(wanted)?;
    let mut have: Vec<(usize, &String)> = model
        .efforts
        .iter()
        .filter_map(|e| Some((rank(e)?, e)))
        .collect();
    have.sort();
    have.iter()
        .rev()
        .find(|(r, _)| *r <= want)
        .or(have.first())
        .map(|(_, e)| (*e).clone())
}

/// Everything the dashboard and `jev-router models` show.
pub fn status() -> Value {
    let s = state().read().unwrap_or_else(|e| e.into_inner());
    let families: Map<String, Value> = FAMILIES
        .iter()
        .zip(&s.models)
        .map(|(f, m)| ((*f).to_string(), m.to_json()))
        .collect();
    json!({"families": families, "fetched_at": s.fetched_at, "error": s.error,
           "pinned": pinned(), "refresh_after_secs": REFRESH_AFTER_SECS})
}

/// Newest model per family from a Models API listing: latest `created_at`, and on a tie
/// the shorter id (the alias rather than its dated snapshot). Capabilities the listing
/// doesn't report keep the family's current values.
pub fn pick(listing: &[Value], current: &[Model]) -> Vec<Model> {
    FAMILIES
        .iter()
        .enumerate()
        .map(|(i, family)| {
            let prefix = format!("claude-{family}-");
            let best = listing
                .iter()
                .filter(|m| m["id"].as_str().is_some_and(|id| id.starts_with(&prefix)))
                .max_by(|a, b| {
                    let key = |m: &Value| {
                        (
                            m["created_at"].as_str().unwrap_or("").to_string(),
                            std::cmp::Reverse(m["id"].as_str().unwrap_or("").len()),
                        )
                    };
                    key(a).cmp(&key(b))
                });
            let Some(m) = best else {
                return current[i].clone();
            };
            let caps = &m["capabilities"];
            let efforts = if caps["effort"].is_object() {
                if caps["effort"]["supported"] == true {
                    EFFORTS
                        .iter()
                        .filter(|e| caps["effort"][**e]["supported"] == true)
                        .map(|e| e.to_string())
                        .collect()
                } else {
                    Vec::new()
                }
            } else {
                current[i].efforts.clone()
            };
            let adaptive = caps["thinking"]["types"]["adaptive"]["supported"]
                .as_bool()
                .unwrap_or(current[i].adaptive);
            let id = m["id"].as_str().unwrap_or_default().to_string();
            Model {
                name: m["display_name"]
                    .as_str()
                    .map(|n| n.trim_start_matches("Claude ").to_string())
                    .unwrap_or_else(|| id.clone()),
                id,
                efforts,
                adaptive,
                context: m["max_input_tokens"].as_u64().unwrap_or(current[i].context),
            }
        })
        .collect()
}

/// Every model the credentials can see, following `has_more` pages.
fn fetch(
    agent: &ureq::Agent,
    upstream: &str,
    auth: &[(String, String)],
) -> Result<Vec<Value>, String> {
    let mut out = Vec::new();
    let mut after: Option<String> = None;
    for _ in 0..20 {
        let mut url = format!("{}/v1/models?limit=100", upstream.trim_end_matches('/'));
        if let Some(id) = &after {
            url.push_str(&format!("&after_id={id}"));
        }
        let mut request = agent.get(&url);
        for (k, v) in auth {
            request = request.set(k, v);
        }
        let page: Value = request
            .call()
            .map_err(|e| format!("GET /v1/models: {e}"))?
            .into_json()
            .map_err(|e| format!("GET /v1/models: bad JSON: {e}"))?;
        out.extend(page["data"].as_array().cloned().unwrap_or_default());
        match (page["has_more"].as_bool(), page["last_id"].as_str()) {
            (Some(true), Some(last)) => after = Some(last.to_string()),
            _ => return Ok(out),
        }
    }
    Ok(out)
}

/// The request headers that authenticate a Models API call: the API key or OAuth token,
/// the API version, and only the OAuth beta (the others are for /v1/messages).
fn auth_headers(headers: &[(String, String)]) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    for (k, v) in headers {
        match k.to_ascii_lowercase().as_str() {
            "x-api-key" | "authorization" | "anthropic-version" => out.push((k.clone(), v.clone())),
            "anthropic-beta" => {
                let oauth: Vec<&str> = v
                    .split(',')
                    .map(str::trim)
                    .filter(|b| b.starts_with("oauth-"))
                    .collect();
                if !oauth.is_empty() {
                    out.push(("anthropic-beta".into(), oauth.join(",")));
                }
            }
            _ => {}
        }
    }
    if !out
        .iter()
        .any(|(k, _)| k.eq_ignore_ascii_case("anthropic-version"))
    {
        out.push(("anthropic-version".into(), "2023-06-01".into()));
    }
    out
}

fn save(s: &State) {
    let families: Map<String, Value> = FAMILIES
        .iter()
        .zip(&s.models)
        .map(|(f, m)| ((*f).to_string(), m.to_json()))
        .collect();
    let record = json!({"fetched_at": s.fetched_at, "error": s.error, "families": families});
    let _ = fs::write(
        cache_path(),
        serde_json::to_string_pretty(&record).unwrap_or_default() + "\n",
    );
}

/// Called with each /v1/messages request's headers. When the cache is a day old, looks the
/// models up again in the background; the request itself never waits for it.
pub fn refresh_if_stale(agent: &ureq::Agent, upstream: &str, headers: &[(String, String)]) {
    static RUNNING: AtomicBool = AtomicBool::new(false);
    let fetched_at = state().read().unwrap_or_else(|e| e.into_inner()).fetched_at;
    if pinned()
        || now().saturating_sub(fetched_at) < REFRESH_AFTER_SECS
        || RUNNING.swap(true, Ordering::SeqCst)
    {
        return;
    }
    let (agent, upstream, auth) = (agent.clone(), upstream.to_string(), auth_headers(headers));
    std::thread::spawn(move || {
        let result = fetch(&agent, &upstream, &auth);
        let mut s = state().write().unwrap_or_else(|e| e.into_inner());
        match result {
            Ok(listing) if !listing.is_empty() => {
                s.models = pick(&listing, &s.models);
                s.fetched_at = now();
                s.error = None;
            }
            Ok(_) => {
                s.fetched_at = now() - REFRESH_AFTER_SECS + RETRY_AFTER_SECS;
                s.error = Some("GET /v1/models returned no models".into());
            }
            Err(error) => {
                s.fetched_at = now() - REFRESH_AFTER_SECS + RETRY_AFTER_SECS;
                s.error = Some(error);
            }
        }
        save(&s);
        drop(s);
        RUNNING.store(false, Ordering::SeqCst);
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn caps(efforts: &[&str], adaptive: bool) -> Value {
        let mut effort = json!({"supported": !efforts.is_empty()});
        for e in EFFORTS {
            effort[e] = json!({"supported": efforts.contains(&e)});
        }
        json!({"effort": effort, "thinking": {"types": {"adaptive": {"supported": adaptive}}}})
    }

    #[test]
    fn picks_newest_per_family_with_its_capabilities() {
        let current: Vec<Model> = FAMILIES.iter().map(|f| builtin(f)).collect();
        let listing = vec![
            json!({"id": "claude-haiku-4-5-20251001", "display_name": "Claude Haiku 4.5", "created_at": "2025-10-01T00:00:00Z"}),
            json!({"id": "claude-haiku-4-5", "display_name": "Claude Haiku 4.5", "created_at": "2025-10-01T00:00:00Z"}),
            json!({"id": "claude-haiku-5-5", "display_name": "Claude Haiku 5.5", "created_at": "2027-01-01T00:00:00Z",
                   "max_input_tokens": 1_000_000, "capabilities": caps(&["low", "medium", "high"], true)}),
            json!({"id": "claude-opus-5", "display_name": "Claude Opus 5", "created_at": "2026-04-01T00:00:00Z"}),
            json!({"id": "claude-opus-5-5", "display_name": "Claude Opus 5.5", "created_at": "2026-09-01T00:00:00Z"}),
            json!({"id": "claude-mythos-5-1", "display_name": "Claude Mythos 5.1", "created_at": "2026-12-01T00:00:00Z"}),
        ];
        let got = pick(&listing, &current);
        assert_eq!(got[0].id, "claude-haiku-5-5");
        assert_eq!(got[0].name, "Haiku 5.5");
        assert_eq!(got[0].efforts, vec!["low", "medium", "high"]);
        assert!(got[0].adaptive);
        assert_eq!(got[0].context, 1_000_000);
        assert_eq!(got[1], current[1], "no sonnet listed: keep the current one");
        assert_eq!(got[2].id, "claude-opus-5-5");
        assert_eq!(
            got[2].efforts, current[2].efforts,
            "capabilities not reported: keep"
        );
        assert_eq!(got[3], current[3], "mythos is not fable");

        let only_old = vec![listing[0].clone(), listing[1].clone()];
        assert_eq!(
            pick(&only_old, &current)[0].id,
            "claude-haiku-4-5",
            "alias beats its snapshot"
        );
    }

    #[test]
    fn effort_falls_back_to_nearest_supported_level() {
        let mut m = builtin("opus");
        assert_eq!(effort_for(&m, "xhigh").as_deref(), Some("xhigh"));
        m.efforts = vec!["low".into(), "medium".into(), "high".into()];
        assert_eq!(effort_for(&m, "max").as_deref(), Some("high"));
        m.efforts = vec!["high".into()];
        assert_eq!(effort_for(&m, "low").as_deref(), Some("high"));
        assert_eq!(effort_for(&builtin("haiku"), "low"), None);
    }

    #[test]
    fn only_auth_headers_and_the_oauth_beta_are_forwarded() {
        let headers = vec![
            ("Authorization".to_string(), "Bearer t".to_string()),
            (
                "anthropic-beta".to_string(),
                "oauth-2025-04-20, interleaved-thinking-2025-05-14".to_string(),
            ),
            ("content-type".to_string(), "application/json".to_string()),
        ];
        let got = auth_headers(&headers);
        assert_eq!(
            got,
            vec![
                ("Authorization".to_string(), "Bearer t".to_string()),
                ("anthropic-beta".to_string(), "oauth-2025-04-20".to_string()),
                ("anthropic-version".to_string(), "2023-06-01".to_string()),
            ]
        );
    }
}
