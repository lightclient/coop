# Cron Scheduler

Implement a cron-based scheduler for Coop that fires scheduled messages into agent sessions. This lets the agent run periodic tasks — heartbeat checks, morning briefings, reminders — without human input.

Read `AGENTS.md` at the project root before starting. Follow the development loop and code quality rules.

## Background

The design doc (`docs/design.md`) already specifies cron jobs in config:

```yaml
cron:
  - name: heartbeat
    cron: "*/30 * * * *"
    situation: system
    message: "check HEARTBEAT.md"
  - name: morning-briefing
    cron: "0 8 * * *"
    situation: system
    message: "morning briefing"
```

Cron jobs run as `situation: system` with `ceiling: full` — they have full trust and full memory access. They are system-initiated, not user-initiated. The agent sees them as user messages injected into a dedicated session.

Currently nothing in Coop handles cron. The `Config` struct has no `cron` field, the gateway has no scheduler loop, and there is no `SessionKind` variant for cron jobs. This prompt covers the full implementation.

## Design

### Principles

1. **Simple.** A background tokio task that sleeps until the next fire time. No external scheduler, no job queue, no persistence of state.
2. **Routed through the existing gateway.** Cron fires produce `InboundMessage`s that go through `MessageRouter::dispatch()` just like any other message. The scheduler doesn't call the gateway directly — it uses the same path as channels.
3. **One session per cron entry.** Each named entry gets its own session: `SessionKind::Cron(name)`. This keeps heartbeat history separate from briefing history.
4. **Trust: system ceiling, user-scoped.** Each cron entry names a `user` from the config. Trust resolves as `min(user.trust, system_ceiling)` — since system ceiling is `Full`, effective trust equals the user's trust level. The user name flows through to `PromptBuilder` (loading per-user memory from `users/{user}/MEMORY.md`) and `ToolContext` (so tools know who they're acting for). An entry without a `user` field defaults to no user context — full trust, no per-user memory.
5. **Output delivery via tools.** Cron responses appear in traces and the event log. To deliver a message to a user, the agent calls `signal_send` during the turn — the `message` field tells it who to contact. No special scheduler→channel coupling.
6. **Missed fires are skipped.** If the gateway was down when a cron should have fired, it does not fire retroactively on startup. The next scheduled time fires normally.
7. **Graceful shutdown.** The scheduler task respects cancellation tokens and stops cleanly.

### Session Kind

Add a `Cron` variant to `SessionKind`:

```rust
pub enum SessionKind {
    Main,
    Dm(String),
    Group(String),
    Isolated(Uuid),
    Cron(String),    // <-- new: cron entry name
}
```

Update `SessionKey::Display` to format as `{agent_id}:cron:{name}`.

Update `parse_session_key()` in `gateway.rs` to parse `cron:` prefixed sessions.

### Config

