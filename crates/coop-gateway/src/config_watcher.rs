use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio_util::sync::CancellationToken;
use tracing::{Instrument, debug, info, info_span, warn};

use crate::config::{Config, SharedConfig};
use crate::config_check;

/// Spawn a background task that polls `config_path` for changes and
/// hot-swaps the `SharedConfig` when the file is modified.
///
/// Fields that require a process restart (`agent.id`, `agent.workspace`,
/// `provider.name`, `channels`, `memory.db_path`, `memory.embedding`) are
/// guarded — the reload is rejected if any of those change.
///
/// If `cron_notify` is provided, it is notified whenever cron entries change
/// so the scheduler can wake from its sleep and re-evaluate.
pub(crate) fn spawn_config_watcher(
    config_path: PathBuf,
    config: SharedConfig,
    shutdown: CancellationToken,
    cron_notify: Option<Arc<tokio::sync::Notify>>,
) -> tokio::task::JoinHandle<()> {
    let span = info_span!("config_watcher", path = %config_path.display());
    tokio::spawn(
        async move {
            config_poll_loop(&config_path, &config, shutdown, cron_notify.as_deref()).await;
        }
        .instrument(span),
    )
}

const POLL_INTERVAL: Duration = Duration::from_secs(2);
const DEBOUNCE: Duration = Duration::from_millis(200);

async fn config_poll_loop(
    config_path: &Path,
    config: &SharedConfig,
    shutdown: CancellationToken,
    cron_notify: Option<&tokio::sync::Notify>,
) {
    let mut last_hash = file_content_hash(config_path);
    info!("config watcher started");

    loop {
        tokio::select! {
            () = tokio::time::sleep(POLL_INTERVAL) => {}
            () = shutdown.cancelled() => {
                debug!("config watcher stopped");
                return;
            }
        }

        if file_content_hash(config_path) == last_hash {
            continue;
        }

        // Debounce: editors often write-rename-delete in quick succession.
        tokio::time::sleep(DEBOUNCE).await;
        // Re-read after debounce in case another write landed.
        last_hash = file_content_hash(config_path);

        let old_cron = config.load().cron.clone();
        try_reload(config_path, config);

        if let Some(notify) = cron_notify
            && config.load().cron != old_cron
        {
            notify.notify_one();
        }
    }
}

/// Cheap content hash — avoids mtime granularity issues on CI/tmpfs.
fn file_content_hash(path: &Path) -> u64 {
    use std::hash::{Hash, Hasher};
    let Ok(bytes) = std::fs::read(path) else {
        return 0;
    };
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    bytes.hash(&mut hasher);
    hasher.finish()
}

fn try_reload(config_path: &Path, config: &SharedConfig) {
    let new_config = match Config::load(config_path) {
        Ok(c) => c,
        Err(e) => {
            warn!(error = %e, "config reload failed: parse error");
            return;
        }
    };

    let config_dir = config_path.parent().unwrap_or_else(|| Path::new("."));
    let report = config_check::validate_config(config_path, config_dir);
    // Filter out environment-dependent checks (API keys) — if the server
    // started successfully the env was already validated; these can't change
    // via config file modification and would block every hot-reload in
    // environments where the env var isn't re-exported to the checker.
    let config_errors: Vec<_> = report
        .results
        .iter()
        .filter(|r| {
            !r.passed
                && r.severity == config_check::Severity::Error
                && r.name != "api_key_present"
                && r.name != "memory_embedding_api_key"
        })
        .collect();
    if !config_errors.is_empty() {
        let errors: Vec<_> = config_errors
            .iter()
            .map(|r| format!("{}: {}", r.name, r.message))
            .collect();
        warn!(errors = ?errors, "config reload rejected: validation errors");
        return;
    }

    let current = config.load();
    if let Some(reasons) = check_restart_only_fields(&current, &new_config) {
        warn!(
            fields = ?reasons,
            "config reload rejected: these fields require a restart"
        );
        return;
    }

    if *current.as_ref() == new_config {
        debug!("config file changed but content is identical, skipping reload");
        return;
    }

    let changed = diff_sections(&current, &new_config);
    config.store(Arc::new(new_config));
    info!(changed = ?changed, "config reloaded");
}

/// Returns `Some(reasons)` if any restart-only fields differ.
fn check_restart_only_fields(current: &Config, new: &Config) -> Option<Vec<&'static str>> {
    let mut reasons = Vec::new();

    if new.agent.id != current.agent.id {
        reasons.push("agent.id");
    }
    if new.agent.workspace != current.agent.workspace {
        reasons.push("agent.workspace");
    }
    if new.provider.name != current.provider.name {
        reasons.push("provider.name");
    }
    if new.channels != current.channels {
        reasons.push("channels");
    }
    if new.memory.db_path != current.memory.db_path {
        reasons.push("memory.db_path");
    }
    if new.memory.embedding != current.memory.embedding {
        reasons.push("memory.embedding");
    }

    if reasons.is_empty() {
        None
    } else {
        Some(reasons)
    }
}

