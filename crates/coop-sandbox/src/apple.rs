use crate::policy::{ExecOutput, SandboxCapabilities, SandboxInfo, SandboxPolicy};
use anyhow::Result;
use std::time::Duration;

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
            "apple/container CLI not found — install from https://github.com/apple/container"
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
        },
    })
}

/// Execute a command inside an apple/container sandbox on macOS.
#[cfg(target_os = "macos")]
pub async fn exec(policy: &SandboxPolicy, command: &str, timeout: Duration) -> Result<ExecOutput> {
    use tracing::debug;

    debug!(
        command_len = command.len(),
        workspace = %policy.workspace.display(),
        "apple/container exec starting"
    );

    let mut cmd = tokio::process::Command::new("container");
    cmd.arg("run");
    cmd.arg("--rm");

    if policy.memory_limit > 0 {
        let mb = policy.memory_limit / (1024 * 1024);
        cmd.args(["--memory", &format!("{mb}m")]);
    }

    cmd.args(["-v", &format!("{}:/work", policy.workspace.display())]);
    cmd.args(["-w", "/work"]);

    if !policy.allow_network {
        cmd.args(["--network", "none"]);
    }

    cmd.arg("ubuntu:24.04");
    cmd.args(["sh", "-c", command]);

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
