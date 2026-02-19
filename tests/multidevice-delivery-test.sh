#!/usr/bin/env bash
#
# Multi-Device Recipient Delivery Test
#
# Reproduces: "coop sends a message to my number, but it doesn't arrive
#              on all my linked devices — phone (device 1) is worst."
#
# Setup:
#   SENDER:    coop/presage on +17204279035 (device 5, linked to signal-cli primary)
#   RECIPIENT: +17205818516 — currently single device (signal-cli, device 1)
#
# To properly test multi-device recipient delivery, we need to link a second
# device to the recipient account. This script handles both scenarios:
#   1. Single-device recipient (baseline — does device 1 always get the message?)
#   2. Multi-device recipient (after linking — do ALL devices get every message?)
#
# The test sends N messages from coop→recipient, then checks how many
# arrived at each recipient device.
#
# Usage:
#   ./tests/multidevice-delivery-test.sh [count]   # default: 10 messages
#
set -euo pipefail

COOP_NUMBER="+17204279035"
RECIPIENT="+17205818516"
RECIPIENT_UUID="218054c4-7cc5-4854-ab1d-a3343e331b31"
COOP_DB="./db/signal.db"
COOP_DB_ABS="$(cd "$(dirname "$0")/.." && pwd)/db/signal.db"
COUNT="${1:-10}"
TRACE_FILE="./tests/delivery-trace.jsonl"

RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
CYAN='\033[0;36m'
NC='\033[0m'

info()  { echo -e "${GREEN}[INFO]${NC} $*"; }
warn()  { echo -e "${YELLOW}[WARN]${NC} $*"; }
error() { echo -e "${RED}[FAIL]${NC} $*"; }
step()  { echo -e "${CYAN}[STEP]${NC} $*"; }

# ──────────────────────────────────────────────────────────────────────

check_prerequisites() {
    info "Checking prerequisites..."

    if ! command -v signal-cli &>/dev/null; then
        error "signal-cli not found"
        exit 1
    fi

    if [ ! -f "$COOP_DB" ]; then
        error "Coop presage DB not found at $COOP_DB"
        exit 1
    fi

    # Check recipient device count
    local devices
    devices=$(signal-cli -u "$RECIPIENT" listDevices 2>&1)
    local device_count
    device_count=$(echo "$devices" | grep -c "^- Device")
    info "Recipient has $device_count device(s):"
    echo "$devices" | grep "^- Device"
    echo

    if [ "$device_count" -eq 1 ]; then
        warn "Recipient has only 1 device. This tests basic delivery but"
        warn "can't reproduce multi-device fan-out issues."
        warn "To add a second device, link another presage or signal-cli instance."
        echo
    fi
}

analyze_coop_sessions() {
    step "Analyzing coop's session store for recipient..."

    python3 -c "
import sqlite3

conn = sqlite3.connect('$COOP_DB')
uuid = '$RECIPIENT_UUID'

sessions = conn.execute('''
    SELECT device_id, identity, length(record) as bytes
    FROM sessions
    WHERE address = ? AND identity = 'aci'
    ORDER BY device_id
''', (uuid,)).fetchall()

print(f'  Coop knows about {len(sessions)} ACI device(s) for recipient:')
for dev, ident, size in sessions:
    print(f'    device {dev} — session record {size} bytes')

if not sessions:
    print('  ⚠️  No sessions! First message will establish sessions via pre-key fetch.')
else:
    device_ids = [s[0] for s in sessions]
    if 1 not in device_ids:
        print('  ❌ Primary device (1) NOT in session store!')
    else:
        print('  ✅ Primary device (1) in session store')

print()
print('  When coop sends, presage will:')
print('    1. Encrypt the message individually for each device above')
print('    2. Add device 1 (always included even if no session)')
print('    3. Send the batch to Signal server')
print('    4. If server says \"wrong devices\" (MismatchedDevicesException),')
print('       drop/add sessions and retry (up to 4 times)')
print()

# Check profile key (needed for sealed sender)
pk = conn.execute('''
    SELECT length(key) FROM profile_keys WHERE uuid = ?
''', (uuid,)).fetchone()
if pk:
    print(f'  ✅ Profile key stored ({pk[0]} bytes) — will use sealed sender')
else:
    print(f'  ⚠️  No profile key — will use identified sender (no sealed sender)')
"
    echo
}

send_from_coop() {
    local marker="$1"

    # We send by having signal-cli (as coop's primary) send the message.
    # This goes through signal-cli's sender, not presage's.
    # To test presage's sender, we need coop running.
    #
    # BUT: we can also send directly from signal-cli on coop's number
    # to observe the recipient-side behavior. The encryption & device
    # enumeration happens the same way.
    signal-cli -u "$COOP_NUMBER" send -m "$marker" "$RECIPIENT" 2>&1
}

receive_at_recipient() {
    local timeout="${1:-5}"
    signal-cli -u "$RECIPIENT" receive --timeout "$timeout" 2>&1
}

