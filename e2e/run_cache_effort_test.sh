#!/usr/bin/env bash
# Live check that an Opus 5.5 effort change keeps the prompt cache when it rides in a
# per-message effort system message, and loses it as a top-level change. Three user turns in
# one Claude Code process (proxy state is in memory): medium, medium, then xhigh. Turn 3 is
# the one to read: Claude Code's tool list can still change between turns 1 and 2 (MCP
# servers finish connecting), which rebuilds the cache whatever the effort does.
# Jev is skipped (JEV_ROUTER_FORCE_RUNG), so no TypeSafe key is needed.
# Spends real (small) money, capped by --max-budget-usd. Run from anywhere:
#   e2e/run_cache_effort_test.sh
set -u
here="$(cd "$(dirname "$0")" && pwd)"
router="${JEV_ROUTER:-$here/../target/release/jev-router}"
data="$HOME/.local/share/claude-router"
work="$(mktemp -d)"
cd "$work" || exit 1

session() {  # session <name> [env...]
  local name="$1"; shift
  local before; before=$(wc -l < "$data/usage.jsonl" 2>/dev/null || echo 0)
  local log_before; log_before=$(wc -l < "$data/proxy.log" 2>/dev/null || echo 0)
  echo "=== $name"
  # One message per turn: wait for each result, or Claude Code merges queued messages.
  env TYPESAFE_API_KEY="${TYPESAFE_API_KEY:-unused}" \
    JEV_ROUTER_FORCE_RUNG="opus-5.5/medium,opus-5.5/medium,opus-5.5/xhigh" "$@" \
    python3 -c '
import json, subprocess, sys
claude = subprocess.Popen(
    [sys.argv[1], "claude", "-p", "--input-format", "stream-json", "--output-format",
     "stream-json", "--verbose", "--max-budget-usd", "0.50"],
    stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, text=True)
for word in ["one", "two", "three"]:
    text = f"Reply with just the word {word}. Do not use any tools."
    claude.stdin.write(json.dumps({"type": "user", "message": {"role": "user", "content": text}}) + "\n")
    claude.stdin.flush()
    for line in claude.stdout:
        if json.loads(line).get("type") == "result":
            break
claude.stdin.close()
claude.wait()
' "$router"
  tail -n +"$((log_before + 1))" "$data/proxy.log" | grep -E "routed|pinned|effort|upstream" | sed 's/^/  log: /'
  tail -n +"$((before + 1))" "$data/usage.jsonl" | python3 -c '
import json, sys
for line in sys.stdin:
    r = json.loads(line)
    if r.get("kind") == "aux":
        continue
    u = r["usage"]
    read, write = u.get("cache_read_input_tokens", 0), u.get("cache_creation_input_tokens", 0)
    print("  %-16s cache_read=%6d cache_write=%6d" % (r["rung"], read, write))
'
}

session "per-message effort (default)"
session "top-level effort (JEV_ROUTER_EFFORT_MESSAGES=off)" JEV_ROUTER_EFFORT_MESSAGES=off
echo "Expect on turn 3 (xhigh): per-message effort reads nearly all of turn 2 and writes a little;"
echo "top-level effort reads only tools + system and rewrites the messages."
