use chrono::{DateTime, Utc};
use coop_core::TrustLevel;
use serde::{Deserialize, Serialize};

/// Query options for memory search.
#[derive(Debug, Clone, Default)]
pub struct MemoryQuery {
    pub text: Option<String>,
    pub stores: Vec<String>,
    pub types: Vec<String>,
    pub people: Vec<String>,
    pub after: Option<DateTime<Utc>>,
    pub before: Option<DateTime<Utc>>,
    pub limit: usize,
    pub max_tokens: Option<usize>,
}

/// Layer 1: compact search result.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ObservationIndex {
    pub id: i64,
    pub title: String,
    pub obs_type: String,
    pub store: String,
    pub created_at: DateTime<Utc>,
    pub token_count: u32,
    pub mention_count: u32,
    pub score: f32,
    pub related_people: Vec<String>,
}

/// Layer 3: full observation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Observation {
    pub id: i64,
    pub title: String,
    pub narrative: String,
    pub facts: Vec<String>,
    pub tags: Vec<String>,
    pub obs_type: String,
    pub store: String,
    pub related_files: Vec<String>,
    pub related_people: Vec<String>,
    pub created_at: DateTime<Utc>,
    pub token_count: u32,
    pub mention_count: u32,
}

/// New observation to be written to memory.
#[derive(Debug, Clone)]
pub struct NewObservation {
    pub session_key: Option<String>,
    pub store: String,
    pub obs_type: String,
    pub title: String,
    pub narrative: String,
    pub facts: Vec<String>,
    pub tags: Vec<String>,
    pub source: String,
    pub related_files: Vec<String>,
    pub related_people: Vec<String>,
    pub token_count: Option<u32>,
    pub expires_at: Option<DateTime<Utc>>,
    pub min_trust: TrustLevel,
}

impl NewObservation {
    pub fn technical(title: impl Into<String>, narrative: impl Into<String>) -> Self {
        Self {
            session_key: None,
            store: "shared".to_owned(),
            obs_type: "technical".to_owned(),
            title: title.into(),
            narrative: narrative.into(),
            facts: Vec::new(),
            tags: Vec::new(),
            source: "agent".to_owned(),
            related_files: Vec::new(),
            related_people: Vec::new(),
            token_count: None,
            expires_at: None,
            min_trust: TrustLevel::Inner,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WriteOutcome {
    Added(i64),
    Updated(i64),
    Deleted(i64),
    Skipped,
    ExactDup,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ObservationHistoryEntry {
    pub observation_id: i64,
    pub old_title: Option<String>,
    pub old_facts: Option<String>,
    pub new_title: Option<String>,
    pub new_facts: Option<String>,
    pub event: String,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Person {
    pub name: String,
    pub store: String,
    pub facts: serde_json::Value,
    pub last_mentioned: Option<DateTime<Utc>>,
    pub mention_count: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionSummary {
    pub session_key: String,
    pub request: String,
    pub outcome: String,
    pub decisions: Vec<String>,
    pub open_items: Vec<String>,
    pub observation_count: usize,
    pub created_at: DateTime<Utc>,
}

pub fn trust_to_store(trust: TrustLevel) -> &'static str {
    match trust {
        TrustLevel::Full => "private",
        TrustLevel::Inner => "shared",
        TrustLevel::Familiar | TrustLevel::Public => "social",
    }
}

pub fn min_trust_for_store(store: &str) -> TrustLevel {
    match store {
        "private" => TrustLevel::Full,
        "shared" => TrustLevel::Inner,
        "social" => TrustLevel::Familiar,
        _ => TrustLevel::Public,
    }
}

pub fn accessible_stores(trust: TrustLevel) -> Vec<String> {
    match trust {
        TrustLevel::Full => vec!["private", "shared", "social"],
        TrustLevel::Inner => vec!["shared", "social"],
        TrustLevel::Familiar => vec!["social"],
        TrustLevel::Public => vec![],
    }
    .into_iter()
    .map(str::to_owned)
    .collect()
}

pub fn trust_to_str(trust: TrustLevel) -> &'static str {
    match trust {
        TrustLevel::Full => "full",
        TrustLevel::Inner => "inner",
        TrustLevel::Familiar => "familiar",
        TrustLevel::Public => "public",
    }
}

pub fn trust_from_str(value: &str) -> TrustLevel {
    match value {
        "full" => TrustLevel::Full,
        "inner" => TrustLevel::Inner,
        "familiar" => TrustLevel::Familiar,
        _ => TrustLevel::Public,
    }
}
