# Sandbox with Owner Role

Implement sandboxed tool execution for Coop with a new "owner" role that bypasses the sandbox. All non-owner users must have their bash tool calls execute inside a sandboxed child process. The owner operates directly on the host, unsandboxed.

On Linux, the sandbox uses kernel primitives directly (namespaces, Landlock, seccomp, cgroups) — no Docker, no container CLI, no daemon. On macOS, it falls back to apple/container (CLI — the only viable option).

Read `AGENTS.md` at the project root before starting. Follow the development loop and code quality rules.

## Background

Read `docs/sandbox-analysis.md` for the full threat model and design rationale.

Today, `BashTool` in `crates/coop-core/src/tools/bash.rs` runs `Command::new("sh").arg("-c").arg(command)` directly on the host. There is no filesystem scoping, no network restriction, and no process isolation. The only protection is a trust-level gate that limits which users can run bash at all.

The core insight: the **owner** (the person who runs Coop on their machine) is trusted implicitly — they already have full host access. Everyone else — Signal contacts, webhook callers, cron jobs on behalf of other users — is running code on someone else's machine and must be sandboxed.

## New Role: Owner

### TrustLevel changes

Add `Owner` as a new variant to `TrustLevel` in `crates/coop-core/src/types.rs`:

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TrustLevel {
    Owner,     // NEW — bypasses sandbox, full host access
    Full,
    Inner,
    Familiar,
    Public,
}
```

Update the `rank()` function and `Ord` implementation:

```rust
fn rank(self) -> u8 {
    match self {
        Self::Owner => 0,    // most privileged
        Self::Full => 1,
        Self::Inner => 2,
        Self::Familiar => 3,
        Self::Public => 4,
    }
}
```

Ordering remains: `Owner < Full < Inner < Familiar < Public` (most trusted is "smallest").

### Semantics

| Trust Level | Sandbox | Tools | Memory Stores | Use Case |
|-------------|---------|-------|---------------|----------|
| **Owner** | No — executes directly on host | All tools, no restrictions | All (private, shared, social) | The person running Coop |
| **Full** | Yes — sandboxed to workspace | All tools, inside sandbox | All (private, shared, social) | Highly trusted remote user |
| **Inner** | Yes — sandboxed to workspace | Bash + file tools, inside sandbox | shared, social | Close contact (e.g. Bob) |
| **Familiar** | Yes — sandboxed to workspace | No bash, read-only file tools | social | Known contact (e.g. Carol) |
| **Public** | Yes — sandboxed to workspace | No tools | None | Unknown sender |

Key distinction: `Owner` and `Full` have the same tool access, but `Owner` runs unsandboxed on the host while `Full` runs inside a sandbox. This is the **only** behavioral difference between `Owner` and `Full`.

### Persistence model

There are no containers, images, or lifecycle to manage. The sandbox is a set of kernel policies applied to the child process before `exec`. The workspace directory is the persistence layer — it's a normal directory on the host filesystem that the sandboxed process can read and write.

Users can install tools, build binaries, and download files into their workspace. Everything in the workspace persists across sessions and restarts because it's just files on disk. System-level installs (`apt install`, `pip install` to global paths) don't work because the sandbox restricts writes to the workspace + `/tmp`. But workspace-local equivalents work fine:

```bash
cargo build                        # ✅ writes to ./target/ in workspace
./target/debug/myapp               # ✅ execute from workspace
curl -L ... -o ./tools/rg          # ✅ download binary to workspace
chmod +x ./tools/rg && ./tools/rg  # ✅ execute it
pip install --target ./venv ...    # ✅ install into workspace
PATH="$PWD/tools:$PATH" rg ...    # ✅ use workspace binaries
```

### Config

In `coop.toml`, the owner is declared like any other user:

```toml
[[users]]
name = "alice"
trust = "owner"
match = ["terminal:default", "signal:alice-uuid"]
```

There should be exactly one owner (or zero, if running without users configured — the terminal default remains `Full` trust for backward compatibility). `config_check` should warn if multiple users are declared as `owner`, and warn if sandbox is enabled but no owner exists (meaning the terminal user would also be sandboxed).

### Terminal default trust

Today, unmatched terminal messages get `TrustLevel::Public`. When sandbox is enabled and no user config matches the terminal, the terminal user would be sandboxed by default. This is wrong — the person at the terminal IS the machine owner.

**When sandbox is enabled:** The terminal channel (`terminal:default`) should default to `Owner` trust if no user match is found. This ensures the person physically at the machine is never sandboxed. For all other channels (Signal, webhooks), unmatched senders remain `Public`.

**When sandbox is disabled:** No behavioral change. Trust defaults remain as they are today (unmatched = `Public`, but sandbox doesn't apply so it doesn't matter for tool execution).

Update `route_message` in `crates/coop-gateway/src/router.rs`:

```rust
// When no user matches and the message is from the terminal:
let user_trust = matched_user.map_or_else(
    || {
        if msg.channel == "terminal:default" && sandbox_enabled {
            TrustLevel::Owner
        } else {
            TrustLevel::Public
        }
    },
    |user| user.trust,
);
```

This requires `route_message` to know whether sandbox is enabled. Pass it as a parameter or include it in the config that's already passed.

### Trust resolution

Update `resolve_trust` in `crates/coop-gateway/src/trust.rs`. The existing logic (`max(user_trust, ceiling)`) already works correctly with the new `Owner` variant because `Owner` has rank 0 (lowest = most trusted) and `max()` picks the least trusted of the two.

However, verify that `Owner` in a group context resolves to `Familiar` (the group ceiling), NOT `Owner`. The owner shouldn't get unsandboxed access in group chats — group messages are visible to other participants and may contain injected content.

```rust
// Owner in DM → Owner (unsandboxed)
assert_eq!(resolve_trust(TrustLevel::Owner, TrustLevel::Full), TrustLevel::Owner);
// Owner in group → Familiar (sandboxed, like everyone else in groups)
assert_eq!(resolve_trust(TrustLevel::Owner, TrustLevel::Familiar), TrustLevel::Familiar);
```

This is already how the math works. Just add the test cases.

### Memory store access

Update `accessible_stores` in `crates/coop-gateway/src/trust.rs`:

```rust
pub(crate) fn accessible_stores(trust: TrustLevel) -> Vec<&'static str> {
    match trust {
        TrustLevel::Owner => vec!["private", "shared", "social"],
        TrustLevel::Full => vec!["private", "shared", "social"],
        TrustLevel::Inner => vec!["shared", "social"],
        TrustLevel::Familiar => vec!["social"],
        TrustLevel::Public => vec![],
    }
}
```

Owner gets the same memory access as Full.

## Sandbox Implementation

### Design principles

1. **No Docker, no container daemon, no CLI shelling on Linux.** The sandbox uses Linux kernel primitives directly via syscalls. The only platform where CLI shelling is acceptable is macOS (apple/container), where no in-process alternative exists.
2. **No container lifecycle.** There are no containers to create, commit, start, stop, or destroy. Each sandboxed bash invocation spawns a child process with kernel policies applied before `exec`. When the command finishes, the process exits. No state to manage.
3. **Workspace is the persistence layer.** The workspace directory is bind-mounted read-write. Everything else is read-only or invisible. Users persist data by writing to the workspace. No image commits, no overlay filesystems.
4. **Opt-in.** Sandbox is disabled by default. When disabled, all tools run directly on the host as today.

### How it works (Linux)

When a non-owner user runs a bash command, instead of:
```rust
Command::new("sh").arg("-c").arg(command).current_dir(workspace)
```

The sandbox does:
1. `fork()` a child process
2. In the child, before `exec`:
   - Create new **user namespace** (`unshare(CLONE_NEWUSER)`) — maps to unprivileged user outside
   - Create new **mount namespace** (`unshare(CLONE_NEWNS)`) — isolate filesystem view
   - Create new **network namespace** (`unshare(CLONE_NEWNET)`) — empty network stack (no interfaces)
   - Create new **PID namespace** (`unshare(CLONE_NEWPID)`) — can't see host processes
   - Bind-mount the workspace read-write at the working directory
   - Bind-mount `/usr`, `/bin`, `/lib`, `/lib64` read-only (host tooling)
   - Mount `/tmp` as tmpfs (writable, ephemeral, size-limited)
   - Mount `/dev/null`, `/dev/zero`, `/dev/urandom` (required by most programs)
   - Apply **Landlock** policy — restrict filesystem access to only the mounted paths
   - Apply **seccomp** filter — block dangerous syscalls (`mount`, `ptrace`, `reboot`, etc.)
3. `exec("sh", "-c", command)` in the sandboxed environment
4. Parent reads stdout/stderr, enforces timeout, returns result

**Resource limits** are applied via cgroups v2 (memory, PIDs, CPU) on the child process. On systems without cgroup write access, fall back to `setrlimit` for basic limits.

### What the sandboxed process sees

```
/work/              ← workspace (read-write) — this is $PWD and $HOME
/usr/               ← host /usr (read-only)
/bin/               ← host /bin (read-only)
/lib/               ← host /lib (read-only)
/lib64/             ← host /lib64 (read-only, if exists)
/tmp/               ← tmpfs (read-write, ephemeral, size-limited)
/dev/null           ← character device
/dev/zero           ← character device
/dev/urandom        ← character device
/proc/              ← procfs (new PID namespace, only sees own processes)
```

Everything else is invisible. No `~/.ssh/`, no `~/.aws/`, no `/etc/shadow`, no other users' home directories. Network interfaces don't exist — `curl`, `nc`, DNS all fail immediately.

### Threat model coverage

| Threat | Mitigation | Mechanism |
|--------|-----------|-----------|
| Read `~/.ssh/id_rsa` | Path doesn't exist in sandbox | Mount namespace + Landlock |
| `curl` exfiltration | No network interfaces | Network namespace (empty) |
| Lateral movement to internal services | No network | Network namespace |
| `rm -rf /` | `/usr` etc. are read-only, only workspace is writable | Read-only bind mounts + Landlock |
| Fork bomb | PID limit enforced | cgroups `pids.max` / `setrlimit` |
| Memory exhaustion | Memory limit enforced | cgroups `memory.max` / `setrlimit` |
| `mount` / `ptrace` escalation | Syscall blocked | seccomp BPF filter |
| Escape via `/proc` or `/sys` | Only PID-namespaced `/proc`, no `/sys` | Mount namespace |

### Where sandbox lives

The sandbox decision (sandboxed vs. unsandboxed) is made at the **tool execution** layer, not in the tools themselves. The existing tools (`BashTool`, `ReadFileTool`, etc.) don't change. A new `SandboxExecutor` wraps the existing executor and intercepts calls for non-owner sessions.

### Crate structure

Create a new crate: `crates/coop-sandbox/`

```
crates/coop-sandbox/
├── Cargo.toml
├── src/
│   ├── lib.rs           # Public API: SandboxPolicy, sandboxed_exec()
│   ├── linux.rs         # Linux implementation: namespaces + Landlock + seccomp
│   ├── landlock.rs      # Landlock policy builder
│   ├── seccomp.rs       # seccomp BPF filter
│   ├── apple.rs         # macOS: apple/container CLI fallback
│   └── policy.rs        # SandboxPolicy config type
└── tests/
    ├── linux.rs         # Linux sandbox integration tests (gated)
    └── policy.rs        # Unit tests for policy construction
