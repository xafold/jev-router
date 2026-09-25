//! Silent auto-tuning from dashboard feedback.
//!
//! Once there are MIN_RATINGS rated decisions (and again after every RETUNE_EVERY new
//! ones), replay every rated decision from its logged Jev answers (free: no Jev calls)
//! under a small grid of soft-knob settings (router::Tuning). A setting is applied only if
//! it satisfies at least MIN_GAIN more ratings than the current one AND changes none of the
//! decisions the user rated "right". Safety floors are not tunable. Runs silently from
//! `jev-router claude` (at launch) and the dashboard (after each rating). Every run is
//! recorded in tuning_history.jsonl; `jev-router tune --reset` or the dashboard undoes it.

use crate::jev::read_log;
use crate::router::{rank_of, route_with, Tuning, QUESTIONS};
use crate::util::{data_dir, utc_iso};
use serde_json::{json, Map, Value};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::PathBuf;

pub const MIN_RATINGS: usize = 20;
pub const RETUNE_EVERY: usize = 5;
pub const MIN_GAIN: usize = 2;

fn tuning_path() -> PathBuf {
    data_dir().join("tuning.json")
}
fn history_path() -> PathBuf {
    data_dir().join("tuning_history.jsonl")
}
pub fn feedback_path() -> PathBuf {
    data_dir().join("feedback.jsonl")
}

/// Latest label per decision id ("clear" removes it).
pub fn feedback_labels() -> Map<String, Value> {
    let mut latest = Map::new();
    for line in fs::read_to_string(feedback_path())
        .unwrap_or_default()
        .lines()
    {
        let Ok(entry) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if let (Some(id), Some(label)) = (entry["id"].as_str(), entry["label"].as_str()) {
            if label == "clear" {
                latest.remove(id);
            } else {
                latest.insert(id.to_string(), json!(label));
            }
        }
    }
    latest
}

fn state() -> Value {
    fs::read_to_string(tuning_path())
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_else(|| json!({}))
}

/// The tuning live routing uses (defaults when nothing was applied).
pub fn current() -> Tuning {
    Tuning::from_json(&state()["tuning"])
}

/// One rated decision, replayable from its logged Jev answers.
pub struct Case {
    pub answers: Map<String, Value>,
    pub previous: Option<usize>,
    pub context: u64,
    pub rated: usize,
    pub label: String,
}

/// The outcome of a search: the settings to use and how many ratings they satisfy.
pub struct Choice {
    pub to: Tuning,
    pub before: usize,
    pub after: usize,
    pub changed: bool,
}

/// Best candidate that satisfies at least MIN_GAIN more ratings without breaking more
/// "right" ones than `now` does. Highest score wins, then fewest changed knobs, then the
/// smallest move from `now`; remaining ties keep the first candidate in grid order.
pub fn choose(cases: &[Case], now: &Tuning) -> Choice {
    let (before, base_broken) = score(cases, now);
    let mut best: Option<(Tuning, usize)> = None;
    for t in candidates() {
        let (sat, broken) = score(cases, &t);
        if broken > base_broken || sat < before + MIN_GAIN {
            continue;
        }
        let better = best.as_ref().is_none_or(|(b, bsat)| {
            let key = |x: &Tuning, s: usize| (s, std::cmp::Reverse(distance(x)));
            match key(&t, sat).cmp(&key(b, *bsat)) {
                std::cmp::Ordering::Equal => movement(&t, now) < movement(b, now),
                order => order == std::cmp::Ordering::Greater,
            }
        });
        if better {
            best = Some((t, sat));
        }
    }
    match best {
        Some((to, after)) => Choice {
            changed: to != *now,
            to,
            before,
            after,
        },
        None => Choice {
            to: now.clone(),
            before,
            after: before,
            changed: false,
        },
    }
}

