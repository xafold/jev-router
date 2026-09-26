//! Golden set: labelled prompts (tests/golden.jsonl) with Jev answers recorded once
//! (tests/golden_answers.json), so rule changes replay offline in `cargo test`.
//! Only rewording a question needs a re-record (`jev-router golden --record`, ~$0.01).

use crate::jev::{all_questions, ask_jev, build_state};
use crate::router::{rank_of, requested_rung, route, route_v2, H, LADDER};
use serde_json::{json, Map, Value};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

fn repo() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
}

pub fn answers_path() -> PathBuf {
    repo().join("tests/golden_answers.json")
}

pub fn cases() -> Vec<Value> {
    fs::read_to_string(repo().join("tests/golden.jsonl"))
        .unwrap_or_default()
        .lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect()
}

fn history(case: &Value) -> Vec<Value> {
    case["history"].as_array().cloned().unwrap_or_default()
}

/// Ask Jev for every golden prompt, as if typed in this repository, and save the answers.
pub fn record() -> Result<Value, String> {
    let mut recorded = Vec::new();
    let all = cases();
    for (n, case) in all.iter().enumerate() {
        let prompt = case["prompt"].as_str().unwrap_or("");
        let state = build_state(prompt, &history(case), repo());
        let jev = ask_jev(&state)?;
        eprintln!(
            "{}/{} {} ms  {prompt:.60}",
            n + 1,
            all.len(),
            jev["latency_ms"]
        );
        recorded.push(
            json!({"prompt": prompt, "model": jev["model"], "latency_ms": jev["latency_ms"],
                             "answers": jev["answers"], "structured": jev["structured"]}),
        );
    }
    let out = json!({"questions": all_questions(), "cases": recorded});
    fs::write(
        answers_path(),
        serde_json::to_string_pretty(&out).unwrap() + "\n",
    )
    .map_err(|e| e.to_string())?;
    Ok(json!({"recorded": recorded.len(), "path": answers_path()}))
}

/// Recorded answers keyed by prompt; None if missing or recorded for other questions.
pub fn recorded() -> Option<Map<String, Value>> {
    let saved: Value = serde_json::from_str(&fs::read_to_string(answers_path()).ok()?).ok()?;
    if saved["questions"].as_object()? != &all_questions() {
        return None;
    }
    Some(
        saved["cases"]
            .as_array()?
            .iter()
            .filter_map(|c| Some((c["prompt"].as_str()?.to_string(), c.clone())))
            .collect(),
    )
}

/// Relative input price per model (dashboard.rs `pricing()`), for a rough cost index.
/// ponytail: ignores effort (more thinking tokens); compare runs with the same rule mix.
fn price(rank: usize) -> f64 {
    match LADDER[rank].model {
        "claude-haiku-4-5" => 1.0,
        "claude-sonnet-5" => 2.0,
        _ => 4.0,
    }
}

const HEAVY: [&str; 5] = [
    "explain_code",
    "review_audit",
    "plan_design",
    "debug_fix",
    "implement",
];

/// Replay v1 and v2 on the recorded answers and score them against the labelled bands.
pub fn evaluate(saved: &Map<String, Value>) -> Value {
    let mut stats: BTreeMap<&str, (usize, usize, usize, f64)> = BTreeMap::new();
    let (mut n, mut intent_ok, mut heavy_haiku) = (0, 0, 0);
    let mut per_intent: BTreeMap<String, (usize, usize)> = BTreeMap::new();
    let mut misses = Vec::new();
    let mut spread: BTreeMap<&str, usize> = BTreeMap::new();
    let all = cases();
    for case in &all {
        let prompt = case["prompt"].as_str().unwrap_or("");
        let Some(rec) = saved.get(prompt) else {
            continue;
        };
        let answers = rec["answers"].as_object().cloned().unwrap_or_default();
        let structured = rec["structured"].as_object().cloned().unwrap_or_default();
        let previous = case["previous"].as_str().and_then(rank_of);
        let (lo, hi) = (
            case["min"].as_str().and_then(rank_of).unwrap_or(0),
            case["max"]
                .as_str()
                .and_then(rank_of)
                .unwrap_or(LADDER.len() - 1),
        );
        let label = case["intent"].as_str().unwrap_or("other").to_string();
        n += 1;
        let v2 = route_v2(&answers, &structured, previous, 0, requested_rung(prompt));
        let v1 = route(&answers, previous, 0);
        let got_intent = v2["intent"]["choice"].as_str().unwrap_or("");
        intent_ok += usize::from(got_intent == label);
        for (rules, d) in [("v1", &v1), ("v2", &v2)] {
            let r = d["final"]["rung"].as_str().and_then(rank_of).unwrap_or(0);
            let s = stats.entry(rules).or_default();
            s.0 += usize::from((lo..=hi).contains(&r));
            s.1 += usize::from(r < lo);
            s.2 += usize::from(r > hi);
            s.3 += price(r);
            if rules == "v2" {
                *spread.entry(LADDER[r].name).or_default() += 1;
                let e = per_intent.entry(label.clone()).or_default();
                e.0 += 1;
                e.1 += usize::from((lo..=hi).contains(&r));
                if r == H && HEAVY.contains(&label.as_str()) {
                    heavy_haiku += 1;
                }
                if !(lo..=hi).contains(&r) {
                    misses.push(json!({"prompt": prompt, "want": [LADDER[lo].name, LADDER[hi].name],
                        "got": LADDER[r].name, "intent": got_intent, "label": label, "why": d["why"]}));
                }
            }
        }
    }
    let pct = |x: usize| {
        if n == 0 {
            0.0
        } else {
            100.0 * x as f64 / n as f64
        }
    };
    let rules: Map<String, Value> = stats
        .iter()
        .map(|(k, s)| {
            (
                k.to_string(),
                json!({"in_band_pct": pct(s.0), "under_pct": pct(s.1), "over_pct": pct(s.2),
                "cost_index": if n == 0 { 0.0 } else { s.3 / (n as f64 * 2.0) }}),
            )
        })
        .collect();
    json!({
        "cases": n, "of": all.len(),
        "rules": rules,
        "intent_accuracy_pct": pct(intent_ok),
        "heavy_intents_on_haiku": heavy_haiku,
        "v2_rungs": spread,
        "v2_in_band_by_intent": per_intent.iter().map(|(k, (t, ok))| (k.clone(), json!(format!("{ok}/{t}")))).collect::<Map<_, _>>(),
        "v2_misses": misses,
    })
}
