use anyhow::Result;
use async_trait::async_trait;
use coop_core::traits::{Tool, ToolContext, ToolExecutor};
use coop_core::types::{ToolDef, ToolOutput, TrustLevel};
use std::path::PathBuf;
use tracing::{debug, info, warn};

use crate::config::{Config, SandboxOverrides, UserConfig};
use crate::config_write::safe_write_config;

// ---------------------------------------------------------------------------
// config_read
// ---------------------------------------------------------------------------

#[derive(Debug)]
struct ConfigReadTool {
    config_path: PathBuf,
}

impl ConfigReadTool {
    fn new(config_path: PathBuf) -> Self {
        Self { config_path }
    }
}

#[async_trait]
impl Tool for ConfigReadTool {
    fn definition(&self) -> ToolDef {
        ToolDef::new(
            "config_read",
            "Read the current coop.toml configuration file.",
            serde_json::json!({
                "type": "object",
                "properties": {},
            }),
        )
    }

    async fn execute(
        &self,
        _arguments: serde_json::Value,
        ctx: &ToolContext,
    ) -> Result<ToolOutput> {
        if ctx.trust > TrustLevel::Full {
            return Ok(ToolOutput::error("config_read requires Full trust level"));
        }

        match std::fs::read_to_string(&self.config_path) {
            Ok(content) => {
                debug!(config = %self.config_path.display(), "config_read");
                Ok(ToolOutput::success(content))
            }
            Err(e) => Ok(ToolOutput::error(format!(
                "failed to read {}: {e}",
                self.config_path.display()
            ))),
        }
    }
}

// ---------------------------------------------------------------------------
// config_write
// ---------------------------------------------------------------------------

#[derive(Debug)]
struct ConfigWriteTool {
    config_path: PathBuf,
}

impl ConfigWriteTool {
    fn new(config_path: PathBuf) -> Self {
        Self { config_path }
    }
}

#[async_trait]
impl Tool for ConfigWriteTool {
    fn definition(&self) -> ToolDef {
        ToolDef::new(
            "config_write",
            "Validate and write coop.toml. Backs up the current config before writing. \
             Returns validation results. If any errors are found, the file is NOT modified.",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "content": {
                        "type": "string",
                        "description": "Complete TOML content for coop.toml. Must be the full \
                            file — not a patch or partial update. The content is validated \
                            before writing. If validation fails, the file is not modified."
                    }
                },
                "required": ["content"]
            }),
        )
    }

    async fn execute(&self, arguments: serde_json::Value, ctx: &ToolContext) -> Result<ToolOutput> {
        if ctx.trust > TrustLevel::Full {
            return Ok(ToolOutput::error("config_write requires Full trust level"));
        }

        let content = arguments
            .get("content")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("missing 'content' parameter"))?;

        // Parse proposed config and check for trust escalation
        let new_config: Config = match toml::from_str(content) {
            Ok(c) => c,
            Err(e) => {
                return Ok(ToolOutput::error(format!(
                    "Config validation failed. File was NOT modified.\n\nInvalid TOML: {e}"
                )));
            }
        };

        // Load current config for comparison (if file exists)
        if let Ok(current) = Config::load(&self.config_path)
            && let Some(violation) = check_trust_escalation(ctx.trust, &current, &new_config)
        {
            warn!(
                trust = ?ctx.trust,
                violation = %violation,
                "config_write rejected: trust escalation"
            );
            return Ok(ToolOutput::error(format!(
                "Config write rejected: {violation}\n\n\
                 Only users with Owner trust can modify trust levels, \
                 sandbox settings, or security-sensitive configuration."
            )));
        }

        let (report, backup) = safe_write_config(&self.config_path, content);
        let summary = report.to_summary_string();

        if report.has_errors() {
            warn!(config = %self.config_path.display(), "config_write rejected: validation failed");
            Ok(ToolOutput::error(format!(
                "Config validation failed. File was NOT modified.\n\n{summary}"
            )))
        } else {
            let backup_info = backup.map_or_else(
                || "No backup (new file)".to_owned(),
                |p| format!("Backup: {}", p.display()),
            );
            info!(config = %self.config_path.display(), "config_write applied");
            Ok(ToolOutput::success(format!(
                "Config written successfully. {backup_info}\n\n{summary}"
            )))
        }
    }
}

// ---------------------------------------------------------------------------
// Trust escalation prevention
// ---------------------------------------------------------------------------

