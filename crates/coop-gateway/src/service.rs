use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::IsTerminal;
#[cfg(unix)]
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Local, Utc};
use coop_ipc::{ClientMessage, IpcClient, PROTOCOL_VERSION, ServerMessage, socket_path};
use serde_json::Value;
use tokio::fs::File;
use tokio::io::{AsyncReadExt, AsyncSeekExt};
use tokio::time::sleep;
use tracing::{Instrument, debug, error, info, info_span, warn};

use crate::cli::GatewayCommands;
use crate::config::Config;
use crate::config_check;
use crate::config_write;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Platform {
    Systemd,
    Launchd,
    Unsupported,
}

impl Platform {
    fn as_str(self) -> &'static str {
        match self {
            Self::Systemd => "systemd",
            Self::Launchd => "launchd",
            Self::Unsupported => "unsupported",
        }
    }
}

impl std::fmt::Display for Platform {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

pub(crate) fn detect_platform() -> Platform {
    if cfg!(target_os = "macos") {
        Platform::Launchd
    } else if cfg!(target_os = "linux") && Path::new("/run/systemd/system").exists() {
        Platform::Systemd
    } else {
        Platform::Unsupported
    }
}

#[derive(Debug, Clone)]
pub(crate) struct ServicePaths {
    pub binary: PathBuf,
    pub config: PathBuf,
    pub unit_file: PathBuf,
    pub env_file: PathBuf,
    pub launchd_wrapper: PathBuf,
    pub trace_file: PathBuf,
    pub stdout_log: PathBuf,
    pub stderr_log: PathBuf,
}

#[derive(Debug, Clone)]
struct GatewayContext {
    agent_id: String,
    safe_agent_id: String,
    service_name: String,
    platform: Platform,
    socket: PathBuf,
    paths: ServicePaths,
}

#[derive(Debug, Clone)]
struct InstallOptions {
    envs: Vec<String>,
    trace_file: Option<String>,
    trace_max_size: Option<u64>,
    rust_log: Option<String>,
    no_start: bool,
    print: bool,
    print_secrets: bool,
}

#[derive(Debug)]
struct CmdResult {
    stdout: String,
    stderr: String,
    success: bool,
    status_code: Option<i32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FileIdentity {
    len: u64,
    #[cfg(unix)]
    inode: u64,
}

impl GatewayContext {
    fn new(config_path: &Path, config: &Config) -> Result<Self> {
        let platform = detect_platform();
        let safe_agent_id = sanitize_agent_id(&config.agent.id);
        let service_name = service_name_for(platform, &safe_agent_id);
        let socket = socket_path(&config.agent.id);
        let paths = resolve_service_paths(config_path, &safe_agent_id, &service_name, platform)?;

        Ok(Self {
            agent_id: config.agent.id.clone(),
            safe_agent_id,
            service_name,
            platform,
            socket,
            paths,
        })
    }

    fn installed(&self) -> bool {
        match self.platform {
            Platform::Systemd | Platform::Launchd => self.paths.unit_file.exists(),
            Platform::Unsupported => false,
        }
    }
}

#[allow(clippy::too_many_lines)]
pub(crate) async fn cmd_gateway(config_path: Option<&str>, command: GatewayCommands) -> Result<()> {
    let config_file = Config::find_config_path(config_path);
    let config = Config::load(&config_file)
        .with_context(|| format!("loading config from {}", config_file.display()))?;
    let ctx = GatewayContext::new(&config_file, &config)?;

    match command {
        GatewayCommands::Install {
            envs,
            trace_file,
            trace_max_size,
            rust_log,
            no_start,
            print,
            print_secrets,
        } => {
            let options = InstallOptions {
                envs,
                trace_file,
                trace_max_size,
                rust_log,
                no_start,
                print,
                print_secrets,
            };
            let span = info_span!(
                "gateway_install",
                agent_id = %ctx.agent_id,
                platform = %ctx.platform,
                service_name = %ctx.service_name,
                config_path = %ctx.paths.config.display(),
                no_start = options.no_start,
                print = options.print,
            );
            async { gateway_install(&ctx, &config, &options).await }
                .instrument(span)
                .await
        }
        GatewayCommands::Uninstall => {
            let span = info_span!(
                "gateway_uninstall",
                agent_id = %ctx.agent_id,
                platform = %ctx.platform,
                service_name = %ctx.service_name,
                config_path = %ctx.paths.config.display(),
            );
            async { gateway_uninstall(&ctx) }.instrument(span).await
        }
        GatewayCommands::Start => {
            let span = info_span!(
                "gateway_start",
                agent_id = %ctx.agent_id,
                platform = %ctx.platform,
                service_name = %ctx.service_name,
                config_path = %ctx.paths.config.display(),
            );
            async { gateway_start(&ctx, &config).await }
                .instrument(span)
                .await
        }
        GatewayCommands::Stop => {
            let span = info_span!(
                "gateway_stop",
                agent_id = %ctx.agent_id,
                platform = %ctx.platform,
                service_name = %ctx.service_name,
                config_path = %ctx.paths.config.display(),
            );
            async { gateway_stop(&ctx).await }.instrument(span).await
        }
        GatewayCommands::Restart => {
            let span = info_span!(
                "gateway_restart",
                agent_id = %ctx.agent_id,
                platform = %ctx.platform,
                service_name = %ctx.service_name,
                config_path = %ctx.paths.config.display(),
            );
            async { gateway_restart(&ctx, &config).await }
                .instrument(span)
                .await
        }
        GatewayCommands::Rollback {
            backup,
            no_restart,
            wait_seconds,
        } => {
            let span = info_span!(
                "gateway_rollback",
                agent_id = %ctx.agent_id,
                platform = %ctx.platform,
                service_name = %ctx.service_name,
                config_path = %ctx.paths.config.display(),
                backup_path = backup.as_deref().unwrap_or_default(),
                no_restart,
                wait_seconds,
            );
            async { gateway_rollback(&ctx, backup.as_deref(), no_restart, wait_seconds).await }
                .instrument(span)
                .await
        }
        GatewayCommands::Status => {
            let span = info_span!(
                "gateway_status",
                agent_id = %ctx.agent_id,
                platform = %ctx.platform,
                service_name = %ctx.service_name,
                config_path = %ctx.paths.config.display(),
            );
            let healthy = async { gateway_status(&ctx).await }
                .instrument(span)
                .await?;
            if healthy {
                Ok(())
            } else {
                std::process::exit(1);
            }
        }
        GatewayCommands::Logs { lines, follow } => {
            let span = info_span!(
                "gateway_logs",
                agent_id = %ctx.agent_id,
                platform = %ctx.platform,
                service_name = %ctx.service_name,
                config_path = %ctx.paths.config.display(),
                lines,
                follow,
            );
            async { gateway_logs(&ctx, lines, follow).await }
                .instrument(span)
                .await
        }
    }
}

pub(crate) fn pid_file_path(agent_id: &str) -> PathBuf {
    socket_path(agent_id).with_extension("pid")
}

pub(crate) fn write_pid_file(agent_id: &str) -> Result<PathBuf> {
    let path = pid_file_path(agent_id);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("creating pid directory {}", parent.display()))?;
    }

    if let Some(existing_pid) = read_pid_file(agent_id) {
        if is_pid_alive(existing_pid) {
            bail!("gateway already running with pid {existing_pid}");
        }
        warn!(pid = existing_pid, path = %path.display(), "removing stale pid file");
        let _ = fs::remove_file(&path);
    }

