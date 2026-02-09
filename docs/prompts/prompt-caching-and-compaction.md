# Prompt Caching and Context Compaction

Two changes that address the same problem from different angles: re-sending conversation history is expensive, and it grows without bound.

**Prompt caching** makes re-sending cheap. Anthropic charges 10% for cached input tokens. Within a tool-call turn where the same prefix is resent 10+ times, caching alone saves ~80% of input costs.

**Compaction** keeps context bounded. When conversation history grows large enough that even cached tokens add up, the LLM summarizes old messages into a structured checkpoint, keeping only recent messages verbatim.

## Evidence

A 9-turn session made 69 API calls totaling 5.2M input tokens (~$78 at Opus pricing). Every call resent the full conversation history. No prompt caching was active (the provider doesn't set `cache_control` on messages or tools, and doesn't parse cache usage from responses). No compaction exists.

```
Turn 1 baseline:   20K tokens   Turn 9 baseline:   90K tokens
69 API calls × 75K avg = 5.2M total input tokens billed at full price
```

With prompt caching: the stable prefix (system prompt + tools + prior messages) would be cached after the first call. Estimated cost drops from $78 to ~$15-20.

With compaction at a 100K threshold: baseline stays bounded instead of growing to 90K+. Combined with caching, estimated cost: ~$8-10.

---

## Part 1: Prompt Caching

### What Anthropic caches

Verified against `@anthropic-ai/sdk` v0.73.0 (`src/resources/messages/messages.ts`).

Anthropic caches by **byte prefix**. Everything from the start of the request up to the last `cache_control` breakpoint is eligible for caching. On cache hit, those tokens cost 10% of the normal input price. On cache miss (first write), they cost 125%.

Cache TTL options (from `CacheControlEphemeral`):
- `"5m"` (default): 5-minute lifetime, 1.25× base price on write
- `"1h"`: 1-hour lifetime, 2× base price on write

Both are refreshed on each hit for no additional cost.

Cache breakpoints can be placed on **any content block** that has a `cache_control` field. Per the SDK types, this includes:
- `TextBlockParam` (system blocks, message text)
- `ImageBlockParam` (user message images)
- `DocumentBlockParam` (PDFs, plain text docs)
- `ToolUseBlockParam` (assistant tool_use blocks)
- `ToolResultBlockParam` (user tool_result blocks)
- `Tool` / `ToolBash20250124` / `ToolTextEditor*` / `WebSearchTool20250305` (tool definitions)
- `SearchResultBlockParam`, `ServerToolUseBlockParam`, `WebSearchToolResultBlockParam`

The SDK type for all of these is:
```typescript
cache_control?: CacheControlEphemeral | null;
// where CacheControlEphemeral = { type: 'ephemeral'; ttl?: '5m' | '1h' }
```

Cache prefixes are created in order: **tools → system → messages**. Up to **4 cache breakpoints** per request. Minimum cacheable prefix varies by model:
- 1,024 tokens: Sonnet 4.5, Opus 4.1, Opus 4, Sonnet 4, Sonnet 3.7
- 2,048 tokens: Haiku 3.5, Haiku 3
- 4,096 tokens: Opus 4.6, Opus 4.5, Haiku 4.5

**20-block lookback window:** The system checks up to 20 blocks backward from each explicit breakpoint. Content more than 20 blocks before a breakpoint won't get cache hits unless you add additional breakpoints closer to that content.

### Response usage fields

From `Usage` in the SDK:

```typescript
interface Usage {
  input_tokens: number;
  output_tokens: number;
  cache_creation_input_tokens: number | null;
  cache_read_input_tokens: number | null;
  cache_creation: CacheCreation | null;  // NEW: granular by TTL
  server_tool_use: ServerToolUsage | null;
  service_tier: 'standard' | 'priority' | 'batch' | null;
  inference_geo: string | null;
}

interface CacheCreation {
  ephemeral_1h_input_tokens: number;
  ephemeral_5m_input_tokens: number;
}
```

