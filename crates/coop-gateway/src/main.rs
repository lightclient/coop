mod config;
mod gateway;
mod router;
mod trust;

use anyhow::{Context, Result};
use chrono::Utc;
use clap::{Parser, Subcommand};
use coop_agent::AnthropicProvider;
use coop_core::tools::DefaultExecutor;
use coop_core::{Content, InboundMessage, Provider, TurnEvent};
use coop_ipc::{
    ClientMessage, IpcConnection, IpcServer, PROTOCOL_VERSION, ServerMessage, socket_path,
};
use coop_tui::{
    App, Container, DisplayMessage, Editor, Footer, InputAction, MarkdownComponent, Spacer,
    StatusLine, Text, ToolBox, Tui, handle_key_event, poll_event,
};
use crossterm::event::Event;
use std::collections::HashMap;
#[cfg(feature = "signal")]
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use tracing::info;

use crate::config::Config;
use crate::gateway::Gateway;
use crate::router::MessageRouter;

#[cfg(feature = "signal")]
use coop_channels::SignalChannel;

#[derive(Parser)]
#[command(name = "coop", version, about = "🐔 Coop — Personal Agent Gateway")]
struct Cli {
    #[command(subcommand)]
    command: Commands,

    #[arg(short, long, global = true)]
    config: Option<String>,
}

#[derive(Subcommand)]
enum Commands {
    Start,
    Chat,
    Signal {
        #[command(subcommand)]
        command: SignalCommands,
    },
    Version,
}

#[derive(Subcommand)]
enum SignalCommands {
    Link {
        #[arg(long, default_value = "coop-agent")]
        device_name: String,
    },
    Unlink,
}