/// Rated decisions that can be replayed: they succeeded and were asked the current
/// questions (a reworded question needs fresh Jev answers, so older ones are skipped).
fn cases() -> Vec<Case> {
    let labels = feedback_labels();
    let questions: Map<String, Value> = QUESTIONS
        .iter()
        .map(|(k, q)| ((*k).to_string(), json!(q)))
        .collect();
    read_log()
        .into_iter()
        .filter_map(|r| {
            let label = labels.get(r["id"].as_str()?)?.as_str()?.to_string();
            let rated = rank_of(r["final"]["rung"].as_str()?)?;
            if r["jev"]["questions"].as_object()? != &questions {
                return None;
            }
            Some(Case {
                answers: r["jev"]["answers"].as_object()?.clone(),
                previous: r["facts"]["previous_rung"].as_str().and_then(rank_of),
                context: r["facts"]["context_tokens"].as_u64().unwrap_or(0),
                rated,
                label,
            })
        })
        .collect()
}

/// (ratings satisfied, "right" ratings whose decision would change).
pub fn score(cases: &[Case], t: &Tuning) -> (usize, usize) {
    let (mut satisfied, mut broken) = (0, 0);
    for c in cases {
        let new = route_with(&c.answers, c.previous, c.context, t)["final"]["rung"]
            .as_str()
            .and_then(rank_of)
            .unwrap_or(c.rated);
        let ok = match c.label.as_str() {
            "right" => new == c.rated,
            "too_low" => new > c.rated,
            "too_high" => new < c.rated,
            _ => true,
        };
        satisfied += usize::from(ok);
        broken += usize::from(c.label == "right" && new != c.rated);
    }
    (satisfied, broken)
}

pub fn candidates() -> Vec<Tuning> {
    let mut out = Vec::new();
    for borderline in [0.10, 0.125, 0.15, 0.175, 0.20] {
        for disagree_spread in [3, 4, 5, 8] {
            for clarify_cap_below in [None, Some(0.20), Some(0.25), Some(0.30), Some(0.35)] {
                for offset in [-1, 0, 1] {
                    out.push(Tuning {
                        borderline,
                        disagree_spread,
                        clarify_cap_below,
                        offset,
                    });
                }
            }
        }
    }
    out
}

/// How far a setting is from the defaults (fewer changed knobs win ties).
fn distance(t: &Tuning) -> usize {
    let d = Tuning::default();
    usize::from(t.borderline != d.borderline)
        + usize::from(t.disagree_spread != d.disagree_spread)
        + usize::from(t.clarify_cap_below != d.clarify_cap_below)
        + usize::from(t.offset != d.offset) * 2
}

/// How far a setting moves from the current one (smaller moves win the remaining ties).
fn movement(t: &Tuning, now: &Tuning) -> f64 {
    (t.borderline - now.borderline).abs()
        + (t.disagree_spread as f64 - now.disagree_spread as f64).abs() * 0.01
        + (t.clarify_cap_below.unwrap_or(0.0) - now.clarify_cap_below.unwrap_or(0.0)).abs()
        + f64::from((t.offset - now.offset).abs())
}

/// Evaluate, and if `apply`, write the result. Returns the run record.
pub fn run(apply: bool) -> Value {
    let cases = cases();
    let now = current();
    let Choice {
        to,
        before: base,
        after,
        changed,
    } = choose(&cases, &now);
    let record = json!({
        "ts": utc_iso(),
        "rated": cases.len(),
        "from": now.to_json(),
        "to": to.to_json(),
        "satisfied_before": base,
        "satisfied_after": after,
        "changed": changed,
        "reason": if changed {
            format!("fits {after} of {} ratings instead of {base}, without changing any rated right", cases.len())
        } else {
            format!("kept settings: nothing fits at least {MIN_GAIN} more of {} ratings without breaking a right one", cases.len())
        },
    });
    if apply {
        save(&to, cases.len(), &record);
    }
    record
}

