# Signal Rich Messages

Implement rich inbound context and outbound Signal actions for the Signal channel. This has three parts: (1) enriching inbound messages so the agent sees full context, (2) exposing Signal-specific outbound actions as tools, and (3) gateway-triggered typing indicators.

Read `AGENTS.md` at the project root before starting. Follow the development loop and code quality rules.

## Background

The Signal channel (`crates/coop-channels/src/signal.rs`) currently extracts only the plain text body from incoming messages and sends only plain text back. Rich Signal features — replies (quotes), emoji reactions, attachments, link previews, typing indicators, read receipts, edits, and deletes — are all silently dropped.

The presage library delivers all these as variants of `ContentBody` or as fields on `DataMessage`. The current `inbound_from_content()` function discards everything except `DataMessage.body`. The current `send_outbound_message()` constructs a bare `DataMessage { body: Some(text) }`.

The agent is an LLM. It reads text and calls tools. The design principle is:
- **Inbound**: the channel renders rich context into readable text for the LLM
- **Outbound**: channel-specific actions are exposed as tools the LLM can call
- **Typing**: managed automatically by the gateway, not the LLM

## Part 1: Enrich inbound messages

### 1a. Add structured fields to `InboundMessage`

In `crates/coop-core/src/types.rs`, add fields to `InboundMessage`:

```rust
pub struct InboundMessage {
    // existing fields unchanged
    pub channel: String,
    pub sender: String,
    pub content: String,
    pub chat_id: Option<String>,
    pub is_group: bool,
    pub timestamp: DateTime<Utc>,
    pub reply_to: Option<String>,

    // new fields
    /// What kind of inbound event this is.
    #[serde(default)]
    pub kind: InboundKind,
    /// Epoch millis timestamp of the original message (for reactions, replies, etc.)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message_timestamp: Option<u64>,
}
```

Add the `InboundKind` enum:

```rust
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InboundKind {
    #[default]
    Text,
    Reaction,
    Typing,
    Receipt,
    Edit,
    Attachment,
}
```

Every existing construction of `InboundMessage` (in `signal.rs`, `main.rs`, `router.rs` tests, `fakes.rs`) must be updated. The `kind` field defaults to `Text` and `message_timestamp` to `None`, so most sites just need the field present. Use `..Default::default()` or add the fields explicitly — whichever is cleaner. `InboundMessage` will need a `Default` impl or the new fields need defaults.

### 1b. Enrich `inbound_from_content()` in `signal.rs`

Replace the current function with one that handles multiple `ContentBody` variants and formats rich context into the `content` string. The function should handle:

**Replies (quotes):** When `DataMessage` has a `quote` field, prepend context:
```
[reply to "{quoted_text}" (at {timestamp})]
{body}
```

**Reactions:** When `DataMessage` has a `reaction` field, format as:
```
[reacted {emoji} to message at {target_timestamp}]
```
Set `kind: InboundKind::Reaction`. Note: reactions have no `body`, so the current code drops them because it calls `body.as_deref()?`. The reaction text IS the formatted content.

**Attachments:** When `DataMessage` has attachments, append metadata:
```
{body}
[attachment: {filename} ({content_type}, {size} bytes)]
```
Set `kind: InboundKind::Attachment` if no text body is present, or leave as `Text` if text accompanies the attachment. Do NOT download attachment data at this stage — just include the metadata from `AttachmentPointer` fields (`file_name`, `content_type`, `size`).

**Link previews:** When `DataMessage` has `preview` entries, append:
```
{body}
[link: {url} — "{title}"]
```

**Edits:** Handle `ContentBody::EditMessage` — extract the inner `data_message`, format the body, set `kind: InboundKind::Edit`:
```
[edited message at {target_timestamp}]
{new_body}
```

**Typing:** Handle `ContentBody::TypingMessage` — set `kind: InboundKind::Typing`, content as empty string. These will be filtered by the gateway (not sent to the agent).

