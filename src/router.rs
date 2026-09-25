//! Claude model + effort router: Jev Nouls -> five decision trees -> median vote -> guard rails.
//! 1:1 port of router/jev_router.py (the reference). Pure logic, no I/O.

use serde_json::{json, Map, Value};

pub struct Rung {
    /// Family + effort, e.g. "opus/high". Versions are not part of the name: the family's
    /// current model comes from models.rs, so a new Opus is picked up without a release.
    pub name: &'static str,
    pub family: &'static str,
    /// None for the Haiku rung: no effort param, the cheapest option.
    pub effort: Option<&'static str>,
}

const fn rung(name: &'static str, family: &'static str, effort: Option<&'static str>) -> Rung {
    Rung {
        name,
        family,
        effort,
    }
}

/// Cheapest -> strongest. The two Fable rungs exist only with `--fable on`; without it
/// the ladder ends at Opus max, exactly as before.
pub const LADDER: [Rung; 10] = [
    rung("haiku", "haiku", None),
    rung("sonnet/low", "sonnet", Some("low")),
    rung("sonnet/medium", "sonnet", Some("medium")),
    rung("sonnet/high", "sonnet", Some("high")),
    rung("opus/medium", "opus", Some("medium")),
    rung("opus/high", "opus", Some("high")),
    rung("opus/xhigh", "opus", Some("xhigh")),
    rung("opus/max", "opus", Some("max")),
    rung("fable/high", "fable", Some("high")),
    rung("fable/max", "fable", Some("max")),
];
pub const H: usize = 0;
pub const S_LOW: usize = 1;
pub const S_MED: usize = 2;
pub const S_HIGH: usize = 3;
pub const O_MED: usize = 4;
pub const O_HIGH: usize = 5;
pub const O_XHIGH: usize = 6;
pub const O_MAX: usize = 7;
pub const F_HIGH: usize = 8;
pub const F_MAX: usize = 9;
/// Tree leaf for the hardest cases: Opus xhigh, or Fable high when Fable is on.
pub const TOP: usize = usize::MAX;

/// Strongest rung allowed.
pub fn top(fable: bool) -> usize {
    if fable {
        F_MAX
    } else {
        O_MAX
    }
}

fn leaf(rank: usize, fable: bool) -> usize {
    match rank {
        TOP if fable => F_HIGH,
        TOP => O_XHIGH,
        r => r,
    }
}

/// Also accepts the versioned names older logs use ("haiku-4.5", "opus-5.5/high").
pub fn rank_of(name: &str) -> Option<usize> {
    let (head, effort) = name
        .split_once('/')
        .map_or((name, None), |(h, e)| (h, Some(e)));
    let family = head.split('-').next().unwrap_or(head);
    LADDER
        .iter()
        .position(|r| r.family == family && r.effort == effort)
}

/// The concrete model and effort a rung sends right now.
pub fn resolve(rank: usize) -> (crate::models::Model, Option<String>) {
    let rung = &LADDER[rank];
    let model = crate::models::model(rung.family);
    let effort = rung
        .effort
        .and_then(|e| crate::models::effort_for(&model, e));
    (model, effort)
}

/// "Opus 5.5 · high", for the status line and the dashboard.
pub fn label(rank: usize) -> String {
    let (model, effort) = resolve(rank);
    effort.map_or(model.name.clone(), |e| format!("{} · {e}", model.name))
}

pub const NOUL_YES: f64 = 0.5;
/// |noul - 0.5| below this is "unsure": the tree takes the harder branch.
pub const BORDERLINE: f64 = 0.15;
/// Guard rails fire only on a clear yes / clear no.
pub const CLEAR_YES: f64 = NOUL_YES + BORDERLINE;
pub const CLEAR_NO: f64 = NOUL_YES - BORDERLINE;
/// Votes this many rungs apart -> move up one rung.
pub const DISAGREE_SPREAD: usize = 4;
/// Switching model or effort invalidates the prompt cache, so past this size a downgrade
/// costs more than it saves.
pub const DOWNGRADE_MAX_CONTEXT_TOKENS: u64 = 20_000;

