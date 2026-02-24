use anyhow::Result;
use async_trait::async_trait;
use coop_core::tools::truncate;
use coop_core::traits::{ToolContext, ToolExecutor};
use coop_core::types::{ToolDef, ToolOutput, TrustLevel};
use coop_sandbox::{NetworkMode, SandboxPolicy};
use std::sync::Arc;
use std::time::Duration;
use tracing::{debug, info};

use crate::config::SharedConfig;

const SANDBOX_TIMEOUT: Duration = Duration::from_secs(120);

pub(crate) struct SandboxExecutor {
    inner: Arc<dyn ToolExecutor>,
    base_policy: SandboxPolicy,
    config: SharedConfig,
}

impl SandboxExecutor {
    pub(crate) fn new(
        inner: Arc<dyn ToolExecutor>,
        base_policy: SandboxPolicy,
        config: SharedConfig,
    ) -> Self {
        Self {
            inner,
            base_policy,
            config,
        }
    }
}

impl std::fmt::Debug for SandboxExecutor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SandboxExecutor")
            .field("base_policy", &self.base_policy)
            .field("sandbox_enabled", &self.config.load().sandbox.enabled)
            .finish_non_exhaustive()
    }
}

#[async_trait]
impl ToolExecutor for SandboxExecutor {
    async fn execute(
        &self,
        name: &str,
        arguments: serde_json::Value,
        ctx: &ToolContext,
    ) -> Result<ToolOutput> {
        // Owner trust bypasses sandbox entirely
        if ctx.trust == TrustLevel::Owner {
            info!(tool = %name, "sandbox bypass: owner trust");
            return self.inner.execute(name, arguments, ctx).await;
        }

        // Only bash goes through the sandbox — other tools pass through
        if name != "bash" {
            return self.inner.execute(name, arguments, ctx).await;
        }

        self.exec_bash_sandboxed(arguments, ctx).await
    }

    fn tools(&self) -> Vec<ToolDef> {
        self.inner.tools()
    }
}

impl SandboxExecutor {
    /// Resolve sandbox policy from live config with per-user overrides.
    ///
    /// Network mode is derived from the config `allow_network` flag and the
    /// caller's trust level:
    ///
    /// | `allow_network` | Trust ≥ Full | Trust < Full |
    /// |-----------------|-------------|--------------|
    /// | `false`         | None        | None         |
    /// | `true`          | Host        | InternetOnly |
    fn resolve_policy(&self, ctx: &ToolContext) -> SandboxPolicy {
        let cfg = self.config.load();

        let mut allow_network = cfg.sandbox.allow_network;
        let mut memory_limit = coop_sandbox::parse_memory_size(&cfg.sandbox.memory)
            .unwrap_or(self.base_policy.memory_limit);
        let mut pids_limit = cfg.sandbox.pids_limit;
        let mut long_lived = cfg.sandbox.long_lived;

        if let Some(user_name) = &ctx.user_name
            && let Some(user) = cfg.users.iter().find(|u| &u.name == user_name)
            && let Some(overrides) = &user.sandbox
        {
            if let Some(user_allow_network) = overrides.allow_network {
                allow_network = user_allow_network;
            }
            if let Some(ref memory) = overrides.memory
                && let Ok(bytes) = coop_sandbox::parse_memory_size(memory)
            {
                memory_limit = bytes;
            }
            if let Some(user_pids) = overrides.pids_limit {
                pids_limit = user_pids;
            }
            if let Some(user_long_lived) = overrides.long_lived {
                long_lived = user_long_lived;
            }
        }

        let network = if !allow_network {
            NetworkMode::None
        } else if ctx.trust <= TrustLevel::Full {
            NetworkMode::Host
        } else {
            NetworkMode::InternetOnly
        };

        SandboxPolicy {
            workspace: ctx.workspace.clone(),
            network,
            memory_limit,
            pids_limit,
            long_lived,
        }
    }

    async fn exec_bash_sandboxed(
        &self,
        arguments: serde_json::Value,
        ctx: &ToolContext,
    ) -> Result<ToolOutput> {
        if ctx.trust > TrustLevel::Inner {
            return Ok(ToolOutput::error(
                "bash tool requires Full or Inner trust level",
            ));
        }

        let command = arguments
            .get("command")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| anyhow::anyhow!("missing required parameter: command"))?;

        let policy = self.resolve_policy(ctx);

        debug!(
            command_len = command.len(),
            trust = ?ctx.trust,
            workspace = %ctx.workspace.display(),
            "sandbox: routing bash through sandboxed exec"
        );

        let result = coop_sandbox::exec_with_user_context(
            &policy,
            command,
            SANDBOX_TIMEOUT,
            ctx.user_name.as_deref(),
            Some(ctx.trust),
        )
        .await;

