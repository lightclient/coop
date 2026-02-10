# Add API Key Rotation to Coop

## Goal

Add support for multiple API keys with automatic rotation. Keys rotate **proactively** when approaching rate limits (90% utilization) and **reactively** on 429 errors. The pool always prefers the key whose rate-limit window resets soonest, spreading load across keys that are about to get fresh capacity.

## Anthropic Rate-Limit Headers (Unified System)

Anthropic uses a **unified rate-limit** system. Every response includes headers for multiple time windows with utilization as a 0.0–1.0 float:

```
anthropic-ratelimit-unified-status: allowed
anthropic-ratelimit-unified-reset: 1770685200

anthropic-ratelimit-unified-5h-status: allowed
anthropic-ratelimit-unified-5h-reset: 1770685200
anthropic-ratelimit-unified-5h-utilization: 0.12

anthropic-ratelimit-unified-7d-status: allowed
anthropic-ratelimit-unified-7d-reset: 1771189200
anthropic-ratelimit-unified-7d-utilization: 0.13

anthropic-ratelimit-unified-7d_sonnet-status: allowed
anthropic-ratelimit-unified-7d_sonnet-reset: 1771092000
anthropic-ratelimit-unified-7d_sonnet-utilization: 0.01

anthropic-ratelimit-unified-representative-claim: five_hour
```

Key fields:

- **`*-utilization`**: float 0.0–1.0. `0.12` = 12% of window budget consumed.
- **`*-reset`**: unix epoch seconds when that window resets.
- **`*-status`**: `allowed` or `rejected`.
- **`representative-claim`**: which window is the current bottleneck (`five_hour` or `seven_day`).
- **`unified-reset`**: epoch of the soonest reset across all windows.

On 429 responses, also: `retry-after: <seconds>`.

There are **no** `tokens-remaining` / `requests-remaining` headers. The utilization float is the only capacity signal.

## Config Change

### Before

Keys come from `ANTHROPIC_API_KEY` env var. No config involvement.

```yaml
provider:
  name: anthropic
```

### After

Keys are specified as a list with an `env:` prefix to reference environment variables. This is explicit about the source (env var, not a literal value) and leaves the door open for other sources later (e.g. `file:`, `vault:`).

```yaml
provider:
  name: anthropic
  api_keys:
    - env:ANTHROPIC_API_KEY
    - env:ANTHROPIC_API_KEY_2
    - env:ANTHROPIC_API_KEY_3
```

At startup, each entry is parsed:
- `env:VAR_NAME` — resolve environment variable `VAR_NAME`. If unset, startup fails with a clear error naming the missing var.
- Any entry without a recognized prefix — reject with an error: `"api_keys entry 'xxx' must use 'env:' prefix (e.g. env:ANTHROPIC_API_KEY)"`.

When `api_keys` is omitted or empty, fall back to `ANTHROPIC_API_KEY` env var (current behavior). A single-key pool behaves identically to today.

## Architecture

### RateLimitInfo (per-key state)

Tracks what we know about a key's rate-limit state, updated from response headers on every request.

```rust
struct RateLimitInfo {
    /// Overall status: true = allowed, false = rejected.
    allowed: bool,
    /// Utilization of the binding window (from representative-claim).
    /// 0.0–1.0 float. None if we haven't seen headers yet.
    utilization: Option<f64>,
    /// Which window is the bottleneck ("five_hour", "seven_day", etc.).
    representative_claim: Option<String>,
    /// Unix epoch when the binding window resets
    /// (from anthropic-ratelimit-unified-reset).
    reset_epoch: Option<u64>,
    /// Hard cooldown: don't use this key until this instant.
    /// Set from `retry-after` header on 429.
    cooldown_until: Option<Instant>,
}
```

We intentionally track only the **unified/binding** values, not every individual window. The `representative-claim` header tells us which window is the bottleneck, and `unified-reset` gives us the soonest reset. This keeps the data model simple — Anthropic already does the multi-window math for us.

### KeyPool (new, in `coop-agent`)

Create `crates/coop-agent/src/key_pool.rs`.