**Receipts:** Handle `ContentBody::ReceiptMessage` — set `kind: InboundKind::Receipt`, content describing the receipt type. These will be filtered by the gateway.

The `extract_supported_data_message` function needs to be replaced or expanded. The new `inbound_from_content` should match on the full `ContentBody` enum.

Set `message_timestamp` from `content.metadata.timestamp` on all inbound messages — this is the identifier the agent needs to reference messages in tool calls.

### 1c. Filter non-actionable events in the gateway

In the signal loop in `crates/coop-gateway/src/main.rs` (`run_signal_loop`), filter out typing and receipt events before dispatching to the router:

```rust
let inbound = coop_core::Channel::recv(&mut signal_channel).await?;
match inbound.kind {
    InboundKind::Typing | InboundKind::Receipt => continue,
    _ => {}
}
```

This prevents typing indicators and read receipts from triggering agent turns.

## Part 2: Signal outbound tools

### 2a. Add a `SignalAction` enum in `signal.rs`

Replace the `mpsc::Sender<OutboundMessage>` with `mpsc::Sender<SignalAction>`:

```rust
enum SignalAction {
    SendText(OutboundMessage),
    React {
        target: SignalTarget,
        emoji: String,
        target_sent_timestamp: u64,
    },
    Reply {
        target: SignalTarget,
        text: String,
        quote_timestamp: u64,
    },
    Typing {
        target: SignalTarget,
        started: bool,
    },
}
```

### 2b. Update the send task

Expand `send_task` to handle all `SignalAction` variants. Each variant constructs the appropriate presage protobuf:

**`SendText`**: Same as current `send_outbound_message` — builds `DataMessage { body: Some(text) }`.

**`React`**: Builds a `DataMessage` with a `Reaction` field. The `target_author_aci` must be looked up. For now, since presage doesn't expose a direct message-by-timestamp lookup on the manager, store the sender UUID on inbound messages and pass it through. Alternatively, the tool can require the author UUID as a parameter (the agent sees it in the formatted inbound context). Start with requiring it as a parameter:

```rust
DataMessage {
    reaction: Some(data_message::Reaction {
        emoji: Some(emoji),
        remove: Some(false),
        target_author_aci: Some(target_author_aci),
        target_sent_timestamp: Some(target_sent_timestamp),
    }),
    ..Default::default()
}
```

Update the `SignalAction::React` variant to include `target_author_aci: String`.

