# Sandbox: Isolated Tool Execution

Implement sandboxed tool execution for Coop using container-based isolation. When enabled, all tool calls (bash, read_file, write_file, edit_file) execute inside a long-lived container per session instead of directly on the host. This protects against prompt injection, malicious dependencies, and accidental damage.

Read `AGENTS.md` at the project root before starting. Follow the development loop and code quality rules.

## Background

Today, `BashTool` in `crates/coop-core/src/tools/bash.rs` runs `Command::new("sh").arg("-c").arg(command)` directly on the host. There is no filesystem scoping (the bash tool can read `~/.ssh`), no network restriction, and no process isolation beyond trust-gating.

Read the blog analysis in `docs/sandbox-analysis.md` for the full threat model and design rationale. The short version: an AI agent executing arbitrary shell commands is running untrusted code, and the primary threats are data exfiltration, credential theft, and lateral movement — not kernel exploits. Container-based isolation addresses all of these.

### Platform isolation properties

The same OCI container abstraction provides different isolation strengths depending on platform:

- **macOS** (apple/container): each container is a dedicated lightweight VM via Virtualization.framework. This is hardware-enforced VM-grade isolation.
- **Linux with gVisor**: containers run under gVisor's userspace kernel (Sentry). Syscalls are intercepted and reimplemented; only ~53-68 host syscalls are exposed.
- **Linux with Docker (runc)**: containers use Linux namespaces/cgroups. Weaker than gVisor but still prevents filesystem and network access outside the container.

Coop doesn't need to know which backend provides the isolation — it talks to the container CLI uniformly.

## Design

### Principles

1. **Opt-in.** Sandbox is disabled by default. When disabled, tools run directly on the host as today. No behavioral change for existing users.
2. **One sandbox per session.** A container is created when the first tool call in a session executes (lazy), and destroyed when the session is cleared or Coop shuts down. Tool calls within a session exec into the same running container.
3. **Workspace bind mount.** The agent workspace directory is mounted read-write at `/work` inside the container. Files the agent creates persist on the host. Session data, SOUL.md, TOOLS.md — all visible inside.
4. **Persistent tooling via image commits.** When the agent installs packages (apt, pip, cargo install, etc.), the container image is committed at session end so those tools persist across restarts. Users can reset to the base image.
5. **Backend auto-detection.** On macOS, prefer `container` (apple/container) if available. On Linux, prefer `docker` with gVisor runtime if available, falling back to default Docker runtime. If no container runtime is found, fall back to direct execution with a warning.
6. **Per-session config overrides.** Sessions can override sandbox settings (image, network, memory) for different use cases. A coding session might need network access; a data-analysis session might not.
7. **No new heavy dependencies in coop-core.** The sandbox crate is a leaf crate. It shells out to the container CLI rather than linking a Docker client library. This protects compile times.

### Architecture

```
Gateway
  │
  ├── SandboxManager (new)
  │     Owns per-session container lifecycle.
  │     Creates containers lazily on first tool call.
  │     Destroys containers on session clear/shutdown.
  │
  └── SandboxExecutor (new, implements ToolExecutor)
        Wraps tool calls as container exec commands.
        bash("cargo build") → container exec <id> sh -c "cargo build"
        read_file / write_file / edit_file → operate on bind-mounted workspace
```

### Crate structure

Create a new crate: `crates/coop-sandbox/`

```
crates/coop-sandbox/
├── Cargo.toml
├── src/
│   ├── lib.rs           # Public API: SandboxManager, SandboxExecutor, types
│   ├── backend.rs       # Backend trait + auto-detection
│   ├── docker.rs        # Docker/Podman backend
│   ├── apple.rs         # apple/container backend  
│   ├── lifecycle.rs     # Container create/start/exec/commit/destroy
│   └── config.rs        # SandboxConfig, SessionSandboxOverride
└── tests/
    └── integration.rs   # Tests with real container runtime (gated)
```

**Cargo.toml dependencies:**
- `coop-core` (for `ToolExecutor`, `Tool`, `ToolContext`, `ToolDef`, `ToolOutput` traits/types)
- `tokio` with features `["process", "sync", "time"]` (for `Command`, channels, timeouts)
- `serde`, `serde_json` (for config deserialization)
- `tracing` (for instrumentation)
- `anyhow` (for error handling)

Do NOT add `reqwest`, `bollard`, or any Docker client library. Shell out to the CLI.

