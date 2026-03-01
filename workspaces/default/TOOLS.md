# Tools

Tool names, parameters, and basic descriptions are provided via the API tool definitions.
This file covers workflow guidance, conceptual context, and reference that tool schemas can't express.

## Configuration workflow

Coop is configured via `coop.toml`. The config file is automatically watched — most changes take effect within seconds without a restart.

1. **Read** the current config with `config_read` to see what's set
2. **Modify** — produce the complete new TOML (config_write requires the full file, not a patch)
3. **Write** with `config_write` — it validates before writing, backs up the old file, and rejects invalid configs

### Security restrictions on config_write

Only the **Owner** can modify security-sensitive configuration. Non-owner callers (Full, Inner, etc.) are blocked from changing:

- **Users** — cannot add, remove, or modify any user (trust level, match rules, sandbox overrides)
- **Sandbox config** — cannot change `[sandbox]` settings (enabled, allow_network, memory, pids_limit)
- **Per-cron sandbox overrides** — cannot modify sandbox overrides on cron entries
- **Prompt files** — cannot change `[prompt]` configuration (which files are loaded into the system prompt)

Non-owner callers can still change non-security config: model, cron messages/schedules, memory settings, etc.

### Hot-reloadable vs restart-required

Changes to these fields take effect immediately (hot-reload):

- `agent.model` — switch models on the fly
- `users` — add/remove users, change trust levels or match patterns
- `cron` — add/remove/edit scheduled tasks
- `memory.prompt_index` — toggle prompt index, change limits
- `memory.auto_capture` — toggle post-turn extraction and message threshold
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
# Trust levels: owner > full > inner > familiar > public
# - owner: bypasses sandbox, full host access (the person running coop)
# - full: complete tool access, sandboxed bash, sees all memory
# - inner: can use bash/file tools (sandboxed), sees shared+social memory
# - familiar: read-only tools, sees social memory only
# - public: no tools, no memory

[[users]]
name = "alice"
trust = "owner"
match = ["terminal:default", "signal:alice-uuid"]

[[users]]
name = "bob"
trust = "full"
match = ["signal:bob-uuid"]

# Per-user sandbox overrides (optional, only when sandbox is enabled)
# [[users]]
# name = "carol"
# trust = "inner"
# match = ["signal:carol-uuid"]
# sandbox = { allow_network = true, memory = "4g", pids_limit = 1024 }

# Group chat configuration
# Each [[groups]] entry opts a Signal group into agent responses.
# Without a matching entry, group messages are silently ignored.
#
# Trigger modes:
#   "always"  — respond to every message (agent replies NO_REPLY to stay silent)
#   "mention" — respond when mention_names appear in the message (default)
#   "regex"   — respond when trigger_regex matches
#   "llm"     — a cheap model decides whether to respond
#
# Trust: unknown senders get default_trust. Known users keep their
# configured trust, optionally capped by trust_ceiling.
#
# History: non-triggering messages are buffered (up to history_limit)
# and prepended as context when a trigger fires.

[[groups]]
match = ["signal:group:<hex-group-id>"]  # from signal traces
trigger = "mention"                       # default trigger mode
mention_names = ["coop", "hey coop"]      # case-insensitive
default_trust = "familiar"                # trust for unknown senders
# trust_ceiling = { fixed = "familiar" }  # cap all members (optional)
# trust_ceiling = "min_member"            # cap to lowest-trust member
# history_limit = 50                      # buffered messages (default: 50)

# LLM trigger example (uses a cheap model to pre-screen messages):
# [[groups]]
# match = ["signal:group:<hex-group-id>"]
# trigger = "llm"
# trigger_model = "claude-haiku-3-5-20241022"  # default trigger model
# default_trust = "familiar"

# Sandbox configuration
# When enabled, non-owner bash commands run in isolated namespaces.
# Owner trust bypasses the sandbox entirely.
[sandbox]
enabled = false                    # default: false
allow_network = false              # allow sandboxed network access (default: false)
memory = "2g"                      # memory limit per command (default: 2g)
pids_limit = 512                   # max PIDs per command (default: 512)

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

