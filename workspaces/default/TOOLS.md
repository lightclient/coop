# Tools

## Configuration

Coop is configured via `coop.yaml`. You can read, validate, and write config changes conversationally using the config tools. The config file is automatically watched — most changes take effect within seconds without a restart.

### Config workflow

1. **Read** the current config with `config_read` to see what's set
2. **Modify** — produce the complete new YAML (config_write requires the full file, not a patch)
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

### config_read

Returns the current coop.yaml contents. No parameters.

### config_write

Validates and writes coop.yaml atomically. Backs up the previous version to `coop.yaml.bak`.

Parameters:
- `content` (string, required) — the complete YAML file contents

The content is validated before writing. If validation fails, the file is not modified and you get an error report showing what's wrong.

## coop.yaml reference

```yaml
# Required: agent identity
agent:
  id: coop                                    # agent name (restart-required)
  model: anthropic/claude-sonnet-4-20250514   # model identifier (hot-reload)
  workspace: ./workspaces/default             # path to workspace dir (restart-required)

# Users and trust levels
# Trust levels: full > inner > familiar > public
# - full: complete access, can use all tools, sees all memory
# - inner: can use bash/file tools, sees shared+social memory
# - familiar: read-only tools, sees social memory only
# - public: no tools, no memory
users:
  - name: alice
    trust: full
    match:
      - "terminal:default"          # matches channel:sender
      - "signal:alice-uuid"         # Signal sender UUID
  - name: bob
    trust: inner
    match:
      - "signal:bob-uuid"

# Channel configuration
channels:
  signal:
    db_path: ./db/signal.db         # path to signal-cli database (restart-required)

# Provider (only "anthropic" supported currently)
provider:
  name: anthropic                   # restart-required

# Memory system
memory:
  db_path: ./db/memory.db          # SQLite database path (restart-required)

  # Prompt index: injects recent relevant memories into system prompt
  prompt_index:
    enabled: true                   # default: true
    limit: 12                       # max observations to include (default: 12)
    max_tokens: 1200                # token budget for index (default: 1200)

  # Retention policy: automatic archival, compression, and deletion
  retention:
    enabled: true                   # default: true
    archive_after_days: 30          # move to archive after N days (default: 30)
    delete_archive_after_days: 365  # delete archived after N days (default: 365)
    compress_after_days: 14         # compress clusters after N days (default: 14)
    compression_min_cluster_size: 3 # min observations to compress (default: 3)
    max_rows_per_run: 200           # max rows processed per maintenance run (default: 200)

  # Embedding provider for semantic search (optional, restart-required)
  # Supported providers: openai, voyage, cohere, openai-compatible
  embedding:
    provider: voyage
    model: voyage-3-large
    dimensions: 1024
    # For openai-compatible only:
    # base_url: https://your-endpoint/v1
    # api_key_env: YOUR_API_KEY_ENV_VAR

# Scheduled tasks
cron:
  # Heartbeat: auto-delivers to all channels the user is bound to.
  # If user has signal match patterns, response goes to Signal.
  # If the agent responds with HEARTBEAT_OK (nothing to report),
  # delivery is suppressed. Empty HEARTBEAT.md skips the LLM call entirely.
  - name: heartbeat
    cron: "*/30 * * * *"            # standard cron expression
    user: alice                     # run as this user (optional, must exist in users)
    message: check HEARTBEAT.md     # message sent to the agent

  # Explicit delivery override: sends response to a specific target
  # instead of auto-resolving from user match patterns.
  - name: morning-briefing
    cron: "0 8 * * *"
    user: alice
    deliver:
      channel: signal
      target: alice-uuid            # or "group:<hex>" for group chats
    message: Morning briefing

  # No delivery, no user — silent internal work.
  - name: cleanup
    cron: "0 3 * * *"
    message: run cleanup
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
- API keys checked via environment: ANTHROPIC_API_KEY (always), plus embedding provider key if configured

## File tools

These operate relative to the workspace directory.

- `read_file` — read file contents (params: path, optional offset/limit)
- `write_file` — create or overwrite a file (requires full/inner trust)
- `edit_file` — find-and-replace in a file (requires full/inner trust)
- `bash` — execute a shell command (requires full/inner trust, 120s timeout)

## Memory tools

Structured observation storage with trust-gated access to three stores:

| Store | Min trust | Use for |
|-------|-----------|---------|
| private | full | personal facts, preferences, secrets |
| shared | inner | knowledge shared with trusted users |
| social | familiar | public-safe facts about people/topics |

- `memory_search` — search observations by text, type, people, time range, store
- `memory_get` — fetch full observation details by ID
- `memory_write` — create a new observation (store defaults based on trust level)
- `memory_timeline` — get observations around a specific observation ID
- `memory_history` — fetch mutation history for an observation
- `memory_people` — search known people across observations

## Signal tools (available when Signal channel is configured)

- `signal_react` — react to a message with an emoji
- `signal_reply` — reply to a specific message (shows as a quote)
- `signal_history` — search message history in the current Signal conversation

## Slash commands (available on all channels)

Users can send these directly:

- `/new`, `/clear`, `/reset` — clear session history
- `/stop` — cancel the current agent turn
- `/status` — show session info (model, context usage, token counts)
- `/help`, `/?` — list available commands