    fs::write(&path, format!("{}\n", std::process::id()))
        .with_context(|| format!("writing pid file {}", path.display()))?;

    #[cfg(unix)]
    {
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600))
            .with_context(|| format!("setting pid file permissions for {}", path.display()))?;
    }

    Ok(path)
}

pub(crate) fn read_pid_file(agent_id: &str) -> Option<u32> {
    let path = pid_file_path(agent_id);
    let content = fs::read_to_string(path).ok()?;
    content.trim().parse::<u32>().ok()
}

pub(crate) fn remove_pid_file(agent_id: &str) {
    let path = pid_file_path(agent_id);
    let _ = fs::remove_file(path);
}

pub(crate) fn is_pid_alive(pid: u32) -> bool {
    if pid == 0 || pid > i32::MAX as u32 {
        return false;
    }

    Command::new("kill")
        .args(["-0", &pid.to_string()])
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

#[allow(clippy::too_many_lines)]
async fn gateway_install(
    ctx: &GatewayContext,
    config: &Config,
    options: &InstallOptions,
) -> Result<()> {
    let config_dir = ctx
        .paths
        .config
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .to_path_buf();
    let report = config_check::validate_config(&ctx.paths.config, &config_dir);
    if report.has_errors() || report.has_warnings() {
        report.print_human();
    }
    if report.has_errors() {
        bail!("config validation failed");
    }

    let env_vars = resolve_effective_env(config, &ctx.paths, options)?;

    if options.print {
        print_generated_configuration(ctx, &env_vars, options.print_secrets);
        return Ok(());
    }

    if matches!(ctx.platform, Platform::Unsupported) {
        bail!(
            "error: no supported service manager detected\n  hint: run `coop gateway install --print` and create a manual service"
        );
    }

    ensure_parent(&ctx.paths.env_file)?;
    ensure_parent(&ctx.paths.stdout_log)?;

    write_file_with_mode(&ctx.paths.env_file, &render_env_file(&env_vars), 0o600)?;

    match ctx.platform {
        Platform::Systemd => {
            ensure_parent(&ctx.paths.unit_file)?;
            let unit = render_systemd_unit(&ctx.agent_id, &ctx.paths);
            fs::write(&ctx.paths.unit_file, unit)
                .with_context(|| format!("writing {}", ctx.paths.unit_file.display()))?;

            run_cmd("systemctl", &["--user", "daemon-reload"])?;
            run_cmd("systemctl", &["--user", "enable", &ctx.service_name])?;

            if !options.no_start {
                run_cmd("systemctl", &["--user", "start", &ctx.service_name])?;
            }

            let user = std::env::var("USER").unwrap_or_default();
            if user.is_empty() {
                warn!("could not determine USER for loginctl enable-linger");
            } else {
                let linger = run_cmd_allow_failure("loginctl", &["enable-linger", &user])?;
                if linger.success {
                    println!("  lingering enabled (gateway will start at boot)");
                } else {
                    println!(
                        "⚠ Could not enable lingering (requires sudo or polkit).\n  Without it, the gateway will stop when you log out.\n  Run manually: sudo loginctl enable-linger $USER"
                    );
                }
            }
        }
        Platform::Launchd => {
            ensure_parent(&ctx.paths.unit_file)?;
            write_file_with_mode(
                &ctx.paths.launchd_wrapper,
                &render_launchd_wrapper(&ctx.paths),
                0o700,
            )?;

            let plist = render_launchd_plist(&ctx.service_name, &ctx.paths);
            fs::write(&ctx.paths.unit_file, plist)
                .with_context(|| format!("writing {}", ctx.paths.unit_file.display()))?;

            let uid = run_cmd("id", &["-u"])?;
            let domain = format!("gui/{uid}/{}", ctx.service_name);

            run_cmd_ignore_failure("launchctl", &["bootout", &domain])?;
            launchd_wait_for_unload(&domain).await;

            if !options.no_start {
                launchd_bootstrap_with_retry(&format!("gui/{uid}"), &display(&ctx.paths.unit_file))
                    .await?;
                run_cmd("launchctl", &["kickstart", "-k", &domain])?;
            }

            let auto_login = run_cmd_allow_failure(
                "defaults",
                &[
                    "read",
                    "/Library/Preferences/com.apple.loginwindow",
                    "autoLoginUser",
                ],
            )?;
            if !auto_login.success || auto_login.stdout.trim().is_empty() {
                println!(
                    "note: macOS auto-login is not enabled. After a restart, the gateway\n      will start when you log in (not at boot).\n      To start at boot, enable auto-login in System Settings → Users & Groups."
                );
            }
        }
        Platform::Unsupported => unreachable!(),
    }

    println!("🐔 coop gateway installed");
    println!(
        "  service:  {} ({})",
        ctx.service_name,
        service_kind_label(ctx.platform)
    );
    println!("  config:   {}", ctx.paths.config.display());
    println!(
        "  traces:   {}",
        trace_path_from_env(&ctx.paths, &env_vars).display()
    );
    println!("  start:    coop gateway start");
    println!("  secrets:  {} (mode 0600)", ctx.paths.env_file.display());
    println!("  note:     protect backups of this env file");

    Ok(())
}

fn gateway_uninstall(ctx: &GatewayContext) -> Result<()> {
    match ctx.platform {
        Platform::Systemd => {
            if !ctx.paths.unit_file.exists() {
                bail!("error: gateway service is not installed");
            }

            run_cmd_ignore_failure("systemctl", &["--user", "stop", &ctx.service_name])?;
            run_cmd_ignore_failure("systemctl", &["--user", "disable", &ctx.service_name])?;

            remove_file_if_exists(&ctx.paths.unit_file)?;
            remove_file_if_exists(&ctx.paths.env_file)?;

            run_cmd("systemctl", &["--user", "daemon-reload"])?;
        }
        Platform::Launchd => {
            if !ctx.paths.unit_file.exists() {
                bail!("error: gateway service is not installed");
            }

            let uid = run_cmd("id", &["-u"])?;
            let domain = format!("gui/{uid}/{}", ctx.service_name);
            run_cmd_ignore_failure("launchctl", &["bootout", &domain])?;

            remove_file_if_exists(&ctx.paths.unit_file)?;
            remove_file_if_exists(&ctx.paths.launchd_wrapper)?;
            remove_file_if_exists(&ctx.paths.env_file)?;
        }
        Platform::Unsupported => {
            bail!("error: no service manager is installed on this platform");
        }
    }

    println!("🐔 coop gateway uninstalled");
    println!("  service:  {}", ctx.service_name);
    Ok(())
}

async fn gateway_start(ctx: &GatewayContext, config: &Config) -> Result<()> {
    match ctx.platform {
        Platform::Systemd if ctx.installed() => {
            run_cmd("systemctl", &["--user", "start", &ctx.service_name])?;
        }
        Platform::Launchd if ctx.installed() => {
            let uid = run_cmd("id", &["-u"])?;
            let domain = format!("gui/{uid}/{}", ctx.service_name);
            // bootstrap may fail if the service is already loaded (errno 5 /
            // "Input/output error" on newer macOS, or "already loaded" on
            // older versions). Ignore — kickstart -k works regardless.
            run_cmd_ignore_failure(
                "launchctl",
                &[
                    "bootstrap",
                    &format!("gui/{uid}"),
                    &display(&ctx.paths.unit_file),
                ],
            )?;
            run_cmd("launchctl", &["kickstart", "-k", &domain])?;
        }
        _ => {
            start_fallback(ctx, config).await?;
        }
    }

    println!("🐔 coop gateway started");
    println!("  service:  {}", ctx.service_name);
    println!("  socket:   {}", ctx.socket.display());
    Ok(())
}

async fn gateway_stop(ctx: &GatewayContext) -> Result<()> {
    match ctx.platform {
        Platform::Systemd if ctx.installed() => {
            run_cmd("systemctl", &["--user", "stop", &ctx.service_name])?;
        }
        Platform::Launchd if ctx.installed() => {
            let uid = run_cmd("id", &["-u"])?;
            let domain = format!("gui/{uid}/{}", ctx.service_name);
            run_cmd("launchctl", &["bootout", &domain])?;
        }
        _ => {
            stop_fallback(ctx).await?;
        }
    }

    println!("🐔 coop gateway stopped");
    println!("  service:  {}", ctx.service_name);
    Ok(())
}

async fn gateway_restart(ctx: &GatewayContext, config: &Config) -> Result<()> {
    match ctx.platform {
        Platform::Systemd if ctx.installed() => {
            run_cmd("systemctl", &["--user", "restart", &ctx.service_name])?;
        }
        Platform::Launchd if ctx.installed() => {
            let uid = run_cmd("id", &["-u"])?;
            let domain = format!("gui/{uid}/{}", ctx.service_name);
            run_cmd_ignore_failure("launchctl", &["bootout", &domain])?;
            launchd_wait_for_unload(&domain).await;
            launchd_bootstrap_with_retry(&format!("gui/{uid}"), &display(&ctx.paths.unit_file))
                .await?;
            run_cmd("launchctl", &["kickstart", "-k", &domain])?;
        }
        _ => {
            if let Err(error) = stop_fallback(ctx).await {
                warn!(error = %error, "fallback restart stop phase failed, continuing with start");
            }
            start_fallback(ctx, config).await?;
        }
    }

    println!("🐔 coop gateway restarted");
    println!("  service:  {}", ctx.service_name);
    Ok(())
}

async fn gateway_rollback(
    ctx: &GatewayContext,
    backup: Option<&str>,
    no_restart: bool,
    wait_seconds: u64,
) -> Result<()> {
    let backup_path = resolve_backup_path(&ctx.paths.config, backup);
    if !backup_path.exists() {
        bail!("backup config not found: {}", backup_path.display());
    }

    let config_dir = ctx.paths.config.parent().unwrap_or_else(|| Path::new("."));
    let report = config_check::validate_config(&backup_path, config_dir);
    if report.has_errors() {
        report.print_human();
        bail!("backup config failed validation");
    }

    let current_content = fs::read_to_string(&ctx.paths.config)
        .with_context(|| format!("reading current config {}", ctx.paths.config.display()))?;
    let failed_snapshot = failed_config_snapshot_path(&ctx.paths.config, Utc::now());
    fs::write(&failed_snapshot, current_content)
        .with_context(|| format!("writing failed snapshot {}", failed_snapshot.display()))?;

    let backup_content = fs::read_to_string(&backup_path)
        .with_context(|| format!("reading backup config {}", backup_path.display()))?;
    config_write::atomic_write(&ctx.paths.config, &backup_content)
        .with_context(|| format!("restoring config from {}", backup_path.display()))?;

    if !no_restart {
        let restored = Config::load(&ctx.paths.config)
            .with_context(|| format!("loading restored config {}", ctx.paths.config.display()))?;
        gateway_restart(ctx, &restored).await?;

        let healthy = wait_for_socket_health(&ctx.socket, Duration::from_secs(wait_seconds)).await;
        if !healthy {
            bail!(
                "rollback restored config but gateway health-check failed\n  restored backup: {}\n  failed config snapshot: {}",
                backup_path.display(),
                failed_snapshot.display()
            );
        }
    }

    println!("🐔 coop gateway rollback complete");
    println!("  restored: {}", backup_path.display());
    println!("  failed:   {}", failed_snapshot.display());
    Ok(())
}

async fn gateway_status(ctx: &GatewayContext) -> Result<bool> {
    let pid = read_pid_file(&ctx.agent_id);
    let pid_alive = pid.is_some_and(is_pid_alive);
    let socket_exists = ctx.socket.exists();
    let socket_healthy = if socket_exists {
        probe_socket_health(&ctx.socket).await
    } else {
        false
    };

    let running = pid_alive && socket_healthy;

    println!("🐔 coop gateway — agent \"{}\"", ctx.agent_id);
    if running {
        let pid_display = pid.map_or_else(|| "unknown".to_owned(), |value| value.to_string());
        println!("  status:   running (pid {pid_display})");
    } else {
        println!("  status:   stopped");
    }

    print_service_status(ctx)?;

    if running
        && let Some(pid_value) = pid
        && let Some(uptime) = pid_uptime(pid_value)
    {
        println!("  uptime:   {uptime}");
    }

    if socket_exists {
        let state = if socket_healthy {
            "healthy"
        } else if pid_alive {
            "unhealthy"
        } else {
            "stale"
        };
        println!("  socket:   {} ({state})", ctx.socket.display());
    } else {
        println!("  socket:   {}", ctx.socket.display());
    }

    println!("  config:   {}", ctx.paths.config.display());
    println!("  traces:   {}", trace_path_for_status(ctx).display());

    if !ctx.installed() {
        println!("  hint:     run `coop gateway install` for auto-start");
    }

    Ok(running)
}

async fn gateway_logs(ctx: &GatewayContext, lines: usize, follow: bool) -> Result<()> {
    let trace_path = trace_path_for_status(ctx);
    if trace_path.exists() {
        tail_trace_file(&trace_path, lines, follow).await?;
        return Ok(());
    }

    if matches!(ctx.platform, Platform::Systemd) && ctx.installed() {
        let mut args: Vec<String> = vec![
            "--user".to_owned(),
            "-u".to_owned(),
            ctx.service_name.clone(),
            "-n".to_owned(),
            lines.to_string(),
        ];
        if follow {
            args.push("-f".to_owned());
        }
        let borrowed: Vec<&str> = std::iter::once("journalctl")
            .chain(args.iter().map(String::as_str))
            .collect();
        let status = Command::new(borrowed[0])
            .args(&borrowed[1..])
            .status()
            .context("failed to execute journalctl")?;
        if !status.success() {
            bail!("journalctl failed with status {status}");
        }
        return Ok(());
    }

    if ctx.paths.stdout_log.exists() {
        tail_plain_file(&ctx.paths.stdout_log, lines, follow).await?;
        return Ok(());
    }

    bail!(
        "no logs available\n  looked for trace file: {}\n  looked for stdout log: {}",
        trace_path.display(),
        ctx.paths.stdout_log.display()
    )
}

async fn start_fallback(ctx: &GatewayContext, config: &Config) -> Result<()> {
    let env_vars = if ctx.paths.env_file.exists() {
        read_env_file(&ctx.paths.env_file)?
    } else {
        resolve_runtime_env(config, &ctx.paths)?
    };

    let pid = spawn_background(&ctx.paths.binary, &ctx.paths.config, &env_vars)?;
    info!(pid, socket = %ctx.socket.display(), "spawned fallback gateway process");

    let healthy = wait_for_socket_health(&ctx.socket, Duration::from_secs(5)).await;
    if !healthy {
        bail!("gateway process started (pid {pid}) but socket did not become healthy within 5s");
    }

    Ok(())
}

async fn stop_fallback(ctx: &GatewayContext) -> Result<()> {
    let Some(pid) = read_pid_file(&ctx.agent_id) else {
        bail!(
            "error: gateway is not running\n  hint: start it with `coop gateway start` or `coop gateway install` for auto-start"
        );
    };

    if !is_pid_alive(pid) {
        warn!(
            pid,
            "pid file exists but process is not alive, removing stale pid file"
        );
        remove_pid_file(&ctx.agent_id);
        bail!(
            "error: gateway is not running\n  hint: start it with `coop gateway start` or `coop gateway install` for auto-start"
        );
    }

    kill_process(pid)?;

    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if !is_pid_alive(pid) {
            break;
        }
        sleep(Duration::from_millis(100)).await;
    }

    if is_pid_alive(pid) {
        bail!("failed to stop gateway process {pid}");
    }

    remove_pid_file(&ctx.agent_id);
    Ok(())
}

