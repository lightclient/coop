# Prompt: Group Chat Support

## Goal

Add `[[groups]]` config to coop so the agent can respond to messages in group chats. Currently **all group messages are silently rejected** because (a) a blanket trust ceiling caps all group senders to `Familiar`, and (b) the authorization gate requires `trust <= Full`. This feature replaces the blanket ceiling with per-sender trust, per-group `default_trust` for unknown senders, an optional `trust_ceiling` to prevent private info leaking into group-visible responses, and flexible trigger modes that control *when* the agent responds.

## Background: Current Flow

1. Signal channel parses inbound messages, setting `is_group: true` and `chat_id: Some("group:<hex>")` for group messages. Message content is prefixed with `[from <sender-uuid> in group:<hex> at <timestamp>]`.
2. Router's `route_message()` caps trust to `TrustLevel::Familiar` for group contexts (regardless of sender identity) and routes to `SessionKind::Group("signal:group:<hex>")`.
3. `is_trust_authorized()` requires `trust <= TrustLevel::Full`. Since `Familiar > Full`, every group message is rejected silently.

**The blanket ceiling is wrong.** Trust should come from the sender's identity, not a hard-coded group cap. The fix: remove the blanket ceiling, add per-group config with `default_trust` for unknown senders (defaults to `Familiar`) and an optional `trust_ceiling` that caps everyone in the group. The ceiling prevents the agent from accessing private memory and leaking sensitive info into a group-visible response.

Key files to understand before starting:
- `crates/coop-gateway/src/router.rs` — routing logic, `route_message()`, `is_trust_authorized()`
- `crates/coop-gateway/src/config.rs` — all config structs, deserialization
- `crates/coop-gateway/src/config_check.rs` — `validate_config()` checks
- `crates/coop-gateway/src/signal_loop.rs` — Signal message dispatch loop
- `crates/coop-gateway/src/gateway.rs` — `run_turn_with_trust()`, prompt building, `build_prompt()`
- `crates/coop-gateway/src/trust.rs` — `resolve_trust()`
- `crates/coop-core/src/types.rs` — `InboundMessage`, `SessionKind`, `TrustLevel`
- `crates/coop-core/src/traits.rs` — `Provider` trait, `complete`, `complete_fast`
- `crates/coop-agent/src/anthropic_provider.rs` — `AnthropicProvider::new()`, `from_key_refs()`, `set_model()`
- `crates/coop-gateway/src/main.rs` — `create_provider()`, service startup
- `crates/coop-gateway/src/init_templates.rs` — default config template
- `crates/coop-gateway/src/init.rs` — `generate_config()` for `coop init`

## Comparison with OpenClaw

OpenClaw has a mature group chat system. Here are the key features and how our design compares:

### What OpenClaw does

