use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use uuid::Uuid;

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
    /// Numeric rank where lower = more trusted.
    fn rank(self) -> u8 {
        match self {
            TrustLevel::Full => 0,
            TrustLevel::Inner => 1,
            TrustLevel::Familiar => 2,
            TrustLevel::Public => 3,
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

/// Message role in a conversation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Role {
    System,
    User,
    Assistant,
    Tool,
}

/// A single message in a conversation session.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    pub content: String,
    pub timestamp: DateTime<Utc>,
    #[serde(default)]
    pub metadata: HashMap<String, String>,
}

impl Message {
    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: Role::User,
            content: content.into(),
            timestamp: Utc::now(),
            metadata: HashMap::new(),
        }
    }

    pub fn assistant(content: impl Into<String>) -> Self {
        Self {
            role: Role::Assistant,
            content: content.into(),
            timestamp: Utc::now(),
            metadata: HashMap::new(),
        }
    }

    pub fn system(content: impl Into<String>) -> Self {
        Self {
            role: Role::System,
            content: content.into(),
            timestamp: Utc::now(),
            metadata: HashMap::new(),
        }
    }
}

/// Identifies a session: which agent + what kind.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SessionKey {
    pub agent_id: String,
    pub kind: SessionKind,
}

/// The kind of session.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum SessionKind {
    Main,
    Dm(String),
    Group(String),
    Isolated(Uuid),
}

/// An inbound message from a channel.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InboundMessage {
    pub channel: String,
    pub sender: String,
    pub content: String,
    pub chat_id: Option<String>,
    pub is_group: bool,
    pub timestamp: DateTime<Utc>,
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

/// Response from an agent runtime.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentResponse {
    pub content: String,
    pub tool_calls: Vec<String>,
}

impl AgentResponse {
    pub fn text(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            tool_calls: Vec::new(),
        }
    }
}
