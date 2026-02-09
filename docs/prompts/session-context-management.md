# Session Context Compaction

## Problem

Coop sends the **entire** conversation history on every provider call. Context accumulates without bound across turns, and each tool-call iteration within a turn re-sends everything that came before.

### Trace evidence (`bug.jsonl`)

A 9-turn session consumed **4.25M input tokens** (~$65 at Opus pricing). The baseline context per turn (first iteration, before any tool calls) grew monotonically:

```
Turn 1 baseline:   20,264 tokens
Turn 2 baseline:   22,565 tokens   (previous turn added ~2K)
Turn 3 baseline:   58,884 tokens   (previous turn read files, +36K)
Turn 4 baseline:   77,702 tokens
Turn 5 baseline:   77,875 tokens
Turn 6 baseline:   78,703 tokens
Turn 7 baseline:   81,427 tokens
Turn 8 baseline:   89,910 tokens
Turn 9 baseline:   90,212 tokens   (4.5x the starting context)
```

By Turn 3, the model was re-sending 60K tokens of old file reads and tool outputs on every single API call — 13 iterations × 58-77K = 880K input tokens for one user message. The session never shrinks.

### Root cause

In `gateway.rs`, `run_turn_with_trust` calls `self.messages(session_key)` which returns the **complete, unmodified** session history. Every assistant response, tool request, and tool result ever produced stays in context forever.

```rust
let messages = self.messages(session_key);
let (response, usage) = self
    .assistant_response(&system_prompt, &messages, &tool_defs, &event_tx)
    .await?;
```

## Solution: LLM-based compaction (pi's approach)

When context usage approaches the model's limit, use the LLM itself to summarize old messages into a structured checkpoint. Keep recent messages intact. The provider then sees `[summary] + [recent messages]` instead of the full history.

This is how pi (the coding agent harness) handles it. Key insight: an LLM summary preserves semantic content (goals, decisions, progress, file operations) far better than mechanical truncation.

### How it works

```
Before compaction:

  msg:  0     1     2     3      4     5     6      7      8     9
      ┌─────┬─────┬─────┬──────┬─────┬─────┬──────┬──────┬─────┬──────┐
      │ usr │ ast │ usr │ tool │ usr │ ast │ tool │ tool │ ast │ tool │
      └─────┴─────┴─────┴──────┴─────┴─────┴──────┴──────┴─────┴──────┘
       └──────────┬───────────┘ └──────────────┬──────────────────────┘
          summarized by LLM                kept (recent ~20K tokens)
                                ↑
                       first_kept_message_id

After compaction — what the provider sees:

      ┌─────────┬─────────┬─────┬─────┬──────┬──────┬─────┬──────┐
      │ summary │ ack     │ usr │ ast │ tool │ tool │ ast │ tool │
      │ (user)  │ (asst)  │     │     │      │      │     │      │
      └─────────┴─────────┴─────┴─────┴──────┴──────┴─────┴──────┘
           ↑         ↑      └─────────────────┬────────────────────┘
      synthetic  synthetic        kept messages (unchanged)
```

The full session history stays on disk untouched. Compaction only changes **what gets sent to the provider**.

### When compaction triggers

Check before each turn (proactive), using the **actual input token count** from the last provider response:

```
if last_input_tokens > context_limit - reserve_tokens:
    compact()
```

Use `Usage.input_tokens` from the most recent assistant response — this is the exact token count from the provider, not an estimate. Fall back to chars/4 estimation only when no usage data is available (first turn of a session).

Default thresholds:
- `reserve_tokens`: 30,000 (room for response + tool calls within the turn)
- `keep_recent_tokens`: 20,000 (recent context preserved verbatim)

### Compaction steps

1. **Find cut point.** Walk backwards from the newest message, accumulating token estimates (chars/4), until `keep_recent_tokens` is reached. Cut at that message. Never cut at a `ToolResult` — it must stay with its `ToolRequest`.

2. **Serialize old messages to text.** Convert messages before the cut point into a text representation that the model won't try to continue as a conversation:
   ```
   [User]: What files are in src/?
   [Assistant]: Let me check.
   [Tool call]: bash(command="ls src/")
   [Tool result]: main.rs lib.rs
   [Assistant]: There are two files: main.rs and lib.rs.
   ```

3. **Call `provider.complete_fast()`** with a summarization prompt (see below). This produces a structured summary.

4. **Store compaction state.** Save the summary and `first_kept_message_id` so future turns can build context from it.

5. **If a previous compaction exists**, pass its summary to the LLM with an "update" prompt so the model merges new information into the existing summary (iterative refinement, not re-summarization from scratch).