Add a `cron` field to the gateway `Config`:

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct Config {
    pub agent: AgentConfig,
    #[serde(default)]
    pub users: Vec<UserConfig>,
    #[serde(default)]
    pub channels: ChannelsConfig,
    #[serde(default)]
    pub provider: ProviderConfig,
    #[serde(default)]
    pub cron: Vec<CronConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct CronConfig {
    /// Unique name for this cron entry (used as session key).
    pub name: String,
    /// Cron expression (standard 5-field: min hour dom month dow).
    pub cron: String,
    /// Message injected into the agent session when the cron fires.
    pub message: String,
    /// User this cron entry runs as (must match a name in config.users).
    /// Determines trust level, per-user memory, and tool context identity.
    /// When omitted, runs with full trust but no user context.
    #[serde(default)]
    pub user: Option<String>,
}
```

The `user` field ties the cron entry to a configured user. This matters because:
- **Per-user memory:** `PromptBuilder.user()` loads `users/{user}/MEMORY.md` into the system prompt. Without a user, the agent has no per-user memory context.
- **Tool context:** `ToolContext.user_name` tells tools who they're acting for.
- **Trust level:** Resolved as `min(user.trust, Full)` — equals the user's trust. Without a user, defaults to `Full`.
- **Prompt identity:** The runtime section shows "User: {name}" so the agent knows whose behalf it's operating on.

At startup, the scheduler should validate that each entry's `user` (if set) exists in `config.users`. Log a warning for unrecognized users (matching the existing behavior in `resolve_tui_user()`).

### Scheduler

The scheduler lives in a new file: `crates/coop-gateway/src/scheduler.rs`.

It takes the config, a `MessageRouter`, and a `CancellationToken` (or a `tokio::sync::watch` shutdown signal — whatever the gateway already uses for shutdown coordination).

```rust
pub(crate) async fn run_scheduler(
    cron: Vec<CronConfig>,
    router: Arc<MessageRouter>,
    shutdown: CancellationToken,
) {
    if cron.is_empty() {
        return;
    }

    // Parse all cron expressions at startup. Log and skip invalid ones.
    let parsed: Vec<(CronConfig, cron::Schedule)> = ...;

    // Main loop: find the next fire time across all cron entries, sleep until then.
    loop {
        let now = Utc::now();
        let next = parsed.iter()
            .filter_map(|(cfg, sched)| {
                sched.upcoming(Utc).next().map(|t| (cfg, t))
            })
            .min_by_key(|(_, t)| *t);

        let Some((cfg, fire_time)) = next else {
            // No upcoming fires (shouldn't happen with valid crons, but be safe).
            break;
        };

        let delay = (fire_time - now).to_std().unwrap_or(Duration::ZERO);

        tokio::select! {
            _ = tokio::time::sleep(delay) => {
                fire_cron(cfg, &router).await;
            }
            _ = shutdown.cancelled() => {
                info!("scheduler shutting down");
                break;
            }
        }
    }
}
```

**Important:** After each fire, re-compute the next fire time for all entries. Don't pre-compute all fire times at startup — cron expressions like `*/30 * * * *` produce infinite sequences.

### Firing a cron entry

When a cron entry fires:

```rust
async fn fire_cron(cfg: &CronConfig, router: &MessageRouter) {
    let span = info_span!(
        "cron_fired",
        cron.name = %cfg.name,
        user = ?cfg.user,
    );

    async {
        info!(
            cron = %cfg.cron,
            message = %cfg.message,
            user = ?cfg.user,
            "cron firing"
        );

        // Encode the user into the sender field so the router can extract it.
        // Format: "cron:{name}" or "cron:{name}:{user}"
        let sender = match &cfg.user {
            Some(user) => format!("cron:{}:{}", cfg.name, user),
            None => format!("cron:{}", cfg.name),
        };

        let inbound = InboundMessage {
            channel: "cron".to_owned(),
            sender,
            content: cfg.message.clone(),
            chat_id: None,
            is_group: false,
            timestamp: Utc::now(),
            reply_to: None,
            kind: InboundKind::Text,
            message_timestamp: None,
        };

        // Use dispatch_collect_text — we don't need to stream events,
        // just fire the turn and let the agent do its thing (including
        // calling signal_send if the cron message tells it to).
        match router.dispatch_collect_text(&inbound).await {
            Ok((decision, _response)) => {
                info!(
                    session = %decision.session_key,
                    trust = ?decision.trust,
                    user = ?decision.user_name,
                    "cron completed"
                );
            }
            Err(error) => {
                error!(error = %error, "cron dispatch failed");
            }
        }
    }
    .instrument(span)
    .await;
}
```

### Routing cron messages

The `MessageRouter` needs to handle `channel: "cron"` messages. Update `route_message()` in `router.rs`:

```rust
// In route_message():
if msg.channel == "cron" {
    // Sender format: "cron:{name}" or "cron:{name}:{user}"
    let rest = msg.sender.strip_prefix("cron:").unwrap_or(&msg.sender);

    // Split into cron name and optional user.
    // The name is the first segment, user is everything after.
    let (cron_name, cron_user) = match rest.find(':') {
        Some(idx) => (&rest[..idx], Some(rest[idx + 1..].to_owned())),
        None => (rest, None),
    };

    // Look up user trust from config. Default to Full if no user specified.
    let (user_trust, user_name) = if let Some(ref user) = cron_user {
        let matched = config.users.iter().find(|u| u.name == *user);
        let trust = matched.map_or(TrustLevel::Full, |u| u.trust);
        (trust, Some(user.clone()))
    } else {
        (TrustLevel::Full, None)
    };

    // System ceiling is Full, so effective trust = min(user.trust, Full) = user.trust
    let trust = resolve_trust(user_trust, TrustLevel::Full);

    return RouteDecision {
        session_key: SessionKey {
            agent_id: agent_id.clone(),
            kind: SessionKind::Cron(cron_name.to_owned()),
        },
        trust,
        user_name,
    };
}
```

Cron messages use `system` ceiling (`Full`), but respect the configured user's trust level. The `user_name` flows through to `Gateway::run_turn_with_trust()`, which passes it to `build_prompt()` (loading per-user memory) and `tool_context()` (so tools know who they're acting for). When no user is specified, trust defaults to `Full` with no user context.

### `signal_send` tool (prerequisite)

Cron sessions have no originating channel to reply to. The normal Signal flow (`signal_loop.rs`) automatically sends the agent's response back to whoever messaged — but cron has no "whoever messaged." The agent needs a tool to initiate messages.

The existing `signal_reply` tool quotes a specific message (requires timestamps, author UUIDs). That's useful in group chats for threading, but in DMs it looks odd — it's obvious the agent is replying to you. `signal_send` is the simpler, general-purpose alternative: just send text to a target.

#### Implementation

Add `SignalSendTool` to `crates/coop-channels/src/signal_tools.rs`, alongside the existing `SignalReactTool` and `SignalReplyTool`:

```rust
#[derive(Debug)]
pub struct SignalSendTool {
    action_tx: mpsc::Sender<SignalAction>,
}

#[derive(Debug, Deserialize)]
struct SendArgs {
    target: String,
    text: String,
}

#[async_trait]
impl Tool for SignalSendTool {
    fn definition(&self) -> ToolDef {
        ToolDef::new(
            "signal_send",
            "Send a Signal message to a user or group",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "target": {
                        "type": "string",
                        "description": "Recipient: a UUID for DM, or group:<hex> for a group"
                    },
                    "text": {
                        "type": "string",
                        "description": "Message text to send"
                    }
                },
                "required": ["target", "text"]
            }),
        )
    }

    async fn execute(
        &self,
        arguments: serde_json::Value,
        _ctx: &ToolContext,
    ) -> Result<ToolOutput> {
        let span = info_span!("signal_tool_send");
        async {
            let args: SendArgs = serde_json::from_value(arguments)?;
            let _parsed = SignalTarget::parse(&args.target)?; // validate target format

            info!(
                tool.name = "signal_send",
                signal.action = "send",
                signal.target = %args.target,
                signal.raw_content = %args.text,
                "signal tool action queued"
            );

            let outbound = OutboundMessage {
                channel: "signal".to_owned(),
                target: args.target,
                content: args.text,
            };

            self.action_tx
                .send(SignalAction::SendText(outbound))
                .await
                .map_err(|_| anyhow::anyhow!("signal action channel closed"))?;

            Ok(ToolOutput::success("message sent"))
        }
        .instrument(span)
        .await
    }
}
```

This reuses `SignalAction::SendText` — the exact same path that `signal_loop.rs` uses to send normal replies. The send task in `signal.rs` already handles target parsing, message construction, and tracing.

**Do not** register `signal_send` in `SignalToolExecutor::new()`. It must only be available to cron sessions — not to normal DM/group sessions. In normal conversations, the agent's reply is already delivered through the originating channel automatically. Giving it `signal_send` there would let it proactively message arbitrary targets outside the current conversation, which is an unintended trust escalation. The existing `signal_react` and `signal_reply` tools are fine for all sessions because they operate on the current conversation context.

Instead, make `SignalSendTool` available through a separate executor (or conditional registration) that the gateway only wires in for cron turns. The simplest approach: give `SignalToolExecutor::new()` an `include_send: bool` parameter:

```rust
impl SignalToolExecutor {
    pub fn new(action_tx: mpsc::Sender<SignalAction>, include_send: bool) -> Self {
        let mut tools: Vec<Box<dyn Tool>> = vec![
            Box::new(SignalReactTool::new(action_tx.clone())),
            Box::new(SignalReplyTool::new(action_tx.clone())),
        ];
        if include_send {
            tools.push(Box::new(SignalSendTool::new(action_tx)));
        }
        Self { tools }
    }
}
```

Normal sessions pass `include_send: false`. Cron routing passes `include_send: true`.

#### Why this replaces `notify`

With `signal_send` available in cron sessions, cron entries don't need a built-in delivery mechanism. The `message` field tells the agent who to contact:

```yaml
message: |
  Morning briefing. Summarize overnight activity.
  Send the briefing to alice-uuid via signal_send.
