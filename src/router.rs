//! Claude model + effort router. v2 (default): request type -> tier + effort -> modifiers ->
//! floors. v1 (JEV_ROUTER_RULES=v1): Jev Nouls -> five decision trees -> median vote -> guard
//! rails, a 1:1 port of router/jev_router.py (the reference). Pure logic, no I/O.

use serde_json::{json, Map, Value};

pub struct Rung {
    pub name: &'static str,
    pub model: &'static str,
    /// None for Haiku 4.5, which takes neither effort nor adaptive thinking.
    pub effort: Option<&'static str>,
}

/// Cheapest -> strongest.
pub const LADDER: [Rung; 8] = [
    Rung {
        name: "haiku-4.5",
        model: "claude-haiku-4-5",
        effort: None,
    },
    Rung {
        name: "sonnet-5/low",
        model: "claude-sonnet-5",
        effort: Some("low"),
    },
    Rung {
        name: "sonnet-5/medium",
        model: "claude-sonnet-5",
        effort: Some("medium"),
    },
    Rung {
        name: "sonnet-5/high",
        model: "claude-sonnet-5",
        effort: Some("high"),
    },
    Rung {
        name: "opus-5.5/medium",
        model: "claude-opus-5-5",
        effort: Some("medium"),
    },
    Rung {
        name: "opus-5.5/high",
        model: "claude-opus-5-5",
        effort: Some("high"),
    },
    Rung {
        name: "opus-5.5/xhigh",
        model: "claude-opus-5-5",
        effort: Some("xhigh"),
    },
    Rung {
        name: "opus-5.5/max",
        model: "claude-opus-5-5",
        effort: Some("max"),
    },
];
pub const H: usize = 0;
pub const S_LOW: usize = 1;
pub const S_MED: usize = 2;
pub const S_HIGH: usize = 3;
pub const O_MED: usize = 4;
pub const O_HIGH: usize = 5;
pub const O_XHIGH: usize = 6;
pub const O_MAX: usize = 7;

