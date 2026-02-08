# Memory System Design (Current Implementation)

This document describes what Coop memory is **today** in this repository.

It replaces the earlier aspirational design with concrete behavior from:
- `crates/coop-memory`
- `crates/coop-gateway/src/memory_tools.rs`
- `crates/coop-gateway/src/gateway.rs`
- `crates/coop-gateway/src/main.rs`

---

## Status

Implemented now:
- Structured observations stored in SQLite
- FTS5 full-text search over observation fields
- Trust-gated memory tools (`memory_search`, `memory_timeline`, `memory_get`, `memory_write`, `memory_history`, `memory_people`)
- Hash-based exact dedup (`title + facts`) with `mention_count` bump
- Observation history table (currently logs ADD events)
- People index table promoted from observation writes
- Auto-capture of non-memory tool executions into memory

Not implemented yet (still future work):
- sqlite-vec vector search
- Embedding computation pipeline
- LLM reconciliation (ADD/UPDATE/DELETE/NONE)
- Retention/compression tiers
- DB-backed memory index injected at prompt boot
- Flat-file import command

---

## High-Level Architecture

### Crate boundary

`coop-memory` is a dedicated crate with:
- `Memory` trait
- `SqliteMemory` implementation
- memory domain types (`Observation`, `MemoryQuery`, etc.)

`coop-gateway` owns runtime wiring:
- opens SQLite DB on startup
- registers memory tools
- passes memory handle into `Gateway`
- auto-captures tool executions into observations

### Runtime wiring

At startup/chat init (`coop-gateway/src/main.rs`):
1. Resolve `memory.db_path` from config dir
2. Open SQLite DB via `SqliteMemory::open(...)`
3. Add `MemoryToolExecutor` to `CompositeExecutor`
4. Pass `Some(memory)` into `Gateway`

Trace event verified in runtime:
- `"initializing memory store"` with DB path field

---

## Data Model (SQLite)

Schema lives in `crates/coop-memory/src/sqlite/schema.rs`.

### `observations`

Core memory record:
- identity/context: `agent_id`, `session_key`, `store`
- structure: `type`, `title`, `narrative`, `facts`, `tags`
- provenance: `source`, `related_files`, `related_people`
- dedup/weighting: `hash`, `mention_count`
- metadata: `token_count`, `created_at`, `updated_at`, `expires_at`, `min_trust`

### `observations_fts`

FTS5 virtual table indexed on:
- `title`, `narrative`, `facts`, `tags`

Maintained by triggers:
- insert/update/delete triggers keep FTS rowid aligned to `observations.id`

### `observation_history`

Mutation history table:
- `observation_id`, old/new title/facts, event, timestamp

Current behavior:
- only `ADD` events are emitted by current write path

### `session_summaries`

Stores generated session summary rows:
- `request`, `outcome`, `decisions`, `open_items`, `observation_count`

### `people`

Promoted people index:
- `name`, `store`, `facts`, `last_mentioned`, `mention_count`
- unique key: `(agent_id, name)`

### Indexes

Implemented indexes:
- `idx_obs_agent`, `idx_obs_store`, `idx_obs_type`, `idx_obs_created`, `idx_obs_trust`, `idx_obs_hash`
- `idx_history_obs`

---

## Memory API (Trait)

`coop-memory/src/traits.rs`:
- `search(query) -> Vec<ObservationIndex>`
- `timeline(anchor, before, after) -> Vec<ObservationIndex>`
- `get(ids) -> Vec<Observation>`
- `write(obs) -> WriteOutcome`
- `people(query) -> Vec<Person>`
- `summarize_session(session_key) -> SessionSummary`
- `history(observation_id) -> Vec<ObservationHistoryEntry>`

`EmbeddingProvider` trait exists in the crate but is not used by the current SQLite backend.

---

## Retrieval Semantics (Current)

### Search (`SqliteMemory::search`)

Two query modes:
1. `text` present → FTS5 query (`MATCH`) + BM25 score
2. no `text` → recency list (`ORDER BY updated_at DESC`)

Filters supported:
- `stores`
- `types`
- `after` / `before`
- `people` (applied post-fetch using `related_people` JSON)

Token budgeting:
- `limit` defaults to 10 when zero
- backend fetches up to `limit * 5` candidates
- optional `max_tokens` stops accumulation once budget exceeded (after at least one result)

Scoring (implemented):
- with text query:
  - `0.6 * fts + 0.2 * recency + 0.2 * mention`
- without text query:
  - `0.7 * recency + 0.3 * mention`