fn save(t: &Tuning, rated: usize, record: &Value) {
    let _ = fs::create_dir_all(data_dir());
    let state = json!({"tuning": t.to_json(), "updated": record["ts"], "rated_at_last_run": rated, "reason": record["reason"]});
    let _ = fs::write(
        tuning_path(),
        serde_json::to_string_pretty(&state).unwrap_or_default() + "\n",
    );
    if let Ok(mut f) = OpenOptions::new()
        .append(true)
        .create(true)
        .mode(0o600)
        .open(history_path())
    {
        let _ = writeln!(f, "{record}");
    }
}

/// Run when there are enough new ratings; silent and never fails the caller.
pub fn maybe_run() {
    let rated = cases().len();
    let last = state()["rated_at_last_run"].as_u64().map(|x| x as usize);
    let due = match last {
        None => rated >= MIN_RATINGS,
        Some(last) => rated >= MIN_RATINGS && rated >= last + RETUNE_EVERY,
    };
    if due {
        let _ = std::panic::catch_unwind(|| run(true));
    }
}

/// Back to the defaults (the dashboard's Reset button, `jev-router tune --reset`).
pub fn reset() -> Value {
    let rated = cases().len();
    let record = json!({"ts": utc_iso(), "rated": rated, "from": current().to_json(), "to": Tuning::default().to_json(),
                        "changed": true, "reason": "reset to defaults by you"});
    save(&Tuning::default(), rated, &record);
    record
}

/// Everything the dashboard shows about tuning.
pub fn status() -> Value {
    let history: Vec<Value> = fs::read_to_string(history_path())
        .unwrap_or_default()
        .lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect();
    json!({"current": current().to_json(), "default": Tuning::default().to_json(), "rated": cases().len(),
           "min_ratings": MIN_RATINGS, "retune_every": RETUNE_EVERY, "min_gain": MIN_GAIN,
           "rated_at_last_run": state()["rated_at_last_run"], "history": history})
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::router::{fake, H, O_MED, S_MED};

    fn case(nouls: &[(&str, f64)], rated: usize, label: &str) -> Case {
        Case {
            answers: fake(nouls),
            previous: None,
            context: 0,
            rated,
            label: label.into(),
        }
    }

    #[test]
    fn prefers_a_setting_that_fixes_ratings_without_breaking_right_ones() {
        // Clear-ish first messages (clarified 0.30) get capped to Haiku and were rated
        // "too low"; a genuinely unclear one (0.05) was rated "right" at Haiku.
        let mut cases: Vec<Case> = (0..3)
            .map(|_| case(&[("clarified", 0.30), ("tests", 0.9)], H, "too_low"))
            .collect();
        cases.push(case(&[("clarified", 0.05)], H, "right"));
        cases.push(case(
            &[("clarified", 0.95), ("concurrency", 0.99), ("tests", 0.9)],
            O_MED,
            "right",
        ));
        // An ordinary clear request with tests, rightly on Sonnet medium: a blanket +1 breaks it.
        cases.push(case(&[("clarified", 0.95), ("tests", 0.9)], S_MED, "right"));
        let choice = choose(&cases, &Tuning::default());
        assert_eq!((choice.before, choice.after, choice.changed), (3, 6, true));
        // The cap moved below 0.30 (so 0.30 is no longer "unclear") but still catches 0.05.
        assert!(choice.to.clarify_cap_below.is_some_and(|c| c <= 0.30));
        assert_eq!(choice.to.offset, 0);
        // A blanket +1 would also fix the "too low" ones but breaks a "right" one.
        let up = Tuning {
            offset: 1,
            ..Tuning::default()
        };
        assert!(score(&cases, &up).1 > 0);
        // Too few differences to act on: nothing changes.
        let small = choose(&cases[3..], &Tuning::default());
        assert!(!small.changed);
    }

    #[test]
    fn tuning_round_trips_and_defaults_match_reference() {
        let t = Tuning {
            borderline: 0.1,
            disagree_spread: 5,
            clarify_cap_below: None,
            offset: -1,
        };
        assert_eq!(Tuning::from_json(&t.to_json()), t);
        assert_eq!(Tuning::from_json(&json!({})), Tuning::default());
        assert_eq!(Tuning::from_json(&json!({"offset": 9})).offset, 1);
    }
}