### Why CLI instead of Docker socket API

The standard programmatic interface for Docker is the Engine REST API over `/var/run/docker.sock`, with typed clients like `bollard` (Rust), `docker-py` (Python), or the Docker Go SDK. The CLI (`docker run`, `docker exec`) is a wrapper around this API — it's the human interface, not the canonical programmatic one. Podman has a Docker-compatible socket API too.

We shell out to the CLI anyway for three reasons:

1. **apple/container has no Rust-accessible API.** Its architecture is Swift CLI → XPC apiserver → Swift Containerization framework. The CLI is the only practical interface from non-Swift code. If we use bollard for Docker but CLI for apple/container, we maintain two completely different code paths instead of one.

2. **Compile time budget.** Bollard depends on hyper, http, tower, pin-project — adding ~3-5s to clean builds. The AGENTS.md compile time rules are explicit: keep incremental leaf builds under 1s.

3. **Low operation frequency.** Coop creates one container per session, execs 20-50 times during a conversation, then destroys it. The ~50ms overhead of fork+exec per CLI call is invisible next to LLM round-trips that take seconds.

**CLI hardening rules** (important — CLI output parsing is fragile if done carelessly):

- Always use `--format` flags for structured output where available (e.g., `docker inspect --format '{{.Id}}'` instead of parsing JSON blobs).
- Always check exit codes first, then parse stdout. Treat any non-zero exit as an error.
- Use `--quiet` flags where available to suppress decoration (e.g., `docker run -d --quiet` returns only the container ID).
- Never parse stderr for success/failure logic — only include it in error messages.
- Set explicit timeouts on all CLI calls (`tokio::time::timeout`). Container operations can hang if the daemon is unresponsive.
- Sanitize all user-provided strings interpolated into CLI args (session IDs, image names). Use `Command::arg()` per-argument, never string interpolation into a shell command.

### Backend trait

```rust
/// A container runtime backend (Docker, apple/container, etc.)
#[async_trait]
pub trait SandboxBackend: Send + Sync + std::fmt::Debug {
    /// Human-readable name for logging.
    fn name(&self) -> &str;

    /// What isolation this backend provides.
    fn isolation_level(&self) -> IsolationLevel;

    /// Create and start a container. Returns the container ID.
    async fn create(&self, config: &ContainerConfig) -> Result<String>;

    /// Execute a command inside a running container.
    async fn exec(&self, container_id: &str, command: &[&str], workdir: &str) -> Result<ExecOutput>;

    /// Stop and remove a container.
    async fn destroy(&self, container_id: &str) -> Result<()>;

    /// Commit a container's filesystem to a new image tag.
    async fn commit(&self, container_id: &str, image_tag: &str) -> Result<()>;

    /// Check if an image exists locally.
    async fn image_exists(&self, image_tag: &str) -> Result<bool>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IsolationLevel {
    /// Dedicated VM per container (apple/container, Firecracker)
    VirtualMachine,
    /// Userspace kernel (gVisor runsc)
    UserspaceKernel,
    /// Linux namespaces/cgroups only (Docker runc)
    Namespace,
    /// No container runtime — direct execution
    None,
}
```

### Backend auto-detection

Implement in `backend.rs`:

```rust
pub fn detect_backend() -> Result<Box<dyn SandboxBackend>> { ... }
```

Detection order:

1. **macOS only:** check for `container` CLI (`which container`). If found, use `AppleBackend`. `IsolationLevel::VirtualMachine`.
2. Check for `docker` CLI (`which docker`). If found:
   a. Check if gVisor runtime is available: `docker info --format '{{.Runtimes}}'` contains `runsc`. If yes, use `DockerBackend { runtime: "runsc" }`. `IsolationLevel::UserspaceKernel`.
   b. Otherwise, use `DockerBackend { runtime: "runc" }`. `IsolationLevel::Namespace`.
3. Check for `podman` CLI. Same as Docker (podman is CLI-compatible).
4. If nothing found, return an error. The caller (gateway) falls back to direct execution.

Each check should be a single `Command::new("which").arg(...)` or equivalent, with a short timeout. Don't block startup for more than a second total.

### Docker backend

Implement in `docker.rs`. Each method shells out to the `docker` CLI:

