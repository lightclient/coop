#!/usr/bin/env bash
# Send a Signal message to coop and verify it was processed successfully.
# Polls traces for completion instead of fixed sleep.
# Returns 0 (PASS) or 1 (FAIL) with diagnostic output.
#
# Usage:
#   ./scripts/send-and-verify.sh "What is 2+2?"
#   ./scripts/send-and-verify.sh "/status"
#   ./scripts/send-and-verify.sh "Read SOUL.md" --timeout 60
#   ./scripts/send-and-verify.sh "Run 'uname -a'" --expect-tool bash --timeout 60
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"

# --- Parse args ---
MESSAGE=""
TIMEOUT=30
SENDER_CMD=""
TARGET=""
TRACE_FILE="traces.jsonl"
EXPECT_TOOL=""
EXPECT_COMMAND=0
CHECK_REPLY=1

while [ $# -gt 0 ]; do
    case "$1" in
        --timeout) TIMEOUT="$2"; shift 2 ;;
        --sender-cmd) SENDER_CMD="$2"; shift 2 ;;
        --target) TARGET="$2"; shift 2 ;;
        --trace-file) TRACE_FILE="$2"; shift 2 ;;
        --expect-tool) EXPECT_TOOL="$2"; shift 2 ;;
        --expect-command) EXPECT_COMMAND=1; shift ;;
        --no-reply-check) CHECK_REPLY=0; shift ;;
        -*) echo "Unknown option: $1" >&2; exit 2 ;;
        *) MESSAGE="$1"; shift ;;
    esac
done

if [ -z "$MESSAGE" ]; then
    echo "Usage: $0 \"message\" [options]" >&2
    exit 2
fi

# --- Auto-detect sender if not specified ---
if [ -z "$SENDER_CMD" ] || [ -z "$TARGET" ]; then
    config=$(bash "$SCRIPT_DIR/discover-accounts.sh" 2>/dev/null) || {
        echo "FAIL: Could not discover accounts. Run discover-accounts.sh manually." >&2
        exit 1
    }
    eval "$config"
    TARGET="${COOP_NUMBER:-}"
fi

if [ -z "$SENDER_CMD" ] || [ -z "$TARGET" ]; then
    echo "FAIL: No sender or target configured." >&2
    exit 1
fi

# --- Ensure coop.toml has the sender's UUID (swap placeholder, restore on exit) ---
COOP_TOML="coop.toml"
PATCHED_TOML=0
if [ -n "${SENDER_UUID:-}" ] && [ -f "$COOP_TOML" ]; then
    if grep -q 'signal:alice-uuid' "$COOP_TOML"; then
        sed -i "s/signal:alice-uuid/signal:$SENDER_UUID/" "$COOP_TOML"
        PATCHED_TOML=1
        echo "Patched coop.toml with sender UUID (will restore)." >&2
    fi
fi

restore_toml() {
    if [ "$PATCHED_TOML" = "1" ] && [ -n "${SENDER_UUID:-}" ]; then
        sed -i "s/signal:$SENDER_UUID/signal:alice-uuid/" "$COOP_TOML"
    fi
}
trap restore_toml EXIT

# --- Record trace position before sending ---
if [ -f "$TRACE_FILE" ]; then
    TRACE_LINES_BEFORE=$(wc -l < "$TRACE_FILE")
else
    TRACE_LINES_BEFORE=0
fi

# --- Send the message ---
echo "Sending: \"$MESSAGE\"" >&2
IFS=' ' read -r -a CMD_PARTS <<< "$SENDER_CMD"
SEND_RESULT=$("${CMD_PARTS[@]}" send -m "$MESSAGE" "$TARGET" 2>&1) || true

if echo "$SEND_RESULT" | grep -q '"type":"SUCCESS"'; then
    echo "Message sent successfully." >&2
else
    echo "FAIL: signal-cli send failed:" >&2
    echo "$SEND_RESULT" >&2
    exit 1
fi

# --- Helper: grep only new trace lines (after our send) ---
# Operates directly on the file to avoid bash variable size issues.
new_grep() {
    if [ -f "$TRACE_FILE" ]; then
        tail -n +"$((TRACE_LINES_BEFORE + 1))" "$TRACE_FILE" | grep "$@"
    else
        return 1
    fi
}

is_command() {
    echo "$MESSAGE" | grep -q '^/'
}

# --- Poll traces for completion ---
echo "Waiting for coop to process (timeout: ${TIMEOUT}s)..." >&2

ELAPSED=0
POLL_INTERVAL=2
COMPLETED=0