        match result {
            Err(e) => Ok(ToolOutput::error(format!("sandbox exec failed: {e}"))),
            Ok(output) => {
                let mut combined = output.stdout;
                if !output.stderr.is_empty() {
                    if !combined.is_empty() {
                        combined.push('\n');
                    }
                    combined.push_str(&output.stderr);
                }

                let truncated = truncate::truncate_tail(&combined);
                let final_output = if truncated.was_truncated {
                    format!(
                        "[output truncated: showing last {} of {} bytes]\n{}",
                        truncated.output.len(),
                        combined.len(),
                        truncated.output
                    )
                } else {
                    truncated.output
                };

                if output.exit_code == 0 {
                    Ok(ToolOutput::success(final_output))
                } else {
                    Ok(ToolOutput::error(format!(
                        "exit code {}\n{final_output}",
                        output.exit_code
                    )))
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{self, shared_config};
    use coop_core::fakes::SimpleExecutor;
    use coop_core::types::TrustLevel;
    use std::path::PathBuf;

    fn test_config() -> config::Config {
        toml::from_str(
            r#"
[agent]
id = "test"
model = "test"

[sandbox]
enabled = true
allow_network = false
memory = "1g"
pids_limit = 256
long_lived = true

[[users]]
name = "alice"
trust = "owner"
match = ["terminal:default"]

[[users]]
name = "bob"
trust = "full"
match = ["signal:bob-uuid"]

[bob_sandbox]

[[users]]
name = "carol"
trust = "full"
match = ["signal:carol-uuid"]
"#,
        )
        .expect("test config should parse")
    }

    fn test_config_with_overrides() -> config::Config {
        toml::from_str(
            r#"
[agent]
id = "test"
model = "test"

[sandbox]
enabled = true
allow_network = false
memory = "1g"
pids_limit = 256
long_lived = true

[[users]]
name = "alice"
trust = "owner"
match = ["terminal:default"]

[[users]]
name = "bob"
trust = "full"
match = ["signal:bob-uuid"]
sandbox = { allow_network = true, memory = "4g", pids_limit = 1024, long_lived = false }
"#,
        )
        .expect("test config should parse")
    }

    fn tool_context(trust: TrustLevel) -> ToolContext {
        ToolContext {
            session_id: "test-session".to_owned(),
            trust,
            workspace: PathBuf::from("/tmp"),
            user_name: None,
        }
    }

    fn tool_context_with_user(trust: TrustLevel, user: &str) -> ToolContext {
        ToolContext {
            session_id: "test-session".to_owned(),
            trust,
            workspace: PathBuf::from("/tmp"),
            user_name: Some(user.to_owned()),
        }
    }

    #[tokio::test]
    async fn owner_bypasses_sandbox() {
        let inner = Arc::new(SimpleExecutor::new());
        let shared = shared_config(test_config());
        let executor = SandboxExecutor::new(inner, SandboxPolicy::default(), shared);

        // Owner trust passes through to inner executor (SimpleExecutor bails on unknown tools)
        let result = executor
            .execute(
                "bash",
                serde_json::json!({"command": "echo hi"}),
                &tool_context(TrustLevel::Owner),
            )
            .await;

        assert!(result.is_err());
        assert!(
            result
                .expect_err("should error")
                .to_string()
                .contains("unknown tool")
        );
    }

    #[tokio::test]
    async fn non_bash_tools_pass_through() {
        let inner = Arc::new(SimpleExecutor::new());
        let shared = shared_config(test_config());
        let executor = SandboxExecutor::new(inner, SandboxPolicy::default(), shared);

        // Non-bash tools pass through to inner executor regardless of trust
        let result = executor
            .execute(
                "read_file",
                serde_json::json!({"path": "test.txt"}),
                &tool_context(TrustLevel::Full),
            )
            .await;

        assert!(result.is_err());
        assert!(
            result
                .expect_err("should error")
                .to_string()
                .contains("unknown tool")
        );
    }

    #[tokio::test]
    async fn familiar_trust_rejected() {
        let inner = Arc::new(SimpleExecutor::new());
        let shared = shared_config(test_config());
        let executor = SandboxExecutor::new(inner, SandboxPolicy::default(), shared);

        let result = executor
            .execute(
                "bash",
                serde_json::json!({"command": "echo hi"}),
                &tool_context(TrustLevel::Familiar),
            )
            .await
            .expect("should succeed with ToolOutput");

        assert!(result.is_error);
        assert!(
            result
                .content
                .contains("requires Full or Inner trust level")
        );
    }

    #[test]
    fn resolve_policy_uses_live_config_globals() {
        let config = test_config();
        let shared = shared_config(config);
        // base_policy differs from config — config values should win
        let base_policy = SandboxPolicy {
            workspace: PathBuf::from("/tmp"),
            network: NetworkMode::Host,
            memory_limit: 999,
            pids_limit: 999,
            long_lived: false,
        };
        let executor = SandboxExecutor::new(Arc::new(SimpleExecutor::new()), base_policy, shared);

        let ctx = tool_context_with_user(TrustLevel::Full, "carol");
        let policy = executor.resolve_policy(&ctx);

        // Config has allow_network=false → None regardless of trust
        assert_eq!(policy.network, NetworkMode::None);
        assert_eq!(policy.memory_limit, 1024 * 1024 * 1024);
        assert_eq!(policy.pids_limit, 256);
        assert!(policy.long_lived);
    }

    #[test]
    fn resolve_policy_applies_user_overrides() {
        let config = test_config_with_overrides();
        let shared = shared_config(config);
        let base_policy = SandboxPolicy::default();
        let executor = SandboxExecutor::new(Arc::new(SimpleExecutor::new()), base_policy, shared);

        // Bob has Full trust + allow_network override → Host
        let ctx = tool_context_with_user(TrustLevel::Full, "bob");
        let policy = executor.resolve_policy(&ctx);

        assert_eq!(policy.network, NetworkMode::Host);
        assert_eq!(policy.memory_limit, 4 * 1024 * 1024 * 1024);
        assert_eq!(policy.pids_limit, 1024);
        assert!(!policy.long_lived); // Bob overrides to false
    }

    #[test]
    fn resolve_policy_unknown_user_uses_config_globals() {
        let config = test_config();
        let shared = shared_config(config);
        let base_policy = SandboxPolicy::default();
        let executor = SandboxExecutor::new(Arc::new(SimpleExecutor::new()), base_policy, shared);

        let ctx = tool_context_with_user(TrustLevel::Full, "mallory");
        let policy = executor.resolve_policy(&ctx);

        // Config has allow_network=false → None
        assert_eq!(policy.network, NetworkMode::None);
        assert_eq!(policy.memory_limit, 1024 * 1024 * 1024);
        assert_eq!(policy.pids_limit, 256);
        assert!(policy.long_lived);
    }

    #[test]
    fn resolve_policy_picks_up_hot_reloaded_globals() {
        let config = test_config();
        let shared = shared_config(config);
        let base_policy = SandboxPolicy::default();
        let executor = SandboxExecutor::new(
            Arc::new(SimpleExecutor::new()),
            base_policy,
            Arc::clone(&shared),
        );

        // Initial: allow_network=false → None
        let ctx = tool_context_with_user(TrustLevel::Full, "carol");
        let policy = executor.resolve_policy(&ctx);
        assert_eq!(policy.network, NetworkMode::None);
        assert_eq!(policy.memory_limit, 1024 * 1024 * 1024);
        assert_eq!(policy.pids_limit, 256);
        assert!(policy.long_lived);

        // Hot-reload: allow_network=true, Full trust → Host
        let mut new_config = test_config();
        new_config.sandbox.allow_network = true;
        new_config.sandbox.memory = "4g".to_owned();
        new_config.sandbox.pids_limit = 1024;
        new_config.sandbox.long_lived = false;
        shared.store(Arc::new(new_config));

        let policy = executor.resolve_policy(&ctx);
        assert_eq!(policy.network, NetworkMode::Host);
        assert_eq!(policy.memory_limit, 4 * 1024 * 1024 * 1024);
        assert_eq!(policy.pids_limit, 1024);
        assert!(!policy.long_lived);
    }

    #[test]
    fn resolve_policy_inner_trust_gets_internet_only() {
        let mut config = test_config();
        config.sandbox.allow_network = true;
        let shared = shared_config(config);
        let base_policy = SandboxPolicy::default();
        let executor = SandboxExecutor::new(Arc::new(SimpleExecutor::new()), base_policy, shared);

        let ctx = tool_context_with_user(TrustLevel::Inner, "carol");
        let policy = executor.resolve_policy(&ctx);

        assert_eq!(policy.network, NetworkMode::InternetOnly);
    }

    #[test]
    fn resolve_policy_inner_trust_no_network_when_disabled() {
        // allow_network=false overrides trust-based mapping
        let config = test_config();
        let shared = shared_config(config);
        let base_policy = SandboxPolicy::default();
        let executor = SandboxExecutor::new(Arc::new(SimpleExecutor::new()), base_policy, shared);

        let ctx = tool_context_with_user(TrustLevel::Inner, "carol");
        let policy = executor.resolve_policy(&ctx);

        assert_eq!(policy.network, NetworkMode::None);
    }

    #[test]
    fn resolve_policy_full_trust_gets_host_network() {
        let mut config = test_config();
        config.sandbox.allow_network = true;
        let shared = shared_config(config);
        let base_policy = SandboxPolicy::default();
        let executor = SandboxExecutor::new(Arc::new(SimpleExecutor::new()), base_policy, shared);

        let ctx = tool_context_with_user(TrustLevel::Full, "carol");
        let policy = executor.resolve_policy(&ctx);

        assert_eq!(policy.network, NetworkMode::Host);
    }
}
