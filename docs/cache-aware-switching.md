# Cache-aware model and effort switching

Status: implemented 2026-09-28. Code: `cache_guard` in `src/router.rs`; per-message effort (`insert_effort_messages`) and the Opus breakpoint fix (`anchor_cache_before_system`) in `src/proxy.rs`.

## The question

In a long Claude Code session, most of the prompt (100-200k tokens) is served from the prompt cache. Cache reads cost 0.05-0.1x the input price, and cache writes cost 1.25x (5-minute TTL) or 2x (1-hour TTL). If the router switches model mid-session, is it better to switch effort instead?

## What the API does (docs checked 2026-09-28)

Sources: the prompt-caching page ("what invalidates the cache" table) and the effort page ("Change effort mid-conversation") on platform.claude.com.

| Change between requests | Tools cache | System cache | Messages cache |
|---|---|---|---|
| Model | lost | lost | lost |
| Top-level `output_config.effort` | model-specific | model-specific | **lost** |
| Effort in a `role: "system"` message (per-message effort, beta) | kept | kept | **kept** |

- **Switching effort the normal way doesn't keep the cache.** A top-level effort change always invalidates the messages cache, which is the bulk of a long session.
- **Per-message effort is the exception.** It uses beta `mid-conversation-output-config-2026-07-01` and a message shaped `{"role": "system", "content": [], "output_config": {"effort": "xhigh"}}`. It applies from the next user turn, and can sit anywhere in `messages`.
- **Which models support it:** Opus 5.5, Opus 5, Fable 5.1 and Mythos 5.1. It is **not** available on Sonnet 5 or Haiku 4.5.
- **Caches are per model.** A model switch has no cache-preserving form.
- **Opus 5.5 cache reads cost $0.20/M, the same as Sonnet 5** (0.05x of $4 vs 0.1x of $2). With a warm cache, moving from Opus to Sonnet saves nothing on context and pays for a full rewrite.

## Cost of a switch

Extra cost of a switch = cached tokens x (1.25 x new model's input price - old model's cache-read price).

| 150k cached tokens | Extra cost |
|---|---|
| Sonnet 5 -> Opus 5.5 | $0.72 |
| Opus 5.5 -> Sonnet 5 | $0.35 |
| Sonnet 5 medium -> high (top-level effort) | $0.35 |
| Opus 5.5 medium -> xhigh (per-message effort) | $0 |

The real log (`usage.jsonl`, 367 requests) shows the same pattern:

- Same-rung turn starts read about 100% from cache (e.g. Sonnet: 81,267 of 81,269 tokens).
- Rung switches rewrite everything: haiku -> sonnet wrote 50.9k, sonnet -> haiku 41.9k, haiku -> opus 44k.

## The rule (v2)

The old rule was "no downgrade past 20k context tokens". The new rule applies when the cache is **warm** and holds more than 20k tokens:

1. **Opus 5.5 effort changes freely**, in both directions. The proxy sends each change as a per-message effort system message.
2. **A request to leave Opus for Sonnet or Haiku becomes Opus medium.** It is the cheapest Opus rung, and the cache is kept.
3. **Other downgrades keep the previous rung.** For example Sonnet high -> Sonnet low, or Sonnet -> Haiku.
4. **Upgrades go ahead only when:**
   - the rebuild costs at most `UPGRADE_REBUILD_BUDGET_USD` ($0.25): Sonnet -> Opus up to about 52k cached tokens, a Sonnet effort raise up to about 109k;
   - or the conversation is leaving Haiku;
   - or a hard rule fired: a security, concurrency or irreversible floor, a failed-again floor, or a model the user named.

   Otherwise the previous rung is kept.
5. **A cold cache means free switching.** A cache counts as cold when the last request of the conversation started longer ago than its TTL. The proxy reads the TTL from Claude Code's `cache_control` (1 hour or 5 minutes).

**How much is in the cache.** The proxy uses the prompt size the API reported for the conversation's last request (input + cache read + cache write). It is passed to the router as the `cached_tokens` fact, and is 0 when the cache is cold. `context_tokens` (messages bytes / 4) is still logged, and v1 still uses it.

