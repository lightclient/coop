#!/usr/bin/env bash
# Pre-flight check: is coop running with signal enabled and tracing on?
# Exits 0 if ready, 1 if not. Prints diagnostics to stderr.
#
# Usage: ./scripts/preflight.sh [traces.jsonl]
set -euo pipefail

TRACE_FILE="${1:-traces.jsonl}"
FAIL=0

check() {
    local label="$1" ok="$2"
    if [ "$ok" = "1" ]; then
        echo "  ✅ $label" >&2
    else
        echo "  ❌ $label" >&2
        FAIL=1
    fi
}

echo "Pre-flight checks:" >&2

# 1. Trace file exists and has recent content
if [ -f "$TRACE_FILE" ]; then
    age=$(python3 -c "
import os, time
mtime = os.path.getmtime('$TRACE_FILE')
print(int(time.time() - mtime))
" 2>/dev/null || echo "9999")
    check "Trace file exists ($TRACE_FILE, modified ${age}s ago)" "1"
    if [ "$age" -gt 300 ]; then
        echo "    ⚠️  Trace file is >5 min old — coop may not be running" >&2
    fi
else
    check "Trace file exists ($TRACE_FILE)" "0"
fi

# 2. Coop process is running
if pgrep -f "coop.*start" >/dev/null 2>&1; then
    check "Coop process running" "1"
else
    check "Coop process running (pgrep -f 'coop.*start')" "0"
fi

# 3. Signal channel configured (in traces)
if [ -f "$TRACE_FILE" ] && grep -q 'signal channel configured\|signal loop listening' "$TRACE_FILE" 2>/dev/null; then
    check "Signal channel active (in traces)" "1"
else
    check "Signal channel active (in traces)" "0"
fi

# 4. No recent ERROR in traces (last 50 lines)
if [ -f "$TRACE_FILE" ]; then
    recent_errors=$(tail -50 "$TRACE_FILE" | grep -c '"level":"ERROR"' || true)
    recent_errors="${recent_errors//[^0-9]/}"
    recent_errors="${recent_errors:-0}"
    if [ "$recent_errors" = "0" ]; then
        check "No recent errors in traces" "1"
    else
        check "No recent errors in traces ($recent_errors found)" "0"
        tail -50 "$TRACE_FILE" | grep '"level":"ERROR"' | tail -3 | while read -r line; do
            msg=$(echo "$line" | python3 -c "import sys,json; print(json.loads(sys.stdin.readline()).get('fields',{}).get('message','?'))" 2>/dev/null || echo "?")
            echo "    → $msg" >&2
        done
    fi
fi

# 5. signal-cli has 2 accounts
ACCOUNT_COUNT=$(signal-cli -o json listAccounts 2>/dev/null | python3 -c "import sys,json; print(len(json.load(sys.stdin)))" 2>/dev/null || echo "0")
if [ "$ACCOUNT_COUNT" -ge 2 ]; then
    check "signal-cli has $ACCOUNT_COUNT accounts" "1"
else
    check "signal-cli has 2+ accounts (found $ACCOUNT_COUNT)" "0"
fi

# 6. ANTHROPIC_API_KEY is set
if [ -n "${ANTHROPIC_API_KEY:-}" ]; then
    check "ANTHROPIC_API_KEY set" "1"
else
    check "ANTHROPIC_API_KEY set" "0"
fi

echo "" >&2
if [ "$FAIL" = "0" ]; then
    echo "Ready for e2e testing." >&2
else
    echo "Fix the issues above before running e2e tests." >&2
fi

exit "$FAIL"
