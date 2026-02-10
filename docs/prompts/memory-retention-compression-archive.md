# Memory Retention / Compression / Archive Pipeline Prompt

You are working in `/root/coop/memory`.

Goal: implement lifecycle maintenance for memory data:
1) retention policy
2) deterministic compression of stale observations
3) archival of aged data

Current baseline:
- observations can expire (`expires_at`)
- no scheduled retention/compression/archive pipeline yet

---

## Constraints (must follow)

- Follow `AGENTS.md` rules.
- Keep provider/HTTP dependencies out of `coop-memory`.
- Any config changes must be validated in `config_check::validate_config`.
- New major async/public paths require tracing spans/events.
- Trust boundaries must remain intact.

---

## Scope and behavior

Implement a first practical pipeline (deterministic, no LLM dependency required):

### A) Retention config

Extend `memory` config with retention settings and sensible defaults:

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

Validate all fields in `config_check`.

### B) Archive storage

Add archive table(s) in `coop-memory` schema, e.g.:
- `observation_archive`

Archive row should preserve enough fields for auditing:
- original observation id
- store/type/title/narrative/facts/tags/source
- related files/people
- mention_count/token_count/hash
- created_at/updated_at/archived_at
- archive_reason

### C) Compression pass

Add deterministic compression pass for stale clusters:
- identify stale observations older than `compress_after_days`
- cluster by stable keys (e.g. `store + type + normalized title`)
- if cluster size >= `compression_min_cluster_size`:
  - create one synthesized summary observation
  - expire originals
  - write history entries for compressed/deactivated rows

The summary format should be deterministic and explain source count.
Do not use model-generated synthesis in this phase.

### D) Archive pass

Archive observations older than `archive_after_days` (and/or expired beyond threshold):
- copy into archive table
- delete from active observations
- keep referential consistency (history can remain by observation_id if schema already permits, or archive related history if needed)

### E) Archive cleanup pass

Delete archive rows older than `delete_archive_after_days`.

### F) Pipeline runner

Add a memory maintenance entrypoint callable from gateway on a schedule:
- run at startup once
- then periodic interval (e.g. hourly) in gateway task loop
- bounded work using `max_rows_per_run`

### G) Tracing

Add tracing for each stage:
- maintenance run start/end
- candidates scanned
- compressed rows count
- archived rows count
- archive cleanup count
- durations/errors

JSONL traces should make maintenance behavior auditable.

### H) Tests

Add/extend tests for:
- config validation for retention settings
- compression creates summary + expires originals
- archive move removes active rows and keeps archive copy
- archive cleanup deletes old archive rows
- bounded `max_rows_per_run` behavior
- scheduled runner triggers maintenance without crashing

Use fake data only.

### I) Docs

Update `docs/memory-design.md`:
- retention/compression/archive lifecycle behavior
- config knobs and defaults
- known limitations

---

## Suggested file touches

- `crates/coop-gateway/src/config.rs`
- `crates/coop-gateway/src/config_check.rs`
- `crates/coop-gateway/src/main.rs` and/or scheduler wiring
- `crates/coop-memory/src/types.rs` (if maintenance types needed)
- `crates/coop-memory/src/traits.rs` (if maintenance API added)
- `crates/coop-memory/src/sqlite/schema.rs`
- `crates/coop-memory/src/sqlite/write_ops.rs` and/or new maintenance module(s)
- `crates/coop-memory/src/sqlite/tests.rs` or dedicated maintenance tests
- `docs/memory-design.md`

---

## Required validation commands

Run and pass:
- `cargo fmt`
- `cargo build -p coop-gateway`
- `cargo test -p coop-memory -p coop-gateway`
- `cargo clippy --all-targets --all-features -- -D warnings`

Trace verification:
- `COOP_TRACE_FILE=traces.jsonl cargo run --bin coop -- start`
- confirm memory maintenance events/fields appear

---

## Deliverable format

At the end provide:
1. concise summary of lifecycle pipeline behavior
2. list of modified files
3. test/build/lint/tracing results
4. any remaining TODOs (e.g. future LLM-based compression)
