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

use anyhow::Result;
use std::time::Duration;

/// Run a command inside a sandboxed environment.
///
/// On Linux: creates namespaces, applies resource limits, execs the command.
/// On macOS: delegates to apple/container CLI.
///
/// Returns stdout, stderr, exit code.
pub async fn exec(policy: &SandboxPolicy, command: &str, timeout: Duration) -> Result<ExecOutput> {
    #[cfg(target_os = "linux")]
    {
        linux::exec(policy, command, timeout).await
    }

    #[cfg(target_os = "macos")]
    {
        apple::exec(policy, command, timeout).await
    }

    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = (policy, command, timeout);
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
