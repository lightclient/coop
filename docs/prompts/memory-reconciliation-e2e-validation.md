# Memory Reconciliation E2E Validation Prompt

You are working in `/root/coop/memory`.

Goal: add end-to-end integration coverage for the memory reconciliation pipeline through real gateway turns/tool traffic (not just unit tests in `coop-memory`).

Current baseline:
- Reconciliation exists in `coop-memory` (`ADD/UPDATE/DELETE/NONE`)
- Gateway wires reconciler via `Provider::complete_fast`
- Unit tests cover write-path behavior directly

This task is specifically about validating the full stack path:
`Gateway turn -> memory_write tool -> SqliteMemory::write -> reconciler -> DB mutation/history -> tool output`

---

## Constraints (must follow)

- Follow `AGENTS.md` rules.
- Keep test data fake-only (`alice`, `bob`, etc.).
- Keep files modular (< ~500 LOC where practical).
- New async/public gateway paths should include tracing spans/events.
- Do not add heavy deps.

---

## Implementation plan

### A) Add a gateway integration test module for memory reconciliation

Create a gateway-focused integration test suite (prefer `crates/coop-gateway/tests/`), e.g.:
- `crates/coop-gateway/tests/memory_reconciliation_e2e.rs`

Test harness requirements:
- Use a real `Gateway` instance.
- Use `SqliteMemory` with temp DB.
- Register `MemoryToolExecutor` in the executor chain.
- Use a scripted provider fake that supports both:
  - `complete()` for turn responses/tool calls
  - `complete_fast()` for reconciliation JSON decisions

### B) Build a scripted provider test double with dual channels

Implement a local test provider that can queue:
- assistant turn outputs (for `complete`) that call `memory_write`
- reconciliation outputs (for `complete_fast`) as strict JSON

This lets tests deterministically drive each decision path from the gateway.

### C) Add E2E scenarios

Add tests that run turns and assert persisted outcomes:

1. **ADD path**
   - First memory_write observation
   - Assert tool result reports `added`
   - Assert DB/history has `ADD`

2. **UPDATE path**
   - Seed candidate via prior turn
   - Reconciliation returns UPDATE with merged content
   - Assert tool result reports `updated`
   - Assert row mutated and history has `UPDATE`

3. **DELETE path**
   - Seed candidate via prior turn
   - Reconciliation returns DELETE
   - Assert old row stale/inaccessible (`expires_at` behavior)
   - Assert replacement row exists
   - Assert history has `DELETE`

4. **NONE path**
   - Seed candidate via prior turn
   - Reconciliation returns NONE
   - Assert mention_count bump
   - Assert tool result reports `skipped`
   - Assert no new row inserted

5. **ExactDup short-circuit**
   - Same title+facts twice
   - Assert `exact_dup`
   - Assert reconciler was not invoked

### D) Validate trust-gated behavior in the full path

Add one integration test where caller trust cannot access requested store:
- memory_write should fail before DB write
- ensure no reconciliation call

### E) Trace-driven verification in tests

Ensure E2E tests verify memory tracing fields/events for:
- reconciliation request/decision/application
- vector fallback activation when applicable
- embedding request metadata (if embedding enabled in test)

Use `COOP_TRACE_FILE` in test runtime or test-scoped trace subscriber and assert expected event messages/fields are present.

---

## Suggested file touches

- `crates/coop-gateway/tests/memory_reconciliation_e2e.rs` (new)
- (optional) `crates/coop-gateway/tests/support/...` helper module(s)
- minimal updates in gateway/memory code only if missing test hooks or trace fields

---

## Required validation commands

Run and pass:

- `cargo fmt`
- `cargo build -p coop-gateway`
- `cargo test -p coop-gateway --test memory_reconciliation_e2e`
- `cargo test -p coop-memory -p coop-gateway`
- `cargo clippy --all-targets --all-features -- -D warnings`

Then run one traced test execution and verify memory reconciliation events exist:

- `COOP_TRACE_FILE=traces.jsonl cargo test -p coop-gateway --test memory_reconciliation_e2e -- --nocapture`
- confirm expected trace lines for reconciliation request/decision/application

---

## Deliverable format

At the end provide:
1. concise summary of E2E coverage added
2. list of modified files
3. test/build/lint/tracing results
4. any remaining gaps in reconciliation observability