**create:**
```
docker run -d \
    [--runtime=runsc]              # if gVisor available
    --name coop-{session_slug}     # deterministic name from session key
    --workdir /work                # default working directory
    -v {workspace}:/work           # workspace bind mount
    -v coop-cargo-cache:/root/.cargo/registry   # persistent cache volumes
    -v coop-cargo-git:/root/.cargo/git
    -v coop-pip-cache:/root/.cache/pip
    --read-only                    # immutable rootfs
    --tmpfs /tmp:size=512m         # writable tmp
    --tmpfs /root:size=256m        # writable home (for dotfiles)
    --network none                 # no network by default
    --memory {memory_limit}        
    --pids-limit 512               
    --cpus {cpu_limit}             
    {image}                        
    sleep infinity                 # keep alive
```

**exec:**
```
docker exec -w {workdir} {container_id} sh -c {command}
```

Capture stdout and stderr separately. Apply the same timeout and output-size limits as the current `BashTool`.

**destroy:**
```
docker rm -f {container_id}
```

**commit:**
```
docker commit {container_id} {image_tag}
```

**image_exists:**
```
docker image inspect {image_tag}
```

Exit code 0 = exists, non-zero = doesn't exist.

### apple/container backend

Implement in `apple.rs`. Nearly identical to Docker backend but uses the `container` CLI:

- `container run -d --name ... --memory ... -v ... {image} sleep infinity`
- `container exec -w {workdir} {container_id} sh -c {command}`
- `container rm -f {container_id}`
- `container commit {container_id} {image_tag}`

The CLI shape is intentionally similar. Key differences:
- apple/container doesn't support `--runtime` (each container is already a VM).
- `--read-only` flag may differ — check apple/container docs and adapt.
- Resource flags: `--memory 2G --cpus 2` (same syntax).
- Network: `--network none` or omit for no network (verify behavior).

Compile-gate this module with `#[cfg(target_os = "macos")]`. On Linux, this module is dead code.

### SandboxManager

Lives in `lifecycle.rs`. Owns the per-session container lifecycle:

```rust
pub struct SandboxManager {
    backend: Box<dyn SandboxBackend>,
    config: SandboxConfig,
    /// Running containers keyed by session ID.
    containers: Mutex<HashMap<String, ContainerState>>,
}

struct ContainerState {
    container_id: String,
    /// Track whether install-like commands ran (for commit decision).
    had_installs: bool,
}
```

Public API:

```rust
impl SandboxManager {
    /// Create a new manager with the detected backend and config.
    pub fn new(backend: Box<dyn SandboxBackend>, config: SandboxConfig) -> Self;

    /// Get or create a container for a session. Lazy — first call creates.
    pub async fn ensure_container(&self, session_id: &str, workspace: &Path) -> Result<String>;

    /// Execute a command in the session's container.
    pub async fn exec(
        &self,
        session_id: &str,
        command: &[&str],
        workdir: &str,
    ) -> Result<ExecOutput>;

    /// Destroy a session's container. Called on session clear or shutdown.
    pub async fn destroy_session(&self, session_id: &str) -> Result<()>;

    /// Destroy all containers. Called on shutdown.
    pub async fn destroy_all(&self) -> Result<()>;

    /// Record that an install-like command ran in this session.
    pub fn mark_install(&self, session_id: &str);

    /// The isolation level of the current backend.
    pub fn isolation_level(&self) -> IsolationLevel;
}
```

**Container naming:** use `coop-{agent_id}-{session_slug}` where `session_slug` is a sanitized, deterministic string derived from the `SessionKey`. This lets you find orphaned containers and clean them up.

**Lazy creation:** `ensure_container` checks the hashmap first. On first call for a session, it creates the container. This avoids paying startup cost for sessions that never use tools.

**Image selection:** On `ensure_container`, check if a committed agent image exists (`coop-sandbox/{agent_id}:latest`). If yes, use it. If no, use the configured `base_image`.

**Shutdown cleanup:** On `destroy_all`, iterate all containers and destroy them. Register this as part of the gateway's graceful shutdown sequence.

### SandboxExecutor

Implements `ToolExecutor` from `coop-core`. Lives in `lib.rs` or a dedicated file.

```rust
pub struct SandboxExecutor {
    manager: Arc<SandboxManager>,
    /// The inner executor for tool definitions (schemas, names).
    inner: Arc<dyn ToolExecutor>,
}
```