```

The agent calls `signal_send(target="alice-uuid", text="...")` as part of the cron turn. This is simpler than a `notify` field because:
- Zero scheduler↔channel coupling — the scheduler just fires turns
- The agent controls the content (it might decide not to send, or send to multiple targets)
- One tool to learn vs. a separate config mechanism

### Integration in `cmd_start`

Wire the scheduler into the gateway daemon in `main.rs`:

```rust
// In cmd_start(), after creating the router:

if !config.cron.is_empty() {
    let cron = config.cron.clone();
    let router = Arc::clone(&router);
    tokio::spawn(async move {
        scheduler::run_scheduler(cron, router).await;
    });
    info!(count = config.cron.len(), "scheduler started");
}
```

The scheduler has no channel dependencies — it just fires turns through the router. The agent calls `signal_send` (or any other tool) as part of the turn if the cron message tells it to. The gateway must wire the `SignalToolExecutor` with `include_send: true` for cron turns (and `include_send: false` for normal sessions).

The scheduler runs alongside the IPC server and Signal loop. It stops when the tokio runtime shuts down (Ctrl-C triggers shutdown, dropping the scheduler task).

For explicit graceful shutdown, use `tokio_util::sync::CancellationToken`:

```rust
let cancel = CancellationToken::new();

let cancel_clone = cancel.clone();
tokio::spawn(async move {
    scheduler::run_scheduler(cron, router, cancel_clone).await;
});

