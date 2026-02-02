# Agent Design: Provider + Own Loop

## Decision

Use Goose **only** for the provider layer (LLM API clients) and message types. Build everything else — the agent loop, tool dispatch, MCP connections, compaction — ourselves.

Goose's `Agent`, `ExtensionManager`, `SessionManager`, and `Config` are too coupled to use as components. The `Provider` trait and `Message` type are clean and worth depending on.

## What We Take from Goose

| Component | Import Path | Why |
|-----------|------------|-----|
| `Provider` trait | `goose::providers::base::Provider` | 20+ LLM providers with streaming, auth, retry, token counting |
| Provider factory | `goose::providers::create_with_named_model` | One-call provider construction |
| `Message` type | `goose::conversation::message::Message` | Multi-content messages (text + tool calls + tool results + thinking) |
| `ProviderUsage` | `goose::providers::base::ProviderUsage` | Token counting per call |
| `ModelConfig` | `goose::model::ModelConfig` | Context limits, model metadata |

## What We Build

| Component | Owner | Notes |
|-----------|-------|-------|
| Agent loop | **Coop** | Stream response → extract tool calls → dispatch → loop |
| Tool dispatch | **Coop** | Route to native tools or MCP clients |
| MCP connections | **Coop** (via `rmcp`) | `rmcp` is standalone, no Goose coupling |
| System prompt | **Coop** | `PromptBuilder` — already built |
| Session persistence | **Coop** | SQLite, Goose's `SessionManager` not used |
| Compaction | **Coop** | Call provider with summarization prompt when context is large |
| Native tools | **Coop** | memory_search, memory_get, read, write, edit |
| Tool permission gating | **Coop** | Trust-level checks before dispatch |
| Streaming events | **Coop** | `TurnEvent` stream for real-time UI/channel updates |

## Core Types

### TurnEvent — streaming output from the agent loop

```rust
/// Events streamed during an agent turn.
pub enum TurnEvent {
    /// Partial text from the model (for streaming to UI/channels).
    TextDelta(String),
    /// A complete assistant message (may contain tool calls).
    AssistantMessage(Message),
    /// Tool execution started.
    ToolStart { id: String, name: String },
    /// Tool execution completed.
    ToolResult { id: String, result: Message },
    /// Context was compacted (history replaced).
    Compacted { new_message_count: usize },
    /// Turn complete.
    Done(TurnResult),
}

pub struct TurnResult {
    /// All new messages produced this turn (to append to session).
    pub new_messages: Vec<Message>,
    /// Cumulative token usage.
    pub usage: ProviderUsage,
    /// Whether the agent hit max_turns (needs user input to continue).
    pub hit_limit: bool,
}
```

### TurnConfig — what Coop passes to each turn

```rust
pub struct TurnConfig {
    pub max_turns: u32,
    pub cancel: Option<CancellationToken>,
}

impl Default for TurnConfig {
    fn default() -> Self {
        Self { max_turns: 25, cancel: None }
    }
}
```

### ToolExecutor — unified dispatch for native + MCP tools

```rust
/// Dispatches tool calls to the right handler.
#[async_trait]
pub trait ToolExecutor: Send + Sync {
    /// Execute a tool call. Returns the result content.
    async fn execute(
        &self,
        name: &str,
        arguments: JsonObject,
        ctx: &ToolContext,
    ) -> Result<CallToolResult>;

    /// List all available tools.
    fn tools(&self) -> Vec<Tool>;
}

/// Context available during tool execution.
pub struct ToolContext {
    pub session_id: String,
    pub trust: TrustLevel,
    pub workspace: PathBuf,
    pub cancel: Option<CancellationToken>,
}
```

### NativeTool — Coop-implemented tools

```rust
#[async_trait]
pub trait NativeTool: Send + Sync {
    fn definition(&self) -> Tool;
    async fn execute(&self, args: JsonObject, ctx: &ToolContext) -> Result<CallToolResult>;
}
```

### McpClient — wrapper around rmcp

```rust
/// A connected MCP server.
pub struct McpClient {
    name: String,
    client: rmcp::Client,
    tools: Vec<Tool>,
}

impl McpClient {
    pub async fn connect(name: &str, transport: impl Transport) -> Result<Self> {
        let client = rmcp::ClientBuilder::new(transport).build().await?;
        let tools = client.list_tools().await?;
        Ok(Self { name: name.to_string(), client, tools })
    }

    pub async fn call_tool(&self, name: &str, args: JsonObject) -> Result<CallToolResult> {
        self.client.call_tool(name, args).await
    }
}
```

### CompositeExecutor — combines native tools + MCP clients