fn sanitize_agent_id(agent_id: &str) -> String {
    agent_id
        .chars()
        .map(|char| {
            if char.is_ascii_alphanumeric() || matches!(char, '-' | '_') {
                char
            } else {
                '-'
            }
        })
        .collect()
}

fn service_name_for(platform: Platform, safe_agent_id: &str) -> String {
    match platform {
        Platform::Systemd => format!("coop-{safe_agent_id}.service"),
        Platform::Launchd => format!("com.coop.{safe_agent_id}"),
        Platform::Unsupported => format!("coop-{safe_agent_id}"),
    }
}

fn resolve_service_paths(
    config_path: &Path,
    safe_agent_id: &str,
    service_name: &str,
    platform: Platform,
) -> Result<ServicePaths> {
    let binary = absolute_path(&std::env::current_exe().context("resolving current executable")?)?;
    let config = absolute_path(config_path)?;
    let config_dir = config
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .to_path_buf();

    let service_dir = config_dir.join("service");
    let logs_dir = config_dir.join("logs");

    let unit_file = match platform {
        Platform::Systemd => home_dir()?.join(".config/systemd/user").join(service_name),
        Platform::Launchd => home_dir()?
            .join("Library/LaunchAgents")
            .join(format!("{service_name}.plist")),
        Platform::Unsupported => service_dir.join(format!("{service_name}.unit")),
    };

    Ok(ServicePaths {
        binary,
        config,
        unit_file,
        env_file: service_dir.join(format!("{safe_agent_id}.env")),
        launchd_wrapper: service_dir.join(format!("launchd-{safe_agent_id}.sh")),
        trace_file: config_dir.join("traces.jsonl"),
        stdout_log: logs_dir.join(format!("{safe_agent_id}.stdout.log")),
        stderr_log: logs_dir.join(format!("{safe_agent_id}.stderr.log")),
    })
}

