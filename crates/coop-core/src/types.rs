//! Core types for Coop.
//!
//! These are Coop's first-class types. Provider implementations in `coop-agent`
//! convert between these and the provider's wire format at the boundary.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fmt;
use uuid::Uuid;

// ---------------------------------------------------------------------------
// Trust
// ---------------------------------------------------------------------------

/// Trust levels, ordered from most to least privileged.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TrustLevel {
    Full,
    Inner,
    Familiar,
    Public,
}

impl TrustLevel {
    fn rank(self) -> u8 {
        match self {
            Self::Full => 0,
            Self::Inner => 1,
            Self::Familiar => 2,
            Self::Public => 3,
        }
    }
}

impl PartialOrd for TrustLevel {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for TrustLevel {
    /// Full < Inner < Familiar < Public (most trusted is "smallest").
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.rank().cmp(&other.rank())
    }
}

// ---------------------------------------------------------------------------
// Message
// ---------------------------------------------------------------------------

/// Role in a conversation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    User,
    Assistant,
}

/// A single piece of content within a message.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Content {
    /// Plain text.
    Text { text: String },

    /// Base64-encoded image.
    Image { data: String, mime_type: String },

    /// A request from the assistant to call a tool.
    ToolRequest {
        id: String,
        name: String,
        arguments: serde_json::Value,
    },

    /// The result of a tool call, sent back to the assistant.
    ToolResult {
        id: String,
        output: String,
        is_error: bool,
    },

    /// Model thinking/reasoning (may be redacted).
    Thinking {
        thinking: String,
        signature: Option<String>,
    },
}

impl Content {
    pub fn text(text: impl Into<String>) -> Self {
        Self::Text { text: text.into() }
    }

    pub fn image(data: impl Into<String>, mime_type: impl Into<String>) -> Self {
        Self::Image {
            data: data.into(),
            mime_type: mime_type.into(),
        }
    }

    pub fn tool_request(
        id: impl Into<String>,
        name: impl Into<String>,
        arguments: serde_json::Value,
    ) -> Self {
        Self::ToolRequest {
            id: id.into(),
            name: name.into(),
            arguments,
        }
    }

    pub fn tool_result(id: impl Into<String>, output: impl Into<String>, is_error: bool) -> Self {
        Self::ToolResult {
            id: id.into(),
            output: output.into(),
            is_error,
        }
    }

    /// Returns the text if this is a Text variant.
    pub fn as_text(&self) -> Option<&str> {
        match self {
            Self::Text { text } => Some(text),
            _ => None,
        }
    }

    /// Returns the tool request fields if this is a ToolRequest variant.
    pub fn as_tool_request(&self) -> Option<(&str, &str, &serde_json::Value)> {
        match self {
            Self::ToolRequest {
                id,
                name,
                arguments,
            } => Some((id, name, arguments)),
            _ => None,
        }
    }
}

impl fmt::Display for Content {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Text { text } => write!(f, "{text}"),
            Self::Image { mime_type, .. } => write!(f, "[image: {mime_type}]"),
            Self::ToolRequest { name, .. } => write!(f, "[tool_request: {name}]"),
            Self::ToolResult { id, is_error, .. } => {
                write!(f, "[tool_result: {id}, error={is_error}]")
            }
            Self::Thinking { .. } => write!(f, "[thinking]"),
        }
    }
}

/// A message in a conversation.
///
/// Multi-content message model.
/// Messages can contain multiple content blocks (e.g., text + tool calls).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    /// Unique message id.
    pub id: String,
    /// User or Assistant.
    pub role: Role,
    /// When this message was created.
    pub created: DateTime<Utc>,
    /// Ordered content blocks.
    pub content: Vec<Content>,
    /// Arbitrary metadata (provider info, channel origin, etc.).
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub metadata: HashMap<String, serde_json::Value>,
}

impl Message {
    /// Create a new user message.
    pub fn user() -> Self {
        Self {
            id: new_id(),
            role: Role::User,
            created: Utc::now(),
            content: Vec::new(),
            metadata: HashMap::new(),
        }
    }

    /// Create a new assistant message.
    pub fn assistant() -> Self {
        Self {
            id: new_id(),
            role: Role::Assistant,
            created: Utc::now(),
            content: Vec::new(),
            metadata: HashMap::new(),
        }
    }

    // -- Builder methods --

    /// Add a content block.
    #[must_use]
    pub fn with_content(mut self, content: Content) -> Self {
        self.content.push(content);
        self
    }

    /// Add text content.
    #[must_use]
    pub fn with_text(self, text: impl Into<String>) -> Self {
        self.with_content(Content::text(text))
    }

    /// Add an image.
    #[must_use]
    pub fn with_image(self, data: impl Into<String>, mime_type: impl Into<String>) -> Self {
        self.with_content(Content::image(data, mime_type))
    }

    /// Add a tool request.
    #[must_use]
    pub fn with_tool_request(
        self,
        id: impl Into<String>,
        name: impl Into<String>,
        arguments: serde_json::Value,
    ) -> Self {
        self.with_content(Content::tool_request(id, name, arguments))
    }