```

**Cargo.toml dependencies** — keep minimal for compile times:

| Crate | Purpose | Notes |
|-------|---------|-------|
| `nix` | Namespace, mount, pivot_root, unshare syscalls | Widely used, moderate size. Use feature flags to pull only what's needed. |
| `landlock` | Landlock filesystem policy | Tiny, pure Rust. Maintained by Landlock author. |
| `seccompiler` | seccomp BPF filter builder | Small, from Firecracker project. |
| `tokio` | `process`, `sync`, `time` features | For async exec, timeouts |
| `tracing` | Instrumentation | |
| `anyhow` | Error handling | |

Do NOT depend on `coop-core`. The sandbox crate is a standalone process-level sandbox. The integration point (`SandboxExecutor`) lives in `coop-gateway` where both crates are available.

### Platform support

| Platform | Backend | Implementation | Isolation |
|----------|---------|----------------|-----------|
| **Linux** | `LinuxSandbox` | In-process syscalls (namespaces, Landlock, seccomp, cgroups) | Namespace + filesystem + network + syscall |
| **macOS** | `AppleSandbox` | apple/container CLI (shelling out — only option) | VM per invocation |
| **Other / fallback** | None | Direct execution with warning | None |

### Public API

The sandbox crate exposes a simple, stateless API. There's no manager, no lifecycle, no containers — just a function that runs a command in a sandbox:

```rust
/// Run a command inside a sandboxed environment.
///
/// On Linux: creates namespaces, applies Landlock + seccomp, execs the command.
/// On macOS: delegates to apple/container CLI.
///
/// Returns stdout, stderr, exit code.
pub async fn exec(policy: &SandboxPolicy, command: &str, timeout: Duration) -> Result<ExecOutput> {
    #[cfg(target_os = "linux")]
    return linux::exec(policy, command, timeout).await;

    #[cfg(target_os = "macos")]
    return apple::exec(policy, command, timeout).await;

    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    anyhow::bail!("sandboxing not supported on this platform");
}

