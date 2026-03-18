# NousResearch/hermes-agent: ideas worth integrating into Coop

Analysis of [github.com/NousResearch/hermes-agent](https://github.com/NousResearch/hermes-agent) — a Python-based multi-platform AI agent by Nous Research — compared against the Coop gateway (Rust). The two projects share many goals (multi-channel agent, tools, memory, scheduling) but take different architectural approaches. This document identifies the strongest ideas from Hermes that Coop either lacks or implements differently, and recommends what to port.

---

## Executive summary

Hermes is a mature, feature-rich Python agent with ~3000 tests, 6+ messaging platforms, and a deep learning loop (skills, session search, memory nudges). Coop is a leaner Rust gateway focused on trust, tracing, and config safety. The best ideas to steal from Hermes fall into six themes:

1. **Self-improving skills with lifecycle management** — the agent creates, patches, and self-improves procedural skills from experience
2. **Programmatic tool calling (execute_code)** — collapse multi-step tool chains into a single Python script with RPC back to the host
3. **Smart model routing** — route simple questions to a cheap model automatically
4. **Session search with LLM summarization** — FTS5 search across past sessions with focused summaries rather than raw transcript dumps
5. **Usage insights and cost tracking** — full analytics dashboard (tokens, cost, tool breakdown, activity patterns)
6. **Secret redaction** — regex-based automatic masking of API keys, tokens, and credentials in all logs and output

---

## Priority shortlist

| Priority | Idea | Why it matters | Hermes reference |
|---|---|---|---|
| **P0** | Self-improving skills lifecycle | The agent creates skills from hard tasks, patches broken ones during use, and builds procedural memory that survives across sessions | `tools/skills_tool.py`, `tools/skill_manager_tool.py`, `agent/prompt_builder.py` |
| **P0** | Programmatic tool calling (execute_code) | Collapses N tool calls into 1 LLM turn by letting the model write a Python script that calls tools via RPC | `tools/code_execution_tool.py` |
| **P0** | Session search with summarization | Gives the agent cross-session recall without bloating context — searches SQLite FTS5, then summarizes matching sessions with a cheap model | `tools/session_search_tool.py`, `hermes_state.py` |
| **P1** | Smart model routing | Automatically routes simple questions ("what time is it?") to a cheap/fast model, saving cost on trivial turns | `agent/smart_model_routing.py` |
| **P1** | Usage insights / cost dashboard | Full analytics: tokens by model, cost estimates, tool usage breakdown, activity patterns, streaks | `agent/insights.py`, `agent/usage_pricing.py` |
| **P1** | Secret redaction in logs and output | Regex-based automatic masking of API keys, tokens, phone numbers, private keys in all text output | `agent/redact.py` |
| **P1** | Context file prompt injection scanning | Scans AGENTS.md, .cursorrules, SOUL.md for prompt injection patterns before including them in the system prompt | `agent/prompt_builder.py` |
| **P1** | Dangerous command approval system | Pattern-based detection of destructive commands with per-session approval state and permanent allowlists | `tools/approval.py` |
| **P2** | Skin/theme engine | Data-driven CLI theming with YAML skin files — zero code changes to add a new theme | `hermes_cli/skin_engine.py` |
| **P2** | Parallel subagent delegation | Spawn up to 3 isolated child agents in parallel, each with restricted toolsets, returning only summaries | `tools/delegate_tool.py` |
| **P2** | Platform-aware skill filtering | Skills can declare OS platform requirements and conditional activation based on available toolsets | `tools/skills_tool.py`, `agent/prompt_builder.py` |
| **P2** | Toolset composition system | Tools grouped into named toolsets that can be composed, enabled/disabled per platform | `toolsets.py` |

---

## Detailed ideas

### 1. Self-improving skills with lifecycle management

**The single strongest idea in Hermes that Coop should steal.**

Coop already has a skills *index* (scan `skills/*/SKILL.md`, show a compact list in the prompt, let the agent load on demand). That is good. But Hermes goes much further:

- **Agent-created skills**: After completing a complex task (5+ tool calls), the agent is nudged to save the approach as a skill via `skill_manage(action='create')`.
- **Self-patching**: When a skill is loaded and found outdated or wrong, the agent patches it *during use* with `skill_manage(action='patch')`. The prompt explicitly says: "Skills that aren't maintained become liabilities."
- **Skills Hub**: Community-shared skills via [agentskills.io](https://agentskills.io), installable with `hermes skills install`.
- **Progressive disclosure**: Skills have frontmatter with metadata, and the full instructions are only loaded when needed (exactly like Coop).
- **Conditional activation**: Skills can declare `fallback_for_toolsets` (show only when a primary tool is unavailable) and `requires_toolsets` (show only when certain tools are available).
- **Platform filtering**: Skills declare which OS platforms they support (`platforms: [macos, linux]`).

**What Coop has**: Skill discovery and index. What Coop lacks: skill creation, patching, conditional activation, and the closed learning loop.

**Key references**: `tools/skills_tool.py`, `tools/skill_manager_tool.py`, `agent/prompt_builder.py` (`build_skills_system_prompt()`), `AGENTS.md`

**Recommendation**: Port the skill creation/patching tools and the nudge logic. This is the core of what makes Hermes a "self-improving" agent. The conditional activation system is also worth copying for cleaner skill management.

---

### 2. Programmatic tool calling (execute_code)

**Hermes's most innovative tool architecture.**

Instead of the agent making 15 sequential tool calls (each burning context), it writes a Python script that calls tools via RPC. The parent process:

1. Generates a `hermes_tools.py` stub module with RPC functions for allowed tools
2. Opens a Unix domain socket and starts an RPC listener thread
3. Spawns a child process with API keys stripped from the environment
4. The script calls tool functions → RPC → parent dispatches → result returns
5. Only stdout is returned to the LLM; intermediate tool results never enter context

**Why this is excellent:**
- Collapses N tool calls into 1 LLM turn
- Zero context cost for intermediate results
- The child process is sandboxed (no API keys in env)
- Built-in helpers: `json_parse()`, `shell_quote()`, `retry()`
- 5-minute timeout, 50KB stdout cap, max 50 tool calls per script

**What Coop has**: Sandboxed bash execution. What Coop lacks: programmatic tool calling that collapses multi-step pipelines.

**Key references**: `tools/code_execution_tool.py`

**Recommendation**: This is a high-value, medium-effort feature. The RPC pattern would translate well to Rust (Unix socket + JSON protocol). The key insight — "let the model write code that calls tools, not call tools one at a time" — is worth adopting even if the implementation differs.

---

### 3. Session search with LLM summarization

**Better cross-session recall than raw transcript retrieval.**

Hermes stores all conversations in SQLite with FTS5 full-text search. The `session_search` tool:

1. Searches messages via FTS5 ranked by relevance
2. Groups results by session, takes top N unique sessions
3. Loads each session's conversation, truncates centered on matches
4. Sends to a cheap model (Gemini Flash) with a focused summarization prompt
5. Returns per-session summaries with metadata — not raw transcripts

**Why this is better than what Coop does:**
- Coop's memory system stores *observations* (structured facts). Session search stores *full transcripts* and retrieves them semantically.
- The summarization step prevents context bloat — the agent gets a focused recap, not 50K of raw chat history.
- Summaries are generated in parallel for speed.

**What Coop has**: Memory observations, FTS5 search, session summaries (auto-generated after each turn). What Coop lacks: searchable full session transcripts with on-demand LLM summarization.

**Key references**: `tools/session_search_tool.py`, `hermes_state.py`

**Recommendation**: Add FTS5 indexing of full session messages (Coop already persists sessions as JSONL). Build a `session_search` tool that searches and summarizes. This complements Coop's observation-based memory with full-transcript recall.

---

### 4. Smart model routing for simple turns

**Route cheap questions to cheap models automatically.**

`agent/smart_model_routing.py` implements a conservative heuristic:
- If the message is short (≤160 chars, ≤28 words)
- Has no code fences, no URLs, no multi-line content
- Contains no "complex keywords" (debug, implement, refactor, analyze, etc.)
- → Route to the configured cheap model (e.g., GPT-4o-mini via OpenRouter)

**Why this is smart:**
- Conservative by design — if in doubt, uses the primary model
- The cheap model config is explicit (provider + model + optional API key)
- Each turn's route is resolved independently
- The routing reason is logged for debugging

**What Coop has**: A single model per session. What Coop lacks: per-turn model routing.

**Key references**: `agent/smart_model_routing.py`

**Recommendation**: Easy to port. Add a `routing` config section and a `choose_cheap_model` function that runs before each provider call. Coop's provider registry already supports multiple models — this just needs a selection heuristic.

---

### 5. Usage insights and cost tracking

**Full analytics dashboard for agent usage.**

`agent/insights.py` provides:
- Total sessions, messages, tool calls, tokens (input/output)
- Cost estimation using a model pricing table (`usage_pricing.py`)
- Model breakdown (sessions, tokens, cost per model)
- Platform breakdown (CLI vs Telegram vs Discord etc.)
- Tool usage ranking with percentages
- Activity patterns by day of week and hour
- Streak tracking (consecutive active days)
- Notable sessions (longest, most messages, most tokens, most tool calls)
- Terminal and gateway formatting

**What Coop has**: Per-turn token counting and context-limit tracking. What Coop lacks: aggregate analytics, cost estimation, and a user-facing `/insights` command.

**Key references**: `agent/insights.py`, `agent/usage_pricing.py`

**Recommendation**: Add a model pricing table and a cost tracking layer. Build an insights query that aggregates session data from Coop's existing stores. This is useful for both operators and the agent itself (answering "how much did I cost this week?").

---

### 6. Secret redaction in logs and output

**Automatic masking of API keys, tokens, and credentials.**

`agent/redact.py` applies regex-based redaction to all text:
- Known API key prefixes (sk-, ghp_, xox*, AIza*, etc.)
- Environment variable assignments (`OPENAI_API_KEY=sk-abc...`)
- JSON fields (`"apiKey": "value"`)
- Authorization headers
- Telegram bot tokens
- Private key blocks
- Database connection strings
- E.164 phone numbers

Short tokens are fully masked; longer tokens preserve prefix+suffix for debuggability. A `RedactingFormatter` wraps Python's logging to auto-redact all log output.

**What Coop has**: The AGENTS.md privacy rules say "never commit PII" but there's no automatic redaction layer. What Coop lacks: runtime redaction of secrets in logs and tool output.

**Key references**: `agent/redact.py`

**Recommendation**: Port the regex patterns and build a Rust redaction function. Apply it in the tracing subscriber (JSONL layer) and in tool output before it reaches the prompt. This is cheap insurance against accidental secret leakage.

---

### 7. Context file injection scanning

**Scan workspace files for prompt injection before including them in the system prompt.**

`agent/prompt_builder.py` includes `_scan_context_content()` which checks AGENTS.md, .cursorrules, and SOUL.md for:
- Invisible Unicode characters (zero-width spaces, BiDi overrides)
- Prompt injection patterns ("ignore previous instructions", "system prompt override", etc.)
- Exfiltration patterns (curl/wget piping secrets)
- Hidden HTML divs

Blocked files get replaced with `[BLOCKED: filename contained potential prompt injection]`.

**What Coop has**: Prompt files are loaded and injected without scanning. What Coop lacks: any defense against malicious workspace files.

**Key references**: `agent/prompt_builder.py` (`_CONTEXT_THREAT_PATTERNS`, `_scan_context_content`)

**Recommendation**: Port the regex patterns. This is especially important for Coop since it loads per-user and per-group workspace files — a lower-trust user's AGENTS.md could contain injection attempts.

---

### 8. Dangerous command approval system

**Pattern-based detection of destructive commands with interactive approval.**

`tools/approval.py` maintains:
- A list of dangerous command patterns (recursive delete, disk format, fork bombs, pipe-to-shell, etc.)
- Per-session approval state (thread-safe, keyed by session)
- Permanent allowlist persistence in config.yaml
- Smart approval via auxiliary LLM (auto-approve low-risk commands)

When a dangerous command is detected, the user is prompted for approval before execution.

**What Coop has**: Trust-level-based tool gating. What Coop lacks: within-trust-level command approval for dangerous operations. A "full trust" user's agent can still accidentally `rm -rf /`.

**Key references**: `tools/approval.py`

**Recommendation**: Add a dangerous command pattern list to the bash tool. For full-trust users, log a warning. For lower-trust users, inject a confirmation step. The pattern list is directly portable.

---

### 9. Parallel subagent delegation with isolated context

**Spawn child agents for parallel workstreams.**

`tools/delegate_tool.py` implements:
- Single-task and batch (up to 3 parallel) modes
- Each child gets: fresh conversation, own task_id, restricted toolset, focused system prompt
- Blocked tools: no recursive delegation, no user interaction, no shared memory writes
- Depth limit (max 2 levels)
- Shared iteration budget across parent + children
- Per-task progress callbacks (CLI tree view, gateway batched updates)
- Tool trace collection from child conversations
- Interrupt propagation from parent to children

**What Coop has**: Documented subagent architecture (design.md), session injection mechanism. What Coop lacks: an actual delegation tool that spawns child agents.

**Key references**: `tools/delegate_tool.py`

**Recommendation**: Implement a `delegate` tool that spawns a child session with restricted tools and isolated context. Coop's `SessionInjection` and provider registry are already set up for this. The key design decisions (blocked tools, depth limit, shared budget) are directly portable.

---

### 10. Memory with injection scanning and frozen snapshots

**Hermes's memory model has good operational details.**

Key design choices:
- Memory is injected as a frozen snapshot at session start → never mutated mid-session → preserves prompt cache
- Mid-session writes update disk immediately but don't touch the prompt
- Memory content is scanned for injection patterns before prompt injection
- Bounded by character limits (not tokens) for model independence
- Behavioral guidance: "prioritize what reduces future user steering"

**What Coop does differently**: Coop's memory system is much richer (observations, reconciliation, FTS + vector search, retention pipeline). But it could benefit from the frozen-snapshot-for-cache-stability pattern and the injection scanning.

**Key references**: `tools/memory_tool.py`

**Recommendation**: Consider the frozen snapshot pattern if prompt cache hit rates are important. Definitely port the injection scanning for memory content.

---

### 11. Toolset composition and per-platform presets

**Named toolset groups with platform-specific defaults.**

`toolsets.py` defines tool groups (web, terminal, file, browser, etc.) that can be composed and enabled/disabled per platform. Each messaging platform gets a preset:

```python
"hermes-telegram": ["web", "terminal", "file", "skills", "memory", ...]
"hermes-discord":  ["web", "terminal", "file", "skills", ...]
```

Users can customize with `hermes tools` — enable/disable entire toolsets per platform.

**What Coop has**: Trust-level-based tool filtering. What Coop lacks: named toolset groups and per-platform/per-user tool presets.

**Key references**: `toolsets.py`, `hermes_cli/tools_config.py`

**Recommendation**: Consider adding named toolsets to Coop's config. This would let operators say "signal users get web+file but not bash" without enumerating individual tools.

---

### 12. Skin/theme engine

**Data-driven CLI theming with zero code changes.**

`hermes_cli/skin_engine.py` provides:
- Built-in skins (default, ares, mono, slate)
- User skins as YAML files in `~/.hermes/skins/`
- Missing values inherit from default
- Runtime switching via `/skin` command
- Customizable: banner colors, spinner faces/verbs, tool prefix, branding text, response box

**What Coop has**: A TUI based on crossterm. What Coop lacks: any theming system.

**Key references**: `hermes_cli/skin_engine.py`, `docs/skins/example-skin.yaml`

**Recommendation**: Low priority but nice UX polish. If Coop's TUI grows, a data-driven theme system prevents hardcoded color constants from proliferating.

---

## Ideas Coop already does better

For context, here's where Coop is ahead of Hermes:

| Capability | Coop | Hermes |
|---|---|---|
| **Trust model** | Bell-LaPadula-inspired trust + ceiling + workspace scoping | No trust model — single-user assumption |
| **Config safety** | Validate → backup → atomic write → rollback → health check | Config is a YAML file with no validation pipeline |
| **Structured memory** | Observations + reconciliation + retention + compression + vector search | File-backed MEMORY.md with character limits |
| **Tracing** | JSONL traces with rotation, required for all new features | Standard Python logging |
| **Prompt caching** | Layered prompt with cache hints, ordered for prefix caching | Anthropic-specific 4-breakpoint caching |
| **Sandboxing** | Platform-adaptive (Landlock/namespaces/containers) with trust-aware network policy | Docker containers with dropped capabilities |
| **Session compaction** | Iterative LLM compaction with file tracking and overflow retry | Head/tail protection with middle summarization |
| **Hot reload** | Safe-field-only hot reload with restart-only field detection | No hot reload |
| **Service management** | systemd/launchd install, status, logs, rollback | Manual process management |

---

## Recommended adoption order

### Phase 1: Quick wins (high value, moderate effort)
1. Secret redaction in logs and tool output
2. Context file injection scanning
3. Dangerous command approval patterns
4. Smart model routing for simple turns
5. Usage insights and cost tracking

### Phase 2: Core capabilities (high value, significant effort)
6. Self-improving skills lifecycle (create, patch, conditional activation)
7. Session search with LLM summarization
8. Programmatic tool calling (execute_code / RPC pattern)

### Phase 3: Multi-agent and UX
9. Parallel subagent delegation
10. Toolset composition and per-platform presets
11. Skin/theme engine

---

## Bottom line

Hermes and Coop are complementary. Coop has stronger infrastructure (trust, tracing, config safety, compaction, sandboxing). Hermes has stronger agent-level intelligence (self-improving skills, programmatic tool calling, cross-session search, model routing, cost tracking, secret redaction).

The highest-leverage ports are:

- **Skills that the agent creates and fixes itself** — this is the core of what makes Hermes feel "self-improving"
- **execute_code** — collapsing tool chains into scripts is a genuinely better architecture for multi-step work
- **Session search** — full-transcript recall with on-demand summarization fills a gap in Coop's observation-based memory
- **Secret redaction** — cheap insurance that should be table stakes
- **Smart model routing** — easy to implement, immediate cost savings

The rest (insights, approval, skins, toolsets) are nice-to-haves that improve the product but aren't architecturally novel.