# Prompt file configuration
# Controls which workspace files are loaded into the system prompt.
# Paths are relative to workspace. Must not contain '..' or be absolute.
#
# Some files (TOOLS.md, AGENTS.md) have built-in defaults compiled into the
# binary. These defaults are always included in the prompt. The user's
# workspace file appends to (extends) the defaults. To fully replace the
# defaults, put <!-- override --> on the first line of the workspace file.
# If the workspace file is deleted, only the built-in defaults are used.
#
# [prompt]
# shared_files = [
#   { path = "SOUL.md", trust = "familiar", cache = "stable" },
#   { path = "TOOLS.md", trust = "full", cache = "session" },
# ]
# user_files = [
#   { path = "AGENTS.md", trust = "full", cache = "stable" },
#   { path = "USER.md", trust = "inner", cache = "session" },
# ]

# Memory system
[memory]
db_path = "./db/memory.db"         # SQLite database path (restart-required)

# Prompt index: injects recent + relevant memories into system prompt
[memory.prompt_index]
enabled = true                     # default: true
limit = 30                         # max observations to include (default: 30)
max_tokens = 3000                  # token budget for index (default: 3000)
recent_days = 3                    # always include observations from last N days

# Auto-capture: post-turn observation extraction
[memory.auto_capture]
enabled = true                     # default: true
min_turn_messages = 4              # skip trivial turns with too few new messages

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

# Per-cron sandbox overrides (optional)
# [[cron]]
# name = "fetch-data"
# cron = "0 * * * *"
# message = "fetch data"
# sandbox = { allow_network = true }
```

### Validation constraints

- `agent.id` and `agent.model` must be non-empty
- `provider.name` must be `anthropic`
- Workspace directory must exist and contain at least SOUL.md
- Memory prompt_index: limit > 0, max_tokens > 0, recent_days in 1..=30
- Memory auto_capture: min_turn_messages >= 1
- Memory retention: all day values > 0, compression_min_cluster_size > 1, delete_archive_after_days >= archive_after_days
- Embedding dimensions: 1..=8192
- Cron users must exist in the users list
- Cron delivery channel must be `signal`
- Cron with user but no `deliver`: warns if user has no non-terminal match patterns (heartbeat will have no delivery targets)
- API keys: if `provider.api_keys` is set, each entry must use `env:` prefix and the referenced env var must be set. Otherwise, `ANTHROPIC_API_KEY` must be set. Plus embedding provider key if configured
- Sandbox: memory must be a valid size (e.g. `2g`, `512m`), pids_limit > 0
- Sandbox: at most one user with `trust = "owner"`; warns if sandbox enabled but no owner configured
- Prompt files: paths must be relative, no `..` or absolute paths, no duplicates
- Groups: `match` must be non-empty; `trigger = "mention"` requires `mention_names`; `trigger = "regex"` requires valid `trigger_regex`; `default_trust = "owner"` warns (dangerous in groups); duplicate match patterns warn; groups without a signal channel configured warn

## Sandbox

When `[sandbox]` is enabled, non-owner bash commands run inside Linux namespaces (user, mount, PID, network) with resource limits. The owner bypasses the sandbox entirely.

### Trust levels and sandbox behavior

| Trust Level | Sandbox | Tools | Memory Stores |
|-------------|---------|-------|---------------|
| **owner** | No — executes directly on host | All tools, unrestricted | All (private, shared, social) |
| **full** | Yes — sandboxed to workspace | All tools, bash in sandbox | All (private, shared, social) |
| **inner** | Yes — sandboxed to workspace | Bash + file tools, in sandbox | shared, social |
| **familiar** | Yes — sandboxed to workspace | Read-only file tools | social |
| **public** | N/A | No tools | None |

The only behavioral difference between `owner` and `full` is sandbox bypass. When sandbox is disabled, they behave identically.

### Terminal default

When sandbox is enabled and no user matches the terminal, the terminal defaults to `owner` trust (the person at the keyboard is the machine owner). For all other channels, unmatched senders remain `public`.

### Check sandbox status

Use `coop sandbox status` to see platform capabilities:

```
Sandbox: linux (namespaces + seccomp)
  ✓ user namespaces
  ✓ network namespaces
  ✗ landlock
  ✓ seccomp
  ✓ cgroups v2
```

## Skills

Skills are reusable instruction sets discovered from `SKILL.md` files. They appear as a menu in the system prompt; use `read_file` to load one when the task matches.

### Installation paths

- **Workspace-level** — `skills/{name}/SKILL.md` (available to all users)
- **Per-user** — `users/{user}/skills/{name}/SKILL.md` (only for that user; overrides a workspace skill with the same name)

Skills are rescanned on every turn, so new skills added to the workspace while coop is running are picked up automatically without a restart.

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
