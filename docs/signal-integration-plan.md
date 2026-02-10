# Signal Integration Plan

## Overview

Add Signal messaging support to Coop via [presage](https://github.com/whisperfish/presage) (a Rust Signal client library), and build a gateway daemon with an IPC protocol that lets any client — TUI, Signal, future channels — interact with agent sessions over a uniform interface.

---

## Current State

Today Coop has:

- A `Channel` trait in `coop-core/src/traits.rs` with `recv()`, `send()`, and `probe()`
- A `TerminalChannel` in `coop-channels/` that bridges mpsc channels between the TUI and gateway
- A `Gateway` in `coop-gateway/` that owns sessions and runs agent turns
- A `route_message()` function in `coop-gateway/src/router.rs` that resolves `InboundMessage → (SessionKey, TrustLevel)`
- A TUI `cmd_chat` function that creates the gateway in-process and talks to it directly

**Problem:** The TUI and gateway are tightly coupled in a single process. This means:
- Can't run the gateway as a daemon and attach/detach TUI sessions
- Can't have Signal and TUI running simultaneously without both being in-process
- Can't restart the TUI without killing agent sessions

---

## Architecture Goal

The gateway runs as a long-lived daemon. All clients — TUI, Signal, future channels — connect to it. Signal is an internal channel (presage runs inside the gateway process). The TUI is an external client that connects over IPC.

```
┌─────────────────────────────────────────────────────┐
│               Gateway Daemon (coop start)            │
│                                                       │
│  ┌──────────────┐   ┌──────────┐   ┌──────────────┐ │
│  │Signal Channel │   │  Router  │   │   Sessions   │ │
│  │  (presage)    │──▶│          │──▶│ (per-sender)  │ │
│  └──────────────┘   │          │   │              │ │
│                      │ identify │   │  Gateway     │ │
│  ┌──────────────┐   │ trust    │   │  run_turn()  │ │
│  │  IPC Server  │──▶│ dispatch │   │              │ │
│  │ (Unix socket) │   └──────────┘   └──────────────┘ │
│  └──────┬───────┘                                     │
│         │                                             │
└─────────┼─────────────────────────────────────────────┘
          │
    Unix domain socket
          │
┌─────────┴───────┐
│  TUI Client     │  (coop chat — separate process)
│  connects, sends │
│  input, receives │
│  streaming events│
└─────────────────┘
```

**Key distinction:**
- **Signal** = internal channel. Presage runs inside the gateway daemon, maintaining its own websocket to Signal servers. The gateway owns its lifecycle.
- **TUI** = external client. Connects to the gateway over a Unix domain socket. Can attach/detach without affecting sessions.

---

## Implementation Phases

### Phase 1: IPC Protocol & Gateway Daemon

**Goal:** Define the IPC protocol, split the gateway into a daemon process, and make the TUI connect as a client.

#### 1a. IPC Transport

Unix domain socket at a well-known path:
```
$XDG_RUNTIME_DIR/coop/{agent-id}.sock
# fallback: /tmp/coop-{agent-id}.sock
```

The gateway creates and listens on this socket when `coop start` runs. The TUI connects to it when `coop chat` runs.

#### 1b. Wire Protocol

Newline-delimited JSON (ndjson) over the Unix socket. Each line is a self-contained JSON message. Simple, debuggable, no extra dependencies.

**Client → Gateway:**

```jsonl
{"type":"send","session":"main","content":"hello world"}
{"type":"clear","session":"main"}
{"type":"list_sessions"}
{"type":"subscribe","session":"main"}
```

| Message | Description |
|---|---|
| `send` | Send user input to a session. Gateway runs a turn and streams events back. |
| `clear` | Clear a session's history. |
| `list_sessions` | List active session keys. |
| `subscribe` | Start receiving streaming events for a session (text deltas, tool calls, etc.). A client subscribes on connect. |

**Gateway → Client:**

```jsonl
{"type":"text_delta","session":"main","text":"Hello! How can"}
{"type":"tool_start","session":"main","id":"call_1","name":"bash","arguments":{"command":"ls"}}
{"type":"tool_result","session":"main","id":"call_1","output":"file.txt\n","is_error":false}
{"type":"assistant_message","session":"main","text":"Here are the files..."}
{"type":"done","session":"main","tokens":350,"hit_limit":false}
{"type":"error","session":"main","message":"provider timeout"}
{"type":"sessions","keys":["reid:main","reid:dm:signal:abc-123"]}
```

These map directly to the existing `TurnEvent` enum:

| TurnEvent | IPC message |
|---|---|
| `TextDelta(text)` | `{"type":"text_delta","text":"..."}` |
| `AssistantMessage(msg)` | `{"type":"assistant_message","text":"..."}` |
| `ToolStart{id,name,args}` | `{"type":"tool_start",...}` |
| `ToolResult{id,message}` | `{"type":"tool_result",...}` |
| `Done(result)` | `{"type":"done","tokens":N}` |
| `Error(msg)` | `{"type":"error","message":"..."}` |

#### 1c. New Crate: `coop-ipc`

Create `crates/coop-ipc/` with:

```
crates/coop-ipc/
├── Cargo.toml
└── src/
    ├── lib.rs
    ├── protocol.rs   # Message types (serde), shared between server & client
    ├── server.rs     # IpcServer: listens on socket, accepts connections
    └── client.rs     # IpcClient: connects to socket, send/receive
```

**`protocol.rs`** — shared types:

```rust
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientMessage {
    Send { session: String, content: String },
    Clear { session: String },
    ListSessions,
    Subscribe { session: String },
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerMessage {
    TextDelta { session: String, text: String },
    ToolStart { session: String, id: String, name: String, arguments: Value },
    ToolResult { session: String, id: String, output: String, is_error: bool },
    AssistantMessage { session: String, text: String },
    Done { session: String, tokens: u32, hit_limit: bool },
    Error { session: String, message: String },
    Sessions { keys: Vec<String> },
}
```

**`server.rs`** — runs inside the gateway daemon:

```rust
pub struct IpcServer {
    socket_path: PathBuf,
    gateway: Arc<Gateway>,
    config: Config,
}

impl IpcServer {
    pub async fn run(&self) -> Result<()> {
        let listener = UnixListener::bind(&self.socket_path)?;
        loop {
            let (stream, _) = listener.accept().await?;
            let gw = self.gateway.clone();
            let cfg = self.config.clone();
            tokio::spawn(handle_client(stream, gw, cfg));
        }
    }
}
```

Each connected client gets a task that reads `ClientMessage` lines and dispatches them. When a `send` arrives, it calls `gateway.run_turn()` and forwards `TurnEvent`s as `ServerMessage` lines back to the client.

**`client.rs`** — used by the TUI:

```rust
pub struct IpcClient {
    reader: BufReader<OwnedReadHalf>,
    writer: OwnedWriteHalf,
}

impl IpcClient {
    pub async fn connect(socket_path: &Path) -> Result<Self> { ... }
    pub async fn send(&mut self, msg: ClientMessage) -> Result<()> { ... }
    pub async fn recv(&mut self) -> Result<ServerMessage> { ... }
}
```

#### 1d. Refactor `cmd_start` and `cmd_chat`

**`cmd_start`** becomes the real daemon:
1. Load config, build system prompt, create provider + executor
2. Create `Gateway`
3. Start `IpcServer` on Unix socket
4. Start Signal channel (if configured) — feeds into gateway via router
5. Write PID file / print socket path
6. Wait for shutdown signal

**`cmd_chat`** becomes a thin TUI client:
1. Connect `IpcClient` to the gateway's socket
2. Subscribe to the main session
3. Run the existing TUI event loop, but instead of calling `gateway.run_turn()` directly, send `ClientMessage::Send` over IPC and receive `ServerMessage` events
4. Map `ServerMessage` variants to the existing `App` methods (`app.append_or_create_assistant()`, `app.push_message(DisplayMessage::tool_call(...))`, etc.)

The TUI code in `main.rs` changes from:
```rust
// Before: in-process
gw.run_turn(&sk, &input, tx.clone()).await
```
To:
```rust
// After: over IPC
ipc_client.send(ClientMessage::Send { session: "main".into(), content: input }).await
// Events arrive asynchronously via ipc_client.recv()
```

The `coop-tui` crate itself (App, input handling, UI rendering) is unchanged. Only the wiring in `main.rs` changes.

#### 1e. Connection lifecycle

- `coop chat` fails fast with a clear error if no gateway is running
- Multiple TUI clients can connect simultaneously (each subscribes to a session)
- If the TUI disconnects, the gateway keeps running, sessions are preserved
- If the gateway stops, the TUI shows "disconnected" and can retry

---

### Phase 2: Signal Channel (`coop-channels`)

**Goal:** Implement `SignalChannel` using presage, running inside the gateway daemon.

#### 2a. Add presage dependencies

In the workspace `Cargo.toml`:
```toml
[patch.crates-io]
curve25519-dalek = { git = 'https://github.com/signalapp/curve25519-dalek', tag = 'signal-curve25519-4.1.3' }
```

In `crates/coop-channels/Cargo.toml`, feature-gated:
```toml
[features]
default = []
signal = ["presage", "presage-store-sqlite"]

[dependencies]
presage = { git = "https://github.com/whisperfish/presage", optional = true }
presage-store-sqlite = { git = "https://github.com/whisperfish/presage", optional = true }
```

#### 2b. Implement `SignalChannel`

New file: `crates/coop-channels/src/signal.rs`

Wraps a presage `Manager<SqliteStore, Registered>`:

```rust
pub struct SignalChannel {
    manager: Manager<SqliteStore, Registered>,
    inbound_rx: mpsc::Receiver<InboundMessage>,
}
```

A background tokio task runs `manager.receive_messages()`, converts presage `Content` into Coop `InboundMessage`, and sends through the mpsc.

**Content → InboundMessage mapping:**

| presage | InboundMessage |
|---|---|
| `content.metadata.sender` (ServiceId → UUID) | `sender` |
| `Thread::Contact(uuid)` | `chat_id = None`, `is_group = false` |
| `Thread::Group(master_key)` | `chat_id = Some(hex(key))`, `is_group = true` |
| `DataMessage.body` | `content` |
| `content.metadata.timestamp` | `timestamp` (epoch ms → DateTime) |
| channel identifier | `channel = "signal"` |

**Outbound routing:**

When the gateway produces a response for a Signal session, the router needs to send it back through Signal. The `OutboundMessage.target` encodes the destination:

- `{uuid}` → `manager.send_message(uuid.into(), DataMessage { body, .. }, timestamp)`
- `group:{hex_master_key}` → `manager.send_message_to_group(key_bytes, DataMessage { body, .. }, timestamp)`

**Filtering:**

Only process `ContentBody::DataMessage` (direct messages) and `ContentBody::SynchronizeMessage` containing `DataMessage` (messages sent from other linked devices). Skip typing indicators, receipts, sticker syncs, group metadata updates, etc.

#### 2c. Device linking CLI

One-time setup before Signal can operate:

```
coop signal link --device-name "coop-agent"
```

This calls `Manager::link_secondary_device()`:
1. Generates a provisioning URL
2. Displays as QR code in terminal (via `qr2term` crate)
3. Waits for user to scan with Signal on phone
4. Stores registration data in SQLite

After linking, the gateway uses `Manager::load_registered()` on startup.

```toml
# coop.toml
channels:
  signal:
    db_path: ./db/signal.db
```

#### 2d. Reconnection

The presage websocket can drop. The background receive task:
1. Detects stream termination (`receive_messages()` returns `None`)
2. Logs the disconnection
3. Backs off exponentially, retries `receive_messages()`
4. Reports health via `Channel::probe()` → `Degraded` / `Healthy`

---

### Phase 3: Message Router

**Goal:** Route incoming messages from all sources (IPC clients, Signal) to the correct session.

#### 3a. Router as the central dispatch hub

The router sits between message sources and the gateway:

```rust
pub struct MessageRouter {
    config: Config,
    gateway: Arc<Gateway>,
}

impl MessageRouter {
    /// Route and dispatch an inbound message, streaming events to the callback.
    pub async fn dispatch(
        &self,
        msg: &InboundMessage,
        event_tx: mpsc::Sender<TurnEvent>,
    ) -> Result<()> {
        let decision = route_message(msg, &self.config);
        self.gateway.run_turn(&decision.session_key, &msg.content, event_tx).await
    }
}
```

Both the IPC server and the Signal channel feed messages through the router:

- **IPC client sends `Send`** → IPC server constructs `InboundMessage` → `router.dispatch()` → streams `TurnEvent`s back to client as `ServerMessage`
- **Signal message arrives** → `SignalChannel::recv()` → `InboundMessage` → `router.dispatch()` → collects full response → sends back via `SignalChannel::send()`

#### 3b. Session routing rules

Extend the existing `route_message()` to handle per-sender sessions:

```rust
let kind = if msg.is_group {
    SessionKind::Group(msg.chat_id.unwrap_or(msg.channel.clone()))
} else {
    match msg.channel.as_str() {
        "terminal:default" => SessionKind::Main,
        _ => SessionKind::Dm(format!("{}:{}", msg.channel, msg.sender)),
    }
};
```

Result:

| Source | Session |
|---|---|
| TUI (terminal) | `SessionKind::Main` |
| Signal DM from Alice | `SessionKind::Dm("signal:{alice-uuid}")` |
| Signal DM from Bob | `SessionKind::Dm("signal:{bob-uuid}")` |
| Signal group | `SessionKind::Group("signal:group:{hex_key}")` |

Trust resolution uses the existing `route_message()` logic — match sender against `config.users[].match` patterns.

#### 3c. Reply routing

When the gateway produces a response, the router must send it back to the correct channel. Track which channel originated each session:

```rust
// In the IPC server handler:
let inbound = InboundMessage {
    channel: "terminal:default".into(),
    sender: "alice".into(), // from config or connection identity
    content: input,
    ..
};
router.dispatch(&inbound, event_tx).await;
// Stream TurnEvents back to this IPC client

// In the Signal channel loop:
let inbound = signal_channel.recv().await?;
let (event_tx, mut event_rx) = mpsc::channel(64);
router.dispatch(&inbound, event_tx).await;
// Collect full text from events, send via signal_channel.send()
```

For Signal, responses are collected into a single text message (no streaming). For TUI over IPC, events stream in real-time.

---

## Config Changes

```toml
# coop.toml additions

channels:
  signal:
    db_path: ./db/signal.db

users:
  - name: alice
    trust: full
    match: ['terminal:default', 'signal:{alice-uuid}']
  - name: bob
    trust: inner
    match: ['signal:{bob-uuid}']
```

---

## CLI Commands

```
coop start                                      # Run gateway daemon
coop chat                                       # Connect TUI to running gateway
coop signal link --device-name "coop-agent"     # One-time Signal device linking
coop signal unlink                              # Remove Signal registration
coop version                                    # Print version
```

---

## Dependency Notes

Presage is not on crates.io — git dependency:
```toml
presage = { git = "https://github.com/whisperfish/presage" }
presage-store-sqlite = { git = "https://github.com/whisperfish/presage" }
```

Workspace `[patch.crates-io]` required:
```toml
curve25519-dalek = { git = 'https://github.com/signalapp/curve25519-dalek', tag = 'signal-curve25519-4.1.3' }
```

Additional crates:
- `qr2term` — QR code display for device linking
- `hex` — encode/decode group master keys
- `tokio::net::UnixListener` / `UnixStream` — IPC (already in tokio with `net` feature)

---

## File Changes Summary

### New files/crates

| File | Purpose |
|---|---|
| `crates/coop-ipc/` | New crate: IPC protocol, server, client |
| `crates/coop-ipc/src/protocol.rs` | `ClientMessage` / `ServerMessage` types |
| `crates/coop-ipc/src/server.rs` | `IpcServer` — listens on Unix socket inside gateway |
| `crates/coop-ipc/src/client.rs` | `IpcClient` — connects from TUI process |
| `crates/coop-channels/src/signal.rs` | `SignalChannel` implementation |

### Modified files

| File | Change |
|---|---|
| `Cargo.toml` (workspace) | Add `coop-ipc`, `[patch.crates-io]` for curve25519 |
| `crates/coop-channels/Cargo.toml` | Add presage deps behind `signal` feature |
| `crates/coop-channels/src/lib.rs` | Export `SignalChannel` |
| `crates/coop-gateway/Cargo.toml` | Add `coop-ipc` dependency |
| `crates/coop-gateway/src/main.rs` | `cmd_start` runs IpcServer + Signal; `cmd_chat` uses IpcClient; add `signal link` subcommand |
| `crates/coop-gateway/src/router.rs` | `MessageRouter` struct, per-sender session routing |
| `crates/coop-gateway/src/config.rs` | Add `channels` config section |
| `crates/coop-core/src/types.rs` | Add `reply_to` field to `InboundMessage` |

---

## Testing Strategy

1. **IPC protocol round-trip**: serialize/deserialize `ClientMessage`/`ServerMessage`, verify ndjson framing
2. **IPC server + client integration**: spawn server on a tempdir socket, connect client, send a message through `FakeProvider`, verify streaming events arrive
3. **Signal routing**: unit tests for `route_message()` with Signal-shaped `InboundMessage` inputs
4. **Router dispatch**: `FakeChannel` + `FakeProvider` through `MessageRouter`, verify correct session selection
5. **Signal device linking**: manual test only (requires real phone)

---

## Risks & Mitigations

| Risk | Mitigation |
|---|---|
| presage is AGPL-3.0 | Feature-gate behind `signal`. Default build has no AGPL code. |
| presage API instability | Pin to specific git rev |
| Unix socket portability (Windows) | macOS/Linux only for now. Windows support via named pipes later if needed. |
| IPC protocol versioning | Include a `version` field in the initial handshake. Keep ndjson so old clients can at least parse unknown messages. |
| Multiple TUI clients on same session | Allow it — both see the same event stream. Gateway serializes turns (one at a time per session). |
| Gateway crash loses sessions | Phase 3+ adds SQLite session persistence. For now, sessions are in-memory. |

---

## Implementation Order

1. **Phase 1a–1c**: Define IPC protocol types in `coop-ipc`, implement `IpcServer` + `IpcClient`
2. **Phase 1d**: Refactor `cmd_start` to run `IpcServer`, refactor `cmd_chat` to connect via `IpcClient`
3. **Phase 1e**: Test attach/detach — start gateway, connect TUI, disconnect, reconnect
4. **Phase 2a–2b**: Add presage, implement `SignalChannel`
5. **Phase 2c**: Add `coop signal link` command
6. **Phase 2d**: Reconnection logic
7. **Phase 3a–3c**: Build `MessageRouter`, wire Signal + IPC through it, per-sender sessions
8. Tests at each phase