// On shutdown:
cancel.cancel();
```

Or simpler: use `tokio::select!` with `ctrl_c` in the scheduler loop itself, since the scheduler is already a long-running select loop.

### Cron parsing

Use the `cron` crate (already listed in design doc dependencies). Add it to `coop-gateway`:

```bash
cd crates/coop-gateway && cargo add cron
```

The `cron` crate parses standard 5-field cron expressions and produces `Schedule` objects that can compute upcoming fire times.

**Important:** The `cron` crate uses 7-field expressions by default (sec min hour dom month dow year). Standard 5-field cron (`min hour dom month dow`) needs to be wrapped: prepend `0 ` for seconds and append ` *` for year, or use 6-field with `0` seconds prefix.

```rust
use cron::Schedule;
use std::str::FromStr;

// Convert 5-field to 7-field: "*/30 * * * *" → "0 */30 * * * * *"
fn parse_cron(expr: &str) -> Result<Schedule> {
    let fields: Vec<&str> = expr.split_whitespace().collect();
    let full_expr = match fields.len() {
        5 => format!("0 {expr} *"),      // 5-field → 7-field (add sec=0, year=*)
        6 => format!("0 {expr}"),         // 6-field → 7-field (add sec=0)
        7 => expr.to_owned(),             // already 7-field
        _ => anyhow::bail!("invalid cron expression: {expr}"),
    };
    Schedule::from_str(&full_expr)
        .with_context(|| format!("invalid cron expression: {expr}"))
}
```

### Timezone

Use UTC for all cron evaluation. Expressions fire in UTC. If local-time scheduling is needed later, add an optional `timezone` field to `CronConfig`.

### `list_sessions` visibility

Cron sessions should appear in `gateway.list_sessions()` so `coop attach` can view them. They already will because they're stored in the sessions HashMap — no extra work needed.

## Implementation Steps

1. **Add `Cron` variant to `SessionKind`** in `crates/coop-core/src/types.rs`. Update `Display` for `SessionKey`. Add test.

2. **Add `CronConfig` and `cron` field** to `crates/coop-gateway/src/config.rs`. Add config parsing tests with and without cron entries.

3. **Add `parse_session_key` support for `cron:`** in `crates/coop-gateway/src/gateway.rs`. Add test.

4. **Add cron routing** in `crates/coop-gateway/src/router.rs`. When `msg.channel == "cron"`, parse sender for cron name and optional user, look up user trust from config, route to `SessionKind::Cron(name)` with resolved trust and user name. Add tests for all four cases (with user, without user, inner-trust user, unknown user).

5. **Add `cron` dependency** to `coop-gateway`: `cd crates/coop-gateway && cargo add cron`.

6. **Add `signal_send` tool** to `crates/coop-channels/src/signal_tools.rs`. Gate it behind `include_send: bool` in `SignalToolExecutor::new()` so it's only available to cron sessions. Update existing call sites to pass `include_send: false`. Add tests following the existing pattern for `signal_react`/`signal_reply`.

7. **Create `crates/coop-gateway/src/scheduler.rs`** with `run_scheduler()` and `fire_cron()`. At startup, validate that each entry's `user` (if set) exists in the config's users list — log a warning for unknown users (same pattern as `resolve_tui_user()`). Include tracing spans and events per the tracing rules in `AGENTS.md`.

8. **Wire scheduler into `cmd_start()`** in `main.rs`. Spawn it after creating the router, before the IPC accept loop.

9. **Add integration test** that creates a scheduler with a cron firing every second (`* * * * * * *` in 7-field), verifies the message is dispatched through the router, and the session receives a response. Use `FakeProvider` from `coop-core/src/fakes.rs`.

10. **Update `coop.yaml`** example in this repo to show a commented-out cron section.

11. **Verify build times.** `touch crates/coop-gateway/src/main.rs && time cargo build` should stay under 1.5s. The `cron` crate is lightweight — it should not impact compile times significantly.

## Config Example

```yaml
agent:
  id: coop
  model: anthropic/claude-sonnet-4-20250514
  workspace: ./workspaces/default

