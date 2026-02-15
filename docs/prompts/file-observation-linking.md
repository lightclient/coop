# File–Observation Linking

You are working in `/root/coop/main`.

Goal: make the `related_files` field on observations actually useful by adding query-time file lookups, a dedicated tool, and prompt-time file context injection. Today `related_files` is written to the DB but never queried — it's dead metadata. This prompt turns it into a live bidirectional link between the filesystem and structured memory.

---

## Motivation

Observations already store `related_files: Vec<String>` (populated by both `memory_write` and auto-capture). But there is no way to:

1. **Find observations for a file** — "what do I know about `src/main.rs`?"
2. **Check file existence** — do the referenced paths still exist on disk?
3. **Show related knowledge when working on a file** — when the agent opens a file, inject relevant observations into context.

This prompt adds all three capabilities without schema migrations, heavy deps, or compile-time regressions.

---

## Constraints

- Follow `AGENTS.md` rules (no PII, `cargo fmt`, clippy clean, tracing, <500 LOC files).
- No new crates. All changes land in `coop-memory` and `coop-gateway`.
- No schema migrations — `related_files` is already stored as a JSON text column. Use SQL JSON functions (`json_each`) for indexed lookups.
- No new heavy dependencies. `rusqlite` already has the `bundled` feature which includes JSON1.
- Keep incremental build time under targets (`<1s` leaf, `<1.5s` root).
- Any config additions must update `config_check::validate_config`.
- Add tracing spans/events for new paths and verify with `COOP_TRACE_FILE`.

---

## Implementation Plan

### A) SQL index for file lookups

Add an index that enables efficient queries against the JSON `related_files` column.

**Schema change** (in `schema::init_schema`):

```sql
-- Generated column + index for file path lookups.
-- This requires no data migration — SQLite JSON1 json_each() works
-- on the existing TEXT column that stores JSON arrays.
CREATE INDEX IF NOT EXISTS idx_obs_files ON observations(agent_id)
    WHERE related_files != '[]';
```

This lightweight partial index filters out the majority of rows (those with no related files) so the `json_each` query below only scans rows that actually have file references.

**Query pattern** for "find observations referencing a path":

```sql
SELECT o.id, o.title, o.type, o.store, o.created_at, o.updated_at,
       o.token_count, o.mention_count, o.related_people, 0.0 AS fts_score
FROM observations o, json_each(o.related_files) AS f
WHERE o.agent_id = ?
  AND (o.expires_at IS NULL OR o.expires_at > ?)
  AND f.value = ?
ORDER BY o.updated_at DESC
LIMIT ?
```

Support prefix matching too (all observations under a directory):

```sql
  AND f.value LIKE ? || '%'
```

Add a method to `SqliteMemory`:

```rust
fn search_by_file(
    &self,
    path: &str,
    prefix_match: bool,
    limit: usize,
) -> Result<Vec<RawIndex>>
```

Place this in a new file `crates/coop-memory/src/sqlite/file_query.rs` to keep `query.rs` focused.

### B) Memory trait extension

Add a method to the `Memory` trait in `crates/coop-memory/src/traits.rs`:

```rust
async fn search_by_file(
    &self,
    path: &str,
    prefix_match: bool,
    limit: usize,
) -> Result<Vec<ObservationIndex>>;
```

Provide a default impl that returns `Ok(Vec::new())` so existing fakes/tests don't break.

Implement it on `SqliteMemory` using the SQL from part A. Trust gating is the caller's responsibility (same as `search`/`timeline`).

### C) `memory_files` tool

Add a new tool in `crates/coop-gateway/src/memory_tools.rs` (or a new `memory_file_tools.rs` if `memory_tools.rs` is approaching 500 lines):