In streaming, `MessageDeltaUsage` (from `message_delta` events) also includes:
```typescript
interface MessageDeltaUsage {
  output_tokens: number;
  input_tokens: number | null;
  cache_creation_input_tokens: number | null;
  cache_read_input_tokens: number | null;
  server_tool_use: ServerToolUsage | null;
}
```

Total input tokens = `input_tokens` + `cache_creation_input_tokens` + `cache_read_input_tokens`.

### What coop does now

**System prompt (OAuth path):** Two blocks with `cache_control: { "type": "ephemeral" }`. Correct.

**System prompt (non-OAuth path):** Sent as a plain string (`json!(system)`). No cache_control. **Not cached at all.**

**Tool definitions:** No `cache_control` on any tool. Tool definitions are part of the prefix and identical across calls within a session. **Not cached.**

**Messages:** No `cache_control` on any message. The entire conversation history — identical up to the current turn — is re-billed at full price on every call. **This is the main cost driver.**

**Usage parsing:** `ApiUsage` and `SseMessageStartUsage` only parse `input_tokens` and `output_tokens`. Anthropic returns `cache_creation_input_tokens`, `cache_read_input_tokens`, and `cache_creation` (granular) but coop drops them. No visibility into whether caching works.

### Changes to `crates/coop-agent/src/anthropic_provider.rs`

#### 1. Parse cache usage from API responses

Add cache fields to usage structs. Use `#[serde(default)]` for backward compatibility:

```rust
#[derive(Debug, Deserialize)]
struct ApiUsage {
    input_tokens: u32,
    output_tokens: u32,
    #[serde(default)]
    cache_creation_input_tokens: Option<u32>,
    #[serde(default)]
    cache_read_input_tokens: Option<u32>,
}
```

Same for `SseMessageStartUsage`:

```rust
#[derive(Debug, Deserialize)]
struct SseMessageStartUsage {
    input_tokens: u32,
    output_tokens: Option<u32>,
    #[serde(default)]
    cache_creation_input_tokens: Option<u32>,
    #[serde(default)]
    cache_read_input_tokens: Option<u32>,
}
```

Update `parse_usage` to populate the existing `Usage` cache fields:

```rust
fn parse_usage(response: &AnthropicResponse) -> Usage {
    Usage {
        input_tokens: Some(response.usage.input_tokens),
        output_tokens: Some(response.usage.output_tokens),
        cache_read_tokens: response.usage.cache_read_input_tokens,
        cache_write_tokens: response.usage.cache_creation_input_tokens,
        stop_reason: response.stop_reason.clone(),
    }
}
```

For streaming, update `SseState::handle_event` for both `MessageStart` and `MessageDelta`:

```rust
// MessageStart — initial usage snapshot
SseEvent::MessageStart { message } => {
    if let Some(u) = message.usage {
        self.usage.input_tokens = Some(u.input_tokens);
        if let Some(out) = u.output_tokens {
            self.usage.output_tokens = Some(out);
        }
        self.usage.cache_read_tokens = u.cache_read_input_tokens;
        self.usage.cache_write_tokens = u.cache_creation_input_tokens;
    }
    SseAction::Continue
}

// MessageDelta — cumulative usage update (cache fields may appear here)
SseEvent::MessageDelta { delta, usage } => {
    if let Some(u) = usage {
        self.usage.output_tokens = Some(u.output_tokens);
        // SDK shows cache fields can update in message_delta too
        if let Some(v) = u.cache_creation_input_tokens {
            self.usage.cache_write_tokens = Some(v);
        }
        if let Some(v) = u.cache_read_input_tokens {
            self.usage.cache_read_tokens = Some(v);
        }
    }
    // ... handle delta.stop_reason etc.
}
```

Add `cache_creation_input_tokens` and `cache_read_input_tokens` to the `SseMessageDelta` usage struct too:

```rust
#[derive(Debug, Deserialize)]
struct SseMessageDeltaUsage {
    output_tokens: u32,
    #[serde(default)]
    input_tokens: Option<u32>,
    #[serde(default)]
    cache_creation_input_tokens: Option<u32>,
    #[serde(default)]
    cache_read_input_tokens: Option<u32>,
}
```