users:
  - name: alice
    trust: full
    match: ["terminal:default", "signal:alice-uuid"]
  - name: bob
    trust: inner
    match: ["signal:bob-uuid"]

cron:
  # Runs as alice — agent checks tasks and updates workspace files silently.
  - name: heartbeat
    cron: "*/30 * * * *"
    user: alice
    message: |
      Heartbeat check. Review HEARTBEAT.md for any pending tasks.
      If there are tasks due, execute them. If nothing is due, do nothing.

  # Runs as alice — agent sends briefing to her via signal_send.
  - name: morning-briefing
    cron: "0 8 * * *"
    user: alice
    message: |
      Morning briefing. Summarize:
      1. Any overnight messages that need attention
      2. Calendar items for today (check CALENDAR.md if it exists)
      3. Any pending tasks from HEARTBEAT.md
      Send the briefing to alice-uuid using signal_send. Keep it concise.

  # Runs as alice — agent sends summary to a group and writes a file.
  - name: weekly-review
    cron: "0 18 * * 5"
    user: alice
    message: |
      Weekly review. Summarize the week:
      1. Key conversations and decisions
      2. Tasks completed and pending
      3. Memory updates needed
      Write the full summary to WEEKLY.md.
      Send a short version to group:deadbeef using signal_send.

  # No user — system-level maintenance, no messaging needed.
  - name: cleanup
    cron: "0 3 * * *"
    message: |
      System maintenance. Check disk usage in workspace directory.
      Archive any log files older than 7 days.

channels:
  signal:
    db_path: ./db/signal.db

provider:
  name: anthropic