- recency: `1 / (1 + days_since_update)`
- mention: `min(mention_count / 10, 1)`
- `ObservationIndex.score` is populated with this computed score

### Timeline

`timeline(anchor, before, after)`:
- fetch anchor by id
- fetch `before` older observations by `created_at`
- fetch `after` newer observations by `created_at`
- return chronological window
- score is `0.0` for timeline entries

### Get

`get(ids)`:
- returns full observations in requested order
- silently omits missing/expired IDs

---

## Write Semantics (Current)

### Dedup path

Hash = SHA-256 of:
- `title`
- null separator
- each `fact` + null separator

If exact hash exists for same `agent_id` and non-expired row:
- increment `mention_count`
- update `updated_at`
- return `WriteOutcome::ExactDup`

### Insert path

If not exact duplicate:
- insert new row into `observations`
- insert `ADD` row into `observation_history`
- upsert all `related_people` into `people`
- return `WriteOutcome::Added(id)`

### Current `WriteOutcome` in practice

Enum includes:
- `Added`, `Updated`, `Deleted`, `Skipped`, `ExactDup`

Current backend behavior emits:
- `Added`
- `ExactDup`

(UPDATE/DELETE/SKIPPED are reserved for future reconciliation flow.)

---

## Tool Surface (Gateway)

Implemented in `crates/coop-gateway/src/memory_tools.rs`.

### `memory_search`
Args:
- `query`, `stores`, `types`, `people`, `after_ms`, `before_ms`, `limit`, `max_tokens`

### `memory_timeline`
Args:
- `anchor` (required), `before`, `after`

### `memory_get`
Args:
- `ids` (required)

### `memory_write`
Args:
- required: `title`
- optional: `store`, `type`, `narrative`, `facts`, `tags`, `related_files`, `related_people`, `source`, `token_count`, `expires_at_ms`, `session_key`

Defaults:
- `store`: derived from trust (`trust_to_store`)
- `type`: `discovery`
- `source`: `agent`
- `session_key`: current tool context session

### `memory_history`
Args:
- `observation_id` (required)

### `memory_people`
Args:
- `query`

All tools return JSON text payloads.

---

## Trust Gating (Enforced Today)

Store visibility matrix:
- `full` → `private`, `shared`, `social`
- `inner` → `shared`, `social`
- `familiar` → `social`
- `public` → none

Applied in tool layer:
- `memory_search`: requested stores intersected with accessible stores
- `memory_get`, `memory_timeline`, `memory_people`: post-filtered by accessible stores
- `memory_history`: verifies target observation store is accessible
- `memory_write`: rejects writes to inaccessible store

Store defaults from trust:
- `full` → `private`
- `inner` → `shared`
- `familiar/public` → `social` (but `public` cannot write because accessible stores are empty)

---

## Automatic Tool Capture (Implemented)

In `Gateway::run_turn_with_trust`, after every tool result:
- non-memory tools are auto-captured via `capture_tool_observation(...)`
- capture runs in background (`tokio::spawn`)
- writes a `technical` observation with:
  - title: `Tool run: <tool_name>`
  - narrative: serialized args + truncated output
  - facts: tool name + error flag
  - tags: `["tool", <tool_name>]`
  - source: `auto`
  - store/min_trust derived from turn trust
  - related file hints from args keys (`path`, `file`, `target`, `from`, `to`)

Current behavior is direct write, not LLM compression.

---

## Prompt Integration (Current Reality)

The database memory is available through tools, but prompt assembly is still file-centric today.

Specifically:
- `PromptBuilder` still manages file-based memory/menu behavior
- DB observations are **not** yet injected as a startup index layer in the system prompt

So current retrieval model is:
- agent asks via memory tools on demand
- not automatic DB preload at boot

---

## Configuration

`coop.yaml` now supports:

```yaml
memory:
  db_path: ./data/memory.db
  embedding:            # optional, currently not consumed by SqliteMemory
    provider: voyage
    model: voyage-3-large
    dimensions: 1024
```

Notes:
- `db_path` is resolved relative to config directory when not absolute
- config check validates memory path shape and embedding field sanity

---

## Known Gaps vs Original Target Design

Still pending from the earlier design draft:
- hybrid semantic retrieval (sqlite-vec + embeddings)
- reconciliation worker with LLM decisions
- archive/retention jobs
- contradiction handling / UPDATE / DELETE flows
- boot-time DB memory index in prompt
- migration/import CLI from markdown memory files

This doc should be updated as each gap is implemented.