```rust
pub struct KeyPool {
    keys: Vec<KeyEntry>,
}

struct KeyEntry {
    value: String,
    is_oauth: bool,
    rate_limits: RwLock<RateLimitInfo>,
}
```

**Key selection: `best_key(&self) -> usize`**

Choose the best key for the next request. This always returns a valid index — it never refuses to pick a key.

Selection logic:

1. **Exclude keys on hard cooldown** (`cooldown_until > now`).
2. Among remaining keys, **partition into "comfortable" (utilization < 0.90 or unknown) and "hot" (≥ 0.90).**
3. If there are comfortable keys: among them, **pick the one whose `reset_epoch` is soonest** (closest to getting fresh capacity). If `reset_epoch` is unknown, treat as `u64::MAX` (least preferred among comfortable keys).
4. If all non-cooldown keys are hot: **pick the one with the lowest utilization** (most remaining headroom). Ties broken by soonest `reset_epoch`.
5. If all keys are on cooldown: **pick the one whose `cooldown_until` is soonest** (will become available first).

**The 90% threshold is a soft preference, never a hard block.** When all keys are above 90%, the pool picks the best available and continues. The system must never refuse to make a request.

**`update_from_headers(&self, key_index: usize, headers: &HeaderMap)`**

Called after every response (success or error). Parses:

- `anthropic-ratelimit-unified-status` → `allowed` (`"allowed"` = true, else false)
- `anthropic-ratelimit-unified-reset` → `reset_epoch` (parse as `u64`)
- `anthropic-ratelimit-unified-representative-claim` → `representative_claim`
- The utilization for the representative claim: if `representative_claim` is `"five_hour"`, read `anthropic-ratelimit-unified-5h-utilization`. If `"seven_day"`, read `anthropic-ratelimit-unified-7d-utilization`. Store as `utilization`.
- `retry-after` → `cooldown_until` (`Instant::now() + Duration::from_secs(value)`)

If a header is missing or unparseable, leave the corresponding field unchanged.

**Mapping `representative_claim` to utilization header:**

```
"five_hour" → anthropic-ratelimit-unified-5h-utilization
"seven_day" → anthropic-ratelimit-unified-7d-utilization
```

If the representative claim names a model-specific window like `"seven_day_sonnet"`, look for `anthropic-ratelimit-unified-7d_sonnet-utilization`. Use a simple mapping function. If no match, fall back to reading all `*-utilization` headers and take the highest.

**`mark_rate_limited(&self, key_index: usize, retry_after_secs: u64)`**

Sets `cooldown_until = Instant::now() + Duration::from_secs(retry_after_secs)` and `allowed = false`.

**`is_near_limit(&self, key_index: usize) -> bool`**

Returns true if `utilization >= 0.90`. Returns false if utilization is unknown.

**`on_cooldown(&self, key_index: usize) -> bool`**

Returns true if `cooldown_until > now`.

**`get(&self, index: usize) -> (&str, bool)`** — returns `(api_key_value, is_oauth)`.

**`len(&self)`** — number of keys.

All methods are `&self`. Use `RwLock<RateLimitInfo>` per key.

### AnthropicProvider Changes

Modify `crates/coop-agent/src/anthropic_provider.rs`:

**1. Replace `api_key: String` and `is_oauth: bool` with `keys: KeyPool`.**

**2. Constructors:**
- `new(api_keys: Vec<String>, model: &str)` — takes resolved key values. Each key auto-detects OAuth.
- `from_env(model: &str)` — reads `ANTHROPIC_API_KEY`, creates single-key pool (backward compat).
- `from_key_refs(key_refs: &[String], model: &str)` — parses each `env:VAR_NAME` entry, resolves the env var, errors if any are missing or have an unrecognized prefix.

**3. `build_request` takes key value + is_oauth** rather than reading from `self`.

**4. `send_with_retry` — new flow:**

