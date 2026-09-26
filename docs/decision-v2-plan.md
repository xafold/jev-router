# Decision v2: plan to fix routing rules and categories

Status: implemented on `feat/decision-v2` (from `main` 7a532f6), 2026-09-26. Written 2026-09-25.

**What shipped, and where it differs from this plan:**

- **Stages 0-3 are implemented, and v2 is now the default.**
  - v1 still runs on the same answers, and every record logs it under `shadow`.
  - `JEV_ROUTER_RULES=v1` serves v1 instead.
  - v1's code and questions are untouched, so the parity fixtures and auto-tuning still apply to it.
  - Phase 0 wasn't applied to v1 separately, because v2 carries those fixes.
- **The repo-summary switch is an env var,** `JEV_ROUTER_REPO_SUMMARY=off`, not a persisted `--repo-summary` flag.
- **Background prompts stay on the pinned model.** Claude Code's own prompts and "continue"-style replies skip Jev and keep the current rung, falling back to Haiku only when nothing is pinned.
- **Golden-set results** (110 prompts, answers recorded 2026-09-26 on jev-1.13.0):

  | | in range | too low | too high | cost index |
  |---|---|---|---|---|
  | v2 | 98.2% | 0.9% | 0.9% | 1.11 |
  | v1 | 67.3% | 30.0% | 2.7% | 0.89 |

  Other numbers:
  - Request type (intent) accuracy: 96%.
  - Heavy intents sent to Haiku: 0.
  - Jev latency with 20 questions: p50 385 ms, max 622 ms.
  - The cost index is the model input price relative to always-Sonnet. v2 costs more than v1 mainly because v1 under-routed 30% of prompts.
- **Two rules were changed after the first replay:**
  - Sonnet effort two steps past "high" becomes Opus medium. One step past sent too much to Opus.
  - The security and concurrency floors skip clearly conversational intents (chat, general knowledge, off-topic): "process vs thread?" is not concurrency work.
- **Not done yet:**
  - Phase 4: auto-tuning for v2, and the rung-distribution warning.
  - Wording A/B tests.
  - Measuring on real logged traffic.

## 1. What is wrong today (evidence)

Source: all 38 records in `~/.local/share/claude-router/decisions.jsonl` plus 13 dashboard ratings.

| # | Problem | Evidence |
|---|---|---|
| P1 | **The clarify cap misfires.** A single `clarified` Noul below 0.35 sends the turn to Haiku. | Fired on 14/38 turns (37%). "python3 -m unittest fails in this folder. Find the bug in cart.py, fix it…" got `clarified=0.24`, went to Haiku and was rated `too_low`. "can you please explain this project" got `clarified=0.05` and went to Haiku. |
| P2 | **Jev can't see the workspace.** State is only `{history, latest}`, so "this folder" and "this project" read as unresolved references. | 33/38 turns had empty history. Every prompt that points at the workspace got a low `clarified`. This is Jev 1.13's documented literal-reading failure. |
| P3 | **Questions only fit code changes.** `trivial`, `multi_component` and `open_ended` all describe *changes*. There is no question for explaining, reviewing, giving an opinion, exploring, operations or off-topic requests. | Explain-project got `multi_component=0.20` and `trivial=0.48` (unsure). About 19% of real assistant traffic is questions ("Programming by Chat", arXiv 2604.00436) and about 8% is project comprehension. None of it has a category. |
| P4 | **"No signal" is read as "easy".** When every answer is no, the trees fall to their cheapest leaves. | Outcomes collapse to two: Haiku (18/38) or Sonnet medium (8/38). Sonnet low, Sonnet high and most Opus rungs almost never occur. |
| P5 | **Off-topic requests are over-routed.** | "what is the weather today at kathmandu", the web-search follow-up and the away-recap went to Sonnet medium. All three were rated `too_high`. |
| P6 | **Claude Code's internal prompts go through Jev.** | `[SUGGESTION MODE: …]` and "The user stepped away… Recap" were routed like user turns. |
| P7 | **Explicit user wishes are ignored.** | "please switch to opus 5.5 high" was read as `prior_failed=0.64` and routed to Sonnet high. |
| P8 | **"Continue" / "yes, do it" is re-routed from scratch.** In short conversations it can downgrade mid-task. | Workflow-control turns are 11.5% of traffic (2604.00436). Only the cache guard (>20k tokens) protects them today. |
| P9 | **The median discards the hard signal.** One tree seeing a hard case can't move the result. Two trees reaching Haiku plus the cap can. | 5-tree median plus a one-Noul cap: nothing in the literature does this. RoRF averages votes against a threshold, RouteLLM uses one calibrated threshold. |

## 2. Design principles for v2