fn home_dir() -> Result<PathBuf> {
    std::env::var("HOME")
        .map(PathBuf::from)
        .context("HOME is not set")
}

fn absolute_path(path: &Path) -> Result<PathBuf> {
    if path.is_absolute() {
        Ok(path.to_path_buf())
    } else {
        Ok(std::env::current_dir()?.join(path))
    }
}

fn service_kind_label(platform: Platform) -> &'static str {
    match platform {
        Platform::Systemd => "systemd user",
        Platform::Launchd => "launchd user agent",
        Platform::Unsupported => "fallback",
    }
}

fn trace_path_for_status(ctx: &GatewayContext) -> PathBuf {
    read_env_file(&ctx.paths.env_file)
        .ok()
        .and_then(|env| env.get("COOP_TRACE_FILE").map(PathBuf::from))
        .unwrap_or_else(|| ctx.paths.trace_file.clone())
}

fn trace_path_from_env(paths: &ServicePaths, env: &BTreeMap<String, String>) -> PathBuf {
    env.get("COOP_TRACE_FILE")
        .map_or_else(|| paths.trace_file.clone(), PathBuf::from)
}

fn resolve_effective_env(
    config: &Config,
    paths: &ServicePaths,
    options: &InstallOptions,
) -> Result<BTreeMap<String, String>> {
    resolve_effective_env_with_lookup(
        config,
        paths,
        &options.envs,
        options.trace_file.as_deref(),
        options.trace_max_size,
        options.rust_log.as_deref(),
        |key| std::env::var(key).ok(),
    )
}

fn resolve_runtime_env(config: &Config, paths: &ServicePaths) -> Result<BTreeMap<String, String>> {
    resolve_effective_env_with_lookup(config, paths, &[], None, None, None, |key| {
        std::env::var(key).ok()
    })
}

#[allow(clippy::too_many_arguments)]
fn resolve_effective_env_with_lookup<F>(
    config: &Config,
    paths: &ServicePaths,
    env_args: &[String],
    trace_file_flag: Option<&str>,
    trace_max_size_flag: Option<u64>,
    rust_log_flag: Option<&str>,
    lookup: F,
) -> Result<BTreeMap<String, String>>
where
    F: Fn(&str) -> Option<String>,
{
    let mut env = BTreeMap::new();

    let mut capture_keys = BTreeSet::new();
    capture_keys.insert("ANTHROPIC_API_KEY".to_owned());
    capture_keys.insert("OPENAI_API_KEY".to_owned());

    for key_ref in &config.provider.api_keys {
        if let Some(variable) = key_ref.strip_prefix("env:") {
            capture_keys.insert(variable.to_owned());
        }
    }

    if let Some(embedding) = &config.memory.embedding
        && let Some(env_var) = embedding.required_api_key_env()
    {
        capture_keys.insert(env_var);
    }

    for key in capture_keys {
        if let Some(value) = lookup(&key) {
            env.insert(key, value);
        }
    }

    if let Some(value) = lookup("COOP_TRACE_FILE") {
        env.insert("COOP_TRACE_FILE".to_owned(), value);
    } else {
        env.insert(
            "COOP_TRACE_FILE".to_owned(),
            paths.trace_file.display().to_string(),
        );
    }

    if let Some(value) = lookup("COOP_TRACE_MAX_SIZE") {
        env.insert("COOP_TRACE_MAX_SIZE".to_owned(), value);
    }

    if let Some(value) = lookup("RUST_LOG") {
        env.insert("RUST_LOG".to_owned(), value);
    }

    for assignment in env_args {
        let (key, value) = parse_env_assignment(assignment)?;
        env.insert(key, value);
    }

    if let Some(value) = trace_file_flag {
        env.insert("COOP_TRACE_FILE".to_owned(), value.to_owned());
    }
    if let Some(value) = trace_max_size_flag {
        env.insert("COOP_TRACE_MAX_SIZE".to_owned(), value.to_string());
    }
    if let Some(value) = rust_log_flag {
        env.insert("RUST_LOG".to_owned(), value.to_owned());
    }

    Ok(env)
}

fn parse_env_assignment(raw: &str) -> Result<(String, String)> {
    let Some((key, value)) = raw.split_once('=') else {
        bail!("invalid --env value '{raw}', expected KEY=VALUE");
    };

    if !valid_env_key(key) {
        bail!("invalid environment variable name '{key}'");
    }

    Ok((key.to_owned(), value.to_owned()))
}

fn valid_env_key(key: &str) -> bool {
    let mut chars = key.chars();
    let Some(first) = chars.next() else {
        return false;
    };

    if !(first.is_ascii_alphabetic() || first == '_') {
        return false;
    }

    chars.all(|char| char.is_ascii_alphanumeric() || char == '_')
}

