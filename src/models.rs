//! Which concrete model each ladder family (haiku, sonnet, opus) runs on.
//!
//! The ladder names the models known at build time. The proxy swaps each for the newest
//! model of its family from GET /v1/models, fetched in the background with the credentials
//! Claude Code already sends, on the first request of each session and then daily. The last
//! answer is kept in models.json so a new session starts on it. JEV_ROUTER_MODELS=builtin
//! pins the build-time models.
//!
//! ponytail: a newer model is assumed to accept what its family's build-time model accepts
//! (effort levels, adaptive thinking, system messages). Pin with JEV_ROUTER_MODELS=builtin
//! if one doesn't.

use crate::util::data_dir;
use serde_json::{json, Map, Value};
use std::fs;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{OnceLock, RwLock};
use std::time::{SystemTime, UNIX_EPOCH};

const FAMILIES: [&str; 3] = ["haiku", "sonnet", "opus"];
const REFRESH_AFTER_SECS: u64 = 24 * 3600;
/// After a failed lookup, try again in an hour rather than on every request.
const RETRY_AFTER_SECS: u64 = 3600;
/// Unix time of this process's last lookup; 0 until the session's first request.
static FETCHED_AT: AtomicU64 = AtomicU64::new(0);

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn pinned() -> bool {
    cfg!(test) || std::env::var("JEV_ROUTER_MODELS").is_ok_and(|v| v == "builtin")
}

fn family_of(id: &str) -> Option<usize> {
    FAMILIES
        .iter()
        .position(|f| id.starts_with(&format!("claude-{f}-")))
}

fn cache_path() -> std::path::PathBuf {
    data_dir().join("models.json")
}

/// Current model id per family (None: use the build-time model).
fn current() -> &'static RwLock<[Option<String>; 3]> {
    static CURRENT: OnceLock<RwLock<[Option<String>; 3]>> = OnceLock::new();
    CURRENT.get_or_init(|| {
        let mut ids: [Option<String>; 3] = Default::default();
        if !pinned() {
            let cache = fs::read_to_string(cache_path())
                .ok()
                .and_then(|t| serde_json::from_str::<Value>(&t).ok())
                .unwrap_or_default();
            for (i, family) in FAMILIES.iter().enumerate() {
                ids[i] = cache["families"][family]["id"].as_str().map(str::to_string);
            }
        }
        RwLock::new(ids)
    })
}

/// The model to send for a ladder model id: the newest of its family when known.
pub fn resolve(builtin: &str) -> String {
    family_of(builtin)
        .and_then(|i| current().read().unwrap_or_else(|e| e.into_inner())[i].clone())
        .unwrap_or_else(|| builtin.to_string())
}

/// Rung name as it runs now: "sonnet-5/low" on claude-sonnet-5-5 is "sonnet-5.5/low".
pub fn label(builtin: &str, effort: Option<&str>) -> String {
    let id = resolve(builtin);
    let parts: Vec<&str> = id
        .trim_start_matches("claude-")
        .split('-')
        .filter(|p| p.len() < 8) // drop a dated snapshot suffix
        .collect();
    let name = match parts.split_first() {
        Some((family, version)) if !version.is_empty() => {
            format!("{family}-{}", version.join("."))
        }
        _ => id.clone(),
    };
    effort.map_or(name.clone(), |e| format!("{name}/{e}"))
}

/// Newest model per family from a Models API listing: latest `created_at`, and on a tie
/// the shorter id (the alias rather than its dated snapshot).
fn pick(listing: &[Value]) -> [Option<(String, String)>; 3] {
    let mut best: [Option<&Value>; 3] = Default::default();
    let key = |m: &Value| {
        (
            m["created_at"].as_str().unwrap_or("").to_string(),
            std::cmp::Reverse(m["id"].as_str().unwrap_or("").len()),
        )
    };
    for m in listing {
        if let Some(i) = m["id"].as_str().and_then(family_of) {
            if best[i].is_none_or(|b| key(m) > key(b)) {
                best[i] = Some(m);
            }
        }
    }
    best.map(|m| {
        let m = m?;
        let id = m["id"].as_str()?.to_string();
        let name = m["display_name"].as_str().unwrap_or(&id).to_string();
        Some((id, name))
    })
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
            _ => break,
        }
    }
    Ok(out)
}