1. **Separate model from effort.** Anthropic's guidance: upgrade the *model* for subtle bugs, unfamiliar domains or architecture; raise *effort* when the work needs broad reading or verification. Compute a capability tier (Haiku / Sonnet / Opus) and an effort level separately, then map them to the ladder.
2. **Intent sets the base, Nouls adjust it.** A category axis (Arch-Router style Domain x Action) plus atomic modifiers. Every prompt gets an intent, including `other`.
3. **Asymmetric costs.** A single strong signal can raise the rung. Lowering it takes agreement: Haiku only when intent is cheap, confidence is high, and depth, breadth and risk are all low.
4. **Uncertain means Sonnet medium, never Haiku.** The default rung when signals are missing or unsure.
5. **Code decides what code can see.** Claude Code's internal prompts, explicit model requests, continuations and context size are detected in code, not by Jev.
6. **Still one Jev request.** Fan out every question in one call. The docs measured 13 questions at 0.27 s and 62 at 0.51 s. Estimated cost: about 2.5k input tokens, roughly $0.0001 per turn.

## 3. Stage 0: code-side pre-routing (no Jev call)

Runs in `proxy.rs` before `decide`. Each check is cheap and deterministic.

| Check | Rule | Fixes |
|---|---|---|
| Claude Code internal prompts | Prefix match on known markers (`[SUGGESTION MODE:`, "The user stepped away and is coming back", title/summary templates) skips Jev and **reuses the conversation's pinned rung**. These requests share the conversation prefix, so switching model would turn a cache read into a full cache write. Use Haiku only when nothing is pinned yet. Log it as `source: "internal"`. Keep the marker list in one const and re-verify it on each Claude Code version bump. | P6, part of P5 |
| Explicit model request | Regex over `latest` for a model family (+ optional effort) next to a verb such as "use", "switch to" or "run on". It sets a **floor** for this turn and pins the conversation until the user says otherwise. A Jev Noul (§4) backs this up for indirect phrasing. | P7 |
| Continuation | Short `latest` (<= 40 chars) matching continue/go on/yes/do it/proceed/ok, with a previous rung, keeps that rung. No Jev call. | P8 |
| Existing | Tool-less calls go to Haiku, tool-loop turns keep their rung, retries keep their rung, `/model` override: unchanged. | |

## 4. Stage 1: state and question set v2

### State

```json
{
  "environment": {
    "assistant": "a coding agent with the user's repository open; it can read any file, search, run commands and edit code",
    "repository": "<cwd basename>",
    "repository_summary": "<opening of <cwd>/CLAUDE.md, else README.md: clipped to ~800 chars, redacted>",
    "first_turn": true
  },
  "history": ["... up to 7 text turns, unchanged ..."],
  "latest": "..."
}
```

Questions that depend on the workspace name `` `environment` `` explicitly, e.g. "…treating 'this project/folder/file' as the repository in `environment`". This follows the docs' fix for literal reading: write the missing assumption into the question.

**`repository_summary`: short, not the whole CLAUDE.md.**

- **Source.** The proxy already knows `cwd`. It reads `<cwd>/CLAUDE.md` from disk (README.md as fallback), takes the opening description up to the first `##` heading, clips it to ~800 chars, and runs it through the existing `redact()`. The result is cached per `cwd`.
- **Why not the whole file.** Irrelevant state lowers accuracy (Jev 1.13 context rot). A full CLAUDE.md is mostly commands, layout and caveats, and none of that helps the questions. This repo's CLAUDE.md is ~2.5k tokens.
- **Which answers it helps.** `can_start` (it can see what "this project" is), `unfamiliar_domain` and `depth`.
- **Guarding against leakage.** A repo *about* security or async code could push `security`/`concurrency` up on every turn. Every question keeps saying "answer about `latest`; `environment` is background only". The golden set includes plain prompts asked in such a repo to catch leakage.
- **Privacy.** This extends what goes to api.typesafe.ai beyond "prompt + 7 turns". Add a `--repo-summary on|off` toggle (default on, persisted like `--fable`) and update the privacy line in the README.
- **Prove it.** Run the golden set with and without the summary. Keep it only if in-band accuracy improves and security/concurrency false positives don't rise.

### Questions (18, one call)

**Intent (Choice, structured criteria `{what, not_for, examples}`, plus `other`)**

