#![allow(clippy::print_stdout, clippy::print_stderr)] // CLI binary — stdout/stderr is the UI

mod cli;
mod compaction;
mod compaction_store;
mod config;
mod config_check;
mod config_tool;
mod config_watcher;
mod config_write;
mod gateway;
mod heartbeat;
mod memory_embedding;
mod memory_prompt_index;
mod memory_reconcile;
mod memory_tools;
mod router;
mod scheduler;
mod session_store;
#[cfg(feature = "signal")]
mod signal_loop;
mod tracing_setup;
mod trust;
mod tui_helpers;

use anyhow::{Context, Result};
use chrono::Utc;
use clap::Parser;
use coop_agent::AnthropicProvider;
use coop_core::tools::{CompositeExecutor, DefaultExecutor};
use coop_core::{InboundKind, InboundMessage, Provider, TurnEvent};
use coop_ipc::{
    ClientMessage, IpcClient, IpcConnection, IpcServer, PROTOCOL_VERSION, ServerMessage,
    socket_path,
};
use coop_memory::{Memory, SqliteMemory};
use coop_tui::{
    App, Container, DisplayMessage, Editor, Footer, InputAction, StatusLine, Tui, handle_key_event,
    poll_event,
};
use crossterm::event::Event;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{Instrument, debug, info, info_span, warn};

use crate::cli::{Cli, Commands, MemoryCommands, SignalCommands};
use crate::config::{Config, SharedConfig, shared_config};
use crate::gateway::Gateway;
use crate::memory_embedding::build_embedder;
use crate::memory_reconcile::ProviderReconciler;
use crate::memory_tools::MemoryToolExecutor;
use crate::router::MessageRouter;
#[cfg(feature = "signal")]
use crate::signal_loop::run_signal_loop;
use crate::tui_helpers::{
    build_tui, extract_tool_result, format_tui_welcome, resolve_working_dir, sync_editor_from_app,
    update_chat_messages,
};

#[cfg(feature = "signal")]
use coop_channels::{SignalChannel, SignalToolExecutor, SignalTypingNotifier};

// Component indices — layout: header(0), chat(1), spacer(2), status(3), editor(4), footer(5)
const CHAT_IDX: usize = 1;
const STATUS_IDX: usize = 3;
const EDITOR_IDX: usize = 4;
const FOOTER_IDX: usize = 5;