/// Check whether sandboxing is available on this platform.
/// Returns the isolation mechanism name, or an error describing why it's unavailable.
pub fn probe() -> Result<SandboxInfo> {
    #[cfg(target_os = "linux")]
    return linux::probe();

    #[cfg(target_os = "macos")]
    return apple::probe();

    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    anyhow::bail!("sandboxing not supported on this platform");
}
```

### SandboxPolicy

```rust
/// Policy for a sandboxed command execution.
pub struct SandboxPolicy {
    /// Directory mounted read-write as the working directory.
    pub workspace: PathBuf,

    /// Whether to allow network access. Default: false (empty network namespace).
    pub allow_network: bool,

    /// Memory limit in bytes. 0 = no limit.
    pub memory_limit: u64,

    /// Max number of PIDs (fork bomb protection). 0 = no limit.
    pub pids_limit: u32,
}
```

No images, no container names, no persistence config. The policy describes the constraints for a single execution.

### ExecOutput

```rust
pub struct ExecOutput {
    pub exit_code: i32,
    pub stdout: String,
    pub stderr: String,
}
```

### SandboxInfo

```rust
pub struct SandboxInfo {
    /// Human-readable name: "linux (namespaces + landlock + seccomp)" or "macos (apple/container)"
    pub name: String,
    /// What's available and what's degraded
    pub capabilities: SandboxCapabilities,
}

