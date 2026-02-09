# Memory Follow-up — Master Execution Prompt

You are working in `/root/coop/memory`.

Goal: execute the remaining memory roadmap tasks end-to-end in a controlled sequence, with hard validation gates between phases.

This is an orchestration prompt. It chains existing detailed prompts:

1. `docs/prompts/memory-reconciliation-e2e-validation.md`
2. `docs/prompts/memory-prompt-bootstrap-index-injection.md`
3. `docs/prompts/memory-retention-compression-archive.md`
4. `docs/prompts/memory-embedding-provider-expansion.md`

Run them in exactly this order.

---

## Global constraints (must follow in every phase)

- Follow `AGENTS.md` rules.
- No PII or real credentials in code/tests/docs.
- Keep heavy HTTP/provider deps out of `coop-memory`.
- Any config changes must be validated in `config_check::validate_config`.
- Add tracing spans/events for new major paths and verify via `COOP_TRACE_FILE`.
- Keep files modular (< ~500 lines where practical).
- Use deterministic/fake data in tests.

---

## Execution protocol

For each phase prompt:

### Step 1: Read and restate
- Read the phase prompt fully.
- Restate the phase acceptance criteria before implementing.

### Step 2: Implement
- Make focused, incremental code changes.
- Preserve existing behavior outside scope.
- Keep trust-gating and fallback semantics intact.

### Step 3: Phase-local validation
- Run any phase-specific tests/commands required by that prompt.
- Fix failures before continuing.

### Step 4: Global quality gate (required after every phase)
Run and pass:

- `cargo fmt`
- `cargo build -p coop-gateway`
- `cargo test -p coop-memory -p coop-gateway`
- `cargo clippy --all-targets --all-features -- -D warnings`

If any command fails, stop and fix before moving to the next phase.

### Step 5: Trace verification
For phases that add/modify runtime behavior, run a short traced start:

- `COOP_TRACE_FILE=traces.jsonl cargo run --bin coop -- start`

Then verify expected phase-specific events/fields are present via `rg`/`grep`.

---

## Phase checkpoints

### Phase 1 checkpoint (Reconciliation E2E)
Must be true before phase 2:
- Gateway-level E2E reconciliation coverage exists for ADD/UPDATE/DELETE/NONE/ExactDup.
- Trace evidence exists for reconciliation request/decision/application.

### Phase 2 checkpoint (Prompt bootstrap index)
Must be true before phase 3:
- System prompt includes trust-gated DB memory index when enabled.
- Public trust injects no memory index.
- Prompt token budget enforcement is tested.

### Phase 3 checkpoint (Retention/compression/archive)
Must be true before phase 4:
- Maintenance pipeline runs with retention config.
- Compression/archive behavior is tested.
- Maintenance tracing is present.

### Phase 4 checkpoint (Embedding provider expansion)
Completion criteria:
- Embedding provider wiring is extensible.
- New provider(s) validated with config/env checks.
- Tracing confirms embedding metadata without secret leakage.

---

## Final hardening pass (after all phases)

Run once at the end:

- `cargo fmt`
- `cargo build`
- `cargo test`
- `cargo clippy --all-targets --all-features -- -D warnings`

Then do one final traced runtime check:

- `COOP_TRACE_FILE=traces.jsonl cargo run --bin coop -- start`

Validate that memory-related traces cover:
- embedding request/result metadata
- vector fallback semantics (if vec unavailable)
- reconciliation events
- prompt index injection events
- maintenance pipeline events

---

## Failure policy

- Do not skip gates.
- Do not proceed to next phase with failing tests/lint/build.
- If blocked by external/runtime dependency, document exact blocker and provide the maximal local verification completed.

---

## Final deliverable format

At completion, provide:

1. **Phase-by-phase summary** of behavior changes.
2. **Modified files list**, grouped by phase.
3. **Validation results** (fmt/build/test/clippy + trace verification evidence).
4. **Remaining TODOs** (if any), explicitly scoped and prioritized.
