# Memory System Design (Current Implementation)

This document reflects the memory system as implemented in this repository today.

Relevant code paths:
- `crates/coop-memory`
- `crates/coop-gateway/src/memory_tools.rs`
- `crates/coop-gateway/src/memory_prompt_index.rs`
- `crates/coop-gateway/src/memory_embedding.rs`
- `crates/coop-gateway/src/memory_reconcile.rs`
- `crates/coop-gateway/src/main.rs`

---

## Status

Implemented now:
- Structured observations in SQLite (`observations`)
- FTS5 retrieval
- Optional semantic vector retrieval via `sqlite-vec` (`observations_vec`) with graceful fallback
- Hybrid ranking (FTS + vector + recency + mention_count)
- Embedding pipeline wired for query/search and write mutations
- Reconciliation pipeline with `ADD / UPDATE / DELETE / NONE`
- Exact-dedup (`title + facts` hash) with `mention_count` bump
- Observation history records for `ADD / UPDATE / DELETE / COMPRESS`
- Trust-gated memory tools (`memory_search`, `memory_timeline`, `memory_get`, `memory_write`, `memory_history`, `memory_people`)
- Auto-capture of non-memory tool executions into observations
- Gateway E2E reconciliation integration coverage (ADD/UPDATE/DELETE/NONE/exact_dup/trust-gate)
- Trust-gated prompt boot-time DB memory index injection
- Retention / deterministic compression / archive cleanup maintenance pipeline
- Periodic maintenance runner in gateway (startup + interval)
- Expanded embedding provider wiring (`openai`, `voyage`, `cohere`, `openai-compatible`)

Still not implemented:
- Flat-file import/migration command
- LLM-based semantic compression (current compression is deterministic rule-based)

---

## Runtime Wiring

Gateway startup (`coop-gateway/src/main.rs`) wires memory with optional embedding and reconciliation components:

1. Resolve `memory.db_path`
2. Build optional `EmbeddingProvider` from `memory.embedding`
3. Build `ProviderReconciler` from the configured LLM provider (`complete_fast`)
4. Open memory with `SqliteMemory::open_with_components(...)`
5. Start maintenance loop when `memory.retention.enabled`:
   - run once at startup
   - run periodically in background

Key startup traces include:
- `initializing memory store`
- `memory embedding configured`
- `memory maintenance loop started`
- `memory maintenance run started` / stage completion events

---

## Prompt Boot-Time DB Memory Index

Gateway prompt build now appends a compact DB memory index before each turn when enabled:

Config:

```toml
memory:
  prompt_index:
    enabled: true
    limit: 12
    max_tokens: 1200
```

Behavior:
- Built in `coop-gateway` (`memory_prompt_index.rs`), not in `coop-core`
- Uses trust-gated stores:
  - `full`: private/shared/social
  - `inner`: shared/social
  - `familiar`: social
  - `public`: none
- Includes compact rows only (id/store/type/title/score/mention/created)
- Enforces `limit` and `max_tokens`
- If generation fails, prompt creation degrades gracefully (turn still proceeds)

Trace events:
- `memory prompt index built`
- `memory prompt index skipped` (with reason)
- `memory prompt index injected`

---

## Data Model

### Active tables
- `observations`
- `observations_fts` + triggers
- `observation_embeddings`
- `observation_history`
- `session_summaries`
- `people`

### Archive table
- `observation_archive`
  - stores original observation payload + metadata
  - fields include `original_observation_id`, store/type/title/narrative/facts/tags/source,
    related files/people, hash, mention/token counts, created/updated/expires timestamps,
    `archived_at`, `archive_reason`

---

## Retrieval Semantics

`Memory::search`:
- With `query.text`: FTS candidate retrieval
- Without `query.text`: recency retrieval
- With text + query embedding + vec enabled: merges vector candidates with FTS

Fallback behavior:
- No embedder: FTS-only
- Embedding request failure: FTS-only for that query
- `sqlite-vec` unavailable/query errors: vector path disables and search continues FTS-only

Store/type/date filters are applied in SQL before ranking.
People filters apply before final sort.

---

## Reconciliation Pipeline

`SqliteMemory::write` flow:

1. Exact hash dedup check
   - match: bump mention_count, return `ExactDup`
2. Similar candidate retrieval (hybrid search; fallback FTS)
3. If no candidate above threshold: `ADD`
4. If candidates exist and reconciler configured:
   - send `ReconcileRequest` with dense candidate indices
   - parse strict JSON decision
   - apply decision

Decision application:
- `ADD`: insert new observation
- `UPDATE`: mutate matched row, record `UPDATE` history
- `DELETE`: expire stale row, insert replacement, record `DELETE` history
- `NONE`: bump mention_count only

If reconciliation fails or returns invalid candidate index, fallback is `ADD`.

---

## Lifecycle Maintenance (Retention / Compression / Archive)

Maintenance API:
- `Memory::run_maintenance(&MemoryMaintenanceConfig)`

Config:

```toml
memory:
  retention:
    enabled: true
    archive_after_days: 30
    delete_archive_after_days: 365
    compress_after_days: 14
    compression_min_cluster_size: 3
    max_rows_per_run: 200
```

Stages per run:
1. **Compression**
   - scans stale observations older than `compress_after_days`
   - clusters by stable key (`store + type + normalized title`)
   - for clusters meeting `compression_min_cluster_size`:
     - inserts deterministic summary observation
     - expires original rows
     - records `COMPRESS` history on originals
2. **Archive move**
   - archives aged/expired observations into `observation_archive`
   - removes archived rows from active table
3. **Archive cleanup**
   - deletes archive rows older than `delete_archive_after_days`

All stages are bounded by `max_rows_per_run`.

---

## Embedding Providers

Embedding implementation is gateway-owned (`memory_embedding.rs`) and uses provider registry wiring.

Supported providers:
- `openai` (`OPENAI_API_KEY`)
- `voyage` (`VOYAGE_API_KEY`)
- `cohere` (`COHERE_API_KEY`)
- `openai-compatible` (requires `base_url` + `api_key_env`)

`openai-compatible` example:

```toml
memory:
  embedding:
    provider: openai-compatible
    model: text-embedding-3-small
    dimensions: 1536
    base_url: https://your-endpoint.example/v1
    api_key_env: OPENAI_COMPAT_API_KEY
```

Safety:
- dimension mismatch is rejected
- traces log metadata only (provider/model/text length/status/dimensions)
- API keys are never logged

---

## Config Validation (`coop check`)

Current memory validation covers:
- `memory.db_path` parent validity
- `memory.prompt_index` (`limit > 0`, `max_tokens > 0`)
- `memory.retention` field constraints + cross-checks
- embedding provider support list
- embedding model non-empty
- embedding dimensions bounded (`1..=8192`)
- provider-specific embedding requirements (e.g. openai-compatible base URL + env var)
- embedding API key env presence

---

## Tracing

Memory tracing includes:
- embedding request/response metadata
- vector fallback activation
- reconciliation request/decision/application
- prompt index build/skip/injection events
- maintenance loop/run/stage metrics

JSONL traces (`COOP_TRACE_FILE`) are the primary debugging interface.

---

## Known Remaining Gaps

- Flat-file memory import/migration path
- Higher-level compaction policies beyond deterministic cluster summarization