#### 2. System prompt caching for non-OAuth path

Currently the non-OAuth path sends `"system": "plain string"`. Change to always return a structured array with `cache_control`:

```rust
fn build_system_blocks(&self, system: &str) -> Value {
    if self.is_oauth {
        // OAuth: Claude Code identity + system prompt, both cached
        json!([
            {
                "type": "text",
                "text": "You are Claude Code, Anthropic's official CLI for Claude.",
                "cache_control": { "type": "ephemeral" }
            },
            {
                "type": "text",
                "text": system,
                "cache_control": { "type": "ephemeral" }
            }
        ])
    } else {
        // Non-OAuth: system prompt as cached block
        json!([{
            "type": "text",
            "text": system,
            "cache_control": { "type": "ephemeral" }
        }])
    }
}
```

Note: The `system` parameter accepts `string | Array<TextBlockParam>` — both are valid. The array form is required for `cache_control`.

#### 3. Tool definition caching

Add `cache_control` to the **last** tool definition. Per the SDK, every tool type (`Tool`, `ToolBash20250124`, `ToolTextEditor*`, `WebSearchTool20250305`) supports `cache_control`. This means the system prompt + all tool definitions form a cached prefix:

```rust
fn format_tools(tools: &[ToolDef], prefix: bool) -> Vec<Value> {
    let len = tools.len();
    tools
        .iter()
        .enumerate()
        .map(|(i, t)| {
            let name = if prefix {
                format!("{TOOL_PREFIX}{}", t.name)
            } else {
                t.name.clone()
            };
            let mut tool = json!({
                "name": name,
                "description": t.description,
                "input_schema": t.parameters
            });
            // Cache breakpoint on last tool
            if i == len - 1 {
                tool["cache_control"] = json!({ "type": "ephemeral" });
            }
            tool
        })
        .collect()
}
```

#### 4. Conversation message caching

This is the biggest win. Within a tool-call loop, each iteration resends the same conversation prefix plus one new tool result.

The `cache_control` field is valid on **any content block** in messages: `TextBlockParam`, `ToolUseBlockParam`, `ToolResultBlockParam`, `ImageBlockParam`, etc. Set it on the **last content block of the second-to-last message** — this caches the entire conversation prefix.

Modify `format_messages` to accept a `cache_at` parameter:

```rust
fn format_messages(messages: &[Message], prefix_tools: bool, cache_at: Option<usize>) -> Vec<Value> {
    let mut formatted: Vec<Value> = Vec::new();
    let mut source_index = 0;

    for m in messages {
        let role = match m.role {
            Role::User => "user",
            Role::Assistant => "assistant",
        };

        let content: Vec<Value> = m
            .content
            .iter()
            .filter_map(|c| match c {
                // ... existing content block formatting, unchanged ...
            })
            .collect();

        if content.is_empty() {
            source_index += 1;
            continue;
        }

        let mut msg = json!({
            "role": role,
            "content": content
        });

        // Set cache breakpoint on the designated message
        if cache_at == Some(source_index) {
            if let Some(arr) = msg["content"].as_array_mut() {
                if let Some(last_block) = arr.last_mut() {
                    last_block["cache_control"] = json!({ "type": "ephemeral" });
                }
            }
        }

        formatted.push(msg);
        source_index += 1;
    }

    formatted
}
```

Update `build_body` to compute the cache breakpoint position. With up to 4 breakpoints total and 2-3 used by system+tools, we have 1-2 for messages. Place one on the second-to-last message:

```rust
fn build_body(
    &self,
    system: &str,
    messages: &[Message],
    tools: &[ToolDef],
    stream: bool,
) -> Value {
    // Place cache breakpoint on second-to-last message.
    // This caches the entire conversation prefix — only the last
    // message (newest content) is uncached.
    let cache_at = if messages.len() >= 2 {
        Some(messages.len() - 2)
    } else {
        None
    };

    let mut body = json!({
        "model": self.model.name,
        "max_tokens": 8192,
        "system": self.build_system_blocks(system),
        "messages": Self::format_messages(messages, self.is_oauth, cache_at),
    });

    // ... rest unchanged ...
}
```