/// The headers that authenticate a Models API call: the API key or OAuth token, the API
/// version, and only the OAuth beta (the others are for /v1/messages).
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

/// Called with each /v1/messages request's headers. Looks the models up in the background
/// on the session's first request and once a day after; the request never waits for it.
/// `log` gets one line per lookup.
pub fn refresh_if_stale(
    agent: &ureq::Agent,
    upstream: &str,
    headers: &[(String, String)],
    log: impl Fn(&str) + Send + 'static,
) {
    static RUNNING: AtomicBool = AtomicBool::new(false);
    if pinned()
        || now().saturating_sub(FETCHED_AT.load(Ordering::Relaxed)) < REFRESH_AFTER_SECS
        || RUNNING.swap(true, Ordering::SeqCst)
    {
        return;
    }
    let (agent, upstream, auth) = (agent.clone(), upstream.to_string(), auth_headers(headers));
    std::thread::spawn(move || {
        let (found, error) = match fetch(&agent, &upstream, &auth) {
            Ok(listing) => (pick(&listing), None),
            Err(error) => (Default::default(), Some(error)),
        };
        if found.iter().all(Option::is_none) {
            let error = error.unwrap_or_else(|| "GET /v1/models listed no ladder models".into());
            log(&format!("models: lookup failed, keeping current: {error}"));
            FETCHED_AT.store(
                now() - REFRESH_AFTER_SECS + RETRY_AFTER_SECS,
                Ordering::Relaxed,
            );
        } else {
            let mut ids = current().write().unwrap_or_else(|e| e.into_inner());
            let mut families = Map::new();
            for (i, family) in FAMILIES.iter().enumerate() {
                if let Some((id, name)) = &found[i] {
                    ids[i] = Some(id.clone());
                    families.insert((*family).into(), json!({"id": id, "name": name}));
                }
            }
            let summary: Vec<String> = ids.iter().flatten().cloned().collect();
            log(&format!("models: {}", summary.join(", ")));
            let record = json!({"fetched_at": now(), "families": families});
            let _ = fs::write(cache_path(), format!("{record:#}\n"));
            FETCHED_AT.store(now(), Ordering::Relaxed);
        }
        RUNNING.store(false, Ordering::SeqCst);
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn picks_newest_per_family() {
        let listing = vec![
            json!({"id": "claude-haiku-4-5-20251001", "created_at": "2025-10-01T00:00:00Z"}),
            json!({"id": "claude-haiku-4-5", "created_at": "2025-10-01T00:00:00Z"}),
            json!({"id": "claude-sonnet-5", "created_at": "2026-04-01T00:00:00Z"}),
            json!({"id": "claude-sonnet-5-5", "display_name": "Claude Sonnet 5.5", "created_at": "2026-09-28T00:00:00Z"}),
            json!({"id": "claude-sonnet-4-6", "created_at": "2026-02-01T00:00:00Z"}),
            json!({"id": "claude-fable-5-1", "created_at": "2026-12-01T00:00:00Z"}),
        ];
        let got = pick(&listing);
        assert_eq!(
            got[0].as_ref().unwrap().0,
            "claude-haiku-4-5",
            "alias beats snapshot"
        );
        assert_eq!(
            got[1],
            Some(("claude-sonnet-5-5".into(), "Claude Sonnet 5.5".into()))
        );
        assert_eq!(got[2], None, "no opus listed: keep the current one");
    }

    #[test]
    fn labels_use_the_model_version() {
        // Pinned under test, so these are the build-time models.
        assert_eq!(label("claude-sonnet-5", Some("low")), "sonnet-5/low");
        assert_eq!(label("claude-opus-5-5", Some("max")), "opus-5.5/max");
        assert_eq!(label("claude-haiku-4-5-20251001", None), "haiku-4.5");
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
        assert_eq!(
            auth_headers(&headers),
            vec![
                ("Authorization".to_string(), "Bearer t".to_string()),
                ("anthropic-beta".to_string(), "oauth-2025-04-20".to_string()),
                ("anthropic-version".to_string(), "2023-06-01".to_string()),
            ]
        );
    }
}
