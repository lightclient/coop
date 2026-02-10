# Memory Embedding Provider Expansion Prompt

You are working in `/root/coop/memory`.

Goal: expand gateway embedding support beyond the current providers and make provider wiring extensible.

Current baseline:
- Embedding calls are gateway-owned (`memory_embedding.rs`)
- Supports `openai` and `voyage`
- `memory.embedding` config validates provider/model/dimensions + API key env

---

## Constraints (must follow)

- Follow `AGENTS.md` rules.
- Keep embedding HTTP/provider code in `coop-gateway` (not `coop-memory`).
- Update `config_check::validate_config` for any new config/provider semantics.
- Add tracing for request/response metadata (no secrets).
- Keep compile-time impact minimal.

---

## Implementation plan

### A) Refactor embedding provider wiring to be extensible

Refactor `memory_embedding.rs` into a provider registry pattern:
- provider enum/registry with per-provider request builder + response parser
- shared HTTP client and validation path
- provider-specific API key env resolution

Keep this lightweight (no new crates).

### B) Add additional providers

Add at least one additional production provider. Preferred set:
- `cohere`
- optional `openai-compatible` mode (custom base URL + API key env)

If implementing `openai-compatible`, extend config safely:

```toml
memory:
  embedding:
    provider: openai-compatible
    model: text-embedding-3-small
    dimensions: 1536
    base_url: https://your-endpoint.example/v1
    api_key_env: OPENAI_COMPAT_API_KEY
```

For fixed providers (e.g. cohere), use deterministic env var mapping in config check.

### C) Strengthen validation

Update config validation to cover:
- supported provider list
- required provider-specific fields
- API key env presence
- dimensions > 0 and reasonable upper bound (prevent accidental huge values)

### D) Tracing and safety

Ensure embedding traces include:
- provider/model
- request text length
- returned dimensions
- status/error class

Never log API keys or full payload content.

### E) Tests

Add/extend tests for:
- provider parsing/normalization
- provider-specific required env behavior
- invalid/missing provider-specific config fields
- response dimension mismatch handling
- openai-compatible field checks (if implemented)

Keep tests deterministic and network-free where possible (unit tests for request/parse paths).

### F) Docs

Update docs to reflect supported providers and config examples:
- `docs/memory-design.md`
- any README/config snippets that mention memory embedding config

---

## Suggested file touches

- `crates/coop-gateway/src/config.rs`
- `crates/coop-gateway/src/config_check.rs`
- `crates/coop-gateway/src/memory_embedding.rs`
- `crates/coop-gateway/src/main.rs` (if wiring changes)
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
- run a short start with embedding config + `COOP_TRACE_FILE=traces.jsonl`
- confirm embedding metadata events exist and secrets are not logged

---

## Deliverable format

At the end provide:
1. concise summary of provider expansion
2. list of modified files
3. test/build/lint/tracing results
4. any provider caveats or remaining TODOs