#[tokio::main]
#[allow(clippy::large_futures)]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    let console_log = matches!(
        cli.command,
        Commands::Start | Commands::Signal { .. } | Commands::Memory { .. } | Commands::Version
    );
    let _tracing_guard = tracing_setup::init(console_log);

    info!(
        version = env!("CARGO_PKG_VERSION"),
        pid = std::process::id(),
        "coop starting"
    );

    match cli.command {
        Commands::Check { format } => cmd_check(cli.config.as_deref(), &format),
        Commands::Start => cmd_start(cli.config.as_deref()).await,
        Commands::Chat { user } => cmd_chat(cli.config.as_deref(), user.as_deref()).await,
        Commands::Attach { session } => cmd_attach(cli.config.as_deref(), &session).await,
        Commands::Signal { command } => cmd_signal(cli.config.as_deref(), command).await,
        Commands::Memory { command } => cmd_memory(cli.config.as_deref(), command).await,
        Commands::Version => {
            println!("🐔 coop {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
    }
}

/// Resolve which user the TUI session runs as.
/// If `--user` is given, validate it exists in config.
/// Otherwise, default to "root".
fn resolve_tui_user(config: &Config, user_flag: Option<&str>) -> String {
    if let Some(name) = user_flag {
        if !config.users.iter().any(|u| u.name == name) {
            let available = config
                .users
                .iter()
                .map(|u| u.name.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            tracing::warn!(
                user = name,
                available = available,
                "user not found in config, using anyway"
            );
        }
        name.to_owned()
    } else {
        "root".to_owned()
    }
}

// ---------------------------------------------------------------------------
// cmd_check — validate config without starting
// ---------------------------------------------------------------------------

#[allow(clippy::unnecessary_wraps)] // must return Result to match main's match arms
fn cmd_check(config_path: Option<&str>, format: &str) -> Result<()> {
    let config_file = Config::find_config_path(config_path);
    let config_dir = config_file
        .parent()
        .unwrap_or(&PathBuf::from("."))
        .to_path_buf();

    let report = config_check::validate_config(&config_file, &config_dir);

    match format {
        "json" => report.print_json(),
        _ => report.print_human(),
    }

    if report.has_errors() {
        std::process::exit(1);
    }
    Ok(())
}

fn init_memory_store(
    config: &Config,
    config_dir: &Path,
    provider: Arc<dyn Provider>,
) -> Result<Arc<dyn Memory>> {
    let memory_db_path = tui_helpers::resolve_config_path(config_dir, &config.memory.db_path);
    info!(path = %memory_db_path.display(), "initializing memory store");

    if let Some(embedding) = &config.memory.embedding {
        info!(
            provider = %embedding.provider,
            model = %embedding.model,
            dimensions = embedding.dimensions,
            "memory embedding configured"
        );
    }

    let embedder = build_embedder(config.memory.embedding.as_ref())
        .context("failed to initialize memory embedding provider")?;
    let reconciler: Arc<dyn coop_memory::Reconciler> = Arc::new(ProviderReconciler::new(provider));

    let memory: Arc<dyn Memory> = Arc::new(
        SqliteMemory::open_with_components(
            &memory_db_path,
            config.agent.id.clone(),
            embedder,
            Some(reconciler),
        )
        .with_context(|| {
            format!(
                "failed to initialize memory db at {}",
                memory_db_path.display()
            )
        })?,
    );

    Ok(memory)
}

fn spawn_memory_maintenance_loop(
    memory: Arc<dyn Memory>,
    config: SharedConfig,
    shutdown_token: CancellationToken,
) -> tokio::task::JoinHandle<()> {
    spawn_memory_maintenance_loop_with_interval(
        memory,
        config,
        shutdown_token,
        Duration::from_secs(3600),
    )
}

fn spawn_memory_maintenance_loop_with_interval(
    memory: Arc<dyn Memory>,
    config: SharedConfig,
    shutdown_token: CancellationToken,
    interval_duration: Duration,
) -> tokio::task::JoinHandle<()> {
    let span = info_span!("memory_maintenance_loop");

    tokio::spawn(
        async move {
            // Run once at startup if enabled.
            let retention = config.load().memory.retention.clone();
            if retention.enabled {
                info!(
                    archive_after_days = retention.archive_after_days,
                    delete_archive_after_days = retention.delete_archive_after_days,
                    compress_after_days = retention.compress_after_days,
                    "memory maintenance loop started"
                );
                run_memory_maintenance_once(&memory, &retention.to_maintenance_config(), "startup")
                    .await;
            } else {
                info!("memory maintenance loop started (retention disabled)");
            }
            let mut last_retention = Some(retention);

            let mut interval = tokio::time::interval(interval_duration);
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

            loop {
                tokio::select! {
                    () = shutdown_token.cancelled() => {
                        info!("memory maintenance loop stopped");
                        break;
                    }
                    _ = interval.tick() => {
                        let retention = config.load().memory.retention.clone();

                        // Log when retention config changes.
                        if last_retention.as_ref() != Some(&retention) {
                            debug!(
                                enabled = retention.enabled,
                                archive_after_days = retention.archive_after_days,
                                delete_archive_after_days = retention.delete_archive_after_days,
                                compress_after_days = retention.compress_after_days,
                                "memory retention config updated via hot-reload"
                            );
                            last_retention = Some(retention.clone());
                        }

                        if retention.enabled {
                            run_memory_maintenance_once(
                                &memory,
                                &retention.to_maintenance_config(),
                                "periodic",
                            )
                            .await;
                        }
                    }
                }
            }
        }
        .instrument(span),
    )
}

async fn run_memory_maintenance_once(
    memory: &Arc<dyn Memory>,
    maintenance: &coop_memory::MemoryMaintenanceConfig,
    phase: &'static str,
) {
    let span = info_span!("memory_maintenance_run", phase);
    async {
        match memory.run_maintenance(maintenance).await {
            Ok(report) => {
                info!(
                    phase,
                    compressed_rows = report.compressed_rows,
                    summary_rows = report.summary_rows,
                    archived_rows = report.archived_rows,
                    archive_deleted_rows = report.archive_deleted_rows,
                    "memory maintenance run succeeded"
                );
            }
            Err(error) => {
                warn!(phase, error = %error, "memory maintenance run failed");
            }
        }
    }
    .instrument(span)
    .await;
}

// ---------------------------------------------------------------------------
// cmd_start — gateway daemon
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_lines)]
async fn cmd_start(config_path: Option<&str>) -> Result<()> {
    let config_file = Config::find_config_path(config_path);
    let config = Config::load(&config_file)
        .with_context(|| format!("loading config from {}", config_file.display()))?;

    let config_dir = config_file
        .parent()
        .unwrap_or(&PathBuf::from("."))
        .to_path_buf();
    let workspace = config.resolve_workspace(&config_dir)?;

    anyhow::ensure!(
        config.provider.name == "anthropic",
        "only the 'anthropic' provider is supported (got '{}')",
        config.provider.name
    );

    let provider: Arc<dyn Provider> = Arc::new(
        AnthropicProvider::from_env(&config.agent.model)
            .context("failed to initialize Anthropic provider")?,
    );

    #[cfg(feature = "signal")]
    let mut signal_channel: Option<SignalChannel> = None;
    #[cfg(feature = "signal")]
    let mut signal_action_tx: Option<mpsc::Sender<coop_channels::SignalAction>> = None;
    #[cfg(feature = "signal")]
    let mut signal_query_tx: Option<mpsc::Sender<coop_channels::SignalQuery>> = None;

    if let Some(signal) = &config.channels.signal {
        #[cfg(feature = "signal")]
        {
            let db_path = tui_helpers::resolve_config_path(&config_dir, &signal.db_path);
            info!(db_path = %db_path.display(), "signal channel configured");

            match SignalChannel::connect(&db_path).await {
                Ok(channel) => {
                    signal_action_tx = Some(channel.action_sender());
                    signal_query_tx = Some(channel.query_sender());
                    signal_channel = Some(channel);
                }
                Err(error) => {
                    tracing::warn!(
                        error = %error,
                        db_path = %db_path.display(),
                        "failed to initialize signal channel",
                    );
                }
            }
        }

        #[cfg(not(feature = "signal"))]
        {
            let _ = signal;
            tracing::warn!(
                "signal is configured, but this binary was built without the 'signal' feature"
            );
        }
    }

    let memory = init_memory_store(&config, &config_dir, Arc::clone(&provider))?;

    // Capture startup values before wrapping in SharedConfig.
    // SharedConfig is created early so it can be shared with tool executors.
    let agent_id = config.agent.id.clone();
    let agent_model = config.agent.model.clone();

    let shared = shared_config(config);

    // Build the delivery sender early so both the send_message tool and the
    // scheduler can use it.
    #[cfg(feature = "signal")]
    let deliver_tx: Option<scheduler::DeliverySender> = signal_action_tx
        .as_ref()
        .map(|tx| scheduler::spawn_signal_delivery_bridge(tx.clone()));

    #[cfg(not(feature = "signal"))]
    let deliver_tx: Option<scheduler::DeliverySender> = None;

    let default_executor = DefaultExecutor::new();
    let config_executor = config_tool::ConfigToolExecutor::new(config_file.clone());
    let memory_executor = MemoryToolExecutor::new(Arc::clone(&memory));

    #[allow(unused_mut)]
    let mut executors: Vec<Box<dyn coop_core::ToolExecutor>> = vec![
        Box::new(default_executor),
        Box::new(config_executor),
        Box::new(memory_executor),
    ];

    #[cfg(feature = "signal")]
    if let (Some(action_tx), Some(query_tx)) = (signal_action_tx.clone(), signal_query_tx.clone()) {
        executors.push(Box::new(SignalToolExecutor::new(action_tx, query_tx)));
    }

    let executor: Arc<dyn coop_core::ToolExecutor> = Arc::new(CompositeExecutor::new(executors));

    #[cfg(feature = "signal")]
    let typing_notifier: Option<Arc<dyn coop_core::TypingNotifier>> =
        signal_action_tx.as_ref().map(|action_tx| {
            Arc::new(SignalTypingNotifier::new(action_tx.clone()))
                as Arc<dyn coop_core::TypingNotifier>
        });

    #[cfg(not(feature = "signal"))]
    let typing_notifier: Option<Arc<dyn coop_core::TypingNotifier>> = None;

    let maintenance_memory = Arc::clone(&memory);

    let gateway = Arc::new(Gateway::new(
        Arc::clone(&shared),
        workspace,
        provider,
        executor,
        typing_notifier,
        Some(memory),
    )?);
    let router = Arc::new(MessageRouter::new(
        Arc::clone(&shared),
        Arc::clone(&gateway),
    ));

    let working_dir = resolve_working_dir();

    println!(
        "{}\n",
        format_tui_welcome(env!("CARGO_PKG_VERSION"), &agent_model, &working_dir)
    );

    let socket = socket_path(&agent_id);
    let server = IpcServer::bind(&socket)?;

    let shutdown_token = CancellationToken::new();

    let _maintenance_task = spawn_memory_maintenance_loop(
        maintenance_memory,
        Arc::clone(&shared),
        shutdown_token.clone(),
    );

    let cron_notify = Arc::new(tokio::sync::Notify::new());

    let _config_watcher = config_watcher::spawn_config_watcher(
        config_file,
        Arc::clone(&shared),
        shutdown_token.clone(),
        Some(Arc::clone(&cron_notify)),
    );

    #[cfg(feature = "signal")]
    if let Some(signal_channel) = signal_channel {
        let router = Arc::clone(&router);
        tokio::spawn(async move {
            if let Err(error) = run_signal_loop(signal_channel, router).await {
                tracing::warn!(error = %error, "signal loop stopped");
            }
        });
    }

    {
        let sched_config = Arc::clone(&shared);
        let sched_router = Arc::clone(&router);
        let sched_token = shutdown_token.clone();
        let sched_notify = Arc::clone(&cron_notify);
        tokio::spawn(async move {
            scheduler::run_scheduler_with_notify(
                sched_config,
                sched_router,
                deliver_tx,
                sched_token,
                Some(sched_notify),
            )
            .await;
        });
    }

    info!(
        agent = %agent_id,
        model = %agent_model,
        socket = %server.socket_path().display(),
        "gateway started"
    );

    println!("gateway listening on {}", server.socket_path().display());

    loop {
        tokio::select! {
            accepted = server.accept() => {
                match accepted {
                    Ok(connection) => {
                        let router = Arc::clone(&router);
                        let gateway = Arc::clone(&gateway);
                        let agent_id = agent_id.clone();
                        tokio::spawn(async move {
                            if let Err(error) = handle_client(connection, router, gateway, agent_id).await {
                                tracing::warn!(error = %error, "ipc client disconnected with error");
                            }
                        });
                    }
                    Err(error) => {
                        tracing::warn!(error = %error, "failed to accept IPC client");
                    }
                }
            }
            _ = tokio::signal::ctrl_c() => {
                info!("shutting down");
                break;
            }
        }
    }

    shutdown_token.cancel();

    #[cfg(feature = "signal")]
    if let Some(action_tx) = signal_action_tx {
        let _ = action_tx.send(coop_channels::SignalAction::Shutdown).await;
        // Brief grace period for the signal runtime to close the websocket cleanly.
        tokio::time::sleep(Duration::from_millis(250)).await;
    }

    Ok(())
}

