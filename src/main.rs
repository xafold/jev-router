//! jev-router: route each Claude Code turn to a model + effort, track every decision.
//! Port of router/cli.py (the reference).

use jev_router::jev::{self, decide, log_path, one_line, read_log, render};
use jev_router::proxy::{start_proxy, Router, AUTO_MODEL};
use jev_router::util::{data_dir, home, ignore_sigint};
use serde_json::{json, Map, Value};
use std::io::{Read, Write};
use std::os::unix::fs::DirBuilderExt;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{exit, Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};
use std::{env, fs, thread};

const USAGE: &str =
    "jev-router: route each Claude Code turn to a model + effort, track every decision.

  jev-router claude [claude args]   start Claude Code behind the routing proxy (auto switch)
  jev-router route \"prompt\"         dry run: route one prompt and print the decision tree
  jev-router log [-n 20]            recent decisions, one line each
  jev-router show [ID]              full decision: Jev input, answers, trees
  jev-router statusline             status-line command: the rung this session is on
  jev-router dashboard [--port N]   local web dashboard of decisions (default port 8765)
  jev-router tune [--apply|--reset] auto-tuning from feedback: dry run, apply now, or reset
  jev-router golden [--record]      score v1 vs v2 rules on tests/golden.jsonl (--record asks Jev)
  jev-router --version              print the version

Decisions are appended as JSON lines to $CLAUDE_ROUTER_LOG
(default ~/.local/share/claude-router/decisions.jsonl).";

fn claude_settings() -> PathBuf {
    home().join(".claude/settings.json")
}

fn read_json(path: &PathBuf) -> Option<Value> {
    serde_json::from_str(&fs::read_to_string(path).ok()?).ok()
}

fn saved_model() -> Option<Value> {
    read_json(&claude_settings())?.get("model").cloned()
}

/// Picking "Jev Router" with Enter in /model saves it as the default model, which would
/// break plain `claude` sessions later. Put the old value back on exit.
fn restore_saved_model(before: Option<Value>) {
    let path = claude_settings();
    let Some(mut settings) = read_json(&path) else {
        return;
    };
    if settings["model"] != AUTO_MODEL {
        return;
    }
    let obj = settings.as_object_mut().unwrap();
    match before {
        Some(model) => {
            obj.insert("model".into(), model);
        }
        None => {
            obj.shift_remove("model");
        }
    }
    let _ = fs::write(
        &path,
        serde_json::to_string_pretty(&settings).unwrap_or_default() + "\n",
    );
}

/// Claude Code shows "jev-router" as the model, never the routed one. Run our status line
/// for this session only; it wraps the user's own status line (if any) and fills in the
/// rung. Global settings are not touched. Returns (claude args, inner status-line command).
fn statusline_args() -> (Vec<String>, Option<String>) {
    let mut inner = None;
    for path in [
        PathBuf::from(".claude/settings.local.json"),
        PathBuf::from(".claude/settings.json"),
        claude_settings(),
    ] {
        let Some(settings) = read_json(&path) else {
            continue;
        };
        let line = &settings["statusLine"];
        if line.is_null() || line == &json!({}) {
            continue;
        }
        if line["type"] == "command" {
            inner = line["command"]
                .as_str()
                .filter(|c| !c.is_empty())
                .map(str::to_string);
        }
        break;
    }
    let exe = env::current_exe()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| "jev-router".into());
    let settings =
        json!({"statusLine": {"type": "command", "command": format!("{exe} statusline")}});
    (vec!["--settings".into(), settings.to_string()], inner)
}