#### 5. Tracing cache usage

Update `provider response complete` tracing events in `gateway.rs`:

```rust
info!(
    input_tokens = usage.input_tokens,
    output_tokens = usage.output_tokens,
    cache_read_tokens = usage.cache_read_tokens,
    cache_write_tokens = usage.cache_write_tokens,
    stop_reason = %usage.stop_reason.as_deref().unwrap_or("unknown"),
    "provider response complete"
);
```

### Testing caching

After implementing, verify with tracing:

```bash
COOP_TRACE_FILE=traces.jsonl cargo run -- <normal session>
```

After the first API call in a tool-call loop:
- `cache_write_tokens` should be >0 (prefix written to cache)
- `cache_read_tokens` should be 0

On subsequent calls within the same turn:
- `cache_read_tokens` should be large (most of prefix cached)
- `cache_write_tokens` should be small (only new content written)

If `cache_read_tokens` is always 0, the breakpoints aren't working. Check that the prefix meets the minimum token threshold for the model being used and that `cache_control` is on the right content block.

### Expected cost impact

For the traced 69-call session:
- System prompt + tools (~20K tokens): cached after first call → 90% savings on 68 calls
- Conversation prefix (grows 20K→90K): mostly cached within each turn → ~80% savings on repeated prefix
- Only new content (latest tool result, ~1-3K per iteration) billed at full price

Estimated reduction: 5.2M billed tokens → ~1.5M effective tokens at full-price equivalent. **~$78 → ~$20.**

---

## Part 2: Context Compaction

Even with prompt caching, unbounded context growth is a problem:
- Cached tokens still cost 10% — at 90K baseline × 69 calls, that's still 6.2M cached tokens ($9 at 10%)
- Eventually the session hits the 200K context window limit
- Large context slows inference (time-to-first-token scales with context size)

Compaction summarizes old messages when context exceeds a threshold, replacing conversation history with a structured summary. The Anthropic SDK v0.73.0 has this built into `BetaToolRunner` — we follow the same approach.

### SDK reference implementation

From `@anthropic-ai/sdk/src/lib/tools/BetaToolRunner.ts` and `CompactionControl.ts`:

**Threshold:** `DEFAULT_TOKEN_THRESHOLD = 100_000` — triggers when `input_tokens + cache_creation_input_tokens + cache_read_input_tokens + output_tokens` exceeds this.

**Mechanism:**
1. After each assistant response, check total tokens against threshold
2. If over threshold, strip `tool_use` blocks from the last assistant message (to avoid 400 errors — `tool_use` requires `tool_result`)
3. Append the summary prompt as a user message to the full conversation
4. Call `client.beta.messages.create()` with the augmented messages
5. Replace the entire `messages` array with a single user message containing the summary text

**Summary prompt** (from `CompactionControl.ts`):
```
You have been working on the task described above but have not yet completed it.
Write a continuation summary that will allow you (or another instance of yourself)
to resume work efficiently in a future context window where the conversation history
will be replaced with this summary. Your summary should be structured, concise, and
actionable. Include:
1. Task Overview
2. Current State
3. Important Discoveries
4. Next Steps
5. Context to Preserve
Wrap your summary in <summary></summary> tags.
```

**Result format:** The compacted messages array becomes:
```json
[{ "role": "user", "content": [{ "type": "text", "text": "<summary>...</summary>" }] }]
```

Note: The SDK does **not** keep recent messages verbatim — it replaces the **entire** history with a single summary message. Simple and aggressive.

### Design for coop

Follow the SDK's approach but adapted for coop's architecture where the full session is persisted on disk.

**Trigger:** After each turn completes, check the most recent `Usage.input_tokens + cache_creation_input_tokens + cache_read_input_tokens + output_tokens`. If it exceeds `COMPACTION_THRESHOLD`, compact before the next turn.

**Configuration:**

