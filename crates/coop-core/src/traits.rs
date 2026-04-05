//! Core trait definitions for Coop.
//!
//! These define the contracts between components. Implementations live in
//! other crates (coop-agent for providers, coop-channels for channels, etc.).

use crate::types::{
    ChannelHealth, InboundMessage, Message, ModelInfo, OutboundMessage, SessionKey, SessionKind,
    ToolDef, ToolOutput, TrustLevel, Usage,
};
use crate::workspace_scope::WorkspaceScope;
use anyhow::Result;
use async_trait::async_trait;
use std::path::{Path, PathBuf};
use std::pin::Pin;

/// A communication channel (terminal, Discord, Signal, etc.).
#[async_trait]
pub trait Channel: Send + Sync {
    /// Unique identifier for this channel.
    fn id(&self) -> &str;

    /// Receive the next inbound message (blocks until available).
    async fn recv(&mut self) -> Result<InboundMessage>;

    /// Send an outbound message.
    async fn send(&self, msg: OutboundMessage) -> Result<()>;

    /// Probe health of this channel.
    async fn probe(&self) -> ChannelHealth;
}

/// Callback for sending typing indicators on a channel.
#[async_trait]
pub trait TypingNotifier: Send + Sync {
    /// Send a typing started/stopped indicator for the given session.
    async fn set_typing(&self, session_key: &SessionKey, started: bool);
}

// ---------------------------------------------------------------------------
// Provider
// ---------------------------------------------------------------------------

/// A streamed response from an LLM provider.
///
/// Yields `(Option<Message>, Option<Usage>)` tuples. Partial messages contain
/// text deltas; the final message is complete. Usage is typically present on
/// the final yield.
pub type ProviderStream =
    Pin<Box<dyn futures::Stream<Item = Result<(Option<Message>, Option<Usage>)>> + Send>>;

/// An LLM provider that can complete conversations.
///
/// Implementations convert between Coop's `Message`/`ToolDef` types and the
/// provider's wire format at the boundary.
#[async_trait]
pub trait Provider: Send + Sync {
    /// Provider name (e.g., "anthropic", "openai").
    fn name(&self) -> &str;

    /// Model info (name, context limit).
    fn model_info(&self) -> ModelInfo;

    /// Run a completion: given system prompt blocks, conversation, and available tools,
    /// return a response message and usage.
    ///
    /// Each element of `system` becomes a separate system block. Providers that
    /// support prompt caching (e.g. Anthropic) place a `cache_control` breakpoint
    /// on each block, so a stable first block caches across turns even when later
    /// blocks change.
    async fn complete(
        &self,
        system: &[String],
        messages: &[Message],
        tools: &[ToolDef],
    ) -> Result<(Message, Usage)>;

    /// Streaming variant of complete. Returns a stream of partial messages.
    async fn stream(
        &self,
        system: &[String],
        messages: &[Message],
        tools: &[ToolDef],
    ) -> Result<ProviderStream>;

    /// Whether this provider supports streaming.
    fn supports_streaming(&self) -> bool {
        false
    }

    /// Update the model name used for subsequent API calls.
    ///
    /// The default implementation is a no-op. Providers that store the model
    /// internally (e.g. `AnthropicProvider`) override this so that
    /// hot-reloaded config changes take effect without a restart.
    fn set_model(&self, _model: &str) {}

    /// Run a "fast" completion (for summarization, naming, etc.).
    /// Falls back to `complete` by default.
    async fn complete_fast(
        &self,
        system: &[String],
        messages: &[Message],
        tools: &[ToolDef],
    ) -> Result<(Message, Usage)> {
        self.complete(system, messages, tools).await
    }
}

// ---------------------------------------------------------------------------
// Tool execution
// ---------------------------------------------------------------------------

/// Context available to a tool during execution.
#[derive(Debug, Clone)]
pub struct ToolContext {
    pub session_id: String,
    pub session_kind: SessionKind,
    pub trust: TrustLevel,
    pub workspace_root: PathBuf,
    pub workspace: PathBuf,
    pub workspace_scope: WorkspaceScope,
    pub user_name: Option<String>,
    pub model: Option<String>,
    pub visible_tools: Vec<String>,
}

impl ToolContext {
    pub fn new(
        session_id: impl Into<String>,
        session_kind: SessionKind,
        trust: TrustLevel,
        workspace_root: impl AsRef<Path>,
        user_name: Option<&str>,
    ) -> Self {
        let workspace_scope =
            WorkspaceScope::for_turn(workspace_root.as_ref(), &session_kind, trust, user_name);
        let workspace = workspace_scope.tool_workspace_root();

        Self {
            session_id: session_id.into(),
            session_kind,
            trust,
            workspace_root: workspace_scope.workspace_root().to_path_buf(),
            workspace,
            workspace_scope,
            user_name: user_name.map(str::to_owned),
            model: None,
            visible_tools: Vec::new(),
        }
    }

    #[must_use]
    pub fn with_model(mut self, model: impl Into<String>) -> Self {
        self.model = Some(model.into());
        self
    }

    #[must_use]
    pub fn with_visible_tools<I, S>(mut self, visible_tools: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.visible_tools = visible_tools.into_iter().map(Into::into).collect();
        self
    }
}

/// A tool that Coop can execute (native or MCP).
#[async_trait]
pub trait Tool: Send + Sync {
    /// The tool definition (name, description, parameter schema).
    fn definition(&self) -> ToolDef;

    /// Execute the tool with the given arguments.
    async fn execute(&self, arguments: serde_json::Value, ctx: &ToolContext) -> Result<ToolOutput>;
}

/// Dispatches tool calls to the right handler.
///
/// The composite executor combines native tools and MCP clients, routing
/// tool calls by name.
#[async_trait]
pub trait ToolExecutor: Send + Sync {
    /// Execute a tool call by name.
    async fn execute(
        &self,
        name: &str,
        arguments: serde_json::Value,
        ctx: &ToolContext,
    ) -> Result<ToolOutput>;

    /// List all available tool definitions.
    fn tools(&self) -> Vec<ToolDef>;
}

// ---------------------------------------------------------------------------
// Session storage
// ---------------------------------------------------------------------------

/// Storage backend for conversation sessions.
#[async_trait]
pub trait SessionStore: Send + Sync {
    /// Load messages for a session.
    async fn load(&self, key: &SessionKey) -> Result<Vec<Message>>;

    /// Save (append) messages to a session.
    async fn save(&self, key: &SessionKey, messages: &[Message]) -> Result<()>;

    /// Replace all messages in a session (used after compaction).
    async fn replace(&self, key: &SessionKey, messages: &[Message]) -> Result<()>;

    /// List all known session keys.
    async fn list(&self) -> Result<Vec<SessionKey>>;

    /// Delete a session.
    async fn delete(&self, key: &SessionKey) -> Result<()>;
}
