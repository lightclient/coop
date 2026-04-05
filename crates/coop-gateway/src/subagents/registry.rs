use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use coop_core::SessionKey;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use uuid::Uuid;

use super::SubagentMode;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum SubagentRunStatus {
    Queued,
    Running,
    Completed,
    Failed,
    Cancelled,
    TimedOut,
}

impl SubagentRunStatus {
    pub(crate) fn is_active(self) -> bool {
        matches!(self, Self::Queued | Self::Running)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct SubagentRunRecord {
    pub run_id: Uuid,
    pub child_session_key: SessionKey,
    pub parent_session_key: SessionKey,
    pub parent_run_id: Option<Uuid>,
    pub requesting_user: Option<String>,
    pub task: String,
    pub profile: Option<String>,
    pub model: String,
    pub mode: SubagentMode,
    pub tool_names: Vec<String>,
    pub status: SubagentRunStatus,
    pub depth: u32,
    pub created_at: DateTime<Utc>,
    pub started_at: Option<DateTime<Utc>>,
    pub ended_at: Option<DateTime<Utc>>,
    pub timeout_seconds: u64,
    pub max_turns: u32,
    pub paths: Vec<String>,
    pub artifact_paths: Vec<String>,
    pub summary: Option<String>,
    pub error: Option<String>,
}

impl SubagentRunRecord {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        run_id: Uuid,
        child_session_key: SessionKey,
        parent_session_key: SessionKey,
        parent_run_id: Option<Uuid>,
        requesting_user: Option<String>,
        task: String,
        profile: Option<String>,
        model: String,
        mode: SubagentMode,
        tool_names: Vec<String>,
        depth: u32,
        timeout_seconds: u64,
        max_turns: u32,
        paths: Vec<String>,
    ) -> Self {
        Self {
            run_id,
            child_session_key,
            parent_session_key,
            parent_run_id,
            requesting_user,
            task,
            profile,
            model,
            mode,
            tool_names,
            status: SubagentRunStatus::Queued,
            depth,
            created_at: Utc::now(),
            started_at: None,
            ended_at: None,
            timeout_seconds,
            max_turns,
            paths,
            artifact_paths: Vec::new(),
            summary: None,
            error: None,
        }
    }
}

#[derive(Debug)]
pub(crate) struct SubagentRegistry {
    path: PathBuf,
    records: Mutex<Vec<SubagentRunRecord>>,
}

impl SubagentRegistry {
    pub(crate) fn new(workspace: &Path) -> Result<Self> {
        let path = workspace.join(".coop-gateway").join("subagents.json");
        let records = load_records(&path)?;
        Ok(Self {
            path,
            records: Mutex::new(records),
        })
    }

    pub(crate) fn insert(&self, record: SubagentRunRecord) -> Result<()> {
        let snapshot = {
            let mut records = self
                .records
                .lock()
                .expect("subagent registry mutex poisoned");
            records.push(record);
            records.clone()
        };
        persist_records(&self.path, &snapshot)
    }

    pub(crate) fn all(&self) -> Vec<SubagentRunRecord> {
        self.records
            .lock()
            .expect("subagent registry mutex poisoned")
            .clone()
    }

    pub(crate) fn list_recent(&self) -> Vec<SubagentRunRecord> {
        let mut records = self.all();
        records.sort_by_key(|record| std::cmp::Reverse(record.created_at.timestamp_millis()));
        records
    }

    pub(crate) fn get(&self, run_id: Uuid) -> Option<SubagentRunRecord> {
        self.records
            .lock()
            .expect("subagent registry mutex poisoned")
            .iter()
            .find(|record| record.run_id == run_id)
            .cloned()
    }

    pub(crate) fn active_count(&self) -> usize {
        self.records
            .lock()
            .expect("subagent registry mutex poisoned")
            .iter()
            .filter(|record| record.status.is_active())
            .count()
    }

    pub(crate) fn child_run_ids(&self, parent_run_id: Uuid) -> Vec<Uuid> {
        self.records
            .lock()
            .expect("subagent registry mutex poisoned")
            .iter()
            .filter(|record| {
                record.parent_run_id == Some(parent_run_id) && record.status.is_active()
            })
            .map(|record| record.run_id)
            .collect()
    }

    pub(crate) fn active_run_ids_for_parent_session(
        &self,
        parent_session: &SessionKey,
    ) -> Vec<Uuid> {
        self.records
            .lock()
            .expect("subagent registry mutex poisoned")
            .iter()
            .filter(|record| {
                &record.parent_session_key == parent_session && record.status.is_active()
            })
            .map(|record| record.run_id)
            .collect()
    }

    pub(crate) fn mark_running(&self, run_id: Uuid) -> Result<Option<SubagentRunRecord>> {
        self.update(run_id, |record| {
            record.status = SubagentRunStatus::Running;
            record.started_at = Some(Utc::now());
        })
    }

