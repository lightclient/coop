#!/usr/bin/env bash
set -euo pipefail

# Configuration
RECIPIENT_UUID="eedf560a-1201-4cde-a863-4e5f82142ebf"  # user's Signal UUID
DB_PATH="../../db/signal.db"
TEST_BINARY="./target/debug/signal-delivery-test"
RECIPIENT_CLI_NUMBER="+17205818516"

# Resolve fresh IPs in case they change
echo "=== Resolving chat.signal.org ==="
RESOLVED_IPS=$(host chat.signal.org | grep "has address" | awk '{print $NF}')
RESOLVED_IPV6=$(host chat.signal.org | grep "has IPv6" | awk '{print $NF}') || true
ALL_IPS="$RESOLVED_IPS"
ALL_IPV6="${RESOLVED_IPV6:-}"
echo "IPv4: $ALL_IPS"
echo "IPv6: $ALL_IPV6"

cleanup() {
    echo ""
    echo "=== CLEANUP: removing iptables rules ==="
    for ip in $ALL_IPS; do
        iptables -D OUTPUT -d "$ip" -j DROP 2>/dev/null || true
    done
    for ip in $ALL_IPV6; do
        ip6tables -D OUTPUT -d "$ip" -j DROP 2>/dev/null || true
    done
    echo "iptables rules removed"

    # Kill background processes
    kill %1 2>/dev/null || true
    kill %2 2>/dev/null || true
}
trap cleanup EXIT

block_signal() {
    echo "=== BLOCKING Signal traffic ==="
    for ip in $ALL_IPS; do
        iptables -A OUTPUT -d "$ip" -j DROP
        echo "  blocked $ip"
    done
    for ip in $ALL_IPV6; do
        ip6tables -A OUTPUT -d "$ip" -j DROP
        echo "  blocked $ip (v6)"
    done
}

unblock_signal() {
    echo "=== UNBLOCKING Signal traffic ==="
    for ip in $ALL_IPS; do
        iptables -D OUTPUT -d "$ip" -j DROP
        echo "  unblocked $ip"
    done
    for ip in $ALL_IPV6; do
        ip6tables -D OUTPUT -d "$ip" -j DROP 2>/dev/null || true
    done
}

# Ensure coop gateway/tui is not running
if pgrep -f "coop-(gateway|tui)" > /dev/null 2>&1; then
    echo "ERROR: coop is running. Stop it first."
    exit 1
fi

# Build
echo "=== Building test binary ==="
cargo build

# Start recipient listener in background
echo "=== Starting recipient listener ==="
signal-cli -u "$RECIPIENT_CLI_NUMBER" receive --timeout 300 > /tmp/signal-recv.log 2>&1 &
RECV_PID=$!
echo "  PID: $RECV_PID"
sleep 2

echo ""
echo "============================================"
echo "  TEST 1: Baseline send (no network issues)"
echo "============================================"
echo ""

MSG1="baseline-test-$(date +%s)"
echo "Sending: $MSG1"
$TEST_BINARY --db-path "$DB_PATH" send "$RECIPIENT_UUID" "$MSG1"
echo "Waiting 5s for delivery..."
sleep 5
if grep -q "$MSG1" /tmp/signal-recv.log; then
    echo "✅ TEST 1 PASSED: baseline message delivered"
else
    echo "❌ TEST 1 FAILED: baseline message not received"
    echo "  (checking log: $(tail -5 /tmp/signal-recv.log))"
fi

echo ""
echo "============================================"
echo "  TEST 2: Send after network recovery"
echo "============================================"
echo "  1. Block Signal traffic (simulate dead TCP)"
echo "  2. Wait 150s (>2 keepalive cycles = 110s)"
echo "  3. Unblock traffic"
echo "  4. Send message"
echo "  5. Check delivery"
echo "============================================"
echo ""

block_signal

echo "Waiting 150 seconds for keepalive detection..."
for i in $(seq 150 -10 0); do
    echo "  ${i}s remaining..."
    sleep 10
done

unblock_signal
echo "Network restored. Waiting 10s for websocket reconnection..."
sleep 10

