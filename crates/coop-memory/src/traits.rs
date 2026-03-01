use anyhow::Result;
use async_trait::async_trait;
use coop_core::SessionKey;

use crate::types::{
    MemoryMaintenanceConfig, MemoryMaintenanceReport, MemoryQuery, NewObservation, Observation,
    ObservationHistoryEntry, ObservationIndex, Person, ReconcileDecision, ReconcileRequest,
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
pub trait Reconciler: Send + Sync {
    async fn reconcile(&self, request: &ReconcileRequest) -> Result<ReconcileDecision>;
}

#[async_trait]
pub trait Memory: Send + Sync {
    async fn search(&self, query: &MemoryQuery) -> Result<Vec<ObservationIndex>>;

    async fn search_by_file(
        &self,
        _path: &str,
        _prefix_match: bool,
        _limit: usize,
    ) -> Result<Vec<ObservationIndex>> {
        Ok(Vec::new())
    }

    async fn timeline(
        &self,
        anchor: i64,
        before: usize,
        after: usize,
    ) -> Result<Vec<ObservationIndex>>;

    async fn get(&self, ids: &[i64]) -> Result<Vec<Observation>>;

    async fn write(&self, obs: NewObservation) -> Result<WriteOutcome>;

    async fn people(&self, query: &str) -> Result<Vec<Person>>;

    /// Add an alias for a known person. If the person doesn't exist, this is a no-op.
    /// Aliases are deduplicated (case-insensitive).
    async fn add_person_alias(&self, _name: &str, _alias: &str) -> Result<bool> {
        Ok(false)
    }

    async fn summarize_session(&self, session_key: &SessionKey) -> Result<SessionSummary>;

    async fn recent_session_summaries(&self, limit: usize) -> Result<Vec<SessionSummary>>;

    async fn history(&self, observation_id: i64) -> Result<Vec<ObservationHistoryEntry>>;

    async fn run_maintenance(
        &self,
        config: &MemoryMaintenanceConfig,
    ) -> Result<MemoryMaintenanceReport>;

    /// Rebuild the vector search index from stored embeddings.
    /// Returns the number of entries rebuilt. Implementations without
    /// vector indexes should return `Ok(0)`.
    async fn rebuild_index(&self) -> Result<usize> {
        Ok(0)
    }
}
