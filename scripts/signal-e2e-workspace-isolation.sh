#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
SKILL_DIR="$ROOT/.claude/skills/signal-e2e-test"
TRACE_FILE="${TRACE_FILE:-$ROOT/traces.jsonl}"
TMUX_SOCKET="${TMUX_SOCKET:-/tmp/claude-tmux-sockets/claude-coop-matrix.sock}"
TMUX_SESSION="${TMUX_SESSION:-claude-coop-matrix}"
START_SCRIPT="${START_SCRIPT:-/tmp/coop-signal-matrix-start.sh}"
COOP_TOML="$ROOT/coop.toml"
ORIGINAL_COOP_TOML="$(mktemp)"
GROUP_NAME="${GROUP_NAME:-Coop E2E Test Group}"
KNOWN_PRESAGE_ERROR='could not create sync message from a (direct|group) message'

cleanup() {
    cp "$ORIGINAL_COOP_TOML" "$COOP_TOML" 2>/dev/null || true
    tmux -f /dev/null -S "$TMUX_SOCKET" kill-session -t "$TMUX_SESSION" 2>/dev/null || true
    tmux -f /dev/null -S "$TMUX_SOCKET" kill-server 2>/dev/null || true
    rm -f "$START_SCRIPT"
}
trap cleanup EXIT

fail() {
    echo "FAIL: $*" >&2
    exit 1
}

pass() {
    echo "PASS: $*" >&2
}

note() {
    echo "==> $*" >&2
}

require_file() {
    [ -f "$1" ] || fail "missing required file: $1"
}

require_file "$SKILL_DIR/scripts/discover-accounts.sh"
require_file "$SKILL_DIR/scripts/preflight.sh"
require_file "$COOP_TOML"
cp "$COOP_TOML" "$ORIGINAL_COOP_TOML"
mkdir -p /tmp/claude-tmux-sockets

ANTHROPIC_API_KEY="$(python3 -c 'import json,pathlib; print(json.loads(pathlib.Path("/root/.pi/agent/auth.json").read_text())["anthropic"]["access"])')"
export ANTHROPIC_API_KEY

# shellcheck disable=SC1090
source <(bash "$SKILL_DIR/scripts/discover-accounts.sh" 2>/dev/null)
SENDER_ARGS=(signal-cli -a "$SENDER_NUMBER" -o json)

b64_to_hex() {
    python3 - "$1" <<'PY'
import base64, sys
print(base64.b64decode(sys.argv[1]).hex())
PY
}

find_or_create_group() {
    local existing
    existing="$(signal-cli -a "$SENDER_NUMBER" -o json listGroups 2>/dev/null | python3 -c '
import json, sys
name = sys.argv[1]
groups = json.load(sys.stdin)
for group in groups:
    if group.get("name") == name:
        print(group["id"])
        break
' "$GROUP_NAME")"
    if [ -n "$existing" ]; then
        echo "$existing"
        return
    fi

    note "creating signal group '$GROUP_NAME'"
    signal-cli -a "$SENDER_NUMBER" -o json updateGroup -n "$GROUP_NAME" -m "$COOP_NUMBER" >/dev/null
    sleep 5
    signal-cli -a "$SENDER_NUMBER" -o json listGroups 2>/dev/null | python3 -c '
import json, sys
name = sys.argv[1]
groups = json.load(sys.stdin)
for group in groups:
    if group.get("name") == name:
        print(group["id"])
        break
else:
    raise SystemExit(1)
' "$GROUP_NAME"
}

GROUP_B64="$(find_or_create_group)"
GROUP_HEX="$(b64_to_hex "$GROUP_B64")"