/// Summarize which top-level sections changed.
fn diff_sections(current: &Config, new: &Config) -> Vec<&'static str> {
    let mut changed = Vec::new();
    if new.users != current.users {
        changed.push("users");
    }
    if new.agent.model != current.agent.model {
        changed.push("agent.model");
    }
    if new.memory.prompt_index != current.memory.prompt_index {
        changed.push("memory.prompt_index");
    }
    if new.memory.retention != current.memory.retention {
        changed.push("memory.retention");
    }
    if new.prompt != current.prompt {
        changed.push("prompt");
    }
    if new.cron != current.cron {
        changed.push("cron");
    }
    changed
}

#[allow(clippy::unwrap_used)]
#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::shared_config;
    use std::fs;

    fn write_config(dir: &Path, toml_str: &str) -> PathBuf {
        let path = dir.join("coop.toml");
        fs::write(&path, toml_str).unwrap();
        path
    }

    fn minimal_toml(id: &str, model: &str, workspace: &str) -> String {
        format!(
            "[agent]\nid = \"{id}\"\nmodel = \"{model}\"\nworkspace = \"{workspace}\"\n\n[provider]\nname = \"anthropic\"\n"
        )
    }

    fn setup_workspace(dir: &Path) -> PathBuf {
        let ws = dir.join("workspace");
        fs::create_dir_all(&ws).unwrap();
        fs::write(ws.join("SOUL.md"), "test").unwrap();
        ws
    }

    #[test]
    fn check_restart_only_rejects_agent_id_change() {
        let ws = "/tmp/ws";
        let a: Config = toml::from_str(&minimal_toml("a", "m", ws)).unwrap();
        let b: Config = toml::from_str(&minimal_toml("b", "m", ws)).unwrap();
        let reasons = check_restart_only_fields(&a, &b).unwrap();
        assert!(reasons.contains(&"agent.id"));
    }

    #[test]
    fn check_restart_only_rejects_workspace_change() {
        let a: Config = toml::from_str(&minimal_toml("a", "m", "/ws1")).unwrap();
        let b: Config = toml::from_str(&minimal_toml("a", "m", "/ws2")).unwrap();
        let reasons = check_restart_only_fields(&a, &b).unwrap();
        assert!(reasons.contains(&"agent.workspace"));
    }

    #[test]
    fn check_restart_only_allows_user_changes() {
        let a: Config = toml::from_str(
            "[agent]\nid = \"a\"\nmodel = \"m\"\n\n[[users]]\nname = \"alice\"\ntrust = \"full\"\nmatch = []\n",
        )
        .unwrap();
        let b: Config = toml::from_str(
            "[agent]\nid = \"a\"\nmodel = \"m\"\n\n[[users]]\nname = \"bob\"\ntrust = \"inner\"\nmatch = []\n",
        )
        .unwrap();
        assert!(check_restart_only_fields(&a, &b).is_none());
    }

    #[test]
    fn check_restart_only_allows_model_change() {
        let a: Config = toml::from_str("[agent]\nid = \"a\"\nmodel = \"model-a\"\n").unwrap();
        let b: Config = toml::from_str("[agent]\nid = \"a\"\nmodel = \"model-b\"\n").unwrap();
        assert!(check_restart_only_fields(&a, &b).is_none());
    }

    #[test]
    fn diff_sections_detects_user_changes() {
        let a: Config = toml::from_str(
            "[agent]\nid = \"a\"\nmodel = \"m\"\n\n[[users]]\nname = \"alice\"\ntrust = \"full\"\nmatch = []\n",
        )
        .unwrap();
        let b: Config = toml::from_str(
            "[agent]\nid = \"a\"\nmodel = \"m\"\n\n[[users]]\nname = \"bob\"\ntrust = \"inner\"\nmatch = []\n",
        )
        .unwrap();
        let changed = diff_sections(&a, &b);
        assert!(changed.contains(&"users"));
    }

    #[test]
    fn diff_sections_detects_model_change() {
        let a: Config = toml::from_str("[agent]\nid = \"a\"\nmodel = \"model-a\"\n").unwrap();
        let b: Config = toml::from_str("[agent]\nid = \"a\"\nmodel = \"model-b\"\n").unwrap();
        let changed = diff_sections(&a, &b);
        assert!(changed.contains(&"agent.model"));
    }

    #[test]
    fn diff_sections_empty_when_identical() {
        let a: Config = toml::from_str("[agent]\nid = \"a\"\nmodel = \"m\"\n").unwrap();
        let b: Config = toml::from_str("[agent]\nid = \"a\"\nmodel = \"m\"\n").unwrap();
        let changed = diff_sections(&a, &b);
        assert!(changed.is_empty());
    }

    #[test]
    fn diff_sections_detects_prompt_change() {
        let a: Config = toml::from_str("[agent]\nid = \"a\"\nmodel = \"m\"\n").unwrap();
        let b: Config = toml::from_str(
            "[agent]\nid = \"a\"\nmodel = \"m\"\n\n[[prompt.shared_files]]\npath = \"SOUL.md\"\n",
        )
        .unwrap();
        let changed = diff_sections(&a, &b);
        assert!(changed.contains(&"prompt"));
    }

    #[test]
    fn try_reload_rejects_invalid_toml() {
        let dir = tempfile::tempdir().unwrap();
        let ws = setup_workspace(dir.path());
        let toml_str = minimal_toml("test", "test-model", &ws.display().to_string());
        let path = write_config(dir.path(), &toml_str);
        let config = shared_config(Config::load(&path).unwrap());

        // Overwrite with garbage
        fs::write(&path, "{{not toml").unwrap();
        try_reload(&path, &config);

        // Config should be unchanged
        assert_eq!(config.load().agent.id, "test");
    }

    #[test]
    fn try_reload_rejects_restart_only_change() {
        let dir = tempfile::tempdir().unwrap();
        let ws = setup_workspace(dir.path());
        let toml_str = minimal_toml("test", "test-model", &ws.display().to_string());
        let path = write_config(dir.path(), &toml_str);
        let config = shared_config(Config::load(&path).unwrap());

        // Change agent.id (restart-only)
        let new_toml = minimal_toml("changed", "test-model", &ws.display().to_string());
        fs::write(&path, &new_toml).unwrap();
        try_reload(&path, &config);

        // Config should be unchanged
        assert_eq!(config.load().agent.id, "test");
    }

    #[test]
    fn try_reload_accepts_user_change() {
        let dir = tempfile::tempdir().unwrap();
        let ws = setup_workspace(dir.path());
        let toml_str = format!(
            "[agent]\nid = \"test\"\nmodel = \"test-model\"\nworkspace = \"{}\"\n\n[provider]\nname = \"anthropic\"\n\n[[users]]\nname = \"alice\"\ntrust = \"full\"\nmatch = []\n",
            ws.display()
        );
        let path = write_config(dir.path(), &toml_str);
        let config = shared_config(Config::load(&path).unwrap());
        assert_eq!(config.load().users.len(), 1);
        assert_eq!(config.load().users[0].name, "alice");

        // Add a user
        let new_toml = format!(
            "[agent]\nid = \"test\"\nmodel = \"test-model\"\nworkspace = \"{}\"\n\n[provider]\nname = \"anthropic\"\n\n[[users]]\nname = \"alice\"\ntrust = \"full\"\nmatch = []\n\n[[users]]\nname = \"bob\"\ntrust = \"inner\"\nmatch = []\n",
            ws.display()
        );
        fs::write(&path, &new_toml).unwrap();
        try_reload(&path, &config);

        // Config should be updated
        assert_eq!(config.load().users.len(), 2);
        assert_eq!(config.load().users[1].name, "bob");
    }

    #[test]
    fn try_reload_skips_identical_content() {
        let dir = tempfile::tempdir().unwrap();
        let ws = setup_workspace(dir.path());
        let toml_str = minimal_toml("test", "test-model", &ws.display().to_string());
        let path = write_config(dir.path(), &toml_str);
        let config = shared_config(Config::load(&path).unwrap());

        // "touch" the file but don't change content — re-write same toml
        fs::write(&path, &toml_str).unwrap();
        // This should not log "config reloaded" (no way to assert that here,
        // but it exercises the identical-content code path)
        try_reload(&path, &config);

        assert_eq!(config.load().agent.id, "test");
    }

    #[tokio::test]
    async fn poll_loop_detects_change() {
        let dir = tempfile::tempdir().unwrap();
        let ws = setup_workspace(dir.path());
        let toml_str = format!(
            "[agent]\nid = \"test\"\nmodel = \"test-model\"\nworkspace = \"{}\"\n\n[provider]\nname = \"anthropic\"\n\n[[users]]\nname = \"alice\"\ntrust = \"full\"\nmatch = []\n",
            ws.display()
        );
        let path = write_config(dir.path(), &toml_str);
        let config = shared_config(Config::load(&path).unwrap());
        let shutdown = CancellationToken::new();

        let handle =
            spawn_config_watcher(path.clone(), Arc::clone(&config), shutdown.clone(), None);

        // Wait for the watcher to start
        tokio::time::sleep(Duration::from_millis(100)).await;

        // Modify the config
        let new_toml = format!(
            "[agent]\nid = \"test\"\nmodel = \"test-model\"\nworkspace = \"{}\"\n\n[provider]\nname = \"anthropic\"\n\n[[users]]\nname = \"alice\"\ntrust = \"full\"\nmatch = []\n\n[[users]]\nname = \"bob\"\ntrust = \"inner\"\nmatch = []\n",
            ws.display()
        );
        fs::write(&path, &new_toml).unwrap();

        // Wait for the poll + debounce
        tokio::time::sleep(Duration::from_secs(3)).await;

        assert_eq!(config.load().users.len(), 2);

        shutdown.cancel();
        let _ = tokio::time::timeout(Duration::from_secs(1), handle).await;
    }

    #[tokio::test]
    async fn poll_loop_notifies_on_cron_change() {
        let dir = tempfile::tempdir().unwrap();
        let ws = setup_workspace(dir.path());
        let toml_str = format!(
            "[agent]\nid = \"test\"\nmodel = \"test-model\"\nworkspace = \"{}\"\n\n[provider]\nname = \"anthropic\"\n",
            ws.display()
        );
        let path = write_config(dir.path(), &toml_str);
        let config = shared_config(Config::load(&path).unwrap());
        let shutdown = CancellationToken::new();
        let notify = Arc::new(tokio::sync::Notify::new());

        let handle = spawn_config_watcher(
            path.clone(),
            Arc::clone(&config),
            shutdown.clone(),
            Some(Arc::clone(&notify)),
        );

        tokio::time::sleep(Duration::from_millis(100)).await;

        // Add a cron entry — should trigger the notify.
        let new_toml = format!(
            "[agent]\nid = \"test\"\nmodel = \"test-model\"\nworkspace = \"{}\"\n\n[provider]\nname = \"anthropic\"\n\n[[cron]]\nname = \"test\"\ncron = \"*/30 * * * *\"\nmessage = \"hello\"\n",
            ws.display()
        );
        fs::write(&path, &new_toml).unwrap();

        // The notify should fire within poll interval + debounce (~2.2s).
        let result = tokio::time::timeout(Duration::from_secs(5), notify.notified()).await;
        assert!(
            result.is_ok(),
            "notify should fire when cron entries change"
        );

        shutdown.cancel();
        let _ = tokio::time::timeout(Duration::from_secs(1), handle).await;
    }

    #[tokio::test]
    async fn poll_loop_does_not_notify_on_non_cron_change() {
        let dir = tempfile::tempdir().unwrap();
        let ws = setup_workspace(dir.path());
        let toml_str = format!(
            "[agent]\nid = \"test\"\nmodel = \"test-model\"\nworkspace = \"{}\"\n\n[provider]\nname = \"anthropic\"\n",
            ws.display()
        );
        let path = write_config(dir.path(), &toml_str);
        let config = shared_config(Config::load(&path).unwrap());
        let shutdown = CancellationToken::new();
        let notify = Arc::new(tokio::sync::Notify::new());

        let handle = spawn_config_watcher(
            path.clone(),
            Arc::clone(&config),
            shutdown.clone(),
            Some(Arc::clone(&notify)),
        );

        tokio::time::sleep(Duration::from_millis(100)).await;

        // Change users but NOT cron — should NOT trigger notify.
        let new_toml = format!(
            "[agent]\nid = \"test\"\nmodel = \"test-model\"\nworkspace = \"{}\"\n\n[provider]\nname = \"anthropic\"\n\n[[users]]\nname = \"alice\"\ntrust = \"full\"\nmatch = []\n",
            ws.display()
        );
        fs::write(&path, &new_toml).unwrap();

        // Wait for poll + debounce to process the change.
        tokio::time::sleep(Duration::from_secs(3)).await;

        // Config should be updated (users changed)...
        assert_eq!(config.load().users.len(), 1);

        // ...but notify should NOT have fired (cron didn't change).
        let result = tokio::time::timeout(Duration::from_millis(100), notify.notified()).await;
        assert!(
            result.is_err(),
            "notify should not fire when only users change"
        );

        shutdown.cancel();
        let _ = tokio::time::timeout(Duration::from_secs(1), handle).await;
    }

    #[tokio::test]
    async fn poll_loop_stops_on_shutdown() {
        let dir = tempfile::tempdir().unwrap();
        let ws = setup_workspace(dir.path());
        let toml_str = minimal_toml("test", "test-model", &ws.display().to_string());
        let path = write_config(dir.path(), &toml_str);
        let config = shared_config(Config::load(&path).unwrap());
        let shutdown = CancellationToken::new();

        let handle = spawn_config_watcher(path, Arc::clone(&config), shutdown.clone(), None);

        tokio::time::sleep(Duration::from_millis(100)).await;
        shutdown.cancel();

        let result = tokio::time::timeout(Duration::from_secs(2), handle).await;
        assert!(result.is_ok(), "watcher should stop promptly on shutdown");
    }
}
