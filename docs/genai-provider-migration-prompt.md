You are working in /root/coop/openai.

Replace Coop’s current API provider system with a genai-backed provider layer, without regressing existing user-visible behavior.

High-level goal
- Replace the current direct provider implementation(s), especially the Anthropic-specific stack, with a genai-based provider system.
- Preserve Coop’s existing Provider trait and gateway behavior as much as possible.
- Add support for multiple backends through genai, including at minimum:
  - anthropic
  - openai
  - openai-compatible
  - ollama
- Do not lose important existing features. If genai does not natively support something we already support, implement that feature in a wrapper layer around genai rather than dropping it.

Read first
- crates/coop-core/src/traits.rs
- crates/coop-core/src/types.rs
- crates/coop-core/src/prompt.rs
- crates/coop-agent/src/anthropic_provider.rs
- crates/coop-agent/src/key_pool.rs
- crates/coop-agent/src/lib.rs
- crates/coop-gateway/src/main.rs
- crates/coop-gateway/src/config.rs
- crates/coop-gateway/src/config_check.rs
- crates/coop-gateway/src/provider_registry.rs
- crates/coop-gateway/src/init.rs
- crates/coop-gateway/src/service.rs
- docs/compile-times.md

Also inspect genai usage patterns/examples for:
- auth resolvers
- service target resolvers
- tool use
- tool-use streaming
- cache control / prompt caching
- usage capture
- OpenAI Responses / codex model support

Repo constraints
- Follow /root/coop/openai/AGENTS.md exactly.
- Do not add heavy dependencies to coop-core.
- Keep compile times in mind; add genai only where needed, ideally coop-agent.
- Split large modules. Do not create new 1000+ line files. Aim for focused modules under ~500 lines.
- Use anyhow::Result.
- Use tracing spans/events on public async provider code.
- Update config_check for any new config error modes.
- Run cargo fmt.
- Run cargo clippy --all-targets --all-features -- -D warnings.
- Verify tracing with COOP_TRACE_FILE=traces.jsonl for the changed provider paths.

Non-negotiable feature preservation
1. Coop prompt caching behavior must continue to work.
   - Preserve the existing intent of stable/session/volatile prompt structure and cache breakpoints.
   - Existing prompt builder semantics in coop-core stay the source of truth.
   - Anthropic prompt caching behavior must not regress.
   - Preserve usage accounting for cache read/write tokens where available.

2. Key rotation must not be lost.
   - Preserve support for provider.api_keys = ["env:..."].
   - Keep rotating keys proactively/reactively where possible.
   - If genai does not expose enough header metadata for our current logic, implement a thin wrapper around genai so rotation still works.
   - Do not delete current key rotation logic unless replacement is equivalent or better.
   - At minimum preserve current Anthropic rotation quality and add reasonable rotation behavior for OpenAI/OpenAI-compatible providers.

3. Image downsampling must not be lost.
   - Refactor existing image prep/downscaling logic out of the current Anthropic provider into reusable modules.
   - Apply provider-specific image size/dimension policies before handing content to genai.
   - Preserve current Anthropic image protections and logging.
   - If another provider has different limits, make them explicit and configurable in code.

4. Streaming/tool behavior must not regress.
   - Preserve non-streaming complete()
   - Preserve streaming stream()
   - Preserve tool request/result roundtripping
   - Preserve partial text delta streaming for the gateway
   - Preserve final usage capture
   - Preserve handling of reasoning/thinking content if available; it is acceptable to skip provider-specific thinking blocks as before, but do not break on them.

5. Current Anthropic users must not be broken.
   - Existing anthropic config should continue to work.
   - If genai does not support a current Anthropic-specific behavior we rely on (for example custom OAuth/subscription conventions, custom headers, query params, tool name prefixing, or identity/system shaping), implement that behavior around genai rather than removing it.
   - If full parity is impossible entirely inside genai, keep a narrow compatibility shim in coop-agent until parity is achieved.

Design requirements
A. Keep Coop’s Provider trait stable if possible.
- Preserve:
  - name()
  - model_info()
  - complete()
  - stream()
  - supports_streaming()
  - set_model()
  - complete_fast()
- Keep ProviderRegistry using Arc<dyn Provider>.

B. Introduce a genai-backed provider module layout in coop-agent.
Suggested split:
- genai_provider.rs (top-level Provider impl)
- model_mapping.rs
- auth_rotation.rs
- image_prep.rs
- usage_mapping.rs
- message_mapping.rs
- stream_mapping.rs
- anthropic_compat.rs (only if needed)
- openai_compat.rs (only if needed)