async fn handle_client(
    mut connection: IpcConnection,
    router: Arc<MessageRouter>,
    gateway: Arc<Gateway>,
    agent_id: String,
) -> Result<()> {
    loop {
        let Ok(message) = connection.recv().await else {
            return Ok(());
        };

        match message {
            ClientMessage::Hello { version } => {
                if version != PROTOCOL_VERSION {
                    tracing::warn!(
                        client_version = version,
                        server_version = PROTOCOL_VERSION,
                        "ipc version mismatch"
                    );
                }
                connection
                    .send(ServerMessage::Hello {
                        version: PROTOCOL_VERSION,
                        agent_id: agent_id.clone(),
                    })
                    .await?;
            }
            ClientMessage::Subscribe { .. } => {}
            ClientMessage::ListSessions => {
                let keys = gateway
                    .list_sessions()
                    .into_iter()
                    .map(|key| key.to_string())
                    .collect();
                connection.send(ServerMessage::Sessions { keys }).await?;
            }
            ClientMessage::Clear { session } => match gateway.resolve_session(&session) {
                Some(key) => gateway.clear_session(&key),
                None => {
                    connection
                        .send(ServerMessage::Error {
                            session,
                            message: "unknown session".to_owned(),
                        })
                        .await?;
                }
            },
            ClientMessage::Stop { session } => match gateway.resolve_session(&session) {
                Some(key) => {
                    gateway.cancel_active_turn(&key);
                }
                None => {
                    connection
                        .send(ServerMessage::Error {
                            session,
                            message: "unknown session".to_owned(),
                        })
                        .await?;
                }
            },
            ClientMessage::Send { session, content } => {
                handle_send(&mut connection, Arc::clone(&router), session, content).await?;
            }
        }
    }
}

