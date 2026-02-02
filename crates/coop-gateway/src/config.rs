use anyhow::{Context, Result};
use coop_core::TrustLevel;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct Config {
    pub agent: AgentConfig,
    #[serde(default)]
    pub users: Vec<UserConfig>,
    #[serde(default)]
    pub provider: ProviderConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct AgentConfig {
    pub id: String,
    pub model: String,
    #[serde(default)]
    pub personality: Option<String>,
    #[serde(default)]
    pub instructions: Option<String>,
    #[serde(default = "default_workspace")]
    pub workspace: String,
}

fn default_workspace() -> String {
    "./workspaces/default".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct UserConfig {
    pub name: String,
    pub trust: TrustLevel,
    #[serde(default)]
    pub r#match: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub(crate) struct ProviderConfig {
    #[serde(default = "default_provider")]
    pub name: String,
}

fn default_provider() -> String {
    "anthropic".to_string()
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

    /// Build the system prompt from personality + instructions files.
    pub(crate) fn build_system_prompt(&self, base_dir: &Path) -> Result<String> {
        let mut parts = Vec::new();

        if let Some(personality_path) = &self.agent.personality {
            let path = base_dir.join(personality_path);
            if path.exists() {
                let content = std::fs::read_to_string(&path).with_context(|| {
                    format!("failed to read personality file: {}", path.display())
                })?;
                parts.push(content);
            }
        }

        if let Some(instructions_path) = &self.agent.instructions {
            let path = base_dir.join(instructions_path);
            if path.exists() {
                let content = std::fs::read_to_string(&path).with_context(|| {
                    format!("failed to read instructions file: {}", path.display())
                })?;
                parts.push(content);
            }
        }

        if parts.is_empty() {
            parts.push("You are a helpful AI assistant.".to_string());
        }

        Ok(parts.join("\n\n"))
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_minimal_config() {
        let yaml = r"
agent:
  id: test
  model: anthropic/claude-sonnet-4-20250514
";
        let config: Config = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(config.agent.id, "test");
        assert_eq!(config.agent.model, "anthropic/claude-sonnet-4-20250514");
        assert!(config.users.is_empty());
    }

    #[test]
    fn parse_full_config() {
        let yaml = r"
agent:
  id: reid
  model: anthropic/claude-sonnet-4-20250514
  personality: ./soul.md
  instructions: ./agents.md
  workspace: ./workspaces/default

users:
  - name: alice
    trust: full
    match: ['terminal:default']
  - name: bob
    trust: inner
    match: ['signal:+15555550101']

provider:
  name: anthropic
";
        let config: Config = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(config.agent.id, "reid");
        assert_eq!(config.users.len(), 2);
        assert_eq!(config.users[0].trust, TrustLevel::Full);
        assert_eq!(config.users[1].trust, TrustLevel::Inner);
        assert_eq!(config.provider.name, "anthropic");
    }
}