### Summary format

The LLM produces a structured checkpoint:

```markdown
## Goal
[What the user is trying to accomplish]

## Progress
### Done
- [x] Completed tasks

### In Progress
- [ ] Current work

## Key Decisions
- **Decision**: Rationale

## Next Steps
1. What should happen next

## Critical Context
- Data needed to continue

<read-files>
path/to/file1.rs
</read-files>

<modified-files>
path/to/changed.rs
</modified-files>
```

## Scope

All changes in `coop-gateway`. No changes to `coop-core` types, `coop-agent`, `coop-channels`, or the `Provider` trait.

## Implementation

### New file: `crates/coop-gateway/src/compaction.rs`

~300 lines. Contains all compaction logic.

```rust
use coop_core::prompt::count_tokens;
use coop_core::{Message, Content, Provider, Role, Usage};
use anyhow::Result;
use tracing::{info, info_span, debug, warn, Instrument};
use std::sync::Arc;

// ── Configuration ──────────────────────────────────────────────────

/// Tokens reserved for the model's response + intra-turn tool calls.
const DEFAULT_RESERVE_TOKENS: usize = 30_000;

/// Recent tokens to keep verbatim (not summarized).
const DEFAULT_KEEP_RECENT_TOKENS: usize = 20_000;

// ── Compaction state ───────────────────────────────────────────────

/// Persisted compaction state for a session.
/// Stored as JSON alongside the session JSONL.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub(crate) struct CompactionState {
    /// LLM-generated summary of old messages.
    pub summary: String,
    /// Message ID from which to start sending to the provider.
    pub first_kept_message_id: String,
    /// How many tokens the context had before compaction.
    pub tokens_before: usize,
    /// Timestamp of compaction.
    pub created_at: chrono::DateTime<chrono::Utc>,
}

// ── Context builder ────────────────────────────────────────────────

/// Build the message list for a provider call, applying compaction if present.
///
/// If `compaction` is Some, returns:
///   [synthetic_user_summary, synthetic_assistant_ack, messages_from_first_kept..]
///
/// If `compaction` is None, returns all messages unchanged.
pub(crate) fn build_provider_context(
    all_messages: &[Message],
    compaction: Option<&CompactionState>,
) -> Vec<Message> {
    // ...
}

// ── Should compact? ────────────────────────────────────────────────

/// Check whether compaction should trigger.
///
/// Uses `last_input_tokens` from the most recent provider Usage response
/// for an exact count. Falls back to chars/4 estimation if no usage data.
pub(crate) fn should_compact(
    all_messages: &[Message],
    compaction: Option<&CompactionState>,
    context_limit: usize,
    last_input_tokens: Option<u32>,
) -> bool {
    let current_tokens = match last_input_tokens {
        Some(t) => t as usize,
        None => estimate_messages_tokens(all_messages, compaction),
    };
    current_tokens > context_limit.saturating_sub(DEFAULT_RESERVE_TOKENS)
}

// ── Token estimation ───────────────────────────────────────────────

/// Estimate total tokens for a message (all content blocks).
pub(crate) fn estimate_message_tokens(msg: &Message) -> usize {
    // Sum chars across all content blocks, divide by 4.
    // For ToolResult, count the output text.
    // For ToolRequest, count name + serialized arguments.
    // For Text, count the text.
    // For Thinking, count the thinking text.
    // ...
}

/// Estimate total context tokens, accounting for compaction.
fn estimate_messages_tokens(
    messages: &[Message],
    compaction: Option<&CompactionState>,
) -> usize {
    // If compaction exists, only count the summary + messages after cut point.
    // Otherwise count all messages.
    // ...
}

// ── Cut point ──────────────────────────────────────────────────────

/// Find the cut point: index of the first message to keep.
///
/// Walks backward from the end, accumulating token estimates.
/// Stops when `keep_recent_tokens` is reached.
/// Never cuts at a ToolResult (must stay with its ToolRequest).
///
/// Returns the index into `messages` of the first message to keep.
fn find_cut_point(messages: &[Message]) -> usize {
    // Walk backwards, accumulate tokens.
    // Valid cut points: User messages, Assistant messages.
    // Never cut at ToolResult — skip back to the preceding message.
    // ...
}

// ── Serialization ──────────────────────────────────────────────────

/// Serialize messages to text for the summarization prompt.
///
/// Produces a format that the model won't try to continue as conversation:
///   [User]: message text
///   [Assistant]: response text
///   [Tool call]: tool_name(arg1="val1", arg2="val2")
///   [Tool result]: output text
///   [Thinking]: (omitted)
fn serialize_for_summary(messages: &[Message]) -> String {
    // ...
}

// ── Summarization prompts ──────────────────────────────────────────

const SUMMARIZATION_SYSTEM: &str =
    "You are a context summarization assistant. Read the conversation and \
     produce a structured summary following the exact format specified. \
     Do NOT continue the conversation. ONLY output the structured summary.";

const INITIAL_SUMMARY_PROMPT: &str = r#"The messages above are a conversation to summarize.
Create a structured context checkpoint that another LLM will use to continue the work.

Use this EXACT format:

## Goal
[What is the user trying to accomplish?]

## Constraints & Preferences
- [Requirements mentioned by user]

## Progress
### Done
- [x] [Completed tasks]

### In Progress
- [ ] [Current work]

## Key Decisions
- **[Decision]**: [Rationale]

## Next Steps
1. [What should happen next]

## Critical Context
- [Data, examples, or references needed to continue]

<read-files>
[files that were read]
</read-files>

<modified-files>
[files that were modified]
</modified-files>

Keep each section concise. Preserve exact file paths, function names, and error messages."#;

const UPDATE_SUMMARY_PROMPT: &str = r#"The messages above are NEW conversation messages.
Update the existing summary (in <previous-summary> tags) with this new information.

RULES:
- PRESERVE all existing information from the previous summary
- ADD new progress, decisions, and context
- UPDATE Progress: move "In Progress" to "Done" when completed
- UPDATE "Next Steps" based on what was accomplished
- PRESERVE exact file paths, function names, and error messages

Use the same format as the previous summary."#;

// ── Main compaction function ───────────────────────────────────────

/// Run compaction: summarize old messages using the provider.
///
/// Returns the new CompactionState to store.
pub(crate) async fn compact(
    messages: &[Message],
    previous: Option<&CompactionState>,
    provider: &dyn Provider,
) -> Result<CompactionState> {
    let span = info_span!("compaction");
    async {
        let cut_index = find_cut_point(messages);

        // Determine which messages to summarize.
        // If there's a previous compaction, we only summarize messages
        // between the old cut point and the new cut point (incremental).
        let (msgs_to_summarize, first_kept_msg) = if let Some(prev) = previous {
            // Find where the previous cut point was
            let prev_start = messages.iter()
                .position(|m| m.id == prev.first_kept_message_id)
                .unwrap_or(0);
            (&messages[prev_start..cut_index], &messages[cut_index])
        } else {
            (&messages[..cut_index], &messages[cut_index])
        };

        let tokens_before = estimate_messages_tokens(messages, previous);

        // Serialize to text
        let conversation_text = serialize_for_summary(msgs_to_summarize);

        // Build the summarization prompt
        let prompt_text = if let Some(prev) = previous {
            format!(
                "<conversation>\n{conversation_text}\n</conversation>\n\n\
                 <previous-summary>\n{}\n</previous-summary>\n\n\
                 {UPDATE_SUMMARY_PROMPT}",
                prev.summary
            )
        } else {
            format!(
                "<conversation>\n{conversation_text}\n</conversation>\n\n\
                 {INITIAL_SUMMARY_PROMPT}"
            )
        };

        info!(
            messages_to_summarize = msgs_to_summarize.len(),
            cut_index,
            total_messages = messages.len(),
            tokens_before,
            has_previous = previous.is_some(),
            "running compaction"
        );

        // Call the provider
        let (response, _usage) = provider
            .complete_fast(
                SUMMARIZATION_SYSTEM,
                &[Message::user().with_text(&prompt_text)],
                &[],  // no tools for summarization
            )
            .await?;

        let summary = response.text();

        info!(
            summary_len = summary.len(),
            summary_tokens = count_tokens(&summary),
            "compaction complete"
        );

        Ok(CompactionState {
            summary,
            first_kept_message_id: first_kept_msg.id.clone(),
            tokens_before,
            created_at: chrono::Utc::now(),
        })
    }
    .instrument(span)
    .await
}
```