write_config() {
    local trust="$1"
    local include_group="$2"
    cat > "$COOP_TOML" <<EOF
[agent]
id = "coop"
model = "anthropic/claude-opus-4-5-20251101"
workspace = "./workspaces/default"

[[users]]
match = ["terminal:default", "signal:$SENDER_UUID"]
name = "alice"
trust = "$trust"
EOF

    if [ "$include_group" = "yes" ]; then
        cat >> "$COOP_TOML" <<'EOF'

[[groups]]
match = ["*"]
trigger = "always"
default_trust = "familiar"
EOF
    fi

    cat >> "$COOP_TOML" <<'EOF'

[channels.signal]
db_path = "./db/signal.db"

[provider]
name = "anthropic"

[memory]
db_path = "./db/memory.db"
EOF
}

write_start_script() {
    cat > "$START_SCRIPT" <<EOF
#!/usr/bin/env bash
set -euo pipefail
cd "$ROOT"
export ANTHROPIC_API_KEY="$ANTHROPIC_API_KEY"
export COOP_TRACE_FILE="$TRACE_FILE"
exec cargo run --features signal --bin coop -- start
EOF
    chmod +x "$START_SCRIPT"
}

restart_coop() {
    note "restarting coop"
    pkill -f 'target/debug/coop start' 2>/dev/null || true
    tmux -f /dev/null -S "$TMUX_SOCKET" kill-server 2>/dev/null || true
    : > "$TRACE_FILE"
    write_start_script
    tmux -f /dev/null -S "$TMUX_SOCKET" new-session -d -s "$TMUX_SESSION" -n shell "$START_SCRIPT"

    local elapsed=0
    while [ "$elapsed" -lt 40 ]; do
        sleep 2
        elapsed=$((elapsed + 2))
        if pgrep -f 'target/debug/coop start' >/dev/null 2>&1 \
            && rg -q 'signal loop listening|signal websocket connected, receiving messages' "$TRACE_FILE" 2>/dev/null; then
            return 0
        fi
    done

    tmux -f /dev/null -S "$TMUX_SOCKET" capture-pane -p -J -t "$TMUX_SESSION":0.0 -S -200 >&2 || true
    fail "coop did not become ready"
}

trace_start() {
    if [ -f "$TRACE_FILE" ]; then
        wc -l < "$TRACE_FILE"
    else
        echo 0
    fi
}

slice_trace() {
    local start_lines="$1"
    local out="$2"
    if [ -f "$TRACE_FILE" ]; then
        tail -n "+$((start_lines + 1))" "$TRACE_FILE" > "$out"
    else
        : > "$out"
    fi
}

wait_for_slice() {
    local start_lines="$1"
    local pattern="$2"
    local timeout="$3"
    local elapsed=0
    local poll=2
    local tmp
    tmp="$(mktemp)"
    while [ "$elapsed" -lt "$timeout" ]; do
        sleep "$poll"
        elapsed=$((elapsed + poll))
        slice_trace "$start_lines" "$tmp"
        if rg -q "$pattern" "$tmp"; then
            rm -f "$tmp"
            return 0
        fi
    done
    echo "timeout waiting for pattern: $pattern" >&2
    echo "--- trace slice tail ---" >&2
    tail -n 40 "$tmp" >&2 || true
    rm -f "$tmp"
    return 1
}

assert_contains() {
    local file="$1"
    local pattern="$2"
    local description="$3"
    rg -q "$pattern" "$file" || fail "$description"
}

assert_no_unexpected_errors() {
    local file="$1"
    local unexpected
    unexpected="$(grep '"level":"ERROR"' "$file" | grep -Ev "$KNOWN_PRESAGE_ERROR" || true)"
    [ -z "$unexpected" ] || {
        echo "$unexpected" >&2
        fail "unexpected errors found in trace slice"
    }
}

drain_sender_inbox() {
    signal-cli -a "$SENDER_NUMBER" -o json receive --timeout 1 --max-messages 100 >/dev/null 2>&1 || true
}

receive_to_file() {
    local out="$1"
    signal-cli -a "$SENDER_NUMBER" -o json receive --timeout 10 --max-messages 100 > "$out" 2>&1 || true
}

