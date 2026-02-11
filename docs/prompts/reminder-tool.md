# Reminder Tool: One-Off Scheduled Reminders

Add a `reminder` tool that lets users schedule one-off reminders through natural conversation. When a user says "remind me this afternoon about X," the agent calls the `reminder` tool to schedule a future delivery. When the time arrives, the reminder fires through the existing cron/delivery infrastructure and reaches the user on their channel.

Read `AGENTS.md` at the project root before starting. Follow the development loop and code quality rules.

## Background

Coop already has cron-based scheduling (`scheduler.rs`) that fires recurring jobs through the `MessageRouter`, with delivery to channels via `DeliverySender`. It also has session injection (`SessionInjection`) for pushing messages into existing sessions. The reminder tool builds on both of these.

The key insight: reminders are just one-shot scheduled jobs. The scheduler already knows how to sleep until a fire time, dispatch a message, and deliver the response. Reminders reuse this machinery with two differences: they fire exactly once and they're created at runtime (not from config).

### Existing infrastructure this builds on

- **`scheduler.rs`**: `run_scheduler()` loop, `DeliverySender`, `fire_cron()` pattern
- **`injection.rs`**: `SessionInjection` for injecting messages into sessions
- **`router.rs`**: `MessageRouter::dispatch_collect_text()` and `inject_collect_text()`
- **`traits.rs`**: `Tool` trait, `ToolContext` for execution context
- **`types.rs`**: `ToolDef`, `ToolOutput`, `SessionKey`, `SessionKind`
- **`config.rs`**: `CronDelivery` (reusable for reminder delivery targets)

## Design

### Principles

1. **Persisted to disk.** Reminders are stored in a `Vec` behind a `Mutex`, shared between the tool (which adds) and the scheduler (which fires and removes). The in-memory state is backed by a JSON file written on every mutation (add, cancel, fire). On startup, pending reminders are loaded from disk. This follows the same pattern as `CompactionStore` — simple JSON file, write-on-mutate, load-on-startup. No SQLite dependency in the gateway.
2. **Fires through the agent, not plain text.** When a reminder fires, it dispatches the reminder message as a user message into a dedicated reminder session — the same way cron fires work. The agent runs a full turn with access to all tools, then the response is delivered via `announce_to_session` (injected into the user's DM session for natural phrasing, then sent via `DeliverySender`). This means reminders can trigger actions ("turn off the lights"), check state ("what's the weather?"), or just echo a simple nudge ("call the dentist"). The agent decides what to do based on the reminder text.
3. **User-scoped.** The tool uses `ToolContext.user_name` and the originating channel/sender to determine where to deliver. If the user has Signal channels configured, deliver there. If they're on terminal, skip (terminal users are local — they don't need push reminders).
4. **Natural time parsing.** The agent interprets "this afternoon," "in 2 hours," "tomorrow at 9am" and converts to an ISO 8601 timestamp. The tool accepts a precise timestamp — the LLM does the natural language → time conversion, not the tool.
5. **Channel-aware delivery.** Reminders resolve the delivery target from the same user match patterns that cron uses (`resolve_cron_delivery_targets` pattern), or accept an explicit channel+target override.

### Data model

```rust
/// A one-off reminder scheduled at runtime.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct Reminder {
    /// Unique identifier.
    pub id: String,
    /// When to fire (UTC).
    pub fire_at: DateTime<Utc>,
    /// The reminder message — injected into the agent session as a user message.
    /// Can be a simple nudge ("call the dentist") or an action instruction
    /// ("turn off the kitchen lights").
    pub message: String,
    /// Who created this reminder (from ToolContext.user_name).
    pub user: Option<String>,
    /// Delivery target: channel + target identifier.
    /// Resolved at creation time from user config or explicit override.
    pub delivery: Vec<(String, String)>,
    /// The session where this reminder was created. Stored so the reminder
    /// session agent can read the originating conversation for context
    /// (e.g. the user discussed how to control lights, and the reminder
    /// needs that context to execute). The agent can use `read_file` on
    /// the session JSONL file in the workspace/sessions/ directory.
    pub source_session: String,
    /// When this reminder was created.
    pub created_at: DateTime<Utc>,
}
```

The `source_session` is populated from `ToolContext.session_id` (which is `SessionKey.to_string()`). The session JSONL files live at `{workspace}/sessions/{slug}.jsonl` where the slug is the session key with `/` and `:` replaced by `_`. The reminder session agent can `read_file` on this path to review the conversation where the reminder was created.

### Shared state

The `ReminderStore` holds reminders in memory and writes through to a JSON file on every mutation. On startup, it loads existing reminders from disk. This follows the same pattern as `CompactionStore` (JSON file, write-on-mutate, load-on-startup).

The file lives alongside other gateway state files. The path is derived from the db directory already used for sessions and compaction state (e.g. `./db/reminders.json`). Use the same directory as `DiskSessionStore` — pass it in at construction.

```rust
/// Thread-safe store for pending reminders.
/// Shared between the reminder tool (adds) and scheduler (fires + removes).
/// Backed by a JSON file for persistence across restarts.
#[derive(Debug, Clone)]
pub(crate) struct ReminderStore {
    inner: Arc<Mutex<Vec<Reminder>>>,
    path: PathBuf,
}

impl ReminderStore {
    /// Create a new store, loading any existing reminders from disk.
    ///
    /// `dir` is the state directory (same as session/compaction stores).
    /// The file `reminders.json` is created inside it.
    pub fn new(dir: impl AsRef<Path>) -> Result<Self> {
        let path = dir.as_ref().join("reminders.json");
        let reminders = Self::load_from_disk(&path)?;
        let count = reminders.len();
        if count > 0 {
            info!(count, path = %path.display(), "loaded pending reminders from disk");
        }
        Ok(Self {
            inner: Arc::new(Mutex::new(reminders)),
            path,
        })
    }

    /// Load reminders from the JSON file. Returns empty vec if file
    /// doesn't exist. Logs and skips corrupt entries.
    fn load_from_disk(path: &Path) -> Result<Vec<Reminder>> {
        match std::fs::read_to_string(path) {
            Ok(content) => {
                let reminders: Vec<Reminder> = serde_json::from_str(&content)
                    .with_context(|| {
                        format!("failed to parse reminders file: {}", path.display())
                    })?;
                Ok(reminders)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
            Err(e) => {
                Err(e).with_context(|| format!("failed to read {}", path.display()))
            }
        }
    }

    /// Write the current in-memory state to disk.
    /// Called after every mutation (add, cancel, take_due).
    fn flush(&self, reminders: &[Reminder]) {
        if let Err(e) = Self::write_to_disk(&self.path, reminders) {
            error!(error = %e, path = %self.path.display(), "failed to persist reminders");
        }
    }

    fn write_to_disk(path: &Path, reminders: &[Reminder]) -> Result<()> {
        let content = serde_json::to_string_pretty(reminders)?;
        std::fs::write(path, content)
            .with_context(|| format!("failed to write {}", path.display()))?;
        Ok(())
    }

    /// Add a reminder. Returns the assigned ID. Persists to disk.
    pub fn add(&self, reminder: Reminder) -> String {
        let id = reminder.id.clone();
        let mut store = self.inner.lock().unwrap();
        store.push(reminder);
        self.flush(&store);
        id
    }

    /// Remove and return all reminders whose fire_at <= now.
    /// Persists the remaining set to disk.
    pub fn take_due(&self) -> Vec<Reminder> {
        let now = Utc::now();
        let mut store = self.inner.lock().unwrap();
        let (due, remaining): (Vec<_>, Vec<_>) =
            store.drain(..).partition(|r| r.fire_at <= now);
        *store = remaining;
        if !due.is_empty() {
            self.flush(&store);
        }
        due
    }

    /// List all pending reminders for a given user.
    pub fn list_for_user(&self, user: &str) -> Vec<Reminder> {
        self.inner
            .lock()
            .unwrap()
            .iter()
            .filter(|r| r.user.as_deref() == Some(user))
            .cloned()
            .collect()
    }

    /// Cancel a reminder by ID. Returns true if found and removed.
    /// Persists to disk on removal.
    pub fn cancel(&self, id: &str) -> bool {
        let mut store = self.inner.lock().unwrap();
        let len_before = store.len();
        store.retain(|r| r.id != id);
        let removed = store.len() < len_before;
        if removed {
            self.flush(&store);
        }
        removed
    }

    /// Return the next fire time across all pending reminders.
    pub fn next_fire_time(&self) -> Option<DateTime<Utc>> {
        self.inner
            .lock()
            .unwrap()
            .iter()
            .map(|r| r.fire_at)
            .min()
    }
}
```