```
loop (up to MAX_RETRIES total attempts):
    key_index = pool.best_key()
    (key_value, is_oauth) = pool.get(key_index)
    response = build_request(body, has_tools, key_value, is_oauth).send()

    // Always update rate-limit state from response headers
    pool.update_from_headers(key_index, response.headers())

    if success:
        if pool.is_near_limit(key_index):
            info!(key_index, utilization, "key approaching rate limit, will rotate on next request")
        return response

    if 429 rate_limit_error:
        retry_after = parse retry-after header, default 60s
        pool.mark_rate_limited(key_index, retry_after)

        next_key = pool.best_key()
        if next_key != key_index and !pool.on_cooldown(next_key):
            info!(old_key = key_index, new_key = next_key, "rate-limited, rotated key")
            continue  // retry immediately with new key — no backoff
        else:
            warn!("all keys rate-limited, waiting {retry_after}s")
            sleep(Duration::from_secs(retry_after))
            continue

    if 429 (overloaded, not rate_limit) or 500/502/503:
        exponential backoff (existing behavior)
        continue

    if non-retryable error:
        bail
```

**5. Header access for streaming.** The initial response to a stream request carries the same rate-limit headers. `send_with_retry` already returns the response — call `update_from_headers` before returning.

**6. Tracing:** Add `key_index` and `key_count` fields to the `anthropic_request` span. Log rotation at `info!` with utilization values. Log header updates at `debug!`. **Never log actual key values.**

### Config Changes

In `crates/coop-gateway/src/config.rs`, add `api_keys` to `ProviderConfig`:

```rust
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub(crate) struct ProviderConfig {
    #[serde(default = "default_provider")]
    pub name: String,
    /// Key references with `env:` prefix (e.g. `env:ANTHROPIC_API_KEY`).
    /// Enables rotation on rate limits. When empty/omitted, falls back
    /// to ANTHROPIC_API_KEY env var.
    #[serde(default)]
    pub api_keys: Vec<String>,
}
```

### Config Check Changes

In `crates/coop-gateway/src/config_check.rs`, update the `api_key_present` check:

- If `provider.api_keys` is non-empty:
  - Validate every entry has the `env:` prefix. Report entries without it as errors.
  - Check that every referenced env var is set. Report each missing one as a separate error.
  - Add info check: `"API keys: 3 configured (rotation enabled)"`.
- If `provider.api_keys` is empty: check `ANTHROPIC_API_KEY` (unchanged).

### main.rs Changes

Three call sites that create the provider (`AnthropicProvider::from_env`):

- If `config.provider.api_keys` is non-empty → `AnthropicProvider::from_key_refs(&config.provider.api_keys, &config.agent.model)`
- Otherwise → `AnthropicProvider::from_env(&config.agent.model)` (unchanged)

## Files to Create

| File | Purpose | Est. Lines |
|---|---|---|
| `crates/coop-agent/src/key_pool.rs` | `KeyPool`, `RateLimitInfo`, header parsing, key selection, tests | ~350 |

## Files to Modify

| File | Change |
|---|---|
| `crates/coop-agent/src/lib.rs` | Add `mod key_pool; pub use key_pool::KeyPool;` |
| `crates/coop-agent/src/anthropic_provider.rs` | Replace single key with `KeyPool`, rotation in `send_with_retry`, update from headers, new constructors |
| `crates/coop-gateway/src/config.rs` | Add `api_keys: Vec<String>` to `ProviderConfig` |
| `crates/coop-gateway/src/config_check.rs` | Update `api_key_present` to validate `env:` prefix and env vars |
| `crates/coop-gateway/src/main.rs` | Use `from_key_refs` when `api_keys` configured (3 call sites) |

## Tests

### `crates/coop-agent/src/key_pool.rs` (unit tests)

