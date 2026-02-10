# Coop — Personal Agent Gateway
## Design Document (WIP)

### What Is This
A personal agent gateway built in Rust. Coop owns the full stack — provider integration, tool-calling loop, channels, sessions, memory, scheduling, multi-agent orchestration. Reliability-first.

---

## Core Concepts

### Agent
The mind. A model, a personality (SOUL.md), behavioral instructions (AGENTS.md), a tool set, and MCP extensions. One agent can serve multiple users.

### User
A person who interacts with the agent. Has a trust level, contact identifiers (Signal number, Discord ID, etc.), and optionally their own workspace for per-user files. Everyone is a user — they just have different trust levels.

### Session
A conversation. Belongs to an agent, associated with a user (or not, in the case of group chats / cron). Has its own message history and context window. Sessions inherit their effective trust from the user + situation.

### Channel
A transport. Signal, Telegram, iMessage, Discord, webchat, etc. Channels receive and send messages. They don't know about agents or memory — they just move bytes.

### Memory Store
A named collection of memories at a specific classification level. The agent writes to stores; trust determines which stores are readable.

---

## Permission Model: Trust + Ceiling

Inspired by Bell-LaPadula. Two inputs determine what the agent can access:

1. **Trust** — assigned to users. "How much does the agent trust this person?"
2. **Ceiling** — assigned to situations (DM, group, public). "What's the maximum appropriate disclosure here?"

The effective trust is always `min(user.trust, situation.ceiling)`. A situation can only lower access, never raise it.

### Trust Levels (ordered, each includes everything below)

```
full      → [private, shared, social]    # complete access
inner     → [shared, social]             # no private (finances, health, credentials)
familiar  → [social]                     # public-safe facts only
public    → []                           # no memory access
```

### Resolution Examples

| User | Situation | Effective | Memory Access |
|------|-----------|-----------|---------------|
| Alice (full) | DM | full | private, shared, social |
| Alice (full) | Group chat | familiar | social |
| Bob (inner) | DM | inner | shared, social |
| Bob (inner) | Group chat | familiar | social |
| Carol (familiar) | DM | familiar | social |
| Unknown person | DM | public | none |
| Cron job | System | full | private, shared, social |

### Tool Permissions

Same model extends to tools. Each trust level defines allowed tools:

```
full:      all tools
inner:     all except [gateway, cron]
familiar:  all except [gateway, cron, exec]
public:    [read, web_search, web_fetch]
```

Effective tool access = `tools_for(min(user.trust, situation.ceiling))`

---

## Configuration

```toml
# coop.toml

agent:
  id: reid
  model: anthropic/claude-opus-4-5
  personality: ./soul.md
  instructions: ./agents.md

  # Memory stores
  memory:
    stores:
      private:
        path: ./memory/private
        # finances, health, credentials, emotions
      shared:
        path: ./memory/shared
        # RECENT.md, household context, cross-user
      social:
        path: ./memory/social
        # people, interests, public facts

  # Trust levels — ordered, each includes all below
  trust:
    full:
      memory: [private, shared, social]
      tools: all
    inner:
      memory: [shared, social]
      tools: all except [gateway, cron]
    familiar:
      memory: [social]
      tools: all except [gateway, cron, exec]
    public:
      memory: []
      tools: [read, web_search, web_fetch]

  # Users and their trust level
  users:
    - name: alice
      trust: full
      match: [signal:+15555550100, webchat:alice]
      workspace: ./workspaces/alice

    - name: bob
      trust: inner
      match: [signal:+15555550101]
      workspace: ./workspaces/bob

    - name: carol
      trust: familiar
      match: [imessage:carol@example.com]

    # Unknown users default to trust: public

  # Situation ceilings
  situations:
    dm:
      ceiling: full           # DMs allow full trust
    group:
      ceiling: familiar       # groups cap at social by default
    public:
      ceiling: public         # webhooks, unknown contacts
    system:
      ceiling: full           # cron, heartbeats

  # Override ceiling for specific groups
  groups:
    - match: signal:group:neighborhood-id
      ceiling: familiar
    - match: signal:group:family-id
      ceiling: inner          # family group gets more access

  # Schedules
  schedules:
    - name: heartbeat
      cron: "*/30 * * * *"
      situation: system
      message: "check HEARTBEAT.md"
    - name: morning-briefing
      cron: "0 8 * * *"
      situation: system
      message: "morning briefing"

# Multiple agents supported
# agent:
#   - id: reid
#     ...
#   - id: codebot
#     ...
```

---

## Architecture

```
┌─────────────────────────────────────────────────────────────┐
│                      Coop Gateway Daemon                     │
│                    (tokio async, single binary)               │
│                                                               │
│  ┌──────────┐  ┌──────────┐  ┌───────────┐  ┌────────────┐ │
│  │ Channels  │  │ Sessions │  │ Scheduler │  │   Config    │ │
│  │          │  │ Manager  │  │ (cron/hb) │  │  + Reload   │ │
│  └────┬─────┘  └────┬─────┘  └─────┬─────┘  └──────┬──────┘ │
│       │              │              │               │         │
│       └──────────┬───┴──────────────┴───────────────┘         │
│                  │                                             │
│          ┌───────▼────────┐                                   │
│          │  Message Router │                                   │
│          │                 │                                   │
│          │  1. Identify user (who)                            │
│          │  2. Identify situation (where)                     │
│          │  3. Resolve trust: min(who, where)                 │
│          │  4. Route to agent session                         │
│          └───────┬────────┘                                   │
│                  │                                             │
│       ┌──────────▼──────────┐                                 │
│       │    Agent Pool        │                                │
│       │   (provider pool)    │                                │
│       └──────────┬──────────┘                                 │
│                  │                                             │
│          ┌───────▼────────┐                                   │
│          │  Memory Layer   │                                   │
│          │  (vector index   │                                  │
│          │   per store)     │                                  │
│          └────────────────┘                                   │
└─────────────────────────────────────────────────────────────┘
```

