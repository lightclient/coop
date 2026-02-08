use anyhow::{Context, Result};
use coop_core::TrustLevel;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use tracing::debug;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct Config {
    pub agent: AgentConfig,
    #[serde(default)]
    pub users: Vec<UserConfig>,
    #[serde(default)]
    pub channels: ChannelsConfig,
    #[serde(default)]
    pub provider: ProviderConfig,
    #[serde(default)]
    pub memory: MemoryConfig,
    #[serde(default)]
    pub cron: Vec<CronConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct AgentConfig {
    pub id: String,
    pub model: String,
    #[serde(default = "default_workspace")]
    pub workspace: String,
}

fn default_workspace() -> String {
    "./workspaces/default".to_owned()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct UserConfig {
    pub name: String,
    pub trust: TrustLevel,
    #[serde(default)]
    pub r#match: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub(crate) struct ChannelsConfig {
    #[serde(default)]
    pub signal: Option<SignalChannelConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct SignalChannelConfig {
    pub db_path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub(crate) struct ProviderConfig {
    #[serde(default = "default_provider")]
    pub name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct MemoryConfig {
    #[serde(default = "default_memory_db_path")]
    pub db_path: String,
    #[serde(default)]
    pub embedding: Option<MemoryEmbeddingConfig>,
}

impl Default for MemoryConfig {
    fn default() -> Self {
        Self {
            db_path: default_memory_db_path(),
            embedding: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct MemoryEmbeddingConfig {
    pub provider: String,
    pub model: String,
    pub dimensions: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct CronDelivery {
    /// Channel to deliver through (e.g. "signal").
    pub channel: String,
    /// Target on that channel (e.g. a UUID for DM, "group:<hex>" for group).
    pub target: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct CronConfig {
    pub name: String,
    pub cron: String,
    pub message: String,
    #[serde(default)]
    pub user: Option<String>,
    #[serde(default)]
    pub deliver: Option<CronDelivery>,
}

fn default_provider() -> String {
    "anthropic".to_owned()
}

fn default_memory_db_path() -> String {
    "./data/memory.db".to_owned()
}

impl Config {
    /// Load config from a YAML file.
    pub(crate) fn load(path: &Path) -> Result<Self> {
        let content = std::fs::read_to_string(path)
            .with_context(|| format!("failed to read config file: {}", path.display()))?;
        let config: Config = serde_yaml::from_str(&content)
            .with_context(|| format!("failed to parse config file: {}", path.display()))?;
        Ok(config)
    }

    /// Resolve the workspace directory to an absolute path.
    ///
    /// Fails if the directory does not exist.
    pub(crate) fn resolve_workspace(&self, base_dir: &Path) -> Result<PathBuf> {
        let workspace = PathBuf::from(&self.agent.workspace);
        let resolved = if workspace.is_absolute() {
            workspace
        } else {
            base_dir.join(workspace)
        };
        anyhow::ensure!(
            resolved.is_dir(),
            "workspace directory not found: {}",
            resolved.display()
        );
        debug!(workspace = %resolved.display(), "resolved workspace path");
        Ok(resolved)
    }

    /// Resolve config path: check arg, then default locations.
    pub(crate) fn find_config_path(explicit: Option<&str>) -> PathBuf {
        if let Some(p) = explicit {
            return PathBuf::from(p);
        }

        // Check current directory
        let local = PathBuf::from("coop.yaml");
        if local.exists() {
            return local;
        }

        // Check XDG config
        if let Ok(config_dir) = std::env::var("XDG_CONFIG_HOME") {
            let xdg = PathBuf::from(config_dir).join("coop/coop.yaml");
            if xdg.exists() {
                return xdg;
            }
        }

        // Check ~/.config/coop
        if let Ok(home) = std::env::var("HOME") {
            let home_config = PathBuf::from(home).join(".config/coop/coop.yaml");
            if home_config.exists() {
                return home_config;
            }
        }

        // Default to local
        local
    }
}

#[allow(clippy::unwrap_used)]
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_minimal_config() {
        let yaml = "
agent:
  id: test
  model: anthropic/claude-sonnet-4-20250514
";
        let config: Config = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(config.agent.id, "test");
        assert_eq!(config.agent.model, "anthropic/claude-sonnet-4-20250514");
        assert!(config.users.is_empty());
        assert!(config.channels.signal.is_none());
        assert_eq!(config.memory.db_path, "./data/memory.db");
        assert!(config.cron.is_empty());
    }

    #[test]
    fn parse_full_config() {
        let yaml = "
agent:
  id: reid
  model: anthropic/claude-sonnet-4-20250514
  workspace: ./workspaces/default

users:
  - name: alice
    trust: full
    match: ['terminal:default']
  - name: bob
    trust: inner
    match: ['signal:bob-uuid']

channels:
  signal:
    db_path: ./data/signal.db

provider:
  name: anthropic
";
        let config: Config = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(config.agent.id, "reid");
        assert_eq!(config.users.len(), 2);
        assert_eq!(config.users[0].trust, TrustLevel::Full);
        assert_eq!(config.users[1].trust, TrustLevel::Inner);
        assert_eq!(
            config.channels.signal.unwrap().db_path,
            "./data/signal.db".to_owned()
        );
        assert_eq!(config.provider.name, "anthropic");
        assert_eq!(config.memory.db_path, "./data/memory.db");
    }

    #[test]
    fn parse_config_with_cron() {
        let yaml = "
agent:
  id: coop
  model: test
cron:
  - name: heartbeat
    cron: '*/30 * * * *'
    user: alice
    message: check HEARTBEAT.md
  - name: cleanup
    cron: '0 3 * * *'
    message: run cleanup
";
        let config: Config = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(config.cron.len(), 2);
        assert_eq!(config.cron[0].name, "heartbeat");
        assert_eq!(config.cron[0].cron, "*/30 * * * *");
        assert_eq!(config.cron[0].user.as_deref(), Some("alice"));
        assert_eq!(config.cron[0].message, "check HEARTBEAT.md");
        assert!(config.cron[0].deliver.is_none());
        assert_eq!(config.cron[1].name, "cleanup");
        assert!(config.cron[1].user.is_none());
        assert!(config.cron[1].deliver.is_none());
    }

    #[test]
    fn parse_config_with_cron_delivery() {
        let yaml = "
agent:
  id: coop
  model: test
cron:
  - name: morning-briefing
    cron: '0 8 * * *'
    user: alice
    deliver:
      channel: signal
      target: alice-uuid
    message: Morning briefing
";
        let config: Config = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(config.cron.len(), 1);
        let delivery = config.cron[0].deliver.as_ref().unwrap();
        assert_eq!(delivery.channel, "signal");
        assert_eq!(delivery.target, "alice-uuid");
    }

    #[test]
    fn parse_config_with_cron_delivery_group_target() {
        let yaml = "
agent:
  id: coop
  model: test
cron:
  - name: weekly-review
    cron: '0 18 * * 5'
    user: alice
    deliver:
      channel: signal
      target: 'group:deadbeef00112233445566778899aabbccddeeff00112233445566778899aabb'
    message: Weekly review
";
        let config: Config = serde_yaml::from_str(yaml).unwrap();
        let delivery = config.cron[0].deliver.as_ref().unwrap();
        assert_eq!(delivery.channel, "signal");
        assert!(delivery.target.starts_with("group:"));
    }

    #[test]
    fn parse_config_with_memory_settings() {
        let yaml = "
agent:
  id: coop
  model: test
memory:
  db_path: ./state/memory.db
  embedding:
    provider: voyage
    model: voyage-3-large
    dimensions: 1024
";
        let config: Config = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(config.memory.db_path, "./state/memory.db");
        let embedding = config.memory.embedding.as_ref().unwrap();
        assert_eq!(embedding.provider, "voyage");
        assert_eq!(embedding.model, "voyage-3-large");
        assert_eq!(embedding.dimensions, 1024);
    }

    #[test]
    fn resolve_workspace_fails_for_missing_dir() {
        let config: Config = serde_yaml::from_str(
            "
agent:
  id: test
  model: test
  workspace: ./does-not-exist
",
        )
        .unwrap();

        let err = config.resolve_workspace(Path::new("/tmp")).unwrap_err();
        assert!(
            err.to_string().contains("workspace directory not found"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn resolve_workspace_succeeds_for_existing_dir() {
        let dir = tempfile::tempdir().unwrap();
        let config: Config = serde_yaml::from_str(&format!(
            "agent:\n  id: test\n  model: test\n  workspace: {}",
            dir.path().display()
        ))
        .unwrap();

        let resolved = config.resolve_workspace(Path::new("/unused")).unwrap();
        assert_eq!(resolved, dir.path());
    }
}