MSG2="recovery-test-$(date +%s)"
echo "Sending: $MSG2"
$TEST_BINARY --db-path "$DB_PATH" send "$RECIPIENT_UUID" "$MSG2"
echo "Waiting 10s for delivery..."
sleep 10
if grep -q "$MSG2" /tmp/signal-recv.log; then
    echo "✅ TEST 2 PASSED: post-recovery message delivered"
else
    echo "❌ TEST 2 FAILED: post-recovery message not received"
    echo "  (this would indicate websocket recovery failure)"
fi

echo ""
echo "============================================"
echo "  TEST 3: Send DURING network outage"
echo "============================================"
echo "  1. Block Signal traffic"
echo "  2. Immediately try to send (should fail or queue)"
echo "  3. Unblock traffic"
echo "  4. Check if message eventually delivers"
echo "============================================"
echo ""

block_signal

MSG3="during-outage-$(date +%s)"
echo "Sending during outage: $MSG3"
$TEST_BINARY --db-path "$DB_PATH" send "$RECIPIENT_UUID" "$MSG3" || echo "  (send returned error, expected)"

unblock_signal
echo "Network restored. Waiting 15s..."
sleep 15
if grep -q "$MSG3" /tmp/signal-recv.log; then
    echo "✅ TEST 3 PASSED: message delivered after outage"
else
    echo "❌ TEST 3: message not delivered (expected if send failed)"
fi

echo ""
echo "============================================"
echo "  TEST 4: Send during keepalive detection window"
echo "============================================"
echo "  1. Block Signal traffic"
echo "  2. Wait 60s (inside the 110s detection window)"
echo "  3. Unblock traffic"
echo "  4. Send immediately"
echo "  5. Check delivery"
echo "============================================"
echo ""

block_signal
echo "Waiting 60 seconds (inside keepalive detection window)..."
for i in $(seq 60 -10 0); do
    echo "  ${i}s remaining..."
    sleep 10
done
unblock_signal
echo "Network restored. Sending immediately..."
sleep 2

MSG4="window-test-$(date +%s)"
echo "Sending: $MSG4"
$TEST_BINARY --db-path "$DB_PATH" send "$RECIPIENT_UUID" "$MSG4"
echo "Waiting 10s for delivery..."
sleep 10
if grep -q "$MSG4" /tmp/signal-recv.log; then
    echo "✅ TEST 4 PASSED: message delivered after mid-window recovery"
else
    echo "❌ TEST 4 FAILED: message not delivered"
fi

echo ""
echo "============================================"
echo "  TEST 5: Rapid send after long outage"
echo "============================================"
echo "  1. Block Signal traffic"
echo "  2. Wait 300s (5 min — well past keepalive detection)"
echo "  3. Unblock traffic"
echo "  4. Send 3 messages rapidly (1s apart)"
echo "  5. Check all 3 deliver"
echo "============================================"
echo ""

block_signal
echo "Waiting 300 seconds (5 minutes)..."
for i in $(seq 300 -30 0); do
    echo "  ${i}s remaining..."
    sleep 30
done
unblock_signal
echo "Network restored. Sending 3 messages rapidly..."
sleep 2

MSG5A="rapid-a-$(date +%s)"
MSG5B="rapid-b-$(date +%s)"
MSG5C="rapid-c-$(date +%s)"
$TEST_BINARY --db-path "$DB_PATH" send "$RECIPIENT_UUID" "$MSG5A"
sleep 1
$TEST_BINARY --db-path "$DB_PATH" send "$RECIPIENT_UUID" "$MSG5B"
sleep 1
$TEST_BINARY --db-path "$DB_PATH" send "$RECIPIENT_UUID" "$MSG5C"

echo "Waiting 15s for delivery..."
sleep 15
PASS=0
for msg in "$MSG5A" "$MSG5B" "$MSG5C"; do
    if grep -q "$msg" /tmp/signal-recv.log; then
        echo "  ✅ $msg delivered"
        PASS=$((PASS + 1))
    else
        echo "  ❌ $msg NOT delivered"
    fi
done
echo "TEST 5: $PASS/3 messages delivered"

echo ""
echo "============================================"
echo "  RESULTS SUMMARY"
echo "============================================"

# Kill receiver and show full log
kill $RECV_PID 2>/dev/null || true
echo ""
echo "Full receive log:"
cat /tmp/signal-recv.log