    /// Add a tool result.
    #[must_use]
    pub fn with_tool_result(
        self,
        id: impl Into<String>,
        output: impl Into<String>,
        is_error: bool,
    ) -> Self {
        self.with_content(Content::tool_result(id, output, is_error))
    }

    /// Set metadata.
    #[must_use]
    pub fn with_metadata(mut self, key: impl Into<String>, value: serde_json::Value) -> Self {
        self.metadata.insert(key.into(), value);
        self
    }

    // -- Accessors --

    /// Concatenate all text content blocks.
    pub fn text(&self) -> String {
        self.content
            .iter()
            .filter_map(Content::as_text)
            .collect::<Vec<_>>()
            .join("")
    }

    /// Extract all tool requests from this message.
    pub fn tool_requests(&self) -> Vec<ToolRequest> {
        self.content
            .iter()
            .filter_map(|c| match c {
                Content::ToolRequest {
                    id,
                    name,
                    arguments,
                } => Some(ToolRequest {
                    id: id.clone(),
                    name: name.clone(),
                    arguments: arguments.clone(),
                }),
                _ => None,
            })
            .collect()
    }

    /// True if this message contains any tool requests.
    pub fn has_tool_requests(&self) -> bool {
        self.content
            .iter()
            .any(|c| matches!(c, Content::ToolRequest { .. }))
    }

    /// True if this message contains any tool results.
    pub fn has_tool_results(&self) -> bool {
        self.content
            .iter()
            .any(|c| matches!(c, Content::ToolResult { .. }))
    }
}

/// A parsed tool request (convenience struct extracted from Content::ToolRequest).
#[derive(Debug, Clone)]
pub struct ToolRequest {
    pub id: String,
    pub name: String,
    pub arguments: serde_json::Value,
}

// ---------------------------------------------------------------------------
// Tool definitions
// ---------------------------------------------------------------------------

/// A tool definition that can be sent to an LLM.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolDef {
    /// Tool name (unique within a session).
    pub name: String,
    /// Human-readable description.
    pub description: String,
    /// JSON Schema for the tool's parameters.
    pub parameters: serde_json::Value,
}

impl ToolDef {
    pub fn new(
        name: impl Into<String>,
        description: impl Into<String>,
        parameters: serde_json::Value,
    ) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            parameters,
        }
    }
}

/// The result of executing a tool.
#[derive(Debug, Clone)]
pub struct ToolOutput {
    /// The text output (or error message).
    pub content: String,
    /// Whether this result represents an error.
    pub is_error: bool,
}

impl ToolOutput {
    pub fn success(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            is_error: false,
        }
    }

    pub fn error(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            is_error: true,
        }
    }
}

// ---------------------------------------------------------------------------
// Provider types
// ---------------------------------------------------------------------------

/// Token usage from a provider call.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub struct Usage {
    pub input_tokens: Option<u32>,
    pub output_tokens: Option<u32>,
    pub cache_read_tokens: Option<u32>,
    pub cache_write_tokens: Option<u32>,
}

impl Usage {
    pub fn total_tokens(&self) -> u32 {
        self.input_tokens.unwrap_or(0) + self.output_tokens.unwrap_or(0)
    }
}

impl std::ops::Add for Usage {
    type Output = Self;
    fn add(self, rhs: Self) -> Self {
        Self {
            input_tokens: sum_opt(self.input_tokens, rhs.input_tokens),
            output_tokens: sum_opt(self.output_tokens, rhs.output_tokens),
            cache_read_tokens: sum_opt(self.cache_read_tokens, rhs.cache_read_tokens),
            cache_write_tokens: sum_opt(self.cache_write_tokens, rhs.cache_write_tokens),
        }
    }
}

impl std::ops::AddAssign for Usage {
    fn add_assign(&mut self, rhs: Self) {
        *self = *self + rhs;
    }
}

fn sum_opt(a: Option<u32>, b: Option<u32>) -> Option<u32> {
    match (a, b) {
        (Some(x), Some(y)) => Some(x + y),
        (Some(x), None) | (None, Some(x)) => Some(x),
        (None, None) => None,
    }
}

/// Model information.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelInfo {
    /// Model name as the provider knows it.
    pub name: String,
    /// Maximum context window in tokens.
    pub context_limit: usize,
}

// ---------------------------------------------------------------------------
// Session types
// ---------------------------------------------------------------------------

/// Identifies a session: which agent + what kind.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SessionKey {
    pub agent_id: String,
    pub kind: SessionKind,
}

impl fmt::Display for SessionKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.kind {
            SessionKind::Main => write!(f, "{}:main", self.agent_id),
            SessionKind::Dm(who) => write!(f, "{}:dm:{who}", self.agent_id),
            SessionKind::Group(id) => write!(f, "{}:group:{id}", self.agent_id),
            SessionKind::Isolated(uuid) => write!(f, "{}:isolated:{uuid}", self.agent_id),
        }
    }
}