### Tool definition

The `reminder` tool supports three operations: `set`, `list`, and `cancel`.

```rust
#[derive(Debug)]
pub(crate) struct ReminderTool {
    store: ReminderStore,
    config: SharedConfig,
}

impl ReminderTool {
    pub fn new(store: ReminderStore, config: SharedConfig) -> Self {
        Self { store, config }
    }
}

#[async_trait]
impl Tool for ReminderTool {
    fn definition(&self) -> ToolDef {
        ToolDef::new(
            "reminder",
            "Schedule, list, or cancel one-off reminders. When a reminder fires, \
             you get a fresh session with full tool access to execute the request — \
             so reminders can trigger actions (run commands, call APIs) not just \
             send text.\n\
             \n\
             BEFORE creating an action reminder (anything beyond a simple nudge), \
             confirm you have a concrete execution plan. If the user says 'turn off \
             the lights in 30 min' and you don't know the command, API endpoint, or \
             tool to use — ASK. Say 'I can set that reminder, but I want to make \
             sure I can actually do it — what system controls your lights?' or check \
             your workspace files (TOOLS.md, memory) first. Don't create a reminder \
             you can't confidently execute.\n\
             \n\
             Write the message as self-contained as possible: resolve references \
             ('this', 'that') and include the specific execution plan (commands, \
             API endpoints, file paths). The reminder session can read the \
             originating conversation for additional context, but a complete \
             message means faster, more reliable execution.",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "action": {
                        "type": "string",
                        "enum": ["set", "list", "cancel"],
                        "description": "Action to perform"
                    },
                    "time": {
                        "type": "string",
                        "description": "ISO 8601 UTC timestamp for when the reminder should fire (e.g. '2026-02-11T18:00:00Z'). Required for 'set'."
                    },
                    "message": {
                        "type": "string",
                        "description": "What to do when the reminder fires. For nudges: 'Remind the user to call Dr. Smith at 555-0123'. For actions: include the full execution plan — 'Turn off kitchen lights by running: curl -X POST http://ha.local:8123/api/...' The reminder session has no conversation history (though it can read the source session file), so resolve references and be specific. Required for 'set'."
                    },
                    "id": {
                        "type": "string",
                        "description": "Reminder ID to cancel. Required for 'cancel'."
                    }
                },
                "required": ["action"]
            }),
        )
    }

    async fn execute(
        &self,
        arguments: serde_json::Value,
        ctx: &ToolContext,
    ) -> Result<ToolOutput> {
        let action = arguments
            .get("action")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("missing required parameter: action"))?;

        match action {
            "set" => self.handle_set(&arguments, ctx).await,
            "list" => self.handle_list(ctx),
            "cancel" => self.handle_cancel(&arguments),
            other => Ok(ToolOutput::error(format!("unknown action: {other}"))),
        }
    }
}
```

#### `set` action

```rust
async fn handle_set(
    &self,
    arguments: &serde_json::Value,
    ctx: &ToolContext,
) -> Result<ToolOutput> {
    let time_str = arguments
        .get("time")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("missing required parameter: time"))?;

    let message = arguments
        .get("message")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("missing required parameter: message"))?;

    let fire_at: DateTime<Utc> = time_str
        .parse()
        .map_err(|e| anyhow::anyhow!("invalid time format: {e}"))?;

    if fire_at <= Utc::now() {
        return Ok(ToolOutput::error(
            "reminder time must be in the future"
        ));
    }

    // Resolve delivery target — prefer the originating channel,
    // fall back to all non-terminal channels from user config.
    let config = self.config.load();
    let delivery = resolve_reminder_delivery(&config, &ctx.session_id, ctx.user_name.as_deref());

    if delivery.is_empty() {
        return Ok(ToolOutput::error(
            "no delivery channel found for this user — \
             reminders require a non-terminal channel (e.g. Signal) \
             configured in the user's match patterns"
        ));
    }

    let id = format!("rem_{}", uuid::Uuid::new_v4().simple());

    let reminder = Reminder {
        id: id.clone(),
        fire_at,
        message: message.to_owned(),
        user: ctx.user_name.clone(),
        delivery,
        source_session: ctx.session_id.clone(),
        created_at: Utc::now(),
    };

    info!(
        reminder.id = %id,
        fire_at = %fire_at,
        user = ?ctx.user_name,
        delivery_count = reminder.delivery.len(),
        "reminder scheduled"
    );

    self.store.add(reminder);

    Ok(ToolOutput::success(format!(
        "Reminder scheduled (id: {id}) for {fire_at}"
    )))
}
```

#### `list` action

```rust
fn handle_list(&self, ctx: &ToolContext) -> Result<ToolOutput> {
    let user = ctx.user_name.as_deref().unwrap_or("unknown");
    let reminders = self.store.list_for_user(user);

    if reminders.is_empty() {
        return Ok(ToolOutput::success("No pending reminders."));
    }

    let mut lines = Vec::new();
    for r in &reminders {
        lines.push(format!(
            "- [{}] {} → \"{}\"",
            r.id, r.fire_at, r.message
        ));
    }
    Ok(ToolOutput::success(lines.join("\n")))
}
```

