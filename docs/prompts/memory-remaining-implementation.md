# Memory Remaining Features Implementation Prompt

You are working in /root/coop/memory.

Goal: implement the remaining memory features that are currently missing:
1) sqlite-vec semantic vector retrieval path
2) LLM reconciliation pipeline (ADD/UPDATE/DELETE/NONE decisioning)
3) embedding calls wired into scoring/write path

Current baseline already exists:
- Structured SQLite memory in crates/coop-memory
- FTS5 search
- memory tools in crates/coop-gateway/src/memory_tools.rs
- exact hash dedup + mention_count bump
- auto-capture of non-memory tools to observations

Use the current code as source of truth and extend it (do not rewrite from scratch).

## Constraints (must follow)
- Follow AGENTS.md rules.
- Keep heavy HTTP/provider dependencies out of coop-memory; keep them in gateway.
- Any config changes must also update config_check::validate_config.
- Add tracing spans/events for all new major paths and verify trace output with COOP_TRACE_FILE.
- Keep files modular (< ~500 lines/file where practical).
- Keep behavior trust-gated.
- No PII in tests/docs.

## Implementation plan

### A) Vector retrieval path (sqlite-vec) with graceful fallback
- Extend coop-memory schema to support vector storage and search.
- Implement vector candidate retrieval for query embeddings.
- If sqlite-vec is unavailable/unconfigured, search must degrade to FTS-only (no hard failure).
- Ensure trust/store/type/date filters are applied before final ranking influence (inaccessible rows must not affect rank).

Expected ranking:
- combine FTS relevance + vector similarity + recency + mention_count
- keep existing recency/mention behavior, add vector term cleanly
- preserve max_tokens/limit behavior in final result set.

### B) Embedding pipeline wiring
- Use existing EmbeddingProvider trait in coop-memory.
- Create concrete embedder implementation in gateway (configured from coop.yaml memory.embedding).
- Instantiate SqliteMemory with embedder from gateway (open_with_embedder path).
- Generate embeddings for:
  - query text during search (when text present and embedder available)
  - observation ADD
  - observation UPDATE
- Do not generate embeddings for ExactDup or NONE/Skipped outcomes.
- Embedding text format: title + "; " + facts (or equivalent deterministic format).
- Add config validation for embedding provider/model/dimensions and missing required API key env vars.

### C) LLM reconciliation pipeline
- Add a reconciler abstraction in coop-memory (trait/interface), implemented in gateway using Provider::complete_fast.
- Write flow should become:
  1. exact hash dedup -> ExactDup (mention_count++)
  2. find similar candidates (hybrid retrieval; fallback FTS)
  3. if no similar above threshold -> ADD
  4. if similar -> call reconciler and apply decision:
     - ADD: insert new observation
     - UPDATE: update matched observation with merged content + history row + re-embed
     - DELETE: remove stale matched observation + history row, then insert new (or document chosen behavior explicitly)
     - NONE: bump mention_count and return Skipped
- Use integer index mapping (0..n-1) for candidate references in reconciliation prompt; never expose DB IDs to model.
- Require strict JSON reconciliation output and robust parsing/error handling.

### D) History/audit correctness
- Ensure observation_history records all mutation types: ADD/UPDATE/DELETE.
- Ensure old/new fields are populated appropriately for UPDATE/DELETE.

### E) Tests
Add/extend tests covering:
- vector path enabled vs fallback path
- embedding called on ADD/UPDATE and not on ExactDup/NONE
- reconciliation decisions ADD/UPDATE/DELETE/NONE
- hash dedup still works
- trust gating still enforced
- history entries for each mutation type
Use fakes/mocks for embedder and reconciler/provider in unit tests.

### F) Tracing + verification
Add tracing events/spans for:
- embedding requests/results (metadata only, no secret leakage)
- vector candidate retrieval
- reconciliation request/decision/application
- fallback path activation (e.g., vec unavailable -> FTS-only)
Verify via runtime trace:
- run with COOP_TRACE_FILE=traces.jsonl
- confirm expected events/fields appear.

### G) Docs update
Update docs/memory-design.md to reflect the new implemented state (not aspirational):
- what is now implemented
- fallback semantics
- reconciliation behavior
- scoring formula and embedding lifecycle
- known remaining gaps (if any)

## Required validation commands
Run and pass:
- cargo fmt
- cargo build -p coop-gateway
- cargo test -p coop-memory -p coop-gateway
- cargo clippy --all-targets --all-features -- -D warnings

Then run a short traced start and confirm memory-related trace events are present:
- COOP_TRACE_FILE=traces.jsonl cargo run --bin coop -- start

## Deliverable format
At the end provide:
1) concise summary of behavior changes
2) list of modified files
3) test/lint/build/tracing results
4) any follow-up TODOs that remain
