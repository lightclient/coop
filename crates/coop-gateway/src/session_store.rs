use anyhow::{Context, Result};
use coop_core::{Message, SessionKey};
use std::io::Write;
use std::path::{Path, PathBuf};
use tracing::{debug, warn};

/// Sync file-backed session store using JSONL format.
///
/// Each session is stored as `{dir}/{slug}.jsonl` where the slug is
/// derived from the session key. Used as a write-through backing store
/// for the gateway's in-memory session cache.
pub(crate) struct DiskSessionStore {
    dir: PathBuf,
}

impl DiskSessionStore {
    pub(crate) fn new(dir: impl AsRef<Path>) -> Result<Self> {
        let dir = dir.as_ref().to_path_buf();
        std::fs::create_dir_all(&dir)
            .with_context(|| format!("failed to create session dir: {}", dir.display()))?;
        Ok(Self { dir })
    }

    pub(crate) fn load(&self, key: &SessionKey) -> Result<Vec<Message>> {
        let path = self.path(key);
        match std::fs::read_to_string(&path) {
            Ok(content) => {
                let mut messages = Vec::new();
                for (i, line) in content.lines().enumerate() {
                    if line.trim().is_empty() {
                        continue;
                    }
                    match serde_json::from_str::<Message>(line) {
                        Ok(msg) => messages.push(msg),
                        Err(e) => {
                            warn!(
                                path = %path.display(),
                                line = i + 1,
                                error = %e,
                                "skipping corrupt message in session file"
                            );
                        }
                    }
                }
                debug!(session = %key, count = messages.len(), "loaded session from disk");
                Ok(messages)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
            Err(e) => Err(e).with_context(|| format!("failed to read {}", path.display())),
        }
    }

    pub(crate) fn append(&self, key: &SessionKey, message: &Message) -> Result<()> {
        let path = self.path(key);
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .with_context(|| format!("failed to open {}", path.display()))?;
        let line = serde_json::to_string(message)?;
        writeln!(file, "{line}")?;
        Ok(())
    }

    pub(crate) fn replace(&self, key: &SessionKey, messages: &[Message]) -> Result<()> {
        let path = self.path(key);
        let mut content = String::new();
        for msg in messages {
            content.push_str(&serde_json::to_string(msg)?);
            content.push('\n');
        }
        std::fs::write(&path, &content)
            .with_context(|| format!("failed to write {}", path.display()))?;
        Ok(())
    }

    pub(crate) fn delete(&self, key: &SessionKey) -> Result<()> {
        let path = self.path(key);
        match std::fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e).with_context(|| format!("failed to delete {}", path.display())),
        }
    }

    fn path(&self, key: &SessionKey) -> PathBuf {
        let slug = key.to_string().replace(['/', ':'], "_");
        self.dir.join(format!("{slug}.jsonl"))
    }
}

#[allow(clippy::unwrap_used)]
#[cfg(test)]
mod tests {
    use super::*;
    use coop_core::{SessionKey, SessionKind};

    fn test_key() -> SessionKey {
        SessionKey {
            agent_id: "coop".to_owned(),
            kind: SessionKind::Dm("signal:alice-uuid".to_owned()),
        }
    }

    #[test]
    fn load_returns_empty_for_missing_file() {
        let dir = tempfile::tempdir().unwrap();
        let store = DiskSessionStore::new(dir.path()).unwrap();
        let messages = store.load(&test_key()).unwrap();
        assert!(messages.is_empty());
    }

    #[test]
    fn append_and_load_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let store = DiskSessionStore::new(dir.path()).unwrap();
        let key = test_key();

        let msg1 = Message::user().with_text("hello");
        let msg2 = Message::assistant().with_text("hi back");
        store.append(&key, &msg1).unwrap();
        store.append(&key, &msg2).unwrap();

        let loaded = store.load(&key).unwrap();
        assert_eq!(loaded.len(), 2);
        assert_eq!(loaded[0].text(), "hello");
        assert_eq!(loaded[1].text(), "hi back");
    }

    #[test]
    fn replace_overwrites_all_messages() {
        let dir = tempfile::tempdir().unwrap();
        let store = DiskSessionStore::new(dir.path()).unwrap();
        let key = test_key();

        store
            .append(&key, &Message::user().with_text("old"))
            .unwrap();
        store
            .replace(&key, &[Message::user().with_text("new")])
            .unwrap();

        let loaded = store.load(&key).unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].text(), "new");
    }

    #[test]
    fn delete_removes_file() {
        let dir = tempfile::tempdir().unwrap();
        let store = DiskSessionStore::new(dir.path()).unwrap();
        let key = test_key();

        store
            .append(&key, &Message::user().with_text("bye"))
            .unwrap();
        store.delete(&key).unwrap();

        let loaded = store.load(&key).unwrap();
        assert!(loaded.is_empty());
    }

    #[test]
    fn delete_missing_file_is_ok() {
        let dir = tempfile::tempdir().unwrap();
        let store = DiskSessionStore::new(dir.path()).unwrap();
        store.delete(&test_key()).unwrap();
    }

    #[test]
    fn corrupt_line_is_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let store = DiskSessionStore::new(dir.path()).unwrap();
        let key = test_key();

        // Write a valid message then a corrupt line
        store
            .append(&key, &Message::user().with_text("good"))
            .unwrap();
        let path = store.path(&key);
        std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(b"not valid json\n")
            .unwrap();

        let loaded = store.load(&key).unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].text(), "good");
    }

    #[test]
    fn session_path_is_deterministic() {
        let dir = tempfile::tempdir().unwrap();
        let store = DiskSessionStore::new(dir.path()).unwrap();
        let key = test_key();
        let path = store.path(&key);
        assert!(path.to_string_lossy().ends_with(".jsonl"));
        assert_eq!(path, store.path(&key));
    }
}