```rust
pub struct CompositeExecutor {
    native_tools: Vec<Box<dyn NativeTool>>,
    mcp_clients: Vec<McpClient>,
}

impl CompositeExecutor {
    pub fn new() -> Self {
        Self { native_tools: vec![], mcp_clients: vec![] }
    }

    pub fn add_native(&mut self, tool: Box<dyn NativeTool>) {
        self.native_tools.push(tool);
    }

    pub fn add_mcp(&mut self, client: McpClient) {
        self.mcp_clients.push(client);
    }
}

#[async_trait]
impl ToolExecutor for CompositeExecutor {
    async fn execute(&self, name: &str, args: JsonObject, ctx: &ToolContext) -> Result<CallToolResult> {
        // Check native tools first
        for tool in &self.native_tools {
            if tool.definition().name == name {
                return tool.execute(args, ctx).await;
            }
        }

        // Then MCP clients (tools are namespaced: "client__toolname")
        for client in &self.mcp_clients {
            for t in &client.tools {
                if t.name == name {
                    return client.call_tool(name, args).await;
                }
            }
        }

        anyhow::bail!("unknown tool: {name}")
    }

    fn tools(&self) -> Vec<Tool> {
        let mut all = Vec::new();
        for tool in &self.native_tools {
            all.push(tool.definition());
        }
        for client in &self.mcp_clients {
            all.extend(client.tools.clone());
        }
        all
    }
}
```

## The Agent Loop

```rust
pub async fn run_turn(
    provider: &dyn Provider,
    executor: &dyn ToolExecutor,
    session_id: &str,
    system_prompt: &str,
    messages: &mut Vec<Message>,
    config: &TurnConfig,
) -> impl Stream<Item = TurnEvent> {
    async_stream::stream! {
        let tools = executor.tools();
        let mut total_usage = ProviderUsage::default();
        let mut new_messages: Vec<Message> = Vec::new();
        let mut turns = 0;

        loop {
            if is_cancelled(&config.cancel) { break; }

            turns += 1;
            if turns > config.max_turns {
                yield TurnEvent::Done(TurnResult {
                    new_messages,
                    usage: total_usage,
                    hit_limit: true,
                });
                return;
            }

            // 1. Call provider
            let (response, usage) = if provider.supports_streaming() {
                let stream = provider.stream(session_id, system_prompt, messages, &tools).await
                    .map_err(|e| anyhow::anyhow!("{e}"))?;
                collect_stream_with_deltas(stream, |delta| {
                    yield TurnEvent::TextDelta(delta);
                }).await?
            } else {
                provider.complete(session_id, system_prompt, messages, &tools).await
                    .map_err(|e| anyhow::anyhow!("{e}"))?
            };

            total_usage = total_usage.combine_with(&usage);
            messages.push(response.clone());
            new_messages.push(response.clone());
            yield TurnEvent::AssistantMessage(response.clone());

            // 2. Extract tool calls
            let tool_requests = extract_tool_requests(&response);
            if tool_requests.is_empty() {
                yield TurnEvent::Done(TurnResult {
                    new_messages,
                    usage: total_usage,
                    hit_limit: false,
                });
                return;
            }

            // 3. Execute tools
            let ctx = ToolContext { session_id: session_id.to_string(), .. };
            let mut result_msg = Message::user();
            for request in &tool_requests {
                yield TurnEvent::ToolStart {
                    id: request.id.clone(),
                    name: request.name.clone(),
                };

                let result = executor.execute(&request.name, request.arguments.clone(), &ctx).await;
                let tool_result = match result {
                    Ok(r) => Ok(r),
                    Err(e) => Ok(CallToolResult {
                        content: vec![Content::text(format!("Error: {e}"))],
                        is_error: Some(true),
                        ..Default::default()
                    }),
                };

                result_msg = result_msg.with_tool_response(request.id.clone(), tool_result);
                yield TurnEvent::ToolResult {
                    id: request.id.clone(),
                    result: result_msg.clone(),
                };
            }

            messages.push(result_msg.clone());
            new_messages.push(result_msg);
        }
    }
}
```

## Compaction

Coop triggers compaction — it knows the context limit from the provider's `ModelConfig`.

```rust
pub async fn compact_if_needed(
    provider: &dyn Provider,
    session_id: &str,
    messages: &mut Vec<Message>,
    threshold: f64, // e.g., 0.8 = compact at 80% of context limit
) -> Result<Option<ProviderUsage>> {
    let model_config = provider.get_model_config();
    let token_count = estimate_tokens(messages); // rough count
    let limit = model_config.context_limit();

    if (token_count as f64 / limit as f64) < threshold {
        return Ok(None);
    }

    // Summarize older messages, keep recent ones
    let summary_prompt = "Summarize the conversation so far concisely, preserving key facts, decisions, and context needed for the ongoing task.";
    let (summary, usage) = provider.complete_fast(
        session_id,
        summary_prompt,
        messages,
        &[],
    ).await?;

    // Replace messages with summary + recent tail
    let tail_count = 4; // keep last few exchanges
    let tail = messages.split_off(messages.len().saturating_sub(tail_count));
    messages.clear();
    messages.push(Message::user().with_text("[Previous conversation summary]"));
    messages.push(summary);
    messages.extend(tail);

    Ok(Some(usage))
}
```