/// The kind of session.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum SessionKind {
    Main,
    Dm(String),
    Group(String),
    Isolated(Uuid),
}

// ---------------------------------------------------------------------------
// Channel types
// ---------------------------------------------------------------------------

/// An inbound message from a channel.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InboundMessage {
    pub channel: String,
    pub sender: String,
    pub content: String,
    pub chat_id: Option<String>,
    pub is_group: bool,
    pub timestamp: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reply_to: Option<String>,
}

/// An outbound message to be sent via a channel.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OutboundMessage {
    pub channel: String,
    pub target: String,
    pub content: String,
}

/// Health status of a channel.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ChannelHealth {
    Healthy,
    Degraded(String),
    Unhealthy(String),
}

// ---------------------------------------------------------------------------
// Turn types (agent loop)
// ---------------------------------------------------------------------------

/// Configuration for a single agent turn.
#[derive(Debug)]
pub struct TurnConfig {
    /// Max tool-call loop iterations before requiring user input.
    pub max_iterations: u32,
}

impl Default for TurnConfig {
    fn default() -> Self {
        Self { max_iterations: 25 }
    }
}

/// The result of a completed turn.
#[derive(Debug, Clone)]
pub struct TurnResult {
    /// New messages produced during this turn (assistant + tool results).
    pub messages: Vec<Message>,
    /// Cumulative token usage.
    pub usage: Usage,
    /// True if the agent hit max_iterations (needs user input to continue).
    pub hit_limit: bool,
}

/// Events streamed during an agent turn.
#[derive(Debug, Clone)]
pub enum TurnEvent {
    /// Partial text delta (for streaming to UI/channels).
    TextDelta(String),
    /// A complete assistant message (may contain tool calls).
    AssistantMessage(Message),
    /// A tool execution started.
    ToolStart {
        id: String,
        name: String,
        arguments: serde_json::Value,
    },
    /// A tool execution completed.
    ToolResult { id: String, message: Message },
    /// Turn complete.
    Done(TurnResult),
    /// Non-fatal error.
    Error(String),
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn new_id() -> String {
    format!("msg_{}", Uuid::new_v4())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn message_builder() {
        let msg = Message::user().with_text("hello").with_text(" world");

        assert_eq!(msg.role, Role::User);
        assert_eq!(msg.text(), "hello world");
        assert_eq!(msg.content.len(), 2);
    }

    #[test]
    fn message_tool_requests() {
        let msg = Message::assistant()
            .with_text("Let me check that.")
            .with_tool_request(
                "call_1",
                "read_file",
                serde_json::json!({"path": "foo.txt"}),
            );

        assert!(msg.has_tool_requests());
        let reqs = msg.tool_requests();
        assert_eq!(reqs.len(), 1);
        assert_eq!(reqs[0].name, "read_file");
        assert_eq!(reqs[0].id, "call_1");
    }

    #[test]
    fn message_tool_result() {
        let msg = Message::user().with_tool_result("call_1", "file contents here", false);

        assert!(msg.has_tool_results());
        assert!(!msg.has_tool_requests());
    }

    #[test]
    fn message_serialization_roundtrip() {
        let msg = Message::assistant().with_text("hello").with_tool_request(
            "id1",
            "bash",
            serde_json::json!({"cmd": "ls"}),
        );

        let json = serde_json::to_string(&msg).unwrap();
        let roundtrip: Message = serde_json::from_str(&json).unwrap();

        assert_eq!(roundtrip.role, Role::Assistant);
        assert_eq!(roundtrip.content.len(), 2);
        assert_eq!(roundtrip.text(), "hello");
        assert_eq!(roundtrip.tool_requests().len(), 1);
    }

    #[test]
    fn trust_ordering() {
        assert!(TrustLevel::Full < TrustLevel::Inner);
        assert!(TrustLevel::Inner < TrustLevel::Familiar);
        assert!(TrustLevel::Familiar < TrustLevel::Public);
    }

    #[test]
    fn usage_arithmetic() {
        let a = Usage {
            input_tokens: Some(100),
            output_tokens: Some(50),
            ..Default::default()
        };
        let b = Usage {
            input_tokens: Some(200),
            output_tokens: None,
            cache_read_tokens: Some(10),
            ..Default::default()
        };
        let c = a + b;
        assert_eq!(c.input_tokens, Some(300));
        assert_eq!(c.output_tokens, Some(50));
        assert_eq!(c.cache_read_tokens, Some(10));
        assert_eq!(c.total_tokens(), 350);
    }

    #[test]
    fn tool_def_construction() {
        let tool = ToolDef::new(
            "read_file",
            "Read the contents of a file",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "File path" }
                },
                "required": ["path"]
            }),
        );
        assert_eq!(tool.name, "read_file");
    }

    #[test]
    fn session_key_display() {
        let key = SessionKey {
            agent_id: "reid".into(),
            kind: SessionKind::Main,
        };
        assert_eq!(key.to_string(), "reid:main");
    }
}