1. **single key pool** — `best_key()` always returns 0.
2. **prefers soonest reset among comfortable keys** — 3 keys all < 90%, different `reset_epoch`, picks soonest.
3. **skips near-limit keys** — key 0 at 95% utilization, key 1 at 50%, `best_key()` returns 1.
4. **skips cooldown keys** — key 0 on cooldown, key 1 fine, `best_key()` returns 1.
5. **all keys hot picks lowest utilization** — key 0 at 92%, key 1 at 95%, `best_key()` returns 0 (lower utilization). **This confirms the system continues past 90%.**
6. **all keys hot tiebreak by soonest reset** — both at 92%, different `reset_epoch`, picks soonest.
7. **all keys on cooldown picks soonest** — picks the one whose `cooldown_until` is earliest.
8. **cooldown expires** — mark key with 0s cooldown, key becomes available.
9. **update_from_headers parses unified headers** — build a `HeaderMap` with real Anthropic header names and values, verify fields.
10. **update_from_headers maps representative claim to utilization** — set `representative-claim: five_hour`, verify it reads `5h-utilization`.
11. **update_from_headers retry-after sets cooldown** — verify `cooldown_until` is set.
12. **update_from_headers ignores missing** — empty `HeaderMap` doesn't clear existing state.
13. **is_near_limit thresholds** — 0.89 → false, 0.90 → true, 1.0 → true.
14. **unknown utilization treated as comfortable** — fresh key with no headers yet preferred over a near-limit key.
15. **oauth detection** — `sk-ant-oat` keys identified as OAuth, `sk-ant-api` as standard.

### `crates/coop-gateway/src/config.rs`

16. **parse config with api_keys** — YAML with `api_keys: [env:KEY_A, env:KEY_B]` deserializes correctly.
17. **parse config without api_keys** — existing configs get empty vec (backward compat).

### `crates/coop-gateway/tests/` or `config_check` tests

18. **config check rejects missing env: prefix** — `api_keys: [ANTHROPIC_API_KEY]` (no prefix) flagged as error with helpful message.
19. **config check reports all missing env vars** — `api_keys: [env:MISSING_1, env:MISSING_2]`, both flagged.
20. **config check passes when all env vars set** — set env vars, verify pass.

### `crates/coop-agent/src/anthropic_provider.rs` or `key_pool.rs`

21. **from_key_refs parses env: prefix** — `["env:VAR"]` resolves correctly when var is set.
22. **from_key_refs rejects unknown prefix** — `["vault:secret"]` errors with clear message.
23. **from_key_refs rejects bare names** — `["ANTHROPIC_API_KEY"]` errors suggesting `env:` prefix.

### Existing tests

All existing `AnthropicProvider` tests must continue to pass. The single-key path must be behaviorally identical to today.

## Constraints

- **No secrets in config.** `api_keys` contains `env:`-prefixed references, never raw key values.
- **OAuth keys in the pool.** Each key independently detects OAuth. Mixed pools (some OAuth, some standard) work — `build_request` uses the current key's `is_oauth`.
- **Backward compatible.** Omitting `api_keys` = single key from `ANTHROPIC_API_KEY`, identical to today.
- **Never blocks.** `best_key()` always returns an index. 90% is a preference, not a gate.
- **No new deps.** Reset epochs are already unix seconds — just parse as `u64`. Utilization is a float — parse with `str::parse::<f64>()`. Use `std::sync::RwLock`, `std::time::{Instant, Duration, SystemTime}`. No chrono, no parking_lot.
- **Compile time.** `key_pool.rs` is a small file in `coop-agent` (leaf crate). No impact on `coop-core`. Verify: `touch crates/coop-gateway/src/main.rs && time cargo build` < 1.5s.
- **Tracing.** Log rotation events at `info!` with utilization + key index. Log header updates at `debug!`. Include `key_index` and `key_count` in request spans. **Never log actual key values.**
- **Proactive rotation is passive.** `best_key()` is called at the start of each request. If the current key is near its limit, the next request naturally picks a better key. No background threads, no timers.

## Dev Loop

```bash
# 1. Create key_pool.rs with KeyPool, RateLimitInfo, header parsing + tests
cargo test -p coop-agent

# 2. Modify anthropic_provider.rs to use KeyPool
cargo test -p coop-agent

# 3. Update config.rs, config_check.rs, main.rs
cargo test -p coop-gateway

# 4. Full check
cargo fmt
cargo clippy --all-targets --all-features -- -D warnings
cargo test
```
