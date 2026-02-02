use crate::types::*;
use anyhow::Result;
use async_trait::async_trait;

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

/// An AI agent runtime that can process conversation turns.
#[async_trait]
pub trait AgentRuntime: Send + Sync {
    /// Run a conversation turn: given message history + system prompt, produce a response.
    async fn turn(
        &self,
        messages: &[Message],
        system_prompt: &str,
    ) -> Result<AgentResponse>;
}

/// Storage backend for conversation sessions.
#[async_trait]
pub trait SessionStore: Send + Sync {
    /// Load messages for a session.
    async fn load(&self, key: &SessionKey) -> Result<Vec<Message>>;

    /// Save (append) messages to a session.
    async fn save(&self, key: &SessionKey, messages: &[Message]) -> Result<()>;

    /// List all known session keys.
    async fn list(&self) -> Result<Vec<SessionKey>>;

    /// Delete a session.
    async fn delete(&self, key: &SessionKey) -> Result<()>;
}
