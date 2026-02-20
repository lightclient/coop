# Signal Delivery Investigation

Status: **Active investigation** — 2026-02-19

## Problem Statement

Coop sends Signal messages via presage (a Rust Signal client library running as a linked device). Messages intermittently fail to reach all of the recipient's linked devices. The problem is worst on phones and worst in the morning after hours of idle time.

## Architecture

```
┌────────────────────────────────────────────────────────────┐
│                  Coop Gateway (long-running)                │
│                                                              │
│  SignalChannel (coop-channels/src/signal.rs)                │
│    ├── receive_task   ← presage receive_messages() stream   │
│    └── send_task      ← sends via presage Manager           │
│          │                                                   │
│          ├── whoami() probe (pre-send websocket health check)│
│          └── manager.send_message(recipient, body, ts)      │
│                │                                             │
│  presage Manager (vendor/presage)                           │
│    ├── identified_websocket   (authenticated, 55s keepalive) │
│    └── unidentified_websocket (sealed sender, 55s keepalive) │
│                │                                             │
│  libsignal-service-rs MessageSender (vendor/libsignal-....) │
│    ├── try_send_message() → encrypt per-device → send batch  │
│    ├── Sealed sender (unidentified WS) tried first           │
│    ├── Falls back to identified WS on Unauthorized           │
│    └── Multi-device sync sent to self after each message     │
└────────────────────────────────────────────────────────────┘
         │
    WebSocket (TLS)
         │
    Signal Server (chat.signal.org)
         │
    Push to recipient devices
```

### Accounts

| Role | Number | Identity |
|------|--------|----------|
| Sender (coop) | +17204279035 | ACI `c51ed74b-99cd-4cc4-8b44-2756cd0f7afd`, device 5 (presage linked device), device 1 (signal-cli primary) |
| Recipient | +17205818516 | ACI `eedf560a-1201-4cde-a863-4e5f82142ebf` (UUID used in tests) — **but note**: tests also showed UUID `218054c4-7cc5-4854-ab1d-a3343e331b31` for this recipient, which needs clarification |

The recipient has **4 linked devices** (device IDs 1, 2, 3, 4 as enumerated in encryption logs).

## Hypotheses Investigated

### H1: Dead websocket not detected before send (iptables test)

**Theory**: The presage websocket dies silently (NAT timeout, ISP dropout). The 55-second keepalive hasn't detected it yet. The send goes through the dead websocket and fails silently.

**Mitigation already in coop**: The `send_task` calls `manager.whoami()` before every durable send. If the identified websocket is dead, `whoami()` fails fast and presage replaces the socket. This protects the identified websocket path.

**Gap**: The unidentified websocket (sealed sender) has no equivalent probe. If the unidentified WS is dead but the identified WS is alive, presage will try sealed sender on the dead socket, fail, and should fall back to identified sender. But the fallback only triggers on `ServiceError::Unauthorized`, not on `WsClosing` or IO errors.

**Test result (iptables test)**: The iptables test was designed to simulate dead TCP connections by blocking Signal server IPs. **However, the test was structurally flawed**: it used short-lived standalone processes (fresh websockets per send) rather than testing coop's long-running persistent websockets. Every send created new connections, bypassing the dead-websocket problem entirely. The test **does not validate or invalidate** this hypothesis.

### H2: Sealed sender delivery to specific devices fails

**Observation**: All sends go through the unidentified websocket (sealed sender) first. The server accepts the message. But sealed sender messages may not be delivered to all devices in the same way as identified sends.

**Evidence from test**: Every test send was accepted by the server via sealed sender (`ws send_messages succeeded (unidentified)`, `message accepted by server`, `needs_sync: false`). The server did not reject or error. If the issue is server-side delivery of sealed sender messages to specific device types, we cannot observe it from the sender side.

### H3: Session ratchet divergence

**Theory**: Coop's presage DB and signal-cli share sessions for the same account (+17204279035). When both are active, they advance the Double Ratchet independently. The recipient's device may reject messages encrypted with a stale ratchet state.

**Evidence**: The test binary uses the same `db/signal.db` as coop. The prompt explicitly warns about this: "If coop sends messages while the test uses a stale copy, the ratchets will diverge and one side will fail." This is a real risk for any multi-client setup sharing the same identity.

### H4: signal-cli on recipient cannot decrypt presage messages

**Evidence**: In all 5 iptables tests + standalone verification, `signal-cli -u +17205818516 receive` produced **zero message output**, even in the baseline test with no network interference. All messages were accepted by the server. The signal-cli receive log showed only `Connection closed unexpectedly, reconnecting` warnings.

This is the strongest signal from the investigation: **the recipient's signal-cli never sees any messages from presage**, even under ideal conditions.

### H5: Account attributes stale (HTTP 422)

**Observation**: Every presage connection triggers `set_account_attributes` which returns HTTP 422 from Signal's server. When there are pending messages in the receive queue, this 422 causes presage's receive handler to fail, which closes the identified websocket. A subsequent send's self-sync then fails because the identified websocket is gone.

