# Design Principles

These are non-negotiable. Every PR, every design decision, every refactor should be evaluated against them.

---

## 1. Simplicity Over Features

Coop must be small enough to reason about. If you can't hold the entire architecture in your head, it's too complex.

**Targets:**
- Core gateway: under 5,000 lines of Rust
- Under 10 source files in the hot path (message in → response out)
- A new contributor should understand the full request lifecycle in under 30 minutes
- Zero abstraction layers that don't earn their keep. If a trait has one implementor, it's a function.

**What this means in practice:**
- No enterprise patterns (factory factories, abstract strategy providers, dependency injection frameworks)
- No "framework" ambitions. Coop is a product, not a library for building products.
- Prefer `match` over `dyn Trait` when there are < 5 variants
- Prefer flat modules over deep nesting. `gateway.rs` not `gateway/core/handler/mod.rs`
- Copy-paste is fine if the alternative is a premature abstraction
- Delete code aggressively. If it's not used, it's not needed.

**Measuring it:**
- `tokei crates/` should stay under 10k lines for Coop-owned code
- `find crates/ -name "*.rs" | wc -l` should stay under 30 files
- Anyone should be able to run `cat crates/coop-gateway/src/main.rs` and understand how the program starts

**The NanoClaw test:** Could someone understand this in 8 minutes? If not, simplify.

---

## 2. Robustness: Never Go Down

The agent must survive configuration errors, bad deploys, crashed providers, and its own mistakes. The worst outcome is an agent that's unreachable — a silent failure that the user discovers hours later when they check their phone and see no response.

### Config Safety

Config changes are the #1 cause of OpenClaw outages. Coop treats config as a deployment:

#### Validate Before Apply

Every config change goes through full validation before touching the running system:

```
User edits coop.toml
         │
         ▼
    ┌──────────┐
    │ Parse    │ ← TOML syntax valid?
    │ Validate │ ← Schema valid? Required fields present?
    │ Verify   │ ← Can we connect to the provider? Do workspace files exist?
    │          │   Are channel credentials valid? Do trust levels parse?
    └────┬─────┘
         │
    ┌────▼─────┐
    │ Dry run  │ ← Build the prompt. Create providers. Don't start serving.
    └────┬─────┘
         │ all checks pass
         ▼
    ┌──────────┐
    │ Snapshot  │ ← Save current working config as .coop.toml.bak
    │ Apply    │ ← Hot-swap the live config
    └────┬─────┘
         │
    ┌────▼─────┐
    │ Health   │ ← Can the agent respond? Can channels connect?
    │ Check    │   If not → automatic rollback to .coop.toml.bak
    └──────────┘
```

#### Automatic Rollback

If the new config passes validation but the agent fails to respond within a health check window (default: 30 seconds), Coop automatically reverts to the previous config and logs exactly what happened.

#### Config Changelog

Every config change is recorded in an append-only log:

```sql
CREATE TABLE config_history (
    id          INTEGER PRIMARY KEY,
    timestamp   TEXT NOT NULL,
    config      TEXT NOT NULL,        -- full TOML snapshot
    source      TEXT NOT NULL,        -- "user_edit", "hot_reload", "rollback"
    valid       BOOLEAN NOT NULL,     -- did validation pass?
    applied     BOOLEAN NOT NULL,     -- was it actually applied?
    rolled_back BOOLEAN DEFAULT FALSE,
    error       TEXT,                 -- validation/apply error if any
    diff        TEXT                  -- human-readable diff from previous
);
```

The agent can query this: "What config changes happened?" → reads `config_history` table.

#### Agent Awareness

When a config change happens (success or rollback), the agent's next session gets a system notification:

```
[system] Config reloaded at 2026-02-02 09:15:00.
Changes: model changed from claude-sonnet to claude-opus.
```

Or on failure:

```
[system] Config change attempted at 2026-02-02 09:15:00 but ROLLED BACK.
Reason: Provider connection failed — Anthropic API returned 401 (invalid key).
Reverted to previous working config from 2026-02-02 08:00:00.
The user's intended change was: api_key updated in provider section.
```

The agent sees this as a system message in its session and can proactively tell the user what happened.

### Crash Recovery

- **Sessions persist to SQLite (WAL mode).** A crash mid-turn loses at most the in-flight response, not the conversation history.
- **Channels auto-reconnect** with exponential backoff. No manual restart needed.
- **Startup health check.** On boot, Coop verifies: config loads, provider responds, channels connect. If any fail, it starts in degraded mode (serves what it can) and logs the specific failures.
- **Watchdog.** A background task pings the agent every N minutes. If the agent stops responding (provider down, OOM, deadlock), the watchdog logs the failure and attempts recovery (restart the agent runtime, switch to fallback provider, etc.).

### Array Merge, Not Replace

