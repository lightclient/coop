# iptables Test Results — 2026-02-19

## Summary

All messages were **accepted by the Signal server** successfully for both the recipient 
and the sync to self. However, `signal-cli` on the recipient side did **not receive any 
messages**, even in the baseline test (TEST 1) with no iptables blocking.

## Key Findings

### 1. Presage sending works correctly

Every send completed successfully:
- Messages encrypted for 4 recipient devices via sealed sender (unidentified WS)
- Server accepted every message (`SendMessageResponse { needs_sync: false }`)
- Self-sync also succeeded via identified websocket
- Each send takes ~700ms end-to-end

### 2. Presage creates fresh websockets per invocation

Each test binary invocation creates new identified + unidentified websockets (~130ms each).
This means the iptables test doesn't test websocket *recovery* as designed — there are
no persistent websockets to kill. Each send is a fresh connection.

**This invalidates the test's original hypothesis.** The iptables test was designed for 
a long-running process (like coop) with persistent websockets. When run as separate 
short-lived processes, each send creates fresh connections, bypassing the dead-websocket 
detection problem entirely.

### 3. signal-cli receiver doesn't see any messages

The `signal-cli -u +17205818516 receive` command produced **zero message output**. Its
log shows only:
```
WARN  ReceiveHelper - Connection closed unexpectedly, reconnecting in 100 ms
WARN  ReceiveHelper - Connection closed unexpectedly, reconnecting in ...
```

This is partially caused by iptables blocking (signal-cli is on the same machine), but
TEST 1 ran before any blocking and still showed no messages. Possible causes:
- signal-cli session keys are out of sync with what presage sends (ratchet divergence)
- signal-cli can't decrypt sealed sender messages from presage's linked device
- signal-cli's websocket wasn't connected in the 5-second window after send

### 4. Account attributes 422 error (non-fatal)

Every presage connection triggers a `set_account_attributes` call that returns HTTP 422.
This is a known issue — Signal's server protocol has changed and presage's registration
data is stale. When pending messages exist in the queue, this 422 causes the identified
websocket to close before the background drain completes, which used to make the self-sync
fail. With an empty queue, the 422 is benign (just a warning log).

### 5. Test design flaw: same-machine iptables

The iptables rules block ALL traffic to Signal's servers from this machine. Since both
the sender (presage test binary) and receiver (`signal-cli`) run on the same machine,
blocking Signal traffic blocks both. This means tests 2-5 don't verify delivery — they
only verify that the sender can create new websockets after unblocking.

## Detailed Results

| Test | Send Status | Receive Status | Notes |
|------|-------------|----------------|-------|
| TEST 1: Baseline | ✅ Server accepted | ❌ Not received | No iptables, clean send |
| TEST 2: After 150s block | ✅ Server accepted | ❌ Not received | signal-cli also blocked |
| TEST 3: During outage | ✅ Send timed out (60s) | ❌ N/A | Expected — network was down |
| TEST 4: 60s block | ✅ Server accepted | ❌ Not received | signal-cli also blocked |
| TEST 5: 300s block, rapid | ✅ All 3 accepted | ❌ None received | signal-cli also blocked |

## Recommendations

### For testing websocket recovery in coop
The iptables approach should be tested against **coop's long-running signal channel**, 
not a standalone binary. Run coop, block Signal traffic, wait for keepalive timeout, 
unblock, then send a message through coop and verify delivery.

### For the delivery investigation
1. **Check recipient device list**: The recipient has 4 devices. signal-cli may be a 
   device that's been de-registered or has stale session keys
2. **Use signal-cli to send, presage to receive**: Reverse the direction to test if 
   the issue is sender-side (presage) or receiver-side (signal-cli)
3. **Check on actual phone**: Verify whether messages sent by the test binary show up 
   on the recipient's phone (not just signal-cli)
4. **Fix the 422 error**: The `set_account_attributes` 422 suggests presage's registration 
   data is outdated. This needs investigation in the presage/libsignal-service layer

## Files Created

- `tests/signal-delivery/Cargo.toml` — standalone crate (not in workspace)
- `tests/signal-delivery/src/main.rs` — test binary with send/send-loop/monitor commands
- `tests/signal-delivery/run-iptables-test.sh` — orchestration script
- `/tmp/iptables-test-results.txt` — raw test output