#[tokio::main]
#[allow(clippy::large_futures)]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_target(false)
        .init();

    match cli.command {
        Commands::Start => cmd_start(cli.config.as_deref()).await,
        Commands::Chat => cmd_chat(cli.config.as_deref()).await,
        Commands::Signal { command } => cmd_signal(cli.config.as_deref(), command).await,
        Commands::Version => {
            println!("🐔 coop {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
    }
}

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

    // Print welcome banner
    let cwd = std::env::current_dir()
        .map(|path| path.display().to_string())
        .unwrap_or_default();
    let home = std::env::var("HOME").unwrap_or_default();
    let working_dir = if !home.is_empty() && cwd.starts_with(&home) {
        format!("~{}", &cwd[home.len()..])
    } else {
        cwd
    };

    println!(
        "{}\n",
        format_tui_welcome(env!("CARGO_PKG_VERSION"), &config.agent.model, &working_dir,)
    );

    let socket = socket_path(&config.agent.id);
    let server = IpcServer::bind(&socket)?;

    if let Some(signal) = &config.channels.signal {
        #[cfg(feature = "signal")]
        {
            let db_path = resolve_config_path(&config_dir, &signal.db_path);
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

    // Gather working directory
    let cwd = std::env::current_dir()
        .map(|path| path.display().to_string())
        .unwrap_or_default();
    let home = std::env::var("HOME").unwrap_or_default();
    let working_dir = if !home.is_empty() && cwd.starts_with(&home) {
        format!("~{}", &cwd[home.len()..])
    } else {
        cwd
    };

    // Detect git branch
    let git_branch = detect_git_branch();

    // Build the TUI
    let mut tui = Tui::new();

    // Create app state
    let mut app = App::new(&config.agent.id, &config.agent.model, "main", 200_000);
    app.version = env!("CARGO_PKG_VERSION").to_string();
    app.working_dir.clone_from(&working_dir);

    // Component indices — layout: header(0), chat(1), spacer(2), status(3), editor(4), footer(5)
    const CHAT_IDX: usize = 1;
    const STATUS_IDX: usize = 3;
    const EDITOR_IDX: usize = 4;
    const FOOTER_IDX: usize = 5;

    // Header — original Coop logo with version info
    let welcome = format_tui_welcome(env!("CARGO_PKG_VERSION"), &config.agent.model, &working_dir);

    tui.root_mut().add_child(Box::new(Text::new(welcome, 1, 1)));
    tui.root_mut().add_child(Box::new(Container::new())); // chat container
    tui.root_mut().add_child(Box::new(Spacer::new(0))); // dynamic spacer
    tui.root_mut().add_child(Box::new(StatusLine::new()));
    tui.root_mut().add_child(Box::new(Editor::new()));
    let mut footer = Footer::new(&working_dir, &config.agent.model, 200_000);
    footer.set_git_branch(git_branch);
    tui.root_mut().add_child(Box::new(footer));

    // Start the TUI
    tui.start()?;
    tui.request_render();
    tui.render_if_needed()?;

    let mut tool_names: HashMap<String, String> = HashMap::new();

    // Channel for receiving TurnEvents from the gateway
    let (event_tx, mut event_rx) = mpsc::channel::<TurnEvent>(128);
    // Track whether a gateway task is running
    let mut turn_task: Option<tokio::task::JoinHandle<Result<()>>> = None;

    // Main event loop
    loop {
        let mut needs_render = false;

        // 1. Receive TurnEvents from gateway
        while let Ok(event) = event_rx.try_recv() {
            needs_render = true;
            match event {
                TurnEvent::TextDelta(text) => {
                    app.append_or_create_assistant(&text);
                    update_chat_messages(&mut tui, &app, CHAT_IDX);
                }
                TurnEvent::ToolStart {
                    id,
                    name,
                    arguments,
                } => {
                    tool_names.insert(id, name.clone());
                    app.push_message(DisplayMessage::tool_call(&name, &arguments));
                    update_chat_messages(&mut tui, &app, CHAT_IDX);
                }
                TurnEvent::ToolResult { id, message } => {
                    let name = tool_names
                        .get(&id)
                        .cloned()
                        .unwrap_or_else(|| "unknown".to_string());
                    let (output, is_error) = extract_tool_result(&message);
                    app.push_message(DisplayMessage::tool_output(&name, output, is_error));
                    update_chat_messages(&mut tui, &app, CHAT_IDX);
                }
                TurnEvent::AssistantMessage(_) => {
                    // Full message already handled via TextDelta streaming
                }
                TurnEvent::Done(result) => {
                    let tokens = result.usage.total_tokens();
                    app.end_turn(tokens);
                    let status = tui.root_mut().children_mut()[STATUS_IDX]
                        .as_any_mut()
                        .and_then(|a| a.downcast_mut::<StatusLine>());
                    if let Some(s) = status {
                        s.set_loading(false);
                        s.set_error(None);
                    }
                    let footer = tui.root_mut().children_mut()[FOOTER_IDX]
                        .as_any_mut()
                        .and_then(|a| a.downcast_mut::<Footer>());
                    if let Some(f) = footer {
                        f.set_usage(0, 0, 0, 0, tokens);
                    }
                    update_chat_messages(&mut tui, &app, CHAT_IDX);
                }
                TurnEvent::Error(message) => {
                    app.push_message(DisplayMessage::system(format!("Error: {message}")));
                    if app.is_loading {
                        app.end_turn(0);
                    }
                    let status = tui.root_mut().children_mut()[STATUS_IDX]
                        .as_any_mut()
                        .and_then(|a| a.downcast_mut::<StatusLine>());
                    if let Some(s) = status {
                        s.set_loading(false);
                        s.set_error(Some(message));
                    }
                    update_chat_messages(&mut tui, &app, CHAT_IDX);
                }
            }
        }

        // Check if the turn task completed (and handle any errors)
        if let Some(ref task) = turn_task
            && task.is_finished()
            && let Some(task) = turn_task.take()
        {
            match task.await {
                Ok(Ok(())) => {}
                Ok(Err(error)) => {
                    app.push_message(DisplayMessage::system(format!("Error: {error:#}")));
                    if app.is_loading {
                        app.end_turn(0);
                    }
                    let status = tui.root_mut().children_mut()[STATUS_IDX]
                        .as_any_mut()
                        .and_then(|a| a.downcast_mut::<StatusLine>());
                    if let Some(s) = status {
                        s.set_loading(false);
                        s.set_error(Some(error.to_string()));
                    }
                    update_chat_messages(&mut tui, &app, CHAT_IDX);
                    needs_render = true;
                }
                Err(error) => {
                    app.push_message(DisplayMessage::system(format!(
                        "Error: task panicked: {error}"
                    )));
                    if app.is_loading {
                        app.end_turn(0);
                    }
                    needs_render = true;
                }
            }
        }

        // 2. Poll for keyboard/mouse events
        if let Some(event) = poll_event(Duration::from_millis(50)) {
            if let Event::Key(key_event) = event {
                app.clear_error();

                match handle_key_event(&mut app, key_event) {
                    InputAction::Submit(input) => {
                        if app.is_loading {
                            app.input = input;
                            app.cursor_pos = app.input.len();
                            app.set_error("Cannot send while agent is responding");
                            let status = tui.root_mut().children_mut()[STATUS_IDX]
                                .as_any_mut()
                                .and_then(|a| a.downcast_mut::<StatusLine>());
                            if let Some(s) = status {
                                s.set_error(app.error_message.clone());
                            }
                        } else {
                            // Clear editor
                            let editor = tui.root_mut().children_mut()[EDITOR_IDX]
                                .as_any_mut()
                                .and_then(|a| a.downcast_mut::<Editor>());
                            if let Some(e) = editor {
                                e.clear();
                            }

                            app.push_message(DisplayMessage::user(&input));
                            app.start_turn();

                            // Update status
                            let status = tui.root_mut().children_mut()[STATUS_IDX]
                                .as_any_mut()
                                .and_then(|a| a.downcast_mut::<StatusLine>());
                            if let Some(s) = status {
                                s.set_loading(true);
                            }

                            update_chat_messages(&mut tui, &app, CHAT_IDX);

                            // Spawn gateway turn via TurnEvent channel
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
                            // Clear chat container
                            let chat = tui.root_mut().children_mut()[CHAT_IDX]
                                .as_any_mut()
                                .and_then(|a| a.downcast_mut::<Container>());
                            if let Some(c) = chat {
                                c.clear();
                            }
                            tui.force_render();

                            // Clear gateway session
                            gateway.clear_session(&session_key);
                        }
                    }
                    InputAction::ToggleVerbose => {
                        app.toggle_verbose();
                        update_chat_messages(&mut tui, &app, CHAT_IDX);
                    }
                    InputAction::None => {}
                }

                // Sync editor state from app
                sync_editor_from_app(&mut tui, &app, EDITOR_IDX);
                needs_render = true;
            }
        } else {
            // Tick loading animation
            app.tick_loading();
            app.tick_error();

            let status = tui.root_mut().children_mut()[STATUS_IDX]
                .as_any_mut()
                .and_then(|a| a.downcast_mut::<StatusLine>());
            if let Some(s) = status {
                s.tick();
                s.set_elapsed(&app.elapsed_text());
                if app.is_loading || app.error_message.is_some() {
                    needs_render = true;
                }
            }
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
    let db_path = resolve_config_path(&config_dir, &signal.db_path);

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
// TUI helpers
// ---------------------------------------------------------------------------

/// Rebuild the chat container from the app's message list.
#[allow(clippy::too_many_lines)]
fn update_chat_messages(tui: &mut Tui, app: &App, chat_idx: usize) {
    let chat = tui.root_mut().children_mut()[chat_idx]
        .as_any_mut()
        .and_then(|a| a.downcast_mut::<Container>());
    let Some(chat) = chat else { return };
    chat.clear();

    for msg in &app.messages {
        match &msg.role {
            coop_tui::DisplayRole::User => {
                let md =
                    MarkdownComponent::new(msg.content.clone(), 1, 1).with_bg(0x34, 0x35, 0x41);
                chat.add_child(Box::new(md));
            }
            coop_tui::DisplayRole::Assistant => {
                let md = MarkdownComponent::new(msg.content.clone(), 1, 1);
                chat.add_child(Box::new(md));
            }
            coop_tui::DisplayRole::System => {
                let styled = coop_tui::utils::fg_rgb(0x80, 0x80, 0x80, &msg.content);
                chat.add_child(Box::new(Text::new(styled, 1, 0)));
            }
            coop_tui::DisplayRole::ToolCall { name, .. } => {
                if !app.verbose {
                    continue;
                }
                let (icon, verb) = tool_label(name);
                let header = if verb == "Run" {
                    format!("{icon} {verb} {name}")
                } else {
                    format!("{icon} {verb}")
                };
                let content = format!(
                    "{}\n{}",
                    coop_tui::utils::bold(&coop_tui::utils::fg_rgb(0xff, 0xff, 0x00, &header)),
                    coop_tui::utils::fg_rgb(0x50, 0x50, 0x50, &msg.content)
                );
                let mut tb = ToolBox::new(1, 1).with_bg(0x28, 0x28, 0x32);
                tb.set_lines(vec![content]);
                chat.add_child(Box::new(tb));
            }
            coop_tui::DisplayRole::ToolOutput { name, is_error } => {
                if !app.verbose {
                    continue;
                }
                let bg = if *is_error {
                    (0x3c, 0x28, 0x28)
                } else {
                    (0x28, 0x32, 0x28)
                };
                let text_color = if *is_error {
                    (0xcc, 0x66, 0x66)
                } else {
                    (0x80, 0x80, 0x80)
                };

                let content_lines: Vec<&str> = msg.content.lines().collect();
                let display = if content_lines.len() > 20 {
                    let mut lines = Vec::new();
                    for l in &content_lines[..10] {
                        lines.push(coop_tui::utils::fg_rgb(
                            text_color.0,
                            text_color.1,
                            text_color.2,
                            l,
                        ));
                    }
                    lines.push(coop_tui::utils::fg_rgb(
                        0x80,
                        0x80,
                        0x80,
                        &format!(
                            "... ({} earlier lines, ctrl+o to expand)",
                            content_lines.len() - 20
                        ),
                    ));
                    for l in &content_lines[content_lines.len() - 10..] {
                        lines.push(coop_tui::utils::fg_rgb(
                            text_color.0,
                            text_color.1,
                            text_color.2,
                            l,
                        ));
                    }
                    lines
                } else {
                    content_lines
                        .iter()
                        .map(|l| {
                            coop_tui::utils::fg_rgb(text_color.0, text_color.1, text_color.2, l)
                        })
                        .collect()
                };

                let _ = name;
                let mut tb = ToolBox::new(1, 1).with_bg(bg.0, bg.1, bg.2);
                tb.set_lines(display);
                chat.add_child(Box::new(tb));
            }
        }
    }
}

fn sync_editor_from_app(tui: &mut Tui, app: &App, editor_idx: usize) {
    let editor = tui.root_mut().children_mut()[editor_idx]
        .as_any_mut()
        .and_then(|a| a.downcast_mut::<Editor>());
    if let Some(e) = editor {
        e.set_text(&app.input);
    }
}

fn tool_label(name: &str) -> (&'static str, &'static str) {
    match name {
        "bash" => ("⚡", "Execute"),
        "read_file" | "Read" => ("📄", "Read"),
        "write_file" | "Write" => ("✏️", "Write"),
        "list_directory" => ("📂", "List"),
        _ => ("🔧", "Run"),
    }
}

/// Extract (output, is_error) from a TurnEvent::ToolResult message.
fn extract_tool_result(message: &coop_core::Message) -> (String, bool) {
    message
        .content
        .iter()
        .find_map(|content| match content {
            Content::ToolResult {
                output, is_error, ..
            } => Some((output.clone(), *is_error)),
            _ => None,
        })
        .unwrap_or_else(|| (message.text(), false))
}

/// Format the welcome banner with ANSI colors.
fn format_tui_welcome(version: &str, model: &str, working_dir: &str) -> String {
    let lc = coop_tui::theme::fg_code(coop_tui::theme::MD_HEADING);
    let ic = coop_tui::theme::fg_code(coop_tui::theme::MUTED);
    let bc = "\x1b[1m\x1b[37m"; // bold white
    let r = coop_tui::theme::RESET;
    format!(
        "\
{lc}  ████████      {r}
{lc}██▓▓▓▓▓▓▓▓██    {r}{bc}Coop v{version}{r}
{lc}██▓▓▓▓▓▓▓▓██    {r}{ic}{model}{r}
{lc}██▓▓██▓▓██▓▓██  {r}{ic}{working_dir}{r}
{lc}██▓▓▓▓▓▓▓▓██    {r}
{lc}  ████████      {r}"
    )
}

fn detect_git_branch() -> Option<String> {
    std::process::Command::new("git")
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
}

#[cfg(feature = "signal")]
fn resolve_config_path(base_dir: &Path, configured_path: &str) -> PathBuf {
    let path = PathBuf::from(configured_path);
    if path.is_absolute() {
        path
    } else {
        base_dir.join(path)
    }
}
