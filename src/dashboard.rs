//! `jev-router dashboard`: a local, read-mostly view of decisions.jsonl.
//!
//! Serves one static page (dashboard.html, compiled in) and a small JSON API. Bound to
//! 127.0.0.1 only and Host/Origin-checked, because the log holds private prompts.
//!   GET  /api/decisions   every decision record
//!   GET  /api/usage       one record per Claude request: tag + token usage (usage.jsonl)
//!   GET  /api/meta        ladder, questions, thresholds, forest (so trees can be drawn)
//!   GET  /api/proxy       proxy.log summary (routed / pinned / passthrough / errors)
//!   GET  /api/feedback    latest feedback label per decision id
//!   POST /api/feedback    {"id": "...", "label": "right"|"too_low"|"too_high"|"clear"}
//!   GET  /api/tuning      auto-tuning status and history;  POST /api/tuning/reset

use crate::jev::read_log;
use crate::router::meta;
use crate::util::{data_dir, utc_iso};
use serde_json::{json, Value};
use std::fs::{self, OpenOptions};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::os::unix::fs::OpenOptionsExt;

const PAGE: &str = include_str!("dashboard.html");
const MAX_BODY: usize = 64 * 1024;
const LABELS: [&str; 4] = ["right", "too_low", "too_high", "clear"];

/// List prices in $ per 1M tokens (claude-api skill, cached 2026-06-24; Opus 5.5 cache read
/// $0.20 is its published rate, the others follow 0.1x input). Cache writes: 1.25x input for
/// the 5-minute TTL, 2x for 1 hour. Keys match model ids by prefix (dated ids included).
fn pricing() -> Value {
    let p = |name: &str, input: f64, output: f64, read: f64| {
        json!({"name": name, "input": input, "output": output, "cache_read": read,
               "cache_write_5m": input * 1.25, "cache_write_1h": input * 2.0})
    };
    json!({
        "claude-opus-5-5": p("Opus 5.5", 4.0, 20.0, 0.20),
        "claude-sonnet-5": p("Sonnet 5", 2.0, 10.0, 0.20),
        "claude-haiku-4-5": p("Haiku 4.5", 1.0, 5.0, 0.10),
        "claude-opus-5": p("Opus 5", 5.0, 25.0, 0.50),
        "claude-fable-5-1": p("Fable 5.1", 10.0, 50.0, 0.25),
        "claude-opus-4-8": p("Opus 4.8", 5.0, 25.0, 0.50),
        "claude-sonnet-4-6": p("Sonnet 4.6", 3.0, 15.0, 0.30),
    })
}

fn usage() -> Value {
    let lines = fs::read_to_string(data_dir().join("usage.jsonl")).unwrap_or_default();
    Value::Array(
        lines
            .lines()
            .filter_map(|l| serde_json::from_str(l).ok())
            .collect(),
    )
}

fn feedback() -> Value {
    Value::Object(crate::autotune::feedback_labels())
}

fn add_feedback(body: &[u8]) -> Result<Value, String> {
    let entry: Value = serde_json::from_slice(body).map_err(|e| format!("bad JSON: {e}"))?;
    let id = entry["id"].as_str().filter(|id| {
        !id.is_empty() && id.len() <= 64 && id.chars().all(|c| c.is_ascii_alphanumeric())
    });
    let label = entry["label"].as_str().filter(|l| LABELS.contains(l));
    let (Some(id), Some(label)) = (id, label) else {
        return Err("need {\"id\": <decision id>, \"label\": right|too_low|too_high|clear}".into());
    };
    let record = json!({"id": id, "label": label, "ts": utc_iso()});
    let mut f = OpenOptions::new()
        .append(true)
        .create(true)
        .mode(0o600)
        .open(crate::autotune::feedback_path())
        .map_err(|e| e.to_string())?;
    writeln!(f, "{record}").map_err(|e| e.to_string())?;
    Ok(record)
}

/// Counts per kind of proxy.log line. The log has no dates, so this is all-time.
fn proxy_summary() -> Value {
    let (
        mut routed,
        mut pinned,
        mut passthrough,
        mut upstream_errors,
        mut transport,
        mut routing_failed,
    ) = (0, 0, 0, 0, 0, 0);
    for line in fs::read_to_string(data_dir().join("proxy.log"))
        .unwrap_or_default()
        .lines()
    {
        let status_error = line
            .rsplit_once('[')
            .and_then(|(_, s)| s.trim_end_matches(']').parse::<u16>().ok())
            .is_some_and(|s| s >= 400);
        if line.contains(" routed -> ") {
            routed += 1;
        } else if line.contains(" pinned -> ") {
            pinned += 1;
        } else if line.contains(" passthrough ") {
            passthrough += 1;
        } else if line.contains("upstream error:") {
            transport += 1;
        } else if line.contains("routing failed:") {
            routing_failed += 1;
        }
        if status_error {
            upstream_errors += 1;
        }
    }
    json!({"routed": routed, "pinned": pinned, "passthrough": passthrough, "upstream_4xx_5xx": upstream_errors,
           "transport_errors": transport, "routing_failed": routing_failed})
}