```json
{
  "name": "memory_files",
  "description": "Find observations linked to a file path. Supports exact match or directory prefix.",
  "input_schema": {
    "type": "object",
    "properties": {
      "path": {
        "type": "string",
        "description": "File path to search (e.g. 'src/main.rs' or 'crates/coop-gateway/')"
      },
      "prefix": {
        "type": "boolean",
        "description": "If true, match all files under this directory prefix"
      },
      "limit": {
        "type": "integer",
        "minimum": 1,
        "maximum": 50
      },
      "check_exists": {
        "type": "boolean",
        "description": "If true, check which referenced files still exist on disk"
      }
    },
    "required": ["path"]
  }
}
```

**Behavior:**

1. Call `memory.search_by_file(path, prefix, limit)`.
2. Apply trust gating (filter by `accessible_stores(ctx.trust)`).
3. If `check_exists` is true, for each observation in results, check which of its `related_files` exist on disk (resolve relative to `ctx.workspace`). Add an `exists: bool` flag per file in the output.
4. Return compact JSON:

```json
{
  "count": 3,
  "path": "crates/coop-gateway/src/gateway.rs",
  "results": [
    {
      "id": 42,
      "title": "Gateway refactor to async channels",
      "type": "decision",
      "store": "shared",
      "created": "2026-02-01",
      "files": [
        {"path": "crates/coop-gateway/src/gateway.rs", "exists": true},
        {"path": "crates/coop-gateway/src/old_router.rs", "exists": false}
      ]
    }
  ]
}
```

**Tracing:** `instrument` the handler. Log `path`, `prefix`, `result_count`, `stale_file_count` (files referenced but not on disk).

### D) Extend `memory_search` with optional file filter

Add an optional `file` parameter to `memory_search`:

```json
"file": {
  "type": "string",
  "description": "Filter results to observations referencing this file path"
}
```

When `file` is set, post-filter `memory_search` results to only those whose `related_files` contains the given path. This is a lightweight client-side filter on the existing search results (no new SQL path needed for the combined query — keep it simple).

### E) Prompt-time file context injection

When the prompt index is built (`memory_prompt_index.rs`), and the user's message references specific files (detected by path-like patterns), automatically enrich the memory index with observations linked to those files.

**Detection heuristic** (add to `memory_prompt_index.rs`):

```rust
fn extract_file_paths(input: &str) -> Vec<String> {
    // Match patterns like: src/main.rs, crates/foo/bar.rs, ./config.toml
    // Simple regex: sequences containing '/' or '.' that look like paths
    // Filter to paths with file extensions or ending in '/'
}
```

**Enrichment flow** (in `build_prompt_index`):

1. Extract file paths from user input.
2. For each path, call `memory.search_by_file(path, false, 5)`.
3. Merge file-linked results into the existing results (dedup by id, same as relevance merge).
4. Render as a separate sub-section in the prompt index:

```
## Memory Index (DB)
Use memory_get with observation IDs for full details.
- id=42 store=shared type=decision title=Gateway refactor score=0.85 mentions=3 created=2026-02-01
- ...

### File-linked observations
- id=42 files=[gateway.rs] title=Gateway refactor
- id=57 files=[gateway.rs, router.rs] title=Router extraction decision
```

This keeps the file-linked section lightweight (title + file list only) and lets the agent `memory_get` for full details.

**Config:** Add an `include_file_links` boolean to `MemoryPromptIndexConfig` (default: `true`). Validate in `config_check`.

### F) Auto-capture file path normalization

The auto-capture extraction prompt (`memory_auto_capture.rs`) already instructs the model to populate `related_files`. Improve quality:

1. In `to_new_observation`, normalize file paths: strip leading `./`, collapse `..` segments, ensure consistent forward slashes.
2. Add a helper `fn normalize_file_path(path: &str) -> String` in `coop-memory/src/types.rs` (it's a pure function, no deps).
3. Call it in both `memory_auto_capture.rs` (auto-capture write) and `memory_tools.rs` (manual `memory_write`), and in the `memory_files` tool query path, so lookups match regardless of how the path was originally written.

```rust
pub fn normalize_file_path(path: &str) -> String {
    let path = path.trim();
    let path = path.strip_prefix("./").unwrap_or(path);
    // Normalize backslashes to forward slashes
    let path = path.replace('\\', "/");
    // Collapse consecutive slashes
    let mut result = String::with_capacity(path.len());
    let mut prev_slash = false;
    for ch in path.chars() {
        if ch == '/' {
            if !prev_slash {
                result.push(ch);
            }
            prev_slash = true;
        } else {
            result.push(ch);
            prev_slash = false;
        }
    }
    // Strip trailing slash for files (keep for directories if it had one)
    result
}
```

---

## Tests

### Unit tests (`crates/coop-memory/src/sqlite/tests.rs` or new test file)

1. **`search_by_file_exact_match`** — Write observations with known `related_files`, query by exact path, verify correct results returned.
2. **`search_by_file_prefix_match`** — Write observations with files under `crates/coop-gateway/`, query with prefix `crates/coop-gateway/`, verify all matched.
3. **`search_by_file_respects_expiry`** — Expired observations should not appear in file search results.
4. **`search_by_file_empty_results`** — Query for a path with no observations, verify empty result.
5. **`normalize_file_path_cases`** — Test `./foo/bar.rs` → `foo/bar.rs`, `foo//bar.rs` → `foo/bar.rs`, backslashes, trailing slashes.

### Integration tests (`crates/coop-gateway/tests/`)

6. **`memory_files_tool_trust_gating`** — Verify that `memory_files` tool respects trust levels (familiar trust can't see private store observations).
7. **`memory_files_tool_check_exists`** — Write observations with file paths, some existing and some not. Verify `check_exists` flags are correct.
8. **`memory_search_file_filter`** — Verify `memory_search` with `file` parameter filters correctly.
9. **`prompt_index_file_enrichment`** — Verify that when user input contains a file path, the prompt index includes file-linked observations.

Use deterministic fake data (Alice/Bob names, `src/main.rs` / `src/lib.rs` paths).

---

## Validation protocol

After implementation, run in order:

```bash
cargo fmt
cargo build
cargo test -p coop-memory
cargo test -p coop-gateway
cargo clippy --all-targets --all-features -- -D warnings
```

Then verify incremental build time:

```bash
touch crates/coop-gateway/src/main.rs && time cargo build
# Must be < 1.5s
```

Trace verification:

```bash
COOP_TRACE_FILE=traces.jsonl cargo run --bin coop -- start &
# Send a message referencing a file path
# Then:
grep "memory_files" traces.jsonl
grep "file_linked_observations" traces.jsonl
kill %1
```

Expected trace events:
- `memory_files` tool execution with path/prefix/result_count fields
- `file_linked_observations` in prompt index build with path_count/observation_count fields

---

## File change summary

| File | Change |
|------|--------|
| `crates/coop-memory/src/sqlite/schema.rs` | Add partial index on `related_files` |
| `crates/coop-memory/src/sqlite/file_query.rs` | **New.** `search_by_file` SQL implementation |
| `crates/coop-memory/src/sqlite/mod.rs` | Add `mod file_query`, wire `Memory::search_by_file` |
| `crates/coop-memory/src/traits.rs` | Add `search_by_file` with default impl |
| `crates/coop-memory/src/types.rs` | Add `normalize_file_path` helper |
| `crates/coop-gateway/src/memory_tools.rs` | Add `memory_files` tool, `file` filter on `memory_search` |
| `crates/coop-gateway/src/memory_prompt_index.rs` | File path extraction + file-linked enrichment |
| `crates/coop-gateway/src/memory_auto_capture.rs` | Normalize file paths before write |
| `crates/coop-gateway/src/config.rs` | Add `include_file_links` to prompt index config |
| `crates/coop-gateway/src/config_check.rs` | Validate new config field |
| `crates/coop-memory/src/sqlite/tests.rs` | New test cases for file search |
| `crates/coop-gateway/tests/` | Integration tests for tool + prompt enrichment |