### Channel System
- Trait-based: each channel implements `listen()` + `send()`
- Auto-reconnect with exponential backoff
- Per-channel rate limiting
- Media pipeline (voice → transcription, image → description)
- Health monitoring and probes

### Session Manager
- SQLite persistence (WAL mode)
- Crash recovery — resume in-flight sessions on restart
- Configurable retention (messages, days, tokens)
- Cross-session messaging (agent-to-agent or session-to-session)
- Sub-agent spawning into isolated sessions

### Agent Pool
- Uses `Provider` trait for LLM calls (see `crates/coop-core/src/traits.rs`)
- Coop owns the agent loop: tool call → execute → loop, compaction, retry
- MCP extensions via `rmcp` crate directly
- Built-in tools (exec, fs, memory, messaging, browser, http) as Coop-native tool executors
- Per-session trust enforcement on tool calls and memory access

### Memory Layer
- Separate vector index per store
- Embeddings via API (OpenAI) or local (fastembed-rs)
- Search scoped to stores accessible at current trust level
- Progressive disclosure: prompt gets index/TOC, agent fetches details via tools

### Prompt Builder
- Layer 0 (always): Agent personality + instructions (~1-2k tokens)
- Layer 1 (always): Memory index/TOC with token cost annotations (~500 tokens)
- Layer 2 (always): User context — who they are, relationship (~200 tokens)
- Layer 3 (on-demand): Full memory content via memory_search/memory_get tools

### Config System
- TOML config with JSON Schema validation (generated from Rust structs)
- Hot reload via file watcher
- Patch merges arrays by key (not replace!)
- Secrets in system keyring, never in config file

---

## Reliability Improvements Over OpenClaw

| Pain Point | OpenClaw | Coop |
|---|---|---|
| Module cache on restart | SIGUSR1 doesn't reload JS | Single binary, no cache |
| Config patch wipes arrays | Silent data loss | Merge-by-key with validation |
| Channel reconnect | Fragile, manual restart | Auto-reconnect + backoff |
| Session state loss | In-memory only | SQLite WAL persistence |
| Vendor code patches | npm update overwrites fixes | Single binary, no vendor code |
| Sandbox hanging | OrbStack/Docker dependency | Direct process spawn or Wasmtime |
| Crash recovery | Start from scratch | Resume in-flight sessions |
| Error visibility | Swallowed errors | Structured tracing throughout |

---

## Build Phases

### Phase 1: Core Loop
- Gateway daemon with graceful shutdown
- Single agent with Anthropic provider
- Webchat channel (HTTP/WebSocket)
- File-based memory (read/write/search)
- Config loading + validation
- Trust resolution

### Phase 2: Channels + Routing
- Signal channel
- Telegram channel
- Message routing with trust resolution
- Media handling (voice transcription)
- User identification

### Phase 3: Persistence + Reliability
- SQLite session storage
- Crash recovery
- Cron/heartbeat scheduler
- Config hot reload

### Phase 4: Multi-Agent + Multi-User
- Agent pool with per-agent config
- Multiple users with workspaces
- Cross-session messaging
- Sub-agent spawning

### Phase 5: Extensions
- iMessage channel
- Discord channel
- Browser automation (CDP)
- Node/device control
- Progressive disclosure prompt builder

---

## Key Dependencies (Rust)

```toml
tokio         # async runtime
axum          # HTTP framework
sqlx          # SQLite (async)
serde         # serialization
dashmap       # concurrent maps
tracing       # structured logging
clap          # CLI
notify        # file watcher
cron          # cron parsing
reqwest       # HTTP client
qdrant/lance  # vector search
fastembed     # local embeddings (optional)

```

---

## Subagent Strategy

Coop can spawn subagents as child sessions with their own tool sets, trust levels, and (optionally) different models. Subagents run within the gateway's agent loop and return a summary to the parent.

**Known limitation: opacity.** A subagent running for 5 minutes looks like a long tool call from the gateway's perspective — no turn counts, no intermediate status, no per-subagent cost breakdown. This is acceptable for now. Tracing spans provide some visibility.

**Future options:**
- Surface subagent events on the parent's event stream
- Per-subagent cost breakdown and progress reporting

## Open Questions

- Sandboxing strategy: Wasmtime? Direct process with seccomp? Or keep Docker as an option?
- Should channels be compiled-in or dynamically loadable (shared libs / WASM plugins)?
- Multi-agent: do agents share a process or run as separate processes coordinated by the gateway?
- How to handle workspace files that the agent edits (MEMORY.md, daily notes) — file locking across sessions?