**`Reply`**: Builds a `DataMessage` with `body` and a `Quote`. The quote needs `id` (timestamp), `author_aci`, and `text`. Like reactions, start by requiring the author UUID as a parameter. The quoted text is optional (Signal will fill it in on the recipient's side if omitted):

```rust
DataMessage {
    body: Some(text),
    quote: Some(data_message::Quote {
        id: Some(quote_timestamp),
        author_aci: Some(quote_author_aci),
        text: None, // recipient's client resolves this
        ..Default::default()
    }),
    ..Default::default()
}
```

Update the `SignalAction::Reply` variant to include `quote_author_aci: String`.

**`Typing`**: Builds a `TypingMessage`:

```rust
TypingMessage {
    timestamp: Some(now_epoch_millis()),
    action: Some(if started {
        typing_message::Action::Started.into()
    } else {
        typing_message::Action::Stopped.into()
    }),
    group_id: match &target {
        SignalTarget::Group { master_key } => Some(master_key.clone()),
        _ => None,
    },
}
```

Typing messages are sent via `manager.send_message()` with the `TypingMessage` as the content body, targeting the recipient or group.

### 2c. Expose an action sender

The `SignalChannel` needs to expose a way to send `SignalAction`s. Add:

```rust
pub struct SignalChannel {
    id: String,
    inbound_rx: mpsc::Receiver<InboundMessage>,
    outbound_tx: mpsc::Sender<OutboundMessage>,  // keep for Channel::send()
    action_tx: mpsc::Sender<SignalAction>,        // new: for tools and gateway
    health: HealthState,
}
```

The `Channel::send()` implementation wraps `OutboundMessage` into `SignalAction::SendText` and sends it through `action_tx`. Remove the separate `outbound_tx` — unify everything through `action_tx`.

Add a public method for sending actions:

```rust
impl SignalChannel {
    pub fn action_sender(&self) -> mpsc::Sender<SignalAction> {
        self.action_tx.clone()
    }
}
```

Make `SignalAction` public so the gateway can construct typing actions.

### 2d. Implement Signal tools

Create `crates/coop-channels/src/signal_tools.rs` (new file) with tool implementations. Each tool holds an `mpsc::Sender<SignalAction>` and implements `coop_core::Tool`.

**`SignalReactTool`**: Sends `SignalAction::React`.

Tool definition:
```
name: "signal_react"
description: "React to a Signal message with an emoji"
parameters: {
    chat_id: string (required) — "Chat identifier, e.g. a UUID for DMs or group:hex for groups"
    emoji: string (required) — "Emoji to react with"
    message_timestamp: integer (required) — "Timestamp of the message to react to"
    author_id: string (required) — "UUID of the message author"
    remove: boolean (optional, default false) — "Remove the reaction instead of adding"
}
```

**`SignalReplyTool`**: Sends `SignalAction::Reply`.

Tool definition:
```
name: "signal_reply"
description: "Reply to a specific Signal message (shows as a quote)"
parameters: {
    chat_id: string (required) — "Chat identifier"
    text: string (required) — "Reply text"
    reply_to_timestamp: integer (required) — "Timestamp of the message to reply to"
    author_id: string (required) — "UUID of the message author being replied to"
}
```

Each tool's `execute` method:
1. Parses arguments from `serde_json::Value`
2. Calls `SignalTarget::parse(chat_id)?` to get the target
3. Constructs the appropriate `SignalAction` variant
4. Sends it via the `mpsc::Sender<SignalAction>`
5. Returns `ToolOutput::success("reaction sent")` or `ToolOutput::success("reply sent")`

The tools need `#[async_trait]` and must be `Send + Sync`. The `mpsc::Sender` is `Clone + Send + Sync`, so holding it in the struct is fine.

### 2e. Register Signal tools in the gateway

The gateway needs access to Signal tools when the Signal channel is active. In `crates/coop-gateway/src/main.rs`, when the Signal channel connects successfully:

1. Get the `action_sender()` from `SignalChannel`
2. Create the Signal tools with that sender
3. Create a composite `ToolExecutor` that includes both the `DefaultExecutor` tools and the Signal tools

This requires a way to compose executors. Add a `CompositeExecutor` to `crates/coop-core/src/tools/mod.rs`:

```rust
pub struct CompositeExecutor {
    executors: Vec<Box<dyn ToolExecutor>>,
}

impl CompositeExecutor {
    pub fn new(executors: Vec<Box<dyn ToolExecutor>>) -> Self {
        Self { executors }
    }
}

#[async_trait]
impl ToolExecutor for CompositeExecutor {
    async fn execute(&self, name: &str, arguments: serde_json::Value, ctx: &ToolContext) -> Result<ToolOutput> {
        for executor in &self.executors {
            if executor.tools().iter().any(|t| t.name == name) {
                return executor.execute(name, arguments, ctx).await;
            }
        }
        Ok(ToolOutput::error(format!("unknown tool: {name}")))
    }

    fn tools(&self) -> Vec<ToolDef> {
        self.executors.iter().flat_map(|e| e.tools()).collect()
    }
}
```

In `cmd_start`, compose the executors:

```rust
let default_executor = DefaultExecutor::new();
let executor: Arc<dyn ToolExecutor> = if signal_channel_available {
    let signal_executor = signal_tools::SignalToolExecutor::new(action_tx);
    Arc::new(CompositeExecutor::new(vec![
        Box::new(default_executor),
        Box::new(signal_executor),
    ]))
} else {
    Arc::new(default_executor)
};
```

Create a `SignalToolExecutor` in `signal_tools.rs` that implements `ToolExecutor` and holds all the Signal tools.

### 2f. Include sender identity in inbound context

The agent needs to know author UUIDs to use `signal_react` and `signal_reply` tools. Update the formatted `content` in `inbound_from_content()` to include the sender UUID:

```
[from {sender_uuid} at {timestamp}]
{body}
```

For group messages, also include the group context:

```
[from {sender_uuid} in group:{group_hex} at {timestamp}]
{body}
```

This gives the agent the `author_id` and `message_timestamp` values it needs for tool calls.

## Part 3: Gateway-triggered typing

### 3a. Pass a typing callback to the gateway

The gateway needs to send typing indicators when an agent turn starts and stops. Since the gateway doesn't know about Signal-specific types, use a trait object callback.

Add to `crates/coop-core/src/traits.rs`:

```rust
/// Callback for sending typing indicators on a channel.
#[async_trait]
pub trait TypingNotifier: Send + Sync {
    /// Send a typing started/stopped indicator for the given session.
    async fn set_typing(&self, session_key: &SessionKey, started: bool);
}
```

### 3b. Implement `TypingNotifier` for Signal

In `crates/coop-channels/src/signal.rs`, implement a `SignalTypingNotifier` that holds the `mpsc::Sender<SignalAction>` and maps `SessionKey` back to a `SignalTarget`:

```rust
pub struct SignalTypingNotifier {
    action_tx: mpsc::Sender<SignalAction>,
}

#[async_trait]
impl TypingNotifier for SignalTypingNotifier {
    async fn set_typing(&self, session_key: &SessionKey, started: bool) {
        let target = match &session_key.kind {
            SessionKind::Dm(identity) => {
                // identity is "signal:{uuid}" — extract the UUID
                let uuid_str = identity.strip_prefix("signal:").unwrap_or(identity);
                match SignalTarget::parse(uuid_str) {
                    Ok(t) => t,
                    Err(_) => return,
                }
            }
            SessionKind::Group(group_id) => {
                // group_id is "signal:group:{hex}" — extract the group part
                let group_part = group_id.strip_prefix("signal:").unwrap_or(group_id);
                match SignalTarget::parse(group_part) {
                    Ok(t) => t,
                    Err(_) => return,
                }
            }
            _ => return, // Main/Isolated sessions don't have a Signal target
        };

        let _ = self.action_tx.send(SignalAction::Typing { target, started }).await;
    }
}
```

### 3c. Wire typing into the gateway

Add an optional `typing_notifier` to `Gateway`:

```rust
pub(crate) struct Gateway {
    // ... existing fields ...
    typing_notifier: Option<Arc<dyn TypingNotifier>>,
}
```

Add a setter or constructor parameter. In `run_turn_with_trust`, send typing started at the beginning and typing stopped at the end:

```rust
pub(crate) async fn run_turn_with_trust(&self, session_key: &SessionKey, ...) -> Result<()> {
    // Send typing started
    if let Some(notifier) = &self.typing_notifier {
        notifier.set_typing(session_key, true).await;
    }

    // ... existing turn logic ...

    // Send typing stopped (in all exit paths — use a guard or finally block)
    if let Some(notifier) = &self.typing_notifier {
        notifier.set_typing(session_key, false).await;
    }

    Ok(())
}
```

Use a drop guard or `scopeguard` pattern to ensure typing stopped is sent even on errors. A simple approach:

```rust
struct TypingGuard {
    notifier: Arc<dyn TypingNotifier>,
    session_key: SessionKey,
}

impl Drop for TypingGuard {
    fn drop(&mut self) {
        let notifier = self.notifier.clone();
        let session_key = self.session_key.clone();
        tokio::spawn(async move {
            notifier.set_typing(&session_key, false).await;
        });
    }
}
```

Create the guard after sending typing started. It will send typing stopped when dropped, regardless of how the function exits.

### 3d. Connect in `cmd_start`

When creating the gateway in `cmd_start`, pass the `SignalTypingNotifier` if the Signal channel is active:

```rust
let typing_notifier: Option<Arc<dyn TypingNotifier>> = if signal_channel_available {
    Some(Arc::new(SignalTypingNotifier::new(action_tx.clone())))
} else {
    None
};

let gateway = Arc::new(Gateway::new(config, system_prompt, provider, executor, typing_notifier));
```

Update the `Gateway::new` signature and the `cmd_chat` call (which passes `None` for typing since there's no Signal channel in chat mode).

## Implementation order

1. **`InboundKind` + `InboundMessage` fields** in `coop-core/src/types.rs` — update all construction sites
2. **Enrich `inbound_from_content()`** in `signal.rs` — handle all content types
3. **Filter in `run_signal_loop`** — skip typing/receipts
4. **`SignalAction` enum** in `signal.rs` — replace `OutboundMessage` channel
5. **Update `send_task`** to handle all action variants
6. **`SignalChannel::action_sender()`** — expose the sender
7. **`CompositeExecutor`** in `coop-core/src/tools/mod.rs`
8. **Signal tools** in `crates/coop-channels/src/signal_tools.rs`
9. **`TypingNotifier` trait** in `coop-core/src/traits.rs`
10. **`SignalTypingNotifier`** in `signal.rs`
11. **Gateway typing integration** — notifier field, guard, wiring
12. **Wire everything in `cmd_start`** — compose executors, pass typing notifier

## Required imports in `signal.rs`

The enriched inbound handling needs these additional presage imports:

```rust
use presage::libsignal_service::content::ContentBody;
use presage::proto::{TypingMessage, ReceiptMessage, EditMessage};
use presage::proto::data_message::{Quote, Reaction};
use presage::proto::typing_message;
```

## Tests

Add tests in each crate:

**`coop-core`**: Test `InboundKind` default, serialization roundtrip of `InboundMessage` with new fields, `CompositeExecutor` routing.

**`coop-channels`**: Test `inbound_from_content` with constructed `Content` objects for each variant (reaction, quote, attachment metadata, edit, typing, receipt). Test `SignalTarget::parse` still works. Test `SignalAction` construction in tool execute methods (mock the sender, verify the action sent).

**`coop-gateway`**: Test that `run_signal_loop` filtering skips `InboundKind::Typing` and `InboundKind::Receipt`. Test that router still routes enriched messages correctly. Test typing guard sends stop on drop.

Use fakes from `coop-core/src/fakes.rs` for trait boundaries. Use placeholder data per `AGENTS.md` rules.

## What this does NOT include

- **Attachment downloading**: Attachments are metadata-only on inbound. Downloading and forwarding binary data (for vision models, etc.) is a separate feature.
- **Sticker rendering**: Sticker metadata could be included as `[sticker: {emoji}]` but is not in scope.
- **Read receipt sending**: Auto-sending read receipts is a future config-driven feature.
- **Message editing/deleting tools**: `signal_edit` and `signal_delete` tools can be added later following the same pattern as `signal_react` and `signal_reply`.
- **Remove reaction**: The `signal_react` tool supports a `remove` parameter but the initial implementation can default to `false` and add removal support trivially.

## Verification

After implementation:

```bash
cargo fmt
cargo build --features signal
cargo test -p coop-core
cargo test -p coop-channels --features signal
cargo test -p coop-gateway --features signal
cargo clippy --all-targets --all-features -- -D warnings
```

Manually verify with `COOP_TRACE_FILE=traces.jsonl`:

Implement a mechanism to stub the send and response for signal then the
following can be verified:

1. Receive a reply in Signal — trace should show formatted quote context in the inbound content
2. Receive a reaction — trace should show formatted reaction, `kind: reaction`
3. Agent calls `signal_react` tool — trace should show tool execution and action sent
4. Agent turn starts — trace should show typing started event
5. Agent turn ends — trace should show typing stopped event