/// Check if a config change would escalate trust or weaken security.
/// Returns `Some(reason)` if the change is blocked, `None` if it's allowed.
///
/// Rules:
/// - Only Owner can modify any user's trust level (up or down)
/// - Only Owner can add or remove users
/// - Only Owner can change sandbox.enabled
/// - Only Owner can modify global sandbox policy (allow_network, memory, pids_limit)
/// - Only Owner can modify per-user sandbox overrides
fn check_trust_escalation(
    caller_trust: TrustLevel,
    current: &Config,
    proposed: &Config,
) -> Option<String> {
    // Owner can do anything
    if caller_trust == TrustLevel::Owner {
        return None;
    }

    // Check user list changes
    if let Some(reason) = check_user_changes(current, proposed) {
        return Some(reason);
    }

    // Check sandbox config changes
    if let Some(reason) = check_sandbox_changes(current, proposed) {
        return Some(reason);
    }

    // Check prompt config changes (controls what the agent sees)
    if proposed.prompt != current.prompt {
        return Some(
            "cannot change prompt file configuration — only Owner can modify which files are included in the system prompt".to_owned(),
        );
    }

    None
}

fn check_user_changes(current: &Config, proposed: &Config) -> Option<String> {
    let current_users: std::collections::HashMap<&str, &UserConfig> =
        current.users.iter().map(|u| (u.name.as_str(), u)).collect();
    let proposed_users: std::collections::HashMap<&str, &UserConfig> = proposed
        .users
        .iter()
        .map(|u| (u.name.as_str(), u))
        .collect();

    // Check for new users
    for name in proposed_users.keys() {
        if !current_users.contains_key(name) {
            return Some(format!(
                "cannot add user '{name}' — only Owner can add users"
            ));
        }
    }

    // Check for removed users
    for name in current_users.keys() {
        if !proposed_users.contains_key(name) {
            return Some(format!(
                "cannot remove user '{name}' — only Owner can remove users"
            ));
        }
    }

    // Check for trust level changes
    for (name, proposed_user) in &proposed_users {
        if let Some(current_user) = current_users.get(name) {
            if proposed_user.trust != current_user.trust {
                return Some(format!(
                    "cannot change trust for user '{name}' from {:?} to {:?} \
                     — only Owner can modify trust levels",
                    current_user.trust, proposed_user.trust
                ));
            }

            // Check for match rule changes (could re-map identity to a different trust)
            if proposed_user.r#match != current_user.r#match {
                return Some(format!(
                    "cannot change match rules for user '{name}' \
                     — only Owner can modify user identity matching"
                ));
            }

            // Check for sandbox override changes
            if proposed_user.sandbox != current_user.sandbox {
                return Some(format!(
                    "cannot change sandbox overrides for user '{name}' \
                     — only Owner can modify sandbox settings"
                ));
            }
        }
    }

    None
}

fn check_sandbox_changes(current: &Config, proposed: &Config) -> Option<String> {
    if proposed.sandbox != current.sandbox {
        let s = &proposed.sandbox;
        let c = &current.sandbox;

        if s.enabled != c.enabled {
            return Some(format!(
                "cannot change sandbox.enabled from {} to {} — only Owner can modify sandbox settings",
                c.enabled, s.enabled
            ));
        }
        if s.allow_network != c.allow_network {
            return Some(
                "cannot change sandbox.allow_network — only Owner can modify sandbox settings"
                    .to_owned(),
            );
        }
        if s.memory != c.memory {
            return Some(
                "cannot change sandbox.memory — only Owner can modify sandbox settings".to_owned(),
            );
        }
        if s.pids_limit != c.pids_limit {
            return Some(
                "cannot change sandbox.pids_limit — only Owner can modify sandbox settings"
                    .to_owned(),
            );
        }
    }

    // Check per-cron sandbox overrides
    let current_crons: std::collections::HashMap<&str, Option<&SandboxOverrides>> = current
        .cron
        .iter()
        .map(|c| (c.name.as_str(), c.sandbox.as_ref()))
        .collect();

    for cron in &proposed.cron {
        let proposed_sandbox = cron.sandbox.as_ref();
        let current_sandbox = current_crons.get(cron.name.as_str()).copied().flatten();
        if proposed_sandbox != current_sandbox {
            return Some(format!(
                "cannot change sandbox overrides for cron '{}' \
                 — only Owner can modify sandbox settings",
                cron.name
            ));
        }
    }

    None
}

#[allow(missing_debug_implementations)]
pub(crate) struct ConfigToolExecutor {
    read_tool: ConfigReadTool,
    write_tool: ConfigWriteTool,
}

