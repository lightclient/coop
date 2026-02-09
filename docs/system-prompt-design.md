# System Prompt Architecture

## Overview

Coop owns the agent runtime (tool loop, streaming) and **the system prompt**. The prompt is how Coop enforces trust, injects personality, and provides context — it's the primary interface between the gateway and the LLM.

This document covers how prompts are built, what prior art teaches us, and how observability integrates.

---

## Prior Art Comparison

We studied two systems: OpenClaw and NanoClaw.

### OpenClaw

Single `buildAgentSystemPrompt()` function (~400 lines JS) that returns one flat string with `##` markdown headers. All workspace files (SOUL.md, AGENTS.md, TOOLS.md, IDENTITY.md, USER.md, HEARTBEAT.md, BOOTSTRAP.md, MEMORY.md) are hardcoded filenames loaded at boot and injected into the prompt as "Project Context." Subagents get a stripped set (AGENTS.md + TOOLS.md only). No trust gating — single owner model. No prompt caching strategy. Head/tail truncation at 20k chars per file. Plugin hooks can mutate the file list before assembly.

**Takeaways:** Convention-over-configuration works (the 8 hardcoded filenames are universally understood). Skills as discovery (list descriptions, let agent fetch on demand) keeps the base prompt small. Head/tail truncation with markers is practical.

### NanoClaw

No prompt builder at all. Delegates entirely to Claude Code's native `CLAUDE.md` convention via the Claude Agent SDK (`query()` function). Each group chat gets its own directory with its own `CLAUDE.md`. Security is filesystem isolation (Linux containers), not application-level checks. Memory is self-editing `CLAUDE.md` + archived conversation transcripts as markdown files.

**Takeaways:** You might not need a complex prompt builder if you lean on the right runtime. Filesystem isolation > application-level permission checks. Per-group directory isolation is clean. The "understand in 8 minutes" philosophy keeps complexity honest.

---

## Prompt Construction

### Layered Architecture

The system prompt is assembled from layers, each with different caching and refresh characteristics:

```
┌─────────────────────────────────────┐
│ Layer 0: Identity (SOUL.md)         │  stable per agent, cacheable
│ Layer 1: Behavior (AGENTS.md)       │  stable per agent, cacheable
├─────────────────────────────────────┤
│ Layer 2: User context               │  static per user, varies by session
│ Layer 3: Workspace files            │  semi-static, refresh on file change
│ Layer 4: Channel context            │  static per session (channel-specific)
├─────────────────────────────────────┤
│ Layer 5: Extensions / Tools         │  injected from tool definitions
│ Layer 6: Runtime context            │  dynamic per turn (date, model, channel)
│ Layer 7: Situation rules            │  depends on DM vs group vs cron
│ Layer 8: Memory index               │  lightweight TOC, not full content
└─────────────────────────────────────┘
```

Layers 0-1 are stable per agent and rarely change.
Layers 2-8 vary per turn or session.

### Channel Context

Layer 4 injects channel-specific formatting instructions so the agent adapts its output to each transport's capabilities. Resolution order:

1. **Workspace file** — `channels/<family>.md` (e.g. `channels/signal.md`). If present and non-empty, used verbatim.
2. **Built-in default** — hardcoded for known channels (currently: Signal → plain text, no markdown).
3. **None** — channels without a file or built-in get no layer (e.g. terminal).

The channel "family" is extracted from the channel identifier by taking everything before the first colon (`terminal:default` → `terminal`). This layer uses `CacheHint::Session` since the channel never changes within a session.

### Trust Gating

Each workspace file has a minimum trust level. Files are only injected when `effective_trust >= file.trust`:

```yaml
prompt:
  files:
    - path: MEMORY.md
      trust: full        # only owner sees this
    - path: TOOLS.md
      trust: familiar    # most users can see tool notes
    - path: IDENTITY.md
      trust: familiar
    - path: USER.md
      per_user: true     # load from user's workspace
      trust: inner
```

Resolution: given effective trust `min(user.trust, situation.ceiling)`, filter files where `effective_trust >= file.trust`.

Example: Alice (full trust) in a group chat (familiar ceiling) → effective trust = familiar → sees TOOLS.md and IDENTITY.md, not MEMORY.md.

### File Convention

Following OpenClaw's proven convention, these filenames have semantic meaning:

| File | Purpose | Default Trust |
|------|---------|---------------|
| `SOUL.md` | Agent personality, voice, values | familiar |
| `AGENTS.md` | Behavioral instructions, memory rules | familiar |
| `TOOLS.md` | Tool usage notes, setup specifics | familiar |
| `IDENTITY.md` | Who the agent is (name, history) | familiar |
| `USER.md` | About the user (per-user workspace) | inner |
| `MEMORY.md` | Long-term curated memory | full |
| `HEARTBEAT.md` | Periodic check tasks | full |