/// State is {"history": [earlier turns], "latest": "<new user message>"}. Every question is
/// about `latest`; `history` is only context, so an old failure or topic can't leak forward.
pub const QUESTIONS: [(&str, &str); 9] = [
    ("clarified", "Given the conversation in `history`, does `latest` give the assistant enough detail to start working on the request?"),
    ("trivial", "Is the request in `latest` a small, self-contained change, such as a one-line fix, a rename, a syntax question, or explaining one concept?"),
    ("multi_component", "Does the request in `latest` need changes across more than one component, such as several files, services, or backend and frontend?"),
    ("perf", "Does `latest` state an explicit performance or scale requirement for the request, such as data volume, throughput, or latency?"),
    ("concurrency", "Does the request in `latest` involve concurrency, such as async tasks, threads, locks, deadlocks, or race conditions?"),
    ("prior_failed", "Does `latest` say that the assistant's previous solution in `history` did not work or made things worse?"),
    ("security", "Does the request in `latest` involve authentication, secrets, access control, or sensitive user data?"),
    ("tests", "Does `latest` explicitly ask for tests?"),
    ("open_ended", "Is the request in `latest` open-ended system design, such as architecting a new system end to end?"),
];

/// Noul answers keyed by question id.
pub type Answers = Map<String, Value>;

fn noul(answers: &Answers, key: &str) -> f64 {
    answers
        .get(key)
        .and_then(Value::as_f64)
        .unwrap_or_else(|| panic!("missing Noul answer: {key}"))
}

pub enum Node {
    Leaf(usize),
    Split(&'static str, Box<Node>, Box<Node>),
}

fn split(key: &'static str, yes: Node, no: Node) -> Node {
    Node::Split(key, Box::new(yes), Box::new(no))
}
use Node::Leaf;

/// Each node is (noul_key, if_yes, if_no); leaves are rungs. The two TOP leaves (a failed
/// concurrency fix, open-ended multi-part design) are where Fable takes over when it is on.
pub fn forest() -> Vec<(&'static str, Node)> {
    vec![
        (
            "scope",
            split(
                "clarified",
                split(
                    "trivial",
                    Leaf(H),
                    split("multi_component", Leaf(O_MED), Leaf(S_MED)),
                ),
                Leaf(H),
            ),
        ),
        (
            "history",
            split(
                "prior_failed",
                split("concurrency", Leaf(TOP), Leaf(O_HIGH)),
                split("perf", Leaf(S_HIGH), Leaf(S_MED)),
            ),
        ),
        (
            "risk",
            split(
                "security",
                Leaf(O_HIGH),
                split("tests", Leaf(S_MED), split("trivial", Leaf(H), Leaf(S_LOW))),
            ),
        ),
        (
            "design",
            split(
                "open_ended",
                split("multi_component", Leaf(TOP), Leaf(O_HIGH)),
                split("concurrency", Leaf(O_MED), Leaf(S_MED)),
            ),
        ),
        (
            "load",
            split(
                "perf",
                split("concurrency", Leaf(O_HIGH), Leaf(S_HIGH)),
                split("trivial", Leaf(H), Leaf(S_LOW)),
            ),
        ),
    ]
}

/// The soft knobs auto-tuning may adjust (autotune.rs). The safety floors (security,
/// concurrency, retry) and the cache guard are deliberately not in here: they never move.
/// `Tuning::default()` is exactly the Python reference's behaviour.
#[derive(Clone, Debug, PartialEq)]
pub struct Tuning {
    /// |noul - 0.5| below this is "unsure" inside the trees (both branches, harder wins).
    pub borderline: f64,
    /// Votes this many rungs apart move the result one rung up.
    pub disagree_spread: usize,
    /// `clarified` below this caps the result at Haiku (so it asks first); None turns it off.
    pub clarify_cap_below: Option<f64>,
    /// Shift the voted rung before the safety rules: -1 cheaper, +1 stronger.
    pub offset: i32,
}

impl Default for Tuning {
    fn default() -> Self {
        Tuning {
            borderline: BORDERLINE,
            disagree_spread: DISAGREE_SPREAD,
            clarify_cap_below: Some(CLEAR_NO),
            offset: 0,
        }
    }
}

impl Tuning {
    pub fn to_json(&self) -> Value {
        json!({"borderline": self.borderline, "disagree_spread": self.disagree_spread,
               "clarify_cap_below": self.clarify_cap_below, "offset": self.offset})
    }