### New file: `crates/coop-gateway/src/compaction_store.rs`

~80 lines. Persistence for `CompactionState`, stored as JSON files alongside session JSONL.

```rust
use crate::compaction::CompactionState;
use anyhow::{Context, Result};
use coop_core::SessionKey;
use std::path::{Path, PathBuf};
use tracing::debug;

/// Stores compaction state as JSON files alongside session JSONL files.
pub(crate) struct CompactionStore {
    dir: PathBuf,
}

impl CompactionStore {
    pub(crate) fn new(dir: impl AsRef<Path>) -> Result<Self> {
        let dir = dir.as_ref().to_path_buf();
        std::fs::create_dir_all(&dir)?;
        Ok(Self { dir })
    }

    pub(crate) fn load(&self, key: &SessionKey) -> Result<Option<CompactionState>> {
        let path = self.path(key);
        match std::fs::read_to_string(&path) {
            Ok(content) => {
                let state: CompactionState = serde_json::from_str(&content)?;
                debug!(session = %key, "loaded compaction state");
                Ok(Some(state))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e).with_context(|| format!("failed to read {}", path.display())),
        }
    }

    pub(crate) fn save(&self, key: &SessionKey, state: &CompactionState) -> Result<()> {
        let path = self.path(key);
        let json = serde_json::to_string_pretty(state)?;
        std::fs::write(&path, json)?;
        debug!(session = %key, "saved compaction state");
        Ok(())
    }

    pub(crate) fn delete(&self, key: &SessionKey) -> Result<()> {
        let path = self.path(key);
        match std::fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e).with_context(|| format!("failed to delete {}", path.display())),
        }
    }

    fn path(&self, key: &SessionKey) -> PathBuf {
        let slug = key.to_string().replace(['/', ':'], "_");
        self.dir.join(format!("{slug}_compaction.json"))
    }
}
```

