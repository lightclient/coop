use crate::policy::{ExecOutput, NetworkMode, SandboxCapabilities, SandboxInfo, SandboxPolicy};
use anyhow::Result;
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;
use tracing::{debug, info, warn};

/// Container cleanup policy
#[derive(Debug, Clone)]
pub struct ContainerCleanupPolicy {
    /// Number of days after which unused containers are cleaned up
    pub cleanup_after_days: u64,
    /// Whether to protect containers owned by full trust users from cleanup
    pub protect_full_trust: bool,
}

impl Default for ContainerCleanupPolicy {
    fn default() -> Self {
        Self {
            cleanup_after_days: 30,   // 1 month default
            protect_full_trust: true, // Protect full trust users by default
        }
    }
}

/// Container registry to manage long-lived containers
static CONTAINER_REGISTRY: std::sync::LazyLock<Mutex<HashMap<String, ContainerInfo>>> =
    std::sync::LazyLock::new(|| Mutex::new(HashMap::new()));

#[derive(Debug, Clone)]
struct ContainerInfo {
    id: String,
    /// Workspace path that was mounted in this container.
    /// Used to validate workspace matches when reusing containers.
    workspace: String,
    last_used: std::time::Instant,
    user_name: Option<String>,
    user_trust: Option<coop_core::TrustLevel>,
}

/// Check whether apple/container is available on this macOS system.
#[cfg(target_os = "macos")]
pub fn probe() -> Result<SandboxInfo> {
    let status = std::process::Command::new("which")
        .arg("container")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()?;

    if !status.success() {
        anyhow::bail!(
            "apple/container CLI not found. Install with:\n\
             brew install apple/apple/container\n\n\
             Or manually from: https://github.com/apple/container\n\n\
             Once installed, restart coop or disable sandbox in config with:\n\
             coop config set sandbox.enabled false"
        );
    }

    Ok(SandboxInfo {
        name: "macos (apple/container VM)".into(),
        capabilities: SandboxCapabilities {
            user_namespaces: false,
            network_namespaces: false,
            landlock: false,
            seccomp: false,
            cgroups_v2: false,
            // VM runs full Linux; iptables available for InternetOnly filtering.
            internet_only: true,
        },
    })
}

/// Generate a container name based on workspace path
fn container_name(workspace: &std::path::Path) -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};

    let mut hasher = DefaultHasher::new();
    workspace.display().to_string().hash(&mut hasher);
    format!("coop-sandbox-{:x}", hasher.finish())
}

/// Insert or update the in-memory container registry entry.
fn register_container(
    name: &str,
    policy: &SandboxPolicy,
    user_name: Option<&str>,
    user_trust: Option<coop_core::TrustLevel>,
) {
    let mut registry = CONTAINER_REGISTRY.lock().unwrap();
    registry
        .entry(name.to_owned())
        .and_modify(|info| {
            info.last_used = std::time::Instant::now();
            if user_name.is_some() {
                info.user_name = user_name.map(|s| s.to_string());
            }
            if user_trust.is_some() {
                info.user_trust = user_trust;
            }
        })
        .or_insert_with(|| ContainerInfo {
            id: name.to_owned(),
            workspace: policy.workspace.display().to_string(),
            last_used: std::time::Instant::now(),
            user_name: user_name.map(|s| s.to_string()),
            user_trust,
        });
}