fn render_env_file(env: &BTreeMap<String, String>) -> String {
    let mut output = String::from("# Managed by coop — do not edit manually.\n");
    for (key, value) in env {
        output.push_str(key);
        output.push('=');
        output.push_str(&shell_quote(value));
        output.push('\n');
    }
    output
}

fn render_env_preview(env: &BTreeMap<String, String>, print_secrets: bool) -> String {
    let mut output = String::new();
    for (key, value) in env {
        let rendered = if print_secrets || !is_secret_key(key) {
            value.clone()
        } else {
            "<redacted>".to_owned()
        };
        output.push_str("    ");
        output.push_str(key);
        output.push('=');
        output.push_str(&rendered);
        output.push('\n');
    }
    output
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

fn parse_shell_value(value: &str) -> String {
    if value.starts_with('\'') && value.ends_with('\'') && value.len() >= 2 {
        value[1..value.len() - 1].replace("'\"'\"'", "'")
    } else if value.starts_with('"') && value.ends_with('"') && value.len() >= 2 {
        value[1..value.len() - 1].to_owned()
    } else {
        value.to_owned()
    }
}

fn is_secret_key(key: &str) -> bool {
    let upper = key.to_ascii_uppercase();
    ["KEY", "TOKEN", "SECRET", "PASSWORD", "AUTH"]
        .iter()
        .any(|needle| upper.contains(needle))
}

fn read_env_file(path: &Path) -> Result<BTreeMap<String, String>> {
    if !path.exists() {
        return Ok(BTreeMap::new());
    }

    let content =
        fs::read_to_string(path).with_context(|| format!("reading env file {}", path.display()))?;
    let mut map = BTreeMap::new();

    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }

        let Some((key, value)) = trimmed.split_once('=') else {
            continue;
        };
        map.insert(key.to_owned(), parse_shell_value(value));
    }

    Ok(map)
}

fn render_systemd_unit(agent_id: &str, paths: &ServicePaths) -> String {
    format!(
        "# Managed by coop — do not edit manually.\n# Reinstall with: coop gateway install\n\n[Unit]\nDescription=Coop Agent Gateway ({agent_id})\nAfter=network-online.target\nWants=network-online.target\n\n[Service]\nType=simple\nExecStart={} start --config {}\nRestart=on-failure\nRestartSec=5\nEnvironmentFile={}\n\n[Install]\nWantedBy=default.target\n",
        display(&paths.binary),
        display(&paths.config),
        display(&paths.env_file),
    )
}

fn render_launchd_wrapper(paths: &ServicePaths) -> String {
    format!(
        "#!/bin/sh\nset -a\n. \"{}\"\nset +a\nexec \"{}\" start --config \"{}\"\n",
        display(&paths.env_file),
        display(&paths.binary),
        display(&paths.config)
    )
}

fn render_launchd_plist(service_name: &str, paths: &ServicePaths) -> String {
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n<plist version=\"1.0\">\n<dict>\n    <key>Label</key>\n    <string>{}</string>\n    <key>ProgramArguments</key>\n    <array>\n        <string>{}</string>\n    </array>\n    <key>RunAtLoad</key>\n    <true/>\n    <key>KeepAlive</key>\n    <dict>\n        <key>SuccessfulExit</key>\n        <false/>\n    </dict>\n    <key>StandardOutPath</key>\n    <string>{}</string>\n    <key>StandardErrorPath</key>\n    <string>{}</string>\n    <key>ProcessType</key>\n    <string>Background</string>\n</dict>\n</plist>\n",
        xml_escape(service_name),
        xml_escape(&display(&paths.launchd_wrapper)),
        xml_escape(&display(&paths.stdout_log)),
        xml_escape(&display(&paths.stderr_log)),
    )
}

fn xml_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn print_generated_configuration(
    ctx: &GatewayContext,
    env_vars: &BTreeMap<String, String>,
    print_secrets: bool,
) {
    println!(
        "🐔 coop gateway — service configuration for agent \"{}\"\n",
        ctx.agent_id
    );

    match ctx.platform {
        Platform::Unsupported => {
            println!("  No supported service manager detected.");
            println!("  Below is the information needed to create a service manually.\n");
            println!("  binary:      {}", ctx.paths.binary.display());
            println!(
                "  arguments:   start --config {}",
                ctx.paths.config.display()
            );
            println!("  environment:");
            print!("{}", render_env_preview(env_vars, print_secrets));
            println!();
            println!("  # OpenRC (/etc/init.d/{}):", ctx.safe_agent_id);
            println!("    command=\"{}\"", ctx.paths.binary.display());
            println!(
                "    command_args=\"start --config {}\"",
                ctx.paths.config.display()
            );
            println!("    command_background=true");
            println!(
                "    pidfile=\"{}\"\n",
                pid_file_path(&ctx.agent_id).display()
            );

            let user = std::env::var("USER").unwrap_or_else(|_| "alice".to_owned());
            println!("  # runit (/etc/sv/{}/run):", ctx.safe_agent_id);
            println!("    #!/bin/sh");
            println!(
                "    exec chpst -u {} {} start --config {}\n",
                user,
                ctx.paths.binary.display(),
                ctx.paths.config.display()
            );

            println!("  # crontab (minimal, no crash recovery):");
            println!(
                "    @reboot COOP_TRACE_FILE={} {} start --config {}",
                trace_path_from_env(&ctx.paths, env_vars).display(),
                ctx.paths.binary.display(),
                ctx.paths.config.display()
            );
        }
        Platform::Systemd => {
            println!("# {}", ctx.paths.unit_file.display());
            println!("{}", render_systemd_unit(&ctx.agent_id, &ctx.paths));
            println!("# {}", ctx.paths.env_file.display());
            print!("{}", render_env_preview(env_vars, print_secrets));
        }
        Platform::Launchd => {
            println!("# {}", ctx.paths.unit_file.display());
            println!("{}", render_launchd_plist(&ctx.service_name, &ctx.paths));
            println!("# {}", ctx.paths.launchd_wrapper.display());
            println!("{}", render_launchd_wrapper(&ctx.paths));
            println!("# {}", ctx.paths.env_file.display());
            print!("{}", render_env_preview(env_vars, print_secrets));
        }
    }
}

fn display(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

fn ensure_parent(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("creating directory {}", parent.display()))?;
    }
    Ok(())
}

fn write_file_with_mode(path: &Path, content: &str, mode: u32) -> Result<()> {
    fs::write(path, content).with_context(|| format!("writing {}", path.display()))?;
    #[cfg(unix)]
    {
        fs::set_permissions(path, fs::Permissions::from_mode(mode))
            .with_context(|| format!("setting permissions for {}", path.display()))?;
    }
    Ok(())
}

fn remove_file_if_exists(path: &Path) -> Result<()> {
    if path.exists() {
        fs::remove_file(path).with_context(|| format!("removing {}", path.display()))?;
    }
    Ok(())
}

/// Wait for launchd to fully unload a service after `bootout`.
///
/// `bootout` returns before launchd finishes tearing down the service.
/// If `bootstrap` runs while teardown is still in progress, launchd
/// returns error 5 (I/O error). We poll `launchctl print` until the
/// service is no longer loaded.
async fn launchd_wait_for_unload(domain: &str) {
    for attempt in 0..20 {
        let result = run_cmd_allow_failure("launchctl", &["print", domain]);
        match result {
            Ok(ref cmd) if !cmd.success => {
                debug!(domain, attempt, "launchd service fully unloaded");
                return;
            }
            _ => {
                debug!(domain, attempt, "launchd service still loaded, waiting");
                sleep(Duration::from_millis(250)).await;
            }
        }
    }
    warn!(
        domain,
        "launchd service did not unload within 5s, proceeding anyway"
    );
}