```

## Testing

### Unit tests

- `config.rs`: Parse config with and without `cron` field. Verify empty default. Verify `user` field is optional (None when omitted). Verify cron expression is stored as string (parsing happens in scheduler, not config).
- `types.rs`: `SessionKind::Cron("heartbeat")` displays as expected. `SessionKey` display and equality.
- `router.rs`: Four routing cases for `channel: "cron"`:
  - `sender: "cron:heartbeat:alice"` → `Cron("heartbeat")`, trust from alice's config, `user_name: Some("alice")`.
  - `sender: "cron:heartbeat"` (no user) → `Cron("heartbeat")`, `Full` trust, `user_name: None`.
  - `sender: "cron:heartbeat:bob"` (inner trust user) → `Cron("heartbeat")`, `Inner` trust, `user_name: Some("bob")`.
  - `sender: "cron:heartbeat:unknown"` (user not in config) → `Cron("heartbeat")`, `Full` trust (default), `user_name: Some("unknown")`.
- `gateway.rs`: `parse_session_key("coop:cron:heartbeat", "coop")` returns correct key.
- `scheduler.rs`: `parse_cron()` handles 5-field, 6-field, 7-field expressions. Invalid expressions return errors. `fire_cron` encodes user into sender correctly.
- `signal_tools.rs`: `signal_send` tool sends `SignalAction::SendText` with correct target and text. Rejects invalid targets (same pattern as existing `signal_react`/`signal_reply` tests). Errors when action channel is closed. `SignalToolExecutor::new(tx, false)` excludes `signal_send` from `tools()`. `SignalToolExecutor::new(tx, true)` includes it.

### Integration test

In `crates/coop-gateway/tests/` (or inline `#[tokio::test]`):

```rust
#[tokio::test]
async fn scheduler_fires_and_routes_message() {
    // Setup: FakeProvider, DefaultExecutor, Gateway, Router
    // Config with alice (trust: full) in users list
    // Create a cron entry: name="test", every second, user="alice"
    // Run scheduler for ~2 seconds
    // Verify:
    //   - session exists with kind Cron("test")
    //   - message was dispatched through router
    //   - RouteDecision has user_name=Some("alice") and trust=Full
    //   - response received from FakeProvider
}

#[tokio::test]
async fn scheduler_fires_without_user() {
    // Same setup but cron entry has no user field
    // Verify: RouteDecision has user_name=None and trust=Full
}
```

Use `tokio::time::pause()` and `tokio::time::advance()` for deterministic time control if possible. The `cron` crate uses `chrono::Utc::now()` internally, so time mocking may require a different approach — in that case, use a short-interval cron and a real timeout.

## Tracing

The scheduler must emit structured trace events per `AGENTS.md` tracing rules:

- `cron_fired` span wrapping each fire (with entry name and user)
- `info!` event when a cron entry fires (name, expression, message, user)
- `info!` event when a cron entry completes (session key, trust, user)
- `error!` event if dispatch fails
- `info!` event on scheduler startup (count of cron entries, next fire times)
- `info!` event on scheduler shutdown

After implementing, verify with:
```bash
COOP_TRACE_FILE=traces.jsonl cargo run -- start
# Wait for a cron entry to fire
grep "cron_fired" traces.jsonl | jq .
```

## Not in Scope

- **Persistent state.** No tracking of last fire time across restarts. Cron entries are stateless — they compute the next fire from now.
- **Dynamic management.** No API to add/remove cron entries at runtime. Edit `coop.yaml` and restart.
- **Retry on failure.** If a cron turn fails, it's logged and the next fire proceeds normally. No retry queue.
- **Concurrent fire protection.** If a cron turn is still running when the next fire time arrives, the new fire waits or is skipped. Use a simple per-entry mutex or check if the session is busy.
- **Sub-second scheduling.** The minimum granularity is 1 minute (standard cron). The 7-field second support is for testing only.

## Dependencies

Only `cron` is new. It's a pure-Rust crate with minimal dependencies (just `chrono` which we already have). It should not measurably impact compile times.

Do **not** add `tokio-cron-scheduler` or similar — it pulls in heavy dependencies. The scheduler loop is simple enough to write directly with `tokio::time::sleep`.
