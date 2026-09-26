//! v2 must meet the plan's acceptance bar on the golden set (docs/decision-v2-plan.md §6).
//! Replays recorded Jev answers: no network. Re-record after rewording a question.

use jev_router::golden::{evaluate, recorded};

#[test]
fn v2_meets_the_acceptance_bar() {
    let Some(saved) = recorded() else {
        panic!("tests/golden_answers.json is missing or stale: run `jev-router golden --record`");
    };
    let r = evaluate(&saved);
    let v2 = &r["rules"]["v2"];
    let report = serde_json::to_string_pretty(&r).unwrap();
    assert_eq!(
        r["cases"], r["of"],
        "every golden prompt needs recorded answers\n{report}"
    );
    assert!(v2["in_band_pct"].as_f64().unwrap() >= 85.0, "{report}");
    assert!(v2["under_pct"].as_f64().unwrap() <= 5.0, "{report}");
    assert_eq!(r["heavy_intents_on_haiku"], 0, "{report}");
}