async fn handle_send(
    connection: &mut IpcConnection,
    router: Arc<MessageRouter>,
    session: String,
    content: String,
) -> Result<()> {
    let inbound = InboundMessage {
        channel: "terminal:default".to_owned(),
        sender: "tui".to_owned(),
        content,
        chat_id: None,
        is_group: false,
        timestamp: Utc::now(),
        reply_to: Some(session.clone()),
        kind: InboundKind::Text,
        message_timestamp: None,
    };

    let (event_tx, mut event_rx) = mpsc::channel(64);
    let router_task = tokio::spawn(async move { router.dispatch(&inbound, event_tx).await });

    while let Some(event) = event_rx.recv().await {
        if let Some(message) = ServerMessage::from_turn_event(session.clone(), event) {
            connection.send(message).await?;
        }
    }

    match router_task.await {
        Ok(Ok(_)) => {}
        Ok(Err(error)) => {
            connection
                .send(ServerMessage::Error {
                    session,
                    message: error.to_string(),
                })
                .await?;
        }
        Err(error) => {
            connection
                .send(ServerMessage::Error {
                    session,
                    message: format!("internal server error: {error}"),
                })
                .await?;
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// cmd_chat — TUI client that connects to the gateway via IPC
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_lines, clippy::items_after_statements)]
async fn cmd_chat(config_path: Option<&str>, user_flag: Option<&str>) -> Result<()> {
    let config_file = Config::find_config_path(config_path);
    let config = Config::load(&config_file)
        .with_context(|| format!("loading config from {}", config_file.display()))?;

    let tui_user = resolve_tui_user(&config, user_flag);

    let config_dir = config_file
        .parent()
        .unwrap_or(&PathBuf::from("."))
        .to_path_buf();
    let workspace = config.resolve_workspace(&config_dir)?;

    anyhow::ensure!(
        config.provider.name == "anthropic",
        "only the 'anthropic' provider is supported (got '{}')",
        config.provider.name
    );

    let provider: Arc<dyn Provider> = Arc::new(
        AnthropicProvider::from_env(&config.agent.model)
            .context("failed to initialize Anthropic provider")?,
    );

    let memory = init_memory_store(&config, &config_dir, Arc::clone(&provider))?;

    let default_executor = DefaultExecutor::new();
    let config_executor = config_tool::ConfigToolExecutor::new(config_file.clone());
    let memory_executor = MemoryToolExecutor::new(Arc::clone(&memory));
    let executor: Arc<dyn coop_core::ToolExecutor> = Arc::new(CompositeExecutor::new(vec![
        Box::new(default_executor),
        Box::new(config_executor),
        Box::new(memory_executor),
    ]));

    let agent_id = config.agent.id.clone();
    let agent_model = config.agent.model.clone();
    let shared = shared_config(config);

    let gateway = Arc::new(Gateway::new(
        Arc::clone(&shared),
        workspace,
        provider,
        executor,
        None,
        Some(memory),
    )?);

    let shutdown_token = CancellationToken::new();
    let _config_watcher = config_watcher::spawn_config_watcher(
        config_file,
        Arc::clone(&shared),
        shutdown_token.clone(),
        None,
    );

    let session_key = gateway.default_session_key();
    let working_dir = resolve_working_dir();

    let (mut tui, mut app, mut tool_names) =
        build_tui(&agent_id, &agent_model, "main", &working_dir, 200_000);

    tui.start()?;
    tui.request_render();
    tui.render_if_needed()?;

    let (event_tx, mut event_rx) = mpsc::channel::<TurnEvent>(128);
    let mut turn_task: Option<tokio::task::JoinHandle<Result<()>>> = None;

    loop {
        let mut needs_render = false;

        while let Ok(event) = event_rx.try_recv() {
            needs_render = true;
            handle_turn_event(event, &mut tui, &mut app, &mut tool_names);
        }

        if let Some(ref task) = turn_task
            && task.is_finished()
            && let Some(task) = turn_task.take()
        {
            needs_render |= handle_turn_task_result(task.await, &mut tui, &mut app);
        }

        if let Some(event) = poll_event(Duration::from_millis(50)) {
            if let Event::Key(key_event) = event {
                app.clear_error();

                match handle_key_event(&mut app, key_event) {
                    InputAction::Submit(input) => {
                        if app.is_loading {
                            app.input = input;
                            app.cursor_pos = app.input.len();
                            app.set_error("Cannot send while agent is responding");
                            set_status_error(&mut tui, app.error_message.clone());
                        } else {
                            clear_editor(&mut tui);
                            app.push_message(DisplayMessage::user(&input));
                            app.start_turn();
                            set_status_loading(&mut tui, true);
                            update_chat_messages(&mut tui, &app, CHAT_IDX);

                            let gw = Arc::clone(&gateway);
                            let sk = session_key.clone();
                            let tx = event_tx.clone();
                            let user = tui_user.clone();
                            turn_task = Some(tokio::spawn(async move {
                                gw.run_turn_with_trust(
                                    &sk,
                                    &input,
                                    coop_core::TrustLevel::Full,
                                    Some(&user),
                                    Some("terminal:default"),
                                    tx,
                                )
                                .await
                            }));
                        }
                    }
                    InputAction::Quit => {
                        app.should_quit = true;
                    }
                    InputAction::Clear => {
                        if !app.is_loading {
                            app.clear();
                            clear_chat(&mut tui);
                            tui.force_render();
                            gateway.clear_session(&session_key);
                        }
                    }
                    InputAction::Stop => {
                        if app.is_loading {
                            gateway.cancel_active_turn(&session_key);
                            app.push_message(DisplayMessage::system("Stopping agent…"));
                            update_chat_messages(&mut tui, &app, CHAT_IDX);
                        }
                    }
                    InputAction::ToggleVerbose => {
                        app.toggle_verbose();
                        update_chat_messages(&mut tui, &app, CHAT_IDX);
                    }
                    InputAction::Help => {
                        app.show_help();
                        update_chat_messages(&mut tui, &app, CHAT_IDX);
                    }
                    InputAction::Status => {
                        app.show_status();
                        update_chat_messages(&mut tui, &app, CHAT_IDX);
                    }
                    InputAction::None => {}
                }

                sync_editor_from_app(&mut tui, &app, EDITOR_IDX);
                needs_render = true;
            }
        } else {
            needs_render |= tick_loading(&mut tui, &mut app);
        }

        if needs_render {
            tui.request_render();
            tui.render_if_needed()?;
        }

        if app.should_quit {
            break;
        }
    }

    shutdown_token.cancel();
    tui.stop()?;
    println!("👋 Goodbye!");
    Ok(())
}

// ---------------------------------------------------------------------------
// cmd_attach — TUI client that connects to a running gateway via IPC
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_lines)]
async fn cmd_attach(config_path: Option<&str>, session: &str) -> Result<()> {
    let config_file = Config::find_config_path(config_path);
    let config = Config::load(&config_file)
        .with_context(|| format!("loading config from {}", config_file.display()))?;

    let socket = socket_path(&config.agent.id);
    let mut client = IpcClient::connect(&socket).await.with_context(|| {
        format!(
            "is the gateway running? (coop start)\nsocket: {}",
            socket.display()
        )
    })?;

    client
        .send(ClientMessage::Hello {
            version: PROTOCOL_VERSION,
        })
        .await?;
    let hello = client.recv().await?;
    let agent_id = match hello {
        ServerMessage::Hello {
            version, agent_id, ..
        } => {
            if version != PROTOCOL_VERSION {
                tracing::warn!(
                    server_version = version,
                    client_version = PROTOCOL_VERSION,
                    "protocol version mismatch"
                );
            }
            agent_id
        }
        other => anyhow::bail!("unexpected server response: {other:?}"),
    };

    info!(agent = %agent_id, session = %session, socket = %socket.display(), "attached to gateway");

    let working_dir = resolve_working_dir();

    let (mut tui, mut app, mut tool_names) = build_tui(
        &agent_id,
        &config.agent.model,
        session,
        &working_dir,
        200_000,
    );

    tui.start()?;
    tui.request_render();
    tui.render_if_needed()?;

    let (mut reader, mut writer) = client.into_split();

    let (ipc_tx, mut ipc_rx) = mpsc::channel::<ServerMessage>(128);
    let session_filter = session.to_owned();
    tokio::spawn(async move {
        while let Ok(msg) = reader.recv().await {
            if ipc_tx.send(msg).await.is_err() {
                break;
            }
        }
    });

    let session_name = session.to_owned();

    loop {
        let mut needs_render = false;

        while let Ok(msg) = ipc_rx.try_recv() {
            needs_render = true;
            handle_server_message(msg, &session_filter, &mut tui, &mut app, &mut tool_names);
        }

        if let Some(event) = poll_event(Duration::from_millis(50)) {
            if let Event::Key(key_event) = event {
                app.clear_error();

                match handle_key_event(&mut app, key_event) {
                    InputAction::Submit(input) => {
                        if app.is_loading {
                            app.input = input;
                            app.cursor_pos = app.input.len();
                            app.set_error("Cannot send while agent is responding");
                            set_status_error(&mut tui, app.error_message.clone());
                        } else {
                            clear_editor(&mut tui);
                            app.push_message(DisplayMessage::user(&input));
                            app.start_turn();
                            set_status_loading(&mut tui, true);
                            update_chat_messages(&mut tui, &app, CHAT_IDX);

                            if let Err(error) = writer
                                .send(ClientMessage::Send {
                                    session: session_name.clone(),
                                    content: input,
                                })
                                .await
                            {
                                app.push_message(DisplayMessage::system(format!(
                                    "Error: {error:#}"
                                )));
                                app.end_turn(0);
                                update_chat_messages(&mut tui, &app, CHAT_IDX);
                            }
                        }
                    }
                    InputAction::Quit => {
                        app.should_quit = true;
                    }
                    InputAction::Clear => {
                        if !app.is_loading {
                            app.clear();
                            clear_chat(&mut tui);
                            tui.force_render();

                            if let Err(error) = writer
                                .send(ClientMessage::Clear {
                                    session: session_name.clone(),
                                })
                                .await
                            {
                                tracing::warn!(error = %error, "failed to send clear");
                            }
                        }
                    }
                    InputAction::Stop => {
                        if app.is_loading {
                            app.push_message(DisplayMessage::system("Stopping agent…"));
                            update_chat_messages(&mut tui, &app, CHAT_IDX);

                            if let Err(error) = writer
                                .send(ClientMessage::Stop {
                                    session: session_name.clone(),
                                })
                                .await
                            {
                                tracing::warn!(error = %error, "failed to send stop");
                            }
                        }
                    }
                    InputAction::ToggleVerbose => {
                        app.toggle_verbose();
                        update_chat_messages(&mut tui, &app, CHAT_IDX);
                    }
                    InputAction::Help => {
                        app.show_help();
                        update_chat_messages(&mut tui, &app, CHAT_IDX);
                    }
                    InputAction::Status => {
                        app.show_status();
                        update_chat_messages(&mut tui, &app, CHAT_IDX);
                    }
                    InputAction::None => {}
                }

                sync_editor_from_app(&mut tui, &app, EDITOR_IDX);
                needs_render = true;
            }
        } else {
            needs_render |= tick_loading(&mut tui, &mut app);
        }

        if needs_render {
            tui.request_render();
            tui.render_if_needed()?;
        }

        if app.should_quit {
            break;
        }
    }

    tui.stop()?;
    println!("👋 Goodbye!");
    Ok(())
}

