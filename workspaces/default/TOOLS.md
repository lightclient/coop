# Tools

Tool names, parameters, and basic descriptions are provided via the API tool definitions.
This file covers workflow guidance, conceptual context, and reference that tool schemas can't express.

## Configuration workflow

Coop is configured via `coop.toml`. The config file is automatically watched — most changes take effect within seconds without a restart.

1. **Read** the current config with `config_read` to see what's set
2. **Modify** — produce the complete new TOML (config_write requires the full file, not a patch)
3. **Write** with `config_write` — it validates before writing, backs up the old file, and rejects invalid configs

### Hot-reloadable vs restart-required

Changes to these fields take effect immediately (hot-reload):

- `agent.model` — switch models on the fly
- `users` — add/remove users, change trust levels or match patterns
- `cron` — add/remove/edit scheduled tasks
- `memory.prompt_index` — toggle prompt index, change limits
- `memory.retention` — change retention policy

These fields require a process restart:

- `agent.id`
- `agent.workspace`
- `provider.name`
- `channels` (all channel config)
- `memory.db_path`
- `memory.embedding` (provider, model, dimensions)

If a user asks to change a restart-required field, apply it with config_write and tell them to restart coop.

## coop.toml reference

```toml
# Required: agent identity
[agent]
id = "coop"                                    # agent name (restart-required)
model = "anthropic/claude-sonnet-4-20250514"   # model identifier (hot-reload)
workspace = "./workspaces/default"             # path to workspace dir (restart-required)

# Users and trust levels
# Trust levels: full > inner > familiar > public
# - full: complete access, can use all tools, sees all memory
# - inner: can use bash/file tools, sees shared+social memory
# - familiar: read-only tools, sees social memory only
# - public: no tools, no memory

[[users]]
name = "alice"
trust = "full"
match = ["terminal:default", "signal:alice-uuid"]

[[users]]
name = "bob"
trust = "inner"
match = ["signal:bob-uuid"]

# Channel configuration
[channels.signal]
db_path = "./db/signal.db"         # path to signal-cli database (restart-required)

# Provider (only "anthropic" supported currently)
[provider]
name = "anthropic"                 # restart-required
# Multiple API keys for automatic rotation on rate limits (optional).
# Each entry is an env: reference. Keys rotate proactively at 90%
# utilization and reactively on 429 errors. Omit for single-key mode.
# api_keys = ["env:ANTHROPIC_API_KEY", "env:ANTHROPIC_API_KEY_2", "env:ANTHROPIC_API_KEY_3"]

# Memory system
[memory]
db_path = "./db/memory.db"         # SQLite database path (restart-required)

# Prompt index: injects recent relevant memories into system prompt
[memory.prompt_index]
enabled = true                     # default: true
limit = 12                         # max observations to include (default: 12)
max_tokens = 1200                  # token budget for index (default: 1200)

# Retention policy: automatic archival, compression, and deletion
[memory.retention]
enabled = true                     # default: true
archive_after_days = 30            # move to archive after N days (default: 30)
delete_archive_after_days = 365    # delete archived after N days (default: 365)
compress_after_days = 14           # compress clusters after N days (default: 14)
compression_min_cluster_size = 3   # min observations to compress (default: 3)
max_rows_per_run = 200             # max rows processed per maintenance run (default: 200)

# Embedding provider for semantic search (optional, restart-required)
# Supported providers: openai, voyage, cohere, openai-compatible
[memory.embedding]
provider = "voyage"
model = "voyage-3-large"
dimensions = 1024
# For openai-compatible only:
# base_url = "https://your-endpoint/v1"
# api_key_env = "YOUR_API_KEY_ENV_VAR"

# Scheduled tasks
# Heartbeat: auto-delivers to all channels the user is bound to.
# If user has signal match patterns, response goes to Signal.
# If the agent responds with HEARTBEAT_OK (nothing to report),
# delivery is suppressed. Empty HEARTBEAT.md skips the LLM call entirely.
[[cron]]
name = "heartbeat"
cron = "*/30 * * * *"              # standard cron expression
user = "alice"                     # run as this user (optional, must exist in users)
message = "check HEARTBEAT.md"     # message sent to the agent

# Explicit delivery override: sends response to a specific target
# instead of auto-resolving from user match patterns.
[[cron]]
name = "morning-briefing"
cron = "0 8 * * *"
user = "alice"
message = "Morning briefing"

[cron.deliver]
channel = "signal"
target = "alice-uuid"              # or "group:<hex>" for group chats

# No delivery, no user — silent internal work.
[[cron]]
name = "cleanup"
cron = "0 3 * * *"
message = "run cleanup"
```

### Validation constraints

- `agent.id` and `agent.model` must be non-empty
- `provider.name` must be `anthropic`
- Workspace directory must exist and contain at least SOUL.md
- Memory prompt_index: limit > 0, max_tokens > 0
- Memory retention: all day values > 0, compression_min_cluster_size > 1, delete_archive_after_days >= archive_after_days
- Embedding dimensions: 1..=8192
- Cron users must exist in the users list
- Cron delivery channel must be `signal`
- Cron with user but no `deliver`: warns if user has no non-terminal match patterns (heartbeat will have no delivery targets)
- API keys: if `provider.api_keys` is set, each entry must use `env:` prefix and the referenced env var must be set. Otherwise, `ANTHROPIC_API_KEY` must be set. Plus embedding provider key if configured

## Skills

Skills are reusable instruction sets discovered from `SKILL.md` files. They appear as a menu in the system prompt; use `read_file` to load one when the task matches.

### Installation paths

- **Workspace-level** — `skills/{name}/SKILL.md` (available to all users)
- **Per-user** — `users/{user}/skills/{name}/SKILL.md` (only for that user; overrides a workspace skill with the same name)

## Diagnostics

When asked why you did something, or when something went wrong, you have direct access to your own internals via bash and read_file. Check these before guessing:

- **Session transcripts** — `sessions/*.jsonl` in the workspace. Each line is a JSON message (role, content, tool calls/results). Your current session file shows exactly what messages were exchanged and what tool calls you made. Use `ls sessions/` to find them, `tail` or `jq` to inspect.
- **Trace log** — `traces.jsonl` in the working directory (present when `COOP_TRACE_FILE` is set). JSONL with spans covering `route_message → agent_turn → turn_iteration → provider_request / tool_execute`. Use `grep`, `jq`, or `rg` to search. Each line has `timestamp`, `level`, `message`, `span`, and `spans` (ancestry). Look for `"level":"ERROR"` for failures, `tool_execute` spans for tool behavior, `provider_request` for API interactions.
- **Config** — `config_read` shows the live config. Check here for model, trust levels, channel bindings, cron schedules.
- **Source code** — the crates are in `crates/` relative to the project root. When behavior is confusing, read the implementation.

## Memory stores

Memory tools operate on three trust-gated stores:

| Store | Min trust | Use for |
|-------|-----------|---------|
| private | full | personal facts, preferences, secrets |
| shared | inner | knowledge shared with trusted users |
| social | familiar | public-safe facts about people/topics |

## Slash commands

Users can send these directly on any channel:

- `/new`, `/clear`, `/reset` — clear session history
- `/stop` — cancel the current agent turn
- `/status` — show session info (model, context usage, token counts)
- `/help`, `/?` — list available commands