When the receive queue is empty (no pending messages), the 422 is logged as a warning but does not close the websocket. The 422 indicates presage's registration data contains fields the server no longer accepts — likely a Signal protocol update that presage hasn't tracked.

**Impact**: In the worst case, the identified websocket is closed by the 422 error during receive startup. This means the self-sync message (sent to the sender's other devices after each outbound message) fails with `WebSocket closing`. The message to the recipient was already sent successfully via sealed sender before the sync was attempted.

## Instrumentation Added

### Vendor patches (committed)

Presage and libsignal-service-rs were vendored into the workspace with detailed send-path tracing:

**`vendor/libsignal-service-rs/src/sender.rs`**:
- `info!` on device enumeration: full device list and count for each recipient
- `info!` on send path choice: identified vs unidentified websocket, with `ws_closed` state
- `info!` on server acceptance: `needs_sync` flag, unidentified flag
- `warn!`/`error!` on all error branches: MismatchedDevices, StaleDevices, Unauthorized fallback, NotFound, unhandled errors

**`vendor/libsignal-service-rs/src/websocket/sender.rs`**:
- `info!` pre/post send for both websocket paths, including `ws_closed` state and response

**`vendor/presage/presage/src/manager/registered.rs`**:
- `info!` when creating new websockets (with `previous_was_closed` flag)
- `debug!` when reusing existing websockets

**Tracing filter** (`crates/coop-gateway/src/tracing_setup.rs`):
- Console: `libsignal_service::sender=debug`, `libsignal_service::websocket::sender=info`, `presage::manager::registered=debug`
- JSONL: Same plus `libsignal_service::websocket::sender=debug`

These patches are active via `[patch]` directives in the workspace `Cargo.toml`:
```toml
[patch."https://github.com/whisperfish/presage"]
presage = { path = "vendor/presage/presage" }
presage-store-sqlite = { path = "vendor/presage/presage-store-sqlite" }

[patch."https://github.com/whisperfish/libsignal-service-rs"]
libsignal-service = { path = "vendor/libsignal-service-rs" }
```

### Coop's existing mitigations (`coop-channels/src/signal.rs`)

1. **`whoami()` pre-send probe**: Before every durable action (text, attachment, reaction, reply), the send_task calls `manager.whoami()` to test the identified websocket. If it fails, the websocket is refreshed.

2. **Reconnection loop**: The receive_task uses exponential backoff (1s → 30s) to reconnect when the receive stream ends.

3. **Health reporting**: Channel health is set to Degraded/Unhealthy on failures, allowing the gateway to track channel status.

## Test Infrastructure Created

### `tests/signal-delivery/` — Standalone presage test binary

A standalone Rust binary that uses presage directly (not through coop) to isolate the sending path:

- **`src/main.rs`**: Binary with `send`, `send-loop`, and `monitor` subcommands
- **`Cargo.toml`**: Standalone crate (not in workspace, has `[workspace]` table to prevent auto-detection), with matching `[patch]` sections for curve25519-dalek, presage, and libsignal-service
- **`run-iptables-test.sh`**: Orchestration script for 5 network failure scenarios using iptables DROP rules

Build: `cd tests/signal-delivery && cargo build`

The test binary spawns a background `receive_messages()` loop (required for websocket responses) and wraps sends in a 60-second timeout. It treats `WsClosing` errors as non-fatal (message to recipient likely delivered, self-sync failed).

### `tests/multidevice-delivery-test.sh` — signal-cli-based delivery test

Tests delivery from signal-cli (same sender number) and from presage (via presage-cli) to the recipient, checking signal-cli on the recipient side for each message.

## Findings

### What works

1. **Presage send path is functional**: Messages are encrypted for all 4 recipient devices, sent via sealed sender, and accepted by the Signal server every time.

2. **Websocket creation is fast**: New identified + unidentified websocket pair takes ~130ms each.

3. **Self-sync works** (when identified WS is healthy): After sending to the recipient, the sync message to the sender's own devices (4 devices: 1, 2, 3, 4) succeeds via the identified websocket.

4. **Device enumeration is correct**: The sender knows about all 4 recipient devices and encrypts for each one.

### What doesn't work

1. **signal-cli on recipient never receives messages from presage**: Zero messages received across all tests, even with no network interference. This is the core unsolved issue.

2. **HTTP 422 on `set_account_attributes`**: Every presage connection gets this error. Non-fatal when the receive queue is empty, but causes identified websocket closure when pending messages exist.

3. **Recipient UUID confusion**: The test prompt uses UUID `eedf560a-1201-4cde-a863-4e5f82142ebf` for the recipient, but the receive logs show incoming messages from `218054c4-7cc5-4854-ab1d-a3343e331b31` for the same number. This needs investigation — it may be a PNI vs ACI distinction or a different account entirely.

### What we don't know

1. **Whether messages arrive on the recipient's phone**: All testing was done with signal-cli. The phone's Signal app may be receiving messages that signal-cli misses.

2. **Why signal-cli doesn't receive**: Could be decryption failure (sealed sender incompatibility), session state mismatch, or signal-cli configuration issue.

3. **Whether the dead-websocket hypothesis is real**: The iptables test couldn't validate it due to the structural flaw (short-lived processes vs. long-running daemon). The `whoami()` probe in coop's send_task should catch most dead identified websocket cases, but the unidentified websocket has no equivalent protection.

4. **Whether the 422 error causes real delivery failures**: It closes the identified websocket when pending messages exist in the queue, which could cause the self-sync to fail. But the message to the recipient goes through the unidentified websocket first and succeeds before the 422 kills the identified WS.

## Fix Applied (2026-02-19)

### Root cause: UUID-only sender certificate + push delivery interaction

Trace analysis of the Feb 19 morning briefing confirmed the send path was healthy from the sender's perspective — all 4 recipient devices enumerated, encrypted, sent via sealed sender, accepted by server, zero errors. The problem is downstream: recipient devices failing to process certain sealed-sender messages delivered via push notification.

**Timeline (recipient's phone, Mountain Time):**
- ✅ 12am, 1am, 2am — hourly heartbeats received (240-675 byte protobufs)
- ⏭️ 3am — HEARTBEAT_OK, no Signal send
- ❌ 4:30am — morning briefing NOT received (2914 byte protobuf)
- ✅ 5am — hourly heartbeat received (340 bytes)

All sends used the identical code path (sealed sender, 4 devices, server accepted, `needs_sync: false`). However, signal-cli sends the **same morning briefing content** to the same phone and it always arrives. signal-cli uses the full sender certificate (including E164 phone number), while presage uses `get_uuid_only_sender_certificate()` which omits the phone number.

The UUID-only certificate doesn't cause 100% delivery failure (small heartbeats got through), but it makes delivery **unreliable** — particularly in the push-notification delivery path used when the phone is sleeping. The recipient's Signal app may need the E164 to match the sealed-sender message to a contact, and this matching may behave differently in the push-wake-fetch path vs. the active-websocket path:

- **Phone awake (active WebSocket)**: Messages delivered inline, processed immediately — UUID-only cert works because the app has full context available
- **Phone sleeping (push delivery)**: Messages queued, delivered via push notification wake. Brief processing window, possibly stricter or different cert validation — UUID-only cert may fail to match, message silently dropped

This explains why the problem is "worst on phones and worst in the morning after hours of idle time."

### Changes

**`vendor/presage/presage/src/manager/registered.rs`**: Switched from `get_uuid_only_sender_certificate()` to `get_sender_certificate()` (includes E164 phone number). This matches signal-cli's behavior, which reliably delivers to sleeping phones.

**`vendor/libsignal-service-rs/src/sender.rs`**: Added sealed-sender fallback to identified sender for `WsError`, `WsClosing`, `Timeout`, and `SendError` — not just `Unauthorized`. If the unidentified websocket is dead/degraded, these transport errors now cause a retry via the identified websocket. Defense-in-depth for H1 (dead unidentified websocket).

## Recommended Next Steps

### Immediate

1. **Verify phone delivery**: Send a test message from the test binary and check whether it arrives on the recipient's actual phone (not signal-cli). This separates "server accepted but not delivered" from "delivered but signal-cli can't see it."

2. **Run coop with tracing overnight**: Deploy coop with `COOP_TRACE_FILE=traces.jsonl` and let it run through a night+morning cycle. Analyze the traces when the morning brief fires to see the exact send path, websocket state, and any errors.

3. **Clarify the recipient UUID**: Verify whether `eedf560a-1201-4cde-a863-4e5f82142ebf` and `218054c4-7cc5-4854-ab1d-a3343e331b31` are the same person (ACI vs PNI) or different accounts.

### Deeper investigation

4. **Test iptables against coop's long-running process**: Instead of the standalone binary, run coop, then block/unblock Signal traffic while coop is running. This actually tests the persistent websocket recovery path.

5. **Fix the 422 error**: Investigate what fields in presage's `set_account_attributes` call are rejected by the server. This may require updating the vendor code or the registration data.

6. **Add unidentified websocket probe**: The `whoami()` probe only covers the identified websocket. Consider adding a health check for the unidentified websocket, or patching `try_send_message` to treat `WsClosing`/IO errors like `Unauthorized` (fall back to identified sender).

7. **Test with a fresh linked device on recipient**: Link a new presage instance as a device on the recipient's account to rule out signal-cli-specific issues.

## File Index

| Path | Description |
|------|-------------|
| `crates/coop-channels/src/signal.rs` | Coop's Signal channel (send_task, receive_task, whoami probe) |
| `vendor/presage/` | Vendored presage with websocket creation tracing |
| `vendor/libsignal-service-rs/` | Vendored libsignal-service with send-path tracing |
| `crates/coop-gateway/src/tracing_setup.rs` | Tracing filter config (console + JSONL) |
| `tests/signal-delivery/` | Standalone test binary and iptables test script |
| `tests/multidevice-delivery-test.sh` | signal-cli + presage delivery comparison test |
| `tests/signal-delivery/prompt-a-enhanced-tracing.md` | Original tracing instrumentation plan |
| `tests/signal-delivery/prompt-b-iptables-simulation.md` | Original iptables test plan |