    #[allow(clippy::needless_pass_by_value, clippy::assigning_clones)]
    pub(crate) fn finish(
        &self,
        run_id: Uuid,
        status: SubagentRunStatus,
        summary: Option<String>,
        artifact_paths: Vec<String>,
        error: Option<String>,
    ) -> Result<Option<SubagentRunRecord>> {
        self.update(run_id, |record| {
            record.status = status;
            record.ended_at = Some(Utc::now());
            record.summary = summary.clone();
            record.artifact_paths = artifact_paths.clone();
            record.error = error.clone();
        })
    }

    fn update<F>(&self, run_id: Uuid, mut update: F) -> Result<Option<SubagentRunRecord>>
    where
        F: FnMut(&mut SubagentRunRecord),
    {
        let (updated, snapshot) = {
            let mut records = self
                .records
                .lock()
                .expect("subagent registry mutex poisoned");
            let Some(record) = records.iter_mut().find(|record| record.run_id == run_id) else {
                return Ok(None);
            };
            update(record);
            (record.clone(), records.clone())
        };
        persist_records(&self.path, &snapshot)?;
        Ok(Some(updated))
    }
}

fn load_records(path: &Path) -> Result<Vec<SubagentRunRecord>> {
    match std::fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .with_context(|| format!("failed to parse {}", path.display())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(error) => Err(error).with_context(|| format!("failed to read {}", path.display())),
    }
}

fn persist_records(path: &Path, records: &[SubagentRunRecord]) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }

    let content = serde_json::to_vec_pretty(records)?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, content).with_context(|| format!("failed to write {}", tmp.display()))?;
    std::fs::rename(&tmp, path)
        .with_context(|| format!("failed to rename {} to {}", tmp.display(), path.display()))?;
    Ok(())
}

#[allow(clippy::unwrap_used)]
#[cfg(test)]
mod tests {
    use super::*;
    use coop_core::SessionKind;

    fn record(status: SubagentRunStatus) -> SubagentRunRecord {
        let run_id = Uuid::parse_str("123e4567-e89b-12d3-a456-426614174000").unwrap();
        SubagentRunRecord {
            run_id,
            child_session_key: SessionKey {
                agent_id: "coop".into(),
                kind: SessionKind::Subagent(run_id),
            },
            parent_session_key: SessionKey {
                agent_id: "coop".into(),
                kind: SessionKind::Main,
            },
            parent_run_id: None,
            requesting_user: Some("alice".into()),
            task: "test task".into(),
            profile: Some("code".into()),
            model: "gpt-5-codex".into(),
            mode: SubagentMode::Wait,
            tool_names: vec!["read_file".into()],
            status,
            depth: 1,
            created_at: Utc::now(),
            started_at: None,
            ended_at: None,
            timeout_seconds: 60,
            max_turns: 8,
            paths: vec!["./foo.txt".into()],
            artifact_paths: Vec::new(),
            summary: None,
            error: None,
        }
    }

    #[test]
    fn registry_round_trips_records() {
        let dir = tempfile::tempdir().unwrap();
        let registry = SubagentRegistry::new(dir.path()).unwrap();
        registry.insert(record(SubagentRunStatus::Queued)).unwrap();

        let reloaded = SubagentRegistry::new(dir.path()).unwrap();
        assert_eq!(reloaded.all().len(), 1);
        assert_eq!(reloaded.all()[0].status, SubagentRunStatus::Queued);
    }

    #[test]
    fn registry_updates_status_transitions() {
        let dir = tempfile::tempdir().unwrap();
        let registry = SubagentRegistry::new(dir.path()).unwrap();
        let original = record(SubagentRunStatus::Queued);
        let run_id = original.run_id;
        registry.insert(original).unwrap();

        let running = registry.mark_running(run_id).unwrap().unwrap();
        assert_eq!(running.status, SubagentRunStatus::Running);
        assert!(running.started_at.is_some());

        let completed = registry
            .finish(
                run_id,
                SubagentRunStatus::Completed,
                Some("done".into()),
                vec!["./result.txt".into()],
                None,
            )
            .unwrap()
            .unwrap();
        assert_eq!(completed.status, SubagentRunStatus::Completed);
        assert_eq!(completed.summary.as_deref(), Some("done"));
        assert_eq!(completed.artifact_paths, vec!["./result.txt"]);
        assert!(completed.ended_at.is_some());
    }

    #[test]
    fn active_count_only_includes_queued_and_running() {
        let dir = tempfile::tempdir().unwrap();
        let registry = SubagentRegistry::new(dir.path()).unwrap();
        registry.insert(record(SubagentRunStatus::Queued)).unwrap();

        let mut completed = record(SubagentRunStatus::Completed);
        completed.run_id = Uuid::parse_str("123e4567-e89b-12d3-a456-426614174001").unwrap();
        completed.child_session_key = SessionKey {
            agent_id: "coop".into(),
            kind: SessionKind::Subagent(completed.run_id),
        };
        registry.insert(completed).unwrap();

        assert_eq!(registry.active_count(), 1);
    }
}