This wraps the existing `DefaultExecutor`. It delegates tool *definitions* to the inner executor (so tool schemas don't change), but redirects *execution* through the sandbox:

```rust
#[async_trait]
impl ToolExecutor for SandboxExecutor {
    async fn execute(
        &self,
        name: &str,
        arguments: serde_json::Value,
        ctx: &ToolContext,
    ) -> Result<ToolOutput> {
        match name {
            "bash" => self.exec_bash(arguments, ctx).await,
            "read_file" => self.exec_read_file(arguments, ctx).await,
            "write_file" => self.exec_write_file(arguments, ctx).await,
            "edit_file" => self.exec_edit_file(arguments, ctx).await,
            // Non-sandboxed tools (signal_send, config_read, etc.)
            // pass through to inner executor.
            _ => self.inner.execute(name, arguments, ctx).await,
        }
    }

    fn tools(&self) -> Vec<ToolDef> {
        self.inner.tools()
    }
}
```

**bash:** Extract the `command` argument. Call `manager.exec(session_id, &["sh", "-c", command], "/work")`. Apply the same 120s timeout and 100KB output limit as the current `BashTool`. If the command looks like an install (`apt install`, `pip install`, `cargo install`, `npm install -g`, `rustup`, `curl ... | sh`), call `manager.mark_install(session_id)`.

**read_file / write_file / edit_file:** These operate on the workspace, which is bind-mounted. Two implementation options:

- **Option A (simpler):** Execute them directly on the host via the inner executor. The workspace is a host directory and the bind mount means files are the same. Path traversal checks already prevent escaping the workspace. This is the recommended approach — it's faster and the existing implementations already enforce workspace scoping.
- **Option B:** Route through `container exec` as well (e.g., `exec cat`, `exec tee`). More consistent but slower and more complex for edit_file.

Use option A. Only `bash` needs to go through the container because it's the only tool that executes arbitrary code. read_file, write_file, and edit_file are already workspace-scoped and don't execute code.

**Trust gating:** The `SandboxExecutor` inherits the trust checks from the inner tools. No changes needed — `BashTool::execute` already checks trust level before running.

### Image commit on session end

When a session is cleared or Coop shuts down, if `auto_commit` is enabled and the session's container had install-like commands:

1. Commit the container: `backend.commit(container_id, "coop-sandbox/{agent_id}:latest")`
2. Then destroy the container.

The install detection heuristic is a simple string match on bash commands:

```rust
fn looks_like_install(cmd: &str) -> bool {
    let patterns = [
        "apt install", "apt-get install",
        "pip install", "pip3 install",
        "cargo install",
        "npm install -g", "yarn global add",
        "rustup", "nvm install", "pyenv install",
        "brew install",
        "curl", "wget",  // broad but catches installer scripts
    ];
    let lower = cmd.to_ascii_lowercase();
    patterns.iter().any(|p| lower.contains(p))
}
```

This is intentionally broad. False positives just cause a (fast) no-op commit. False negatives mean the user reinstalls on next session.

### Config

Add to `crates/coop-gateway/src/config.rs`:

```rust
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub(crate) struct SandboxConfig {
    /// Enable sandboxed tool execution. Default: false.
    #[serde(default)]
    pub enabled: bool,

    /// Container runtime backend. "auto" | "docker" | "apple" | "podman".
    /// Default: "auto" (detect best available).
    #[serde(default = "default_sandbox_backend")]
    pub backend: String,

    /// Base image for new sandboxes (used when no committed image exists).
    /// Default: "ubuntu:24.04".
    #[serde(default = "default_sandbox_image")]
    pub base_image: String,

    /// Memory limit per sandbox. Default: "2g".
    #[serde(default = "default_sandbox_memory")]
    pub memory: String,

    /// CPU limit per sandbox. Default: 2.
    #[serde(default = "default_sandbox_cpus")]
    pub cpus: u32,

    /// Network mode. "none" | "host" | "bridge". Default: "none".
    #[serde(default = "default_sandbox_network")]
    pub network: String,

    /// Commit container image after sessions that install packages.
    /// Default: true.
    #[serde(default = "default_sandbox_auto_commit")]
    pub auto_commit: bool,

    /// Persistent named volumes for build caches.
    /// Default: ["cargo", "pip", "npm"]
    #[serde(default = "default_sandbox_cache_volumes")]
    pub cache_volumes: Vec<String>,
}
```

Add `SandboxConfig` as an optional field on the top-level `Config`:

```rust
pub(crate) struct Config {
    pub agent: AgentConfig,
    // ...existing fields...
    #[serde(default)]
    pub sandbox: SandboxConfig,
}
```

Default values:

```rust
fn default_sandbox_backend() -> String { "auto".to_owned() }
fn default_sandbox_image() -> String { "ubuntu:24.04".to_owned() }
fn default_sandbox_memory() -> String { "2g".to_owned() }
fn default_sandbox_cpus() -> u32 { 2 }
fn default_sandbox_network() -> String { "none".to_owned() }
fn default_sandbox_auto_commit() -> bool { true }
fn default_sandbox_cache_volumes() -> Vec<String> {
    vec!["cargo".to_owned(), "pip".to_owned(), "npm".to_owned()]
}
```

### Per-session sandbox overrides

Sessions may need different sandbox settings. A coding session might need network access to pull dependencies; a data-analysis session might need more memory.

Add an optional `sandbox` section to `CronConfig` and allow session-level overrides via a new slash command or config mechanism.

For cron jobs, add to `CronConfig`:

```rust
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct CronConfig {
    // ...existing fields...

    /// Per-cron sandbox overrides (merged on top of global sandbox config).
    #[serde(default)]
    pub sandbox: Option<SessionSandboxOverride>,
}
```

For user sessions via config:

```rust
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct UserConfig {
    pub name: String,
    pub trust: TrustLevel,
    #[serde(default)]
    pub r#match: Vec<String>,

    /// Per-user sandbox overrides.
    #[serde(default)]
    pub sandbox: Option<SessionSandboxOverride>,
}
```

The override struct:

```rust
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub(crate) struct SessionSandboxOverride {
    /// Override the container image for this session.
    #[serde(default)]
    pub image: Option<String>,

    /// Override the network mode for this session.
    #[serde(default)]
    pub network: Option<String>,

    /// Override the memory limit for this session.
    #[serde(default)]
    pub memory: Option<String>,

    /// Override the CPU limit for this session.
    #[serde(default)]
    pub cpus: Option<u32>,
}
```

Resolution: start from global `SandboxConfig`, overlay the session-specific `SessionSandboxOverride` fields (non-None values win). Implement a `resolve` method:

```rust
impl SandboxConfig {
    pub fn resolve(&self, overrides: Option<&SessionSandboxOverride>) -> ResolvedSandboxConfig {
        let o = overrides.cloned().unwrap_or_default();
        ResolvedSandboxConfig {
            image: o.image.unwrap_or_else(|| self.base_image.clone()),
            memory: o.memory.unwrap_or_else(|| self.memory.clone()),
            cpus: o.cpus.unwrap_or(self.cpus),
            network: o.network.unwrap_or_else(|| self.network.clone()),
            cache_volumes: self.cache_volumes.clone(),
            auto_commit: self.auto_commit,
        }
    }
}
```

### Example configs

**Minimal — enable sandbox with defaults:**
```toml
[sandbox]
enabled = true
```

**Custom image and network:**
```toml
[sandbox]
enabled = true
base_image = "ghcr.io/myorg/coop-dev:latest"
network = "bridge"
memory = "4g"
cpus = 4
```

**Per-user override — Alice gets network, Bob doesn't:**
```toml
[sandbox]
enabled = true
network = "none"

[[users]]
name = "alice"
trust = "full"
match = ["terminal:default", "signal:alice-uuid"]
sandbox = { network = "bridge" }

[[users]]
name = "bob"
trust = "inner"
match = ["signal:bob-uuid"]
# No sandbox override — inherits network = "none"
```

**Cron with custom image:**
```toml
[[cron]]
name = "deploy"
cron = "0 2 * * *"
user = "alice"
message = "run deploy pipeline"
sandbox = { image = "myorg/deploy-tools:latest", network = "bridge" }
```

**Paranoid mode — never persist container state:**
```toml
[sandbox]
enabled = true
auto_commit = false
base_image = "ghcr.io/myorg/locked-down:latest"
network = "none"
```

### CLI commands

Add a `Sandbox` subcommand group to `cli.rs`:

```rust
#[derive(Subcommand)]
pub(crate) enum Commands {
    // ...existing commands...

    /// Manage the sandbox environment.
    Sandbox {
        #[command(subcommand)]
        command: SandboxCommands,
    },
}

#[derive(Subcommand)]
pub(crate) enum SandboxCommands {
    /// Show sandbox status (backend, isolation level, running containers).
    Status,
    /// Reset the agent's sandbox image to the base image.
    Reset,
    /// Remove all sandbox containers and volumes.
    Clean,
}
```

**`coop sandbox status`:** Print the detected backend, isolation level, whether a committed image exists, and any running containers.

**`coop sandbox reset`:** Remove the committed agent image (`coop-sandbox/{agent_id}:latest`), so the next session starts fresh from the base image.

**`coop sandbox clean`:** Destroy all `coop-*` containers and optionally remove cache volumes.

### config_check validation

Add sandbox validation to `config_check.rs`:

1. **sandbox_backend:** If `sandbox.enabled`, check that the configured (or auto-detected) backend CLI is installed. Severity: error.
2. **sandbox_image:** If `sandbox.enabled`, check that the base image can be pulled or exists locally. Severity: warning (image will be pulled on first use).
3. **sandbox_network:** Validate network mode is one of `none`, `host`, `bridge`. Severity: error.
4. **sandbox_memory:** Validate memory format (number with optional K/M/G suffix). Severity: error.
5. **sandbox_cpus:** Validate cpus > 0. Severity: error.
6. **sandbox_overrides:** Validate session override fields use the same formats. Severity: error.

### Gateway integration

In `crates/coop-gateway/src/gateway.rs`, modify `Gateway::new()`:

```rust
// In Gateway::new(), after building the executor:
let executor: Arc<dyn ToolExecutor> = if config.load().sandbox.enabled {
    match coop_sandbox::detect_and_create_manager(&config.load().sandbox) {
        Ok(manager) => {
            let manager = Arc::new(manager);
            info!(
                backend = manager.backend_name(),
                isolation = ?manager.isolation_level(),
                "sandbox enabled"
            );
            Arc::new(coop_sandbox::SandboxExecutor::new(manager, executor))
        }
        Err(e) => {
            warn!(error = %e, "sandbox enabled but backend detection failed, running unsandboxed");
            executor
        }
    }
} else {
    executor
};
```

The `SandboxExecutor` wraps the existing executor. The gateway doesn't need to know about containers — it still calls `self.executor.execute(name, args, ctx)` as before.

**Session cleanup:** When the gateway clears a session (`clear_session`), also call `sandbox_manager.destroy_session(session_id)`. When the gateway shuts down, call `sandbox_manager.destroy_all()`.

**Passing overrides:** The `ToolContext` needs to carry the resolved sandbox config for the session. Add an optional field:

```rust
pub struct ToolContext {
    pub session_id: String,
    pub trust: TrustLevel,
    pub workspace: PathBuf,
    pub user_name: Option<String>,
    /// Sandbox overrides for this session, if sandbox is enabled.
    pub sandbox_overrides: Option<SessionSandboxOverride>,
}
```

The gateway's `tool_context()` method resolves overrides from the user config or cron config and attaches them.

### Tracing

All sandbox operations must be instrumented:

- `info!` on container create (backend, image, session, isolation level)
- `info!` on container destroy
- `debug!` on each exec (command, container_id)
- `debug!` on commit (image tag, session)
- `warn!` on backend detection failure
- `error!` on container create/exec/destroy failures
- Span: `sandbox_exec` as a child of `tool_execute`

### Startup message

When sandbox is enabled, show the isolation level in the TUI/log startup banner:

```
🐔 Coop v0.x.x
Agent: reid | Model: claude-sonnet-4
🔒 Sandbox: apple/container (VM isolation)
```

Or on Linux:
```
🔒 Sandbox: docker + gVisor (userspace kernel)
```

Or fallback:
```
🔒 Sandbox: docker (namespace isolation)
```

### Future: socket API migration

If CLI parsing becomes a maintenance burden, the Docker backend can be migrated to use the Docker Engine REST API over `/var/run/docker.sock` directly — it's a simple HTTP/JSON protocol. A minimal implementation needs only `hyper` with Unix socket support (or raw `tokio::net::UnixStream` + hand-rolled HTTP), avoiding the full bollard dependency. The `SandboxBackend` trait doesn't change — only the Docker backend internals. apple/container would remain CLI-based regardless.

## Implementation plan

### Phase 1: Core sandbox infrastructure

1. Create `crates/coop-sandbox/` with `Cargo.toml`.
2. Implement `SandboxBackend` trait and `IsolationLevel` enum in `backend.rs`.
3. Implement `DockerBackend` in `docker.rs` (create, exec, destroy, commit, image_exists).
4. Implement backend auto-detection in `backend.rs`.
5. Implement `SandboxManager` in `lifecycle.rs` (lazy container creation, session tracking, destroy).
6. Implement `SandboxExecutor` in `lib.rs` (wraps `ToolExecutor`, routes bash through container exec).
7. Add `SandboxConfig` to gateway config with defaults.
8. Wire up in `Gateway::new()` — create `SandboxExecutor` when enabled.

### Phase 2: Session lifecycle integration

9. Hook `destroy_session` into gateway's `clear_session` and shutdown path.
10. Implement image commit logic (install detection heuristic, commit on session end).
11. Add `SessionSandboxOverride` to `UserConfig` and `CronConfig`.
12. Implement config resolution (global + per-session overrides).
13. Thread `sandbox_overrides` through `ToolContext`.

### Phase 3: apple/container backend

14. Implement `AppleBackend` in `apple.rs` (compile-gated to macOS).
15. Add to auto-detection order.
16. Test on macOS with apple/container installed.

### Phase 4: CLI and validation

17. Add `coop sandbox status/reset/clean` commands.
18. Add sandbox checks to `config_check.rs`.
19. Add startup banner showing sandbox status.

### Phase 5: Testing

20. Unit tests for config parsing, override resolution, install detection heuristic.
21. Integration tests (gated behind a feature flag or env var) that create/exec/destroy a real container.
22. Test graceful fallback when no container runtime is available.

## Testing

### Unit tests (always run)

- Config parsing: sandbox section with all fields, minimal, defaults.
- Override resolution: global only, global + user override, global + cron override, all fields overridden.
- Install detection: positive cases (apt install, pip install, cargo install), negative cases (cargo build, pip freeze), edge cases (mixed case, multi-line).
- Container name generation from session keys.

### Integration tests (gated)

Gate behind `COOP_SANDBOX_TEST=1` env var. These require a container runtime on the test machine.

- Create container, exec `echo hello`, verify output, destroy.
- Create container with workspace mount, write a file from host, read it from container, verify.
- Create container, exec `apt-get update` (needs network=bridge for this test), commit, verify image exists, destroy, create new container from committed image, verify apt cache is warm.
- Verify container is destroyed on cleanup.
- Verify `--network none` actually blocks network (exec `curl` should fail).

### Fake backend for tests

Add a `FakeSandboxBackend` for unit testing code that uses the sandbox without a real container runtime:

```rust
#[derive(Debug)]
pub struct FakeSandboxBackend {
    pub exec_responses: Mutex<Vec<ExecOutput>>,
    pub created: Mutex<Vec<String>>,
    pub destroyed: Mutex<Vec<String>>,
}
```

## Files changed

**New crate:**
- `crates/coop-sandbox/Cargo.toml`
- `crates/coop-sandbox/src/lib.rs`
- `crates/coop-sandbox/src/backend.rs`
- `crates/coop-sandbox/src/docker.rs`
- `crates/coop-sandbox/src/apple.rs`
- `crates/coop-sandbox/src/lifecycle.rs`
- `crates/coop-sandbox/src/config.rs`
- `crates/coop-sandbox/tests/integration.rs`

**Modified:**
- `Cargo.toml` — add `coop-sandbox` to workspace members
- `crates/coop-gateway/Cargo.toml` — add `coop-sandbox` dependency
- `crates/coop-gateway/src/config.rs` — add `SandboxConfig`, `SessionSandboxOverride`, fields on `Config`, `UserConfig`, `CronConfig`
- `crates/coop-gateway/src/config_check.rs` — add sandbox validation checks
- `crates/coop-gateway/src/cli.rs` — add `Sandbox` subcommand
- `crates/coop-gateway/src/main.rs` — handle `Sandbox` subcommand
- `crates/coop-gateway/src/gateway.rs` — create `SandboxExecutor` when enabled, hook destroy into session clear and shutdown
- `crates/coop-core/src/traits.rs` — add `sandbox_overrides: Option<SessionSandboxOverride>` to `ToolContext` (or keep it opaque with `serde_json::Value`)

**Not modified:**
- `crates/coop-core/src/tools/bash.rs` — unchanged. The `SandboxExecutor` intercepts bash calls before they reach `BashTool`.
- `crates/coop-core/src/tools/read_file.rs` — unchanged. File tools pass through to the inner executor and operate on the bind-mounted workspace directly.