#### `cancel` action

```rust
fn handle_cancel(&self, arguments: &serde_json::Value) -> Result<ToolOutput> {
    let id = arguments
        .get("id")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("missing required parameter: id"))?;

    if self.store.cancel(id) {
        info!(reminder.id = %id, "reminder cancelled");
        Ok(ToolOutput::success(format!("Reminder {id} cancelled.")))
    } else {
        Ok(ToolOutput::error(format!(
            "Reminder {id} not found."
        )))
    }
}
```

### Delivery target resolution

Deliver to the channel the reminder was created from. The originating channel is derived from the session key in `ToolContext.session_id`. For DM sessions the key is `{agent}:dm:{channel}:{target}` (e.g. `coop:dm:signal:alice-uuid`), so we can extract the channel and target directly.

If the originating channel is terminal (can't push to a terminal) or can't be parsed (main session, cron session), fall back to all non-terminal channels from the user's config — same as cron delivery.

```rust
/// Resolve delivery targets for a reminder.
///
/// Primary: extract the originating channel from the session key.
/// Fallback: all non-terminal channels from the user's config match patterns.
fn resolve_reminder_delivery(
    config: &Config,
    session_id: &str,
    user_name: Option<&str>,
) -> Vec<(String, String)> {
    // Try to extract channel:target from a DM session key.
    // Format: "{agent_id}:dm:{channel}:{target}"
    if let Some(rest) = session_id.split_once(":dm:").map(|(_, rest)| rest) {
        if let Some((channel, target)) = rest.split_once(':') {
            if channel != "terminal" {
                return vec![(channel.to_owned(), target.to_owned())];
            }
        }
    }

    // Fallback: all non-terminal channels from user config.
    let Some(user_name) = user_name else {
        return Vec::new();
    };

    let Some(user) = config.users.iter().find(|u| u.name == user_name) else {
        return Vec::new();
    };

    user.r#match
        .iter()
        .filter_map(|pattern| {
            let (channel, target) = pattern.split_once(':')?;
            if channel == "terminal" {
                None
            } else {
                Some((channel.to_owned(), target.to_owned()))
            }
        })
        .collect()
}
```

**Examples:**

| Created from | `session_id` | Delivery target |
|---|---|---|
| Signal DM | `coop:dm:signal:alice-uuid` | `signal:alice-uuid` (direct) |
| Telegram DM | `coop:dm:telegram:alice-id` | `telegram:alice-id` (direct) |
| Terminal | `coop:main` | All non-terminal channels from config (fallback) |
| Group chat | `coop:group:signal:group:deadbeef` | Falls through to fallback (groups don't have a single DM target) |

### Scheduler integration

The scheduler's main loop already computes the next fire time and sleeps. Extend it to also check `ReminderStore::next_fire_time()` and wake for the earlier of (cron, reminder).

Modify `run_scheduler_with_notify` to accept an `Option<ReminderStore>`:

```rust
pub(crate) async fn run_scheduler_with_notify(
    config: SharedConfig,
    router: Arc<MessageRouter>,
    deliver_tx: Option<DeliverySender>,
    shutdown: CancellationToken,
    cron_notify: Option<Arc<tokio::sync::Notify>>,
    reminders: Option<ReminderStore>,
) {
    // ... existing setup ...

    loop {
        // ... existing cron re-parse on config change ...

        // Check for due reminders BEFORE computing sleep duration.
        if let Some(ref store) = reminders {
            let due = store.take_due();
            for reminder in due {
                let deliver_tx = deliver_tx.clone();
                let router = Arc::clone(&router);
                let config = Arc::clone(&config);
                tokio::spawn(async move {
                    fire_reminder(&reminder, &router, deliver_tx.as_ref(), &config).await;
                });
            }
        }

        let now = Utc::now();

        // Next cron fire time (existing logic).
        let next_cron = parsed
            .iter()
            .filter_map(|(cfg, sched)| sched.upcoming(Utc).next().map(|t| (cfg, t)))
            .min_by_key(|(_, t)| *t);

        // Next reminder fire time.
        let next_reminder = reminders.as_ref().and_then(|s| s.next_fire_time());

        // Sleep until the earlier of the two.
        let next_fire = match (next_cron.map(|(_, t)| t), next_reminder) {
            (Some(c), Some(r)) => Some(c.min(r)),
            (Some(c), None) => Some(c),
            (None, Some(r)) => Some(r),
            (None, None) => None,
        };

        let Some(fire_time) = next_fire else {
            tokio::select! {
                () = notify.notified() => continue,
                () = shutdown.cancelled() => {
                    info!("scheduler shutting down");
                    return;
                }
            }
        };

        let delay = (fire_time - now).to_std().unwrap_or(Duration::ZERO);

        tokio::select! {
            () = tokio::time::sleep(delay) => {
                // Check if it was a cron that fired...
                // (existing cron fire logic)

                // ... or a reminder (handled at top of next loop iteration
                // via take_due()).
            }
            () = notify.notified() => {
                debug!("scheduler woken by config change");
            }
            () = shutdown.cancelled() => {
                info!("scheduler shutting down");
                return;
            }
        }
    }
}
```

**Important:** Reminders are checked at the top of each loop iteration via `take_due()`, not at the point of waking. This is simpler and handles the case where multiple reminders come due while a cron is firing. The sleep always wakes at the right time because we take `min(next_cron, next_reminder)`.

### Firing a reminder

When a reminder fires, it follows the same two-phase pattern as cron delivery:

1. **Dispatch to a reminder session.** The reminder message is injected as a user message into a dedicated session (`SessionKind::Cron` with a `reminder:{id}` name — reusing the existing cron session kind to avoid touching `coop-core`). The agent runs a full turn with tools available. This is where actions happen ("turn off the lights") or content gets generated.

2. **Announce to user's DM session.** The agent's response is passed through `announce_to_session` — injected into the user's DM session so the agent can rephrase it naturally for the user, then delivered via `DeliverySender`.

This reuses `announce_to_session` and `deliver_to_target` from `scheduler.rs` — the same path cron already uses.

```rust
async fn fire_reminder(
    reminder: &Reminder,
    router: &MessageRouter,
    deliver_tx: Option<&DeliverySender>,
    shared_config: &SharedConfig,
) {
    let span = info_span!(
        "reminder_fired",
        reminder.id = %reminder.id,
        user = ?reminder.user,
    );

    async {
        let lateness = Utc::now() - reminder.fire_at;
        if lateness > chrono::Duration::seconds(60) {
            warn!(
                reminder.id = %reminder.id,
                late_by_secs = lateness.num_seconds(),
                "firing late reminder (gateway was likely down)"
            );
        }

        info!(
            message = %reminder.message,
            fire_at = %reminder.fire_at,
            delivery_count = reminder.delivery.len(),
            "reminder firing"
        );

        if reminder.delivery.is_empty() {
            warn!(reminder.id = %reminder.id, "reminder has no delivery targets");
            return;
        }

        let config_snapshot = shared_config.load();
        let agent_id = config_snapshot.agent.id.clone();

        // Build the sender string for routing (same format as cron).
        let sender = match &reminder.user {
            Some(user) => format!("cron:reminder-{}:{}", reminder.id, user),
            None => format!("cron:reminder-{}", reminder.id),
        };

        // Determine prompt channel from delivery targets.
        let prompt_channel = reminder.delivery
            .first()
            .map(|(channel, _)| channel.clone());

        // Build the session file path so the agent can read it for context.
        let session_slug = reminder.source_session.replace(['/', ':'], "_");
        let session_file = format!("sessions/{session_slug}.jsonl");

        let content = format!(
            "[This is a scheduled reminder set by the user. Execute the request \
             and respond with the result. Your response will be delivered to the \
             user via {channel}. Keep it conversational.\n\
             \n\
             If you need more context about what the user meant, read the \
             conversation where this reminder was created: {session_file}]\n\n\
             {message}",
            channel = prompt_channel.as_deref().unwrap_or("messaging"),
            session_file = session_file,
            message = reminder.message,
        );

        let inbound = InboundMessage {
            channel: "cron".to_owned(),
            sender,
            content,
            chat_id: None,
            is_group: false,
            timestamp: Utc::now(),
            reply_to: None,
            kind: InboundKind::Text,
            message_timestamp: None,
        };

        // Phase 1: Run agent turn in a reminder session.
        match router
            .dispatch_collect_text_with_channel(&inbound, prompt_channel)
            .await
        {
            Ok((_decision, response)) => {
                if response.trim().is_empty() {
                    debug!(reminder.id = %reminder.id, "reminder produced empty response");
                    return;
                }

                // Phase 2: Announce to user's DM session and deliver.
                for (channel, target) in &reminder.delivery {
                    announce_to_session(
                        &format!("reminder:{}", reminder.id),
                        &response,
                        channel,
                        target,
                        &agent_id,
                        router,
                        deliver_tx,
                    )
                    .await;
                }

                info!(reminder.id = %reminder.id, "reminder completed and delivered");
            }
            Err(e) => {
                error!(
                    reminder.id = %reminder.id,
                    error = %e,
                    "reminder dispatch failed"
                );

                // Fallback: deliver the raw reminder text so the user
                // at least gets the nudge even if the agent turn failed.
                for (channel, target) in &reminder.delivery {
                    deliver_to_target(
                        channel,
                        target,
                        &format!("Reminder: {}", reminder.message),
                        deliver_tx,
                    )
                    .await;
                }
            }
        }
    }
    .instrument(span)
    .await;
}
```

**Key details:**

- The reminder routes through `channel: "cron"` with a `cron:reminder-{id}:{user}` sender, so the existing cron routing logic in `router.rs` handles trust resolution and session creation. Each reminder gets its own session via `SessionKind::Cron("reminder-{id}")`.
- The agent has full tool access during the reminder turn — it can call `bash`, `read_file`, Signal tools, memory tools, etc. This is what enables "turn off the lights" or "check the weather" reminders.
- On dispatch failure (provider error, etc.), the raw reminder text is delivered as a fallback so the user still gets the nudge.
- The `announce_to_session` step (same as cron) injects into the user's DM session, letting the agent rephrase naturally before delivery. For group targets, it delivers directly (also same as cron).

### Why not inject into the user's DM session?

The reminder *executes* in a dedicated session, not the user's DM session. This is deliberate:

1. **Concurrency.** The gateway holds a per-session `try_lock()` — if the user is mid-conversation, an injected turn would either block until they finish or be skipped entirely. Neither is acceptable for a time-sensitive reminder.

2. **Context staleness.** By the time a reminder fires, the user's conversation may have moved on entirely. "Remind me about this" — "this" refers to the conversation at *creation* time, not fire time. The agent resolves context-dependent references into the `message` field when creating the reminder. When it can't fully resolve context, the reminder stores `source_session` — the reminder session agent can `read_file` on the session JSONL to recover the original conversation.

3. **Session pollution.** Action reminders involve tool calls (bash, API calls) that would pollute the user's conversation history with unrelated tool_use/tool_result pairs, bloating the context window.

The *delivery* phase does touch the DM session via `announce_to_session` — but that's a lightweight rephrasing turn, not the execution. The agent sees the user's recent conversation context there and can say "hey, earlier you asked me to..." naturally.

### Waking the scheduler for new reminders

When the tool adds a reminder, the scheduler might be sleeping until a distant cron fire. It needs to wake up and recompute the next fire time.

Reuse the existing `cron_notify: Option<Arc<tokio::sync::Notify>>` mechanism. The `ReminderTool` holds a clone of the same `Notify`, and calls `notify.notify_one()` after adding a reminder.

```rust
#[derive(Debug)]
pub(crate) struct ReminderTool {
    store: ReminderStore,
    config: SharedConfig,
    scheduler_notify: Arc<tokio::sync::Notify>,
}

impl ReminderTool {
    pub fn new(
        store: ReminderStore,
        config: SharedConfig,
        scheduler_notify: Arc<tokio::sync::Notify>,
    ) -> Self {
        Self { store, config, scheduler_notify }
    }
}

// In handle_set, after store.add(reminder):
self.scheduler_notify.notify_one();
```

### Tool registration

The `ReminderTool` is a native tool that needs access to `SharedConfig` and `ReminderStore`. It belongs in `coop-gateway` (not `coop-core`) because it depends on gateway-specific types (`SharedConfig`, `ReminderStore`).

Create `crates/coop-gateway/src/reminder.rs` containing `Reminder`, `ReminderStore`, `ReminderTool`, and `resolve_reminder_delivery`.

Register `ReminderTool` in `cmd_start()` in `main.rs` alongside the other executors:

```rust
// In cmd_start():
let reminder_store = ReminderStore::new(&state_dir)?;  // same dir as DiskSessionStore
let scheduler_notify = Arc::new(tokio::sync::Notify::new());

// ... existing executor setup ...
let reminder_executor = ReminderToolExecutor::new(
    reminder_store.clone(),
    Arc::clone(&shared),
    Arc::clone(&scheduler_notify),
);

let mut executors: Vec<Box<dyn coop_core::ToolExecutor>> = vec![
    Box::new(default_executor),
    Box::new(config_executor),
    Box::new(memory_executor),
    Box::new(reminder_executor),  // <-- new
];

// ... existing signal tool setup ...

// Pass reminder store and notify to scheduler:
tokio::spawn(async move {
    scheduler::run_scheduler_with_notify(
        sched_config,
        sched_router,
        deliver_tx,
        sched_token,
        Some(scheduler_notify),
        Some(reminder_store),
    ).await;
});
```

The `ReminderToolExecutor` is a thin wrapper implementing `ToolExecutor` for a single tool, following the same pattern as `ConfigToolExecutor`:

```rust
pub(crate) struct ReminderToolExecutor {
    tool: ReminderTool,
}

impl ReminderToolExecutor {
    pub fn new(
        store: ReminderStore,
        config: SharedConfig,
        scheduler_notify: Arc<tokio::sync::Notify>,
    ) -> Self {
        Self {
            tool: ReminderTool::new(store, config, scheduler_notify),
        }
    }
}

#[async_trait]
impl ToolExecutor for ReminderToolExecutor {
    async fn execute(
        &self,
        name: &str,
        arguments: serde_json::Value,
        ctx: &ToolContext,
    ) -> Result<ToolOutput> {
        if name == "reminder" {
            self.tool.execute(arguments, ctx).await
        } else {
            Ok(ToolOutput::error(format!("unknown tool: {name}")))
        }
    }

    fn tools(&self) -> Vec<ToolDef> {
        vec![self.tool.definition()]
    }
}
```

### Trust requirements

The `reminder` tool requires `Full` or `Inner` trust (same as `bash`). Add a trust check at the top of `execute()`:

```rust
if ctx.trust > TrustLevel::Inner {
    return Ok(ToolOutput::error(
        "reminder tool requires Full or Inner trust level",
    ));
}
```

### TUI integration (`cmd_tui`)

The TUI command (`cmd_tui`) in `main.rs` also creates executors. Add the reminder executor there too, following the same pattern. The TUI's scheduler (if one exists) or a new background task needs the `ReminderStore`.

If the TUI doesn't run the scheduler (check `cmd_tui` — it may not spawn `run_scheduler`), you'll need to add a simple reminder-firing loop for TUI mode. Since TUI users are local (terminal), reminders from TUI sessions won't typically have delivery targets (terminal is filtered out). This is fine — the tool will return an error explaining that a non-terminal channel is needed. If Signal is configured alongside TUI, the existing scheduler handles delivery.

## Implementation Steps

1. **Create `crates/coop-gateway/src/reminder.rs`** with:
   - `Reminder` struct (derives `Serialize`, `Deserialize` for JSON persistence)
   - `ReminderStore` (with `new(dir)`, `add`, `take_due`, `list_for_user`, `cancel`, `next_fire_time`; writes `reminders.json` on every mutation, loads on construction)
   - `ReminderTool` (implements `Tool`)
   - `ReminderToolExecutor` (implements `ToolExecutor`)
   - `resolve_reminder_delivery` function
   - Unit tests for `ReminderStore` operations, persistence round-trips, and delivery resolution

2. **Modify `crates/coop-gateway/src/scheduler.rs`**:
   - Update `run_scheduler_with_notify` signature to accept `Option<ReminderStore>`
   - Update `run_scheduler` (test helper) to pass `None` for reminders
   - At top of main loop, call `store.take_due()` and fire due reminders
   - When computing next sleep time, take `min(next_cron, next_reminder)`
   - Add `fire_reminder` function that dispatches through `router.dispatch_collect_text_with_channel()` (same as `fire_cron`), then delivers via `announce_to_session` (same two-phase pattern as cron delivery). On dispatch failure, fall back to delivering the raw reminder text.
   - `fire_reminder` needs `router`, `deliver_tx`, and `shared_config` — same args as `fire_cron`
   - Reuse existing `announce_to_session` and `deliver_to_target` (already `pub(crate)` or in the same file)
   - Update all existing callers to pass `None` for reminders where not needed

3. **Register the tool in `main.rs`**:
   - Add `mod reminder;`
   - In `cmd_start()`: create `ReminderStore`, `scheduler_notify`, `ReminderToolExecutor`
   - Add `ReminderToolExecutor` to the executors list
   - Pass `reminder_store` and `scheduler_notify` to `run_scheduler_with_notify`
   - In `cmd_tui()`: same executor setup (if the scheduler runs in TUI mode)

4. **Add tracing** per AGENTS.md rules:
   - `info!` when a reminder is scheduled (id, fire_at, user, delivery targets)
   - `info!` when a reminder fires (id, message, user)
   - `info!` when a reminder is delivered (channel, target)
   - `info!` when a reminder is cancelled (id)
   - `warn!` when delivery targets can't be resolved
   - `reminder_fired` span wrapping each fire

5. **Add `config_check` validation**: In `config_check::validate_config`, no new checks are needed since reminders don't have config — they're runtime-only. But verify that the existing user match pattern validation still covers the delivery resolution path.

6. **Tests**:
   - Unit tests in `reminder.rs` (see Testing section)
   - Integration test: schedule a reminder, advance time, verify delivery
   - Scheduler integration: verify reminder wakes scheduler from cron sleep

7. **Verify build times**: `touch crates/coop-gateway/src/main.rs && time cargo build` must stay under 1.5s. No new dependencies are needed (chrono, uuid, serde are already in the workspace).

## File placement

All reminder code goes in `crates/coop-gateway/src/reminder.rs`. This follows the project pattern:
- `scheduler.rs` — cron scheduling
- `heartbeat.rs` — heartbeat token handling
- `reminder.rs` — one-off reminder scheduling

The file should stay under 500 lines (per compile time rules). The `Reminder`, `ReminderStore` (with file I/O), `ReminderTool`, `ReminderToolExecutor`, `resolve_reminder_delivery`, and tests should fit comfortably within that. The persistence logic is ~30 lines (load + flush + write), following the same pattern as `CompactionStore`.

## Config Example

No config changes needed. Reminders are created at runtime via the tool. The delivery targets are resolved from the existing user match patterns:

```toml
# Existing config — no changes needed for reminders.
# Alice's Signal channel is automatically used for reminder delivery.
[[users]]
name = "alice"
trust = "full"
match = ["terminal:default", "signal:alice-uuid"]
```

## Example Conversations

### Simple nudge
```
User: Hey, remind me at 3pm today to call the dentist.

Agent: [calls reminder tool: {action: "set", time: "2026-02-11T15:00:00Z",
        message: "Remind the user to call the dentist."}]

Tool output: Reminder scheduled (id: rem_abc123) for 2026-02-11T15:00:00Z

Agent: Done! I'll remind you at 3pm UTC to call the dentist.
```

At 3:00 PM, the scheduler fires the reminder. The agent runs a turn in a reminder session, produces "Hey, just a heads up — you wanted to call the dentist!", which gets injected into Alice's DM session and delivered via Signal.

### Action reminder — agent knows how
```
User: In 30 minutes, turn off the kitchen lights.

Agent: [reads TOOLS.md, finds Home Assistant config]
Agent: [calls reminder tool: {action: "set", time: "2026-02-11T04:09:00Z",
        message: "Turn off the kitchen lights. Run: curl -X POST
                  http://homeassistant.local:8123/api/services/light/turn_off
                  -H 'Authorization: Bearer <token from TOOLS.md>'
                  -d '{\"entity_id\": \"light.kitchen\"}'"}]

Tool output: Reminder scheduled (id: rem_def456) for 2026-02-11T04:09:00Z

Agent: Got it, I'll turn off the kitchen lights in 30 minutes.
```

The agent checked TOOLS.md, found the Home Assistant setup, and wrote a complete execution plan into the message. At fire time, the reminder session agent runs the curl command directly — no guessing needed.

### Action reminder — agent asks first
```
User: In 30 minutes, turn off the kitchen lights.

Agent: I can set that reminder, but I want to make sure I can actually
       do it when the time comes. How do you control your lights — is
       there a Home Assistant instance, a smart plug, or something else
       I should call?

User: Yeah, Home Assistant at ha.local:8123, bearer token is in TOOLS.md.

Agent: [calls reminder tool: {action: "set", time: "2026-02-11T04:09:00Z",
        message: "Turn off the kitchen lights. Run: curl -X POST
                  http://ha.local:8123/api/services/light/turn_off
                  -H 'Authorization: Bearer <token from TOOLS.md>'
                  -d '{\"entity_id\": \"light.kitchen\"}'"}]

Tool output: Reminder scheduled (id: rem_def456) for 2026-02-11T04:09:00Z

Agent: All set — I'll turn off the kitchen lights in 30 minutes.
```

The agent didn't know how to control the lights, so it asked before creating the reminder. Now the message has a concrete plan.

### List and cancel
```
User: What reminders do I have?

Agent: [calls reminder tool: {action: "list"}]

Tool output:
- [rem_abc123] 2026-02-11T15:00:00Z → "Remind the user to call the dentist."
- [rem_def456] 2026-02-11T04:09:00Z → "Turn off the kitchen lights..."

Agent: You have two reminders:
1. Today at 3pm — Call the dentist
2. In about 25 minutes — Turn off the kitchen lights
```

```
User: Cancel the dentist reminder.

Agent: [calls reminder tool: {action: "cancel", id: "rem_abc123"}]

Tool output: Reminder rem_abc123 cancelled.

Agent: Done, I've cancelled the dentist reminder.
```

## Testing

### Unit tests (`reminder.rs`)

```rust
fn make_store() -> (ReminderStore, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let store = ReminderStore::new(dir.path()).unwrap();
    (store, dir)
}

fn sample_reminder(id: &str, hours_from_now: i64, user: &str) -> Reminder {
    Reminder {
        id: id.to_owned(),
        fire_at: Utc::now() + chrono::Duration::hours(hours_from_now),
        message: format!("reminder {id}"),
        user: Some(user.to_owned()),
        delivery: vec![("signal".to_owned(), format!("{user}-uuid"))],
        source_session: "test:dm:signal:alice-uuid".to_owned(),
        created_at: Utc::now(),
    }
}

#[test]
fn store_add_and_list() {
    let (store, _dir) = make_store();
    store.add(sample_reminder("rem_1", 1, "alice"));
    let list = store.list_for_user("alice");
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].id, "rem_1");
}

#[test]
fn store_persists_to_disk_and_survives_reload() {
    let dir = tempfile::tempdir().unwrap();

    // Add a reminder in one store instance.
    {
        let store = ReminderStore::new(dir.path()).unwrap();
        store.add(sample_reminder("rem_persist", 1, "alice"));
    }

    // Create a new store from the same directory — should load from disk.
    let store2 = ReminderStore::new(dir.path()).unwrap();
    let list = store2.list_for_user("alice");
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].id, "rem_persist");
}

#[test]
fn store_cancel_persists_removal_to_disk() {
    let dir = tempfile::tempdir().unwrap();

    {
        let store = ReminderStore::new(dir.path()).unwrap();
        store.add(sample_reminder("rem_1", 1, "alice"));
        store.add(sample_reminder("rem_2", 2, "alice"));
        assert!(store.cancel("rem_1"));
    }

    // Reload — only rem_2 should remain.
    let store2 = ReminderStore::new(dir.path()).unwrap();
    let list = store2.list_for_user("alice");
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].id, "rem_2");
}

#[test]
fn store_take_due_persists_to_disk() {
    let dir = tempfile::tempdir().unwrap();

    {
        let store = ReminderStore::new(dir.path()).unwrap();
        // One past-due, one future.
        store.add(Reminder {
            id: "rem_past".to_owned(),
            fire_at: Utc::now() - chrono::Duration::seconds(10),
            message: "overdue".to_owned(),
            user: Some("alice".to_owned()),
            delivery: vec![],
            source_session: "test:main".to_owned(),
            created_at: Utc::now(),
        });
        store.add(sample_reminder("rem_future", 1, "alice"));

        let due = store.take_due();
        assert_eq!(due.len(), 1);
        assert_eq!(due[0].id, "rem_past");
    }

    // Reload — only the future reminder should remain.
    let store2 = ReminderStore::new(dir.path()).unwrap();
    let list = store2.list_for_user("alice");
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].id, "rem_future");
}

#[test]
fn store_loads_empty_when_no_file() {
    let dir = tempfile::tempdir().unwrap();
    let store = ReminderStore::new(dir.path()).unwrap();
    assert!(store.next_fire_time().is_none());
}

#[test]
fn store_take_due_removes_past_reminders() {
    let (store, _dir) = make_store();
    store.add(Reminder {
        id: "rem_past".to_owned(),
        fire_at: Utc::now() - chrono::Duration::seconds(10),
        message: "overdue".to_owned(),
        user: Some("alice".to_owned()),
        delivery: vec![],
        source_session: "test:main".to_owned(),
        created_at: Utc::now(),
    });
    store.add(sample_reminder("rem_future", 1, "alice"));

    let due = store.take_due();
    assert_eq!(due.len(), 1);
    assert_eq!(due[0].id, "rem_past");

    let remaining = store.list_for_user("alice");
    assert_eq!(remaining.len(), 1);
    assert_eq!(remaining[0].id, "rem_future");
}

#[test]
fn store_cancel_removes_by_id() {
    let (store, _dir) = make_store();
    store.add(sample_reminder("rem_1", 1, "alice"));
    assert!(store.cancel("rem_1"));
    assert!(store.list_for_user("alice").is_empty());
}

#[test]
fn store_cancel_returns_false_for_unknown_id() {
    let (store, _dir) = make_store();
    assert!(!store.cancel("rem_nonexistent"));
}

#[test]
fn store_next_fire_time() {
    let (store, _dir) = make_store();
    assert!(store.next_fire_time().is_none());

    let t1 = Utc::now() + chrono::Duration::hours(2);
    let t2 = Utc::now() + chrono::Duration::hours(1);
    store.add(Reminder {
        id: "rem_1".to_owned(),
        fire_at: t1,
        message: "later".to_owned(),
        user: None,
        delivery: vec![],
        source_session: "test:main".to_owned(),
        created_at: Utc::now(),
    });
    store.add(Reminder {
        id: "rem_2".to_owned(),
        fire_at: t2,
        message: "sooner".to_owned(),
        user: None,
        delivery: vec![],
        source_session: "test:main".to_owned(),
        created_at: Utc::now(),
    });

    assert_eq!(store.next_fire_time(), Some(t2));
}

#[test]
fn store_list_filters_by_user() {
    let (store, _dir) = make_store();
    store.add(sample_reminder("rem_alice", 1, "alice"));
    store.add(sample_reminder("rem_bob", 1, "bob"));

    let alice_reminders = store.list_for_user("alice");
    assert_eq!(alice_reminders.len(), 1);
    assert_eq!(alice_reminders[0].id, "rem_alice");
}

#[test]
fn resolve_delivery_uses_originating_signal_channel() {
    let config: Config = toml::from_str(r#"
[agent]
id = "test"
model = "test"

[[users]]
name = "alice"
trust = "full"
match = ["terminal:default", "signal:alice-uuid", "telegram:alice-tg"]
"#).unwrap();

    // Created from Signal DM — should deliver to Signal only, not Telegram.
    let targets = resolve_reminder_delivery(
        &config, "test:dm:signal:alice-uuid", Some("alice"),
    );
    assert_eq!(targets, vec![("signal".to_owned(), "alice-uuid".to_owned())]);
}

#[test]
fn resolve_delivery_uses_originating_telegram_channel() {
    let config: Config = toml::from_str(r#"
[agent]
id = "test"
model = "test"

[[users]]
name = "alice"
trust = "full"
match = ["signal:alice-uuid", "telegram:alice-tg"]
"#).unwrap();

    // Created from Telegram DM — should deliver to Telegram only.
    let targets = resolve_reminder_delivery(
        &config, "test:dm:telegram:alice-tg", Some("alice"),
    );
    assert_eq!(targets, vec![("telegram".to_owned(), "alice-tg".to_owned())]);
}

#[test]
fn resolve_delivery_terminal_falls_back_to_all_channels() {
    let config: Config = toml::from_str(r#"
[agent]
id = "test"
model = "test"

[[users]]
name = "alice"
trust = "full"
match = ["terminal:default", "signal:alice-uuid"]
"#).unwrap();

    // Created from terminal (main session) — can't push to terminal,
    // falls back to all non-terminal channels.
    let targets = resolve_reminder_delivery(
        &config, "test:main", Some("alice"),
    );
    assert_eq!(targets, vec![("signal".to_owned(), "alice-uuid".to_owned())]);
}

#[test]
fn resolve_delivery_empty_for_terminal_only_user() {
    let config: Config = toml::from_str(r#"
[agent]
id = "test"
model = "test"

[[users]]
name = "alice"
trust = "full"
match = ["terminal:default"]
"#).unwrap();

    // Terminal-only user, created from terminal — no delivery targets.
    let targets = resolve_reminder_delivery(
        &config, "test:main", Some("alice"),
    );
    assert!(targets.is_empty());
}

#[test]
fn resolve_delivery_empty_for_unknown_user() {
    let config: Config = toml::from_str(r#"
[agent]
id = "test"
model = "test"
"#).unwrap();

    let targets = resolve_reminder_delivery(
        &config, "test:dm:signal:mallory-uuid", Some("mallory"),
    );
    // Originating channel is signal:mallory-uuid — still works even
    // if user isn't in config (we have the channel from session key).
    assert_eq!(targets, vec![("signal".to_owned(), "mallory-uuid".to_owned())]);
}

#[test]
fn resolve_delivery_empty_for_none_user_and_main_session() {
    let config: Config = toml::from_str(r#"
[agent]
id = "test"
model = "test"
"#).unwrap();

    // No user, main session — nothing to deliver to.
    let targets = resolve_reminder_delivery(
        &config, "test:main", None,
    );
    assert!(targets.is_empty());
}
```

### Tool execution tests

```rust
fn make_tool() -> (ReminderTool, ReminderStore, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let store = ReminderStore::new(dir.path()).unwrap();
    let config = shared_config(test_config_with_signal_user());
    let notify = Arc::new(tokio::sync::Notify::new());
    let tool = ReminderTool::new(store.clone(), config, notify);
    (tool, store, dir)
}

#[tokio::test]
async fn tool_set_creates_reminder() {
    let (tool, store, _dir) = make_tool();

    let future_time = (Utc::now() + chrono::Duration::hours(1))
        .to_rfc3339();

    let output = tool.execute(
        serde_json::json!({
            "action": "set",
            "time": future_time,
            "message": "call the dentist"
        }),
        &tool_context_with_user("alice"),
    ).await.unwrap();

    assert!(!output.is_error);
    assert!(output.content.contains("Reminder scheduled"));

    let reminders = store.list_for_user("alice");
    assert_eq!(reminders.len(), 1);
    assert_eq!(reminders[0].message, "call the dentist");
}

#[tokio::test]
async fn tool_set_persists_to_disk() {
    let dir = tempfile::tempdir().unwrap();
    let store = ReminderStore::new(dir.path()).unwrap();
    let config = shared_config(test_config_with_signal_user());
    let notify = Arc::new(tokio::sync::Notify::new());
    let tool = ReminderTool::new(store, config, notify);

    let future_time = (Utc::now() + chrono::Duration::hours(1))
        .to_rfc3339();

    tool.execute(
        serde_json::json!({
            "action": "set",
            "time": future_time,
            "message": "persisted reminder"
        }),
        &tool_context_with_user("alice"),
    ).await.unwrap();

    // Reload from same directory — reminder should survive.
    let store2 = ReminderStore::new(dir.path()).unwrap();
    let list = store2.list_for_user("alice");
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].message, "persisted reminder");
}

#[tokio::test]
async fn tool_set_rejects_past_time() {
    let (tool, _, _dir) = make_tool();

    let past_time = (Utc::now() - chrono::Duration::hours(1))
        .to_rfc3339();

    let output = tool.execute(
        serde_json::json!({
            "action": "set",
            "time": past_time,
            "message": "too late"
        }),
        &tool_context_with_user("alice"),
    ).await.unwrap();

    assert!(output.is_error);
    assert!(output.content.contains("future"));
}

#[tokio::test]
async fn tool_set_rejects_no_delivery_channel() {
    let dir = tempfile::tempdir().unwrap();
    let store = ReminderStore::new(dir.path()).unwrap();
    // Config with terminal-only user.
    let config = shared_config(test_config_terminal_only());
    let notify = Arc::new(tokio::sync::Notify::new());
    let tool = ReminderTool::new(store, config, notify);

    let future_time = (Utc::now() + chrono::Duration::hours(1))
        .to_rfc3339();

    let output = tool.execute(
        serde_json::json!({
            "action": "set",
            "time": future_time,
            "message": "no channel"
        }),
        &tool_context_with_user("alice"),
    ).await.unwrap();

    assert!(output.is_error);
    assert!(output.content.contains("no delivery channel"));
}

#[tokio::test]
async fn tool_list_shows_pending_reminders() {
    let (tool, store, _dir) = make_tool();
    store.add(sample_reminder("rem_1", 1, "alice"));

    let output = tool.execute(
        serde_json::json!({"action": "list"}),
        &tool_context_with_user("alice"),
    ).await.unwrap();

    assert!(!output.is_error);
    assert!(output.content.contains("rem_1"));
}

#[tokio::test]
async fn tool_cancel_removes_reminder() {
    let (tool, store, _dir) = make_tool();
    store.add(sample_reminder("rem_1", 1, "alice"));

    let output = tool.execute(
        serde_json::json!({"action": "cancel", "id": "rem_1"}),
        &tool_context_with_user("alice"),
    ).await.unwrap();

    assert!(!output.is_error);
    assert!(store.list_for_user("alice").is_empty());
}

#[tokio::test]
async fn tool_rejects_low_trust() {
    let (tool, _, _dir) = make_tool();

    let output = tool.execute(
        serde_json::json!({"action": "list"}),
        &tool_context_with_trust(TrustLevel::Familiar),
    ).await.unwrap();

    assert!(output.is_error);
    assert!(output.content.contains("trust"));
}
```

### Scheduler integration tests

```rust
#[tokio::test]
async fn scheduler_fires_due_reminders() {
    // Setup: FakeProvider, DefaultExecutor, Gateway, Router — same
    // pattern as existing scheduler tests (make_shared_config_and_router).
    let dir = tempfile::tempdir().unwrap();
    let store = ReminderStore::new(dir.path()).unwrap();
    store.add(Reminder {
        id: "rem_now".to_owned(),
        fire_at: Utc::now() - chrono::Duration::seconds(1),
        message: "fire now".to_owned(),
        user: Some("alice".to_owned()),
        delivery: vec![("signal".to_owned(), "alice-uuid".to_owned())],
        source_session: "test:dm:signal:alice-uuid".to_owned(),
        created_at: Utc::now(),
    });

    let (tx, mut rx) = mpsc::channel::<OutboundMessage>(8);
    let deliver_tx = DeliverySender::new(tx);

    // ... set up shared config, router via make_shared_config_and_router ...
    // Run scheduler with Some(store.clone()), deliver_tx, for ~2s.

    // The agent runs a turn on the reminder message, then the response
    // is announced into the DM session and delivered.
    let msg = rx.try_recv().unwrap();
    assert_eq!(msg.channel, "signal");
    assert_eq!(msg.target, "alice-uuid");
    // Content is the FakeProvider's response, not the raw reminder text
    // (because it goes through the agent + announce_to_session).

    // Reminder should be removed from store (and disk).
    assert!(store.list_for_user("alice").is_empty());
    let store2 = ReminderStore::new(dir.path()).unwrap();
    assert!(store2.list_for_user("alice").is_empty());
}

#[tokio::test]
async fn scheduler_fires_late_reminders_on_startup() {
    // Simulate gateway downtime: write a past-due reminder to disk,
    // then start the scheduler — it should fire immediately.
    let dir = tempfile::tempdir().unwrap();
    let store = ReminderStore::new(dir.path()).unwrap();
    store.add(Reminder {
        id: "rem_missed".to_owned(),
        fire_at: Utc::now() - chrono::Duration::minutes(30),
        message: "you missed this".to_owned(),
        user: Some("alice".to_owned()),
        delivery: vec![("signal".to_owned(), "alice-uuid".to_owned())],
        source_session: "test:dm:signal:alice-uuid".to_owned(),
        created_at: Utc::now() - chrono::Duration::hours(1),
    });

    // Drop and reload to simulate restart.
    drop(store);
    let store = ReminderStore::new(dir.path()).unwrap();
    assert_eq!(store.list_for_user("alice").len(), 1);

    let (tx, mut rx) = mpsc::channel::<OutboundMessage>(8);
    let deliver_tx = DeliverySender::new(tx);

    // ... set up scheduler with Some(store.clone()), run briefly ...

    // Past-due reminder fires on first loop iteration.
    // Goes through full agent turn + announce, so content is FakeProvider output.
    let msg = rx.try_recv().unwrap();
    assert_eq!(msg.channel, "signal");
    assert_eq!(msg.target, "alice-uuid");
    assert!(store.list_for_user("alice").is_empty());
}

#[tokio::test]
async fn scheduler_wakes_for_new_reminder() {
    // Start scheduler with only a distant cron entry.
    // Add a reminder via the store + notify.
    // Verify the scheduler fires the reminder within ~2s,
    // not waiting for the distant cron.
}

#[tokio::test]
async fn reminder_dispatch_failure_delivers_fallback_text() {
    // Use FailingProvider. Schedule a reminder that fires immediately.
    // Verify that the raw reminder text is delivered as a fallback
    // when the agent turn fails.
}
```

### Startup behavior

On gateway start, `ReminderStore::new()` loads `reminders.json` from the state directory. Any reminders whose `fire_at` is already past will be picked up by `take_due()` on the first scheduler loop iteration and fired immediately — running a full agent turn just like a normal reminder fire. This handles the case where the gateway was down when a reminder should have fired — the user still gets the reminder, just late.

Late delivery is detected and logged inside `fire_reminder` (see the lateness check in the code above). The agent's system prompt prefix mentions it's a scheduled reminder but doesn't mention lateness — the agent naturally responds to the content. If an action reminder ("turn off the lights") is late, the agent still executes it, which is the right behavior.

## Not in Scope

- **Recurring reminders.** Use cron entries for recurring schedules. The reminder tool is for one-shots only.
- **Timezone handling.** The tool accepts UTC timestamps. The LLM is responsible for converting "3pm" to the correct UTC time based on conversation context. A future iteration could accept timezone info.
- **Snooze / reschedule.** Cancel and re-create for now.
- **Rich delivery.** The reminder message is delivered as plain text. No formatting, no attachments.
- **Reminder editing.** Cancel + re-create.

## Dependencies

No new crate dependencies. Everything needed (`chrono`, `uuid`, `serde` + `serde_json`, `tokio`, `tracing`) is already in the workspace. The JSON file I/O uses `std::fs` (already used by `DiskSessionStore` and `CompactionStore`).

## Tracing

Per AGENTS.md rules:

- `info!` on reminder creation (id, fire_at, user, delivery count)
- `info!` on reminder fire (id, message, user)
- `info!` on delivery (channel, target, content_len)
- `info!` on cancellation (id)
- `warn!` on delivery failure or missing targets
- `reminder_fired` span wrapping each fire

Verify after implementation:
```bash
COOP_TRACE_FILE=traces.jsonl cargo run -- start
# Schedule a reminder, wait for it to fire
grep "reminder" traces.jsonl | jq .
```