pub struct SandboxCapabilities {
    pub user_namespaces: bool,     // false if kernel.unprivileged_userns_clone=0
    pub network_namespaces: bool,
    pub landlock: bool,            // false if kernel < 5.13 or not enabled
    pub seccomp: bool,
    pub cgroups_v2: bool,          // false if no write access to cgroupfs
}
```

`probe()` checks each capability independently. The sandbox works in **degraded mode** if some capabilities are missing — e.g., if Landlock is unavailable but namespaces work, the mount namespace still provides filesystem isolation. `probe()` reports what's available so the gateway can log it at startup.

### Linux implementation (`linux.rs`)

```rust
pub async fn exec(policy: &SandboxPolicy, command: &str, timeout: Duration) -> Result<ExecOutput> {
    // 1. Set up cgroup (if available) for resource limits
    let cgroup = setup_cgroup(policy)?;  // best-effort, may be None

    // 2. Fork+exec via tokio::process::Command with pre_exec hook
    let output = tokio::time::timeout(timeout, {
        let mut cmd = tokio::process::Command::new("sh");
        cmd.arg("-c").arg(command);
        cmd.current_dir(&policy.workspace);
        cmd.env("HOME", "/work");
        cmd.env("PATH", "/work/tools:/usr/local/bin:/usr/bin:/bin");

        // SAFETY: pre_exec runs in the forked child before exec.
        // All functions called here must be async-signal-safe.
        unsafe {
            cmd.pre_exec(move || {
                apply_sandbox(policy)?;
                Ok(())
            });
        }

        cmd.output()
    }).await??;

    // 3. Clean up cgroup (if created)
    if let Some(cg) = cgroup {
        cleanup_cgroup(cg);
    }

    // 4. Return result
    Ok(ExecOutput { ... })
}