C. Use genai’s hooks rather than fighting it.
- Prefer AuthResolver for per-request auth/key selection.
- Prefer ServiceTargetResolver for endpoint/base URL overrides and OpenAI-compatible/local targets.
- Use genai tool and streaming APIs for normalized tool behavior.
- Use genai usage capture features where available.
- Use genai cache-control features where they are sufficient.
- Where genai falls short, add wrapper logic around it rather than forking coop-core abstractions.

D. Preserve backward compatibility in config.
- Existing anthropic configs must continue to load.
- Extend provider config to support multiple backends cleanly.
- Support at least:
  - [provider] name = "anthropic"
  - [provider] name = "openai"
  - [provider] name = "openai-compatible"
  - [provider] name = "ollama"
- Add base_url / api_key_env / extra headers only where necessary.
- Keep provider.api_keys support.
- Update init/check/help text so the CLI no longer assumes Anthropic only.

Implementation details
1. Add genai dependency to coop-agent with cargo add. Do not manually edit dependency versions if cargo add works.
2. Build a new provider factory path in coop-gateway/main.rs that no longer hardcodes Anthropic-only startup.
3. Update config validation so provider.name is no longer restricted to anthropic.
4. Implement a genai-backed Provider that:
   - translates Coop system/messages/tools into genai chat requests
   - translates genai responses/streams back into Coop Message/Usage
   - supports set_model hot reload
   - reports context limits sensibly
5. Preserve current model naming compatibility.
   - Existing anthropic model strings should still work.
   - Add a clean translation layer from Coop config model strings to genai model identifiers/namespaces.
   - Ensure OpenAI codex-style models route correctly via genai’s OpenAI Responses support when needed.
6. Generalize key rotation.
   - Refactor the current KeyPool if useful.
   - Keep header-aware Anthropic behavior if possible.
   - Add generic retry-after / 429 behavior for OpenAI/OpenAI-compatible providers.
   - Never log raw secrets.
7. Refactor image resizing/downsampling into a shared module and keep structured tracing for:
   - original size
   - resized size
   - mime type
   - reason for downscaling
8. Preserve prompt caching.
   - Inspect current AnthropicProvider cache breakpoint logic carefully.
   - Recreate equivalent semantics with genai where possible.
   - If genai cannot represent a needed cache breakpoint directly, implement a provider-side workaround/compat layer.
   - Preserve cache usage mapping into coop_core::Usage:
     - input_tokens
     - output_tokens
     - cache_read_tokens
     - cache_write_tokens
     - stop_reason
9. Preserve tool semantics.
   - ToolDef -> genai tool schema mapping
   - tool request ID/name/arguments mapping
   - tool result mapping
   - streaming partial tool-call handling
10. Preserve tracing quality.
   - Add spans/events for:
     - provider selection
     - model resolution
     - request method (complete vs stream)
     - message/tool counts
     - selected key index / pool size
     - retry/rotation decisions
     - cache-control decisions
     - usage results
     - image downscaling decisions
   - Verify expected fields appear in traces.jsonl, not just console output.

Acceptance criteria
- coop starts with anthropic config exactly as before, but using the new provider stack.
- coop can be configured for openai.
- coop can be configured for openai-compatible with a custom base_url.
- coop can be configured for ollama.
- Existing prompt caching behavior for Anthropic still works and cache usage is still surfaced.
- Key rotation still works; at minimum Anthropic parity is preserved.
- Image downscaling still works and is covered by tests.
- Tool calls work in both complete() and stream().
- Gateway logic remains unchanged apart from provider/config plumbing.
- No hardcoded “only anthropic supported” checks remain.
- All new config paths are validated by config_check.
- Tracing shows provider/model/request/usage/rotation events.
- Build, tests, fmt, and clippy pass.

Tests to add/update
- coop-agent unit tests:
  - message mapping
  - tool mapping
  - usage mapping
  - stream mapping
  - image prep/downscaling
  - key selection/rotation behavior
  - cache-control mapping
- coop-gateway tests:
  - provider config parsing/validation for new providers
  - startup/provider factory coverage
  - model hot reload / set_model
- keep or update live tests where useful, gated by env vars
- if tracing fields changed, add/adjust tracing verification tests

Validation commands
- cargo fmt
- cargo build
- cargo test -p coop-agent
- cargo test -p coop-gateway
- cargo clippy --all-targets --all-features -- -D warnings
- COOP_TRACE_FILE=traces.jsonl cargo test -p coop-gateway <relevant test or command> and inspect trace output
- When implementation is complete, run the signal-e2e-test skill to verify the full end-to-end path. This change touches provider calls, prompt building, tools, and agent-turn behavior, so unit tests alone are not sufficient.

Important guidance
- Do not accept regressions just because genai does not support something out of the box.
- Use genai as the new core provider transport/normalization layer, but keep small provider-specific compatibility wrappers where required.
- Prefer preserving Coop behavior over achieving a “pure” genai integration.
- Keep the diff reviewable and the code modular.