### Modifications to `gateway.rs`

Add two fields to `Gateway`:

```rust
pub(crate) struct Gateway {
    // ... existing fields ...
    compaction_store: CompactionStore,
    /// Cached compaction state per session (loaded lazily from disk).
    compaction_cache: Mutex<HashMap<SessionKey, CompactionState>>,
}
```

Initialize in `Gateway::new()`:

```rust
let compaction_store = CompactionStore::new(workspace.join("sessions"))?;
```

In `run_turn_with_trust`, add compaction check **before** the iteration loop, after appending the user message:

```rust
// Check if compaction is needed (proactive, before the turn starts)
let compaction = self.get_compaction(session_key);
let last_input_tokens = self.last_input_tokens(session_key);
let context_limit = self.provider.model_info().context_limit;

if compaction::should_compact(
    &self.messages(session_key),
    compaction.as_ref(),
    context_limit,
    last_input_tokens,
) {
    match compaction::compact(
        &self.messages(session_key),
        compaction.as_ref(),
        self.provider.as_ref(),
    ).await {
        Ok(new_state) => {
            info!(
                tokens_before = new_state.tokens_before,
                summary_len = new_state.summary.len(),
                first_kept = %new_state.first_kept_message_id,
                "session compacted"
            );
            self.set_compaction(session_key, new_state);
        }
        Err(e) => {
            warn!(error = %e, "compaction failed, continuing with full context");
        }
    }
}
```

Inside the iteration loop, replace the raw `self.messages()` call:

```rust
// BEFORE (current):
let messages = self.messages(session_key);

// AFTER:
let all_messages = self.messages(session_key);
let compaction = self.get_compaction(session_key);
let messages = compaction::build_provider_context(&all_messages, compaction.as_ref());
```

Add helper methods to `Gateway`:

```rust
/// Get compaction state for a session (from cache or disk).
fn get_compaction(&self, key: &SessionKey) -> Option<CompactionState> {
    let mut cache = self.compaction_cache.lock().expect("compaction cache poisoned");
    if let Some(state) = cache.get(key) {
        return Some(state.clone());
    }
    match self.compaction_store.load(key) {
        Ok(Some(state)) => {
            cache.insert(key.clone(), state.clone());
            Some(state)
        }
        Ok(None) => None,
        Err(e) => {
            warn!(session = %key, error = %e, "failed to load compaction state");
            None
        }
    }
}

/// Store compaction state (cache + disk).
fn set_compaction(&self, key: &SessionKey, state: CompactionState) {
    if let Err(e) = self.compaction_store.save(key, &state) {
        warn!(session = %key, error = %e, "failed to persist compaction state");
    }
    self.compaction_cache
        .lock()
        .expect("compaction cache poisoned")
        .insert(key.clone(), state);
}

/// Get the input token count from the last provider response in this session.
/// Returns None if no assistant messages with usage data exist.
fn last_input_tokens(&self, key: &SessionKey) -> Option<u32> {
    let messages = self.messages(key);
    // Walk backwards to find the last assistant message with usage metadata
    for msg in messages.iter().rev() {
        if msg.role == Role::Assistant {
            if let Some(tokens) = msg.metadata.get("input_tokens") {
                return tokens.as_u64().map(|t| t as u32);
            }
        }
    }
    None
}
```

Also update `clear_session` to clear compaction state:

