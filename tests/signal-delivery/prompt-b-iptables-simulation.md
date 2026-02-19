# Prompt B: iptables-Based Dead Connection Simulation

## Goal

Write a self-contained test that uses presage to send Signal messages, then simulates network death via iptables to verify websocket recovery behavior. This tests whether presage correctly recovers from dead TCP connections and whether messages are delivered after recovery.

## Background

Coop uses presage as a linked Signal device (device 5 on `+17204279035`, signal-cli is device 1). Messages sent via presage intermittently fail to reach all of the recipient's linked devices. One hypothesis is that websocket connections die silently and presage doesn't recover properly before sending.

Both presage websockets (identified and unidentified) have 55-second keepalives. After a silent TCP death, the keepalive should detect it within ~110 seconds. The `whoami()` probe in coop adds extra protection for the identified websocket. We need to verify this actually works end-to-end.

### Key files

- Presage source: `/root/.cargo/git/checkouts/presage-ce211e77a0d9397d/1b81dfe/presage/src/manager/registered.rs`
- libsignal-service-rs sender: `/root/.cargo/git/checkouts/libsignal-service-rs-7e457f3dfb9f3190/aebf607/src/sender.rs`
- Coop signal channel: `/root/coop/main/crates/coop-channels/src/signal.rs`
- Coop's signal DB: `/root/coop/main/db/signal.db`
- Signal server IPs: `76.223.92.165`, `13.248.212.111` (chat.signal.org via AWS Global Accelerator)

### Accounts

- Sender: `+17204279035` (signal-cli primary device 1, presage linked device 5)
- Recipient: `+17205818516` (signal-cli, single device for now)

## Step 1: Write the test binary

Create a standalone Rust binary at `tests/signal-delivery/src/main.rs` that uses presage directly (not through coop) to send messages. This isolates the test from coop's own logic.