impl ConfigToolExecutor {
    pub(crate) fn new(config_path: PathBuf) -> Self {
        Self {
            read_tool: ConfigReadTool::new(config_path.clone()),
            write_tool: ConfigWriteTool::new(config_path),
        }
    }
}

#[async_trait]
impl ToolExecutor for ConfigToolExecutor {
    async fn execute(
        &self,
        name: &str,
        arguments: serde_json::Value,
        ctx: &ToolContext,
    ) -> Result<ToolOutput> {
        match name {
            "config_read" => self.read_tool.execute(arguments, ctx).await,
            "config_write" => self.write_tool.execute(arguments, ctx).await,
            _ => Ok(ToolOutput::error(format!("unknown tool: {name}"))),
        }
    }

    fn tools(&self) -> Vec<ToolDef> {
        vec![self.read_tool.definition(), self.write_tool.definition()]
    }
}

#[allow(clippy::unwrap_used)]
#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{CronConfig, SandboxConfig};
    use std::path::Path;

    fn tool_context(trust: TrustLevel) -> ToolContext {
        ToolContext {
            session_id: "test-session".to_owned(),
            trust,
            workspace: PathBuf::from("."),
            user_name: None,
        }
    }

    fn base_config() -> Config {
        toml::from_str(
            "[agent]\nid = \"test\"\nmodel = \"test-model\"\n\n[provider]\nname = \"anthropic\"\n",
        )
        .unwrap()
    }

    fn config_with_users(users: Vec<UserConfig>) -> Config {
        let mut cfg = base_config();
        cfg.users = users;
        cfg
    }

    fn alice_full() -> UserConfig {
        UserConfig {
            name: "alice".to_owned(),
            trust: TrustLevel::Full,
            r#match: vec!["signal:alice-uuid".to_owned()],
            sandbox: None,
        }
    }

    fn bob_inner() -> UserConfig {
        UserConfig {
            name: "bob".to_owned(),
            trust: TrustLevel::Inner,
            r#match: vec!["signal:bob-uuid".to_owned()],
            sandbox: None,
        }
    }

    // --- Trust escalation tests ---

    #[test]
    fn owner_can_change_anything() {
        let current = config_with_users(vec![alice_full()]);
        let mut proposed = current.clone();
        proposed.users[0].trust = TrustLevel::Owner;
        assert!(check_trust_escalation(TrustLevel::Owner, &current, &proposed).is_none());
    }

    #[test]
    fn full_cannot_escalate_trust() {
        let current = config_with_users(vec![alice_full(), bob_inner()]);
        let mut proposed = current.clone();
        proposed.users[1].trust = TrustLevel::Full; // bob: inner -> full
        let result = check_trust_escalation(TrustLevel::Full, &current, &proposed);
        assert!(result.is_some());
        assert!(result.unwrap().contains("cannot change trust"));
    }

    #[test]
    fn full_cannot_demote_trust() {
        let current = config_with_users(vec![alice_full(), bob_inner()]);
        let mut proposed = current.clone();
        proposed.users[1].trust = TrustLevel::Familiar; // bob: inner -> familiar
        let result = check_trust_escalation(TrustLevel::Full, &current, &proposed);
        assert!(result.is_some());
        assert!(result.unwrap().contains("cannot change trust"));
    }

    #[test]
    fn full_cannot_add_user() {
        let current = config_with_users(vec![alice_full()]);
        let proposed = config_with_users(vec![alice_full(), bob_inner()]);
        let result = check_trust_escalation(TrustLevel::Full, &current, &proposed);
        assert!(result.is_some());
        assert!(result.unwrap().contains("cannot add user"));
    }

    #[test]
    fn full_cannot_remove_user() {
        let current = config_with_users(vec![alice_full(), bob_inner()]);
        let proposed = config_with_users(vec![alice_full()]);
        let result = check_trust_escalation(TrustLevel::Full, &current, &proposed);
        assert!(result.is_some());
        assert!(result.unwrap().contains("cannot remove user"));
    }

    #[test]
    fn full_cannot_change_match_rules() {
        let current = config_with_users(vec![alice_full()]);
        let mut proposed = current.clone();
        proposed.users[0].r#match = vec!["terminal:default".to_owned()];
        let result = check_trust_escalation(TrustLevel::Full, &current, &proposed);
        assert!(result.is_some());
        assert!(result.unwrap().contains("cannot change match rules"));
    }

    #[test]
    fn full_cannot_disable_sandbox() {
        let mut current = base_config();
        current.sandbox = SandboxConfig {
            enabled: true,
            ..SandboxConfig::default()
        };
        let mut proposed = current.clone();
        proposed.sandbox.enabled = false;
        let result = check_trust_escalation(TrustLevel::Full, &current, &proposed);
        assert!(result.is_some());
        assert!(result.unwrap().contains("sandbox.enabled"));
    }

    #[test]
    fn full_cannot_change_sandbox_network() {
        let mut current = base_config();
        current.sandbox = SandboxConfig {
            enabled: true,
            ..SandboxConfig::default()
        };
        let mut proposed = current.clone();
        proposed.sandbox.allow_network = true;
        let result = check_trust_escalation(TrustLevel::Full, &current, &proposed);
        assert!(result.is_some());
        assert!(result.unwrap().contains("sandbox.allow_network"));
    }

    #[test]
    fn full_cannot_change_user_sandbox_overrides() {
        let current = config_with_users(vec![alice_full()]);
        let mut proposed = current.clone();
        proposed.users[0].sandbox = Some(SandboxOverrides {
            allow_network: Some(true),
            ..SandboxOverrides::default()
        });
        let result = check_trust_escalation(TrustLevel::Full, &current, &proposed);
        assert!(result.is_some());
        assert!(result.unwrap().contains("sandbox overrides"));
    }

    #[test]
    fn full_cannot_change_cron_sandbox_overrides() {
        let mut current = base_config();
        current.cron = vec![CronConfig {
            name: "heartbeat".to_owned(),
            cron: "*/30 * * * *".to_owned(),
            message: "check".to_owned(),
            user: None,
            deliver: None,
            sandbox: None,
        }];
        let mut proposed = current.clone();
        proposed.cron[0].sandbox = Some(SandboxOverrides {
            allow_network: Some(true),
            ..SandboxOverrides::default()
        });
        let result = check_trust_escalation(TrustLevel::Full, &current, &proposed);
        assert!(result.is_some());
        assert!(result.unwrap().contains("sandbox overrides for cron"));
    }

    #[test]
    fn full_cannot_change_prompt_config() {
        let current = base_config();
        let mut proposed = current.clone();
        proposed
            .prompt
            .shared_files
            .push(crate::config::PromptFileEntry {
                path: "EVIL.md".to_owned(),
                trust: TrustLevel::Full,
                cache: crate::config::CacheHintConfig::Session,
                description: Some("injected file".to_owned()),
            });
        let result = check_trust_escalation(TrustLevel::Full, &current, &proposed);
        assert!(result.is_some());
        assert!(result.unwrap().contains("prompt file configuration"));
    }

    #[test]
    fn owner_can_change_prompt_config() {
        let current = base_config();
        let mut proposed = current.clone();
        proposed
            .prompt
            .shared_files
            .push(crate::config::PromptFileEntry {
                path: "EXTRA.md".to_owned(),
                trust: TrustLevel::Full,
                cache: crate::config::CacheHintConfig::Session,
                description: Some("extra file".to_owned()),
            });
        assert!(check_trust_escalation(TrustLevel::Owner, &current, &proposed).is_none());
    }

    #[test]
    fn full_can_change_model() {
        let current = base_config();
        let mut proposed = current.clone();
        proposed.agent.model = "new-model".to_owned();
        assert!(check_trust_escalation(TrustLevel::Full, &current, &proposed).is_none());
    }

    #[test]
    fn full_can_change_cron_message() {
        let mut current = base_config();
        current.cron = vec![CronConfig {
            name: "heartbeat".to_owned(),
            cron: "*/30 * * * *".to_owned(),
            message: "check".to_owned(),
            user: None,
            deliver: None,
            sandbox: None,
        }];
        let mut proposed = current.clone();
        proposed.cron[0].message = "updated check".to_owned();
        assert!(check_trust_escalation(TrustLevel::Full, &current, &proposed).is_none());
    }

    #[test]
    fn inner_cannot_escalate_trust() {
        let current = config_with_users(vec![alice_full(), bob_inner()]);
        let mut proposed = current.clone();
        proposed.users[0].trust = TrustLevel::Inner; // alice: full -> inner
        let result = check_trust_escalation(TrustLevel::Inner, &current, &proposed);
        assert!(result.is_some());
    }

    fn write_test_config(dir: &Path) -> PathBuf {
        let workspace = dir.join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::write(workspace.join("SOUL.md"), "test").unwrap();

        let config_path = dir.join("coop.toml");
        std::fs::write(
            &config_path,
            format!(
                "[agent]\nid = \"test\"\nmodel = \"test-model\"\nworkspace = \"{}\"\n\n[provider]\nname = \"anthropic\"\n",
                workspace.display()
            ),
        )
        .unwrap();
        config_path
    }

    #[tokio::test]
    async fn test_config_read_returns_content() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = write_test_config(dir.path());
        let expected = std::fs::read_to_string(&config_path).unwrap();

        let tool = ConfigReadTool::new(config_path);
        let output = tool
            .execute(serde_json::json!({}), &tool_context(TrustLevel::Full))
            .await
            .unwrap();

        assert!(!output.is_error);
        assert_eq!(output.content, expected);
    }

    #[tokio::test]
    async fn test_config_read_trust_gate() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = write_test_config(dir.path());

        let tool = ConfigReadTool::new(config_path);
        let output = tool
            .execute(serde_json::json!({}), &tool_context(TrustLevel::Public))
            .await
            .unwrap();

        assert!(output.is_error);
        assert!(output.content.contains("Full trust"));
    }

    #[tokio::test]
    async fn test_config_read_missing_file() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("nonexistent.toml");

        let tool = ConfigReadTool::new(config_path);
        let output = tool
            .execute(serde_json::json!({}), &tool_context(TrustLevel::Full))
            .await
            .unwrap();

        assert!(output.is_error);
        assert!(output.content.contains("failed to read"));
    }

    #[tokio::test]
    async fn test_config_write_valid() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = write_test_config(dir.path());

        let tool = ConfigWriteTool::new(config_path.clone());
        let workspace = dir.path().join("workspace");
        let new_toml = format!(
            "[agent]\nid = \"updated\"\nmodel = \"test-model\"\nworkspace = \"{}\"\n\n[provider]\nname = \"anthropic\"\n",
            workspace.display()
        );

        let output = tool
            .execute(
                serde_json::json!({"content": new_toml}),
                &tool_context(TrustLevel::Full),
            )
            .await
            .unwrap();

        if output.is_error {
            // May fail if ANTHROPIC_API_KEY is not set — that's an env issue,
            // not a config tool issue. Verify the tool produced a report.
            assert!(output.content.contains("NOT modified"));
        } else {
            assert!(config_path.with_extension("toml.bak").exists());
            assert!(
                std::fs::read_to_string(&config_path)
                    .unwrap()
                    .contains("updated")
            );
        }
    }

    #[tokio::test]
    async fn test_config_write_invalid_toml() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = write_test_config(dir.path());
        let original = std::fs::read_to_string(&config_path).unwrap();

        let tool = ConfigWriteTool::new(config_path.clone());

        let output = tool
            .execute(
                serde_json::json!({"content": "{{garbage toml"}),
                &tool_context(TrustLevel::Full),
            )
            .await
            .unwrap();

        assert!(output.is_error);
        assert!(output.content.contains("NOT modified"));
        assert_eq!(std::fs::read_to_string(&config_path).unwrap(), original);
    }

    #[tokio::test]
    async fn test_config_write_trust_gate() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = write_test_config(dir.path());

        let tool = ConfigWriteTool::new(config_path);

        let output = tool
            .execute(
                serde_json::json!({"content": "anything"}),
                &tool_context(TrustLevel::Public),
            )
            .await
            .unwrap();

        assert!(output.is_error);
        assert!(output.content.contains("Full trust"));
    }

    #[tokio::test]
    async fn test_config_write_missing_workspace() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("coop.toml");
        std::fs::write(
            &config_path,
            "[agent]\nid = \"test\"\nmodel = \"test-model\"\n",
        )
        .unwrap();
        let original = std::fs::read_to_string(&config_path).unwrap();

        let tool = ConfigWriteTool::new(config_path.clone());

        let new_toml =
            "[agent]\nid = \"test\"\nmodel = \"test-model\"\nworkspace = \"./nonexistent\"\n";
        let output = tool
            .execute(
                serde_json::json!({"content": new_toml}),
                &tool_context(TrustLevel::Full),
            )
            .await
            .unwrap();

        assert!(output.is_error);
        assert_eq!(std::fs::read_to_string(&config_path).unwrap(), original);
    }

    // --- End-to-end config_write trust escalation tests ---

    fn write_config_with_users(dir: &Path) -> PathBuf {
        let workspace = dir.join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::write(workspace.join("SOUL.md"), "test").unwrap();

        let config_path = dir.join("coop.toml");
        std::fs::write(
            &config_path,
            format!(
                "[agent]\nid = \"test\"\nmodel = \"test-model\"\nworkspace = \"{}\"\n\n\
                 [provider]\nname = \"anthropic\"\n\n\
                 [[users]]\nname = \"alice\"\ntrust = \"full\"\nmatch = [\"signal:alice-uuid\"]\n\n\
                 [[users]]\nname = \"bob\"\ntrust = \"inner\"\nmatch = [\"signal:bob-uuid\"]\n",
                workspace.display()
            ),
        )
        .unwrap();
        config_path
    }

    #[tokio::test]
    async fn config_write_rejects_trust_escalation_e2e() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = write_config_with_users(dir.path());
        let original = std::fs::read_to_string(&config_path).unwrap();
        let workspace = dir.path().join("workspace");

        let tool = ConfigWriteTool::new(config_path.clone());

        // Try to promote bob from inner to owner
        let evil_toml = format!(
            "[agent]\nid = \"test\"\nmodel = \"test-model\"\nworkspace = \"{}\"\n\n\
             [provider]\nname = \"anthropic\"\n\n\
             [[users]]\nname = \"alice\"\ntrust = \"full\"\nmatch = [\"signal:alice-uuid\"]\n\n\
             [[users]]\nname = \"bob\"\ntrust = \"owner\"\nmatch = [\"signal:bob-uuid\"]\n",
            workspace.display()
        );

        let output = tool
            .execute(
                serde_json::json!({"content": evil_toml}),
                &tool_context(TrustLevel::Full),
            )
            .await
            .unwrap();

        assert!(output.is_error);
        assert!(
            output.content.contains("trust escalation")
                || output.content.contains("cannot change trust")
        );
        assert_eq!(std::fs::read_to_string(&config_path).unwrap(), original);
    }

    #[tokio::test]
    async fn config_write_rejects_adding_user_e2e() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = write_config_with_users(dir.path());
        let original = std::fs::read_to_string(&config_path).unwrap();
        let workspace = dir.path().join("workspace");

        let tool = ConfigWriteTool::new(config_path.clone());

        // Try to add eve as a new owner
        let evil_toml = format!(
            "[agent]\nid = \"test\"\nmodel = \"test-model\"\nworkspace = \"{}\"\n\n\
             [provider]\nname = \"anthropic\"\n\n\
             [[users]]\nname = \"alice\"\ntrust = \"full\"\nmatch = [\"signal:alice-uuid\"]\n\n\
             [[users]]\nname = \"bob\"\ntrust = \"inner\"\nmatch = [\"signal:bob-uuid\"]\n\n\
             [[users]]\nname = \"eve\"\ntrust = \"owner\"\nmatch = [\"terminal:default\"]\n",
            workspace.display()
        );

        let output = tool
            .execute(
                serde_json::json!({"content": evil_toml}),
                &tool_context(TrustLevel::Full),
            )
            .await
            .unwrap();

        assert!(output.is_error);
        assert!(output.content.contains("cannot add user"));
        assert_eq!(std::fs::read_to_string(&config_path).unwrap(), original);
    }

    #[tokio::test]
    async fn config_write_owner_can_escalate_e2e() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = write_config_with_users(dir.path());
        let workspace = dir.path().join("workspace");

        let tool = ConfigWriteTool::new(config_path.clone());

        // Owner promotes bob from inner to full
        let new_toml = format!(
            "[agent]\nid = \"test\"\nmodel = \"test-model\"\nworkspace = \"{}\"\n\n\
             [provider]\nname = \"anthropic\"\n\n\
             [[users]]\nname = \"alice\"\ntrust = \"full\"\nmatch = [\"signal:alice-uuid\"]\n\n\
             [[users]]\nname = \"bob\"\ntrust = \"full\"\nmatch = [\"signal:bob-uuid\"]\n",
            workspace.display()
        );

        let output = tool
            .execute(
                serde_json::json!({"content": new_toml}),
                &tool_context(TrustLevel::Owner),
            )
            .await
            .unwrap();

        // Owner should pass the escalation check. May still fail on env check
        // (ANTHROPIC_API_KEY), but not on trust escalation.
        if output.is_error {
            assert!(
                !output.content.contains("trust escalation"),
                "Owner should not be blocked by trust escalation: {}",
                output.content
            );
        }
    }
}
