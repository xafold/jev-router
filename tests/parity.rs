//! The Rust port must reproduce the Python reference exactly. Fixtures come from
//! tests/gen_parity.py, which runs router/ (Python) over randomized and edge-case inputs.
//! Rung names were later renamed to family names ("opus-5.5/high" -> "opus/high"); the
//! Fable tier is off here, which is the ladder the reference implements.

use jev_router::autotune::{choose, Case};
use jev_router::jev::clip;
use jev_router::proxy::{apply_rung, fresh_prompt, history_of};
use jev_router::router::{rank_of, route, route_with, Tuning};
use serde_json::{json, Value};

#[test]
fn matches_python_reference() {
    // Fixtures were made with the built-in models, not whatever models.json has cached.
    std::env::set_var("JEV_ROUTER_MODELS", "builtin");
    let fixtures = include_str!("parity.jsonl");
    let mut counts = [0usize; 5];
    for (i, line) in fixtures.lines().enumerate() {
        let case: Value = serde_json::from_str(line).unwrap();
        match case["kind"].as_str().unwrap() {
            "route" => {
                let previous = case["previous_rung"].as_str().and_then(rank_of);
                let got = route(
                    case["answers"].as_object().unwrap(),
                    previous,
                    case["context_tokens"].as_u64().unwrap(),
                );
                assert_eq!(got, case["expected"], "route case {i}: {}", case["answers"]);
                counts[0] += 1;
            }
            "clip" => {
                assert_eq!(
                    json!(clip(case["text"].as_str().unwrap())),
                    case["expected"],
                    "clip case {i}"
                );
                counts[1] += 1;
            }
            "proxy" => {
                let body = &case["body"];
                assert_eq!(
                    json!(fresh_prompt(body)),
                    case["fresh_prompt"],
                    "fresh_prompt case {i}"
                );
                assert_eq!(json!(history_of(body)), case["history"], "history case {i}");
                let mut got = body.clone();
                apply_rung(&mut got, rank_of(case["rung"].as_str().unwrap()).unwrap());
                assert_eq!(
                    got, case["expected"],
                    "apply_rung case {i} ({})",
                    case["rung"]
                );
                counts[2] += 1;
            }
            "route_tuned" => {
                let previous = case["previous_rung"].as_str().and_then(rank_of);
                let tuning = Tuning::from_json(&case["tuning"]);
                assert_eq!(tuning.to_json(), case["tuning"], "tuning round trip {i}");
                let got = route_with(
                    case["answers"].as_object().unwrap(),
                    previous,
                    case["context_tokens"].as_u64().unwrap(),
                    &tuning,
                );
                assert_eq!(
                    got, case["expected"],
                    "route_tuned case {i}: {}",
                    case["tuning"]
                );
                counts[3] += 1;
            }
            "choose" => {
                let cases: Vec<Case> = case["cases"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|c| Case {
                        answers: c["answers"].as_object().unwrap().clone(),
                        previous: c["previous"].as_u64().map(|p| p as usize),
                        context: c["context"].as_u64().unwrap(),
                        rated: c["rated"].as_u64().unwrap() as usize,
                        label: c["label"].as_str().unwrap().to_string(),
                        fable: false,
                    })
                    .collect();
                let got = choose(&cases, &Tuning::default());
                let got = json!({"to": got.to.to_json(), "before": got.before, "after": got.after, "changed": got.changed});
                assert_eq!(got, case["expected"], "choose case {i}");
                counts[4] += 1;
            }
            other => panic!("unknown case kind {other}"),
        }
    }
    assert!(
        counts.iter().all(|c| *c > 0),
        "every kind covered: {counts:?}"
    );
}