/// Bootstrap a launchd service with retries.
///
/// Even after `launchctl print` reports the service as unloaded, launchd
/// may still briefly reject `bootstrap` with error 5. Retry a few times
/// with backoff to handle this window.
async fn launchd_bootstrap_with_retry(domain_target: &str, plist_path: &str) -> Result<()> {
    for attempt in 0..5 {
        let result = run_cmd_allow_failure("launchctl", &["bootstrap", domain_target, plist_path])?;
        if result.success {
            return Ok(());
        }

        let is_retryable = result.status_code == Some(5)
            || result.stderr.contains("Input/output error")
            || result.stderr.contains("Operation now in progress");

        if !is_retryable {
            let stderr = if result.stderr.trim().is_empty() {
                "(no stderr)"
            } else {
                result.stderr.trim()
            };
            bail!("launchctl bootstrap failed: {stderr}");
        }

        warn!(attempt, stderr = %result.stderr.trim(), "launchctl bootstrap returned retryable error, retrying");
        sleep(Duration::from_millis(500)).await;
    }
    bail!(
        "launchctl bootstrap failed after 5 retries — launchd may still be tearing down the previous service"
    )
}

fn run_cmd(program: &str, args: &[&str]) -> Result<String> {
    let result = run_cmd_allow_failure(program, args)?;
    if !result.success {
        error!(
            program,
            args = ?args,
            status = ?result.status_code,
            stderr = %truncate_for_log(&result.stderr),
            "external command failed"
        );
        let stderr = if result.stderr.trim().is_empty() {
            "(no stderr)"
        } else {
            result.stderr.trim()
        };
        bail!("{program} failed: {stderr}");
    }
    Ok(result.stdout.trim().to_owned())
}

fn run_cmd_ignore_failure(program: &str, args: &[&str]) -> Result<()> {
    let result = run_cmd_allow_failure(program, args)?;
    if !result.success {
        warn!(
            program,
            args = ?args,
            status = ?result.status_code,
            stderr = %truncate_for_log(&result.stderr),
            "external command failed (ignored)"
        );
    }
    Ok(())
}

fn run_cmd_allow_failure(program: &str, args: &[&str]) -> Result<CmdResult> {
    info!(program, args = ?args, "executing external command");
    let output = Command::new(program)
        .args(args)
        .output()
        .with_context(|| format!("failed to execute {program}"))?;

    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();

    debug!(
        program,
        args = ?args,
        status = ?output.status.code(),
        stderr = %truncate_for_log(&stderr),
        "external command finished"
    );

    Ok(CmdResult {
        stdout,
        stderr,
        success: output.status.success(),
        status_code: output.status.code(),
    })
}

fn truncate_for_log(value: &str) -> String {
    const MAX: usize = 300;
    if value.len() <= MAX {
        value.to_owned()
    } else {
        format!("{}…", &value[..MAX])
    }
}

fn print_service_status(ctx: &GatewayContext) -> Result<()> {
    if !ctx.installed() {
        println!("  service:  not installed");
        return Ok(());
    }

    match ctx.platform {
        Platform::Systemd => {
            let active =
                run_cmd_allow_failure("systemctl", &["--user", "is-active", &ctx.service_name])?;
            let enabled =
                run_cmd_allow_failure("systemctl", &["--user", "is-enabled", &ctx.service_name])?;
            let enabled_suffix = if enabled.success { ", enabled" } else { "" };
            if active.success {
                println!(
                    "  service:  {} (systemd user{})",
                    ctx.service_name, enabled_suffix
                );
            } else {
                println!(
                    "  service:  {} (systemd user{}, inactive)",
                    ctx.service_name, enabled_suffix
                );
            }
        }
        Platform::Launchd => {
            println!("  service:  {} (launchd user agent)", ctx.service_name);
        }
        Platform::Unsupported => {
            println!("  service:  not installed");
        }
    }

    Ok(())
}

fn pid_uptime(pid: u32) -> Option<String> {
    let output = run_cmd_allow_failure("ps", &["-o", "etime=", "-p", &pid.to_string()]).ok()?;
    if output.success {
        let uptime = output.stdout.trim();
        if uptime.is_empty() {
            None
        } else {
            Some(uptime.to_owned())
        }
    } else {
        None
    }
}

fn resolve_backup_path(config_path: &Path, backup: Option<&str>) -> PathBuf {
    backup.map_or_else(|| config_path.with_extension("toml.bak"), PathBuf::from)
}

fn failed_config_snapshot_path(config_path: &Path, timestamp: DateTime<Utc>) -> PathBuf {
    let stem = config_path
        .file_stem()
        .and_then(|value| value.to_str())
        .unwrap_or("coop");
    let snapshot = format!("{stem}.failed-{}.toml", timestamp.format("%Y%m%d-%H%M%S"));
    config_path.with_file_name(snapshot)
}

async fn wait_for_socket_health(socket: &Path, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;

    while Instant::now() < deadline {
        if probe_socket_health(socket).await {
            return true;
        }
        sleep(Duration::from_millis(100)).await;
    }

    false
}

async fn probe_socket_health(socket: &Path) -> bool {
    let attempt = async {
        let mut client = IpcClient::connect(socket).await?;
        client
            .send(ClientMessage::Hello {
                version: PROTOCOL_VERSION,
            })
            .await?;
        let response = client.recv().await?;
        if matches!(response, ServerMessage::Hello { .. }) {
            Ok::<(), anyhow::Error>(())
        } else {
            bail!("unexpected hello response")
        }
    };

    tokio::time::timeout(Duration::from_secs(1), attempt)
        .await
        .ok()
        .and_then(Result::ok)
        .is_some()
}

pub(crate) fn spawn_background(
    binary: &Path,
    config: &Path,
    env_vars: &BTreeMap<String, String>,
) -> Result<u32> {
    let mut command = Command::new(binary);
    command.args(["start", "--config", &display(config)]);
    for (key, value) in env_vars {
        command.env(key, value);
    }

    let child = command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .context("failed to spawn gateway process")?;

    Ok(child.id())
}

fn kill_process(pid: u32) -> Result<()> {
    let status = Command::new("kill")
        .args(["-TERM", &pid.to_string()])
        .status()
        .context("failed to execute kill")?;

    if !status.success() {
        bail!("failed to send SIGTERM to pid {pid}");
    }

    Ok(())
}

async fn tail_trace_file(path: &Path, lines: usize, follow: bool) -> Result<()> {
    let initial = read_last_n_lines(path, lines).await?;
    let use_color = std::io::stdout().is_terminal();
    for line in initial {
        println!("{}", format_trace_line(&line, use_color));
    }

    if follow {
        follow_file(path, true).await?;
    }

    Ok(())
}

async fn tail_plain_file(path: &Path, lines: usize, follow: bool) -> Result<()> {
    let initial = read_last_n_lines(path, lines).await?;
    for line in initial {
        println!("{line}");
    }

    if follow {
        follow_file(path, false).await?;
    }

    Ok(())
}

