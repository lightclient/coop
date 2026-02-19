# Prompt A: Enhanced Presage Tracing for Signal Delivery Diagnosis

## Goal

Patch presage (local git checkout) to add detailed tracing around the message send path, then run coop overnight to capture the actual failure when the morning brief fires. The traces should tell us exactly why messages aren't reaching all recipient devices.

## Background

Coop sends messages via presage (a Rust Signal client library). Messages intermittently fail to reach all of the recipient's linked devices — worst on phones, worst in the morning after hours of idle. We've analyzed the code exhaustively but can't find a single smoking gun. We need to observe the actual failure with detailed instrumentation.

Presage is consumed as a git dependency:
```toml
# crates/coop-channels/Cargo.toml
presage = { git = "https://github.com/whisperfish/presage", rev = "1b81dfe", optional = true }
presage-store-sqlite = { git = "https://github.com/whisperfish/presage", rev = "1b81dfe", optional = true }
```

The local checkout lives at:
```
/root/.cargo/git/checkouts/presage-ce211e77a0d9397d/1b81dfe/
```

And libsignal-service-rs (presage's dependency) at:
```
/root/.cargo/git/checkouts/libsignal-service-rs-7e457f3dfb9f3190/aebf607/
```

## Step 1: Patch libsignal-service-rs sender.rs

File: `/root/.cargo/git/checkouts/libsignal-service-rs-7e457f3dfb9f3190/aebf607/src/sender.rs`

### 1a. Trace device enumeration in `create_encrypted_messages` (~line 862)

After the `devices` HashSet is built (after the `devices.insert(*DEFAULT_DEVICE_ID)` and the self-removal), add:

```rust
tracing::info!(
    recipient = %recipient.service_id_string(),
    devices = ?devices.iter().map(|d| d.to_string()).collect::<Vec<_>>(),
    device_count = devices.len(),
    "encrypting message for recipient devices"
);
```

### 1b. Trace each per-device encryption in the for loop (~line 896)

The existing `trace!("sending message to device {}", device_id)` should be upgraded to `info!`:

```rust
tracing::info!(
    recipient = %recipient.service_id_string(),
    device_id = %device_id,
    "encrypting message for device"
);
```

### 1c. Trace the send path choice in `try_send_message` (~line 600)

The existing `tracing::debug!("sending via unidentified")` and `tracing::debug!("sending identified")` should be upgraded to `info!` and include more context:

```rust
let send = if let Some(unidentified) = &unidentified_access {
    tracing::info!(
        recipient = %recipient.service_id_string(),
        message_count = messages.messages.len(),
        devices = ?messages.messages.iter().map(|m| m.destination_device_id).collect::<Vec<_>>(),
        "sending via unidentified websocket (sealed sender)"
    );
    self.unidentified_ws
        .send_messages_unidentified(messages, unidentified)
        .await
} else {
    tracing::info!(
        recipient = %recipient.service_id_string(),
        message_count = messages.messages.len(),
        devices = ?messages.messages.iter().map(|m| m.destination_device_id).collect::<Vec<_>>(),
        "sending via identified websocket"
    );
    self.identified_ws.send_messages(messages).await
};
```

### 1d. Trace the server response and all error branches (~line 612+)

After `Ok(SendMessageResponse { needs_sync })`:

```rust
Ok(SendMessageResponse { needs_sync }) => {
    tracing::info!(
        recipient = %recipient.service_id_string(),
        needs_sync,
        unidentified = unidentified_access.is_some(),
        "message accepted by server"
    );
    return Ok(SentMessage { ... });
},
```

For `MismatchedDevicesException`:

```rust
Err(ServiceError::MismatchedDevicesException(ref m)) => {
    tracing::warn!(
        recipient = %recipient.service_id_string(),
        extra_devices = ?m.extra_devices,
        missing_devices = ?m.missing_devices,
        "server reported mismatched devices — will retry"
    );
    // ... existing handling
},
```

For `StaleDevices`:

```rust
Err(ServiceError::StaleDevices(ref m)) => {
    tracing::warn!(
        recipient = %recipient.service_id_string(),
        stale_devices = ?m.stale_devices,
        "server reported stale devices — will retry"
    );
    // ... existing handling
},
```

For the default error case:

```rust
Err(e) => {
    tracing::error!(
        recipient = %recipient.service_id_string(),
        error = %e,
        unidentified = unidentified_access.is_some(),
        "send failed with unhandled error"
    );
    return Err(MessageSenderError::ServiceError(e));
},
```

For the `Unauthorized` fallback:

```rust
Err(ServiceError::Unauthorized) if unidentified_access.is_some() => {
    tracing::warn!(
        recipient = %recipient.service_id_string(),
        "sealed sender unauthorized — falling back to identified sender"
    );
    unidentified_access = None;
},
```

## Step 2: Patch libsignal-service-rs websocket/mod.rs

File: `/root/.cargo/git/checkouts/libsignal-service-rs-7e457f3dfb9f3190/aebf607/src/websocket/mod.rs`

### 2a. Trace websocket creation

In `SignalWebSocket::new()` (~line 351), add after creating the struct:

```rust
tracing::info!("websocket created with keepalive path: {}", keep_alive_path);
```

### 2b. Trace keepalive failures more prominently

The existing `tracing::warn!("Websocket will be closed due to failed keepalives.")` is good. Also add the websocket type info if possible.

## Step 3: Patch libsignal-service-rs websocket/sender.rs

File: `/root/.cargo/git/checkouts/libsignal-service-rs-7e457f3dfb9f3190/aebf607/src/websocket/sender.rs`

### 3a. Trace the actual HTTP-over-websocket send

In both `send_messages` and `send_messages_unidentified`, add tracing:

```rust
pub async fn send_messages(
    &mut self,
    messages: OutgoingPushMessages,
) -> Result<SendMessageResponse, ServiceError> {
    tracing::info!(
        destination = %messages.destination.service_id_string(),
        message_count = messages.messages.len(),
        online = messages.online,
        ws_closed = self.is_closed(),
        "sending messages via identified websocket"
    );
    let request = WebSocketRequestMessage::new(Method::PUT)
        .path(format!("/v1/messages/{}", messages.destination.service_id_string()))
        .json(&messages)?;
    let result = self.request_json(request).await;
    match &result {
        Ok(resp) => tracing::info!(?resp, "send_messages response"),
        Err(e) => tracing::error!(%e, "send_messages failed"),
    }
    result
}
```

Same for `send_messages_unidentified`.

## Step 4: Patch presage registered.rs

File: `/root/.cargo/git/checkouts/presage-ce211e77a0d9397d/1b81dfe/presage/src/manager/registered.rs`

### 4a. Trace websocket reuse vs creation

In `identified_websocket()` (~line 218) and `unidentified_websocket()` (~line 251), trace whether we're reusing or creating:

```rust
// In identified_websocket():
match identified_ws.as_ref().filter(|ws| !ws.is_closed()).filter(|ws| !(require_unused && ws.is_used())) {
    Some(ws) => {
        tracing::debug!("reusing existing identified websocket");
        Ok(ws.clone())
    },
    None => {
        tracing::info!("creating new identified websocket (previous was closed or absent)");
        // ... existing creation code
    }
}

// In unidentified_websocket():
match unidentified_ws.as_ref().filter(|ws| !ws.is_closed()) {
    Some(ws) => {
        tracing::debug!("reusing existing unidentified websocket");
        Ok(ws.clone())
    },
    None => {
        tracing::info!("creating new unidentified websocket (previous was closed or absent)");
        // ... existing creation code
    }
}
```

## Step 5: Update coop tracing filter

File: `crates/coop-gateway/src/tracing_setup.rs`

Ensure the tracing filter allows `info` level for `libsignal_service::sender` and `libsignal_service::websocket::sender`. The current filter from commit 8d5fae6 already has `libsignal_service::sender=debug` which should capture `info!` events. Verify both the console and JSONL filters include these modules.

## Step 6: Build and verify

```bash
cd /root/coop/main
cargo build 2>&1
```

If there are compile errors from the patches, fix them. The patches modify files in the cargo git checkout, so cargo should detect them and recompile.

Verify the tracing works:

```bash
COOP_TRACE_FILE=traces.jsonl cargo run -- --config coop.toml &
sleep 10
# Send a test message via signal-cli to trigger a response
signal-cli -u +17205818516 send -m "tracing test" +17204279035
sleep 30
kill %1
# Check traces for the new instrumentation
grep "encrypting message for\|sending via.*websocket\|message accepted\|send_messages" traces.jsonl
```

You should see lines like:
- `"encrypting message for recipient devices"` with device list
- `"sending via unidentified websocket (sealed sender)"` or `"sending via identified websocket"`
- `"message accepted by server"` with needs_sync flag

## Step 7: Deploy overnight

Run coop with tracing enabled and let it run overnight. The morning brief (or any scheduled send) will be captured with full detail.

```bash
COOP_TRACE_FILE=traces.jsonl cargo run --release -- --config coop.toml
```

## Step 8: Analyze the failure

After a failed delivery, search the traces:

```bash
# Find all send attempts
grep "encrypting message for\|sending via\|message accepted\|send failed\|mismatched devices\|sealed sender unauthorized\|websocket.*closed\|creating new.*websocket" traces.jsonl

# Find errors
grep '"level":"ERROR"\|"level":"WARN"' traces.jsonl

# Find the timeline around the morning brief
grep "cron\|brief\|send_text\|encrypting\|accepted\|failed" traces.jsonl
```

The traces should reveal:
- Whether the send succeeded or failed from presage's perspective
- Which websocket was used (identified vs unidentified)
- Which devices were encrypted for
- Whether the server reported any device mismatches
- Whether any websocket needed to be recreated

## What to look for

1. **Send succeeds but phone doesn't get it** → Problem is server-side delivery or phone-side reception (push notification, decryption). We'd need to check the phone's Signal app logs.

2. **Send fails with WsClosing** → Dead websocket wasn't detected by keepalive. Check the websocket creation/reuse trace to see if a stale socket was used.

3. **Send fails with Unauthorized, falls back to identified, then succeeds** → Sealed sender certificate expired or was rejected. The identified fallback should work but this is worth noting.

4. **MismatchedDevices on every send** → Session store is out of sync with server. The retry should fix it, but frequent mismatches indicate a deeper problem.

5. **Send succeeds for fewer devices than expected** → `create_encrypted_messages` didn't enumerate all devices. Check the "encrypting message for recipient devices" trace for the device list.