/// Applied in the forked child, before exec. Must be async-signal-safe.
fn apply_sandbox(policy: &SandboxPolicy) -> std::io::Result<()> {
    // User namespace — run as "root" inside, unprivileged outside
    unshare(CloneFlags::CLONE_NEWUSER)?;
    write_uid_gid_map()?;

    // Mount namespace — isolate filesystem view
    unshare(CloneFlags::CLONE_NEWNS)?;
    setup_mounts(policy)?;

    // Network namespace — empty, no interfaces
    if !policy.allow_network {
        unshare(CloneFlags::CLONE_NEWNET)?;
    }

    // PID namespace
    unshare(CloneFlags::CLONE_NEWPID)?;

    // Landlock — restrict filesystem access
    apply_landlock(policy)?;

    // seccomp — restrict syscalls (must be last — can't make more policy changes after)
    apply_seccomp()?;

    Ok(())
}
```

**Mount setup (`setup_mounts`):**
```rust
fn setup_mounts(policy: &SandboxPolicy) -> std::io::Result<()> {
    // Make all existing mounts private (don't propagate to host)
    mount(None::<&str>, "/", None::<&str>, MsFlags::MS_REC | MsFlags::MS_PRIVATE, None::<&str>)?;

    // Create a new root with minimal filesystem
    let new_root = tempdir()?;

    // Bind-mount workspace read-write
    bind_mount(&policy.workspace, new_root.join("work"), /* readonly: */ false)?;

    // Bind-mount host tooling read-only
    for path in &["/usr", "/bin", "/lib", "/lib64", "/etc/alternatives", "/etc/ld.so.cache"] {
        if Path::new(path).exists() {
            bind_mount(path, new_root.join(path.trim_start_matches('/')), /* readonly: */ true)?;
        }
    }

    // Mount tmpfs for /tmp
    mount_tmpfs(new_root.join("tmp"), "512m")?;

    // Mount minimal /dev
    create_dev_nodes(new_root.join("dev"))?;  // null, zero, urandom

    // Mount /proc (PID-namespaced)
    mount_proc(new_root.join("proc"))?;

    // pivot_root into the new root
    pivot_root(&new_root, &old_root)?;
    umount_old_root()?;

    Ok(())
}
```

**Landlock policy (`landlock.rs`):**
```rust
fn apply_landlock(policy: &SandboxPolicy) -> std::io::Result<()> {
    let abi = ABI::V5;  // or best available
    let status = Ruleset::default()
        .handle_access(AccessFs::from_all(abi))?
        .create()?
        // Workspace: full access
        .add_rule(PathBeneath::new(open_path("/work")?, AccessFs::from_all(abi)))?
        // /tmp: full access
        .add_rule(PathBeneath::new(open_path("/tmp")?, AccessFs::from_all(abi)))?
        // /usr, /bin, /lib: read + execute only
        .add_rule(PathBeneath::new(open_path("/usr")?, AccessFs::from_read(abi) | AccessFs::Execute))?
        .add_rule(PathBeneath::new(open_path("/bin")?, AccessFs::from_read(abi) | AccessFs::Execute))?
        .add_rule(PathBeneath::new(open_path("/lib")?, AccessFs::from_read(abi) | AccessFs::Execute))?
        // /proc: read only
        .add_rule(PathBeneath::new(open_path("/proc")?, AccessFs::from_read(abi)))?
        // /dev: read only
        .add_rule(PathBeneath::new(open_path("/dev")?, AccessFs::from_read(abi)))?
        .restrict_self()?;
    Ok(())
}
```

**seccomp filter (`seccomp.rs`):**
```rust
fn apply_seccomp() -> std::io::Result<()> {
    // Block dangerous syscalls. Default action: allow.
    // This is a denylist approach — simpler and more compatible than allowlist.
    let filter = SeccompFilter::new(
        vec![
            // Prevent namespace/mount escalation
            (libc::SYS_mount, SeccompAction::Errno(libc::EPERM)),
            (libc::SYS_umount2, SeccompAction::Errno(libc::EPERM)),
            (libc::SYS_pivot_root, SeccompAction::Errno(libc::EPERM)),
            (libc::SYS_unshare, SeccompAction::Errno(libc::EPERM)),
            (libc::SYS_setns, SeccompAction::Errno(libc::EPERM)),

            // Prevent ptrace (debugging/injection)
            (libc::SYS_ptrace, SeccompAction::Errno(libc::EPERM)),
            (libc::SYS_process_vm_readv, SeccompAction::Errno(libc::EPERM)),
            (libc::SYS_process_vm_writev, SeccompAction::Errno(libc::EPERM)),

            // Prevent kernel module loading
            (libc::SYS_init_module, SeccompAction::Errno(libc::EPERM)),
            (libc::SYS_finit_module, SeccompAction::Errno(libc::EPERM)),
            (libc::SYS_delete_module, SeccompAction::Errno(libc::EPERM)),

            // Prevent reboot/kexec
            (libc::SYS_reboot, SeccompAction::Errno(libc::EPERM)),
            (libc::SYS_kexec_load, SeccompAction::Errno(libc::EPERM)),
            (libc::SYS_kexec_file_load, SeccompAction::Errno(libc::EPERM)),

            // Prevent keyring manipulation
            (libc::SYS_add_key, SeccompAction::Errno(libc::EPERM)),
            (libc::SYS_request_key, SeccompAction::Errno(libc::EPERM)),
            (libc::SYS_keyctl, SeccompAction::Errno(libc::EPERM)),
        ],
        SeccompAction::Allow,  // default: allow everything else
    )?;
    SeccompFilter::apply(filter)?;
    Ok(())
}
```

### macOS implementation (`apple.rs`)

On macOS, there are no kernel primitives for process-level sandboxing. The only option is apple/container, which provides VM-grade isolation via Virtualization.framework but requires CLI invocation.

```rust
#[cfg(target_os = "macos")]
pub async fn exec(policy: &SandboxPolicy, command: &str, timeout: Duration) -> Result<ExecOutput> {
    // apple/container uses a VM per invocation — heavier but maximally isolated.
    // Each exec creates a short-lived container, runs the command, captures output, destroys.
    let output = tokio::time::timeout(timeout, {
        Command::new("container")
            .arg("run")
            .arg("--rm")
            .args(["--memory", &format_memory(policy.memory_limit)])
            .args(["-v", &format!("{}:/work", policy.workspace.display())])
            .args(["-w", "/work"])
            .args(network_args(policy))
            .arg("ubuntu:24.04")  // or configured base image
            .args(["sh", "-c", command])
            .output()
    }).await??;

    Ok(ExecOutput {
        exit_code: output.status.code().unwrap_or(-1),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    })
}

