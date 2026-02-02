# Goose Integration Strategy

## Decision

Use Goose as a **library dependency at the provider and MCP layer**, not through its high-level `Agent` API. Coop owns the agent loop, session management, prompt construction, memory, and subagent orchestration. Goose provides LLM provider clients and (via `rmcp`) MCP tool connections.

## Why not use Goose's Agent directly?

Goose's `Agent.reply()` bundles five layers into one call:

```
Layer 5: Orchestration    — subagents, recipes, scheduling
Layer 4: Session/Prompt   — SessionManager (SQLite), PromptManager (Jinja2), hints
Layer 3: Agent loop       — reply(), tool call → execute → loop, compaction, retry
Layer 2: Extensions       — ExtensionManager, MCP client connections, tool discovery
Layer 1: Provider         — dyn Provider: stream(system, messages, tools) → response
```

Coop's core value proposition is layers 3-5: token-sensitive prompts, trust-gated memory, observable sessions, controlled subagent spawning. Using `Agent.reply()` means fighting Goose for control of the things that differentiate Coop.

The `Provider` trait (layer 1) is the clean boundary. It says: "here's a system prompt, conversation history, and tool definitions — give me a response." No sessions, no prompt opinions, no config singletons.

## What Goose provides

### Provider trait (`goose::providers::base::Provider`)

```rust
#[async_trait]
pub trait Provider: Send + Sync {
    async fn complete(
        &self,
        session_id: Option<&str>,
        system: &str,
        messages: &[Message],
        tools: &[Tool],
    ) -> Result<(Message, ProviderUsage), ProviderError>;

    async fn stream(
        &self,
        session_id: Option<&str>,
        system: &str,
        messages: &[Message],
        tools: &[Tool],
    ) -> Result<MessageStream, ProviderError>;
}
```

One factory call gives us any provider with auth, streaming, retry, token counting, and model config:

```rust
let provider = create_with_named_model("anthropic", "claude-opus-4-5").await?;
```

This covers 20+ providers (Anthropic, OpenAI, Bedrock, Vertex, Ollama, OpenRouter, etc.) without reimplementing any of them.

### MCP tools (`rmcp` crate)

Goose uses `rmcp` for MCP protocol support. We depend on it directly — it's a standalone crate with no Goose coupling:

```rust
let client = rmcp::ClientBuilder::new(transport).build().await?;
let tools = client.list_tools().await?;
let result = client.call_tool("bash", json!({"command": "ls"})).await?;
```

### Message types (`goose::conversation::message::Message`)

We reuse Goose's `Message` type for provider compatibility. It handles the complexity of multi-content messages (text + tool calls + tool results) that providers expect.

### Token counting (`goose::token_counter`, `tiktoken-rs`)

Goose bundles tiktoken-rs and has token counting utilities. Provider responses include `ProviderUsage` with input/output token counts.

## What Coop builds

| Component | Owner | Notes |
|-----------|-------|-------|
| LLM provider clients | Goose | 20+ providers, streaming, auth, retry |
| MCP tool connections | rmcp | Standalone, no Goose coupling |
| Agent loop | **Coop** | Tool call → execute → loop. We control compaction, retry, subagents. |
| System prompt | **Coop** | Prompt builder with layers, trust gating, token budgets |
| Session state | **Coop** | SQLite persistence, trust metadata, cross-session messaging |
| Memory | **Coop** | Progressive disclosure, trust-scoped stores, vector search |
| Subagent orchestration | **Coop** | Control child prompt/tools/budget, stream events |
| Native tools | **Coop** | memory_search, memory_get, coop_status, etc. |

## Coop's agent loop

The core loop is straightforward. Complexity is added incrementally.

```rust
pub async fn run_turn(
    provider: &dyn Provider,
    system_prompt: &str,
    messages: &mut Vec<Message>,
    tools: &[Tool],
    tool_executor: &dyn ToolExecutor,
    config: &TurnConfig,
) -> Result<TurnResult> {
    let mut turns = 0;

    loop {
        let (response, usage) = provider.stream(
            Some(&config.session_id),
            system_prompt,
            messages,
            tools,
        ).await?;

        messages.push(response.clone());

        let tool_requests = response.tool_requests();
        if tool_requests.is_empty() {
            return Ok(TurnResult { usage, turns });
        }

        for request in &tool_requests {
            let result = tool_executor.execute(request, &config.trust).await?;
            messages.push(Message::tool_result(request.id, result));
        }

        turns += 1;
        if turns >= config.max_turns {
            break;
        }

        if needs_compaction(messages, config.context_limit) {
            compact(provider, messages).await?;
        }
    }

    Ok(TurnResult { usage, turns })
}
```

### Incremental sophistication

1. **Phase 1:** Basic loop — no compaction, no retry, no streaming
2. **Phase 2:** Streaming events forwarded to TUI/channels
3. **Phase 3:** Context compaction (port Goose's algorithm, it's open source)
4. **Phase 4:** Provider-level retry with exponential backoff
5. **Phase 5:** Subagent spawning — `run_turn` with different prompt/tools/budget

## What this unlocks

**Subagent control:** A subagent is just another `run_turn` call with a lean system prompt, scoped tool set, and its own token budget. We can stream events from parent and child simultaneously and report per-subagent cost.

**Memory as native tools:** `memory_search` and `memory_get` are Coop-implemented tool executors — no MCP server needed. They respect trust level because Coop controls the executor.

**Prompt caching:** We own the system prompt string end-to-end. When we need multi-block Anthropic cache control, we can call the Anthropic provider's lower-level methods directly.

**Session ownership:** Messages live in our SQLite. We decide what history gets sent to the provider, when compaction fires, and what context subagents inherit.

## Dependency setup

```toml
[workspace.dependencies]
# Pin to specific commit — Goose is pre-1.0, API changes are expected
goose = { git = "https://github.com/block/goose", rev = "<pin>" }
rmcp = { version = "0.1", features = ["client", "transport-child-process"] }
```

Selective imports — we use providers and message types, not the agent framework:

```rust
use goose::providers::{create_with_named_model, base::Provider};
use goose::providers::base::{ProviderUsage, Usage, MessageStream};
use goose::conversation::message::Message;
use goose::token_counter;
```

We do NOT import: `goose::agents`, `goose::session`, `goose::config` (except minimally for provider init).

## Config singleton workaround

Goose's `Config::global()` is a `OnceCell` singleton used 72+ times internally. We avoid most of it by not using `Agent`, but provider creation (`create_with_named_model`) reads API keys from Config.

**Phase 1:** Set API keys as environment variables before provider creation. Goose checks env vars before the config file — `ANTHROPIC_API_KEY` just works.

**Phase 2:** If we need per-agent provider config, instantiate providers directly (e.g., `AnthropicProvider::new(...)`) bypassing the factory and the singleton entirely.

## Risk: Goose API stability

Goose is pre-1.0 and actively developed. The `Provider` trait and `Message` types are relatively stable (they're the foundation everything else builds on), but breaking changes are possible.

Mitigation:
- Pin to a specific git commit, never `branch = "main"`
- Wrap Goose types in thin Coop adapters where practical
- Every Goose upgrade is a deliberate, tested change
- The provider trait surface we use is small — breakage is easy to fix

## Future: contributing upstream

If Coop's agent loop proves robust, there may be value in contributing a "headless" or "embedded" mode back to Goose that exposes the provider layer more cleanly as a standalone interface. This would benefit other projects building on Goose's provider ecosystem without wanting its full agent framework.
