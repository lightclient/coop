use anyhow::{Context, Result};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use tracing::debug;

pub(crate) struct UserModelStore {
    path: PathBuf,
    overrides: Mutex<HashMap<String, String>>,
}

impl UserModelStore {
    pub(crate) fn new(workspace: &Path) -> Result<Self> {
        let path = workspace.join("user-models.json");
        let overrides = load_overrides(&path)?;
        Ok(Self {
            path,
            overrides: Mutex::new(overrides),
        })
    }

    pub(crate) fn get(&self, user_name: &str) -> Option<String> {
        self.overrides
            .lock()
            .expect("user model store mutex poisoned")
            .get(user_name)
            .cloned()
    }

    pub(crate) fn set(&self, user_name: &str, model: &str) -> Result<()> {
        let snapshot = {
            let mut overrides = self
                .overrides
                .lock()
                .expect("user model store mutex poisoned");
            overrides.insert(user_name.to_owned(), model.to_owned());
            overrides.clone()
        };
        persist_overrides(&self.path, &snapshot)?;
        debug!(user = %user_name, model = %model, "persisted user model override");
        Ok(())
    }

    pub(crate) fn clear(&self, user_name: &str) -> Result<()> {
        let snapshot = {
            let mut overrides = self
                .overrides
                .lock()
                .expect("user model store mutex poisoned");
            overrides
                .remove(user_name)
                .is_some()
                .then(|| overrides.clone())
        };
        if let Some(snapshot) = snapshot {
            persist_overrides(&self.path, &snapshot)?;
            debug!(user = %user_name, "cleared user model override");
        }
        Ok(())
    }
}

fn load_overrides(path: &Path) -> Result<HashMap<String, String>> {
    match std::fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .with_context(|| format!("failed to parse {}", path.display())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(HashMap::new()),
        Err(error) => Err(error).with_context(|| format!("failed to read {}", path.display())),
    }
}

fn persist_overrides(path: &Path, overrides: &HashMap<String, String>) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }

    let content = serde_json::to_vec_pretty(overrides)?;
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

    #[test]
    fn missing_store_starts_empty() {
        let dir = tempfile::tempdir().unwrap();
        let store = UserModelStore::new(dir.path()).unwrap();
        assert_eq!(store.get("alice"), None);
    }

    #[test]
    fn set_and_reload_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let store = UserModelStore::new(dir.path()).unwrap();
        store
            .set("alice", "anthropic/claude-opus-4-0-20250514")
            .unwrap();

        let reloaded = UserModelStore::new(dir.path()).unwrap();
        assert_eq!(
            reloaded.get("alice").as_deref(),
            Some("anthropic/claude-opus-4-0-20250514")
        );
    }

    #[test]
    fn clear_removes_override() {
        let dir = tempfile::tempdir().unwrap();
        let store = UserModelStore::new(dir.path()).unwrap();
        store.set("alice", "gpt-5-mini").unwrap();
        store.clear("alice").unwrap();

        let reloaded = UserModelStore::new(dir.path()).unwrap();
        assert_eq!(reloaded.get("alice"), None);
    }
}
