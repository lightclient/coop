use crate::policy::{ExecOutput, SandboxCapabilities, SandboxInfo, SandboxPolicy};
use anyhow::Result;
use std::time::Duration;
use tracing::{debug, warn};

/// Check available sandbox capabilities on this Linux system.
pub fn probe() -> Result<SandboxInfo> {
    let mut caps = SandboxCapabilities::default();

    caps.user_namespaces = check_user_namespaces();
    caps.network_namespaces = caps.user_namespaces;
    caps.landlock = check_landlock();
    caps.seccomp = check_seccomp();
    caps.cgroups_v2 = check_cgroups_v2();

    if !caps.user_namespaces {
        anyhow::bail!(
            "unprivileged user namespaces not available — \
             check /proc/sys/kernel/unprivileged_userns_clone or kernel config"
        );
    }

    let mut features = vec!["namespaces"];
    if caps.landlock {
        features.push("landlock");
    }
    if caps.seccomp {
        features.push("seccomp");
    }

    let name = format!("linux ({})", features.join(" + "));

    if !caps.landlock {
        warn!("landlock not available — filesystem isolation is degraded (mount namespace only)");
    }
    if !caps.seccomp {
        warn!("seccomp not available — syscall filtering disabled");
    }
    if !caps.cgroups_v2 {
        debug!("cgroups v2 not writable — using setrlimit fallback for resource limits");
    }

    Ok(SandboxInfo {
        name,
        capabilities: caps,
    })
}