fn cmd_claude(claude_args: &[String]) -> ! {
    if env::var("TYPESAFE_API_KEY").map_or(true, |k| k.is_empty()) {
        println!("jev-router: no TYPESAFE_API_KEY; starting claude without routing");
        let error = Command::new("claude").args(claude_args).exec();
        eprintln!("jev-router: could not start claude: {error}");
        exit(127);
    }
    jev_router::autotune::maybe_run(); // silent: tunes only once there are enough ratings
    let upstream =
        env::var("ANTHROPIC_BASE_URL").unwrap_or_else(|_| "https://api.anthropic.com".into());
    let data = data_dir();
    let _ = fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&data);
    let router = Arc::new(Router::new(
        decide,
        data.join("status"),
        data.join("proxy.log"),
        data.join("usage.jsonl"),
    ));
    let port = start_proxy(router, upstream).unwrap_or_else(|e| {
        eprintln!("jev-router: could not start proxy: {e}");
        exit(1);
    });
    let (settings, inner) = statusline_args();
    let mut command = Command::new("claude");
    command
        .env("ANTHROPIC_BASE_URL", format!("http://127.0.0.1:{port}"))
        // "Jev Router" row in /model. Claude Code sends the id verbatim behind a base URL,
        // which is how the proxy tells "route this" from "the user picked a model".
        .env("ANTHROPIC_CUSTOM_MODEL_OPTION", AUTO_MODEL)
        .env("ANTHROPIC_CUSTOM_MODEL_OPTION_NAME", "Jev Router")
        .env(
            "ANTHROPIC_CUSTOM_MODEL_OPTION_DESCRIPTION",
            "Route each turn to a model + effort with Jev",
        )
        // Declared so Claude Code composes thinking + effort; the proxy strips them for Haiku.
        .env(
            "ANTHROPIC_CUSTOM_MODEL_OPTION_SUPPORTED_CAPABILITIES",
            "thinking,adaptive_thinking,interleaved_thinking,effort,max_effort",
        )
        // Compact at Haiku's 200K window so a switch down to Haiku can never overflow it.
        .env("CLAUDE_CODE_MAX_CONTEXT_TOKENS", "200000")
        .args(settings)
        .args(claude_args);
    if env::var_os("ANTHROPIC_MODEL").is_none() {
        command.env("ANTHROPIC_MODEL", AUTO_MODEL); // session-only; a model the user set wins
    }
    if let Some(inner) = inner {
        command.env("JEV_ROUTER_INNER_STATUSLINE", inner);
    }
    let before = saved_model();
    let mut child = command.spawn().unwrap_or_else(|e| {
        eprintln!("jev-router: could not start claude: {e}");
        exit(127);
    });
    ignore_sigint(); // after spawn, so Claude Code keeps the default SIGINT disposition
    let code = child.wait().ok().and_then(|s| s.code()).unwrap_or(1);
    restore_saved_model(before);
    exit(code);
}

/// Run `sh -c command` with `input` on stdin; stdout, or "" after `timeout`.
fn run_with_timeout(command: &str, input: String, timeout: Duration) -> String {
    let Ok(mut child) = Command::new("sh")
        .args(["-c", command])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
    else {
        return String::new();
    };
    let mut stdin = child.stdin.take().unwrap();
    thread::spawn(move || {
        let _ = stdin.write_all(input.as_bytes());
    });
    let mut stdout = child.stdout.take().unwrap();
    let reader = thread::spawn(move || {
        let mut out = String::new();
        let _ = stdout.read_to_string(&mut out);
        out
    });
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(10)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return String::new();
            }
        }
    }
    reader.join().unwrap_or_default()
}

/// Status-line command. Runs the user's own status line with the same stdin and swaps its
/// "jev-router" model label for the rung this session was routed to.
fn cmd_statusline() {
    let mut raw = String::new();
    let _ = std::io::stdin().read_to_string(&mut raw);
    let status = serde_json::from_str::<Value>(&raw)
        .ok()
        .and_then(|v| v["session_id"].as_str().map(str::to_string))
        .and_then(|s| read_json(&data_dir().join("status").join(format!("{s}.json"))))
        .unwrap_or_else(|| json!({}));
    // ponytail: shows the latest routed turn, which can briefly be a sub-agent's; key the
    // status by conversation if that turns out to confuse.
    let label = format!(
        "jev-router: {}",
        status["rung"].as_str().unwrap_or("waiting for first turn")
    );
    let Some(inner) = env::var("JEV_ROUTER_INNER_STATUSLINE")
        .ok()
        .filter(|c| !c.is_empty())
    else {
        println!("{label}");
        return;
    };
    let out = run_with_timeout(&inner, raw, Duration::from_secs(5));
    let manual = status["manual"].as_str().is_some_and(|m| !m.is_empty());
    let out = if out.contains("jev-router") {
        out.replacen("jev-router", &label, 1)
    } else if !manual {
        // A manual /model pick already shows its own name.
        format!("{label}\n{out}")
    } else {
        out
    };
    print!("{out}");
}