| Option | Base tier / effort | Examples |
|---|---|---|
| `chat` | Haiku | hello, thanks, meta-chat |
| `general_knowledge` | Haiku | "what is a decorator", "what does git stash do" |
| `off_topic` | Haiku | weather, news, non-software |
| `lookup` | Haiku -> Sonnet low | "list the files", "what's in config.toml", "run ls" |
| `explain_code` | Sonnet medium | "explain this project", "how does routing work here" |
| `review_audit` | Sonnet high | "review this diff", "audit for bugs", "is this safe" |
| `plan_design` | Opus high | "plan the migration", "opinion on the architecture", "how should we redesign X" |
| `debug_fix` | Sonnet medium | failing test, stack trace, "X is broken" |
| `implement` | Sonnet medium | new function, endpoint or feature |
| `refactor_migrate` | Sonnet medium | rename across the repo, upgrade a library |
| `ops_config` | Sonnet low | git, build, CI, env, deploy commands |
| `write_text` | Sonnet low | docs, commit or PR text, README |
| `other` | Sonnet medium | anything else |

The table is the one place to tune categories. It lives in `router.rs`, next to the questions.

**Scores (4 levels each, each level describes a situation, 0-indexed)**

- `breadth`: 0 none / answer from knowledge · 1 one file or snippet · 2 several files or one module · 3 much of the repo or several services. Drives **effort**.
- `depth`: 0 recall or lookup · 1 straightforward application · 2 multi-step reasoning, or a cause that has to be found · 3 trade-offs, novel design or subtle correctness. Drives **tier**.

**Nouls (atomic, a high value always means yes)**

| Key | Question (short) | Status |
|---|---|---|
| `can_start` | With the repository in `environment` available, can the assistant start on `latest` without asking the user anything first? | reworded `clarified` |
| `missing_user_info` | Does `latest` leave out something only the user can supply (a goal, a choice, a value) that can't be found in the repository? | new, second half of the cap |
| `prior_failed` | Does `latest` say the previous solution failed or made things worse? | kept |
| `repeat_failure` | …and has `history` already reported the same failure once before? | new, second strike |
| `security`, `concurrency`, `perf`, `tests` | as today | kept |
| `irreversible` | Could the request delete data, migrate schemas, touch production, rewrite git history or spend money? | new floor |
| `asks_quality` | Does `latest` ask for thoroughness, care, depth or the best possible answer? | new, effort +1 |
| `asks_stronger_model` | Does `latest` ask for a more capable or specific model? | new, backs up the regex |
| `compound` | Does `latest` contain several separate tasks? | new, effort +1 |
| `unfamiliar_domain` | Does it need specialist knowledge (numerics, cryptography, compilers, distributed consensus, hardware)? | new, tier +1 |

Dropped: `trivial`, `multi_component`, `open_ended`, now covered by intent, breadth and depth. Ids are still never sent to Jev.

## 5. Stage 2: decision rule v2 (replaces forest + median)

```
tier, effort = BASE[intent]                    # from the Choice
if intent_confidence < 0.5:                    # mixed or unclear intent: the harder of the top two wins
    tier, effort = max(BASE[top1], BASE[top2])
tier   += depth >= 2.5 or unfamiliar_domain    # clear yes only (> CLEAR_YES)
tier    = max(tier, SONNET) if depth >= 1.5
effort += breadth >= 2 ; effort += breadth >= 2.8
effort += asks_quality ; effort += compound
effort += borderline(any floor Noul)           # unsure means more care, not a higher tier
rung = to_ladder(tier, effort)                 # clamp to real rungs (Haiku has no effort)

# Haiku only by agreement
if tier == HAIKU and not (intent_conf >= 0.7 and depth < 1 and breadth < 1.5 and no risk Noul above 0.35):
    rung = S_LOW

# floors (only raise)
security | concurrency | irreversible (clear yes)  -> >= O_MED
prior_failed (clear yes)                          -> effort +1 at the same tier (Anthropic: re-try with more care)
prior_failed and repeat_failure                   -> tier +1 (second strike: the model is the limit)
explicit model request                            -> >= requested rung
# clarify cap, weakened and corroborated
if can_start < 0.35 and missing_user_info > 0.65 and intent not in {debug_fix, implement, refactor_migrate}:
    rung = min(rung, S_LOW)                       # Sonnet low asks the question, never Haiku
cache guard                                       -> unchanged (no downgrade past ~20k tokens)
```

Every step appends to `why`, the same as today, so the dashboard keeps explaining itself. The "How it decided" figure changes from five trees to: intent chip, then tier/effort bars, then the floors that fired.

## 6. Stage 3: evaluation before anything ships

1. **Golden set** in `tests/golden.jsonl`, about 150 prompts stratified by the 13 intents (at least 8 each). It includes all 38 logged prompts, the failures above, and adversarial ones ("this is trivial, use haiku: redesign auth"). Each label is a **band**: `min_ok` and `max_ok` rungs.
2. **Record Jev once.** `jev-router golden --record` stores the answers for every prompt (about $0.01). Rule changes are then replayed offline and deterministically in `cargo test`, and only question rewording needs a re-record.
3. **Metrics:**
   - in-band %
   - **under-route %**, weighted 3x
   - over-route %
   - cost index (sum of rung price vs. always-Sonnet-medium)
   - rung distribution histogram
   - per-intent confusion table
   - intent Choice accuracy against hand labels