1. **Two activation modes**: `always` and `mention` (per-group configurable via the channel's `groups` map and a `/activation` command).

2. **Silent reply token (`NO_REPLY`)**: In `always` mode, the LLM sees every message but can respond with exactly `NO_REPLY` to stay silent. The group intro system prompt tells the agent: "If no response is needed, reply with exactly NO_REPLY." This lets the main model decide naturally whether to respond.

3. **Pending group history**: In `mention` mode, messages that don't trigger a response are buffered in memory (up to `historyLimit`, default 50). When the agent IS finally mentioned, all pending messages are injected as `[Chat messages since your last reply - for context]` before the current message. This gives the agent conversational context. History is cleared after each agent reply.

4. **Group intro system prompt**: A system prompt block is injected for group sessions containing:
   - "You are replying inside a Signal group chat."
   - Activation mode description (always-on vs mention-only)
   - Silent token instructions (for always mode)
   - "Be a good group participant: mostly lurk and follow the conversation; reply only when directly addressed or you can add clear value."
   - "Address the specific sender noted in the message context."

5. **Implicit mention (reply-to detection)**: If someone replies to the bot's own message, that counts as an implicit mention even without name-matching. Natural group UX.

6. **Mention pattern config**: `mentionPatterns` is an array of regex patterns derived from the agent's identity name/emoji, configurable per-agent or globally. Supports WhatsApp @-mention JID resolution, phone number detection, etc.

7. **Untrusted context separation**: Sender names, group subjects, and chat history are placed in user-role "untrusted context" blocks, not system prompts. Prevents prompt injection from group member names.

8. **Group member tracking**: Tracks sender names/IDs per group to build a participant roster for context.

9. **Ack reactions**: Reacts with 👀 to acknowledge receipt before processing starts (configurable via `reactionLevel`).

10. **Inbound debouncing**: Multiple rapid messages from the same sender in the same group are batched into a single agent call.

### What OpenClaw does NOT have

- **LLM-based trigger gating with a cheap model** — OpenClaw uses either `always` (LLM sees everything, full cost) or `mention` (pattern-based, zero LLM cost). There is no middle ground where a cheap model pre-screens messages before the expensive model runs. In chatty groups, `always` mode sends every message to the expensive model which burns through tokens. Our `llm` trigger mode fills this gap.

### What we adopt from OpenClaw

- **Silent reply token** — for `always` mode, the LLM can output `NO_REPLY` to stay silent
- **Pending group history** — non-triggering messages buffered and injected as context when triggered
- **Group intro system prompt** — activation mode, etiquette, silent token instructions

### What we add beyond OpenClaw

- **`trigger = "llm"` mode** — a cheap model (e.g. haiku) pre-screens every message with the full conversation context (same system prompt, same history, same pending messages). Only messages classified as YES get escalated to the expensive main model. This saves significant tokens in chatty groups where most messages are irrelevant chatter.
- **`trigger = "regex"` mode** — pattern-based triggering beyond name mentions

## Config Design

Add a `[[groups]]` array to `coop.toml`:

```toml
# Cheap model pre-screens every message, escalates to main model only when needed
[[groups]]
match = ["signal:group:deadbeef0011223344556677..."]
trigger = "llm"
trigger_model = "claude-haiku-3-5-20241022"

# Respond to every message with the main model, LLM decides via NO_REPLY token.
# Cap everyone to Familiar — agent can't leak private info into the group.
[[groups]]
match = ["signal:group:aabbccdd..."]
trigger = "always"
trust_ceiling = { fixed = "familiar" }

# Only respond when mentioned by name.
# Small trusted group — unknown senders get Full, no ceiling.
[[groups]]
match = ["signal:group:11223344..."]
trigger = "mention"
mention_names = ["coop", "cooper", "@coop"]
default_trust = "full"

# Dynamic ceiling: if an unknown (Familiar) person is in the group,
# everyone gets capped to Familiar. If all members are Full+, ceiling is Full.
[[groups]]
match = ["signal:group:99aabbcc..."]
trigger = "mention"
mention_names = ["coop"]
trust_ceiling = "min_member"

# Only respond when message matches regex pattern
[[groups]]
match = ["signal:group:55667788..."]
trigger = "regex"
trigger_regex = "^!(ask|hey)"
```

### Trigger mode comparison

| Mode | Cost | How it works |
|------|------|-------------|
| `always` | High — every message hits main model | Main model sees every message. Can output `NO_REPLY` to stay silent. Best for small/important groups. |
| `llm` | Medium — every message hits cheap model, some hit main | Cheap model (haiku) pre-screens with full context. Only escalates to main model on YES. Best for chatty groups. |
| `mention` | Zero LLM cost for non-mentions | Pattern match on agent names. Pending history provides context when triggered. Best default. |
| `regex` | Zero LLM cost for non-matches | Regex pattern match on message body. Same history buffering as mention. |

### Config struct

```rust
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct GroupConfig {
    /// Group identifiers to match. Format: "signal:group:<hex>".
    /// A special value "*" matches all groups on that channel.
    pub r#match: Vec<String>,

    /// When to trigger a response. Default: "mention".
    #[serde(default = "default_group_trigger")]
    pub trigger: GroupTrigger,

    /// Names the agent responds to when trigger = "mention".
    /// Matched case-insensitively anywhere in the message text.
    #[serde(default)]
    pub mention_names: Vec<String>,

    /// Regex pattern when trigger = "regex". Matched against raw message
    /// text (after the [from ...] prefix is stripped).
    #[serde(default)]
    pub trigger_regex: Option<String>,

    /// Model for trigger = "llm" classification. This cheap model
    /// pre-screens messages with the full conversation context.
    /// Default: "claude-haiku-3-5-20241022".
    #[serde(default)]
    pub trigger_model: Option<String>,

    /// Custom system prompt appended for trigger = "llm" classification.
    /// The model receives the full normal system prompt + conversation
    /// history, with this appended as an additional instruction.
    /// Default: built-in classification prompt (see DEFAULT_TRIGGER_PROMPT).
    #[serde(default)]
    pub trigger_prompt: Option<String>,

    /// Trust level assigned to unknown senders (not in [[users]]).
    /// Default: Familiar (access to social memory only).
    #[serde(default = "default_group_trust")]
    pub default_trust: TrustLevel,

    /// Trust ceiling mode for this group. Controls how the maximum effective
    /// trust is determined. Prevents the agent from leaking private info
    /// into a group-visible response.
    ///
    /// - `none`: No ceiling. Users keep their configured trust. (Default)
    /// - `fixed:<level>`: Static ceiling. All users capped to this level.
    ///   Example: `fixed:familiar` caps everyone to social-only memory.
    /// - `min_member`: Dynamic ceiling. Queries Signal for the full group
    ///   membership, cross-references with [[users]] config, and uses the
    ///   lowest trust as the ceiling. Unknowns count at `default_trust`.
    ///   If Alice (Owner) and an unknown (Familiar) are in the group,
    ///   Alice gets capped to Familiar too. Cached per session after first query.
    ///
    /// Uses existing `resolve_trust(user_trust, ceiling)`.
    #[serde(default)]
    pub trust_ceiling: TrustCeiling,

    /// Max buffered messages for pending group history context.
    /// Used by mention/regex/llm triggers to provide conversation context
    /// when the agent is finally triggered. Default: 50. Set to 0 to disable.
    #[serde(default = "default_group_history_limit")]
    pub history_limit: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum GroupTrigger {
    /// Respond to every message. The main model sees all messages and can
    /// reply with NO_REPLY to stay silent. High token cost.
    Always,
    /// A cheap model pre-screens every message with the full conversation
    /// context. Only messages classified as relevant are escalated to the
    /// main model. Medium token cost.
    Llm,
    /// Only respond when the agent is mentioned by name, or when
    /// someone replies to the agent's message. Zero LLM cost for skips.
    Mention,
    /// Only respond when the message matches a regex pattern.
    /// Zero LLM cost for skips.
    Regex,
}

/// How to determine the trust ceiling for a group.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum TrustCeiling {
    /// No ceiling. Users keep their configured trust level.
    #[default]
    None,
    /// Static ceiling. All users in this group are capped to this level.
    Fixed(TrustLevel),
    /// Dynamic: ceiling = lowest trust among all group members.
    /// Queries presage for full membership on first message, cross-references
    /// with [[users]] config. Unknown members count at `default_trust`.
    /// Cached per session after first query.
    MinMember,
}

fn default_group_trigger() -> GroupTrigger {
    GroupTrigger::Mention
}

fn default_group_trust() -> TrustLevel {
    TrustLevel::Familiar
}

const fn default_group_history_limit() -> usize {
    50
}

const DEFAULT_TRIGGER_MODEL: &str = "claude-haiku-3-5-20241022";
```

Add `groups: Vec<GroupConfig>` to `Config` with `#[serde(default)]`.

**Design note**: The default trigger is `mention` (not `always` or `llm`) because it has zero LLM cost for non-mentions. Users opt into `llm` or `always` explicitly.

## Implementation Plan

### 1. Config (`config.rs`)

- Add `GroupConfig`, `GroupTrigger`, `DEFAULT_TRIGGER_MODEL` as above
- Add `groups: Vec<GroupConfig>` field to `Config`
- Write tests: parse groups config, defaults, roundtrip, all trigger variants

### 2. Config validation (`config_check.rs`)

Add a `check_groups()` function called from `validate_config()`. Checks:

- `match` is non-empty for each group entry
- `match` patterns look like valid group identifiers (`signal:group:*` or `*`)
- `mention_names` is non-empty when `trigger = "mention"`
- `trigger_regex` is present and compiles when `trigger = "regex"`
- `default_trust` is not `Owner` (warn: granting Owner to unknown senders is dangerous)
- If `trust_ceiling` is set and is more restrictive (higher ord) than `default_trust`, warn (the ceiling will override `default_trust` for unknowns, which may confuse the user)
- No duplicate match patterns across group entries
- Warn if groups are configured but no signal channel is configured

### 3. Silent reply token

Define `SILENT_REPLY_TOKEN = "NO_REPLY"` in `group_trigger.rs`.

**Detection**: After a turn completes, check if the final assistant text matches the silent token. If so, suppress the outbound message (don't send to channel).

The `is_silent_reply()` function should match flexibly: trim whitespace, check if the text is just the token optionally surrounded by whitespace/punctuation (following OpenClaw's approach).

**Where to detect**: In `signal_loop.rs`'s `dispatch_signal_turn_background()`, after collecting the final text:
```rust
if is_silent_reply(&text) {
    debug!(target = target, "suppressing silent reply token");
    text.clear(); // flush_text will skip empty text
}
```

### 4. Group trigger evaluation (new file: `group_trigger.rs`)

Create `crates/coop-gateway/src/group_trigger.rs` (~150 lines).

```rust
pub(crate) const SILENT_REPLY_TOKEN: &str = "NO_REPLY";

pub(crate) enum TriggerDecision {
    Respond,
    Skip,
}

/// Evaluate non-LLM triggers (always, mention, regex).
/// For trigger = "llm", the caller must use Gateway::evaluate_llm_trigger() instead.
pub(crate) fn evaluate_trigger(
    msg: &InboundMessage,
    group_config: &GroupConfig,
) -> TriggerDecision

/// Check if assistant output is the silent reply token.
pub(crate) fn is_silent_reply(text: &str) -> bool

/// Strip the "[from ... at ...]" envelope prefix from a message body.
fn strip_envelope_prefix(content: &str) -> &str
```

Trigger logic for the sync cases:
- **`always`**: Return `Respond`. (Every message goes to the main model.)
- **`mention`**: Strip `[from ...]` prefix, check if any `mention_names` appears case-insensitively.
- **`regex`**: Compile regex (use `regex::Regex` — add `regex` dep to `coop-gateway` only), strip prefix, match.
- **`llm`**: Panic/unreachable — LLM trigger is handled separately by `Gateway::evaluate_llm_trigger()`.

### 5. LLM trigger — the full-context cheap model pre-screen

This is the core differentiator. The cheap model gets **the exact same context** the main model would get: same system prompt, same conversation history, same pending group history. It just decides YES/NO.

#### 5a. Provider registry (new file: `provider_registry.rs`)

Instead of a single `Arc<dyn Provider>` and ad-hoc trigger provider maps, introduce a `ProviderRegistry` — a general-purpose lookup from model name to provider. This serves the immediate need (trigger models) and future needs (per-user models, per-agent models, etc.).

Create `crates/coop-gateway/src/provider_registry.rs` (~80 lines):

```rust
use coop_core::Provider;
use std::collections::HashMap;
use std::sync::Arc;

/// Registry of provider instances keyed by model name.
///
/// Each unique model gets its own provider instance (own HTTP client,
/// own key pool tracking). The primary model is always present.
/// Additional models are registered at startup from config (e.g. group
/// trigger models) or lazily when needed (future: per-user models).
///
/// Providers are fully concurrent — no locks, no set_model() calls.
pub(crate) struct ProviderRegistry {
    primary: Arc<dyn Provider>,
    by_model: HashMap<String, Arc<dyn Provider>>,
}

impl ProviderRegistry {
    pub(crate) fn new(primary: Arc<dyn Provider>) -> Self {
        let model_name = primary.model_info().name.clone();
        let mut by_model = HashMap::new();
        by_model.insert(model_name, Arc::clone(&primary));
        Self { primary, by_model }
    }

    /// Register an additional provider for a specific model.
    pub(crate) fn register(&mut self, model: String, provider: Arc<dyn Provider>) {
        self.by_model.insert(model, provider);
    }

    /// The primary provider (agent.model from config).
    pub(crate) fn primary(&self) -> &Arc<dyn Provider> {
        &self.primary
    }

    /// Look up a provider by model name. Falls back to primary if not found.
    pub(crate) fn get(&self, model: &str) -> &Arc<dyn Provider> {
        self.by_model.get(model).unwrap_or(&self.primary)
    }

    /// Look up a provider by model name. Returns None if not registered.
    pub(crate) fn get_exact(&self, model: &str) -> Option<&Arc<dyn Provider>> {
        self.by_model.get(model)
    }

    /// Update the primary provider's model (for hot-reload).
    pub(crate) fn sync_primary_model(&self, model: &str) {
        self.primary.set_model(model);
    }
}
```

**Startup** (`main.rs`): Build the registry with the primary provider, then register trigger models:

```rust
fn build_provider_registry(config: &Config) -> Result<ProviderRegistry> {
    let primary = create_provider(config)?;
    let mut registry = ProviderRegistry::new(Arc::new(primary));

    // Register trigger model providers for LLM group triggers.
    let primary_model = registry.primary().model_info().name.clone();
    let trigger_models: HashSet<String> = config.groups.iter()
        .filter(|g| g.trigger == GroupTrigger::Llm)
        .map(|g| g.trigger_model.as_deref()
            .unwrap_or(DEFAULT_TRIGGER_MODEL)
            .to_owned())
        .filter(|m| *m != primary_model)  // primary already registered
        .collect();

    for model in trigger_models {
        let provider = create_provider_with_model(config, &model)?;
        info!(model = %model, "registered trigger provider");
        registry.register(model, Arc::new(provider));
    }

    Ok(registry)
}
```

`create_provider_with_model()` is a small helper:

```rust
fn create_provider_with_model(config: &Config, model: &str) -> Result<AnthropicProvider> {
    if config.provider.api_keys.is_empty() {
        let api_key = std::env::var("ANTHROPIC_API_KEY")
            .context("ANTHROPIC_API_KEY not set")?;
        AnthropicProvider::new(vec![api_key], model)
    } else {
        let keys = coop_agent::key_pool::resolve_key_refs(&config.provider.api_keys)?;
        AnthropicProvider::new(keys, model)
    }
}
```

Each registered provider is a separate instance — own HTTP client, own key pool. This means:
- **Fully concurrent**: different models hit different providers with no locks
- **Bounded**: one provider per unique model, not per group (ten groups sharing `haiku` share one provider)
- **No `set_model()` races**: each provider is pinned to its model at construction
- **Extensible**: future per-user models just call `registry.register()` at startup

**Gateway changes**: Replace `provider: Arc<dyn Provider>` with `providers: ProviderRegistry`. Update all call sites:

| Before | After |
|--------|-------|
| `self.provider.complete(...)` | `self.providers.primary().complete(...)` |
| `self.provider.stream(...)` | `self.providers.primary().stream(...)` |
| `self.provider.model_info()` | `self.providers.primary().model_info()` |
| `self.provider.supports_streaming()` | `self.providers.primary().supports_streaming()` |
| `Arc::clone(&self.provider)` | `Arc::clone(self.providers.primary())` |
| `self.sync_provider_model()` | `self.providers.sync_primary_model(&model)` |

These are all mechanical replacements. The primary provider behaves identically.

#### 5b. Gateway LLM trigger method (`gateway.rs`)

Add the evaluation method:

```rust
/// Evaluate the LLM trigger: call the cheap trigger model with the full
/// conversation context to decide if the main model should respond.
///
/// The trigger model receives:
/// - The same system prompt the main model would get (personality, tools, identity)
/// - A group intro block explaining it's in a group chat
/// - The full conversation history from the session
/// - The pending group history (buffered non-triggering messages)
/// - A classification instruction asking for YES/NO
/// - The current message
///
/// Returns true if the main model should respond.
pub(crate) async fn evaluate_llm_trigger(
    &self,
    session_key: &SessionKey,
    user_input: &str,
    trust: TrustLevel,
    user_name: Option<&str>,
    channel: Option<&str>,
    group_config: &GroupConfig,
) -> bool {
    let model = group_config.trigger_model.as_deref()
        .unwrap_or(DEFAULT_TRIGGER_MODEL);
    let trigger_provider = self.providers.get(model);

    // Build the same system prompt the main model would see.
    let system = match self.build_prompt(trust, user_name, channel, user_input).await {
        Ok(blocks) => blocks,
        Err(e) => {
            warn!(error = %e, "failed to build prompt for LLM trigger, skipping");
            return false;
        }
    };

    // Append the classification instruction.
    let trigger_instruction = group_config.trigger_prompt.as_deref()
        .unwrap_or(DEFAULT_TRIGGER_PROMPT);
    let mut system_with_trigger = system;
    system_with_trigger.push(trigger_instruction.to_owned());

    // Build messages: session history + the new user message.
    let mut messages = self.messages(session_key);
    messages.push(Message::user().with_text(user_input));

    // Call the cheap model (no tools — just classification).
    match trigger_provider.complete(&system_with_trigger, &messages, &[]).await {
        Ok((response, usage)) => {
            let text = response.text();
            let should_respond = text.to_uppercase().contains("YES");
            debug!(
                session = %session_key,
                trigger_model = model,
                decision = if should_respond { "YES" } else { "NO" },
                trigger_input_tokens = ?usage.input_tokens,
                trigger_output_tokens = ?usage.output_tokens,
                "LLM trigger evaluated"
            );
            should_respond
        }
        Err(e) => {
            warn!(
                error = %e,
                session = %session_key,
                "LLM trigger call failed, defaulting to skip"
            );
            false
        }
    }
}
```

#### 5c. Default trigger prompt

```rust
pub(crate) const DEFAULT_TRIGGER_PROMPT: &str = "\
You are evaluating whether the assistant should respond to the latest message \
in this group chat. You have the full conversation context and system \
instructions above.

Evaluate the most recent message and reply with ONLY \"YES\" or \"NO\":
- YES: The assistant should respond (message is directed at the assistant, \
asks a question, requests help, or the assistant can add clear value)
- NO: The assistant should stay silent (casual chatter between other people, \
reactions, status updates, messages not relevant to the assistant)

Reply with only YES or NO, nothing else.";
```

### 6. Pending group history (new file: `group_history.rs`)

Create `crates/coop-gateway/src/group_history.rs` (~120 lines).

Buffers non-triggering messages per group session so the agent (and the trigger model) have conversational context.

```rust
use coop_core::SessionKey;
use std::collections::{HashMap, VecDeque};

pub(crate) struct GroupHistoryEntry {
    pub sender: String,
    pub body: String,
    pub timestamp: u64,
}

pub(crate) struct GroupHistoryBuffer {
    buffers: HashMap<SessionKey, VecDeque<GroupHistoryEntry>>,
}

impl GroupHistoryBuffer {
    pub(crate) fn new() -> Self { ... }

    /// Record a non-triggering message for later context injection.
    pub(crate) fn record(&mut self, key: &SessionKey, entry: GroupHistoryEntry, limit: usize) { ... }

    /// Peek at pending history without draining. Returns formatted context
    /// string, or None if empty. Used by LLM trigger to include history
    /// in the classification call without consuming it.
    pub(crate) fn peek_context(&self, key: &SessionKey) -> Option<String> { ... }

    /// Drain pending history for a session. Returns formatted context
    /// string, or None if empty. Used when a trigger fires and the
    /// messages should be included in the turn's user input.
    pub(crate) fn drain_context(&mut self, key: &SessionKey) -> Option<String> { ... }

    /// Clear history for a session.
    pub(crate) fn clear(&mut self, key: &SessionKey) { ... }
}
```

**Formatting** (shared by `peek_context` and `drain_context`):
```
[Chat messages since your last reply — for context]
[Alice at 1234567890] hey does anyone know how to fix the build?
[Bob at 1234567891] I think it's the cargo.lock
[Alice at 1234567892] @coop can you help?

[Current message — respond to this]
```

**Storage**: `Mutex<GroupHistoryBuffer>` in `Gateway`.

**Integration with LLM trigger**: The LLM trigger calls `peek_context()` to include buffered history in its classification call without consuming it. If the trigger returns YES, `drain_context()` is called to consume the history and prepend it to the user input for the real turn.

#### Group membership query for `min_member` ceiling

`min_member` needs the full group membership to compute the trust floor. Presage stores this locally — `store.group(master_key)` returns `Group { members: Vec<Member> }` with every member's UUID. It's a local SQLite read, instant.

**New query variant** in `crates/coop-channels/src/signal/query.rs`:

```rust
pub enum SignalQuery {
    RecentMessages { ... },
    /// Fetch all member UUIDs for a group.
    GroupMembers {
        master_key: Vec<u8>,
        reply: oneshot::Sender<Result<Vec<String>>>,  // UUIDs as strings
    },
}
```

The query handler extracts `member.aci` as UUID strings from the presage `Group`.

**Ceiling computation** in router/gateway — a helper function:

```rust
/// Compute the min_member trust ceiling by cross-referencing group
/// members with [[users]] config. Members not in config get `default_trust`.
fn compute_min_member_ceiling(
    member_uuids: &[String],
    config: &Config,
    default_trust: TrustLevel,
) -> TrustLevel {
    member_uuids.iter()
        .map(|uuid| {
            // Check if this UUID matches any [[users]] entry.
            let signal_id = format!("signal:{uuid}");
            config.users.iter()
                .find(|u| u.r#match.iter().any(|p| p == &signal_id || p == uuid))
                .map_or(default_trust, |u| u.trust)
        })
        .max()  // max in Ord = least privileged = most restrictive
        .unwrap_or(default_trust)
}
```

**Revision-based cache invalidation**: Every Signal group has a monotonically increasing `revision` number. Presage bumps the stored group data when it sees a new revision (membership changes, title changes, etc.). We cache the computed ceiling alongside the revision and re-query only when the revision changes.

**Threading the revision**: Add `group_revision: Option<u32>` to `InboundMessage` (in `coop-core/src/types.rs`). Populate it from the `GroupContextV2::revision` field in `chat_context_from_data_message()` (in `coop-channels/src/signal/inbound.rs`).

```rust
// In coop-core/src/types.rs, add to InboundMessage:
#[serde(default, skip_serializing_if = "Option::is_none")]
pub group_revision: Option<u32>,

// In coop-channels/src/signal/inbound.rs, chat_context_from_data_message:
fn chat_context_from_data_message(
    data_message: &DataMessage,
    sender: &str,
) -> (Option<String>, bool, Option<String>, Option<u32>) {
    if let Some(group_v2) = &data_message.group_v2 {
        if let Some(master_key) = &group_v2.master_key {
            let chat_id = format!("group:{}", hex::encode(master_key));
            let reply_to = Some(chat_id.clone());
            let revision = group_v2.revision;
            return (Some(chat_id), true, reply_to, revision);
        }
    }
    (None, false, Some(sender.to_owned()), None)
}
```

**Cache struct**:

```rust
pub(crate) struct GroupCeilingCache {
    /// Per-session: (revision, computed_ceiling).
    cache: HashMap<SessionKey, (u32, TrustLevel)>,
}

impl GroupCeilingCache {
    pub(crate) fn new() -> Self { ... }

    /// Get cached ceiling if revision matches.
    pub(crate) fn get(&self, key: &SessionKey, revision: u32) -> Option<TrustLevel> {
        self.cache.get(key)
            .filter(|(cached_rev, _)| *cached_rev == revision)
            .map(|(_, ceiling)| *ceiling)
    }

    /// Update cache with new revision and ceiling.
    pub(crate) fn set(&mut self, key: SessionKey, revision: u32, ceiling: TrustLevel) {
        self.cache.insert(key, (revision, ceiling));
    }
}
```

**Storage**: `Mutex<GroupCeilingCache>` in `Gateway`.

**Flow for `min_member` in the router**:

1. Extract `revision` from `msg.group_revision` (default 0 if missing)
2. Check cache with revision → if hit, use cached ceiling
3. If miss or revision changed, send `SignalQuery::GroupMembers`
4. Cross-reference member UUIDs with `[[users]]` config
5. Compute `max()` of all trust levels (least privileged)
6. Cache with revision and return

This is:
- **Fast**: local SQLite query, cached between messages of the same revision
- **Correct**: sees all members including lurkers, recomputes on membership changes
- **Restart-safe**: re-queries on first message after restart (instant)
- **Change-aware**: revision bump from a join/leave triggers automatic recompute

### 7. Group intro system prompt injection

Modify `Gateway::build_prompt()` to accept an optional group context and inject a group-specific system block.

**Approach**: Add an optional `group_intro: Option<&str>` parameter to `build_prompt()`, or better: have the caller (`run_turn_with_trust`) look up the group config and build the intro, then pass it as part of the prompt pipeline.

Cleanest approach: Add a new method `build_group_intro()` and call it from `run_turn_with_trust()` before the turn loop starts. Append the group intro as an additional system block.

```rust
fn build_group_intro(trigger: &GroupTrigger, agent_id: &str) -> String {
    let activation = match trigger {
        GroupTrigger::Always => "always-on (you receive every group message)",
        GroupTrigger::Llm => "trigger-only (a classifier determined you should respond; \
             recent chat context may be included)",
        GroupTrigger::Mention | GroupTrigger::Regex =>
            "trigger-only (you are invoked only when explicitly mentioned or triggered; \
             recent chat context may be included)",
    };

    let mut lines = vec![
        "You are replying inside a group chat.".to_owned(),
        format!("Activation: {activation}."),
    ];

    if matches!(trigger, GroupTrigger::Always) {
        lines.push(format!(
            "If no response is needed, reply with exactly \"{}\" \
             (and nothing else) so the system stays silent. Do not add any other \
             words, punctuation, or explanations.",
            SILENT_REPLY_TOKEN
        ));
        lines.push(
            "Be extremely selective: reply only when directly addressed \
             or clearly helpful. Otherwise stay silent.".to_owned()
        );
    }

    lines.push(
        "Be a good group participant: mostly lurk and follow the conversation; \
         reply only when directly addressed or you can add clear value.".to_owned()
    );
    lines.push(
        "Address the specific sender noted in the message context.".to_owned()
    );

    lines.join(" ")
}
```

**Where to inject**: In `run_turn_with_trust()`, after `build_prompt()` returns `system_blocks`, check if the session is a group with a matching config. If so, append the group intro as a new system block:

```rust
// After build_prompt():
if let SessionKind::Group(_) = &session_key.kind {
    let cfg = self.config.load();
    if let Some(group_config) = find_group_config_by_session(session_key, &cfg) {
        let intro = build_group_intro(&group_config.trigger, &cfg.agent.id);
        system_prompt.push(intro);
    }
}
```

Note: need a helper `find_group_config_by_session()` that matches on the session key's group ID rather than the inbound message. This is needed because `run_turn_with_trust()` doesn't have access to the original `InboundMessage`.

### 8. Router changes (`router.rs`)

#### 8a. Group matching helper

```rust
pub(crate) fn find_group_config<'a>(msg: &InboundMessage, config: &'a Config) -> Option<&'a GroupConfig> {
    if !msg.is_group { return None; }
    let chat_id = msg.chat_id.as_deref()?;
    let namespaced = if chat_id.starts_with(&format!("{}:", msg.channel)) {
        chat_id.to_owned()
    } else {
        format!("{}:{}", msg.channel, chat_id)
    };
    config.groups.iter().find(|g| {
        g.r#match.iter().any(|pattern| {
            pattern == &namespaced || pattern == "*"
        })
    })
}

/// Find group config by session key (for use in gateway where InboundMessage is not available).
pub(crate) fn find_group_config_by_session<'a>(session_key: &SessionKey, config: &'a Config) -> Option<&'a GroupConfig> {
    let group_id = match &session_key.kind {
        SessionKind::Group(id) => id,
        _ => return None,
    };
    config.groups.iter().find(|g| {
        g.r#match.iter().any(|pattern| {
            pattern == group_id || pattern == "*"
        })
    })
}
```

#### 8b. Remove group trust ceiling, use per-sender trust

The current code applies a blanket `Familiar` ceiling to all group messages:

```rust
// REMOVE THIS:
let ceiling = if group_context {
    TrustLevel::Familiar   // ← wrong: throws away sender identity
} else {
    TrustLevel::Owner
};
```

Replace with: **per-sender trust** with a configurable ceiling mode. Unknown senders get the group's `default_trust`. Known senders keep their configured trust. Then the ceiling mode is applied:

```rust
let user_trust = matched_user.map_or_else(
    || {
        if msg.channel == "terminal:default" && config.sandbox.enabled {
            TrustLevel::Owner
        } else if let Some(group_config) = find_group_config(msg, config) {
            group_config.default_trust  // configurable per group
        } else {
            TrustLevel::Public
        }
    },
    |user| user.trust,
);

// Apply group ceiling based on mode.
let ceiling = match find_group_config(msg, config).map(|gc| &gc.trust_ceiling) {
    Some(TrustCeiling::Fixed(level)) => *level,
    Some(TrustCeiling::MinMember) => {
        // Query group membership and compute ceiling.
        // Cached after first query — instant from local SQLite.
        self.resolve_min_member_ceiling(&session_key, group_config, config).await
    }
    _ => TrustLevel::Owner,  // None or no group config = no ceiling
};
let trust = resolve_trust(user_trust, ceiling);
```

This means:
- Alice (`Owner`), no ceiling → `Owner` trust
- Alice (`Owner`), `fixed:familiar` → `Familiar` (private memory hidden)
- Alice (`Owner`), `min_member` with unknown in group → `Familiar` (unknown's default_trust caps everyone)
- Alice (`Owner`), `min_member` all members `Full+` → `Full`
- Unknown sender, `default_trust = "familiar"` → `Familiar` (social memory only)
- Unknown sender, `default_trust = "full"` → `Full` (trusted small group)
- Unknown sender in unconfigured group → `Public` (rejected by auth gate)

#### 8c. Modify `is_trust_authorized()`

Configured groups are explicitly opted-in — that's the authorization. The auth gate just checks that a matching group config exists:

```rust
fn is_trust_authorized(decision: &RouteDecision, msg: &InboundMessage, config: &Config) -> bool {
    if msg.channel.starts_with("terminal") {
        return true;
    }
    match &decision.session_key.kind {
        SessionKind::Group(_) => {
            // Configured groups are explicitly opted-in. The [[groups]]
            // entry is the authorization. Trust level controls capabilities
            // (memory access, tools), not whether the message is processed.
            find_group_config(msg, config).is_some()
        }
        _ => decision.trust <= TrustLevel::Full,
    }
}
```

Update call sites in `dispatch_inner()` to pass `&self.config.load()`.

#### 8d. Trigger evaluation and history in dispatch flow

In `MessageRouter::dispatch_inner()`, after authorization, before `run_turn_with_trust()`:

```rust
// After is_trust_authorized passes:
if msg.is_group {
    let config = self.config.load();
    if let Some(group_config) = find_group_config(msg, &config) {
        // Step 1: Evaluate trigger.
        let should_respond = match group_config.trigger {
            GroupTrigger::Always => true,
            GroupTrigger::Llm => {
                // Peek at pending history so the trigger model sees it.
                let history_context = self.gateway.peek_group_history(&decision.session_key);
                let full_input = prepend_history_context(&msg.content, history_context.as_deref());
                self.gateway.evaluate_llm_trigger(
                    &decision.session_key,
                    &full_input,
                    decision.trust,
                    decision.user_name.as_deref(),
                    Some(&msg.channel),
                    group_config,
                ).await
            }
            GroupTrigger::Mention | GroupTrigger::Regex => {
                evaluate_trigger(msg, group_config) == TriggerDecision::Respond
            }
        };

        if !should_respond {
            // Buffer this message for future context.
            self.gateway.record_group_history(
                &decision.session_key,
                msg,
                group_config.history_limit,
            );
            debug!(
                session = %decision.session_key,
                trigger = ?group_config.trigger,
                "group message skipped by trigger"
            );
            let _ = event_tx.send(TurnEvent::Done(TurnResult {
                messages: Vec::new(),
                usage: Usage::default(),
                hit_limit: false,
            })).await;
            return Ok(decision);
        }

        // Step 2: Drain buffered history and prepend to message content.
        let history_context = self.gateway.drain_group_history(&decision.session_key);
        let user_input = prepend_history_context(&msg.content, history_context.as_deref());

        // Step 3: Run the turn with the enriched input.
        // (Use user_input instead of msg.content for run_turn_with_trust)
    }
}
```

The `prepend_history_context()` helper:
```rust
fn prepend_history_context(message: &str, history: Option<&str>) -> String {
    match history {
        Some(ctx) => format!("{ctx}\n{message}"),
        None => message.to_owned(),
    }
}
```

### 9. Signal loop changes (`signal_loop.rs`)

Add silent reply suppression. In `dispatch_signal_turn_background()`, after collecting the final text:

```rust
// Before flush_text_via_action:
if is_silent_reply(&text) {
    debug!(target = target, "suppressing silent reply token");
    text.clear();
}
```

Also add the same check in the `dispatch_collect_text_with_channel()` path (used by cron delivery and other non-signal paths) so the token doesn't leak to any channel.

### 10. Init template changes (`init_templates.rs`, `init.rs`)

Add a commented-out `[[groups]]` example to `generate_config()` in `init.rs`:

```toml
# Uncomment to enable group chat responses:
# [[groups]]
# match = ["signal:group:YOUR_GROUP_ID_HERE"]
# trigger = "mention"
# mention_names = ["your-agent-name"]
# default_trust = "familiar"          # trust for unknown senders (default)
# trust_ceiling = { fixed = "familiar" }  # cap everyone to prevent private info leaks
#
# Dynamic ceiling (caps everyone to lowest-trust observed member):
# [[groups]]
# match = ["signal:group:YOUR_GROUP_ID_HERE"]
# trigger = "mention"
# mention_names = ["your-agent-name"]
# trust_ceiling = "min_member"
#
# For LLM-based trigger (cheap model pre-screens messages):
# [[groups]]
# match = ["signal:group:YOUR_GROUP_ID_HERE"]
# trigger = "llm"
# trigger_model = "claude-haiku-3-5-20241022"
```

### 11. Tests

#### Config tests (`config.rs`)
- Parse groups with all trigger types
- Parse groups with defaults (trigger defaults to "mention")
- Parse empty groups array
- Parse wildcard match
- Parse default_trust override
- Parse `trust_ceiling = "none"`, `trust_ceiling = { fixed = "familiar" }`, `trust_ceiling = "min_member"`
- Parse history_limit override
- Parse trigger_model and trigger_prompt
- default_trust defaults to Familiar
- trust_ceiling defaults to None
- Roundtrip serialization

#### Config check tests (`config_check.rs`)
- Valid group config passes
- Empty match fails
- Invalid match pattern warns
- Mention trigger without mention_names fails
- Regex trigger without trigger_regex fails
- Regex trigger with invalid regex fails
- `default_trust = "owner"` warns (dangerous)
- `trust_ceiling = { fixed = "owner" }` with `default_trust = "owner"` warns
- Groups without signal channel warns
- Duplicate match patterns warns

#### Group trigger tests (`group_trigger.rs`)
- `always` trigger returns Respond
- `mention` trigger matches name case-insensitively
- `mention` trigger skips unrelated messages
- `mention` trigger strips `[from ...]` prefix before matching
- `mention` trigger matches partial word boundaries
- `regex` trigger matches pattern
- `regex` trigger skips non-matching messages
- `regex` trigger strips `[from ...]` prefix before matching
- `is_silent_reply` detects "NO_REPLY"
- `is_silent_reply` detects "NO_REPLY" with whitespace
- `is_silent_reply` rejects normal text
- `is_silent_reply` rejects text containing NO_REPLY mid-sentence
- `strip_envelope_prefix` strips correctly
- `strip_envelope_prefix` handles messages without prefix

#### Group history tests (`group_history.rs`)
- Record and drain returns formatted context
- Record and peek returns formatted context without consuming
- Drain clears buffer
- Peek does not clear buffer
- History respects limit (oldest dropped)
- Empty buffer returns None
- Multiple sessions are independent
- `GroupCeilingCache` get returns None for uncached session
- `GroupCeilingCache` get returns None when revision mismatches
- `GroupCeilingCache` get returns cached ceiling when revision matches
- `GroupCeilingCache` set overwrites on new revision
- `compute_min_member_ceiling` returns least privileged across members
- `compute_min_member_ceiling` uses default_trust for unknown members
- `compute_min_member_ceiling` all known Full users returns Full
- `compute_min_member_ceiling` one unknown member returns default_trust

#### Provider registry tests (`provider_registry.rs`)
- `new()` registers primary model in by_model
- `get()` returns primary for unknown models (fallback)
- `get_exact()` returns None for unknown models
- `register()` adds a new model
- `get()` returns registered provider for known model
- `primary()` returns the primary provider
- Multiple registered providers are independent

#### Gateway LLM trigger tests (`gateway.rs`)
- LLM trigger with FakeProvider returning "YES" → true
- LLM trigger with FakeProvider returning "NO" → false
- LLM trigger with failing provider → false (graceful fallback)
- LLM trigger receives the same system prompt blocks as a normal turn
- LLM trigger receives session history + new user message
- LLM trigger includes pending group history in user input

#### Router tests (`router.rs`)
- Unknown sender in configured group gets `default_trust` from group config
- Unknown sender in configured group with `default_trust = "full"` gets `Full`
- Known `Owner` user in group with `TrustCeiling::None` keeps `Owner` trust
- Known `Owner` user in group with `TrustCeiling::Fixed(Familiar)` gets `Familiar`
- Known `Full` user in group with `TrustCeiling::Fixed(Inner)` gets `Inner`
- `TrustCeiling::MinMember` caps to lowest-trust group member
- `TrustCeiling::MinMember` uses cached ceiling when revision unchanged
- `TrustCeiling::MinMember` recomputes ceiling when revision changes
- Group message with matching config passes `is_trust_authorized`
- Group message without config is rejected
- `find_group_config` matches exact group ID
- `find_group_config` matches wildcard
- `find_group_config` returns None for unconfigured groups
- `find_group_config_by_session` matches by session key

#### Integration tests (`signal_loop/tests.rs`)
- Group message with `trigger = "always"` gets a response
- Group message with `trigger = "mention"` and matching name gets a response
- Group message with `trigger = "mention"` and no match is silently skipped
- Group message in unconfigured group is silently rejected
- Silent reply token (NO_REPLY) is suppressed and not sent to channel

## File Change Summary

| File | Change |
|------|--------|
| `crates/coop-core/src/types.rs` | Add `group_revision: Option<u32>` to `InboundMessage` |
| `crates/coop-channels/src/signal/inbound.rs` | Thread `group_v2.revision` into `InboundMessage::group_revision` |
| `crates/coop-gateway/src/config.rs` | Add `GroupConfig`, `GroupTrigger`, `TrustCeiling`, add `groups` to `Config` |
| `crates/coop-gateway/src/config_check.rs` | Add `check_groups()`, call from `validate_config()` |
| `crates/coop-gateway/src/group_trigger.rs` | New: trigger evaluation (always/mention/regex), silent reply detection, envelope stripping, `DEFAULT_TRIGGER_PROMPT` |
| `crates/coop-gateway/src/group_history.rs` | New: pending message buffer with peek/drain; `GroupCeilingCache` for `min_member` ceiling |
| `crates/coop-channels/src/signal/query.rs` | Add `GroupMembers` query variant |
| `crates/coop-gateway/src/router.rs` | `find_group_config()`, `find_group_config_by_session()`, modify `route_message()` trust, modify `is_trust_authorized()`, trigger eval + history in `dispatch_inner()` |
| `crates/coop-gateway/src/provider_registry.rs` | New: `ProviderRegistry` — model-keyed provider lookup |
| `crates/coop-gateway/src/gateway.rs` | Replace `provider: Arc<dyn Provider>` with `providers: ProviderRegistry`; add `group_history`, `group_ceiling_cache` fields; `evaluate_llm_trigger()`, `resolve_min_member_ceiling()` methods; `build_group_intro()`; history record/peek/drain methods; inject group intro in `run_turn_with_trust()` |
| `crates/coop-gateway/src/signal_loop.rs` | Suppress silent reply token before sending |
| `crates/coop-gateway/src/main.rs` | `build_provider_registry()` + `create_provider_with_model()` helpers; pass `ProviderRegistry` to `Gateway::new()` |
| `crates/coop-gateway/src/init.rs` | Add commented-out groups example in `generate_config()` |

## Deferred / Future Enhancements

These are noted for future work, do NOT implement now:

1. **Reply-to as implicit mention**: If someone replies to the bot's message in a group, treat it as a mention. Requires tracking message timestamps to match Signal quotes back to assistant messages. Add a `// TODO: implicit mention on reply-to` comment in `evaluate_trigger()`.

2. **Ack reactions**: React with 👀 when processing starts in a group. Needs Signal reaction sending support.

3. **Inbound debouncing**: Batching rapid messages from the same sender. The signal loop already handles one-turn-at-a-time per session.

4. **Per-group tool restrictions**: Finer-grained than the trust-based access model.

5. **Group member names in context**: Resolve member UUIDs to profile names for richer group intro prompts.

6. **Runtime activation command**: `/activation always` and `/activation mention` slash commands.

## E2E Testing — Signal Group Chat

End-to-end testing over real Signal. Follows the same architecture as the Signal E2E Trace Loop (`docs/prompts/signal-e2e-trace-loop.md`). Uses the existing two-account setup (Alice as test sender, Bob/coop as receiver) plus a new Signal group created for testing.

**Prerequisite**: The `signal-e2e-test` skill must be functional — DM message flow already working (scenarios 1-3 from the trace loop should pass). Group E2E builds on top of that.

### Setup: Create a test group

Both accounts (Alice and Bob) must be in a Signal group. Create it once, reuse across test runs.

```bash
# Discover accounts
eval "$(bash .claude/skills/signal-e2e-test/scripts/discover-accounts.sh 2>/dev/null)"

# Create a group with both accounts (Alice creates it, adds Bob)
${SENDER_CMD} updateGroup -n "Coop E2E Test Group" -m ${COOP_NUMBER}

# List groups to get the group ID (base64-encoded)
${SENDER_CMD} listGroups -d 2>&1 | grep -i "coop e2e"
# Note the group ID — looks like "base64encodedGroupId=="

# Also list from coop's side (after coop receives the group update)
signal-cli -a ${COOP_NUMBER} -o json listGroups
```

Store the group ID for use in tests:

```bash
export GROUP_ID="<base64-group-id-from-above>"
```

The group ID in coop's config uses hex encoding of the master key, formatted as `signal:group:<hex>`. To find the hex ID, send a test message to the group and check coop's traces:

```bash
${SENDER_CMD} send -g "${GROUP_ID}" -m "Hello group"
sleep 8
grep 'signal_receive_event' traces.jsonl | tail -5
# Look for chat_id field: "group:<hex>"
# That hex string is what goes in coop.toml [[groups]] match
```

### Configure coop.toml for group testing

Add a `[[groups]]` entry pointing to the test group. Start with `mention` trigger for initial tests, switch trigger modes per scenario:

```toml
[[groups]]
match = ["signal:group:<hex-from-traces>"]
trigger = "mention"
mention_names = ["coop", "hey coop"]
default_trust = "familiar"
```

Coop hot-reloads config — no restart needed after changing `[[groups]]`.

### Scenarios

Execute these in order. Each builds on previous setup. Use the trace-driven verification pattern from the E2E skill.

**Trace helpers** (same as DM tests):

```bash
SKILL_DIR=/root/coop/main/.claude/skills/signal-e2e-test

# Check traces for new events after a timestamp
new_grep() {
    tail -n +"$((TRACE_LINES_BEFORE + 1))" traces.jsonl | grep "$@"
}
TRACE_LINES_BEFORE=$(wc -l < traces.jsonl)
```

#### Scenario G1: Unconfigured group — silent rejection

**Setup:** Comment out the `[[groups]]` section in `coop.toml`.

**Action:**
```bash
TRACE_LINES_BEFORE=$(wc -l < traces.jsonl)
${SENDER_CMD} send -g "${GROUP_ID}" -m "Hello from unconfigured group"
sleep 10
```

**Pass criteria:**
- `signal_receive_event` logged (message received by presage)
- `signal inbound dispatched` with `signal.is_group = true`
- `route_message` entered
- Message rejected by `is_trust_authorized` — trace shows trust rejection
- NO `agent_turn` started
- NO `signal_action_send` (no reply sent to group)
- No `ERROR` entries

#### Scenario G2: Mention trigger — matching name gets response

**Setup:** Restore `[[groups]]` with `trigger = "mention"` and `mention_names = ["coop"]`.

**Action:**
```bash
TRACE_LINES_BEFORE=$(wc -l < traces.jsonl)
${SENDER_CMD} send -g "${GROUP_ID}" -m "hey coop, what is 2+2?"
sleep 20
```

**Pass criteria:**
- `signal_receive_event` with group context
- Trigger evaluation logged: `"group trigger evaluated"` or equivalent debug trace
- Trigger decision: `Respond`
- `agent_turn` started with correct session key (`SessionKind::Group`)
- Group intro system block present in prompt (check for "group chat" in system prompt traces)
- `signal_action_send` with reply sent to the group
- Reply content is coherent (answers 2+2)
- No `ERROR` entries

#### Scenario G3: Mention trigger — no match is silently skipped

**Action:**
```bash
TRACE_LINES_BEFORE=$(wc -l < traces.jsonl)
${SENDER_CMD} send -g "${GROUP_ID}" -m "hey everyone, random chatter"
sleep 10
```

**Pass criteria:**
- `signal_receive_event` logged
- Trigger evaluation: `Skip`
- Message buffered in group history (trace shows `"group message skipped by trigger"`)
- NO `agent_turn` started
- NO `signal_action_send`
- No `ERROR` entries

#### Scenario G4: Pending history — context from skipped messages

**Action:** Send several non-triggering messages, then a triggering one:
```bash
TRACE_LINES_BEFORE=$(wc -l < traces.jsonl)
${SENDER_CMD} send -g "${GROUP_ID}" -m "I've been working on the build system"
sleep 3
${SENDER_CMD} send -g "${GROUP_ID}" -m "The cargo.lock keeps conflicting"
sleep 3
${SENDER_CMD} send -g "${GROUP_ID}" -m "coop, can you help with cargo lock conflicts?"
sleep 25
```

**Pass criteria:**
- First two messages: `Skip`, buffered in history
- Third message: `Respond` (mention match)
- Pending history drained and prepended to user input
- `agent_turn` input contains context from the first two messages
- Reply references the cargo.lock / build system context (not just the isolated question)
- No `ERROR` entries

#### Scenario G5: Always trigger — every message gets response

**Setup:** Change config to `trigger = "always"`.

**Action:**
```bash
TRACE_LINES_BEFORE=$(wc -l < traces.jsonl)
${SENDER_CMD} send -g "${GROUP_ID}" -m "just some casual chatter"
sleep 20
```

**Pass criteria:**
- Trigger evaluation: `Respond` (always mode)
- `agent_turn` started
- Group intro system block includes NO_REPLY token instructions
- Reply sent OR silent token suppressed (either is correct behavior)
- No `ERROR` entries

#### Scenario G6: Silent reply token suppression (always mode)

**Action:** Send a message that should not need a response:
```bash
TRACE_LINES_BEFORE=$(wc -l < traces.jsonl)
${SENDER_CMD} send -g "${GROUP_ID}" -m "ok thanks"
sleep 20
```

**Pass criteria:**
- `agent_turn` started and completed
- If agent outputs `NO_REPLY`:
  - Trace shows `"suppressing silent reply token"`
  - NO `signal_action_send` with reply to group
- If agent outputs a real reply: that's also acceptable (agent decided to respond)
- Either way: no `ERROR` entries, no empty message sent to group

#### Scenario G7: Regex trigger

**Setup:** Change config to `trigger = "regex"`, `trigger_regex = "^!(ask|help)"`.

**Action:**
```bash
TRACE_LINES_BEFORE=$(wc -l < traces.jsonl)
${SENDER_CMD} send -g "${GROUP_ID}" -m "!ask what time is it"
sleep 20
```

**Pass criteria:**
- Trigger evaluation: `Respond` (regex match)
- `agent_turn` started, reply sent
- No `ERROR` entries

Then test non-matching:
```bash
TRACE_LINES_BEFORE=$(wc -l < traces.jsonl)
${SENDER_CMD} send -g "${GROUP_ID}" -m "just chatting"
sleep 10
```

**Pass criteria:**
- Trigger evaluation: `Skip`
- NO `agent_turn`, no reply

#### Scenario G8: LLM trigger — cheap model pre-screen

**Setup:** Change config to `trigger = "llm"`, `trigger_model = "claude-haiku-3-5-20241022"`.

**Action:** Send a message clearly directed at the agent:
```bash
TRACE_LINES_BEFORE=$(wc -l < traces.jsonl)
${SENDER_CMD} send -g "${GROUP_ID}" -m "Can someone explain how Rust's borrow checker works?"
sleep 30
```

**Pass criteria:**
- Trigger evaluation trace shows: `trigger_model = "claude-haiku-3-5-20241022"`
- LLM trigger made a provider call (trace shows `provider_request` or equivalent from the trigger provider)
- Decision logged: `YES` or `NO`
- If `YES`: `agent_turn` started with main model, reply sent
- If `NO`: message buffered, no agent turn
- Trigger token usage logged: `trigger_input_tokens`, `trigger_output_tokens`
- No `ERROR` entries

Then send clearly irrelevant chatter:
```bash
TRACE_LINES_BEFORE=$(wc -l < traces.jsonl)
${SENDER_CMD} send -g "${GROUP_ID}" -m "lol nice meme"
sleep 20
```

**Pass criteria:**
- LLM trigger evaluates to `NO`
- NO `agent_turn` with main model
- Message buffered in pending history

#### Scenario G9: Trust — unknown sender gets default_trust

**Setup:** Change config to `trigger = "always"`, `default_trust = "familiar"`. Remove Alice's UUID from `[[users]]` (make her unknown).

**Action:**
```bash
TRACE_LINES_BEFORE=$(wc -l < traces.jsonl)
${SENDER_CMD} send -g "${GROUP_ID}" -m "coop, read my private notes"
sleep 20
```

**Pass criteria:**
- `route_message` shows `trust = "Familiar"` (not Public, not Full)
- `agent_turn` starts (message is allowed through)
- Agent has access to `social` memory store only (not `private`, not `shared`)
- No `ERROR` entries

**Restore:** Re-add Alice's UUID to `[[users]]` after this test.

#### Scenario G10: Trust ceiling — fixed ceiling caps known user

**Setup:** Alice is configured as `trust = "full"` in `[[users]]`. Group config has `trust_ceiling = { fixed = "familiar" }`.

**Action:**
```bash
TRACE_LINES_BEFORE=$(wc -l < traces.jsonl)
${SENDER_CMD} send -g "${GROUP_ID}" -m "coop, search my private memory"
sleep 20
```

**Pass criteria:**
- `route_message` shows `trust = "Familiar"` (Alice's Full capped to Familiar by ceiling)
- Agent uses `social` memory store only
- No `ERROR` entries

#### Scenario G11: Trust ceiling — min_member dynamic ceiling

**Setup:** Alice is `trust = "full"` in `[[users]]`. Group config has `trust_ceiling = "min_member"`, `default_trust = "familiar"`.

**Action:** Alice sends a message — but the group also contains Bob (coop's own account), and potentially other unknown members:
```bash
TRACE_LINES_BEFORE=$(wc -l < traces.jsonl)
${SENDER_CMD} send -g "${GROUP_ID}" -m "coop, what can you access?"
sleep 20
```

**Pass criteria:**
- Trace shows `SignalQuery::GroupMembers` sent (membership queried)
- `compute_min_member_ceiling` cross-references members with `[[users]]`
- If any unknown member exists → ceiling = `Familiar`
- If all members are known with `Full+` → ceiling = `Full`
- Resulting trust logged in `route_message`
- No `ERROR` entries

#### Scenario G12: Trust ceiling — revision-based cache invalidation

**Setup:** Same as G11 (`min_member` mode). The ceiling should already be cached from G11.

**Action:** Add a new member to the group (or remove one), then send a message:
```bash
# Add a third number if available, or use updateGroup to change something
# that bumps the revision. Even changing the group name works:
${SENDER_CMD} updateGroup -g "${GROUP_ID}" -n "Coop E2E Test Group v2"
sleep 5

TRACE_LINES_BEFORE=$(wc -l < traces.jsonl)
${SENDER_CMD} send -g "${GROUP_ID}" -m "coop, has anything changed?"
sleep 20
```

**Pass criteria:**
- Group revision in message differs from cached revision
- Cache miss → `SignalQuery::GroupMembers` re-sent
- Ceiling recomputed with current membership
- New ceiling cached with new revision
- No `ERROR` entries

#### Scenario G13: Group session isolation — DM still works

**Action:** After group tests, send a DM to verify it still works:
```bash
TRACE_LINES_BEFORE=$(wc -l < traces.jsonl)
${SENDER_CMD} send -m "DM test after group tests — what is 3+3?" ${COOP_NUMBER}
sleep 20
```

**Pass criteria:**
- `route_message` with `SessionKind::Dm` (not Group)
- Trust is Alice's configured level (no group ceiling applied)
- `agent_turn` completes normally
- Reply sent as DM
- No `ERROR` entries

#### Scenario G14: Multiple trigger modes across restarts

**Setup:** Configure two groups (if a second group is available) with different triggers. Or test one group with a config change + restart.

**Action:** Stop coop, clear traces, restart, send a group message:
```bash
# Stop coop (Ctrl+C in tmux)
> traces.jsonl
# Restart coop
# (tmux send-keys pattern from E2E skill)
sleep 15  # wait for startup

TRACE_LINES_BEFORE=$(wc -l < traces.jsonl)
${SENDER_CMD} send -g "${GROUP_ID}" -m "coop, are you there after restart?"
sleep 20
```

**Pass criteria:**
- Coop starts cleanly with group config loaded
- Group message processed correctly after restart
- If `min_member` mode: membership re-queried on first message (no stale cache)
- Reply sent
- No `ERROR` entries

### E2E Verification Workflow

Same pattern as the DM E2E loop:

```bash
# 1. Build
cargo fmt && cargo build --features signal

# 2. Start coop in tmux with tracing
tmux send-keys -t coop 'COOP_TRACE_FILE=traces.jsonl cargo run --features signal --bin coop -- start' Enter
sleep 15
bash .claude/skills/signal-e2e-test/scripts/preflight.sh

# 3. Create group (once), configure coop.toml

# 4. Run scenarios in order: G1 through G14
#    On FAIL: read traces, fix, rebuild, restart, re-run failing scenario

# 5. Clean sweep: restart with empty traces, run G2, G3, G5, G8, G10, G13
#    back-to-back, verify zero errors

# 6. Full unit test check
cargo test --workspace
```

### Bug logging

Follow the same convention as the DM E2E loop:
- Create `docs/bugs/NNN-group-*.md` for each bug
- Include trace evidence
- Update `docs/bugs/SESSION-LOG.md` with group test results

### Scenario summary table

| # | Scenario | Trigger | What it tests |
|---|----------|---------|---------------|
| G1 | Unconfigured group | — | Silent rejection of groups not in config |
| G2 | Mention match | mention | Basic group response flow |
| G3 | Mention miss | mention | Silent skip, no reply |
| G4 | Pending history | mention | Context from buffered messages |
| G5 | Always mode | always | Every message gets a turn |
| G6 | NO_REPLY suppression | always | Silent token not sent to group |
| G7 | Regex trigger | regex | Pattern-based triggering |
| G8 | LLM trigger | llm | Cheap model pre-screen |
| G9 | Unknown sender trust | always | default_trust applied correctly |
| G10 | Fixed ceiling | always | trust_ceiling caps known users |
| G11 | min_member ceiling | always | Dynamic ceiling from membership |
| G12 | Revision invalidation | always | Cache recompute on membership change |
| G13 | Session isolation | — | DM still works after group tests |
| G14 | Restart resilience | mention | State recovery after restart |

## Constraints

- **Do NOT add `regex` to `coop-core`**. Add it only to `coop-gateway/Cargo.toml`. Core is a root dependency — adding regex there slows every build.
- **Do NOT add `reqwest` to any new crate**. LLM calls go through the existing provider.
- **Keep new files under 200 lines** each.
- **Keep files under 500 lines** per AGENTS.md compile-time rules.
- **Use `cargo fmt` and `cargo clippy --all-targets --all-features -- -D warnings`** before finishing.
- **Run `cargo test -p coop-gateway`** to verify all tests pass.
- Follow existing patterns: use `tracing::debug!` for trigger decisions, `tracing::info!` for key events.
- Use fake/placeholder data in tests (never real PII). Use the Alice/Bob convention from AGENTS.md.
- Use `ProviderRegistry` for all provider access. Each unique model gets its own provider instance. Do NOT use `set_model()` on non-primary providers. Replace `self.provider` with `self.providers.primary()` everywhere.

## Development Loop

```bash
# 1. Add config structs + tests → cargo test -p coop-gateway
# 2. Add group_trigger.rs + tests → cargo test -p coop-gateway
# 3. Add group_history.rs + tests → cargo test -p coop-gateway
# 4. Modify router + tests → cargo test -p coop-gateway
# 5. Add provider_registry.rs + tests → cargo test -p coop-gateway
# 6. Refactor gateway.rs: replace provider with ProviderRegistry → cargo test -p coop-gateway
# 7. Add build_provider_registry in main.rs → cargo build
# 8. Add evaluate_llm_trigger + group intro in gateway.rs → cargo test -p coop-gateway
# 9. Add silent reply suppression in signal_loop.rs → cargo test -p coop-gateway
# 10. Add config_check + tests → cargo test -p coop-gateway
# 11. Update init template
# 12. Full check: cargo fmt && cargo clippy --all-targets --all-features -- -D warnings && cargo test
# 13. E2E: signal-e2e-test skill — run group scenarios G1-G14 over real Signal
```
