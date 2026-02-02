# Design Principles

These are non-negotiable. Every PR, every design decision, every refactor should be evaluated against them.

---

## 1. Simplicity Over Features

Coop must be small enough to reason about. If you can't hold the entire architecture in your head, it's too complex.

**Targets:**
- Core gateway: under 5,000 lines of Rust (excluding Goose dependency)
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
User edits coop.yaml
         │
         ▼
    ┌──────────┐
    │ Parse    │ ← YAML syntax valid?
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
    │ Snapshot  │ ← Save current working config as .coop.yaml.bak
    │ Apply    │ ← Hot-swap the live config
    └────┬─────┘
         │
    ┌────▼─────┐
    │ Health   │ ← Can the agent respond? Can channels connect?
    │ Check    │   If not → automatic rollback to .coop.yaml.bak
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
    config      TEXT NOT NULL,        -- full YAML snapshot
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

```yaml
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

## Anti-Patterns (Things We Will Not Do)

- **Config sprawl.** One config file. One format. No env var overrides that shadow file config. No "merge 5 sources" priority chains.
- **Plugin system.** No dynamic loading, no plugin registries, no hook chains. If you want different behavior, modify the code. The codebase is small enough that this is safe.
- **Module cache / hot reload of code.** Single binary. Restart is fast. No JS module cache bugs, no stale code paths.
- **Implicit behavior.** Every decision the gateway makes should be traceable to a config value or a default. No "magic" that requires reading the source to understand.
- **Swallowed errors.** Every error gets logged with context. `unwrap()` only in places where failure is truly impossible. `expect("reason")` everywhere else.

---

## Summary

```
Simple   → small codebase, flat structure, no premature abstraction
Robust   → validate-before-apply, auto-rollback, crash recovery
Observable → the agent can diagnose itself
Reliable → prioritize message persistence and config safety above all
```

If a feature conflicts with these principles, the feature loses.