#[allow(clippy::naive_bytecount)]
async fn read_last_n_lines(path: &Path, line_count: usize) -> Result<Vec<String>> {
    let mut file = File::open(path)
        .await
        .with_context(|| format!("opening {}", path.display()))?;
    let metadata = file.metadata().await?;
    let mut offset = metadata.len();
    let mut newline_count = 0usize;
    let mut chunks: Vec<Vec<u8>> = Vec::new();

    while offset > 0 && newline_count <= line_count {
        let chunk_size = offset.min(8_192) as usize;
        offset -= chunk_size as u64;
        file.seek(std::io::SeekFrom::Start(offset)).await?;
        let mut chunk = vec![0_u8; chunk_size];
        file.read_exact(&mut chunk).await?;
        newline_count += chunk.iter().filter(|byte| **byte == b'\n').count();
        chunks.push(chunk);
    }

    chunks.reverse();
    let total_len: usize = chunks.iter().map(Vec::len).sum();
    let mut data = Vec::with_capacity(total_len);
    for chunk in chunks {
        data.extend_from_slice(&chunk);
    }

    let text = String::from_utf8_lossy(&data);
    let mut lines: Vec<String> = text.lines().map(ToOwned::to_owned).collect();
    if lines.len() > line_count {
        lines.drain(0..(lines.len() - line_count));
    }
    Ok(lines)
}

async fn follow_file(path: &Path, parse_jsonl: bool) -> Result<()> {
    let use_color = parse_jsonl && std::io::stdout().is_terminal();

    let mut file = File::open(path)
        .await
        .with_context(|| format!("opening {}", path.display()))?;
    let mut identity = file_identity(path).await?;
    let mut offset = identity.len;
    let mut pending = String::new();

    loop {
        let maybe_new_identity = file_identity(path).await;
        if let Ok(next_identity) = maybe_new_identity
            && rotation_detected(identity, next_identity, offset)
        {
            info!(path = %path.display(), "trace file rotated or truncated, reopening");
            file = File::open(path)
                .await
                .with_context(|| format!("reopening {}", path.display()))?;
            identity = next_identity;
            offset = 0;
            pending.clear();
        }

        file.seek(std::io::SeekFrom::Start(offset)).await?;
        let mut buf = Vec::new();
        let bytes = file.read_to_end(&mut buf).await?;
        if bytes > 0 {
            offset += bytes as u64;
            identity = file_identity(path).await.unwrap_or(identity);
            pending.push_str(&String::from_utf8_lossy(&buf));

            while let Some(newline_idx) = pending.find('\n') {
                let line = pending[..newline_idx].to_owned();
                pending.drain(..=newline_idx);
                if parse_jsonl {
                    println!("{}", format_trace_line(&line, use_color));
                } else {
                    println!("{line}");
                }
            }
        }

        tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                break;
            }
            () = sleep(Duration::from_millis(200)) => {}
        }
    }

    Ok(())
}

async fn file_identity(path: &Path) -> Result<FileIdentity> {
    let metadata = tokio::fs::metadata(path)
        .await
        .with_context(|| format!("reading metadata for {}", path.display()))?;

    Ok(FileIdentity {
        len: metadata.len(),
        #[cfg(unix)]
        inode: metadata.ino(),
    })
}

fn rotation_detected(previous: FileIdentity, next: FileIdentity, offset: u64) -> bool {
    #[cfg(unix)]
    if previous.inode != next.inode {
        return true;
    }

    next.len < offset || next.len < previous.len
}