assert_received_text() {
    local file="$1"
    local regex="$2"
    python3 - "$file" "$regex" <<'PY'
import json, re, sys
path, pattern = sys.argv[1], sys.argv[2]
rx = re.compile(pattern, re.I)
for raw in open(path, 'r', encoding='utf-8', errors='ignore'):
    raw = raw.strip()
    if not raw:
        continue
    try:
        data = json.loads(raw)
    except Exception:
        continue
    env = data.get('envelope') or {}
    dm = env.get('dataMessage') or {}
    msg = dm.get('message')
    if isinstance(msg, str) and rx.search(msg):
        raise SystemExit(0)
    sync = env.get('syncMessage') or {}
    sent = sync.get('sentMessage') or sync.get('sent') or {}
    msg = ((sent.get('message') or {}) if isinstance(sent.get('message'), dict) else {}).get('message')
    if isinstance(msg, str) and rx.search(msg):
        raise SystemExit(0)
raise SystemExit(1)
PY
}

assert_received_attachment() {
    local file="$1"
    python3 - "$file" <<'PY'
import json, sys
for raw in open(sys.argv[1], 'r', encoding='utf-8', errors='ignore'):
    raw = raw.strip()
    if not raw:
        continue
    try:
        data = json.loads(raw)
    except Exception:
        continue
    env = data.get('envelope') or {}
    dm = env.get('dataMessage') or {}
    attachments = dm.get('attachments') or []
    if attachments:
        raise SystemExit(0)
    sync = env.get('syncMessage') or {}
    for key in ('sentMessage', 'sent'):
        sent = sync.get(key) or {}
        message = sent.get('message') or {}
        attachments = message.get('attachments') or []
        if attachments:
            raise SystemExit(0)
raise SystemExit(1)
PY
}

send_direct_text() {
    local message="$1"
    local result
    result="$(${SENDER_ARGS[@]} send -m "$message" "$COOP_NUMBER" 2>&1 || true)"
    echo "$result" | grep -q '"type":"SUCCESS"' || fail "signal-cli direct send failed: $result"
}

send_direct_attachment() {
    local message="$1"
    local attachment="$2"
    local result
    result="$(${SENDER_ARGS[@]} send -m "$message" -a "$attachment" -- "$COOP_NUMBER" 2>&1 || true)"
    echo "$result" | grep -q '"type":"SUCCESS"' || fail "signal-cli direct attachment send failed: $result"
}

send_group_text() {
    local message="$1"
    local result
    result="$(${SENDER_ARGS[@]} send -g "$GROUP_B64" -m "$message" 2>&1 || true)"
    echo "$result" | grep -q '"type":"SUCCESS"' || fail "signal-cli group send failed: $result"
}

send_group_attachment() {
    local message="$1"
    local attachment="$2"
    local result
    result="$(${SENDER_ARGS[@]} send -g "$GROUP_B64" -m "$message" -a "$attachment" 2>&1 || true)"
    echo "$result" | grep -q '"type":"SUCCESS"' || fail "signal-cli group attachment send failed: $result"
}

run_command_scenario() {
    local name="$1"
    local sender="$2"
    local message="$3"
    local start slice recv
    note "$name"
    drain_sender_inbox
    start="$(trace_start)"
    "$sender" "$message"
    wait_for_slice "$start" 'channel slash command handled' 60 || fail "$name did not complete"
    slice="$(mktemp)"
    recv="$(mktemp)"
    slice_trace "$start" "$slice"
    assert_contains "$slice" 'channel slash command handled' "$name missing slash command handling"
    assert_no_unexpected_errors "$slice"
    receive_to_file "$recv"
    rm -f "$slice" "$recv"
    pass "$name"
}