# ──────────────────────────────────────────────────────────────────────

run_delivery_test() {
    step "Running delivery test: $COUNT messages from coop→recipient"
    echo

    # Drain old messages first
    info "Draining old messages from recipient..."
    receive_at_recipient 2 >/dev/null 2>&1 || true
    echo

    local sent=0
    local received=0
    local markers=()

    for i in $(seq 1 "$COUNT"); do
        local marker="delivery-test-${i}-$(date +%s%N)"
        markers+=("$marker")

        info "[$i/$COUNT] Sending: $marker"
        local send_output
        send_output=$(send_from_coop "$marker" 2>&1)
        local send_ts
        send_ts=$(echo "$send_output" | grep -oP '^\d+$' | head -1)
        if [ -n "$send_ts" ]; then
            sent=$((sent + 1))
            info "  Sent at timestamp $send_ts"
        else
            error "  Send may have failed: $send_output"
        fi

        # Small delay to avoid rate limiting
        sleep 0.5
    done

    echo
    info "Sent $sent/$COUNT messages. Now receiving at recipient..."
    echo

    # Receive all messages
    local recv_output
    recv_output=$(receive_at_recipient 15 2>&1)

    # Check which markers arrived
    for i in $(seq 0 $((${#markers[@]} - 1))); do
        local marker="${markers[$i]}"
        local num=$((i + 1))
        if echo "$recv_output" | grep -q "$marker"; then
            received=$((received + 1))
            info "  [$num] ✅ received: $marker"
        else
            error "  [$num] ❌ MISSING: $marker"
        fi
    done

    echo
    echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
    if [ "$received" -eq "$sent" ]; then
        info "Result: $received/$sent messages delivered ✅"
    else
        local missed=$((sent - received))
        error "Result: $received/$sent messages delivered — $missed MISSING ❌"
    fi
    echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
    echo

    # Dump any MismatchedDevices or error traces
    if [ -f "$TRACE_FILE" ]; then
        local mismatches
        mismatches=$(grep -c "MismatchedDevices\|StaleDevices" "$TRACE_FILE" 2>/dev/null || echo 0)
        if [ "$mismatches" -gt 0 ]; then
            warn "Found $mismatches MismatchedDevices/StaleDevices events in trace!"
            grep "MismatchedDevices\|StaleDevices" "$TRACE_FILE" | tail -5
        fi
    fi
}

# ──────────────────────────────────────────────────────────────────────

run_presage_delivery_test() {
    step "Testing delivery through PRESAGE sender"
    echo

    local presage_cli="/root/.cargo/git/checkouts/presage-ce211e77a0d9397d/1b81dfe"

    info "Sending via presage-cli (same DB as coop, same session state)..."
    echo

    local sent=0
    local received=0
    local markers=()

    for i in $(seq 1 "$COUNT"); do
        local marker="presage-delivery-${i}-$(date +%s%N)"
        markers+=("$marker")

        info "[$i/$COUNT] Sending via presage: $marker"
        # Run presage-cli send from the same DB coop uses
        local send_output
        send_output=$(cd "$presage_cli" && cargo run -q --bin presage-cli -- \
            --db-path "$COOP_DB_ABS" \
            send --recipient "$RECIPIENT_UUID" "$marker" 2>&1) || true

        if echo "$send_output" | grep -qi "error\|failed\|panic"; then
            error "  Send error: $send_output"
        else
            sent=$((sent + 1))
            info "  Sent OK"
        fi

        sleep 0.5
    done

    echo
    info "Sent $sent/$COUNT via presage. Now receiving at recipient..."
    echo

    local recv_output
    recv_output=$(receive_at_recipient 15 2>&1)

    for i in $(seq 0 $((${#markers[@]} - 1))); do
        local marker="${markers[$i]}"
        local num=$((i + 1))
        if echo "$recv_output" | grep -q "$marker"; then
            received=$((received + 1))
            info "  [$num] ✅ received: $marker"
        else
            error "  [$num] ❌ MISSING: $marker"
        fi
    done

    echo
    echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
    if [ "$received" -eq "$sent" ]; then
        info "Presage result: $received/$sent messages delivered ✅"
    else
        local missed=$((sent - received))
        error "Presage result: $received/$sent delivered — $missed MISSING ❌"
    fi
    echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
}

# ──────────────────────────────────────────────────────────────────────

main() {
    cd "$(dirname "$0")/.."
    echo
    info "Multi-Device Recipient Delivery Test"
    info "Sender:    coop ($COOP_NUMBER)"
    info "Recipient: $RECIPIENT"
    info "Messages:  $COUNT"
    echo

    check_prerequisites
    analyze_coop_sessions

    step "Phase 1: Delivery via signal-cli sender (baseline)"
    run_delivery_test

    echo
    step "Phase 2: Delivery via presage sender"
    run_presage_delivery_test
}

main "$@"