```rust
/// Compact when total tokens exceeds this.
/// Matches Anthropic SDK default.
const COMPACTION_THRESHOLD: usize = 100_000;
```

### Compaction state

```rust
// crates/coop-gateway/src/compaction.rs

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub(crate) struct CompactionState {
    /// LLM-generated structured summary.
    pub summary: String,
    /// Message ID from which to start sending to the provider.
    /// None = send only the summary (SDK-style total replacement).
    pub first_kept_message_id: Option<String>,
    /// Total tokens at time of compaction.
    pub tokens_at_compaction: u32,
    /// Timestamp.
    pub created_at: chrono::DateTime<chrono::Utc>,
}
```

Stored as `{session_slug}_compaction.json` alongside session JSONL files.

### Compaction process

Following the SDK's approach:

1. Take the full message history
2. If last message is assistant with `tool_use` blocks, strip them (or remove the message if it only contained `tool_use`)
3. Append the summary prompt as a user message
4. Call provider with these messages (using the same model)
5. Store the summary text as compaction state
6. On future provider calls, send only `[{ role: "user", content: summary }]` plus messages added after compaction

```rust
pub(crate) async fn compact(
    messages: &[Message],
    provider: &dyn Provider,
    model: &str,
) -> Result<CompactionState> {
    // Strip tool_use from last assistant message to avoid 400 error
    let mut msgs = messages.to_vec();
    if let Some(last) = msgs.last_mut() {
        if last.role == Role::Assistant {
            last.content.retain(|c| !matches!(c, Content::ToolRequest { .. }));
            if last.content.is_empty() {
                msgs.pop();
            }
        }
    }

    // Append summary prompt
    msgs.push(Message::user().with_text(SUMMARY_PROMPT));

    // Call provider
    let response = provider.complete(&msgs, /* system */ "", /* tools */ &[]).await?;

    // Extract summary text
    let summary = response.content.iter()
        .filter_map(|c| match c {
            Content::Text(t) => Some(t.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");

    Ok(CompactionState {
        summary,
        first_kept_message_id: None,
        tokens_at_compaction: /* from usage */,
        created_at: chrono::Utc::now(),
    })
}
```

### Summary prompt

Use the same prompt as the Anthropic SDK, adapted:

```rust
const SUMMARY_PROMPT: &str = r#"You have been working on the task described above but have not yet completed it. Write a continuation summary that will allow you (or another instance of yourself) to resume work efficiently in a future context window where the conversation history will be replaced with this summary. Your summary should be structured, concise, and actionable. Include:
1. Task Overview — The user's core request and success criteria, any clarifications or constraints
2. Current State — What has been completed, files created/modified/analyzed (with paths), key outputs
3. Important Discoveries — Technical constraints, decisions made and rationale, errors and resolutions, approaches that didn't work
4. Next Steps — Specific actions needed, blockers or open questions, priority order
5. Context to Preserve — User preferences, domain-specific details, promises made
Be concise but complete—err on the side of including information that would prevent duplicate work or repeated mistakes. Write in a way that enables immediate resumption of the task.
Wrap your summary in <summary></summary> tags."#;
```

### Building provider context

```rust
pub(crate) fn build_provider_context(
    all_messages: &[Message],
    compaction: Option<&CompactionState>,
) -> Vec<Message> {
    let Some(state) = compaction else {
        return all_messages.to_vec();
    };

    // Following SDK: replace history with summary as single user message
    let summary_msg = Message::user().with_text(state.summary.clone());

    match &state.first_kept_message_id {
        None => {
            // SDK-style: only summary, no kept messages
            vec![summary_msg]
        }
        Some(id) => {
            // Hybrid: summary + recent messages
            let kept_start = all_messages
                .iter()
                .position(|m| m.id == *id)
                .unwrap_or(0);
            let mut context = vec![summary_msg];
            context.extend_from_slice(&all_messages[kept_start..]);
            context
        }
    }
}
```

### Integration with gateway.rs

Add fields to `Gateway`:

```rust
pub(crate) struct Gateway {
    // ... existing fields ...
    compaction_store: CompactionStore,
    compaction_cache: Mutex<HashMap<SessionKey, CompactionState>>,
}
```

**Check after each turn** — at the end of `run_turn_with_trust`, after the iteration loop:

```rust
// After the turn loop, check if compaction is needed for next turn
let total_tokens = total_usage.input_tokens.unwrap_or(0) as usize
    + total_usage.cache_read_tokens.unwrap_or(0) as usize
    + total_usage.cache_write_tokens.unwrap_or(0) as usize
    + total_usage.output_tokens.unwrap_or(0) as usize;

if total_tokens > COMPACTION_THRESHOLD {
    let all_messages = self.messages(session_key);
    match compaction::compact(&all_messages, self.provider.as_ref(), &self.model).await {
        Ok(state) => {
            info!(
                tokens_before = total_tokens,
                summary_len = state.summary.len(),
                "session compacted"
            );
            self.set_compaction(session_key, state);
        }
        Err(e) => {
            warn!(error = %e, "compaction failed, continuing with full context");
        }
    }
}
```

**Apply compaction in the iteration loop:**

```rust
// BEFORE:
let messages = self.messages(session_key);

// AFTER:
let all_messages = self.messages(session_key);
let compaction_state = self.get_compaction(session_key);
let messages = compaction::build_provider_context(&all_messages, compaction_state.as_ref());
```

**Clear compaction on session clear.**

### Compaction store

~80 lines. JSON files alongside session JSONL:

```rust
pub(crate) struct CompactionStore {
    dir: PathBuf,
}

impl CompactionStore {
    pub(crate) fn new(dir: impl AsRef<Path>) -> Result<Self> { ... }
    pub(crate) fn load(&self, key: &SessionKey) -> Result<Option<CompactionState>> { ... }
    pub(crate) fn save(&self, key: &SessionKey, state: &CompactionState) -> Result<()> { ... }
    pub(crate) fn delete(&self, key: &SessionKey) -> Result<()> { ... }
}
```

### Tests

**`crates/coop-gateway/tests/compaction.rs`:**

```rust
#[test]
fn below_threshold_does_not_compact()

#[test]
fn above_threshold_triggers_compaction()

#[test]
fn build_context_without_compaction_returns_all()

#[test]
fn build_context_with_compaction_returns_summary_only()
// SDK-style: single user message with summary

#[test]
fn strip_tool_use_from_last_assistant_message()
// tool_use blocks removed before sending to summarizer

#[test]
fn strip_tool_use_removes_empty_assistant_message()
// If last assistant message was only tool_use, it's removed entirely

#[tokio::test]
async fn compact_calls_provider_with_summary_prompt_appended()

#[tokio::test]
async fn compaction_failure_is_non_fatal()
// Provider error → turn proceeds with full context, warning logged

#[test]
fn clear_session_removes_compaction_state()
```

**Add caching tests to `crates/coop-agent/src/anthropic_provider.rs`:**

```rust
#[test]
fn format_messages_sets_cache_control_on_second_to_last()

#[test]
fn format_messages_no_cache_on_single_message()

#[test]
fn format_tools_sets_cache_on_last_tool()

#[test]
fn build_system_blocks_non_oauth_has_cache_control()

#[test]
fn parse_usage_includes_cache_tokens()

#[test]
fn parse_usage_handles_missing_cache_fields()
// Backward compat: cache tokens are None when not present
```

### Tracing verification

After both changes, run with `COOP_TRACE_FILE=traces.jsonl` and verify:

**Caching working:**
```bash
grep "provider response complete" traces.jsonl | python3 -c "
import sys, json
for line in sys.stdin:
    obj = json.loads(line)
    f = obj.get('fields', {})
    if f.get('message') == 'provider response complete':
        print(f'input={f.get(\"input_tokens\",0):>8,}  cache_read={f.get(\"cache_read_tokens\",0):>8,}  cache_write={f.get(\"cache_write_tokens\",0):>8,}')
"
```