#[cfg(target_os = "macos")]
pub fn probe() -> Result<SandboxInfo> {
    // Check if `container` CLI is available
    let status = std::process::Command::new("which")
        .arg("container")
        .status()?;
    if !status.success() {
        anyhow::bail!("apple/container CLI not found — install from https://github.com/apple/container");
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
```

**macOS performance note:** Each bash invocation spins up a lightweight VM. This adds ~200-500ms latency per command. For interactive use this is noticeable. Consider a long-lived container approach for macOS in a future phase if latency is a problem (keep a `container run -d` VM per session, `container exec` into it).

### Integration in coop-gateway

#### SandboxExecutor

Create `crates/coop-gateway/src/sandbox_executor.rs`:

```rust
pub(crate) struct SandboxExecutor {
    inner: Arc<dyn ToolExecutor>,
    base_policy: SandboxPolicy,
}
```

Implements `ToolExecutor`. The routing logic:

```rust
#[async_trait]
impl ToolExecutor for SandboxExecutor {
    async fn execute(&self, name: &str, arguments: Value, ctx: &ToolContext) -> Result<ToolOutput> {
        // Owner trust → bypass sandbox, delegate directly to inner executor
        if ctx.trust == TrustLevel::Owner {
            return self.inner.execute(name, arguments, ctx).await;
        }

        // Non-owner: route sandboxable tools through the sandbox
        match name {
            "bash" => self.exec_bash_sandboxed(arguments, ctx).await,
            // File tools operate on the workspace directory directly — no sandboxing needed.
            // They're already workspace-scoped and don't execute code.
            _ => self.inner.execute(name, arguments, ctx).await,
        }
    }

    fn tools(&self) -> Vec<ToolDef> {
        self.inner.tools()
    }
}
```

**Why only `bash` goes through the sandbox:**
- `bash` executes arbitrary code — this is the attack surface.
- `read_file`, `write_file`, `edit_file` are already workspace-scoped by their implementations and don't execute code. They operate on files in the workspace directory. The workspace is the same directory on the host.
- Non-sandboxable tools (`memory_*`, `config_*`, `web_search`, etc.) pass through to the inner executor. These are Coop-internal tools with their own trust gates.

**`exec_bash_sandboxed`:**
1. Extract `command` from arguments.
2. Build a `SandboxPolicy` from the base policy + any per-user overrides from context.
3. Call `coop_sandbox::exec(&policy, &command, TIMEOUT)`.
4. Apply the same output truncation as `BashTool` (reuse `coop_core::tools::truncate`).
5. Return `ToolOutput` with appropriate success/error status based on exit code.

```rust
async fn exec_bash_sandboxed(&self, arguments: Value, ctx: &ToolContext) -> Result<ToolOutput> {
    if ctx.trust > TrustLevel::Inner {
        return Ok(ToolOutput::error("bash tool requires Full or Inner trust level"));
    }

    let command = arguments
        .get("command")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("missing required parameter: command"))?;

    let policy = SandboxPolicy {
        workspace: ctx.workspace.clone(),
        ..self.base_policy.clone()
    };

    let result = coop_sandbox::exec(&policy, command, TIMEOUT).await;

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

            let r = truncate::truncate_tail(&combined);
            // ... same truncation logic as BashTool ...

            if output.exit_code == 0 {
                Ok(ToolOutput::success(final_output))
            } else {
                Ok(ToolOutput::error(format!("exit code {}\n{final_output}", output.exit_code)))
            }
        }
    }
}
```

#### Gateway wiring

In `Gateway::new()`, after building the executor:

```rust
let executor: Arc<dyn ToolExecutor> = if config.load().sandbox.enabled {
    match coop_sandbox::probe() {
        Ok(info) => {
            info!(sandbox = %info.name, "sandbox enabled");
            let policy = SandboxPolicy::from_config(&config.load().sandbox);
            Arc::new(SandboxExecutor::new(executor, policy))
        }
        Err(e) => {
            warn!(error = %e, "sandbox enabled but not available on this platform — running unsandboxed");
            executor
        }
    }
} else {
    executor
};
```

No session cleanup needed — there are no containers or lifecycle to manage. Each sandboxed bash call is a self-contained child process that exits when done.

#### ToolContext changes

`ToolContext` already carries `trust: TrustLevel`. The `SandboxExecutor` uses `ctx.trust == TrustLevel::Owner` to decide whether to sandbox. No new fields needed on `ToolContext`.

For per-user sandbox overrides (phase 5), add later:
```rust
pub sandbox_overrides: Option<SandboxOverrides>,
```

### Config

Add to `Config` in `crates/coop-gateway/src/config.rs`:

```rust
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub(crate) struct SandboxConfig {
    /// Enable sandboxed tool execution for non-owner users. Default: false.
    #[serde(default)]
    pub enabled: bool,

    /// Allow sandboxed processes to access the network. Default: false.
    #[serde(default)]
    pub allow_network: bool,

    /// Memory limit per sandboxed command. Default: "2g".
    #[serde(default = "default_sandbox_memory")]
    pub memory: String,

    /// Max PIDs per sandboxed command (fork bomb protection). Default: 512.
    #[serde(default = "default_sandbox_pids")]
    pub pids_limit: u32,
}