**How per-message effort works in the proxy:**
- The top-level effort stays at the **anchor**, the effort the cache was written with.
- Each change is stored as a (message index, effort) mark and re-inserted before the same user turn on every request. That keeps the prefix byte-identical.
- The anchor resets when the conversation leaves Opus or the cache goes cold.
- If the API answers 400 to a request carrying effort messages, the proxy drops them for the rest of the process and retries once with a top-level effort. For example, this happens when the beta isn't enabled for the account.
- `JEV_ROUTER_EFFORT_MESSAGES=off` turns the feature off.

## Tests

- `cargo test --release`:
  - `v2_cache_aware_switching` covers the rule table.
  - `opus_effort_changes_ride_in_system_messages_and_keep_the_prefix` checks the anchor, the insert position, tool-loop stability and the fallback strip.
  - `leaving_opus_drops_the_effort_anchor` and `cache_ttl_reads_the_system_blocks` cover the rest.
- `opus_breakpoint_moves_off_trailing_system_messages` covers the breakpoint fix.
- `e2e/run_cache_effort_test.sh` is a live check: three turns in one Claude Code process (medium, medium, xhigh), with per-message effort and without it. It spends a few cents.

**Live results, 2026-09-29** (combined build, `e2e/run_cache_effort_test.sh`):

| Turn | Per-message effort (read / write) | Top-level effort (read / write) |
|---|---|---|
| 1 medium | 0 / 25,203 | 21,753 / 3,427 |
| 2 medium | 25,203 / 6,259 | 25,180 / 6,239 |
| 3 -> xhigh | 31,462 / 5,193 | 31,419 / 5,175 |

- The API accepts the beta (`opus-5.5/xhigh (effort via system message) [200]`), and no fallback fired.
- Each turn reads everything the previous turn sent, in both modes. The per-turn cache miss described below is fixed.
- **The top-level effort change did not lose the cache either.** That contradicts the docs. A likely reason: Claude Code sends `per-turn-control-2026-07-01` (an older name for the per-message effort beta) on every request, which may change how top-level effort is cached. This is untested. Until it is measured, the per-message effort messages have no measured benefit in Claude Code sessions on Opus 5.5. They are harmless, and they follow the documented cache-preserving form.
- Still to check: whether a Sonnet 5 top-level effort change keeps the cache in Claude Code. If it does, rule 3 is too strict for Sonnet effort changes.

## Fixed: Opus 5.5 lost its cache at every turn behind a custom base URL

Behind any `ANTHROPIC_BASE_URL` that isn't first-party, Claude Code ends each turn's first request with `role: "system"` messages (hook output, `tool_addition`). It puts its only message breakpoint on the last of them. On Opus 5.5, a cache entry written at a trailing system message is never read on the next turn: cache diagnostics reported `messages_changed`. So every turn rewrote the whole history.

| 3 turns, turn 3 | Cache read | Cache write |
|---|---|---|
| Opus 5.5, first-party | 37,682 | 52 |
| Opus 5.5 behind a proxy, before the fix | 23,537 | 13,181 |
| Opus 5.5 behind a proxy, after the fix | 31,462 | 5,193 |

`anchor_cache_before_system` in `src/proxy.rs` moves that breakpoint onto the user message just before the trailing system messages. It runs for Opus 5.5 rungs and for a manual Opus 5.5 pick. Sonnet and Haiku are unchanged, because the proxy already folds their system messages into user text. The ~5k written per turn is the new trailing system messages, which are read on the next turn. First-party Claude Code avoids even that with `clear_at` reminders.

## Not done

- **Per-model TTL pricing:** the rebuild cost assumes the 5-minute write price (1.25x). With the 1-hour TTL, a real rewrite costs 2x, so the $0.25 budget is generous there.
- **Pre-warming a cache before an upgrade** (`max_tokens: 0`): this still pays the write, so it doesn't help a switch.
- **Autotune of `UPGRADE_REBUILD_BUDGET_USD`:** not done yet.