/// Create or ensure a long-lived container exists for the workspace.
///
/// Uses a probe-based approach instead of parsing `container ps` output,
/// which isn't reliable across container CLI implementations (the Apple
/// `container` CLI uses different subcommands/formats than Docker).
///
/// Strategy:
/// 1. Try `container exec <name> true` — if it works, container is running.
/// 2. Try `container start <name>` — if it works, container was stopped.
/// 3. Create a new container with `container run`.
/// 4. If create fails with "already exists", try start again (race condition).
async fn ensure_container(
    policy: &SandboxPolicy,
    user_name: Option<&str>,
    user_trust: Option<coop_core::TrustLevel>,
) -> Result<String> {
    let name = container_name(&policy.workspace);

    // Step 1: Probe whether the container is already running.
    // `container exec <name> true` is the most reliable check — it works
    // regardless of CLI output format and confirms the container is usable.
    let probe = tokio::process::Command::new("container")
        .args(["exec", &name, "true"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .await;

    if let Ok(status) = probe
        && status.success()
    {
        register_container(&name, policy, user_name, user_trust);
        info!(container = %name, workspace = %policy.workspace.display(), "reusing running container");
        return Ok(name);
    }

    // Step 2: Container might exist but be stopped. Try to start it.
    let start = tokio::process::Command::new("container")
        .args(["start", &name])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .await;

    if let Ok(status) = start
        && status.success()
    {
        register_container(&name, policy, user_name, user_trust);
        info!(container = %name, workspace = %policy.workspace.display(), "started and reusing stopped container");
        return Ok(name);
    }

    // Step 3: Container doesn't exist — create it.
    debug!(container = %name, workspace = %policy.workspace.display(), "creating new long-lived container");

    let mut cmd = tokio::process::Command::new("container");
    cmd.arg("run");
    cmd.args(["-d", "--name", &name]); // -d for detached mode, remove --rm

    if policy.memory_limit > 0 {
        let mb = policy.memory_limit / (1024 * 1024);
        cmd.args(["--memory", &format!("{mb}m")]);
    }

    cmd.args(["-v", &format!("{}:/work", policy.workspace.display())]);
    cmd.args(["-w", "/work"]);

    match policy.network {
        NetworkMode::None => {
            cmd.args(["--network", "none"]);
        }
        NetworkMode::Host | NetworkMode::InternetOnly => {}
    }

    // Use an image with common development tools pre-installed
    cmd.arg("ubuntu:24.04");
    // Keep the container running with a long-lived process
    cmd.args(["tail", "-f", "/dev/null"]);

    let output = cmd.output().await?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);

        // Step 4: Race condition — container appeared between our probe and
        // create. Try to start and reuse it.
        if stderr.contains("already exists") {
            info!(container = %name, "container already exists, attempting to start and reuse");
            let _ = tokio::process::Command::new("container")
                .args(["start", &name])
                .status()
                .await;

            register_container(&name, policy, user_name, user_trust);
            info!(container = %name, workspace = %policy.workspace.display(), "reused existing container after create conflict");
            return Ok(name);
        }

        anyhow::bail!("failed to create container {}: {}", name, stderr);
    }

    // Install common development tools in the new container
    info!(container = %name, "installing development tools in new container");
    let install_result = tokio::process::Command::new("container")
        .args([
            "exec",
            &name,
            "sh",
            "-c",
            "apt-get update && apt-get install -y \
             curl wget git vim nano build-essential \
             python3 python3-pip python3-venv \
             nodejs npm \
             rust-bin-stable \
             && apt-get clean \
             && rm -rf /var/lib/apt/lists/*",
        ])
        .output()
        .await?;

    if !install_result.status.success() {
        warn!(
            container = %name,
            stderr = %String::from_utf8_lossy(&install_result.stderr),
            "failed to install development tools, container will still work but with limited tooling"
        );
    } else {
        info!(container = %name, "development tools installed successfully");
    }

    register_container(&name, policy, user_name, user_trust);
    info!(container = %name, workspace = %policy.workspace.display(), "created new long-lived container");
    Ok(name)
}

/// Clean up old unused containers (call periodically)
pub async fn cleanup_old_containers() -> Result<()> {
    cleanup_old_containers_with_policy(None).await
}

/// Clean up old unused containers with specific policy
pub async fn cleanup_old_containers_with_policy(
    policy: Option<&ContainerCleanupPolicy>,
) -> Result<()> {
    let default_policy = ContainerCleanupPolicy::default();
    let policy = policy.unwrap_or(&default_policy);

    let old_containers = {
        let mut registry = CONTAINER_REGISTRY.lock().unwrap();
        let cutoff = std::time::Instant::now()
            - Duration::from_secs(policy.cleanup_after_days * 24 * 60 * 60);

        let to_remove: Vec<_> = registry
            .iter()
            .filter(|(_, info)| {
                // Never clean up containers for full trust users if protect_full_trust is enabled
                if policy.protect_full_trust {
                    if let Some(trust) = info.user_trust {
                        if trust >= coop_core::TrustLevel::Full {
                            debug!(container = %info.id, trust = ?trust, "skipping cleanup for full trust user");
                            return false;
                        }
                    }
                }
                info.last_used < cutoff
            })
            .map(|(name, _)| name.clone())
            .collect();

        for name in &to_remove {
            registry.remove(name);
        }

        to_remove
    };

    for name in old_containers {
        info!(container = %name, "cleaning up old container");
        let _ = tokio::process::Command::new("container")
            .args(["stop", &name])
            .status()
            .await;
        let _ = tokio::process::Command::new("container")
            .args(["rm", &name])
            .status()
            .await;
    }

    Ok(())
}