## Per-Agent Configuration

Each agent gets its own provider instance and tool executor:

```rust
pub struct AgentInstance {
    pub id: String,
    pub provider: Arc<dyn Provider>,
    pub executor: CompositeExecutor,
    pub workspace: PathBuf,
    pub default_trust: TrustLevel,
    pub max_turns: u32,
}

impl AgentInstance {
    pub async fn from_config(config: &AgentConfig) -> Result<Self> {
        let provider = create_with_named_model(&config.provider, &config.model).await?;

        let mut executor = CompositeExecutor::new();

        // Add native tools
        executor.add_native(Box::new(ReadTool::new(&config.workspace)));
        executor.add_native(Box::new(WriteTool::new(&config.workspace)));
        executor.add_native(Box::new(EditTool::new(&config.workspace)));

        // Connect MCP extensions
        for ext_config in &config.extensions {
            let transport = ChildProcessTransport::new(&ext_config.command, &ext_config.args)?;
            let client = McpClient::connect(&ext_config.name, transport).await?;
            executor.add_mcp(client);
        }

        Ok(Self {
            id: config.id.clone(),
            provider,
            executor,
            workspace: config.workspace.clone(),
            default_trust: TrustLevel::Full,
            max_turns: config.max_turns.unwrap_or(25),
        })
    }
}
```

## Gateway Integration

```rust
pub struct Gateway {
    agents: HashMap<String, AgentInstance>,
    sessions: SessionStore,
    prompt_builder: PromptBuilder,
}

impl Gateway {
    pub async fn handle_message(
        &self,
        session_key: &SessionKey,
        user_input: &str,
    ) -> impl Stream<Item = TurnEvent> {
        let agent = &self.agents[&session_key.agent_id];
        let mut messages = self.sessions.load(session_key).await?;

        // Build system prompt
        let system_prompt = self.prompt_builder
            .trust(trust_for_session(session_key))
            .build()
            .to_flat_string();

        // Append user message
        messages.push(Message::user().with_text(user_input));

        // Maybe compact before the turn
        compact_if_needed(&*agent.provider, &session_key.to_string(), &mut messages, 0.8).await?;

        // Run the turn
        let config = TurnConfig {
            max_turns: agent.max_turns,
            cancel: None,
        };
        let event_stream = run_turn(
            &*agent.provider,
            &agent.executor,
            &session_key.to_string(),
            &system_prompt,
            &mut messages,
            &config,
        ).await;

        // After turn completes, persist messages
        // (done by the caller consuming the stream)
        event_stream
    }
}
```

## Config::global() Workaround

The factory function `create_with_named_model` calls `Config::global().get_secret()` for API keys. Goose checks env vars first, so:

```rust
// Set API keys as env vars before provider creation
std::env::set_var("ANTHROPIC_API_KEY", &coop_config.providers.anthropic.api_key);
std::env::set_var("OPENAI_API_KEY", &coop_config.providers.openai.api_key);

// Now the factory works without a goose config file
let provider = create_with_named_model("anthropic", "claude-sonnet-4-20250514").await?;
```

Phase 2: If we need to bypass the singleton entirely, construct providers directly:
```rust
// Direct construction — no factory, no Config::global()
let api_client = ApiClient::new("https://api.anthropic.com", auth)?;
let provider = AnthropicProvider { api_client, model, .. };
```
(Requires the struct fields to be pub, which they currently aren't — potential upstream PR.)

## Dependency Graph

```
coop-core (traits, types — depends on goose for Message + rmcp for Tool)
    ↑            ↑             ↑
coop-agent   coop-channels   coop-gateway
(providers,  (signal, etc.)  (router, config,
 tool loop,                   orchestration)
 MCP clients,
 native tools)
    ↑            ↑             ↑
    └────────────┴─────────────┘
                 ↑
             coop-tui
```

Note: coop-core now depends on goose (for Message) and rmcp (for Tool). This breaks the "zero external deps" rule, but these types are the core currency of the system. The alternative — defining our own message type and converting at boundaries — adds complexity for no benefit since we'd be mirroring Goose's type exactly.

## Migration Steps

1. Update `coop-core`: add goose + rmcp deps, define `TurnEvent`, `TurnConfig`, `ToolExecutor`, `NativeTool`, `ToolContext`
2. Implement `CompositeExecutor` in `coop-agent` (native tools only, no MCP yet)
3. Implement `run_turn` loop in `coop-agent`
4. Implement basic native tools: `read`, `write`, `edit`
5. Update `Gateway` to use `run_turn` instead of `AgentRuntime::turn`
6. Wire TUI to consume `TurnEvent` stream
7. Remove `GooseRuntime` subprocess
8. Add MCP client support via `rmcp`
