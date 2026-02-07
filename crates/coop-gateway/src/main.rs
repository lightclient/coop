mod cli;
mod config;
mod gateway;
mod router;
mod tracing_setup;
mod trust;
mod tui_helpers;

use anyhow::{Context, Result};
use chrono::Utc;
use clap::Parser;
use coop_agent::AnthropicProvider;
use coop_core::tools::DefaultExecutor;
use coop_core::{InboundMessage, Provider, TurnEvent};
use coop_ipc::{
    ClientMessage, IpcClient, IpcConnection, IpcServer, PROTOCOL_VERSION, ServerMessage,
    socket_path,
};
use coop_tui::{
    App, Container, DisplayMessage, Editor, Footer, InputAction, StatusLine, Tui, handle_key_event,
    poll_event,
};
use crossterm::event::Event;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use tracing::info;

use crate::cli::{Cli, Commands, SignalCommands};
use crate::config::Config;
use crate::gateway::Gateway;
use crate::router::MessageRouter;
use crate::tui_helpers::{
    build_tui, extract_tool_result, format_tui_welcome, resolve_working_dir, sync_editor_from_app,
    update_chat_messages,
};

#[cfg(feature = "signal")]
use coop_channels::SignalChannel;

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
        Commands::Start | Commands::Signal { .. } | Commands::Version
    );
    let _tracing_guard = tracing_setup::init(console_log);

    info!(
        version = env!("CARGO_PKG_VERSION"),
        pid = std::process::id(),
        "coop starting"
    );

    match cli.command {
        Commands::Start => cmd_start(cli.config.as_deref()).await,
        Commands::Chat => cmd_chat(cli.config.as_deref()).await,
        Commands::Attach { session } => cmd_attach(cli.config.as_deref(), &session).await,
        Commands::Signal { command } => cmd_signal(cli.config.as_deref(), command).await,
        Commands::Version => {
            println!("🐔 coop {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
    }
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
    let system_prompt = config.build_system_prompt(&config_dir)?;

    anyhow::ensure!(
        config.provider.name == "anthropic",
        "only the 'anthropic' provider is supported (got '{}')",
        config.provider.name
    );

    let provider: Arc<dyn Provider> = Arc::new(
        AnthropicProvider::from_env(&config.agent.model)
            .context("failed to initialize Anthropic provider")?,
    );
    let executor = Arc::new(DefaultExecutor::new());
    let gateway = Arc::new(Gateway::new(
        config.clone(),
        system_prompt,
        provider,
        executor,
    ));
    let router = Arc::new(MessageRouter::new(config.clone(), gateway.clone()));

    let working_dir = resolve_working_dir();

    println!(
        "{}\n",
        format_tui_welcome(env!("CARGO_PKG_VERSION"), &config.agent.model, &working_dir)
    );

    let socket = socket_path(&config.agent.id);
    let server = IpcServer::bind(&socket)?;

    if let Some(signal) = &config.channels.signal {
        #[cfg(feature = "signal")]
        {
            let db_path = tui_helpers::resolve_config_path(&config_dir, &signal.db_path);
            info!(db_path = %db_path.display(), "signal channel configured");

            match SignalChannel::connect(&db_path).await {
                Ok(signal_channel) => {
                    let router = router.clone();
                    tokio::spawn(async move {
                        if let Err(error) = run_signal_loop(signal_channel, router).await {
                            tracing::warn!(error = %error, "signal loop stopped");
                        }
                    });
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

    info!(
        agent = %config.agent.id,
        model = %config.agent.model,
        socket = %server.socket_path().display(),
        "gateway started"
    );

    println!("gateway listening on {}", server.socket_path().display());

    loop {
        tokio::select! {
            accepted = server.accept() => {
                match accepted {
                    Ok(connection) => {
                        let router = router.clone();
                        let gateway = gateway.clone();
                        let agent_id = config.agent.id.clone();
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
                            message: "unknown session".to_string(),
                        })
                        .await?;
                }
            },
            ClientMessage::Send { session, content } => {
                handle_send(&mut connection, router.clone(), session, content).await?;
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
        channel: "terminal:default".to_string(),
        sender: "alice".to_string(),
        content,
        chat_id: None,
        is_group: false,
        timestamp: Utc::now(),
        reply_to: Some(session.clone()),
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

#[cfg(feature = "signal")]
async fn run_signal_loop(
    mut signal_channel: SignalChannel,
    router: Arc<MessageRouter>,
) -> Result<()> {
    loop {
        let inbound = coop_core::Channel::recv(&mut signal_channel).await?;
        let Some(target) = signal_reply_target(&inbound) else {
            continue;
        };

        let (_decision, response) = router.dispatch_collect_text(&inbound).await?;
        if response.trim().is_empty() {
            continue;
        }

        coop_core::Channel::send(
            &signal_channel,
            coop_core::OutboundMessage {
                channel: "signal".to_string(),
                target,
                content: response,
            },
        )
        .await?;
    }
}

#[cfg(feature = "signal")]
fn signal_reply_target(msg: &InboundMessage) -> Option<String> {
    if let Some(reply_to) = &msg.reply_to {
        return Some(reply_to.clone());
    }

    if msg.is_group {
        return msg.chat_id.as_ref().map(|chat_id| {
            if chat_id.starts_with("group:") {
                chat_id.clone()
            } else {
                format!("group:{chat_id}")
            }
        });
    }

    Some(msg.sender.clone())
}

// ---------------------------------------------------------------------------
// cmd_chat — TUI client that connects to the gateway via IPC
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_lines, clippy::items_after_statements)]
async fn cmd_chat(config_path: Option<&str>) -> Result<()> {
    let config_file = Config::find_config_path(config_path);
    let config = Config::load(&config_file)
        .with_context(|| format!("loading config from {}", config_file.display()))?;

    let config_dir = config_file
        .parent()
        .unwrap_or(&PathBuf::from("."))
        .to_path_buf();
    let system_prompt = config.build_system_prompt(&config_dir)?;

    anyhow::ensure!(
        config.provider.name == "anthropic",
        "only the 'anthropic' provider is supported (got '{}')",
        config.provider.name
    );

    let provider: Arc<dyn Provider> = Arc::new(
        AnthropicProvider::from_env(&config.agent.model)
            .context("failed to initialize Anthropic provider")?,
    );
    let executor = Arc::new(DefaultExecutor::new());
    let gateway = Arc::new(Gateway::new(
        config.clone(),
        system_prompt,
        provider,
        executor,
    ));

    let session_key = gateway.default_session_key();
    let working_dir = resolve_working_dir();

    let (mut tui, mut app, mut tool_names) = build_tui(
        &config.agent.id,
        &config.agent.model,
        "main",
        &working_dir,
        200_000,
    );

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

                            let gw = gateway.clone();
                            let sk = session_key.clone();
                            let tx = event_tx.clone();
                            turn_task = Some(tokio::spawn(async move {
                                gw.run_turn_with_trust(&sk, &input, coop_core::TrustLevel::Full, tx)
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
                    InputAction::ToggleVerbose => {
                        app.toggle_verbose();
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
    let session_filter = session.to_string();
    tokio::spawn(async move {
        while let Ok(msg) = reader.recv().await {
            if ipc_tx.send(msg).await.is_err() {
                break;
            }
        }
    });

    let session_name = session.to_string();

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
                    InputAction::ToggleVerbose => {
                        app.toggle_verbose();
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
                .unwrap_or_else(|| "unknown".to_string());
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
                .unwrap_or_else(|| "unknown".to_string());
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