```rust
pub(crate) fn clear_session(&self, session_key: &SessionKey) {
    self.sessions.lock().expect("...").remove(session_key);
    if let Err(e) = self.session_store.delete(session_key) { ... }
    // Clear compaction state too
    if let Err(e) = self.compaction_store.delete(session_key) {
        warn!(session = %session_key, error = %e, "failed to delete compaction state");
    }
    self.compaction_cache.lock().expect("...").remove(session_key);
}
```

**Usage tracking:** To make `last_input_tokens()` work, store the provider's reported `input_tokens` in the assistant message metadata. In `assistant_response_streaming` and `assistant_response_non_streaming`, before returning:

```rust
if let Some(input_tokens) = usage.input_tokens {
    response = response.with_metadata(
        "input_tokens",
        serde_json::Value::from(input_tokens),
    );
}
```

This uses the existing `Message.metadata` field — no type changes needed.

### Cut point rules

Valid cut points (can split here):
- **User messages** (start of a turn)
- **Assistant messages without tool requests** (natural stopping point)

Invalid cut points (never split here):
- **ToolResult messages** — must stay with the preceding ToolRequest
- **Assistant messages with tool requests** — the ToolResults that follow them must be included

When walking backwards, if the accumulated tokens exceed `keep_recent_tokens` and the current position is a ToolResult, keep walking back until reaching a valid cut point.

### Tests: `crates/coop-gateway/tests/compaction.rs`

```rust
// Test cases:

#[test]
fn small_session_no_compaction() {
    // A few messages well under the limit → should_compact returns false
}

#[test]
fn large_session_triggers_compaction() {
    // Messages exceeding context_limit - reserve → should_compact returns true
}

#[test]
fn build_context_without_compaction_returns_all() {
    // No compaction state → all messages returned unchanged
}

#[test]
fn build_context_with_compaction_prepends_summary() {
    // Compaction exists → returns [summary_user, summary_ack, kept_messages..]
}

#[test]
fn build_context_preserves_recent_messages() {
    // Messages after first_kept_message_id are returned unchanged
}

#[test]
fn cut_point_never_splits_tool_result() {
    // ToolResult messages are always kept with their ToolRequest
}

#[test]
fn cut_point_respects_keep_recent_tokens() {
    // Cut point keeps approximately keep_recent_tokens of recent content
}

#[test]
fn serialize_for_summary_produces_text_format() {
    // Verify [User]:, [Assistant]:, [Tool call]:, [Tool result]: format
}

#[test]
fn clear_session_clears_compaction() {
    // After clear, compaction state is None
}

#[tokio::test]
async fn compact_calls_provider_complete_fast() {
    // Use FakeProvider, verify it receives the summarization prompt
    // and the result becomes the CompactionState summary
}

#[tokio::test]
async fn compact_iterative_includes_previous_summary() {
    // Second compaction passes previous summary in <previous-summary> tags
}

#[tokio::test]
async fn compaction_failure_falls_back_to_full_context() {
    // Provider error during compaction → turn proceeds with full history
}
```

### Tracing verification

After implementation, run with `COOP_TRACE_FILE=traces.jsonl` and confirm:
- `compaction` spans appear when context exceeds threshold
- `running compaction` event shows `messages_to_summarize`, `cut_index`, `tokens_before`
- `compaction complete` event shows `summary_len`, `summary_tokens`
- `session compacted` event shows `tokens_before`, `first_kept`
- Subsequent `provider_request` spans show reduced `message_count`
- `input_tokens` in `provider response complete` events are bounded (~20K + summary, not growing without limit)

## Expected impact

With compaction at a 20K keep threshold, the trace scenario would look like:

```
Turn 1 baseline:   20,264 tokens  (no compaction needed)
Turn 2 baseline:   22,565 tokens  (no compaction needed)
Turn 3 baseline:   ~22,000 tokens (compacted: 58K → summary + 20K kept)
Turn 4 baseline:   ~22,000 tokens (compacted again)
...
Turn 9 baseline:   ~22,000 tokens (stable)
```

Each turn would cost ~22K × iterations instead of 90K × iterations. The session total would drop from **4.25M to ~1.1M input tokens** (~75% reduction), and more importantly, **the baseline stays bounded** regardless of session length.

## Files to create/modify

- **New:** `crates/coop-gateway/src/compaction.rs` (~300 lines)
- **New:** `crates/coop-gateway/src/compaction_store.rs` (~80 lines)
- **Modify:** `crates/coop-gateway/src/gateway.rs` (~50 lines: add fields, compaction check, context building, usage tracking)
- **New:** `crates/coop-gateway/tests/compaction.rs`
- **No changes to:** `coop-core`, `coop-agent`, `coop-channels`, `coop-tui`
