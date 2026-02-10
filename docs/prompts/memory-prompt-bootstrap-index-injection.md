# Memory Prompt Boot-Time DB Index Injection Prompt

You are working in `/root/coop/memory`.

Goal: inject a trust-gated compact DB memory index into the system prompt at turn start, so the agent sees relevant memory candidates without first calling memory tools.

Current baseline:
- PromptBuilder is file-centric.
- DB memory is tool-accessed only.
- Structured retrieval and trust gating already exist in `coop-memory` and `memory_tools`.

---

## Constraints (must follow)

- Follow `AGENTS.md` rules.
- Keep heavy deps out of `coop-core` and `coop-memory`.
- Do not introduce crate cycles (do not add `coop-memory` dependency to `coop-core`).
- Keep trust gating strict.
- Add tracing spans/events for prompt index generation.
- Update config validation when adding config fields.

---

## Implementation plan

### A) Add gateway-side prompt memory index assembly

Implement memory index assembly in `coop-gateway` (not `coop-core`), likely as a helper module, e.g.:
- `crates/coop-gateway/src/memory_prompt_index.rs`

Responsibilities:
- Query DB memory before each turn using trust-accessible stores only.
- Build a compact text block appended to system prompt.
- Keep index token-bounded and deterministic.

Suggested index row shape:
- observation id (for later `memory_get`)
- store/type
- title
- short score/mention metadata
- created/updated time (compact)

Do not include full narratives/facts in the index block.

### B) Add configurable index budget

Extend config (`coop.toml`) with optional memory prompt index settings:

```toml
memory:
  prompt_index:
    enabled: true
    limit: 12
    max_tokens: 1200
```

Add defaults in config structs.

### C) Integrate in `Gateway::build_prompt`

Update prompt build flow:
1. Build existing file-based prompt with `PromptBuilder`.
2. If memory configured and prompt_index enabled:
   - collect accessible stores for trust
   - run memory search for recent/high-signal entries
   - render compact index block
   - append block to final system prompt
3. If trust is public or no accessible stores, skip block entirely.

### D) Enforce trust and token safety

- `full` sees private/shared/social
- `inner` sees shared/social
- `familiar` sees social
- `public` sees none

Ensure max_tokens/limit are respected for the injected block.
If index assembly fails, degrade gracefully to existing prompt (no hard turn failure).

### E) Tracing

Add trace events with fields such as:
- trust level
- accessible store count
- index result count
- index token estimate
- fallback/skipped reason

Console output should remain a superset of JSONL fields.

### F) Tests

Add gateway tests validating:
- full trust prompt includes memory index when data exists
- familiar/public trust excludes private/shared data
- public trust injects no memory index block
- token budget truncates index output as configured
- failures in index assembly do not break turn prompt creation

Use fake data only.

### G) Docs

Update:
- `docs/memory-design.md` (prompt integration section)
- any config docs/examples that mention memory config

Reflect implemented behavior (not aspirational language).

---

## Suggested file touches

- `crates/coop-gateway/src/config.rs`
- `crates/coop-gateway/src/config_check.rs`
- `crates/coop-gateway/src/gateway.rs`
- `crates/coop-gateway/src/memory_prompt_index.rs` (new)
- `crates/coop-gateway/src/main.rs` (if module wiring needed)
- `docs/memory-design.md`

---

## Required validation commands

Run and pass:
- `cargo fmt`
- `cargo build -p coop-gateway`
- `cargo test -p coop-gateway`
- `cargo test -p coop-memory -p coop-gateway`
- `cargo clippy --all-targets --all-features -- -D warnings`

Trace verification:
- `COOP_TRACE_FILE=traces.jsonl cargo run --bin coop -- start`
- confirm prompt-index events and trust-gated fields are present

---

## Deliverable format

At the end provide:
1. concise summary of prompt index injection behavior
2. list of modified files
3. test/build/lint/tracing results
4. known follow-up gaps (if any)
