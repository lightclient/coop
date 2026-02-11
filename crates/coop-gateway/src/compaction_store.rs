//! Persistent storage for compaction state.
//!
//! Stores compaction state as JSON files alongside session JSONL files.

use anyhow::{Context, Result};
use coop_core::types::SessionKey;
use std::path::{Path, PathBuf};
use tracing::debug;

use crate::compaction::CompactionState;

pub(crate) struct CompactionStore {
    dir: PathBuf,
}

impl CompactionStore {
    pub(crate) fn new(dir: impl AsRef<Path>) -> Result<Self> {
        let dir = dir.as_ref().to_path_buf();
        std::fs::create_dir_all(&dir)
            .with_context(|| format!("failed to create compaction dir: {}", dir.display()))?;
        Ok(Self { dir })
    }

    pub(crate) fn load(&self, key: &SessionKey) -> Result<Option<CompactionState>> {
        let path = self.path_for(key);
        if !path.exists() {
            return Ok(None);
        }

        let data = std::fs::read_to_string(&path)
            .with_context(|| format!("failed to read compaction: {}", path.display()))?;
        let state: CompactionState = serde_json::from_str(&data)
            .with_context(|| format!("failed to parse compaction: {}", path.display()))?;

        debug!(session = %key, "loaded compaction state");
        Ok(Some(state))
    }

    pub(crate) fn save(&self, key: &SessionKey, state: &CompactionState) -> Result<()> {
        let path = self.path_for(key);
        let data = serde_json::to_string_pretty(state)?;
        std::fs::write(&path, data)
            .with_context(|| format!("failed to write compaction: {}", path.display()))?;
        debug!(session = %key, "saved compaction state");
        Ok(())
    }

    pub(crate) fn delete(&self, key: &SessionKey) -> Result<()> {
        let path = self.path_for(key);
        if path.exists() {
            std::fs::remove_file(&path)
                .with_context(|| format!("failed to delete compaction: {}", path.display()))?;
            debug!(session = %key, "deleted compaction state");
        }
        Ok(())
    }

    fn path_for(&self, key: &SessionKey) -> PathBuf {
        let slug = key.to_string().replace(':', "_");
        self.dir.join(format!("{slug}_compaction.json"))
    }
}

#[allow(clippy::unwrap_used)]
#[cfg(test)]
mod tests {
    use super::*;
    use coop_core::types::SessionKind;

    fn test_key() -> SessionKey {
        SessionKey {
            agent_id: "coop".into(),
            kind: SessionKind::Main,
        }
    }

    #[test]
    fn roundtrip_save_load() {
        let dir = tempfile::tempdir().unwrap();
        let store = CompactionStore::new(dir.path()).unwrap();
        let key = test_key();

        let state = CompactionState {
            summary: "<summary>test</summary>".into(),
            files_touched: vec![],
            compaction_count: 1,
            tokens_at_compaction: 100_000,
            created_at: chrono::Utc::now(),
            messages_at_compaction: Some(42),
        };

        store.save(&key, &state).unwrap();
        let loaded = store.load(&key).unwrap().unwrap();
        assert_eq!(loaded.summary, state.summary);
        assert_eq!(loaded.tokens_at_compaction, state.tokens_at_compaction);
        assert_eq!(loaded.messages_at_compaction, Some(42));
        assert_eq!(loaded.compaction_count, 1);
    }

    #[test]
    fn load_missing_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        let store = CompactionStore::new(dir.path()).unwrap();
        let key = test_key();

        assert!(store.load(&key).unwrap().is_none());
    }

    #[test]
    fn delete_removes_file() {
        let dir = tempfile::tempdir().unwrap();
        let store = CompactionStore::new(dir.path()).unwrap();
        let key = test_key();

        let state = CompactionState {
            summary: "test".into(),
            files_touched: vec![],
            compaction_count: 0,
            tokens_at_compaction: 100_000,
            created_at: chrono::Utc::now(),
            messages_at_compaction: Some(10),
        };

        store.save(&key, &state).unwrap();
        assert!(store.load(&key).unwrap().is_some());

        store.delete(&key).unwrap();
        assert!(store.load(&key).unwrap().is_none());
    }

    #[test]
    fn delete_nonexistent_is_ok() {
        let dir = tempfile::tempdir().unwrap();
        let store = CompactionStore::new(dir.path()).unwrap();
        let key = test_key();

        // Should not error
        store.delete(&key).unwrap();
    }
}
