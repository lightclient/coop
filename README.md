```
        ████████████████
        ████████████████
    ████▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓████
    ████▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓████
    ████▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓████
    ████▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓████
    ████▓▓▓▓████▓▓▓▓████▓▓▓▓████
    ████▓▓▓▓████▓▓▓▓████▓▓▓▓████
    ████▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓████
    ████▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓████
        ████████████████
        ████████████████
```

# Coop

A personal agent gateway in Rust. Coop routes messages between channels (Signal, Telegram, Discord, terminal, webhooks) and AI agent sessions running on your machine. It enforces trust-based access control, persists conversations, and manages agent lifecycles.

- ~~Phase 1 — gateway + terminal TUI.~~
- ~~Phase 2 - separate gateway + telemetry = dogfooding, tight LLM loop~~
- **Phase 3 - chat channel integration**
- Phase 4 - user and permissions

## Quick start

Coop needs `ANTHROPIC_API_KEY` in your environment. You can use a standard API key from [console.anthropic.com](https://console.anthropic.com/), or reuse your Claude Code OAuth token:

```bash
export ANTHROPIC_API_KEY=$(jq -r '.claudeAiOauth.accessToken' ~/.claude/.credentials.json)
```

```bash
# terminal 1
cargo run --bin coop -- start

# terminal 2
cargo run --bin coop -- chat
```

## Memory

Coop has a built-in structured memory system backed by SQLite. The agent can search, write, and browse observations across trust-gated stores. Memory works out of the box with zero config — add a `memory:` section to your `coop.yaml` to customise it.

### Minimal setup (works immediately)

Memory is enabled by default. With no `memory:` section in your config, Coop stores observations in `./db/memory.db`, injects a compact memory index into each prompt, and runs periodic maintenance. The agent gets six tools: `memory_search`, `memory_write`, `memory_get`, `memory_timeline`, `memory_history`, `memory_people`.

### Full config reference

```yaml
memory:
  # Path to the SQLite database (relative to config dir or absolute).
  # Default: ./db/memory.db
  db_path: ./db/memory.db

  # Prompt index: injects a compact summary of recent observations into the
  # system prompt before each turn so the agent has context without searching.
  prompt_index:
    enabled: true       # default: true
    limit: 12           # max observations to include (default: 12)
    max_tokens: 1200    # token budget for the index block (default: 1200)

  # Retention: automatic compression, archiving, and cleanup of old observations.
  # Runs once at startup and periodically in the background.
  retention:
    enabled: true                    # default: true
    compress_after_days: 14          # cluster & merge stale observations (default: 14)
    compression_min_cluster_size: 3  # minimum cluster size to trigger compression (default: 3)
    archive_after_days: 30           # move expired observations to archive table (default: 30)
    delete_archive_after_days: 365   # permanently delete old archive rows (default: 365)
    max_rows_per_run: 200            # bound each maintenance stage (default: 200)

  # Embedding: optional semantic vector search. Without this, retrieval is
  # FTS-only (full-text search), which works well for most use cases.
  embedding:
    provider: openai               # openai | voyage | cohere | openai-compatible
    model: text-embedding-3-small
    dimensions: 1536
```

### Memory stores and trust

Observations live in one of three stores, gated by the user's trust level:

| Store | Who can access | Use for |
|-------|---------------|---------|
| `private` | Full trust only | Personal credentials, private notes, secrets |
| `shared` | Full + Inner trust | Project context, technical decisions, shared state |
| `social` | Full + Inner + Familiar trust | Public-facing info, meeting notes, social context |

Public-trust users have no memory access at all. The prompt index follows the same gates — a Familiar-trust user only sees `social` observations in their prompt.

### Memory tools

The agent gets these tools automatically when memory is configured:

| Tool | Description |
|------|-------------|
| `memory_search` | Full-text (and optionally vector) search across observations |
| `memory_write` | Create or reconcile a structured observation |
| `memory_get` | Fetch full observation details by ID |
| `memory_timeline` | Browse observations around a specific ID |
| `memory_history` | View mutation history (ADD/UPDATE/DELETE/COMPRESS) for an observation |
| `memory_people` | Search known people mentioned in observations |

### Reconciliation

When the agent writes an observation that overlaps with existing data, Coop automatically reconciles using the LLM:

- **Exact duplicate** — bumps mention count, no new row
- **Similar existing** — LLM decides: `ADD` (new), `UPDATE` (merge), `DELETE` (replace stale), or `NONE` (skip)
- **No match** — inserts as new observation

All mutations are recorded in `observation_history` for auditability.

### Embedding providers

Embeddings are optional. Without them, search uses FTS5 (SQLite full-text search). Adding an embedding provider enables hybrid retrieval (FTS + vector similarity + recency ranking).

**OpenAI** (default):
```yaml
embedding:
  provider: openai
  model: text-embedding-3-small
  dimensions: 1536
```
Requires `OPENAI_API_KEY` in your environment.

**Voyage AI**:
```yaml
embedding:
  provider: voyage
  model: voyage-3-lite
  dimensions: 512
```
Requires `VOYAGE_API_KEY`.

**Cohere**:
```yaml
embedding:
  provider: cohere
  model: embed-english-v3.0
  dimensions: 1024
```
Requires `COHERE_API_KEY`.

**OpenAI-compatible** (any endpoint that speaks the OpenAI embeddings API):
```yaml
embedding:
  provider: openai-compatible
  model: text-embedding-3-small
  dimensions: 1536
  base_url: https://your-endpoint.example/v1
  api_key_env: YOUR_CUSTOM_KEY_ENV
```
Requires the env var named in `api_key_env`.

### Validating your config

```bash
cargo run --bin coop -- check
```

This validates all memory settings: db path, prompt index limits, retention constraints, embedding provider/model/dimensions, and required API key env vars.

### Debugging with traces

```bash
COOP_TRACE_FILE=traces.jsonl cargo run --bin coop -- start
```

Memory emits structured trace events for: embedding requests/responses, reconciliation decisions, prompt index build/injection, and maintenance stages. Search the JSONL file with `grep` or `jq`. See [Memory Design](docs/memory-design.md) for the full trace event catalogue.

## Architecture

Five workspace crates:

| Crate | Purpose |
|-------|---------|
| `coop-core` | Domain types, trait boundaries, prompt builder, test fakes |
| `coop-agent` | LLM provider integration (Anthropic API) |
| `coop-memory` | Structured memory store (SQLite observations + retrieval) |
| `coop-gateway` | CLI entry point, daemon lifecycle, gateway routing, config |
| `coop-ipc` | Unix socket IPC protocol and client/server transport |
| `coop-channels` | Channel adapters (terminal; Signal scaffolded) |
| `coop-tui` | Terminal UI (crossterm) |

## Workspace

Agent personality and context live in workspace files (default: `./workspaces/default/`):

| File | Purpose | Trust |
|------|---------|-------|
| `SOUL.md` | Agent personality and voice | familiar |
| `AGENTS.md` | Behavioral instructions | familiar |
| `TOOLS.md` | Tool usage notes | familiar |
| `IDENTITY.md` | Agent identity | familiar |
| `USER.md` | Per-user info | inner |
| `MEMORY.md` | Long-term curated memory | full |
| `HEARTBEAT.md` | Periodic check tasks | full |

All files are optional. Trust level controls which files are visible in a given session — see [System Prompt Design](docs/system-prompt-design.md).

### Channel prompts

Coop injects channel-specific formatting instructions into the system prompt so the agent adapts its output to each channel's capabilities. For example, Signal messages use plain text (no markdown), while terminal sessions get rich formatting.

Built-in defaults are provided for known channels:

| Channel | Default behavior |
|---------|-----------------|
| `signal` | Plain text only — no markdown, asterisks, backticks, code fences, or bullet markers |
| `terminal` | No restrictions (supports rich formatting) |

To override the built-in or add instructions for a new channel, create a file in the workspace:

```
workspaces/default/channels/signal.md     # override Signal default
workspaces/default/channels/discord.md    # add Discord-specific instructions
```

The file content replaces the built-in default entirely. The channel name is the part before the first colon in the channel identifier (`terminal:default` → `terminal`, `signal` → `signal`).

## Development

```bash
just check    # fmt, toml, lint, deny, test
just fmt      # auto-format
just lint     # clippy
just test     # cargo test --all
just build    # release build
```

## Docs

- [Architecture](docs/architecture.md) — core concepts and high-level design
- [Design](docs/design.md) — full design document with config, trust model, and build phases
- [Phase 1 Plan](docs/phase1-plan.md) — gateway + terminal TUI (current milestone)
- [Testing Strategy](docs/testing-strategy.md) — trait boundaries, fakes, fixture-driven testing
- [Memory Design](docs/memory-design.md) — structured observations, SQLite + FTS5, progressive disclosure

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or [MIT License](LICENSE-MIT) at your option.