run_text_scenario() {
    local name="$1"
    local sender="$2"
    local message="$3"
    local expect_text_regex="$4"
    shift 4
    local start slice recv
    note "$name"
    drain_sender_inbox
    start="$(trace_start)"
    "$sender" "$message"
    wait_for_slice "$start" 'turn complete|signal.action":"send_text"|signal.action":"send_attachment"' 120 || fail "$name did not complete"
    sleep 2
    slice="$(mktemp)"
    recv="$(mktemp)"
    slice_trace "$start" "$slice"
    assert_contains "$slice" 'route_message' "$name missing route_message"
    assert_contains "$slice" 'agent_turn' "$name missing agent_turn"
    assert_contains "$slice" 'signal.action":"send_text"|signal.action":"send_attachment"' "$name missing reply send"
    assert_no_unexpected_errors "$slice"
    while [ "$#" -gt 0 ]; do
        local kind="$1"; shift
        local pattern="$1"; shift
        local desc="$1"; shift
        case "$kind" in
            contains) assert_contains "$slice" "$pattern" "$desc" ;;
            not_contains) if rg -q "$pattern" "$slice"; then fail "$desc"; fi ;;
            *) fail "unknown assertion kind: $kind" ;;
        esac
    done
    if [ -n "$expect_text_regex" ]; then
        receive_to_file "$recv"
        assert_received_text "$recv" "$expect_text_regex" || fail "$name missing expected receiver text '$expect_text_regex'"
    fi
    rm -f "$slice" "$recv"
    pass "$name"
}

run_attachment_send_scenario() {
    local name="$1"
    local sender="$2"
    local message="$3"
    local attachment="$4"
    local expect_text_regex="$5"
    shift 5
    local start slice recv
    note "$name"
    drain_sender_inbox
    start="$(trace_start)"
    "$sender" "$message" "$attachment"
    wait_for_slice "$start" 'turn complete|signal.action":"send_text"|signal.action":"send_attachment"' 120 || fail "$name did not complete"
    sleep 2
    slice="$(mktemp)"
    recv="$(mktemp)"
    slice_trace "$start" "$slice"
    assert_no_unexpected_errors "$slice"
    while [ "$#" -gt 0 ]; do
        local kind="$1"; shift
        local pattern="$1"; shift
        local desc="$1"; shift
        case "$kind" in
            contains) assert_contains "$slice" "$pattern" "$desc" ;;
            not_contains) if rg -q "$pattern" "$slice"; then fail "$desc"; fi ;;
            *) fail "unknown assertion kind: $kind" ;;
        esac
    done
    if [ -n "$expect_text_regex" ]; then
        receive_to_file "$recv"
        assert_received_text "$recv" "$expect_text_regex" || fail "$name missing expected receiver text '$expect_text_regex'"
    fi
    rm -f "$slice" "$recv"
    pass "$name"
}

run_send_image_scenario() {
    local name="$1"
    local sender="$2"
    local message="$3"
    local attachment="$4"
    local confirm_text_regex="$5"
    shift 5
    local start slice recv
    note "$name"
    drain_sender_inbox
    start="$(trace_start)"
    "$sender" "$message" "$attachment"
    wait_for_slice "$start" 'signal.action":"send_attachment"' 120 || fail "$name missing send_attachment"
    sleep 2
    slice="$(mktemp)"
    recv="$(mktemp)"
    slice_trace "$start" "$slice"
    assert_contains "$slice" 'tool_execute' "$name missing tool execution"
    assert_contains "$slice" 'signal_send_image' "$name missing signal_send_image tool"
    assert_contains "$slice" 'signal.action":"send_attachment"' "$name missing send_attachment action"
    assert_no_unexpected_errors "$slice"
    while [ "$#" -gt 0 ]; do
        local kind="$1"; shift
        local pattern="$1"; shift
        local desc="$1"; shift
        case "$kind" in
            contains) assert_contains "$slice" "$pattern" "$desc" ;;
            not_contains) if rg -q "$pattern" "$slice"; then fail "$desc"; fi ;;
            *) fail "unknown assertion kind: $kind" ;;
        esac
    done
    receive_to_file "$recv"
    assert_received_attachment "$recv" || fail "$name missing received attachment"
    assert_received_text "$recv" "$confirm_text_regex" || fail "$name missing confirmation text '$confirm_text_regex'"
    rm -f "$slice" "$recv"
    pass "$name"
}