OpenClaw's `config.patch` replaces arrays, silently deleting data. Coop merges arrays by key:

```toml
# Existing config:
users:
  - name: alice
    trust: full
  - name: bob
    trust: inner

# Patch:
users:
  - name: carol
    trust: familiar

# Result (merge by "name" key):
users:
  - name: alice
    trust: full
  - name: bob
    trust: inner
  - name: carol
    trust: familiar
```

To remove a user, use explicit `_remove: true` marker. No silent data loss.

---

## 3. Observable: The Agent Knows What Happened

Covered in detail in `system-prompt-design.md`, but the principle bears repeating: **the agent should be able to diagnose its own failures.**

If a user asks "why didn't you respond last night?" the agent should be able to:
1. Query the event log for errors in that time window
2. Read the trace for the failed message
3. Check the config history for any changes
4. Present a clear explanation with the root cause

This requires:
- Structured tracing to SQLite (not just stderr logs)
- Agent introspection tools (`coop_status`, `coop_logs`, `coop_trace`)
- Config change notifications injected into sessions
- Cost/token tracking per API call

---

## 4. Reliability Budget

Not every component needs the same reliability. Prioritize:

| Component | Reliability | Rationale |
|-----------|-------------|-----------|
| Message persistence | Critical | Lost messages = broken trust |
| Config safety | Critical | Bad config = total outage |
| Channel connectivity | High | Reconnect automatically, degrade gracefully |
| Agent response | High | Retry on provider errors, fall back if needed |
| Memory search | Medium | Degraded recall is acceptable; no recall is not |
| Scheduling | Medium | Missed heartbeat is fine; missed time-sensitive alert is not |
| Cost tracking | Low | Nice to have, not critical path |

---

## 5. Token Sensitivity: Every Token Earns Its Place

Context windows are finite and expensive. Coop treats token budget the way embedded systems treat memory — every allocation is deliberate, every byte justified.

**The principle:** The agent should always know the cost of bringing something into context, and the gateway should make that cost visible at every decision point.

### Progressive Disclosure

Nothing enters the context window by default. The prompt gives the agent a **menu with prices**, and the agent chooses what to fetch:

```
## Available Context
- memory/MEMORY.md        (3,847 tok) — long-term facts, people, dates
- memory/RECENT.md        (1,102 tok) — rolling 7-day context
- memory/PEOPLE.md          (814 tok) — public-safe people info
- workspace/TOOLS.md        (422 tok) — tool setup notes
- workspace/HEARTBEAT.md     (38 tok) — periodic check tasks

Use memory_get to load what you need. Budget: ~120k remaining.
```

The agent sees token counts before deciding to load anything. A birthday lookup doesn't need to pull 4k tokens of MEMORY.md — it can search first, then fetch just the relevant lines.

### Token Accounting

Every operation that touches the context window reports its token cost:

- **Prompt builder:** logs per-layer token counts (`identity: 850 tok, behavior: 1200 tok, runtime: 340 tok`)
- **memory_search results:** include estimated tokens per result (`3 results, ~280 tok total`)
- **memory_get:** returns content with actual token count in metadata
- **Tool responses:** include token cost of the response payload
- **Turn summary:** total input tokens, cached tokens, output tokens, cost

This data feeds into the observability layer (event log) and is available to the agent via `coop_status`.

### Context Window Management

- **Token budget:** Each turn starts with a known budget (model max minus system prompt minus reserved output). The gateway tracks consumed budget across tool calls within a turn.
- **Truncation with annotation:** When content must be truncated, include a marker with what was cut and how to get the rest: `[truncated at 2000/8400 tokens — use memory_get(path, offset=80) for remainder]`
- **Compaction signals:** When a session approaches 80% of the context window, inject a system note suggesting the agent summarize or compact. At 90%, force compaction (summarize older messages, keep recent).

### What This Means for Code

- Every function that produces prompt content returns `(String, usize)` — the content and its token count.
- Token counting uses `tiktoken-rs` (cl100k_base for Anthropic models), not character-based estimation.
- The prompt builder has a hard token ceiling per layer, with overflow going to "available via tool" instead.
- Memory search results are ranked by relevance but also annotated with token cost, so the agent can make informed retrieval decisions.

### Anti-Pattern: The Context Dump

OpenClaw loads MEMORY.md (often 5-10k tokens), AGENTS.md, TOOLS.md, IDENTITY.md, USER.md, and more into every single prompt — even for a simple "what time is it?" question. This is the equivalent of `SELECT *` on every query. Coop never does this. The base prompt is lean, and everything else is on-demand.

---

## 6. Identity Resolution: One Person, Many Handles

When integrating multiple channels (Signal, WhatsApp, iMessage, Discord, etc.), the agent **must** reliably recognize the same person across all of them. Failure here is catastrophic — it breaks the illusion of a single cohesive agent and makes the agent appear confused or forgetful.