Files are loaded from `{workspace}/` with fallback to defaults. Missing files are skipped (no `[MISSING]` markers — simpler).

### Memory: Index, Not Dump

Unlike OpenClaw (which injects full MEMORY.md into every prompt), Coop gives the agent a **priced menu** — a lightweight index showing what's available and what it costs to load:

```
## Available Memory (trust: full)
- private/MEMORY.md   (3,847 tok) — long-term facts, people, dates, preferences
- private/FINANCE.md    (920 tok) — financial profile
- shared/RECENT.md    (1,102 tok) — rolling 7-day context
- social/PEOPLE.md      (814 tok) — public-safe people info

Remaining budget: ~118k tokens. Use memory_search to find specific facts,
or memory_get(path, from, lines) to load sections.
```

The agent decides what to pull based on the question. "What's Carol's birthday?" → `memory_search("Carol birthday")` returns a 50-token snippet, not 4k tokens of MEMORY.md. The token cost of each search result is shown so the agent can decide whether to fetch more or stop.

A `familiar` trust session sees only social stores in the index. A `public` session sees no memory index at all.

This is the **progressive disclosure** model: the agent always knows what's available and what it costs, and makes informed decisions about what enters the context window.

### Situation Overlays

Instead of one massive AGENTS.md with conditional sections, situation-specific rules live in separate files referenced by config:

```yaml
situations:
  dm:
    ceiling: full
  group:
    ceiling: familiar
    prompt_overlay: ./prompts/group-rules.md
  system:
    ceiling: full
    prompt_overlay: ./prompts/heartbeat-rules.md
```

The overlay content is appended to the prompt only when the session matches that situation.

---

## Phased Implementation

### Phase 1: Simple (NanoClaw-inspired)

Read files from workspace directory, concatenate, pass as `override_system_prompt()`. Trust gating is the only "smart" part. No caching, no memory index, no situation overlays.

```rust
fn build_prompt(workspace: &Path, trust: TrustLevel) -> String {
    let mut parts = vec![];
    for file in PROMPT_FILES {
        if trust >= file.min_trust {
            if let Ok(content) = fs::read_to_string(workspace.join(file.name)) {
                parts.push(format!("## {}\n{}", file.name, content));
            }
        }
    }
    parts.join("\n\n")
}
```

### Phase 2: Caching

#### How Anthropic prompt caching actually works

Anthropic's API accepts `system` as an array of content blocks, each with optional `cache_control`. The **only** cache type is `"ephemeral"` — despite the name, it means "cache this for ~5 minutes." There is no "stable" or "permanent" cache option.

Coop's Anthropic provider wraps the system prompt as content blocks with `cache_control: {"type": "ephemeral"}`. Anthropic then automatically caches **prefix bytes** — identical leading content across API calls gets a ~90% input token discount within the TTL window.

**What this means for us:**
- Our layer ordering (Stable identity/behavior first → Session context → Volatile runtime last) is already getting cache hits on the stable prefix, with zero extra work.
- `CacheHint::Stable/Session/Volatile` in our code describes *how often we expect content to change*, not anything in Anthropic's API. It drives layer ordering, which drives prefix cache hit rates.
- Explicit multi-block breakpoints (separate cache entries for identity vs. session context) can be added by sending multiple system blocks. Prefix caching already covers the common case.
- Other providers (OpenAI, Bedrock, etc.) have different or no caching semantics. Our ordering is a net positive regardless.

### Phase 3: Memory Index

Replace full MEMORY.md injection with TOC + token annotations. Add memory_search/memory_get tools scoped by trust level.

### Phase 4: Situation Overlays

Load situation-specific prompt files. Per-user workspace files.

---

## Observability Design

### The Problem

OpenClaw's observability is poor. When something fails, the agent can't introspect its own logs to determine what happened. Debugging requires the human to manually check processes, log files, and gateway state.

Coop should be **self-diagnosable**: the agent should be able to answer "what went wrong?" by querying its own telemetry.

### Structured Tracing

Coop uses the `tracing` crate. Gateway-level spans cover every operation:

```
[gateway] message_received channel=signal user=alice trust=full
  [router] resolved_trust user=full situation=dm effective=full
  [prompt] built_system_prompt layers=6 tokens=4200 cached_tokens=3100
  [agent] turn_started session=abc model=claude-opus-4-5
    [provider] api_call model=claude-opus-4-5 input_tokens=5200
    [tool] bash command="ls" duration=120ms status=ok
    [tool] memory_search query="birthday" results=3 duration=80ms
    [provider] api_call model=claude-opus-4-5 input_tokens=5800
  [agent] turn_completed duration=4.2s total_tokens=8500 cost=$0.12
  [channel] response_sent channel=signal length=240
```

### Event Log (SQLite)

Every significant event gets a row in an `events` table:

```sql
CREATE TABLE events (
    id          INTEGER PRIMARY KEY,
    timestamp   TEXT NOT NULL,
    level       TEXT NOT NULL,        -- info, warn, error
    category    TEXT NOT NULL,        -- gateway, router, prompt, agent, tool, channel
    session_id  TEXT,
    agent_id    TEXT,
    user_id     TEXT,
    channel     TEXT,
    event_type  TEXT NOT NULL,        -- message_received, turn_started, tool_call, api_error, etc.
    duration_ms INTEGER,
    tokens_in   INTEGER,
    tokens_out  INTEGER,
    cost_usd    REAL,
    error       TEXT,
    metadata    TEXT                  -- JSON blob for event-specific data
);

CREATE INDEX idx_events_session ON events(session_id, timestamp);
CREATE INDEX idx_events_error ON events(level, timestamp) WHERE level IN ('warn', 'error');
CREATE INDEX idx_events_type ON events(event_type, timestamp);
```

### Tracing Subscriber

A custom `tracing` subscriber writes to both:
- **Stderr** — human-readable logs for development (`tracing-subscriber` with `fmt` layer)
- **SQLite** — structured events for agent introspection (custom layer)

```rust
use tracing_subscriber::layer::SubscriberExt;

let fmt_layer = tracing_subscriber::fmt::layer()
    .with_target(false)
    .with_thread_ids(false);

let sqlite_layer = SqliteTracingLayer::new(db.clone());

let subscriber = tracing_subscriber::registry()
    .with(fmt_layer)
    .with(sqlite_layer);

tracing::subscriber::set_global_default(subscriber)?;
```

### Agent Introspection Tools

The agent gets built-in tools for self-diagnosis:

**`coop_status`** — Current gateway health:
- Channel connectivity (last message time, error count per channel)
- Active sessions (count, last activity)
- Memory store stats (observation count, index freshness)
- Recent errors (last 10)

**`coop_logs`** — Query the event log:
- `coop_logs(session_id="abc")` — all events for a session
- `coop_logs(level="error", since="1h")` — recent errors
- `coop_logs(event_type="api_error", limit=5)` — last 5 API errors
- `coop_logs(user="alice", since="today")` — all events for a user today

**`coop_trace`** — Detailed trace for a specific message/turn:
- Shows the full span tree: trust resolution → prompt build → API calls → tool executions → response
- Includes timing, token counts, and any errors at each step

### Example Interaction

```
User: "Why did my message fail last night?"

Agent: *calls coop_logs(level="error", since="12h")*

Agent: "At 2:47 AM, your Signal message hit an Anthropic API rate limit (429).
The gateway retried 3 times with exponential backoff (1s, 2s, 4s), but all
retries also got 429. The session was in a high-traffic period — 12 turns
in the previous 5 minutes. The message was queued and delivered successfully
on the next attempt at 2:48 AM."
```

### What Gets Logged (Event Types)

| Event Type | Category | When |
|------------|----------|------|
| `message_received` | channel | Inbound message from any channel |
| `message_sent` | channel | Outbound message to any channel |
| `channel_error` | channel | Channel connection failure, reconnect |
| `trust_resolved` | router | User + situation → effective trust |
| `prompt_built` | prompt | System prompt assembled, token count |
| `turn_started` | agent | Agent begins processing |
| `turn_completed` | agent | Agent finishes, with duration + tokens |
| `api_call` | provider | LLM API request, with model + tokens |
| `api_error` | provider | LLM API failure (rate limit, timeout, etc.) |
| `tool_call` | tool | MCP tool invocation, with name + duration |
| `tool_error` | tool | Tool execution failure |
| `memory_search` | memory | Memory query, with store + result count |
| `memory_write` | memory | Memory observation created |
| `session_created` | session | New session started |
| `session_compacted` | session | Context compaction triggered |
| `schedule_fired` | scheduler | Cron/heartbeat triggered |
| `config_reloaded` | config | Config file changed and reloaded |

### Retention

Events older than 30 days are pruned automatically (configurable). The agent can query anything within the retention window. For long-term analysis, events can be exported to JSON.

### Cost Tracking

Every `api_call` event includes `tokens_in`, `tokens_out`, and `cost_usd` (calculated from known model pricing). The agent can answer:

- "How much did I cost today?" → `SELECT SUM(cost_usd) FROM events WHERE event_type='api_call' AND date(timestamp)=date('now')`
- "Which session used the most tokens?" → aggregate by session_id
- "What's my average cost per turn?" → simple division

---

## External Telemetry

Coop includes zero external telemetry by default — all tracing stays local. We may add opt-in anonymous usage stats later.

---

## Open Questions

1. **Cache breakpoints.** Anthropic's prompt caching requires specific system block structure. Verify that our provider implementation supports multiple system blocks.

2. **Memory index format.** ~~What's the right level of detail for the TOC?~~ **Decided:** File name + token count + one-line description. Token counts are mandatory — the agent needs to know the cost of loading each file to make informed retrieval decisions. This is core to our token sensitivity principle.