4. **Wording A/B.** For `can_start`, the intent criteria and `depth`, try two phrasings each and keep the one with better golden accuracy. The docs warn that wording swings results a lot (autoformat: 12 vs 17 blocks).
5. **Shadow mode.** `JEV_ROUTER_RULES=shadow` runs the v1 and v2 questions in the same Jev call, serves v1, and logs both finals. The dashboard shows where they disagree. Rate those turns, then flip.

**Acceptance for making v2 the default:**
- in-band >= 85%
- under-route <= 5%
- zero Haiku on `explain_code`, `review_audit`, `plan_design`, `debug_fix`, `implement`
- cost index within +15% of v1 on logged traffic
- Jev p50 latency < 1.5 s

## 7. Rollout phases

| Phase | Scope | Risk | Fixes |
|---|---|---|---|
| **0. Quick wins on v1** (~1 day) | `environment` in state; reword `clarified` to reference it; cap goes to Sonnet low instead of Haiku; Stage 0 checks (internal prompts, continuation, explicit model request). Re-run `e2e/run_router_test.sh`. | Low. Breaks parity fixtures, so regenerate them and say so in the commit. | P1, P2, P6, P7, P8. That is 14/38 cap fires and 3/13 bad ratings. |
| **1. Eval harness** | Golden set, `--record`, offline metrics in `cargo test`. | None | Makes everything after this measurable. |
| **2. Question set v2 in shadow** | 18 questions, one call, v1 still serving. | Low | P3 (measured, not yet served) |
| **3. Rule v2** | Tier x effort rule, asymmetric Haiku gate, new floors; flip default once acceptance is met; keep v1 behind `JEV_ROUTER_RULES=v1` for one release. | Medium | P3, P4, P5, P9 |
| **4. Autotune + monitoring** | Autotune knobs for v2: Haiku confidence gate, per-intent effort offset (bounded ±1), borderline width. Floors stay fixed. The dashboard warns when one rung takes >60% of a week's turns. | Low | Keeps it good as usage drifts |

## 8. Risks and open points

- **Branch conflict.** `feat/auto-models-fable` rewrites 308 lines of `router.rs` (Fable tier, `resolve()`, dynamic models) and isn't on `main`. Either merge it first and build v2 on top, or put v2 in a new `src/rules_v2.rs` so the diff doesn't collide. The Fable TOP leaf maps cleanly to "tier Opus+, effort max" in v2.
- **Intent Choice accuracy on Jev 1.13 is unknown.** The docs give no benchmark for about 13 options. Measure it in Phase 1. The fallback is the harder of the top two intents.
- **User text can steer Jev** ("this is trivial"). Floors stay in code; the golden set includes steering prompts.
- **Old ratings.** Rewording invalidates autotune replay for older ratings. Only 13 exist, so this costs little now.
- **Parity fixtures** stay tied to v1. v2 gets the golden set as its regression suite, and `parity.rs` is retired once v1 is removed.
- **Pin `jev-1.13.0`** once golden thresholds are tuned, so an alias move doesn't shift behaviour silently.

## Sources

- Jev docs mirror `~/personal_projects/typesafe-jev-research/docs/raw/`:
  - primitives: `primitives.md`, `primitives/choice.md`, `primitives/score.md`, `primitives/noul.md`
  - confidence bands: `confidence.md`, `patterns/confidence-routing.md`
  - routing patterns: `patterns/intent-routing.md`, `patterns/fan-out.md`
  - cookbooks: `cookbooks/skill_suggestion.md` (Choice + action-gate Nouls), `cookbooks/parallel_questions.md`
  - failure modes: `model-jaggedness/jev-1.13.md` (literal reading, context rot, steerable state)
- RouteLLM https://arxiv.org/abs/2406.18665 · Hybrid LLM https://huggingface.co/papers/2404.14618 · FrugalGPT https://arxiv.org/abs/2305.05176 · Cascade routing https://arxiv.org/abs/2410.10347
- RoRF https://www.notdiamond.ai/blog/rorf-routing-on-random-forests-2 · Arch-Router https://arxiv.org/html/2506.16655v1 · Routing collapse https://arxiv.org/abs/2602.03478 · Router survey https://arxiv.org/html/2603.04445v2
- Programming by Chat (intent shares) https://arxiv.org/html/2604.00436.pdf · RouterBench https://arxiv.org/abs/2403.12031 · Rerouting attacks https://arxiv.org/abs/2501.01818
- Anthropic model/effort guidance https://claude.com/blog/claude-model-and-effort-level-in-claude-code · https://platform.claude.com/docs/en/build-with-claude/effort