pub fn rank_of(name: &str) -> Option<usize> {
    LADDER.iter().position(|r| r.name == name)
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

/// Each node is (noul_key, if_yes, if_no); leaves are rungs.
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
                split("concurrency", Leaf(O_XHIGH), Leaf(O_HIGH)),
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
                split("multi_component", Leaf(O_XHIGH), Leaf(O_HIGH)),
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
    fn node(n: &Node) -> Value {
        match n {
            Node::Leaf(rank) => json!({"leaf": LADDER[*rank].name}),
            Node::Split(key, yes, no) => json!({"key": key, "yes": node(yes), "no": node(no)}),
        }
    }
    let forest: Map<String, Value> = forest()
        .iter()
        .map(|(name, n)| ((*name).to_string(), node(n)))
        .collect();
    json!({
        "ladder": LADDER.iter().map(|r| json!({"name": r.name, "model": r.model, "effort": r.effort})).collect::<Vec<_>>(),
        "questions": QUESTIONS.iter().map(|(k, q)| json!({"key": k, "instructions": q}))
            .chain(questions_v2().into_iter().filter(|(_, q)| q["type"] == "noul")
                .map(|(k, q)| json!({"key": k, "instructions": q["instructions"]})))
            .collect::<Vec<_>>(),
        "intents": INTENTS.iter().map(|i| json!({"key": i.key, "start": LADDER[to_ladder(i.tier, i.effort)].name, "what": i.what})).collect::<Vec<_>>(),
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
    walk_with(node, answers, path, BORDERLINE)
}

fn walk_with(
    node: &Node,
    answers: &Answers,
    path: Vec<Step>,
    borderline: f64,
) -> (usize, Vec<Step>) {
    match node {
        Node::Leaf(rank) => (*rank, path),
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
                let yes = walk_with(if_yes, answers, step("unsure->yes"), borderline);
                let no = walk_with(if_no, answers, step("unsure->no"), borderline);
                return if no.0 > yes.0 { no } else { yes };
            }
            if p > NOUL_YES {
                walk_with(if_yes, answers, step("yes"), borderline)
            } else {
                walk_with(if_no, answers, step("no"), borderline)
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
    let mut trees = Map::new();
    let mut ranks = Vec::new();
    for (name, tree) in forest() {
        let (rung, path) = walk_with(&tree, answers, Vec::new(), tuning.borderline);
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
    let median = ranks[ranks.len() / 2];
    let spread = ranks[ranks.len() - 1] - ranks[0];
    let mut rank = median;
    let mut why = vec![format!("median of votes: {}", LADDER[median].name)];

    if spread >= tuning.disagree_spread {
        rank = (rank + 1).min(LADDER.len() - 1);
        why.push(format!(
            "trees disagree (spread {spread} rungs): +1 -> {}",
            LADDER[rank].name
        ));
    }

    if tuning.offset != 0 {
        rank = (rank as i32 + tuning.offset).clamp(0, LADDER.len() as i32 - 1) as usize;
        why.push(format!(
            "auto-tuning: {:+} -> {}",
            tuning.offset, LADDER[rank].name
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
            let floor = (previous + 1).min(LADDER.len() - 1);
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

    let rung = &LADDER[rank];
    json!({
        "trees": trees,
        "votes": ranks.iter().map(|r| LADDER[*r].name).collect::<Vec<_>>(),
        "median": LADDER[median].name,
        "spread": spread,
        "why": why,
        "final": {"rung": rung.name, "model": rung.model, "effort": rung.effort},
    })
}

// --- Decision v2 ---------------------------------------------------------------------
// Request type (Choice) sets a starting model tier and effort; two Scores (reasoning depth,
// reading breadth) and atomic Nouls adjust it; floors only raise. v1 above stays as is
// (JEV_ROUTER_RULES=v1, parity fixtures, auto-tuning). Plan: docs/decision-v2-plan.md.

pub const TIER_HAIKU: usize = 0;
pub const TIER_SONNET: usize = 1;
pub const TIER_OPUS: usize = 2;
/// Effort index: low, medium, high, xhigh, max.
pub const EFFORT_MAX: usize = 4;

/// Intent confidence below this: take the harder of the two likeliest intents.
pub const INTENT_SURE: f64 = 0.5;
/// Haiku needs at least this intent confidence (plus low depth, breadth and risk).
pub const HAIKU_SURE: f64 = 0.7;
/// `depth` (0-3): from here at least Sonnet; from DEPTH_HARD one tier stronger.
pub const DEPTH_SONNET: f64 = 1.5;
pub const DEPTH_HARD: f64 = 2.5;
/// `breadth` (0-3): effort +1 from BREADTH_WIDE, another +1 from BREADTH_REPO.
pub const BREADTH_WIDE: f64 = 2.0;
pub const BREADTH_REPO: f64 = 2.8;

pub struct Intent {
    pub key: &'static str,
    pub tier: usize,
    pub effort: usize,
    pub what: &'static str,
    pub not_for: &'static str,
    pub examples: &'static [&'static str],
}

const fn intent(
    key: &'static str,
    tier: usize,
    effort: usize,
    what: &'static str,
    not_for: &'static str,
    examples: &'static [&'static str],
) -> Intent {
    Intent {
        key,
        tier,
        effort,
        what,
        not_for,
        examples,
    }
}

/// Request types and where each one starts. The one place to tune categories.
pub const INTENTS: [Intent; 13] = [
    intent("chat", TIER_HAIKU, 0,
        "Greetings, thanks, small talk, or a reply that needs no work",
        "Short replies that ask for work, such as 'fix it'",
        &["hello", "thanks!", "Reply with just the word ok"]),
    intent("general_knowledge", TIER_HAIKU, 0,
        "A general programming or computing question answerable without looking at the repository",
        "Questions about how this repository's own code works",
        &["what is a Python decorator?", "what does git stash do?", "difference between TCP and UDP"]),
    intent("off_topic", TIER_HAIKU, 0,
        "A request unrelated to software, such as weather, news or trivia",
        "Anything about code, tools or the repository",
        &["what is the weather today in Kathmandu", "who won the match yesterday"]),
    intent("lookup", TIER_HAIKU, 0,
        "Find, show, list or run one simple thing in the repository and report the result",
        "Explaining how code works, or changing code",
        &["list the files in src", "what port does the dashboard use?", "run ls and tell me the count"]),
    intent("explain_code", TIER_SONNET, 1,
        "Explain or summarise how the repository, a part of it, or some code works",
        "General knowledge questions that don't need the repository; reviews looking for problems",
        &["can you please explain this project", "how does routing work here?", "walk me through proxy.rs"]),
    intent("review_audit", TIER_SONNET, 2,
        "Review, audit or critique code, a diff or a design for bugs, risks or quality",
        "Explaining code without judging it; fixing a known bug",
        &["review this diff", "audit the repo for security issues", "is this implementation correct?"]),
    intent("plan_design", TIER_OPUS, 2,
        "Plan work, design or redesign a system, or give an opinion or recommendation on an approach",
        "Carrying out an already decided, well-specified change",
        &["create a plan to improve the decision rules", "how should we architect the sync service?", "what's your opinion on this design?"]),
    intent("debug_fix", TIER_SONNET, 1,
        "Something is broken: find the cause of a bug, error, failing test or wrong behaviour and fix it",
        "Adding new behaviour that never existed",
        &["the tests fail, find the bug and fix it", "this throws KeyError on checkout", "why does the proxy hang?"]),
    intent("implement", TIER_SONNET, 1,
        "Write new code: a feature, function, endpoint, script or test",
        "Restructuring existing code without changing behaviour; fixing a bug",
        &["add a --json flag to the log command", "write a thread-safe Inventory class", "add tests for cart.py"]),
    intent("refactor_migrate", TIER_SONNET, 1,
        "Restructure, rename, clean up, upgrade or migrate existing code without changing what it does",
        "New features; bug fixes",
        &["rename Router to Proxy everywhere", "upgrade to serde 2", "split main.rs into modules"]),
    intent("ops_config", TIER_SONNET, 0,
        "Git, build, packaging, CI, environment, install or deployment tasks",
        "Changing application logic",
        &["commit this and open a PR", "why does cargo build fail to link?", "set up the release workflow"]),
    intent("write_text", TIER_SONNET, 0,
        "Write or edit prose: docs, README, comments, commit or PR messages",
        "Writing code",
        &["update the README", "write a commit message for this", "document the CLI flags"]),
    intent("other", TIER_SONNET, 1,
        "Anything that fits none of the other options",
        "",
        &[]),
];

/// Coding-action intents: never capped for "unclear", the agent can explore the repo first.
const ACTION_INTENTS: [&str; 3] = ["debug_fix", "implement", "refactor_migrate"];
/// Questions *about* a topic ("process vs thread?") aren't work on it: no security or
/// concurrency floor for these. The irreversible floor always applies.
const NO_WORK_INTENTS: [&str; 3] = ["chat", "general_knowledge", "off_topic"];

const SCOPE: &str = "The assistant is the coding agent described in `environment`: it can read the whole repository itself, so 'this project', 'this folder' or 'this file' refer to that repository. `environment` and `history` are background only.";

/// v2 questions beyond v1's Nouls (v2 also reuses v1's prior_failed, security, concurrency,
/// perf, tests). Same rules as v1: atomic, a high Noul always means yes, ids never sent.
pub fn questions_v2() -> Vec<(&'static str, Value)> {
    let intent_criteria: Map<String, Value> = INTENTS
        .iter()
        .map(|i| {
            let mut c = json!({"what": i.what});
            if !i.not_for.is_empty() {
                c["not_for"] = json!(i.not_for);
            }
            if !i.examples.is_empty() {
                c["examples"] = json!(i.examples);
            }
            (i.key.to_string(), c)
        })
        .collect();
    let noul = |q: &str| json!({"type": "noul", "instructions": format!("{SCOPE} {q}")});
    vec![
        ("intent", json!({"type": "choice",
            "instructions": format!("{SCOPE} What kind of request is `latest`?"),
            "criteria": intent_criteria})),
        ("depth", json!({"type": "score",
            "instructions": format!("{SCOPE} How much reasoning does handling `latest` well require?"),
            "criteria": [
                "Recalling a fact, looking something up, replying briefly, or running a given command",
                "Applying a known, straightforward approach to a clearly described task",
                "Multi-step reasoning: finding an unknown cause, or coordinating several dependent changes",
                "Weighing trade-offs, designing something new, or getting subtle correctness right"]})),
        ("breadth", json!({"type": "score",
            "instructions": format!("{SCOPE} How much of the repository must the assistant read or change to handle `latest` well?"),
            "criteria": [
                "None: general knowledge or the conversation is enough",
                "One file, one function, or a pasted snippet",
                "Several related files or one module",
                "A broad view of the repository: many modules, or several services"]})),
        ("can_start", noul("Can the assistant start working on `latest` right away, reading the repository as needed, without first asking the user a question?")),
        ("missing_user_info", noul("Does `latest` leave out information that only the user can give, such as their goal, a choice between options, or a value, and that cannot be found in the repository or in `history`?")),
        ("repeat_failure", noul("Is `latest` at least the second user message saying a solution failed, meaning an earlier user message in `history` already said a previous solution did not work?")),
        ("irreversible", noul("Could carrying out `latest` destroy or overwrite data, change a database schema, affect a production system, rewrite git history, or spend money?")),
        ("asks_quality", noul("Does `latest` explicitly ask for thoroughness, care, depth, or the best possible answer?")),
        ("asks_stronger_model", noul("Does `latest` ask the assistant to use a stronger or more capable AI model, or say the current model is not good enough?")),
        ("compound", noul("Does `latest` contain two or more separate tasks that each need their own work?")),
        ("unfamiliar_domain", noul("Does handling `latest` need specialist knowledge, such as numerical methods, cryptography, compilers, distributed consensus, embedded hardware, or a niche framework?")),
    ]
}

/// A model the user named for this turn ("switch to opus 5.5 high", "use haiku"), found in
/// code. Negated mentions ("don't use haiku") don't count.
/// ponytail: English verbs only; the asks_stronger_model Noul covers indirect phrasing.
pub fn requested_rung(text: &str) -> Option<usize> {
    static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let re = RE.get_or_init(|| {
        regex::Regex::new(concat!(
            r"(?i)\b(?:use|using|switch(?:\s+over)?\s+to|switch|change\s+to|move\s+to|go\s+with|run\s+(?:it\s+|this\s+)?(?:on|with)|try)\s+",
            r"(?:the\s+)?(haiku|sonnet|opus)(?:[\s-]*\d+(?:\.\d+)?)?",
            r"(?:\s*(?:at|with|on|,|/|·|-)?\s*(low|medium|high|xhigh|max)\b)?",
        ))
        .unwrap()
    });
    let caps = re.captures_iter(text).last()?;
    let start = caps.get(0)?.start();
    let before = text[..start].to_lowercase();
    let before = before.trim_end();
    if [
        "don't",
        "dont",
        "do not",
        "never",
        "not",
        "stop",
        "instead of",
        "no need to",
    ]
    .iter()
    .any(|n| before.ends_with(n))
    {
        return None;
    }
    let effort = caps.get(2).map(|m| m.as_str().to_lowercase());
    let rung = match (caps[1].to_lowercase().as_str(), effort.as_deref()) {
        ("haiku", _) => H,
        ("sonnet", Some("low")) => S_LOW,
        ("sonnet", Some("high" | "xhigh" | "max")) => S_HIGH,
        ("sonnet", _) => S_MED,
        ("opus", Some("low" | "medium")) => O_MED,
        ("opus", Some("xhigh")) => O_XHIGH,
        ("opus", Some("max")) => O_MAX,
        ("opus", _) => O_HIGH,
        _ => return None,
    };
    Some(rung)
}

fn to_ladder(tier: usize, effort: usize) -> usize {
    match tier {
        TIER_HAIKU => H,
        // Sonnet has no xhigh/max: two steps past high means Opus (golden set: one step
        // past high sent too much to Opus for no gain).
        TIER_SONNET => [S_LOW, S_MED, S_HIGH, S_HIGH, O_MED][effort.min(EFFORT_MAX)],
        _ => [O_MED, O_MED, O_HIGH, O_XHIGH, O_MAX][effort.min(EFFORT_MAX)],
    }
}

fn base_of(key: &str) -> (usize, usize) {
    INTENTS
        .iter()
        .find(|i| i.key == key)
        .map_or((TIER_SONNET, 1), |i| (i.tier, i.effort))
}

/// One step up in model tier from a rung: Haiku -> Sonnet medium, Sonnet -> Opus medium,
/// Opus -> two effort steps.
fn next_tier(rank: usize) -> usize {
    match rank {
        H => S_MED,
        S_LOW..=S_HIGH => O_MED,
        r => (r + 2).min(O_MAX),
    }
}

/// Answers + code-known facts -> v2 decision. `structured` holds the Choice/Score answers
/// (`intent`, `depth`, `breadth`); `requested` is a model the user named (requested_rung).
pub fn route_v2(
    answers: &Answers,
    structured: &Map<String, Value>,
    previous_rung: Option<usize>,
    context_tokens: u64,
    requested: Option<usize>,
) -> Value {
    let p = |k: &str| noul(answers, k);
    let yes = |k: &str| p(k) > CLEAR_YES;
    let name = |r: usize| LADDER[r].name;
    let mut ranked: Vec<(String, f64)> = structured["intent"]["probabilities"]
        .as_object()
        .into_iter()
        .flatten()
        .map(|(k, v)| (k.clone(), v.as_f64().unwrap_or(0.0)))
        .collect();
    ranked.sort_by(|a, b| b.1.total_cmp(&a.1));
    let top = ranked.first().map_or("other".to_string(), |r| r.0.clone());
    let confidence = structured["intent"]["confidence"].as_f64().unwrap_or(0.0);
    let depth = structured["depth"]["score"].as_f64().unwrap_or(1.0);
    let breadth = structured["breadth"]["score"].as_f64().unwrap_or(1.0);

    let (mut tier, mut effort) = base_of(&top);
    let mut why = vec![format!(
        "request type: {top} ({confidence:.2}) -> start at {}",
        name(to_ladder(tier, effort))
    )];
    if confidence < INTENT_SURE {
        if let Some((second, _)) = ranked.get(1) {
            let (t2, e2) = base_of(second);
            if (t2, e2) > (tier, effort) {
                (tier, effort) = (tier.max(t2), effort.max(e2));
                why.push(format!(
                    "unsure between {top} and {second}: the harder one -> {}",
                    name(to_ladder(tier, effort))
                ));
            }
        }
    }
    if depth >= DEPTH_SONNET && tier == TIER_HAIKU {
        tier = TIER_SONNET;
        why.push(format!(
            "reasoning depth {depth:.1}: at least Sonnet -> {}",
            name(to_ladder(tier, effort))
        ));
    }
    if (depth >= DEPTH_HARD || yes("unfamiliar_domain")) && tier < TIER_OPUS {
        tier += 1;
        let reason = if depth >= DEPTH_HARD {
            format!("reasoning depth {depth:.1}")
        } else {
            "specialist domain".into()
        };
        why.push(format!(
            "{reason}: stronger model -> {}",
            name(to_ladder(tier, effort))
        ));
    }
    let mut more = Vec::new();
    if breadth >= BREADTH_WIDE {
        more.push(format!("reads broadly ({breadth:.1})"));
    }
    if breadth >= BREADTH_REPO {
        more.push("much of the repository".into());
    }
    for (k, label) in [
        ("asks_quality", "asks for thoroughness"),
        ("compound", "several tasks"),
        ("perf", "performance needs"),
    ] {
        if yes(k) {
            more.push(label.into());
        }
    }
    let risks = ["security", "concurrency", "irreversible"];
    if risks.iter().any(|k| (p(k) - NOUL_YES).abs() < BORDERLINE) {
        more.push("unsure about a risk".into());
    }
    if !more.is_empty() {
        effort = (effort + more.len()).min(EFFORT_MAX);
        why.push(format!(
            "more effort: {} -> {}",
            more.join(", "),
            name(to_ladder(tier, effort))
        ));
    }
    if yes("tests") && tier > TIER_HAIKU && effort == 0 {
        effort = 1;
        why.push(format!(
            "asks for tests: effort -> {}",
            name(to_ladder(tier, effort))
        ));
    }

    // Haiku only by agreement: a sure, shallow, narrow, risk-free request.
    if tier == TIER_HAIKU {
        let risky = risks
            .iter()
            .chain(&["prior_failed"])
            .any(|k| p(k) > CLEAR_NO);
        if confidence < HAIKU_SURE || depth >= 1.0 || breadth >= 1.5 || effort > 0 || risky {
            tier = TIER_SONNET;
            why.push(format!(
                "not clearly simple: no Haiku -> {}",
                name(to_ladder(tier, effort))
            ));
        }
    }
    let mut rank = to_ladder(tier, effort);

    if let Some(r) = requested {
        rank = r;
        why.push(format!("you asked for a model: -> {}", name(r)));
    } else if yes("asks_stronger_model") {
        let floor = previous_rung.map_or(O_MED, |prev| (prev + 1).clamp(O_MED, O_MAX));
        if rank < floor {
            rank = floor;
            why.push(format!(
                "asks for a stronger model: floor -> {}",
                name(rank)
            ));
        }
    }
    // The cap needs two answers to agree, skips coding actions (the agent can explore
    // first) and only goes down to Sonnet low. It runs before the floors, so they win.
    let unclear = p("can_start") < CLEAR_NO && yes("missing_user_info");
    if unclear
        && requested.is_none()
        && !ACTION_INTENTS.contains(&top.as_str())
        && !yes("prior_failed")
        && rank > S_LOW
    {
        rank = S_LOW;
        why.push(format!(
            "needs clarification: cap -> {} asks the questions",
            name(rank)
        ));
    }
    let work = !(NO_WORK_INTENTS.contains(&top.as_str()) && confidence >= INTENT_SURE);
    for (k, label) in [
        ("security", "security-sensitive"),
        ("concurrency", "concurrency"),
        ("irreversible", "irreversible"),
    ] {
        if yes(k) && rank < O_MED && (work || k == "irreversible") {
            rank = O_MED;
            why.push(format!("{label}: floor -> {}", name(rank)));
        }
    }
    if let Some(previous) = previous_rung {
        if yes("prior_failed") {
            let (floor, how) = if yes("repeat_failure") {
                (next_tier(previous), "failed again")
            } else {
                ((previous + 1).min(O_MAX), "failed")
            };
            if rank < floor {
                rank = floor;
                why.push(format!(
                    "last answer ({}) {how}: floor -> {}",
                    name(previous),
                    name(floor)
                ));
            }
        }
        if rank < previous && context_tokens > DOWNGRADE_MAX_CONTEXT_TOKENS {
            rank = previous;
            why.push(format!(
                "~{context_tokens} context tokens: no downgrade (cache rebuild) -> {}",
                name(rank)
            ));
        }
    }

    let rung = &LADDER[rank];
    json!({
        "rules": "v2",
        "intent": {"choice": top, "confidence": confidence,
                   "runner_up": ranked.get(1).map(|r| &r.0)},
        "depth": depth,
        "breadth": breadth,
        "requested": requested.map(name),
        "why": why,
        "final": {"rung": rung.name, "model": rung.model, "effort": rung.effort},
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

    /// v2 answers: every Noul a clear no unless listed; one sure intent.
    fn v2(
        intent: &str,
        conf: f64,
        depth: f64,
        breadth: f64,
        nouls: &[(&str, f64)],
    ) -> (Answers, Map<String, Value>) {
        let mut answers = fake(nouls);
        for (k, q) in questions_v2() {
            if q["type"] == "noul" {
                let default = if k == "can_start" { 0.9 } else { 0.05 };
                let p = nouls
                    .iter()
                    .find(|(n, _)| *n == k)
                    .map_or(default, |(_, p)| *p);
                answers.insert(k.into(), json!(p));
            }
        }
        let mut probs = Map::new();
        probs.insert(intent.into(), json!(conf));
        probs.insert("other".into(), json!(1.0 - conf));
        let structured = json!({
            "intent": {"choice": intent, "probabilities": probs, "confidence": conf},
            "depth": {"score": depth}, "breadth": {"score": breadth},
        });
        (answers, structured.as_object().unwrap().clone())
    }
    fn rung_v2(
        c: &(Answers, Map<String, Value>),
        previous: Option<usize>,
        ctx: u64,
        requested: Option<usize>,
    ) -> String {
        route_v2(&c.0, &c.1, previous, ctx, requested)["final"]["rung"]
            .as_str()
            .unwrap()
            .to_string()
    }

    #[test]
    fn v2_intent_sets_the_start() {
        assert_eq!(
            rung_v2(&v2("general_knowledge", 0.9, 0.1, 0.0, &[]), None, 0, None),
            name(H)
        );
        assert_eq!(
            rung_v2(&v2("explain_code", 0.9, 1.2, 1.5, &[]), None, 0, None),
            name(S_MED)
        );
        assert_eq!(
            rung_v2(&v2("plan_design", 0.9, 2.0, 2.0, &[]), None, 0, None),
            name(O_XHIGH)
        );
        assert_eq!(
            rung_v2(&v2("review_audit", 0.9, 1.5, 1.5, &[]), None, 0, None),
            name(S_HIGH)
        );
        assert_eq!(
            rung_v2(&v2("off_topic", 0.9, 0.0, 0.0, &[]), None, 0, None),
            name(H)
        );
    }

    #[test]
    fn v2_explain_this_project_is_not_haiku_even_when_jev_calls_it_unclear() {
        // The logged failure: clarified=0.05 capped it to Haiku under v1.
        let c = v2(
            "explain_code",
            0.8,
            1.3,
            2.9,
            &[("can_start", 0.05), ("missing_user_info", 0.3)],
        );
        let r = rank_of(&rung_v2(&c, None, 0, None)).unwrap();
        assert!(r >= S_HIGH, "{}", name(r));
        // Only both answers agreeing cap it, and only to Sonnet low.
        let c = v2(
            "explain_code",
            0.8,
            1.3,
            2.9,
            &[("can_start", 0.05), ("missing_user_info", 0.9)],
        );
        assert_eq!(rung_v2(&c, None, 0, None), name(S_LOW));
        // Coding actions are never capped: the agent can read the repo first.
        let c = v2(
            "debug_fix",
            0.9,
            2.0,
            1.0,
            &[
                ("can_start", 0.05),
                ("missing_user_info", 0.9),
                ("tests", 0.96),
            ],
        );
        assert_eq!(rung_v2(&c, None, 0, None), name(S_MED));
    }

    #[test]
    fn v2_haiku_needs_agreement() {
        assert_eq!(
            rung_v2(&v2("general_knowledge", 0.6, 0.1, 0.0, &[]), None, 0, None),
            name(S_LOW)
        );
        assert_eq!(
            rung_v2(&v2("lookup", 0.9, 0.2, 1.8, &[]), None, 0, None),
            name(S_LOW)
        );
        assert_eq!(
            rung_v2(
                &v2("chat", 0.9, 0.0, 0.0, &[("security", 0.4)]),
                None,
                0,
                None
            ),
            name(S_MED)
        );
        // Unsure between chat and a harder intent: the harder one wins.
        let mut c = v2("chat", 0.45, 0.5, 0.5, &[]);
        c.1["intent"]["probabilities"] = json!({"chat": 0.45, "debug_fix": 0.4, "other": 0.15});
        assert_eq!(rung_v2(&c, None, 0, None), name(S_MED));
    }

    #[test]
    fn v2_floors_and_failures() {
        let c = v2(
            "implement",
            0.9,
            1.5,
            1.0,
            &[("concurrency", 0.99), ("tests", 0.99)],
        );
        assert_eq!(rung_v2(&c, None, 0, None), name(O_MED));
        let c = v2("ops_config", 0.9, 1.0, 1.0, &[("irreversible", 0.9)]);
        assert_eq!(rung_v2(&c, None, 0, None), name(O_MED));
        let failed = v2("debug_fix", 0.9, 1.5, 1.0, &[("prior_failed", 0.9)]);
        assert_eq!(rung_v2(&failed, Some(S_MED), 0, None), name(S_HIGH));
        let again = v2(
            "debug_fix",
            0.9,
            1.5,
            1.0,
            &[("prior_failed", 0.9), ("repeat_failure", 0.9)],
        );
        assert_eq!(rung_v2(&again, Some(S_HIGH), 0, None), name(O_MED));
        assert_eq!(rung_v2(&again, Some(O_HIGH), 0, None), name(O_MAX));
        // Cache guard still holds a long conversation on its rung.
        let small = v2("chat", 0.9, 0.0, 0.0, &[]);
        assert_eq!(rung_v2(&small, Some(O_HIGH), 50_000, None), name(O_HIGH));
        assert_eq!(rung_v2(&small, Some(O_HIGH), 5_000, None), name(H));
    }

    #[test]
    fn v2_honours_a_requested_model_but_not_over_safety() {
        let c = v2("explain_code", 0.9, 1.0, 1.0, &[]);
        assert_eq!(rung_v2(&c, None, 0, Some(O_HIGH)), name(O_HIGH));
        let risky = v2("implement", 0.9, 1.0, 1.0, &[("security", 0.9)]);
        assert_eq!(rung_v2(&risky, None, 0, Some(H)), name(O_MED));
        let c = v2(
            "explain_code",
            0.9,
            1.0,
            1.0,
            &[("asks_stronger_model", 0.9)],
        );
        assert_eq!(rung_v2(&c, Some(S_HIGH), 0, None), name(O_MED));
    }

    #[test]
    fn requested_rung_reads_explicit_model_names() {
        assert_eq!(
            requested_rung("please switch to opus 5.5 high for this"),
            Some(O_HIGH)
        );
        assert_eq!(requested_rung("use sonnet at low effort"), Some(S_LOW));
        assert_eq!(requested_rung("Use Haiku for this one"), Some(H));
        assert_eq!(requested_rung("run it on opus max"), Some(O_MAX));
        assert_eq!(requested_rung("don't use haiku for this"), None);
        assert_eq!(requested_rung("why invoke an extra haiku model here"), None);
        assert_eq!(requested_rung("explain this project"), None);
    }
}
