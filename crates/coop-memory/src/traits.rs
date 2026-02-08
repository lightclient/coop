use anyhow::Result;
use async_trait::async_trait;
use coop_core::SessionKey;

use crate::types::{
    MemoryQuery, NewObservation, Observation, ObservationHistoryEntry, ObservationIndex, Person,
    SessionSummary, WriteOutcome,
};

#[async_trait]
pub trait EmbeddingProvider: Send + Sync {
    async fn embed(&self, text: &str) -> Result<Vec<f32>>;

    async fn embed_batch(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        let mut out = Vec::with_capacity(texts.len());
        for text in texts {
            out.push(self.embed(text).await?);
        }
        Ok(out)
    }

    fn dimensions(&self) -> usize;
}

#[async_trait]
pub trait Memory: Send + Sync {
    async fn search(&self, query: &MemoryQuery) -> Result<Vec<ObservationIndex>>;

    async fn timeline(
        &self,
        anchor: i64,
        before: usize,
        after: usize,
    ) -> Result<Vec<ObservationIndex>>;

    async fn get(&self, ids: &[i64]) -> Result<Vec<Observation>>;

    async fn write(&self, obs: NewObservation) -> Result<WriteOutcome>;

    async fn people(&self, query: &str) -> Result<Vec<Person>>;

    async fn summarize_session(&self, session_key: &SessionKey) -> Result<SessionSummary>;

    async fn history(&self, observation_id: i64) -> Result<Vec<ObservationHistoryEntry>>;
}