fn default_sandbox_memory() -> String { "2g".to_owned() }
fn default_sandbox_pids() -> u32 { 512 }
```

Add as a field on `Config`:
```rust
pub(crate) struct Config {
    pub agent: AgentConfig,
    #[serde(default)]
    pub users: Vec<UserConfig>,
    // ...existing fields...
    #[serde(default)]
    pub sandbox: SandboxConfig,
}
```

Much simpler than the container-based config — no backend selection, no base image, no cache volumes. The sandbox uses the host's installed tools and the workspace for everything.

### config_check validation

Add to `crates/coop-gateway/src/config_check.rs`:

1. **sandbox_available:** If `sandbox.enabled`, call `coop_sandbox::probe()`. If it fails, error. If it succeeds with degraded capabilities, warn with details.
2. **sandbox_memory:** Validate memory format (number with K/M/G suffix). Severity: error.
3. **sandbox_pids:** Validate > 0. Severity: error.
4. **sandbox_multiple_owners:** Warn if more than one user has `trust = "owner"`.
5. **sandbox_no_owner:** If `sandbox.enabled`, warn if no user has `trust = "owner"` and terminal default would be sandboxed.
6. **sandbox_user_namespaces:** On Linux, check `cat /proc/sys/kernel/unprivileged_userns_clone`. If 0, error — sandbox won't work without root or reconfiguring the kernel.

### CLI commands

Add to `crates/coop-gateway/src/cli.rs`:

```rust
#[derive(Subcommand)]
pub(crate) enum SandboxCommands {
    /// Show sandbox status (platform, capabilities, degraded features).
    Status,
}
```

**`coop sandbox status`:** Call `probe()`, print the platform backend, each capability and whether it's available, and any warnings about degraded mode. Example output:

```
Sandbox: linux (namespaces + landlock + seccomp)
  ✓ user namespaces
  ✓ network namespaces
  ✓ landlock (ABI v5)
  ✓ seccomp
  ✗ cgroups v2 (no write access — using setrlimit fallback)
```

No `reset` or `clean` commands needed — there's nothing to reset or clean. No containers, no images.

### Startup banner

When sandbox is enabled:

```
🐔 Coop v0.x.x
Agent: reid | Model: claude-sonnet-4
🔒 Sandbox: linux (namespaces + landlock + seccomp) — owner bypasses
```

Or on macOS:
```
🔒 Sandbox: macos (apple/container VM) — owner bypasses
```

### Example configs

**Minimal — enable sandbox:**
```toml
[sandbox]
enabled = true

[[users]]
name = "alice"
trust = "owner"
match = ["terminal:default", "signal:+15555550100"]

[[users]]
name = "bob"
trust = "full"
match = ["signal:+15555550101"]
```

Alice at the terminal or via Signal DM → `Owner` trust → unsandboxed.
Bob via Signal DM → `Full` trust → bash runs in sandbox, can only access workspace.
Unknown sender → `Public` → sandboxed (no tools anyway).

**No explicit owner — terminal defaults to owner when sandbox is on:**
```toml
[sandbox]
enabled = true

[[users]]
name = "bob"
trust = "inner"
match = ["signal:+15555550101"]
```

Terminal user → no match → defaults to `Owner` (because sandbox enabled) → unsandboxed.
Bob → `Inner` → sandboxed.

**Allow network (for dependency fetching):**
```toml
[sandbox]
enabled = true
allow_network = true
memory = "4g"
pids_limit = 1024
```

**Per-user overrides (phase 5):**
```toml
[sandbox]
enabled = true
allow_network = false

[[users]]
name = "alice"
trust = "owner"
match = ["terminal:default"]