fn respond(
    stream: &mut TcpStream,
    status: &str,
    content_type: &str,
    body: &[u8],
) -> io::Result<()> {
    let head = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nCache-Control: no-store\r\n\
         X-Content-Type-Options: nosniff\r\nReferrer-Policy: no-referrer\r\n\
         Content-Security-Policy: default-src 'none'; script-src 'unsafe-inline'; style-src 'unsafe-inline'; connect-src 'self'; img-src data:\r\n\
         Connection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(head.as_bytes())?;
    stream.write_all(body)
}

fn json_response(stream: &mut TcpStream, status: &str, value: &Value) -> io::Result<()> {
    respond(
        stream,
        status,
        "application/json",
        value.to_string().as_bytes(),
    )
}

fn handle(mut stream: TcpStream, port: u16) -> io::Result<()> {
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut line = String::new();
    reader.read_line(&mut line)?;
    let mut parts = line.split_whitespace();
    let (method, path) = (parts.next().unwrap_or(""), parts.next().unwrap_or("/"));
    let (mut host, mut origin, mut length) = (String::new(), None, 0usize);
    loop {
        let mut header = String::new();
        if reader.read_line(&mut header)? == 0 || header.trim().is_empty() {
            break;
        }
        if let Some((k, v)) = header.split_once(':') {
            match k.trim().to_ascii_lowercase().as_str() {
                "host" => host = v.trim().to_string(),
                "origin" => origin = Some(v.trim().to_string()),
                "content-length" => length = v.trim().parse().unwrap_or(0),
                _ => {}
            }
        }
    }
    // DNS-rebinding and cross-site guard: only our own loopback origin may talk to us.
    let allowed = [format!("127.0.0.1:{port}"), format!("localhost:{port}")];
    let origin_ok = origin
        .as_deref()
        .is_none_or(|o| allowed.iter().any(|a| o == format!("http://{a}")));
    if !allowed.contains(&host) || !origin_ok {
        return respond(&mut stream, "403 Forbidden", "text/plain", b"forbidden");
    }
    if length > MAX_BODY {
        return respond(
            &mut stream,
            "413 Payload Too Large",
            "text/plain",
            b"too large",
        );
    }
    let mut body = vec![0; length];
    reader.read_exact(&mut body)?;
    let path = path.split('?').next().unwrap_or("/");
    match (method, path) {
        ("GET", "/") => respond(
            &mut stream,
            "200 OK",
            "text/html; charset=utf-8",
            PAGE.as_bytes(),
        ),
        ("GET", "/api/decisions") => {
            json_response(&mut stream, "200 OK", &Value::Array(read_log()))
        }
        ("GET", "/api/meta") => {
            let mut m = meta();
            m["pricing"] = pricing();
            m["jev_per_mtok"] = json!(0.042);
            json_response(&mut stream, "200 OK", &m)
        }
        ("GET", "/api/usage") => json_response(&mut stream, "200 OK", &usage()),
        ("GET", "/api/proxy") => json_response(&mut stream, "200 OK", &proxy_summary()),
        ("GET", "/api/feedback") => json_response(&mut stream, "200 OK", &feedback()),
        ("GET", "/api/tuning") => json_response(&mut stream, "200 OK", &crate::autotune::status()),
        ("POST", "/api/tuning/reset") => {
            json_response(&mut stream, "200 OK", &crate::autotune::reset())
        }
        ("POST", "/api/feedback") => match add_feedback(&body) {
            Ok(record) => {
                crate::autotune::maybe_run(); // silent: tunes only once there are enough ratings
                json_response(&mut stream, "200 OK", &record)
            }
            Err(error) => json_response(&mut stream, "400 Bad Request", &json!({"error": error})),
        },
        _ => respond(&mut stream, "404 Not Found", "text/plain", b"not found"),
    }
}

/// Serve until Ctrl-C. Port 8765 by default; a random free port if that one is taken.
pub fn serve(port: u16) -> io::Result<()> {
    let listener =
        TcpListener::bind(("127.0.0.1", port)).or_else(|_| TcpListener::bind("127.0.0.1:0"))?;
    let port = listener.local_addr()?.port();
    println!("jev-router dashboard: http://127.0.0.1:{port}  (local only; Ctrl-C to stop)");
    for stream in listener.incoming().flatten() {
        std::thread::spawn(move || {
            let _ = handle(stream, port);
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn feedback_is_validated() {
        assert!(add_feedback(br#"{"id": "abc", "label": "nope"}"#).is_err());
        assert!(add_feedback(br#"{"id": "../etc", "label": "right"}"#).is_err());
        assert!(add_feedback(b"not json").is_err());
    }
}