make_text_file() {
    local path="$1"
    local first_line="$2"
    printf '%s\nsecond line\n' "$first_line" > "$path"
}

make_red_png() {
    local path="$1"
    python3 - "$path" <<'PY'
import struct, zlib, sys
path = sys.argv[1]
width = height = 1
raw = b'\x00\xff\x00\x00'
compressed = zlib.compress(raw)

def chunk(tag, data):
    return struct.pack('>I', len(data)) + tag + data + struct.pack('>I', zlib.crc32(tag + data) & 0xffffffff)

png = b'\x89PNG\r\n\x1a\n'
png += chunk(b'IHDR', struct.pack('>IIBBBBB', width, height, 8, 2, 0, 0, 0))
png += chunk(b'IDAT', compressed)
png += chunk(b'IEND', b'')
open(path, 'wb').write(png)
PY
}

DM_TEXT_FILE="/tmp/coop-dm-attachment.txt"
GROUP_TEXT_FILE="/tmp/coop-group-attachment.txt"
RED_PNG="/tmp/coop-red.png"
make_text_file "$DM_TEXT_FILE" 'ATTACH_DM_OK'
make_text_file "$GROUP_TEXT_FILE" 'ATTACH_GROUP_OK'
make_red_png "$RED_PNG"
mkdir -p "$ROOT/workspaces/default/users/alice" "$ROOT/workspaces/default/users/bob"
printf 'INNER_OK\n' > "$ROOT/workspaces/default/users/alice/ok.txt"
printf 'BOB_SECRET\n' > "$ROOT/workspaces/default/users/bob/secret.txt"

note "phase 1: full-trust direct + group tests"
write_config full yes
restart_coop

run_command_scenario 'dm /status' send_direct_text '/status'
run_command_scenario 'dm /new' send_direct_text '/new'
run_text_scenario \
  'dm write/edit/read global scope' \
  send_direct_text \
  'Use write_file to write ./e2e-global.txt with exactly GLOBAL_E2E_ONE. Then use edit_file to change it to exactly GLOBAL_E2E_TWO. Then use read_file and reply with only GLOBAL_E2E_TWO.' \
  'GLOBAL_E2E_TWO' \
  contains 'tool_execute' 'global scope scenario missing tool execution' \
  contains 'write_file' 'global scope scenario missing write_file' \
  contains 'edit_file' 'global scope scenario missing edit_file' \
  contains 'read_file' 'global scope scenario missing read_file' \
  contains 'principal":"Global"' 'global scope scenario missing Global principal' \
  contains 'scoped_root":"\./"' 'global scope scenario missing global scoped_root'

run_attachment_send_scenario \
  'dm attachment save + read' \
  send_direct_attachment \
  'Read the file I just attached using read_file and reply with only its first line.' \
  "$DM_TEXT_FILE" \
  'ATTACH_DM_OK' \
  contains 'saved signal attachment' 'dm attachment scenario missing saved attachment trace' \
  contains 'relative_path":"\./attachments/' 'dm attachment scenario missing relative attachment path' \
  contains 'read_file' 'dm attachment scenario missing read_file tool'

run_attachment_send_scenario \
  'dm image auto-injection' \
  send_direct_attachment \
  'What color is the attached image? Reply with one word.' \
  "$RED_PNG" \
  'red' \
  contains 'saved signal attachment' 'dm image scenario missing saved attachment trace' \
  contains 'injecting image into message' 'dm image scenario missing image injection trace' \
  not_contains 'workspace scope denied access' 'dm image scenario unexpectedly denied image scope'