[[users]]
name = "bob"
trust = "full"
match = ["signal:+15555550101"]
sandbox = { allow_network = true }
```

Bob gets network access in his sandbox. Global default is no network.

## Tracing

All sandbox operations must be instrumented:

- `info!` when sandbox is probed at startup (capabilities, degraded features)
- `info!` when bypassing sandbox for owner trust
- `debug!` on each sandboxed exec (command summary, policy)
- `debug!` on sandbox setup steps (namespaces created, landlock applied, seccomp applied)
- `warn!` on degraded capability (e.g., Landlock unavailable, cgroups fallback)
- `error!` on sandbox setup failure
- Span: `sandbox_exec` as a child of `tool_execute`

## Testing

### Unit tests (always run)

- `TrustLevel::Owner` ordering: `Owner < Full < Inner < Familiar < Public`.
- `resolve_trust` with `Owner`: owner in DM → owner, owner in group → familiar.
- `accessible_stores` with `Owner`: same as `Full`.
- Serde: `trust = "owner"` deserializes correctly.
- Config parsing: `SandboxConfig` with all fields, minimal, defaults.
- `SandboxPolicy` construction from config.

### Integration tests (gated behind `COOP_SANDBOX_TEST=1`)

Require Linux with unprivileged user namespaces enabled.

- **Basic exec:** `exec(policy, "echo hello", timeout)` → stdout contains "hello", exit code 0.
- **Workspace isolation:** write a file to workspace, `exec(policy, "cat file.txt", ...)` → reads it. `exec(policy, "cat /etc/passwd", ...)` → fails (path doesn't exist or access denied).
- **Network isolation:** `exec(policy, "curl http://example.com", ...)` → fails (no network).
- **Read-only host paths:** `exec(policy, "touch /usr/test", ...)` → fails (read-only).
- **Workspace write:** `exec(policy, "echo test > /work/new.txt", ...)` → succeeds, file exists on host.
- **Execute from workspace:** write a script to workspace, `exec(policy, "chmod +x script.sh && ./script.sh", ...)` → succeeds.
- **PID isolation:** `exec(policy, "ps aux", ...)` → only sees sandbox processes, not host.
- **Timeout:** `exec(policy, "sleep 999", Duration::from_secs(1))` → times out.

### Fake sandbox for gateway unit tests

For testing the `SandboxExecutor` routing without actually sandboxing:

```rust
/// Test-only: mock sandbox that executes commands unsandboxed.
/// Verifies that SandboxExecutor routes correctly.
#[cfg(test)]
pub fn exec_unsandboxed(policy: &SandboxPolicy, command: &str, timeout: Duration) -> Result<ExecOutput> {
    // Just run the command directly — tests sandbox routing, not sandbox isolation
}
```

## Implementation Plan

### Phase 1: Owner trust level

1. Add `Owner` variant to `TrustLevel` in `crates/coop-core/src/types.rs`.
2. Update `rank()`, `Ord`, tests.
3. Update `accessible_stores` in `crates/coop-gateway/src/trust.rs`.
4. Add `resolve_trust` test cases for `Owner`.
5. Update `route_message` terminal default logic.
6. Add `SandboxConfig` to `Config`.
7. `cargo test` — fix any fallthrough/exhaustive match issues across the codebase.

### Phase 2: Sandbox crate (Linux)

8. Create `crates/coop-sandbox/` with `Cargo.toml`.
9. Implement `SandboxPolicy`, `ExecOutput`, `SandboxInfo` types.
10. Implement `probe()` — check for user namespaces, Landlock, seccomp, cgroups v2.
11. Implement `linux::exec()` — fork with `pre_exec` hook that applies namespace + mount + Landlock + seccomp.
12. Implement mount setup (bind-mount workspace, read-only host paths, tmpfs, devnodes, pivot_root).
13. Implement Landlock policy builder.
14. Implement seccomp BPF filter.
15. Integration tests (gated behind `COOP_SANDBOX_TEST=1`).

### Phase 3: Gateway integration

16. Create `SandboxExecutor` in `crates/coop-gateway/src/sandbox_executor.rs`.
17. Wire into `Gateway::new()`.
18. Add config_check validations.
19. Add `coop sandbox status` CLI command.
20. Add startup banner.

### Phase 4: apple/container backend (macOS)

21. Implement `apple::exec()` and `apple::probe()` in `apple.rs` (`#[cfg(target_os = "macos")]`).
22. Test on macOS with apple/container installed.

### Phase 5: Per-user overrides

23. Add `SandboxOverrides` to `UserConfig` and `CronConfig`.
24. Implement policy resolution (global + per-user merge).
25. Thread overrides through `ToolContext`.

## Files changed

**New crate:**
- `crates/coop-sandbox/Cargo.toml`
- `crates/coop-sandbox/src/lib.rs`
- `crates/coop-sandbox/src/linux.rs`
- `crates/coop-sandbox/src/landlock.rs`
- `crates/coop-sandbox/src/seccomp.rs`
- `crates/coop-sandbox/src/apple.rs`
- `crates/coop-sandbox/src/policy.rs`
- `crates/coop-sandbox/tests/linux.rs`
- `crates/coop-sandbox/tests/policy.rs`

**Modified:**
- `Cargo.toml` (workspace) — add `coop-sandbox` to members
- `crates/coop-core/src/types.rs` — add `Owner` to `TrustLevel`, update rank/ord
- `crates/coop-gateway/Cargo.toml` — add `coop-sandbox` dependency
- `crates/coop-gateway/src/config.rs` — add `SandboxConfig`, field on `Config`
- `crates/coop-gateway/src/config_check.rs` — sandbox validation checks
- `crates/coop-gateway/src/cli.rs` — `Sandbox` subcommand
- `crates/coop-gateway/src/main.rs` — handle `Sandbox` subcommand
- `crates/coop-gateway/src/gateway.rs` — create `SandboxExecutor` when sandbox enabled
- `crates/coop-gateway/src/sandbox_executor.rs` — NEW file, `SandboxExecutor` impl
- `crates/coop-gateway/src/router.rs` — terminal default to `Owner` when sandbox enabled
- `crates/coop-gateway/src/trust.rs` — `Owner` in `accessible_stores`, new tests

**Not modified:**
- `crates/coop-core/src/tools/bash.rs` — unchanged. `SandboxExecutor` intercepts before `BashTool`.
- `crates/coop-core/src/tools/read_file.rs` — unchanged. File tools pass through.
- `crates/coop-core/src/traits.rs` — `ToolContext` unchanged (trust field is sufficient).