fn cmd_route(prompt: Option<&String>) {
    let prompt = prompt.cloned().unwrap_or_else(|| {
        let mut text = String::new();
        let _ = std::io::stdin().read_to_string(&mut text);
        text
    });
    match decide(&prompt, &[], Map::new(), "cli", None) {
        Ok(record) => println!("{}", render(&record)),
        Err(error) => {
            eprintln!("jev-router: {error}");
            exit(1);
        }
    }
}

fn cmd_log(args: &[String]) {
    let n = match args {
        [flag, n, ..] if flag == "-n" => n.parse().unwrap_or(20),
        _ => 20,
    };
    let records = read_log();
    for record in &records[records.len().saturating_sub(n)..] {
        println!("{}", one_line(record));
    }
}

fn cmd_show(id: Option<&String>) {
    let records: Vec<Value> = read_log()
        .into_iter()
        .filter(|r| {
            id.is_none_or(|id| {
                r["id"]
                    .as_str()
                    .is_some_and(|rid| rid.starts_with(id.as_str()))
            })
        })
        .collect();
    match records.last() {
        Some(record) => println!("{}", render(record)),
        None => {
            eprintln!("no matching decision in {}", log_path().display());
            exit(1);
        }
    }
}

fn main() {
    jev::load_env();
    let args: Vec<String> = env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        // Everything after `claude` belongs to Claude Code, unparsed.
        Some("claude") => cmd_claude(&args[1..]),
        Some("route") => cmd_route(args.get(1)),
        Some("log") => cmd_log(&args[1..]),
        Some("show") => cmd_show(args.get(1)),
        Some("statusline") => cmd_statusline(),
        Some("tune") => {
            let record = match args.get(1).map(String::as_str) {
                Some("--reset") => jev_router::autotune::reset(),
                Some("--apply") => jev_router::autotune::run(true),
                _ => jev_router::autotune::run(false), // dry run
            };
            println!(
                "{}",
                serde_json::to_string_pretty(&record).unwrap_or_default()
            );
        }
        Some("golden") => {
            let report = if args.get(1).is_some_and(|a| a == "--record") {
                jev_router::golden::record().map(|_| {
                    jev_router::golden::recorded().map(|s| jev_router::golden::evaluate(&s))
                })
            } else {
                Ok(jev_router::golden::recorded().map(|s| jev_router::golden::evaluate(&s)))
            };
            match report {
                Ok(Some(r)) => println!("{}", serde_json::to_string_pretty(&r).unwrap_or_default()),
                Ok(None) => {
                    eprintln!("jev-router: no answers recorded for the current questions; run `jev-router golden --record`");
                    exit(1);
                }
                Err(error) => {
                    eprintln!("jev-router: {error}");
                    exit(1);
                }
            }
        }
        Some("dashboard") => {
            let port = match &args[1..] {
                [flag, port, ..] if flag == "--port" => port.parse().unwrap_or(8765),
                _ => 8765,
            };
            if let Err(error) = jev_router::dashboard::serve(port) {
                eprintln!("jev-router: dashboard failed: {error}");
                exit(1);
            }
        }
        Some("-h" | "--help") => println!("{USAGE}"),
        Some("-V" | "--version") => println!("jev-router {}", env!("CARGO_PKG_VERSION")),
        _ => {
            eprintln!("{USAGE}");
            exit(2);
        }
    }
}
