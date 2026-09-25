#!/usr/bin/env bash
# Live end-to-end test of `jev-router claude` on the toy project. Spends real (small) money:
# every scenario is capped with --max-budget-usd. Run from anywhere:
#   e2e/run_router_test.sh
set -u
here="$(cd "$(dirname "$0")" && pwd)"
router="${JEV_ROUTER:-jev-router}"  # the installed (Rust) build; JEV_ROUTER=target/release/jev-router tests a local build
data="$HOME/.local/share/claude-router"
out_dir="$(mktemp -d)"
cd "$here/toy_shop" || exit 1
cp "$here/fixtures/cart.py" cart.py && rm -f inventory.py test_inventory.py  # reset the toy project

run() {  # run <name> <prompt> [extra claude args...]
  local name="$1" prompt="$2"; shift 2
  local before; before=$(wc -l < "$data/proxy.log" 2>/dev/null || echo 0)
  echo "=== $name"
  "$router" claude -p "$prompt" --output-format json --max-budget-usd 0.50 \
    --allowedTools "Read" "Edit" "Write" "Bash(python3 -m unittest*)" "$@" > "$out_dir/$name.json" 2>/dev/null
  python3 - "$out_dir/$name.json" <<'EOF'
import json, sys
try:
    out = json.load(open(sys.argv[1]))
except ValueError:
    sys.exit("  no JSON result (run failed)")
served = {m: round(u.get("costUSD", 0), 4) for m, u in (out.get("modelUsage") or {}).items()}
print(f"  result: {str(out.get('result', '')).strip()[:160]!r}")
print(f"  error: {out.get('is_error')}  turns: {out.get('num_turns')}  cost: ${out.get('total_cost_usd', 0):.4f}")
print(f"  served by (model: $): {served}")
EOF
  echo "  proxy.log:"; tail -n +"$((before + 1))" "$data/proxy.log" | sed 's/^/    /'
}

run trivial "In one sentence, what is a Python decorator? Do not use any tools."
run bugfix "python3 -m unittest fails in this folder. Find the bug in cart.py, fix it, and re-run the tests to confirm they pass. Reply with one line."
run harder "Add a thread-safe Inventory class in inventory.py: reserve(name, qty) must never oversell when many threads call it concurrently (use a lock), plus unittest tests that hammer it from 20 threads. Run the tests and reply with one line."
run manual "Reply with just the word ok" --model claude-haiku-4-5

echo "=== tests after the runs"
python3 -m unittest 2>&1 | tail -1
echo "=== decisions"
"$router" log -n 8