while [ "$ELAPSED" -lt "$TIMEOUT" ]; do
    sleep "$POLL_INTERVAL"
    ELAPSED=$((ELAPSED + POLL_INTERVAL))

    if is_command; then
        if new_grep -q 'channel slash command handled' 2>/dev/null; then
            COMPLETED=1; break
        fi
    else
        # Look for actual text reply (not receipts)
        if new_grep 'signal_action_send' 2>/dev/null | grep -q 'send_text\|send_reaction'; then
            COMPLETED=1; break
        fi
        if new_grep -q '"agent turn complete\|"agent_turn.*CLOSE"' 2>/dev/null; then
            COMPLETED=1; break
        fi
    fi

    printf "." >&2
done
echo "" >&2

# Let final trace lines flush
sleep 1

# --- Analyze results ---
PASS=1

report() {
    local label="$1" ok="$2"
    if [ "$ok" = "1" ]; then
        echo "  ✅ $label" >&2
    else
        echo "  ❌ $label" >&2
        PASS=0
    fi
}

echo "" >&2
echo "Results:" >&2

# Check message received
if new_grep -qE 'signal_receive_event|signal inbound' 2>/dev/null; then
    report "Message received by coop" "1"
else
    report "Message received by coop" "0"
fi

# Check dispatch
if new_grep -qE 'signal inbound dispatched|channel slash command' 2>/dev/null; then
    report "Message dispatched" "1"
else
    report "Message dispatched" "0"
fi

if is_command || [ "$EXPECT_COMMAND" = "1" ]; then
    if new_grep -q 'channel slash command handled' 2>/dev/null; then
        report "Slash command handled" "1"
    else
        report "Slash command handled" "0"
    fi
    if new_grep -q 'agent_turn' 2>/dev/null; then
        report "No agent_turn for command" "0"
    else
        report "No agent_turn for command" "1"
    fi
else
    if new_grep -q 'agent_turn' 2>/dev/null; then
        report "Agent turn started" "1"
    else
        report "Agent turn started" "0"
    fi
    if new_grep 'signal_action_send' 2>/dev/null | grep -q 'send_text\|send_reaction'; then
        report "Reply sent via Signal" "1"
    else
        report "Reply sent via Signal" "0"
    fi
fi

# Check specific tool if requested
if [ -n "$EXPECT_TOOL" ]; then
    if new_grep 'tool_execute' 2>/dev/null | grep -q "$EXPECT_TOOL"; then
        report "Tool '$EXPECT_TOOL' executed" "1"
    else
        report "Tool '$EXPECT_TOOL' executed" "0"
    fi
fi

# Check no errors
ERROR_COUNT=$(new_grep -c '"level":"ERROR"' 2>/dev/null || true)
ERROR_COUNT="${ERROR_COUNT//[^0-9]/}"
ERROR_COUNT="${ERROR_COUNT:-0}"
if [ "$ERROR_COUNT" = "0" ]; then
    report "No errors" "1"
else
    report "No errors ($ERROR_COUNT found)" "0"
fi

if [ "$COMPLETED" = "0" ]; then
    report "Completed within timeout (${TIMEOUT}s)" "0"
fi

# --- Check reply received by Alice ---
if [ "$CHECK_REPLY" = "1" ] && [ "$PASS" = "1" ]; then
    echo "" >&2
    echo "Checking if Alice received the reply..." >&2
    RECV_ACCOUNT=$(echo "$SENDER_CMD" | grep -oP '(?<=-a )\S+' || echo "")
    if [ -n "$RECV_ACCOUNT" ]; then
        RECV_RESULT=$(signal-cli -a "$RECV_ACCOUNT" -o json receive --timeout 10 2>&1 || true)
        if echo "$RECV_RESULT" | grep -q 'dataMessage\|syncMessage'; then
            report "Alice received reply" "1"
        else
            echo "  ⚠️  Could not confirm Alice received reply (signal-cli receive timeout)" >&2
        fi
    fi
fi

# --- Final verdict ---
echo "" >&2
if [ "$PASS" = "1" ] && [ "$COMPLETED" = "1" ]; then
    echo "PASS ✅" >&2
    exit 0
else
    echo "FAIL ❌" >&2
    echo "" >&2
    echo "Recent trace tail:" >&2
    tail -n +"$((TRACE_LINES_BEFORE + 1))" "$TRACE_FILE" 2>/dev/null | tail -10 | while read -r line; do
        echo "$line" | python3 -c "
import sys, json
d = json.loads(sys.stdin.readline())
ts = d.get('timestamp','?')[-12:]
lvl = d.get('level','?')
msg = d.get('fields',{}).get('message', '...')
print(f'  {ts} [{lvl}] {msg}')
" 2>/dev/null || echo "  (unparseable)" >&2
    done
    exit 1
fi
