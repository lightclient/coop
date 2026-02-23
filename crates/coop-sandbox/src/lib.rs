//! Process-level sandbox for Coop tool execution.
//!
//! On Linux: uses kernel primitives (namespaces, Landlock, seccomp, cgroups).
//! On macOS: falls back to apple/container CLI.
//! On other platforms: sandboxing is not available.

pub mod policy;

#[cfg(target_os = "linux")]
pub mod linux;

#[cfg(target_os = "macos")]
pub mod apple;

pub use policy::{ExecOutput, SandboxCapabilities, SandboxInfo, SandboxPolicy, parse_memory_size};

#[cfg(target_os = "macos")]
pub use apple::ContainerCleanupPolicy;

use anyhow::Result;
use std::time::Duration;

/// Run a command inside a sandboxed environment.
///
/// On Linux: creates namespaces, applies resource limits, execs the command.
/// On macOS: delegates to apple/container CLI.
///
/// Returns stdout, stderr, exit code.
pub async fn exec(policy: &SandboxPolicy, command: &str, timeout: Duration) -> Result<ExecOutput> {
    exec_with_user_context(policy, command, timeout, None, None).await
}

/// Run a command inside a sandboxed environment with user context for cleanup policy.
///
/// The user information is used to determine cleanup behavior for long-lived containers.
pub async fn exec_with_user_context(
    policy: &SandboxPolicy, 
    command: &str, 
    timeout: Duration, 
    user_name: Option<&str>, 
    user_trust: Option<coop_core::TrustLevel>
) -> Result<ExecOutput> {
    #[cfg(target_os = "linux")]
    {
        linux::exec(policy, command, timeout, user_name, user_trust).await
    }

    #[cfg(target_os = "macos")]
    {
        apple::exec(policy, command, timeout, user_name, user_trust).await
    }

    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = (policy, command, timeout, user_name, user_trust);
        anyhow::bail!("sandboxing not supported on this platform");
    }
}

/// Check whether sandboxing is available on this platform.
/// Returns the isolation mechanism name, or an error describing why it's unavailable.
pub fn probe() -> Result<SandboxInfo> {
    #[cfg(target_os = "linux")]
    {
        linux::probe()
    }

    #[cfg(target_os = "macos")]
    {
        apple::probe()
    }

    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        anyhow::bail!("sandboxing not supported on this platform");
    }
}

/// Clean up old unused containers (platform-specific).
/// This should be called periodically to prevent accumulation of stale containers.
pub async fn cleanup_old_containers() -> Result<()> {
    #[cfg(target_os = "macos")]
    {
        apple::cleanup_old_containers().await
    }

    #[cfg(not(target_os = "macos"))]
    {
        // No cleanup needed for other platforms currently
        Ok(())
    }
}

/// Clean up old unused containers with specific cleanup policy (platform-specific).
#[cfg(target_os = "macos")]
pub async fn cleanup_old_containers_with_policy(policy: Option<&ContainerCleanupPolicy>) -> Result<()> {
    apple::cleanup_old_containers_with_policy(policy).await
}