/// Execute a command inside an apple/container sandbox on macOS.
#[cfg(target_os = "macos")]
pub async fn exec(
    policy: &SandboxPolicy,
    command: &str,
    timeout: Duration,
    user_name: Option<&str>,
    user_trust: Option<coop_core::TrustLevel>,
) -> Result<ExecOutput> {
    debug!(
        command_len = command.len(),
        workspace = %policy.workspace.display(),
        long_lived = policy.long_lived,
        network = ?policy.network,
        "apple/container exec starting"
    );

    if policy.long_lived {
        // Use persistent container that survives between commands
        exec_long_lived(policy, command, timeout, user_name, user_trust).await
    } else {
        // Use ephemeral container (original behavior)
        exec_ephemeral(policy, command, timeout).await
    }
}

/// Execute command in a long-lived container
async fn exec_long_lived(
    policy: &SandboxPolicy,
    command: &str,
    timeout: Duration,
    user_name: Option<&str>,
    user_trust: Option<coop_core::TrustLevel>,
) -> Result<ExecOutput> {
    let container_name = ensure_container(policy, user_name, user_trust).await?;

    // Execute the command in the existing container
    let mut cmd = tokio::process::Command::new("container");
    cmd.args(["exec", "-w", "/work", &container_name]);
    cmd.args(["sh", "-c", command]);

    let result = tokio::time::timeout(timeout, cmd.output()).await;

    match result {
        Err(_) => Ok(ExecOutput {
            exit_code: -1,
            stdout: String::new(),
            stderr: format!("command timed out after {}s", timeout.as_secs()),
        }),
        Ok(Err(e)) => anyhow::bail!("failed to exec in container {}: {e}", container_name),
        Ok(Ok(output)) => {
            let exit_code = output.status.code().unwrap_or(-1);
            debug!(container = %container_name, exit_code, "apple/container exec complete");
            Ok(ExecOutput {
                exit_code,
                stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
                stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
            })
        }
    }
}

/// Execute command in an ephemeral container (original behavior)
async fn exec_ephemeral(
    policy: &SandboxPolicy,
    command: &str,
    timeout: Duration,
) -> Result<ExecOutput> {
    let mut cmd = tokio::process::Command::new("container");
    cmd.arg("run");
    cmd.arg("--rm"); // Remove container after execution

    if policy.memory_limit > 0 {
        let mb = policy.memory_limit / (1024 * 1024);
        cmd.args(["--memory", &format!("{mb}m")]);
    }

    cmd.args(["-v", &format!("{}:/work", policy.workspace.display())]);
    cmd.args(["-w", "/work"]);

    let effective_command;
    match policy.network {
        NetworkMode::None => {
            cmd.args(["--network", "none"]);
            effective_command = command.to_owned();
        }
        NetworkMode::Host => {
            effective_command = command.to_owned();
        }
        NetworkMode::InternetOnly => {
            // VM has full networking; block private ranges via iptables.
            effective_command = format!(
                concat!(
                    "for NET in 10.0.0.0/8 172.16.0.0/12 192.168.0.0/16 ",
                    "169.254.0.0/16 100.64.0.0/10; do ",
                    "iptables -A OUTPUT -d \"$NET\" -j REJECT 2>/dev/null; done; ",
                    "for NET6 in fc00::/7 fe80::/10; do ",
                    "ip6tables -A OUTPUT -d \"$NET6\" -j REJECT 2>/dev/null; done; ",
                    "{}"
                ),
                command
            );
        }
    }

    cmd.arg("ubuntu:24.04");
    cmd.args(["sh", "-c", &effective_command]);

    let result = tokio::time::timeout(timeout, cmd.output()).await;

    match result {
        Err(_) => Ok(ExecOutput {
            exit_code: -1,
            stdout: String::new(),
            stderr: format!("command timed out after {}s", timeout.as_secs()),
        }),
        Ok(Err(e)) => anyhow::bail!("failed to spawn apple/container: {e}"),
        Ok(Ok(output)) => {
            let exit_code = output.status.code().unwrap_or(-1);
            debug!(exit_code, "apple/container exec complete");
            Ok(ExecOutput {
                exit_code,
                stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
                stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
            })
        }
    }
}