**Compaction working:**
```bash
grep "session compacted" traces.jsonl
```

---

## Files to create/modify

### Part 1 (caching) — `coop-agent` only:
- **Modify:** `crates/coop-agent/src/anthropic_provider.rs`
  - `ApiUsage`: add `cache_creation_input_tokens`, `cache_read_input_tokens`
  - `SseMessageStartUsage`: add same cache fields
  - `SseMessageDeltaUsage`: add same cache fields (SDK shows these in `message_delta`)
  - `parse_usage`: populate `cache_read_tokens`, `cache_write_tokens`
  - `SseState::handle_event`: populate cache fields from both `MessageStart` and `MessageDelta`
  - `build_system_blocks`: non-OAuth path returns array with `cache_control`
  - `format_tools`: add `cache_control` on last tool
  - `format_messages`: accept `cache_at` param, set `cache_control` on designated message's last content block
  - `build_body`: compute `cache_at = messages.len() - 2`
- **Modify:** `crates/coop-gateway/src/gateway.rs`
  - Add `cache_read_tokens`, `cache_write_tokens` to `provider response complete` tracing events

### Part 2 (compaction) — `coop-gateway` only:
- **New:** `crates/coop-gateway/src/compaction.rs` (~200 lines)
- **New:** `crates/coop-gateway/src/compaction_store.rs` (~80 lines)
- **Modify:** `crates/coop-gateway/src/gateway.rs`
  - Add `compaction_store`, `compaction_cache` fields
  - Add compaction check after turn loop (total tokens > 100K threshold)
  - Apply `build_provider_context` in iteration loop
  - Clear compaction state on session clear
- **New:** `crates/coop-gateway/tests/compaction.rs`

### No changes to:
- `coop-core` (types, traits, prompt — all unchanged)
- `coop-channels`
- `coop-tui`

## SDK differences from initial prompt

Corrections made after reviewing `@anthropic-ai/sdk` v0.73.0:

1. **`ttl` field on `cache_control`**: The SDK shows `CacheControlEphemeral` has an optional `ttl` field (`"5m" | "1h"`). Initial prompt only showed `{"type": "ephemeral"}` which defaults to 5m — correct but the option exists for 1h caching at 2× write cost.

2. **`cache_creation` granular field**: Response `Usage` now includes `cache_creation: { ephemeral_1h_input_tokens, ephemeral_5m_input_tokens }` in addition to the flat `cache_creation_input_tokens`. We can ignore the granular field for now (we only use 5m caching) but should deserialize and skip it.

3. **`message_delta` carries cache fields**: Initial prompt only updated cache fields from `message_start`. The SDK's `MessageDeltaUsage` also includes `cache_creation_input_tokens` and `cache_read_input_tokens` — must handle both events.

4. **`cache_control` on `ToolResultBlockParam` and `ToolUseBlockParam`**: Initial prompt only mentioned tool definitions and text blocks. The SDK shows cache_control is valid on tool_result and tool_use content blocks too — useful for caching large tool outputs.

5. **Compaction threshold**: Changed from 80K to 100K to match SDK's `DEFAULT_TOKEN_THRESHOLD`.

6. **Compaction replaces all history**: The SDK replaces the entire message array with a single user message containing the summary. Initial prompt kept recent messages verbatim. The SDK approach is simpler and sufficient — but we support both modes via `first_kept_message_id`.

7. **Compaction strips `tool_use` blocks**: Before sending to the summarizer, the SDK removes `tool_use` blocks from the last assistant message to avoid 400 errors (orphaned tool_use without tool_result). Important edge case.

8. **Compaction uses same model by default**: The SDK defaults to the same model for summarization. Our initial prompt mentioned `complete_fast()` — this should use the same model unless configured otherwise.

9. **No `<previous-summary>` iterative mode**: The SDK doesn't do iterative compaction with a previous summary. It just re-compacts the full (already compacted) conversation. Simpler.

10. **Breakpoint limit is 4, not unlimited**: Must be strategic about placement. With system (1-2) + tools (1) + messages (1) = 4 breakpoints used.