    /// Missing or out-of-range fields fall back to the default.
    pub fn from_json(v: &Value) -> Tuning {
        let d = Tuning::default();
        Tuning {
            borderline: v["borderline"]
                .as_f64()
                .filter(|b| (0.0..0.5).contains(b))
                .unwrap_or(d.borderline),
            disagree_spread: v["disagree_spread"]
                .as_u64()
                .map_or(d.disagree_spread, |x| x as usize),
            clarify_cap_below: match &v["clarify_cap_below"] {
                Value::Null if v.get("clarify_cap_below").is_some() => None,
                x => x.as_f64().or(d.clarify_cap_below),
            },
            offset: v["offset"].as_i64().map_or(0, |x| x.clamp(-1, 1) as i32),
        }
    }
}

/// The ladder, questions, thresholds and forest as JSON, for the dashboard.
pub fn meta() -> Value {
    let fable = crate::util::fable_enabled();
    fn node(n: &Node, fable: bool) -> Value {
        match n {
            Node::Leaf(rank) => json!({"leaf": LADDER[leaf(*rank, fable)].name}),
            Node::Split(key, yes, no) => {
                json!({"key": key, "yes": node(yes, fable), "no": node(no, fable)})
            }
        }
    }
    let forest: Map<String, Value> = forest()
        .iter()
        .map(|(name, n)| ((*name).to_string(), node(n, fable)))
        .collect();
    json!({
        "ladder": (0..LADDER.len()).map(|i| { let (m, e) = resolve(i); json!({"name": LADDER[i].name, "family": LADDER[i].family, "model": m.id, "effort": e, "label": label(i)}) }).collect::<Vec<_>>(),
        "fable": fable,
        "models": crate::models::status(),
        "questions": QUESTIONS.iter().map(|(k, q)| json!({"key": k, "instructions": q})).collect::<Vec<_>>(),
        "forest": forest,
        "thresholds": {"noul_yes": NOUL_YES, "borderline": BORDERLINE, "clear_yes": CLEAR_YES, "clear_no": CLEAR_NO,
                       "disagree_spread": DISAGREE_SPREAD, "downgrade_max_context_tokens": DOWNGRADE_MAX_CONTEXT_TOKENS},
    })
}

#[derive(Clone)]
pub struct Step {
    pub key: &'static str,
    pub noul: f64,
    pub branch: &'static str,
}

/// Follow one tree to a leaf. An unsure Noul follows both branches and keeps the higher
/// rung ("when unsure, assume the harder case"). Ties keep the yes branch, as in Python.
pub fn walk(node: &Node, answers: &Answers, path: Vec<Step>) -> (usize, Vec<Step>) {
    walk_with(node, answers, path, BORDERLINE, false)
}

fn walk_with(
    node: &Node,
    answers: &Answers,
    path: Vec<Step>,
    borderline: f64,
    fable: bool,
) -> (usize, Vec<Step>) {
    match node {
        Node::Leaf(rank) => (leaf(*rank, fable), path),
        Node::Split(key, if_yes, if_no) => {
            let p = noul(answers, key);
            let step = |branch| {
                let mut next = path.clone();
                next.push(Step {
                    key,
                    noul: p,
                    branch,
                });
                next
            };
            if (p - NOUL_YES).abs() < borderline {
                let yes = walk_with(if_yes, answers, step("unsure->yes"), borderline, fable);
                let no = walk_with(if_no, answers, step("unsure->no"), borderline, fable);
                return if no.0 > yes.0 { no } else { yes };
            }
            if p > NOUL_YES {
                walk_with(if_yes, answers, step("yes"), borderline, fable)
            } else {
                walk_with(if_no, answers, step("no"), borderline, fable)
            }
        }
    }
}

/// Answers + code-known facts -> decision JSON (same shape the Python reference logs).
/// `previous_rung` answered the last turn; `context_tokens` is the conversation size so far.
pub fn route(answers: &Answers, previous_rung: Option<usize>, context_tokens: u64) -> Value {
    route_with(answers, previous_rung, context_tokens, &Tuning::default())
}

/// `route` with auto-tuned soft knobs. The safety floors always apply unchanged.
pub fn route_with(
    answers: &Answers,
    previous_rung: Option<usize>,
    context_tokens: u64,
    tuning: &Tuning,
) -> Value {
    route_tiered(answers, previous_rung, context_tokens, tuning, false)
}

/// `route_with`, optionally with the Fable tier. With `fable` false this is exactly the
/// ladder without Fable. With it on:
///   - the TOP tree leaves vote Fable high instead of Opus xhigh;
///   - a tree that reaches its TOP leaf on clear answers only (no "unsure" branch) floors
///     the result at Fable high: the hardest work goes to Fable even though one tree's
///     vote can't move the median;
///   - a failed answer from any Opus rung goes straight to Fable high;
///   - the strongest rung is Fable max instead of Opus max.
///
/// Tree spread is measured as if Fable were off, so turning it on doesn't make the
/// "trees disagree, +1" rule fire more often.
pub fn route_tiered(
    answers: &Answers,
    previous_rung: Option<usize>,
    context_tokens: u64,
    tuning: &Tuning,
    fable: bool,
) -> Value {
    let last = top(fable);
    // A rung from before Fable was turned off still counts as the strongest one left.
    let previous_rung = previous_rung.map(|p| p.min(last));
    let mut trees = Map::new();
    let mut ranks = Vec::new();
    let mut base = Vec::new();
    let mut hardest = Vec::new();
    for (name, tree) in forest() {
        let (rung, path) = walk_with(&tree, answers, Vec::new(), tuning.borderline, fable);
        base.push(if rung >= F_HIGH { O_XHIGH } else { rung });
        if rung == F_HIGH && path.iter().all(|s| !s.branch.starts_with("unsure")) {
            hardest.push(name);
        }
        ranks.push(rung);
        let path: Vec<Value> = path
            .iter()
            .map(|s| json!({"key": s.key, "noul": s.noul, "branch": s.branch}))
            .collect();
        trees.insert(
            name.into(),
            json!({"vote": LADDER[rung].name, "path": path}),
        );
    }
    ranks.sort_unstable();
    base.sort_unstable();
    let median = ranks[ranks.len() / 2];
    let spread = base[base.len() - 1] - base[0];
    let mut rank = median;
    let mut why = vec![format!("median of votes: {}", LADDER[median].name)];

    if spread >= tuning.disagree_spread {
        rank = (rank + 1).min(last);
        why.push(format!(
            "trees disagree (spread {spread} rungs): +1 -> {}",
            LADDER[rank].name
        ));
    }

    if tuning.offset != 0 {
        rank = (rank as i32 + tuning.offset).clamp(0, last as i32) as usize;
        why.push(format!(
            "auto-tuning: {:+} -> {}",
            tuning.offset, LADDER[rank].name
        ));
    }

    // Before the cap: an unclear request still gets its clarifying questions first.
    if !hardest.is_empty() && rank < F_HIGH {
        rank = F_HIGH;
        why.push(format!(
            "hardest case ({} tree): floor -> {}",
            hardest.join(", "),
            LADDER[F_HIGH].name
        ));
    }

    // Guard rails. The cap runs first so the safety floors always win over it.
    // A failed fix is never handed to the cheapest model, even if it reads as unclear.
    let cap = tuning
        .clarify_cap_below
        .is_some_and(|below| noul(answers, "clarified") < below);
    if cap && noul(answers, "prior_failed") <= CLEAR_YES {
        rank = 0;
        why.push(format!(
            "needs clarification: cap -> {} asks the questions",
            LADDER[0].name
        ));
    }
    if noul(answers, "security") > CLEAR_YES && rank < O_MED {
        rank = O_MED;
        why.push(format!(
            "security-sensitive: floor -> {}",
            LADDER[O_MED].name
        ));
    }
    if noul(answers, "concurrency") > CLEAR_YES && rank < O_MED {
        rank = O_MED;
        why.push(format!("concurrency: floor -> {}", LADDER[O_MED].name));
    }
    if let Some(previous) = previous_rung {
        if noul(answers, "prior_failed") > CLEAR_YES {
            let floor = if fable && LADDER[previous].family == "opus" {
                F_HIGH
            } else {
                (previous + 1).min(last)
            };
            if rank < floor {
                rank = floor;
                why.push(format!(
                    "last answer ({}) failed: floor -> {}",
                    LADDER[previous].name, LADDER[floor].name
                ));
            }
        }
        if rank < previous && context_tokens > DOWNGRADE_MAX_CONTEXT_TOKENS {
            rank = previous;
            why.push(format!(
                "~{context_tokens} context tokens: no downgrade (cache rebuild) -> {}",
                LADDER[rank].name
            ));
        }
    }

    let (model, effort) = resolve(rank);
    json!({
        "trees": trees,
        "votes": ranks.iter().map(|r| LADDER[*r].name).collect::<Vec<_>>(),
        "median": LADDER[median].name,
        "spread": spread,
        "why": why,
        "final": {"rung": LADDER[rank].name, "model": model.id, "effort": effort},
    })
}

#[cfg(test)]
pub fn fake(nouls: &[(&str, f64)]) -> Answers {
    // Unlisted Nouls are a clear "no".
    QUESTIONS
        .iter()
        .map(|(k, _)| {
            let p = nouls.iter().find(|(n, _)| n == k).map_or(0.05, |(_, p)| *p);
            ((*k).to_string(), json!(p))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn final_rung(answers: &Answers, previous: Option<usize>, ctx: u64) -> String {
        route(answers, previous, ctx)["final"]["rung"]
            .as_str()
            .unwrap()
            .to_string()
    }
    fn name(rank: usize) -> String {
        LADDER[rank].name.to_string()
    }

    #[test]
    fn self_check_matches_python_reference() {
        assert_eq!(
            final_rung(&fake(&[("clarified", 0.95), ("trivial", 0.95)]), None, 0),
            name(H)
        );
        assert_eq!(final_rung(&fake(&[]), None, 0), name(H));
        assert_eq!(
            final_rung(&fake(&[("clarified", 0.95), ("security", 0.9)]), None, 0),
            name(O_MED)
        );
        let live = fake(&[
            ("clarified", 0.50),
            ("multi_component", 0.95),
            ("perf", 0.75),
            ("concurrency", 0.99),
            ("prior_failed", 0.99),
            ("security", 0.09),
            ("tests", 0.01),
            ("trivial", 0.08),
            ("open_ended", 0.14),
        ]);
        let d = route(&live, None, 0);
        assert_eq!(d["trees"]["scope"]["vote"], name(O_MED));
        assert_eq!(
            d["votes"],
            json!([
                name(S_LOW),
                name(O_MED),
                name(O_MED),
                name(O_HIGH),
                name(O_XHIGH)
            ])
        );
        assert_eq!(d["final"]["rung"], name(O_HIGH));
        assert_eq!(final_rung(&live, Some(O_HIGH), 0), name(O_XHIGH));
        assert_eq!(final_rung(&live, Some(O_XHIGH), 0), name(O_MAX));
        let mut unclear = live.clone();
        unclear.insert("clarified".into(), json!(0.27));
        unclear.insert("perf".into(), json!(0.41));
        assert_eq!(final_rung(&unclear, None, 0), name(O_HIGH));
        let small = fake(&[("clarified", 0.95), ("trivial", 0.95)]);
        assert_eq!(final_rung(&small, Some(O_HIGH), 50_000), name(O_HIGH));
        assert_eq!(final_rung(&small, Some(O_HIGH), 5_000), name(H));
        let inventory = fake(&[
            ("clarified", 0.89),
            ("multi_component", 0.23),
            ("perf", 0.33),
            ("concurrency", 1.0),
            ("tests", 0.99),
        ]);
        let votes = route(&inventory, None, 0)["votes"].clone();
        assert_eq!(
            votes
                .as_array()
                .unwrap()
                .iter()
                .filter(|v| **v == name(S_MED))
                .count(),
            3
        );
        assert_eq!(final_rung(&inventory, None, 0), name(O_MED));
    }

    fn tiered(answers: &Answers, previous: Option<usize>, fable: bool) -> String {
        route_tiered(answers, previous, 0, &Tuning::default(), fable)["final"]["rung"]
            .as_str()
            .unwrap()
            .to_string()
    }

    #[test]
    fn legacy_names_map_to_family_rungs() {
        assert_eq!(rank_of("haiku-4.5"), Some(H));
        assert_eq!(rank_of("sonnet-5/low"), Some(S_LOW));
        assert_eq!(rank_of("opus-5.5/max"), Some(O_MAX));
        assert_eq!(rank_of("opus/xhigh"), Some(O_XHIGH));
        assert_eq!(rank_of("fable/max"), Some(F_MAX));
        assert_eq!(rank_of("opus"), None);
    }

    #[test]
    fn fable_tier() {
        // Open-ended multi-part design: the design tree's TOP leaf votes Fable.
        let design = fake(&[
            ("clarified", 0.95),
            ("open_ended", 0.95),
            ("multi_component", 0.95),
        ]);
        let on = route_tiered(&design, None, 0, &Tuning::default(), true);
        let off = route_tiered(&design, None, 0, &Tuning::default(), false);
        assert_eq!(on["trees"]["design"]["vote"], name(F_HIGH));
        assert_eq!(off["trees"]["design"]["vote"], name(O_XHIGH));
        // One clear TOP vote floors the result at Fable; without Fable nothing changes.
        assert_eq!(on["final"]["rung"], name(F_HIGH));
        assert_eq!(
            off["final"]["rung"],
            route(&design, None, 0)["final"]["rung"]
        );
        // Reached through an "unsure" branch: no Fable floor.
        let unsure = fake(&[
            ("clarified", 0.95),
            ("open_ended", 0.55),
            ("multi_component", 0.95),
        ]);
        assert_ne!(tiered(&unsure, None, true), name(F_HIGH));
        // Unclear request: Haiku asks first, even when it looks like the hardest case.
        let vague = fake(&[
            ("clarified", 0.2),
            ("open_ended", 0.95),
            ("multi_component", 0.95),
        ]);
        assert_eq!(tiered(&vague, None, true), name(H));
        // A failed concurrency fix is the other hardest case.
        let race = fake(&[
            ("clarified", 0.95),
            ("prior_failed", 0.95),
            ("concurrency", 0.95),
        ]);
        assert_eq!(tiered(&race, None, true), name(F_HIGH));

        // A failed answer from Opus goes straight to Fable; without Fable, one step up.
        let failed = fake(&[("clarified", 0.95), ("prior_failed", 0.95)]);
        assert_eq!(tiered(&failed, Some(O_MED), true), name(F_HIGH));
        assert_eq!(tiered(&failed, Some(O_MED), false), name(O_HIGH));
        assert_eq!(tiered(&failed, Some(F_HIGH), true), name(F_MAX));
        assert_eq!(tiered(&failed, Some(F_MAX), true), name(F_MAX));
        // Fable turned off mid-conversation: its rung counts as Opus max.
        assert_eq!(tiered(&failed, Some(F_HIGH), false), name(O_MAX));
        // A failed Sonnet answer still climbs one step at a time.
        assert_eq!(tiered(&failed, Some(S_MED), true), name(S_HIGH));

        // Fable never shows up for work that doesn't reach the TOP leaves.
        assert_eq!(
            tiered(&fake(&[("clarified", 0.95), ("trivial", 0.95)]), None, true),
            name(H)
        );
        assert_eq!(
            tiered(&fake(&[("clarified", 0.95), ("security", 0.9)]), None, true),
            name(O_MED)
        );
    }

    #[test]
    fn fable_does_not_change_the_disagreement_spread() {
        let levels = [0.05, 0.5, 0.95];
        for &a in &levels {
            for &b in &levels {
                for &c in &levels {
                    let answers = fake(&[
                        ("clarified", 0.95),
                        ("open_ended", a),
                        ("multi_component", b),
                        ("prior_failed", c),
                        ("concurrency", a),
                        ("perf", b),
                    ]);
                    let on = route_tiered(&answers, None, 0, &Tuning::default(), true);
                    let off = route_tiered(&answers, None, 0, &Tuning::default(), false);
                    assert_eq!(on["spread"], off["spread"], "{answers:?}");
                }
            }
        }
    }
}