fn format_trace_line(line: &str, use_color: bool) -> String {
    let parsed: Result<Value, _> = serde_json::from_str(line);
    let Ok(value) = parsed else {
        return line.to_owned();
    };

    let timestamp = value
        .get("timestamp")
        .and_then(Value::as_str)
        .and_then(format_timestamp)
        .unwrap_or_else(|| "-".to_owned());

    let level = value
        .get("level")
        .and_then(Value::as_str)
        .unwrap_or("INFO")
        .to_owned();

    let fields = value.get("fields").and_then(Value::as_object);
    let message = fields
        .and_then(|map| map.get("message"))
        .and_then(Value::as_str)
        .unwrap_or("-");

    let span = value
        .get("span")
        .and_then(|span| span.get("name"))
        .and_then(Value::as_str)
        .or_else(|| {
            value
                .get("spans")
                .and_then(Value::as_array)
                .and_then(|spans| spans.first())
                .and_then(|span| span.get("name"))
                .and_then(Value::as_str)
        })
        .unwrap_or("-");

    let detail_keys = [
        "session",
        "tool",
        "input_tokens",
        "output_len",
        "pid",
        "agent_id",
    ];
    let details = fields
        .map(|map| {
            detail_keys
                .iter()
                .filter_map(|key| {
                    map.get(*key)
                        .map(|value| format!("{key}={}", render_json_scalar(value)))
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();

    let level_rendered = if use_color {
        colorize_level(&level)
    } else {
        level
    };

    if details.is_empty() {
        format!("{timestamp} {level_rendered:<5} [{span}] {message}")
    } else {
        format!(
            "{timestamp} {level_rendered:<5} [{span}] {message} ({})",
            details.join(", ")
        )
    }
}

fn format_timestamp(value: &str) -> Option<String> {
    let parsed = DateTime::parse_from_rfc3339(value).ok()?;
    Some(
        parsed
            .with_timezone(&Local)
            .format("%Y-%m-%d %H:%M:%S")
            .to_string(),
    )
}

fn render_json_scalar(value: &Value) -> String {
    match value {
        Value::String(value) => value.clone(),
        Value::Number(value) => value.to_string(),
        Value::Bool(value) => value.to_string(),
        _ => value.to_string(),
    }
}

fn colorize_level(level: &str) -> String {
    let (prefix, suffix) = match level {
        "ERROR" => ("\x1b[31m", "\x1b[0m"),
        "WARN" => ("\x1b[33m", "\x1b[0m"),
        "INFO" => ("\x1b[32m", "\x1b[0m"),
        "DEBUG" => ("\x1b[34m", "\x1b[0m"),
        _ => ("\x1b[0m", "\x1b[0m"),
    };
    format!("{prefix}{level}{suffix}")
}

#[allow(clippy::unwrap_used)]
#[cfg(test)]
mod tests {
    use super::*;

    fn test_config() -> Config {
        Config {
            agent: crate::config::AgentConfig {
                id: "agent/main".to_owned(),
                model: "anthropic/test".to_owned(),
                workspace: "./workspaces/default".to_owned(),
            },
            users: Vec::new(),
            channels: crate::config::ChannelsConfig::default(),
            provider: crate::config::ProviderConfig {
                name: "anthropic".to_owned(),
                api_keys: vec![
                    "env:ANTHROPIC_API_KEY".to_owned(),
                    "env:SECONDARY_KEY".to_owned(),
                ],
            },
            prompt: crate::config::PromptConfig::default(),
            memory: crate::config::MemoryConfig::default(),
            tools: crate::config::ToolsConfig::default(),
            cron: Vec::new(),
            sandbox: crate::config::SandboxConfig::default(),
        }
    }

    #[test]
    fn service_name_sanitizes_agent_id() {
        let sanitized = sanitize_agent_id("agent/main:prod");
        assert_eq!(sanitized, "agent-main-prod");
        assert_eq!(
            service_name_for(Platform::Systemd, &sanitized),
            "coop-agent-main-prod.service"
        );
        assert_eq!(
            service_name_for(Platform::Launchd, &sanitized),
            "com.coop.agent-main-prod"
        );
    }

    #[test]
    fn pid_file_round_trip() {
        let agent = format!("pid-test-{}", std::process::id());
        let path = write_pid_file(&agent).unwrap();
        assert!(path.exists());
        let pid = read_pid_file(&agent).unwrap();
        assert_eq!(pid, std::process::id());
        remove_pid_file(&agent);
        assert!(!path.exists());
    }

    #[test]
    fn is_pid_alive_checks_current_pid() {
        assert!(is_pid_alive(std::process::id()));
        assert!(!is_pid_alive(0));
        assert!(!is_pid_alive(u32::MAX));
    }

    #[test]
    fn systemd_unit_generation_uses_env_file() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = ServicePaths {
            binary: tmp.path().join("coop"),
            config: tmp.path().join("coop.toml"),
            unit_file: tmp.path().join("coop.service"),
            env_file: tmp.path().join("service.env"),
            launchd_wrapper: tmp.path().join("wrapper.sh"),
            trace_file: tmp.path().join("traces.jsonl"),
            stdout_log: tmp.path().join("stdout.log"),
            stderr_log: tmp.path().join("stderr.log"),
        };

        let rendered = render_systemd_unit("agent", &paths);
        assert!(rendered.contains("EnvironmentFile="));
        assert!(rendered.contains(&display(&paths.env_file)));
        assert!(!rendered.contains("%i"));
    }

    #[test]
    fn launchd_plist_generation_uses_wrapper() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = ServicePaths {
            binary: tmp.path().join("coop"),
            config: tmp.path().join("coop.toml"),
            unit_file: tmp.path().join("coop.plist"),
            env_file: tmp.path().join("service.env"),
            launchd_wrapper: tmp.path().join("wrapper.sh"),
            trace_file: tmp.path().join("traces.jsonl"),
            stdout_log: tmp.path().join("stdout.log"),
            stderr_log: tmp.path().join("stderr.log"),
        };

        let rendered = render_launchd_plist("com.coop.agent", &paths);
        assert!(rendered.contains("ProgramArguments"));
        assert!(rendered.contains(&xml_escape(&display(&paths.launchd_wrapper))));
        assert!(!rendered.contains(&xml_escape(&display(&paths.binary))));
    }

    #[test]
    fn xml_escaping_works() {
        let escaped = xml_escape("a&b<c>d");
        assert_eq!(escaped, "a&amp;b&lt;c&gt;d");
    }

    #[test]
    fn env_file_generation_and_permissions() {
        let tmp = tempfile::tempdir().unwrap();
        let env_file = tmp.path().join("service.env");

        let mut env = BTreeMap::new();
        env.insert("COOP_TRACE_FILE".to_owned(), "/tmp/traces.jsonl".to_owned());
        env.insert("ANTHROPIC_API_KEY".to_owned(), "test-token".to_owned());

        write_file_with_mode(&env_file, &render_env_file(&env), 0o600).unwrap();
        let content = fs::read_to_string(&env_file).unwrap();
        assert!(content.contains("COOP_TRACE_FILE"));

        #[cfg(unix)]
        {
            let mode = fs::metadata(&env_file).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }
    }

    #[test]
    fn environment_capture_filters_correctly() {
        let config = test_config();
        let tmp = tempfile::tempdir().unwrap();
        let paths = ServicePaths {
            binary: tmp.path().join("coop"),
            config: tmp.path().join("coop.toml"),
            unit_file: tmp.path().join("coop.service"),
            env_file: tmp.path().join("service.env"),
            launchd_wrapper: tmp.path().join("wrapper.sh"),
            trace_file: tmp.path().join("traces.jsonl"),
            stdout_log: tmp.path().join("stdout.log"),
            stderr_log: tmp.path().join("stderr.log"),
        };

        let source = BTreeMap::from([
            ("ANTHROPIC_API_KEY".to_owned(), "a".to_owned()),
            ("SECONDARY_KEY".to_owned(), "b".to_owned()),
            ("OPENAI_API_KEY".to_owned(), "c".to_owned()),
            ("HOME".to_owned(), "/home/alice".to_owned()),
            ("PATH".to_owned(), "/usr/bin".to_owned()),
        ]);

        let env =
            resolve_effective_env_with_lookup(&config, &paths, &[], None, None, None, |key| {
                source.get(key).cloned()
            })
            .unwrap();

        assert!(env.contains_key("ANTHROPIC_API_KEY"));
        assert!(env.contains_key("SECONDARY_KEY"));
        assert!(env.contains_key("OPENAI_API_KEY"));
        assert!(!env.contains_key("HOME"));
        assert!(!env.contains_key("PATH"));
    }

    #[test]
    fn environment_captures_embedding_api_key() {
        let mut config = test_config();
        config.memory.embedding = Some(crate::config::MemoryEmbeddingConfig {
            provider: "openai-compatible".to_owned(),
            model: "text-embedding-3-small".to_owned(),
            dimensions: 1536,
            base_url: Some("https://openrouter.ai/api/v1".to_owned()),
            api_key_env: Some("OPENROUTER_API_KEY".to_owned()),
        });

        let tmp = tempfile::tempdir().unwrap();
        let paths = ServicePaths {
            binary: tmp.path().join("coop"),
            config: tmp.path().join("coop.toml"),
            unit_file: tmp.path().join("coop.service"),
            env_file: tmp.path().join("service.env"),
            launchd_wrapper: tmp.path().join("wrapper.sh"),
            trace_file: tmp.path().join("traces.jsonl"),
            stdout_log: tmp.path().join("stdout.log"),
            stderr_log: tmp.path().join("stderr.log"),
        };

        let source = BTreeMap::from([
            ("ANTHROPIC_API_KEY".to_owned(), "a".to_owned()),
            ("OPENROUTER_API_KEY".to_owned(), "r".to_owned()),
        ]);

        let env =
            resolve_effective_env_with_lookup(&config, &paths, &[], None, None, None, |key| {
                source.get(key).cloned()
            })
            .unwrap();

        assert!(env.contains_key("OPENROUTER_API_KEY"));
        assert_eq!(env["OPENROUTER_API_KEY"], "r");
    }

    #[test]
    fn print_redacts_secrets_by_default() {
        let mut env = BTreeMap::new();
        env.insert("ANTHROPIC_API_KEY".to_owned(), "secret".to_owned());
        env.insert("COOP_TRACE_FILE".to_owned(), "/tmp/traces.jsonl".to_owned());

        let redacted = render_env_preview(&env, false);
        assert!(redacted.contains("ANTHROPIC_API_KEY=<redacted>"));
        assert!(redacted.contains("COOP_TRACE_FILE=/tmp/traces.jsonl"));

        let plain = render_env_preview(&env, true);
        assert!(plain.contains("ANTHROPIC_API_KEY=secret"));
    }

    #[test]
    fn rollback_paths_are_resolved() {
        let config = PathBuf::from("/tmp/coop.toml");
        let backup = resolve_backup_path(&config, None);
        assert_eq!(backup, PathBuf::from("/tmp/coop.toml.bak"));

        let snapshot = failed_config_snapshot_path(
            &config,
            DateTime::parse_from_rfc3339("2026-02-15T13:54:00Z")
                .unwrap()
                .with_timezone(&Utc),
        );
        assert_eq!(
            snapshot,
            PathBuf::from("/tmp/coop.failed-20260215-135400.toml")
        );
    }

    #[test]
    fn rotation_detection_handles_inode_and_truncation() {
        let previous = FileIdentity {
            len: 100,
            #[cfg(unix)]
            inode: 1,
        };
        let next_same = FileIdentity {
            len: 90,
            #[cfg(unix)]
            inode: 1,
        };
        assert!(rotation_detected(previous, next_same, 95));

        let next_inode = FileIdentity {
            len: 10,
            #[cfg(unix)]
            inode: 2,
        };
        assert!(rotation_detected(previous, next_inode, 100));
    }
}