**The problem:** A single human might appear as:
- `+1-555-555-0100` (E.164 phone)
- `555.555.0100` (formatted phone)
- `alice@example.com` (email)
- `@alice-dev` (GitHub/Twitter handle)
- `U03ABC123XYZ` (Slack user ID)
- `123456789012345678` (Discord snowflake)
- `alice.example.55` (WhatsApp ID)
- `Alice` (display name)
- A UUID from the gateway's internal contact registry

If the agent sees `+15555550100` on Signal and `alice.example.55` on WhatsApp and doesn't recognize these as the same person, it will:
- Greet someone like a stranger when they've been talking for months
- Fail to recall context from previous conversations on other channels
- Potentially leak information ("you said X" to the wrong identity)
- Generally appear broken

### Requirements

1. **Canonical identity layer.** Every contact has a single internal UUID. All channel-specific identifiers map to this UUID. The agent always sees the canonical identity, never raw channel IDs.

2. **Robust phone normalization.** Phones are the primary cross-channel identifier. Must handle:
   - All E.164 formats (`+13035551234`, `13035551234`)
   - Regional formatting (`303-555-1234`, `(303) 555-1234`, `303.555.1234`)
   - Country code variations (`+1`, `1`, or omitted for US)
   - WhatsApp-specific formats (`13035551234@s.whatsapp.net`)
   - Leading zeros, spaces, dashes, parentheses, dots

3. **Alias table.** One person can have multiple identifiers. The identity layer maintains:
   ```sql
   CREATE TABLE contact_aliases (
       contact_id  UUID NOT NULL REFERENCES contacts(id),
       channel     TEXT NOT NULL,  -- "signal", "whatsapp", "discord", etc.
       identifier  TEXT NOT NULL,  -- channel-specific ID
       normalized  TEXT NOT NULL,  -- canonical form (e.g., E.164 for phones)
       PRIMARY KEY (channel, identifier)
   );
   ```

4. **Fuzzy matching with confirmation.** When a new identifier appears that *might* match an existing contact (e.g., same phone, different format), the system should:
   - Auto-link if confidence is high (exact phone match after normalization)
   - Flag for user confirmation if ambiguous (same first name, different number)
   - Never silently create duplicate contacts for the same person

5. **Agent-visible identity.** The agent's prompt should show the canonical name and note which channel the message came from:
   ```
   [Alice via Signal] hey what's the wifi password?
   ```
   Not:
   ```
   [+15555550100] hey what's the wifi password?
   ```

6. **Identity in memory.** Memory entries should reference canonical contact IDs, not raw channel identifiers. When the agent writes "Alice mentioned X", it should be retrievable regardless of which channel Alice uses next.

### What This Means for Code

- **Phone parsing is not optional.** Use `phonenumber` crate with full validation. Test against a battery of real-world formats.
- **Identity resolution happens at the channel adapter layer.** By the time a message reaches the router, it should have a resolved `contact_id`, not a raw channel ID.
- **Display names are hints, not identifiers.** Display names can change, collide, or be spoofed. Never use them as primary keys.
- **Test with multi-channel scenarios.** Integration tests should verify that sending a message from Signal, then WhatsApp, then iMessage, all resolve to the same conversation with the same person.

### Anti-Pattern: Channel-Scoped Identity

OpenClaw currently routes by channel-specific identifiers. If you message from Signal, you're a Signal session. From WhatsApp, you're a different WhatsApp session. The memory search might find the Signal history, but it's fragile and the session context is split. Coop should have one session per *person*, with messages from all channels feeding into it.

---

## Anti-Patterns (Things We Will Not Do)

- **Config sprawl.** One config file. One format. No env var overrides that shadow file config. No "merge 5 sources" priority chains.
- **Plugin system.** No dynamic loading, no plugin registries, no hook chains. If you want different behavior, modify the code. The codebase is small enough that this is safe.
- **Module cache / hot reload of code.** Single binary. Restart is fast. No JS module cache bugs, no stale code paths.
- **Implicit behavior.** Every decision the gateway makes should be traceable to a config value or a default. No "magic" that requires reading the source to understand.
- **Swallowed errors.** Every error gets logged with context. `unwrap()` only in places where failure is truly impossible. `expect("reason")` everywhere else.

---

## Summary

```
Simple          → small codebase, flat structure, no premature abstraction
Robust          → validate-before-apply, auto-rollback, crash recovery
Observable      → the agent can diagnose itself
Reliable        → prioritize message persistence and config safety above all
Token-sensitive → every context token is deliberate, costs are visible
Identity-aware  → one person across all channels, never confused about who you're talking to
```

If a feature conflicts with these principles, the feature loses.
