use anyhow::{Context, Result};
use arc_swap::ArcSwap;
use coop_core::TrustLevel;
use coop_core::prompt::{CacheHint, PromptFileConfig};
use coop_memory::MemoryMaintenanceConfig;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tracing::debug;

/// Shared config handle. Readers call `.load()` for a lock-free snapshot.
pub(crate) type SharedConfig = Arc<ArcSwap<Config>>;

/// Wrap a `Config` in an `ArcSwap` for lock-free sharing.
pub(crate) fn shared_config(config: Config) -> SharedConfig {
    Arc::new(ArcSwap::from_pointee(config))
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct Config {
    pub agent: AgentConfig,
    #[serde(default)]
    pub users: Vec<UserConfig>,
    #[serde(default)]
    pub channels: ChannelsConfig,
    #[serde(default)]
    pub provider: ProviderConfig,
    #[serde(default)]
    pub prompt: PromptConfig,
    #[serde(default)]
    pub memory: MemoryConfig,
    #[serde(default)]
    pub cron: Vec<CronConfig>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct AgentConfig {
    pub id: String,
    pub model: String,
    #[serde(default = "default_workspace")]
    pub workspace: String,
}

fn default_workspace() -> String {
    "./workspaces/default".to_owned()
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct UserConfig {
    pub name: String,
    pub trust: TrustLevel,
    #[serde(default)]
    pub r#match: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub(crate) struct ChannelsConfig {
    #[serde(default)]
    pub signal: Option<SignalChannelConfig>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct SignalChannelConfig {
    pub db_path: String,
    /// When true, flush assistant text to the user on every tool-call
    /// boundary (each turn iteration sends a message). Default: false
    /// (one consolidated reply at the end of the turn).
    #[serde(default)]
    pub verbose: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub(crate) struct ProviderConfig {
    #[serde(default = "default_provider")]
    pub name: String,
    /// Key references with `env:` prefix (e.g. `env:ANTHROPIC_API_KEY`).
    /// Enables rotation on rate limits. When empty/omitted, falls back
    /// to ANTHROPIC_API_KEY env var.
    #[serde(default)]
    pub api_keys: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct PromptConfig {
    #[serde(default = "default_shared_files")]
    pub shared_files: Vec<PromptFileEntry>,
    #[serde(default = "default_user_files")]
    pub user_files: Vec<PromptFileEntry>,
}

impl Default for PromptConfig {
    fn default() -> Self {
        Self {
            shared_files: default_shared_files(),
            user_files: default_user_files(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct PromptFileEntry {
    pub path: String,
    #[serde(default = "default_file_trust")]
    pub trust: TrustLevel,
    #[serde(default = "default_file_cache")]
    pub cache: CacheHintConfig,
    #[serde(default)]
    pub description: Option<String>,
}

impl PromptFileEntry {
    pub(crate) fn to_core(&self) -> PromptFileConfig {
        let description = self.description.clone().unwrap_or_else(|| {
            self.path
                .strip_suffix(".md")
                .unwrap_or(&self.path)
                .to_owned()
        });
        PromptFileConfig {
            path: self.path.clone(),
            min_trust: self.trust,
            cache: self.cache.to_core(),
            description,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum CacheHintConfig {
    Stable,
    Session,
    Volatile,
}

impl CacheHintConfig {
    fn to_core(&self) -> CacheHint {
        match self {
            Self::Stable => CacheHint::Stable,
            Self::Session => CacheHint::Session,
            Self::Volatile => CacheHint::Volatile,
        }
    }
}

fn default_file_trust() -> TrustLevel {
    TrustLevel::Full
}

fn default_file_cache() -> CacheHintConfig {
    CacheHintConfig::Session
}

fn default_shared_files() -> Vec<PromptFileEntry> {
    vec![
        PromptFileEntry {
            path: "SOUL.md".into(),
            trust: TrustLevel::Familiar,
            cache: CacheHintConfig::Stable,
            description: Some("Agent personality and voice".into()),
        },
        PromptFileEntry {
            path: "IDENTITY.md".into(),
            trust: TrustLevel::Familiar,
            cache: CacheHintConfig::Session,
            description: Some("Agent identity".into()),
        },
        PromptFileEntry {
            path: "TOOLS.md".into(),
            trust: TrustLevel::Full,
            cache: CacheHintConfig::Session,
            description: Some("Tool setup notes".into()),
        },
    ]
}

fn default_user_files() -> Vec<PromptFileEntry> {
    vec![
        PromptFileEntry {
            path: "AGENTS.md".into(),
            trust: TrustLevel::Full,
            cache: CacheHintConfig::Stable,
            description: Some("Behavioral instructions".into()),
        },
        PromptFileEntry {
            path: "USER.md".into(),
            trust: TrustLevel::Inner,
            cache: CacheHintConfig::Session,
            description: Some("Per-user info".into()),
        },
        PromptFileEntry {
            path: "TOOLS.md".into(),
            trust: TrustLevel::Full,
            cache: CacheHintConfig::Session,
            description: Some("Per-user tool notes".into()),
        },
    ]
}

impl PromptConfig {
    pub(crate) fn shared_core_configs(&self) -> Vec<PromptFileConfig> {
        self.shared_files
            .iter()
            .map(PromptFileEntry::to_core)
            .collect()
    }

    pub(crate) fn user_core_configs(&self) -> Vec<PromptFileConfig> {
        self.user_files
            .iter()
            .map(PromptFileEntry::to_core)
            .collect()
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct MemoryConfig {
    #[serde(default = "default_memory_db_path")]
    pub db_path: String,
    #[serde(default)]
    pub embedding: Option<MemoryEmbeddingConfig>,
    #[serde(default)]
    pub prompt_index: MemoryPromptIndexConfig,
    #[serde(default)]
    pub retention: MemoryRetentionConfig,
}

impl Default for MemoryConfig {
    fn default() -> Self {
        Self {
            db_path: default_memory_db_path(),
            embedding: None,
            prompt_index: MemoryPromptIndexConfig::default(),
            retention: MemoryRetentionConfig::default(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct MemoryPromptIndexConfig {
    #[serde(default = "default_prompt_index_enabled")]
    pub enabled: bool,
    #[serde(default = "default_prompt_index_limit")]
    pub limit: usize,
    #[serde(default = "default_prompt_index_max_tokens")]
    pub max_tokens: usize,
}

impl Default for MemoryPromptIndexConfig {
    fn default() -> Self {
        Self {
            enabled: default_prompt_index_enabled(),
            limit: default_prompt_index_limit(),
            max_tokens: default_prompt_index_max_tokens(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct MemoryRetentionConfig {
    #[serde(default = "default_memory_retention_enabled")]
    pub enabled: bool,
    #[serde(default = "default_memory_archive_after_days")]
    pub archive_after_days: i64,
    #[serde(default = "default_memory_delete_archive_after_days")]
    pub delete_archive_after_days: i64,
    #[serde(default = "default_memory_compress_after_days")]
    pub compress_after_days: i64,
    #[serde(default = "default_memory_compression_min_cluster_size")]
    pub compression_min_cluster_size: usize,
    #[serde(default = "default_memory_max_rows_per_run")]
    pub max_rows_per_run: usize,
}

impl Default for MemoryRetentionConfig {
    fn default() -> Self {
        Self {
            enabled: default_memory_retention_enabled(),
            archive_after_days: default_memory_archive_after_days(),
            delete_archive_after_days: default_memory_delete_archive_after_days(),
            compress_after_days: default_memory_compress_after_days(),
            compression_min_cluster_size: default_memory_compression_min_cluster_size(),
            max_rows_per_run: default_memory_max_rows_per_run(),
        }
    }
}

impl MemoryRetentionConfig {
    pub(crate) fn to_maintenance_config(&self) -> MemoryMaintenanceConfig {
        MemoryMaintenanceConfig {
            archive_after_days: self.archive_after_days,
            delete_archive_after_days: self.delete_archive_after_days,
            compress_after_days: self.compress_after_days,
            compression_min_cluster_size: self.compression_min_cluster_size,
            max_rows_per_run: self.max_rows_per_run,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct MemoryEmbeddingConfig {
    pub provider: String,
    pub model: String,
    pub dimensions: usize,
    #[serde(default)]
    pub base_url: Option<String>,
    #[serde(default)]
    pub api_key_env: Option<String>,
}

impl MemoryEmbeddingConfig {
    pub(crate) fn normalized_provider(&self) -> String {
        self.provider.trim().to_ascii_lowercase()
    }

    pub(crate) fn is_supported_provider(&self) -> bool {
        matches!(
            self.normalized_provider().as_str(),
            "openai" | "voyage" | "cohere" | "openai-compatible"
        )
    }

    pub(crate) fn required_api_key_env(&self) -> Option<String> {
        match self.normalized_provider().as_str() {
            "openai" => Some("OPENAI_API_KEY".to_owned()),
            "voyage" => Some("VOYAGE_API_KEY".to_owned()),
            "cohere" => Some("COHERE_API_KEY".to_owned()),
            "openai-compatible" => self
                .api_key_env
                .as_ref()
                .map(|value| value.trim().to_owned())
                .filter(|value| !value.is_empty()),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct CronDelivery {
    /// Channel to deliver through (e.g. "signal").
    pub channel: String,
    /// Target on that channel (e.g. a UUID for DM, "group:<hex>" for group).
    pub target: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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
    "./db/memory.db".to_owned()
}

const fn default_prompt_index_enabled() -> bool {
    true
}

const fn default_prompt_index_limit() -> usize {
    12
}

const fn default_prompt_index_max_tokens() -> usize {
    1_200
}

const fn default_memory_retention_enabled() -> bool {
    true
}

const fn default_memory_archive_after_days() -> i64 {
    30
}

const fn default_memory_delete_archive_after_days() -> i64 {
    365
}

const fn default_memory_compress_after_days() -> i64 {
    14
}

const fn default_memory_compression_min_cluster_size() -> usize {
    3
}

const fn default_memory_max_rows_per_run() -> usize {
    200
}

impl Config {
    /// Load config from a TOML file.
    pub(crate) fn load(path: &Path) -> Result<Self> {
        let content = std::fs::read_to_string(path)
            .with_context(|| format!("failed to read config file: {}", path.display()))?;
        let config: Config = toml::from_str(&content)
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
        let local = PathBuf::from("coop.toml");
        if local.exists() {
            return local;
        }

        // Check XDG config
        if let Ok(config_dir) = std::env::var("XDG_CONFIG_HOME") {
            let xdg = PathBuf::from(config_dir).join("coop/coop.toml");
            if xdg.exists() {
                return xdg;
            }
        }

        // Check ~/.config/coop
        if let Ok(home) = std::env::var("HOME") {
            let home_config = PathBuf::from(home).join(".config/coop/coop.toml");
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
        let toml_str = r#"
[agent]
id = "test"
model = "anthropic/claude-sonnet-4-20250514"
"#;
        let config: Config = toml::from_str(toml_str).unwrap();
        assert_eq!(config.agent.id, "test");
        assert_eq!(config.agent.model, "anthropic/claude-sonnet-4-20250514");
        assert!(config.users.is_empty());
        assert!(config.channels.signal.is_none());
        assert_eq!(config.memory.db_path, "./db/memory.db");
        assert!(config.memory.prompt_index.enabled);
        assert_eq!(config.memory.prompt_index.limit, 12);
        assert_eq!(config.memory.prompt_index.max_tokens, 1_200);
        assert!(config.memory.retention.enabled);
        assert_eq!(config.memory.retention.archive_after_days, 30);
        assert_eq!(config.memory.retention.delete_archive_after_days, 365);
        assert_eq!(config.memory.retention.compress_after_days, 14);
        assert_eq!(config.memory.retention.compression_min_cluster_size, 3);
        assert_eq!(config.memory.retention.max_rows_per_run, 200);
        assert!(config.cron.is_empty());
    }

    #[test]
    fn parse_full_config() {
        let toml_str = r#"
[agent]
id = "reid"
model = "anthropic/claude-sonnet-4-20250514"
workspace = "./workspaces/default"

[[users]]
name = "alice"
trust = "full"
match = ["terminal:default"]

[[users]]
name = "bob"
trust = "inner"
match = ["signal:bob-uuid"]

[channels.signal]
db_path = "./db/signal.db"

[provider]
name = "anthropic"
"#;
        let config: Config = toml::from_str(toml_str).unwrap();
        assert_eq!(config.agent.id, "reid");
        assert_eq!(config.users.len(), 2);
        assert_eq!(config.users[0].trust, TrustLevel::Full);
        assert_eq!(config.users[1].trust, TrustLevel::Inner);
        assert_eq!(
            config.channels.signal.unwrap().db_path,
            "./db/signal.db".to_owned()
        );
        assert_eq!(config.provider.name, "anthropic");
        assert_eq!(config.memory.db_path, "./db/memory.db");
        assert!(config.memory.prompt_index.enabled);
        assert_eq!(config.memory.prompt_index.limit, 12);
        assert_eq!(config.memory.prompt_index.max_tokens, 1_200);
        assert!(config.memory.retention.enabled);
        assert_eq!(config.memory.retention.archive_after_days, 30);
    }

    #[test]
    fn parse_config_with_cron() {
        let toml_str = r#"
[agent]
id = "coop"
model = "test"

[[cron]]
name = "heartbeat"
cron = "*/30 * * * *"
user = "alice"
message = "check HEARTBEAT.md"

[[cron]]
name = "cleanup"
cron = "0 3 * * *"
message = "run cleanup"
"#;
        let config: Config = toml::from_str(toml_str).unwrap();
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
        let toml_str = r#"
[agent]
id = "coop"
model = "test"

[[cron]]
name = "morning-briefing"
cron = "0 8 * * *"
user = "alice"
message = "Morning briefing"

[cron.deliver]
channel = "signal"
target = "alice-uuid"
"#;
        let config: Config = toml::from_str(toml_str).unwrap();
        assert_eq!(config.cron.len(), 1);
        let delivery = config.cron[0].deliver.as_ref().unwrap();
        assert_eq!(delivery.channel, "signal");
        assert_eq!(delivery.target, "alice-uuid");
    }

    #[test]
    fn parse_config_with_cron_delivery_group_target() {
        let toml_str = r#"
[agent]
id = "coop"
model = "test"

[[cron]]
name = "weekly-review"
cron = "0 18 * * 5"
user = "alice"
message = "Weekly review"

[cron.deliver]
channel = "signal"
target = "group:deadbeef00112233445566778899aabbccddeeff00112233445566778899aabb"
"#;
        let config: Config = toml::from_str(toml_str).unwrap();
        let delivery = config.cron[0].deliver.as_ref().unwrap();
        assert_eq!(delivery.channel, "signal");
        assert!(delivery.target.starts_with("group:"));
    }

    #[test]
    fn parse_config_with_memory_settings() {
        let toml_str = r#"
[agent]
id = "coop"
model = "test"

[memory]
db_path = "./state/memory.db"

[memory.prompt_index]
enabled = false
limit = 5
max_tokens = 300

[memory.retention]
enabled = true
archive_after_days = 10
delete_archive_after_days = 20
compress_after_days = 4
compression_min_cluster_size = 2
max_rows_per_run = 50

[memory.embedding]
provider = "voyage"
model = "voyage-3-large"
dimensions = 1024
"#;
        let config: Config = toml::from_str(toml_str).unwrap();
        assert_eq!(config.memory.db_path, "./state/memory.db");
        assert!(!config.memory.prompt_index.enabled);
        assert_eq!(config.memory.prompt_index.limit, 5);
        assert_eq!(config.memory.prompt_index.max_tokens, 300);
        assert!(config.memory.retention.enabled);
        assert_eq!(config.memory.retention.archive_after_days, 10);
        assert_eq!(config.memory.retention.delete_archive_after_days, 20);
        assert_eq!(config.memory.retention.compress_after_days, 4);
        assert_eq!(config.memory.retention.compression_min_cluster_size, 2);
        assert_eq!(config.memory.retention.max_rows_per_run, 50);
        let embedding = config.memory.embedding.as_ref().unwrap();
        assert_eq!(embedding.provider, "voyage");
        assert_eq!(embedding.model, "voyage-3-large");
        assert_eq!(embedding.dimensions, 1024);
    }

    #[test]
    fn parse_config_with_openai_compatible_embedding() {
        let toml_str = r#"
[agent]
id = "coop"
model = "test"

[memory.embedding]
provider = "openai-compatible"
model = "text-embedding-3-small"
dimensions = 1536
base_url = "https://example.test/v1"
api_key_env = "OPENAI_COMPAT_API_KEY"
"#;
        let config: Config = toml::from_str(toml_str).unwrap();
        let embedding = config.memory.embedding.as_ref().unwrap();
        assert_eq!(embedding.provider, "openai-compatible");
        assert_eq!(
            embedding.base_url.as_deref(),
            Some("https://example.test/v1")
        );
        assert_eq!(
            embedding.api_key_env.as_deref(),
            Some("OPENAI_COMPAT_API_KEY")
        );
    }

    #[test]
    fn resolve_workspace_fails_for_missing_dir() {
        let toml_str = r#"
[agent]
id = "test"
model = "test"
workspace = "./does-not-exist"
"#;
        let config: Config = toml::from_str(toml_str).unwrap();

        let err = config.resolve_workspace(Path::new("/tmp")).unwrap_err();
        assert!(
            err.to_string().contains("workspace directory not found"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn resolve_workspace_succeeds_for_existing_dir() {
        let dir = tempfile::tempdir().unwrap();
        let toml_str = format!(
            "[agent]\nid = \"test\"\nmodel = \"test\"\nworkspace = \"{}\"",
            dir.path().display()
        );
        let config: Config = toml::from_str(&toml_str).unwrap();

        let resolved = config.resolve_workspace(Path::new("/unused")).unwrap();
        assert_eq!(resolved, dir.path());
    }

    #[test]
    fn parse_minimal_config_gets_default_prompt() {
        let toml_str = r#"
[agent]
id = "test"
model = "test-model"
"#;
        let config: Config = toml::from_str(toml_str).unwrap();
        assert_eq!(config.prompt.shared_files.len(), 3);
        assert_eq!(config.prompt.shared_files[0].path, "SOUL.md");
        assert_eq!(config.prompt.shared_files[1].path, "IDENTITY.md");
        assert_eq!(config.prompt.shared_files[2].path, "TOOLS.md");
        assert_eq!(config.prompt.user_files.len(), 3);
        assert_eq!(config.prompt.user_files[0].path, "AGENTS.md");
        assert_eq!(config.prompt.user_files[1].path, "USER.md");
        assert_eq!(config.prompt.user_files[2].path, "TOOLS.md");
    }

    #[test]
    fn parse_custom_prompt_shared_files() {
        let toml_str = r#"
[agent]
id = "test"
model = "test-model"

[[prompt.shared_files]]
path = "SOUL.md"
trust = "familiar"
cache = "stable"

[[prompt.shared_files]]
path = "CONTEXT.md"
description = "Project context"
"#;
        let config: Config = toml::from_str(toml_str).unwrap();
        assert_eq!(config.prompt.shared_files.len(), 2);
        assert_eq!(config.prompt.shared_files[0].path, "SOUL.md");
        assert_eq!(config.prompt.shared_files[0].trust, TrustLevel::Familiar);
        assert_eq!(config.prompt.shared_files[0].cache, CacheHintConfig::Stable);
        assert_eq!(config.prompt.shared_files[1].path, "CONTEXT.md");
        assert_eq!(
            config.prompt.shared_files[1].description.as_deref(),
            Some("Project context")
        );
        // user_files should get defaults since not specified
        assert_eq!(config.prompt.user_files.len(), 3);
    }

    #[test]
    fn parse_empty_user_files() {
        let toml_str = r#"
[agent]
id = "test"
model = "test-model"

[prompt]
user_files = []
"#;
        let config: Config = toml::from_str(toml_str).unwrap();
        assert!(config.prompt.user_files.is_empty());
        // shared_files should get defaults
        assert_eq!(config.prompt.shared_files.len(), 3);
    }

    #[test]
    fn prompt_file_entry_to_core_defaults_description() {
        let entry = PromptFileEntry {
            path: "SOUL.md".into(),
            trust: TrustLevel::Familiar,
            cache: CacheHintConfig::Stable,
            description: None,
        };
        let core = entry.to_core();
        assert_eq!(core.description, "SOUL");
        assert_eq!(core.path, "SOUL.md");
        assert_eq!(core.min_trust, TrustLevel::Familiar);
    }

    #[test]
    fn prompt_file_entry_to_core_uses_description() {
        let entry = PromptFileEntry {
            path: "SOUL.md".into(),
            trust: TrustLevel::Full,
            cache: CacheHintConfig::Session,
            description: Some("Agent personality".into()),
        };
        let core = entry.to_core();
        assert_eq!(core.description, "Agent personality");
    }

    #[test]
    fn prompt_config_roundtrip() {
        let toml_str = r#"
[agent]
id = "test"
model = "test-model"

[[prompt.shared_files]]
path = "SOUL.md"
trust = "familiar"
cache = "stable"
description = "Agent personality"

[[prompt.shared_files]]
path = "TOOLS.md"

[[prompt.user_files]]
path = "AGENTS.md"
cache = "stable"

[[prompt.user_files]]
path = "USER.md"
trust = "inner"
"#;
        let config: Config = toml::from_str(toml_str).unwrap();
        let serialized = toml::to_string(&config).unwrap();
        let config2: Config = toml::from_str(&serialized).unwrap();
        assert_eq!(config.prompt, config2.prompt);
    }

    #[test]
    fn prompt_file_entry_defaults() {
        let toml_str = r#"
[agent]
id = "test"
model = "test-model"

[[prompt.shared_files]]
path = "CUSTOM.md"
"#;
        let config: Config = toml::from_str(toml_str).unwrap();
        let entry = &config.prompt.shared_files[0];
        assert_eq!(entry.trust, TrustLevel::Full);
        assert_eq!(entry.cache, CacheHintConfig::Session);
        assert!(entry.description.is_none());
    }

    #[test]
    fn parse_config_with_api_keys() {
        let toml_str = r#"
[agent]
id = "test"
model = "test-model"

[provider]
name = "anthropic"
api_keys = ["env:ANTHROPIC_API_KEY", "env:ANTHROPIC_API_KEY_2"]
"#;
        let config: Config = toml::from_str(toml_str).unwrap();
        assert_eq!(config.provider.api_keys.len(), 2);
        assert_eq!(config.provider.api_keys[0], "env:ANTHROPIC_API_KEY");
        assert_eq!(config.provider.api_keys[1], "env:ANTHROPIC_API_KEY_2");
    }

    #[test]
    fn parse_config_without_api_keys() {
        let toml_str = r#"
[agent]
id = "test"
model = "test-model"

[provider]
name = "anthropic"
"#;
        let config: Config = toml::from_str(toml_str).unwrap();
        assert!(config.provider.api_keys.is_empty());
    }
}