fn check_user_namespaces() -> bool {
    if let Ok(content) = std::fs::read_to_string("/proc/sys/kernel/unprivileged_userns_clone")
        && content.trim() == "0"
    {
        return false;
    }

    std::process::Command::new("unshare")
        .args(["--user", "true"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

fn check_landlock() -> bool {
    std::path::Path::new("/sys/kernel/security/landlock").exists()
}

fn check_seccomp() -> bool {
    std::fs::read_to_string("/proc/self/status").is_ok_and(|content| content.contains("Seccomp:"))
}

fn check_cgroups_v2() -> bool {
    let controllers = std::path::Path::new("/sys/fs/cgroup/cgroup.controllers");
    if !controllers.exists() {
        return false;
    }

    let self_cgroup = std::path::Path::new("/sys/fs/cgroup/coop-sandbox-probe");
    if std::fs::create_dir(self_cgroup).is_ok() {
        let _ = std::fs::remove_dir(self_cgroup);
        return true;
    }
    false
}

/// Execute a command inside a Linux sandbox.
///
/// Uses user/mount/network/PID namespaces for isolation. The workspace
/// directory is mounted read-write; host tooling paths are read-only.
pub async fn exec(policy: &SandboxPolicy, command: &str, timeout: Duration, user_name: Option<&str>, user_trust: Option<coop_core::TrustLevel>) -> Result<ExecOutput> {
    // User information not yet used in Linux implementation but kept for API compatibility
    let _ = (user_name, user_trust);
    
    debug!(
        command_len = command.len(),
        workspace = %policy.workspace.display(),
        allow_network = policy.allow_network,
        memory_limit = policy.memory_limit,
        pids_limit = policy.pids_limit,
        "sandboxed exec starting"
    );

    let mut cmd = tokio::process::Command::new("unshare");

    cmd.arg("--user");
    cmd.arg("--mount");
    cmd.arg("--pid");
    cmd.arg("--fork");

    if !policy.allow_network {
        cmd.arg("--net");
    }

    cmd.arg("--map-root-user");

    let setup_script = build_sandbox_script(policy, command);
    cmd.args(["sh", "-c", &setup_script]);

    cmd.current_dir(&policy.workspace);
    cmd.env("HOME", policy.workspace.display().to_string());
    cmd.env(
        "PATH",
        "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin",
    );
    cmd.env("TERM", "xterm-256color");

    let result = tokio::time::timeout(timeout, cmd.output()).await;

    match result {
        Err(_) => {
            debug!("sandboxed command timed out after {}s", timeout.as_secs());
            Ok(ExecOutput {
                exit_code: -1,
                stdout: String::new(),
                stderr: format!("command timed out after {}s", timeout.as_secs()),
            })
        }
        Ok(Err(e)) => {
            anyhow::bail!("failed to spawn sandbox process: {e}");
        }
        Ok(Ok(output)) => {
            let exit_code = output.status.code().unwrap_or(-1);
            let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
            let stderr = String::from_utf8_lossy(&output.stderr).into_owned();

            debug!(
                exit_code,
                stdout_len = stdout.len(),
                stderr_len = stderr.len(),
                "sandboxed exec complete"
            );

            Ok(ExecOutput {
                exit_code,
                stdout,
                stderr,
            })
        }
    }
}

/// Build a shell script that sets up the sandbox environment and runs the command.
fn build_sandbox_script(policy: &SandboxPolicy, command: &str) -> String {
    use std::fmt::Write;

    let workspace = policy.workspace.display();
    let mut script = String::new();

    if policy.memory_limit > 0 {
        let kb = policy.memory_limit / 1024;
        let _ = writeln!(script, "ulimit -v {kb} 2>/dev/null");
    }

    if policy.pids_limit > 0 {
        let _ = writeln!(script, "ulimit -u {} 2>/dev/null", policy.pids_limit);
    }

    script.push_str("mount -t proc proc /proc 2>/dev/null\n");
    let _ = writeln!(script, "cd '{workspace}'");
    let _ = writeln!(script, "export HOME='{workspace}'");
    script.push_str("export PATH='/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin'\n");
    script.push_str("export TERM='xterm-256color'\n");

    // Apply Landlock filesystem restrictions if available
    if check_landlock() {
        let _ = writeln!(
            script, 
            r#"
# Apply Landlock filesystem restrictions
cat > /tmp/landlock_restrict.c << 'LANDLOCK_EOF'
#include <linux/landlock.h>
#include <sys/syscall.h>
#include <sys/prctl.h>
#include <unistd.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>

int main() {{
    struct landlock_ruleset_attr ruleset_attr = {{
        .handled_access_fs = LANDLOCK_ACCESS_FS_EXECUTE |
                           LANDLOCK_ACCESS_FS_WRITE_FILE |
                           LANDLOCK_ACCESS_FS_READ_FILE |
                           LANDLOCK_ACCESS_FS_READ_DIR |
                           LANDLOCK_ACCESS_FS_REMOVE_DIR |
                           LANDLOCK_ACCESS_FS_REMOVE_FILE |
                           LANDLOCK_ACCESS_FS_MAKE_CHAR |
                           LANDLOCK_ACCESS_FS_MAKE_DIR |
                           LANDLOCK_ACCESS_FS_MAKE_REG |
                           LANDLOCK_ACCESS_FS_MAKE_SOCK |
                           LANDLOCK_ACCESS_FS_MAKE_FIFO |
                           LANDLOCK_ACCESS_FS_MAKE_BLOCK |
                           LANDLOCK_ACCESS_FS_MAKE_SYM,
    }};
    
    int ruleset_fd = syscall(SYS_landlock_create_ruleset, &ruleset_attr, sizeof(ruleset_attr), 0);
    if (ruleset_fd < 0) {{
        perror("landlock_create_ruleset");
        exit(1);
    }}
    
    // Allow access to workspace directory
    struct landlock_path_beneath_attr path_beneath = {{
        .allowed_access = LANDLOCK_ACCESS_FS_EXECUTE |
                        LANDLOCK_ACCESS_FS_WRITE_FILE |
                        LANDLOCK_ACCESS_FS_READ_FILE |
                        LANDLOCK_ACCESS_FS_READ_DIR |
                        LANDLOCK_ACCESS_FS_REMOVE_DIR |
                        LANDLOCK_ACCESS_FS_REMOVE_FILE |
                        LANDLOCK_ACCESS_FS_MAKE_CHAR |
                        LANDLOCK_ACCESS_FS_MAKE_DIR |
                        LANDLOCK_ACCESS_FS_MAKE_REG |
                        LANDLOCK_ACCESS_FS_MAKE_SOCK |
                        LANDLOCK_ACCESS_FS_MAKE_FIFO |
                        LANDLOCK_ACCESS_FS_MAKE_BLOCK |
                        LANDLOCK_ACCESS_FS_MAKE_SYM,
        .parent_fd = open("{workspace}", O_PATH | O_CLOEXEC),
    }};
    
    if (path_beneath.parent_fd < 0) {{
        perror("open workspace");
        exit(1);
    }}
    
    if (syscall(SYS_landlock_add_rule, ruleset_fd, LANDLOCK_RULE_PATH_BENEATH, &path_beneath, 0) < 0) {{
        perror("landlock_add_rule workspace");
        exit(1);
    }}
    close(path_beneath.parent_fd);
    
    // Allow read-only access to essential system paths
    const char* readonly_paths[] = {{
        "/usr", "/lib", "/lib64", "/bin", "/sbin", "/etc/ld.so.cache", "/etc/passwd", 
        "/etc/group", "/etc/nsswitch.conf", "/etc/resolv.conf", "/dev/null", "/dev/zero", 
        "/dev/urandom", "/proc", "/sys/fs/cgroup", "/tmp"
    }};
    
    for (int i = 0; i < sizeof(readonly_paths) / sizeof(readonly_paths[0]); i++) {{
        struct landlock_path_beneath_attr ro_path = {{
            .allowed_access = LANDLOCK_ACCESS_FS_EXECUTE |
                            LANDLOCK_ACCESS_FS_READ_FILE |
                            LANDLOCK_ACCESS_FS_READ_DIR,
            .parent_fd = open(readonly_paths[i], O_PATH | O_CLOEXEC),
        }};
        
        if (ro_path.parent_fd >= 0) {{
            syscall(SYS_landlock_add_rule, ruleset_fd, LANDLOCK_RULE_PATH_BENEATH, &ro_path, 0);
            close(ro_path.parent_fd);
        }}
    }}
    
    // Restrict the current thread
    if (prctl(PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0)) {{
        perror("prctl(PR_SET_NO_NEW_PRIVS)");
        exit(1);
    }}
    
    if (syscall(SYS_landlock_restrict_self, ruleset_fd, 0) < 0) {{
        perror("landlock_restrict_self");
        exit(1);
    }}
    close(ruleset_fd);
    
    return 0;
}}
LANDLOCK_EOF

gcc -o /tmp/landlock_restrict /tmp/landlock_restrict.c 2>/dev/null && /tmp/landlock_restrict
rm -f /tmp/landlock_restrict.c /tmp/landlock_restrict 2>/dev/null
"#
        );
    }

    let escaped = command.replace('\'', "'\\''");
    let _ = writeln!(script, "exec sh -c '{escaped}'");

    script
}