run_send_image_scenario \
  'dm signal_send_image' \
  send_direct_attachment \
  'Use the signal_send_image tool to send back the attached image, then reply with only SENT_DM_IMAGE.' \
  "$RED_PNG" \
  'SENT_DM_IMAGE' \
  contains 'saved signal attachment' 'dm signal_send_image missing saved attachment trace' \
  contains 'relative_path":"\./attachments/' 'dm signal_send_image missing relative attachment path'

run_text_scenario \
  'group basic reply' \
  send_group_text \
  'What is 2+2? Reply with only 4.' \
  '^4$' \
  contains 'signal.chat_id":"group:' 'group basic reply missing group chat trace'

run_text_scenario \
  'group scoped write/read' \
  send_group_text \
  'Use write_file to write ./group-note.txt with exactly GROUP_SCOPE_ONE. Then use edit_file to change it to exactly GROUP_SCOPE_TWO. Then use read_file and reply with only GROUP_SCOPE_TWO.' \
  'GROUP_SCOPE_TWO' \
  contains 'tool_execute' 'group scope scenario missing tool execution' \
  contains 'write_file' 'group scope scenario missing write_file' \
  contains 'edit_file' 'group scope scenario missing edit_file' \
  contains 'read_file' 'group scope scenario missing read_file' \
  contains 'principal":"Group' 'group scope scenario missing group principal' \
  contains 'scoped_root":"groups/' 'group scope scenario missing group scoped_root'

run_attachment_send_scenario \
  'group attachment save + read' \
  send_group_attachment \
  'Read the file I just attached using read_file and reply with only its first line.' \
  "$GROUP_TEXT_FILE" \
  'ATTACH_GROUP_OK' \
  contains 'saved signal attachment' 'group attachment scenario missing saved attachment trace' \
  contains 'relative_path":"\./attachments/' 'group attachment scenario missing relative attachment path' \
  contains 'scoped_root":"groups/' 'group attachment scenario missing group scoped root' \
  contains 'read_file' 'group attachment scenario missing read_file tool'

run_attachment_send_scenario \
  'group image auto-injection' \
  send_group_attachment \
  'What color is the attached image? Reply with one word.' \
  "$RED_PNG" \
  'red' \
  contains 'saved signal attachment' 'group image scenario missing saved attachment trace' \
  contains 'injecting image into message' 'group image scenario missing image injection trace' \
  contains 'scoped_root":"groups/' 'group image scenario missing group scoped root' \
  not_contains 'workspace scope denied access' 'group image scenario unexpectedly denied image scope'

run_send_image_scenario \
  'group signal_send_image' \
  send_group_attachment \
  'Use the signal_send_image tool to send back the attached image, then reply with only SENT_GROUP_IMAGE.' \
  "$RED_PNG" \
  'SENT_GROUP_IMAGE' \
  contains 'signal.target_kind":"group"' 'group signal_send_image missing group target' \
  contains 'signal.action":"send_attachment"' 'group signal_send_image missing attachment send'

note "phase 2: inner-trust scoped DM tests"
write_config inner no
restart_coop

run_text_scenario \
  'inner scoped read allowed' \
  send_direct_text \
  'Use read_file on ./ok.txt and reply with only INNER_OK.' \
  'INNER_OK' \
  contains 'read_file' 'inner allowed scenario missing read_file tool' \
  contains 'principal":"User' 'inner allowed scenario missing user principal' \
  contains 'scoped_root":"users/alice/' 'inner allowed scenario missing alice scoped_root'

run_text_scenario \
  'inner scoped traversal denied' \
  send_direct_text \
  'Use read_file on ../bob/secret.txt and reply with the exact error message.' \
  'path|workspace|not allowed|outside' \
  contains 'read_file' 'inner denial scenario missing read_file tool' \
  contains 'workspace scope denied access' 'inner denial scenario missing scope denial trace' \
  contains 'scoped_root":"users/alice/' 'inner denial scenario missing alice scoped_root'

pass 'full Signal workspace-isolation matrix'