The binary should:
1. Open the existing presage SQLite store at `../../db/signal.db` (coop's linked device)
2. Create a `Manager` in linked device mode
3. Send a text message to the recipient UUID
4. Report success/failure with timing

Create `tests/signal-delivery/Cargo.toml`:
```toml
[package]
name = "signal-delivery-test"
version = "0.1.0"
edition = "2021"

[dependencies]
presage = { git = "https://github.com/whisperfish/presage", rev = "1b81dfe" }
presage-store-sqlite = { git = "https://github.com/whisperfish/presage", rev = "1b81dfe" }
tokio = { version = "1", features = ["full"] }
tracing = "0.1"
tracing-subscriber = { version = "0.3", features = ["env-filter"] }
anyhow = "1"
clap = { version = "4", features = ["derive"] }
```

**Important**: This is a standalone binary crate, NOT part of the coop workspace. Do not add it to the workspace `Cargo.toml`. It should be built separately with `cd tests/signal-delivery && cargo build`.

The binary needs these subcommands:
- `send <recipient-uuid> <message>` — send a single message, report success/failure
- `send-loop <recipient-uuid> --count N --interval-secs S` — send N messages at S-second intervals, report delivery success/failure for each
- `monitor` — connect and listen for incoming messages, print them with timestamps

### Key presage API usage

Look at presage-cli for reference: `/root/.cargo/git/checkouts/presage-ce211e77a0d9397d/1b81dfe/presage-cli/src/main.rs`

The basic pattern:
```rust
use presage::manager::registered::RegistrationData;
use presage_store_sqlite::SqliteStore;

// Open existing store (coop's DB)
let db_path = "../../db/signal.db";
let store = SqliteStore::open(db_path).await?;
let manager = Manager::load_registered(store).await?;

// Send a message
use presage::proto::DataMessage;
let data_message = DataMessage {
    body: Some(message_text.to_string()),
    timestamp: Some(std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_millis() as u64),
    ..Default::default()
};

let content = presage::proto::ContentBody::from(data_message);
manager.send_message(recipient_service_id, content, timestamp).await?;
```

Check presage-cli's send command for the exact API. The manager may need to be `&mut`.

### IMPORTANT: SQLite locking

Coop and this test binary CANNOT use the same `signal.db` simultaneously — SQLite will lock. Either:
- Stop coop before running the test, OR  
- Copy the DB to a test location: `cp db/signal.db /tmp/signal-test.db` and use that

The copy approach is safer but note: the copied DB will share session state (ratchet keys). If coop sends messages while the test uses a stale copy, the ratchets will diverge and one side will fail. **Stop coop before testing.**

## Step 2: Write the test orchestration script

Create `tests/signal-delivery/run-iptables-test.sh`:

```bash
#!/usr/bin/env bash
set -euo pipefail

# Configuration
RECIPIENT_UUID="eedf560a-1201-4cde-a863-4e5f82142ebf"  # user's Signal UUID
SIGNAL_IPS="76.223.92.165 13.248.212.111"  # chat.signal.org IPs
SIGNAL_IPV6="2600:9000:a507:ab6d:4ce3:2f58:25d7:9cbf 2600:9000:a61f:527c:d5eb:a431:5239:3232"
DB_PATH="../../db/signal.db"
TEST_BINARY="./target/debug/signal-delivery-test"
RECIPIENT_CLI_NUMBER="+17205818516"

# Resolve fresh IPs in case they change
echo "=== Resolving chat.signal.org ==="
RESOLVED_IPS=$(host chat.signal.org | grep "has address" | awk '{print $NF}')
RESOLVED_IPV6=$(host chat.signal.org | grep "has IPv6" | awk '{print $NF}')
ALL_IPS="$RESOLVED_IPS"
ALL_IPV6="$RESOLVED_IPV6"
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

# Ensure coop is not running
if pgrep -f "coop" > /dev/null 2>&1; then
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
$TEST_BINARY send "$RECIPIENT_UUID" "$MSG1"
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
$TEST_BINARY send "$RECIPIENT_UUID" "$MSG2"
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
$TEST_BINARY send "$RECIPIENT_UUID" "$MSG3" || echo "  (send returned error, expected)"

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
$TEST_BINARY send "$RECIPIENT_UUID" "$MSG4"
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
$TEST_BINARY send "$RECIPIENT_UUID" "$MSG5A"
sleep 1
$TEST_BINARY send "$RECIPIENT_UUID" "$MSG5B"  
sleep 1
$TEST_BINARY send "$RECIPIENT_UUID" "$MSG5C"

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
```

## Step 3: Build and run

```bash
# Make sure coop is stopped
pkill -f coop || true

# Build the test binary
cd /root/coop/main/tests/signal-delivery
cargo build

# Run the test (requires root for iptables)
chmod +x run-iptables-test.sh
./run-iptables-test.sh 2>&1 | tee /tmp/iptables-test-results.txt
```

## Step 4: Interpret results

### If all tests pass
The websocket recovery mechanism works correctly. The problem is likely NOT dead websockets. Move investigation to:
- Server-side delivery issues (unlikely)
- Phone-side push notification throttling (iOS/Android platform issue)
- Session ratchet desync causing decryption failures on specific devices
- The problem only manifests with multi-device recipients (need to add a second device to `+17205818516` to test)

### If TEST 2 fails (send after recovery)
The websocket keepalive detected the dead connection (>110s), but presage failed to create a new websocket or the new websocket failed to send. Check:
- Tracing output for "creating new websocket" events
- Whether the error is from the identified or unidentified websocket
- Whether sealed sender fallback to identified works

### If TEST 4 fails (send during detection window)
The send happened while the websocket was dead but not yet detected. This is the ~110s vulnerability window. The fix would be:
- Add an equivalent of `whoami()` for the unidentified websocket
- Or: fall back to identified sender on WsClosing/IO errors (not just Unauthorized)
- Or: force-close both websockets when whoami() fails

### If TEST 5 fails partially (some rapid messages lost)
Race condition in websocket recreation — the first send triggers reconnection but subsequent sends arrive before it completes. Fix: serialize sends through a single connection establishment.

## Step 5: Test with the fix applied

Based on results, implement one of these fixes in coop's `send_task` (in `crates/coop-channels/src/signal.rs`):

### Fix Option 1: Clear both websockets on probe failure

After the existing `whoami()` probe fails, also invalidate the unidentified websocket:

```rust
if is_durable_action(&action)
    && let Err(error) = Box::pin(manager.whoami()).await
{
    warn!(error = %error, "pre-send websocket probe failed, connection will be refreshed");
    // Also clear the unidentified websocket since the network was down
    // (requires adding a method to presage Manager)
    manager.clear_unidentified_websocket().await;
}
```

This requires patching presage to expose a `clear_unidentified_websocket()` method.

### Fix Option 2: Retry with identified sender on connection errors

Patch libsignal-service-rs `try_send_message` to treat `WsClosing` and IO errors like `Unauthorized` — clear `unidentified_access` and retry via the identified websocket:

```rust
Err(ServiceError::WsClosing { .. }) | Err(ServiceError::WsError(_))
    if unidentified_access.is_some() =>
{
    tracing::warn!("websocket error using unidentified; retry over identified");
    unidentified_access = None;
},
```

### Fix Option 3: Send with timeout + retry

Wrap the send in a timeout. If the send takes longer than e.g. 30 seconds (indicating a stuck websocket), cancel and retry with a fresh connection.

After applying the fix, re-run the iptables test suite to verify all tests pass.

## Notes

- The iptables approach simulates a COMPLETE network failure (both websockets die). To test asymmetric failure (only one websocket dies), you'd need a TCP proxy approach — see below.
- The test binary reuses coop's signal.db, which means it shares the same Signal session keys and device identity. Only one process should use the DB at a time.
- Signal's server may rate-limit if you send too many test messages. Keep test runs reasonable.
- The resolved IPs for chat.signal.org may change. The script resolves them fresh at startup.
- `iptables -j DROP` silently drops packets (simulating network death). `-j REJECT` would send RST, which is a different failure mode (immediate detection). Use DROP for realistic dead-connection simulation.

## Optional: TCP proxy for asymmetric testing

If iptables tests pass but the problem persists, you may need to test asymmetric failure where only the unidentified websocket dies. This requires a TCP proxy:

1. Run two socat instances forwarding to chat.signal.org:443
2. Patch presage to use different proxy ports for identified vs unidentified websockets
3. Kill only the unidentified proxy
4. Verify presage detects the dead unidentified websocket and recovers

This is significantly more complex and should only be attempted if iptables tests don't reveal the issue.