// ---------------------------------------------------------------------------
// cmd_memory
// ---------------------------------------------------------------------------

async fn cmd_memory(config_path: Option<&str>, command: MemoryCommands) -> Result<()> {
    let config_file = Config::find_config_path(config_path);
    let config = Config::load(&config_file)
        .with_context(|| format!("loading config from {}", config_file.display()))?;

    let config_dir = config_file
        .parent()
        .unwrap_or(&PathBuf::from("."))
        .to_path_buf();

    let provider: Arc<dyn Provider> = Arc::new(
        AnthropicProvider::from_env(&config.agent.model)
            .context("failed to initialize Anthropic provider")?,
    );

    let memory = init_memory_store(&config, &config_dir, provider)?;

    match command {
        MemoryCommands::RebuildIndex => {
            let count = memory.rebuild_index().await?;
            println!("rebuilt vec index: {count} entries");
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// cmd_signal
// ---------------------------------------------------------------------------

#[allow(clippy::large_futures, clippy::unused_async)]
async fn cmd_signal(config_path: Option<&str>, command: SignalCommands) -> Result<()> {
    #[cfg(feature = "signal")]
    {
        cmd_signal_enabled(config_path, command).await
    }

    #[cfg(not(feature = "signal"))]
    {
        let _ = (config_path, command);
        anyhow::bail!("signal support is not enabled in this build")
    }
}

#[cfg(feature = "signal")]
#[allow(clippy::large_futures)]
async fn cmd_signal_enabled(config_path: Option<&str>, command: SignalCommands) -> Result<()> {
    let config_file = Config::find_config_path(config_path);
    let config = Config::load(&config_file)
        .with_context(|| format!("loading config from {}", config_file.display()))?;

    let config_dir = config_file
        .parent()
        .unwrap_or(&PathBuf::from("."))
        .to_path_buf();

    let signal = config
        .channels
        .signal
        .ok_or_else(|| anyhow::anyhow!("signal channel is not configured in coop.yaml"))?;
    let db_path = tui_helpers::resolve_config_path(&config_dir, &signal.db_path);

    match command {
        SignalCommands::Link { device_name } => {
            SignalChannel::link_device(&db_path, device_name.clone(), |url| {
                println!("Scan this QR code with Signal to link your device:\n");
                qr2term::print_qr(url).context("failed to render provisioning QR code")?;
                println!("\nProvisioning URL: {url}");
                Ok(())
            })
            .await?;
            println!("linked signal device using {}", db_path.display());
        }
        SignalCommands::Unlink => {
            SignalChannel::unlink(&db_path).await?;
            println!("cleared signal registration at {}", db_path.display());
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Shared TUI event handlers (used by both cmd_chat and cmd_attach)
// ---------------------------------------------------------------------------

fn handle_turn_event(
    event: TurnEvent,
    tui: &mut Tui,
    app: &mut App,
    tool_names: &mut HashMap<String, String>,
) {
    match event {
        TurnEvent::TextDelta(text) => {
            app.append_or_create_assistant(&text);
            update_chat_messages(tui, app, CHAT_IDX);
        }
        TurnEvent::ToolStart {
            id,
            name,
            arguments,
        } => {
            tool_names.insert(id, name.clone());
            app.push_message(DisplayMessage::tool_call(&name, &arguments));
            update_chat_messages(tui, app, CHAT_IDX);
        }
        TurnEvent::ToolResult { id, message } => {
            let name = tool_names
                .get(&id)
                .cloned()
                .unwrap_or_else(|| "unknown".to_owned());
            let (output, is_error) = extract_tool_result(&message);
            app.push_message(DisplayMessage::tool_output(&name, output, is_error));
            update_chat_messages(tui, app, CHAT_IDX);
        }
        TurnEvent::AssistantMessage(_) => {}
        TurnEvent::Done(result) => {
            let tokens = result.usage.total_tokens();
            app.end_turn(tokens);
            set_status_loading(tui, false);
            set_status_error(tui, None);
            set_footer_usage(tui, tokens);
            update_chat_messages(tui, app, CHAT_IDX);
        }
        TurnEvent::Error(message) => {
            app.push_message(DisplayMessage::system(format!("Error: {message}")));
            if app.is_loading {
                app.end_turn(0);
            }
            set_status_loading(tui, false);
            set_status_error(tui, Some(message));
            update_chat_messages(tui, app, CHAT_IDX);
        }
    }
}

fn handle_server_message(
    msg: ServerMessage,
    session_filter: &str,
    tui: &mut Tui,
    app: &mut App,
    tool_names: &mut HashMap<String, String>,
) {
    match msg {
        ServerMessage::TextDelta { text, session } if session == session_filter => {
            app.append_or_create_assistant(&text);
            update_chat_messages(tui, app, CHAT_IDX);
        }
        ServerMessage::ToolStart {
            id,
            name,
            arguments,
            session,
        } if session == session_filter => {
            tool_names.insert(id, name.clone());
            app.push_message(DisplayMessage::tool_call(&name, &arguments));
            update_chat_messages(tui, app, CHAT_IDX);
        }
        ServerMessage::ToolResult {
            id,
            output,
            is_error,
            session,
        } if session == session_filter => {
            let name = tool_names
                .get(&id)
                .cloned()
                .unwrap_or_else(|| "unknown".to_owned());
            app.push_message(DisplayMessage::tool_output(&name, output, is_error));
            update_chat_messages(tui, app, CHAT_IDX);
        }
        ServerMessage::AssistantMessage { session, .. } if session == session_filter => {}
        ServerMessage::Done {
            tokens, session, ..
        } if session == session_filter => {
            app.end_turn(tokens);
            set_status_loading(tui, false);
            set_status_error(tui, None);
            set_footer_usage(tui, tokens);
            update_chat_messages(tui, app, CHAT_IDX);
        }
        ServerMessage::Error { message, session } if session == session_filter => {
            app.push_message(DisplayMessage::system(format!("Error: {message}")));
            if app.is_loading {
                app.end_turn(0);
            }
            set_status_loading(tui, false);
            set_status_error(tui, Some(message));
            update_chat_messages(tui, app, CHAT_IDX);
        }
        _ => {}
    }
}

/// Returns true if a task error occurred and needs render.
fn handle_turn_task_result(
    join_result: std::result::Result<Result<()>, tokio::task::JoinError>,
    tui: &mut Tui,
    app: &mut App,
) -> bool {
    match join_result {
        Ok(Ok(())) => false,
        Ok(Err(error)) => {
            app.push_message(DisplayMessage::system(format!("Error: {error:#}")));
            if app.is_loading {
                app.end_turn(0);
            }
            set_status_loading(tui, false);
            set_status_error(tui, Some(error.to_string()));
            update_chat_messages(tui, app, CHAT_IDX);
            true
        }
        Err(error) => {
            app.push_message(DisplayMessage::system(format!(
                "Error: task panicked: {error}"
            )));
            if app.is_loading {
                app.end_turn(0);
            }
            true
        }
    }
}

fn tick_loading(tui: &mut Tui, app: &mut App) -> bool {
    app.tick_loading();
    app.tick_error();

    let status = tui.root_mut().children_mut()[STATUS_IDX]
        .as_any_mut()
        .and_then(|a| a.downcast_mut::<StatusLine>());
    if let Some(s) = status {
        s.tick();
        s.set_elapsed(&app.elapsed_text());
        if app.is_loading || app.error_message.is_some() {
            return true;
        }
    }
    false
}

fn set_status_loading(tui: &mut Tui, loading: bool) {
    let status = tui.root_mut().children_mut()[STATUS_IDX]
        .as_any_mut()
        .and_then(|a| a.downcast_mut::<StatusLine>());
    if let Some(s) = status {
        s.set_loading(loading);
    }
}

fn set_status_error(tui: &mut Tui, error: Option<String>) {
    let status = tui.root_mut().children_mut()[STATUS_IDX]
        .as_any_mut()
        .and_then(|a| a.downcast_mut::<StatusLine>());
    if let Some(s) = status {
        s.set_error(error);
    }
}

fn set_footer_usage(tui: &mut Tui, tokens: u32) {
    let footer = tui.root_mut().children_mut()[FOOTER_IDX]
        .as_any_mut()
        .and_then(|a| a.downcast_mut::<Footer>());
    if let Some(f) = footer {
        f.set_usage(0, 0, 0, 0, tokens);
    }
}

fn clear_editor(tui: &mut Tui) {
    let editor = tui.root_mut().children_mut()[EDITOR_IDX]
        .as_any_mut()
        .and_then(|a| a.downcast_mut::<Editor>());
    if let Some(e) = editor {
        e.clear();
    }
}

fn clear_chat(tui: &mut Tui) {
    let chat = tui.root_mut().children_mut()[CHAT_IDX]
        .as_any_mut()
        .and_then(|a| a.downcast_mut::<Container>());
    if let Some(c) = chat {
        c.clear();
    }
}

#[allow(clippy::unwrap_used)]
#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::MemoryRetentionConfig;
    use async_trait::async_trait;
    use coop_core::SessionKey;
    use coop_memory::{
        MemoryMaintenanceConfig, MemoryMaintenanceReport, MemoryQuery, NewObservation, Observation,
        ObservationHistoryEntry, ObservationIndex, Person, SessionSummary, WriteOutcome,
    };
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn test_config() -> Config {
        serde_yaml::from_str(
            "
agent:
  id: test
  model: test-model
users:
  - name: alice
    trust: full
    match: ['terminal:default']
  - name: bob
    trust: full
    match: ['signal:bob-uuid']
",
        )
        .unwrap()
    }

    #[derive(Debug, Default)]
    struct CountingMemory {
        maintenance_calls: AtomicUsize,
    }

    impl CountingMemory {
        fn maintenance_calls(&self) -> usize {
            self.maintenance_calls.load(Ordering::Relaxed)
        }
    }

    #[async_trait]
    impl Memory for CountingMemory {
        async fn search(&self, _query: &MemoryQuery) -> Result<Vec<ObservationIndex>> {
            Ok(Vec::new())
        }

        async fn timeline(
            &self,
            _anchor: i64,
            _before: usize,
            _after: usize,
        ) -> Result<Vec<ObservationIndex>> {
            Ok(Vec::new())
        }

        async fn get(&self, _ids: &[i64]) -> Result<Vec<Observation>> {
            Ok(Vec::new())
        }

        async fn write(&self, _obs: NewObservation) -> Result<WriteOutcome> {
            Ok(WriteOutcome::Skipped)
        }

        async fn people(&self, _query: &str) -> Result<Vec<Person>> {
            Ok(Vec::new())
        }

        async fn summarize_session(&self, session_key: &SessionKey) -> Result<SessionSummary> {
            Ok(SessionSummary {
                session_key: session_key.to_string(),
                request: String::new(),
                outcome: String::new(),
                decisions: Vec::new(),
                open_items: Vec::new(),
                observation_count: 0,
                created_at: Utc::now(),
            })
        }

        async fn history(&self, _observation_id: i64) -> Result<Vec<ObservationHistoryEntry>> {
            Ok(Vec::new())
        }

        async fn run_maintenance(
            &self,
            _config: &MemoryMaintenanceConfig,
        ) -> Result<MemoryMaintenanceReport> {
            self.maintenance_calls.fetch_add(1, Ordering::Relaxed);
            Ok(MemoryMaintenanceReport::default())
        }
    }

    #[test]
    fn resolve_tui_user_defaults_to_root() {
        let config = test_config();
        let user = resolve_tui_user(&config, None);
        assert_eq!(user, "root");
    }

    #[test]
    fn resolve_tui_user_explicit_flag() {
        let config = test_config();
        let user = resolve_tui_user(&config, Some("bob"));
        assert_eq!(user, "bob");
    }

    #[test]
    fn resolve_tui_user_accepts_unknown_with_warning() {
        let config = test_config();
        let user = resolve_tui_user(&config, Some("mallory"));
        assert_eq!(user, "mallory");
    }

    #[test]
    fn resolve_tui_user_defaults_to_root_with_empty_config() {
        let config: Config = serde_yaml::from_str(
            "
agent:
  id: test
  model: test-model
",
        )
        .unwrap();
        let user = resolve_tui_user(&config, None);
        assert_eq!(user, "root");
    }

    #[tokio::test]
    async fn maintenance_loop_runs_startup_and_periodic_without_crashing() {
        let memory = Arc::new(CountingMemory::default());
        let memory_dyn: Arc<dyn Memory> = Arc::<CountingMemory>::clone(&memory);
        let shutdown = CancellationToken::new();

        let mut config = test_config();
        config.memory.retention = MemoryRetentionConfig {
            enabled: true,
            archive_after_days: 1,
            delete_archive_after_days: 2,
            compress_after_days: 1,
            compression_min_cluster_size: 3,
            max_rows_per_run: 10,
        };
        let shared = shared_config(config);

        let handle = spawn_memory_maintenance_loop_with_interval(
            memory_dyn,
            shared,
            shutdown.clone(),
            Duration::from_millis(20),
        );

        tokio::time::sleep(Duration::from_millis(80)).await;
        shutdown.cancel();
        handle.await.unwrap();

        assert!(memory.maintenance_calls() >= 2);
    }

    #[tokio::test]
    async fn maintenance_loop_skips_when_disabled() {
        let memory = Arc::new(CountingMemory::default());
        let memory_dyn: Arc<dyn Memory> = Arc::<CountingMemory>::clone(&memory);
        let shutdown = CancellationToken::new();

        let mut config = test_config();
        config.memory.retention = MemoryRetentionConfig {
            enabled: false,
            archive_after_days: 1,
            delete_archive_after_days: 2,
            compress_after_days: 1,
            compression_min_cluster_size: 3,
            max_rows_per_run: 10,
        };
        let shared = shared_config(config);

        let handle = spawn_memory_maintenance_loop_with_interval(
            memory_dyn,
            shared,
            shutdown.clone(),
            Duration::from_millis(20),
        );

        tokio::time::sleep(Duration::from_millis(80)).await;
        shutdown.cancel();
        handle.await.unwrap();

        // Maintenance should never have been called since retention is disabled.
        assert_eq!(memory.maintenance_calls(), 0);
    }

    #[tokio::test]
    async fn maintenance_loop_picks_up_hot_reloaded_retention() {
        let memory = Arc::new(CountingMemory::default());
        let memory_dyn: Arc<dyn Memory> = Arc::<CountingMemory>::clone(&memory);
        let shutdown = CancellationToken::new();

        // Start with retention disabled.
        let mut config = test_config();
        config.memory.retention = MemoryRetentionConfig {
            enabled: false,
            archive_after_days: 1,
            delete_archive_after_days: 2,
            compress_after_days: 1,
            compression_min_cluster_size: 3,
            max_rows_per_run: 10,
        };
        let shared = shared_config(config);

        let handle = spawn_memory_maintenance_loop_with_interval(
            memory_dyn,
            Arc::clone(&shared),
            shutdown.clone(),
            Duration::from_millis(20),
        );

        // Let a few ticks pass — should NOT run maintenance.
        tokio::time::sleep(Duration::from_millis(60)).await;
        assert_eq!(
            memory.maintenance_calls(),
            0,
            "should not run when disabled"
        );

        // Hot-reload: enable retention.
        let mut new_config = shared.load().as_ref().clone();
        new_config.memory.retention.enabled = true;
        new_config.memory.retention.archive_after_days = 99;
        shared.store(Arc::new(new_config));

        // Let a few more ticks pass — should now run maintenance.
        tokio::time::sleep(Duration::from_millis(60)).await;
        shutdown.cancel();
        handle.await.unwrap();

        assert!(
            memory.maintenance_calls() >= 1,
            "maintenance should run after hot-reload enables retention, got {}",
            memory.maintenance_calls()
        );
    }
}
