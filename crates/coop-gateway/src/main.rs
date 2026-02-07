mod config;
mod gateway;
mod router;
mod trust;

use anyhow::{Context, Result};
use chrono::Utc;
use clap::{Parser, Subcommand};
use coop_agent::AnthropicProvider;
use coop_core::tools::DefaultExecutor;
use coop_core::{InboundMessage, Provider};
use coop_ipc::{
    ClientMessage, IpcClient, IpcConnection, IpcServer, PROTOCOL_VERSION, ServerMessage,
    socket_path,
};
use coop_tui::{App, DisplayMessage, InputAction, handle_key_event, poll_event};
use crossterm::{
    event::Event,
    terminal::{disable_raw_mode, enable_raw_mode},
};
use ratatui::backend::CrosstermBackend;
use ratatui::{Terminal, TerminalOptions, Viewport};
use std::collections::HashMap;
use std::io;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use tracing::info;

use crate::config::Config;
use crate::gateway::Gateway;
use crate::router::MessageRouter;

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
        Commands::Signal { command } => cmd_signal(cli.config.as_deref(), command),
        Commands::Version => {
            println!("🐔 coop {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
    }
}

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

    let socket = socket_path(&config.agent.id);
    let server = IpcServer::bind(&socket)?;

    if config.channels.signal.is_some() {
        info!("signal channel configured (link/unlink command available)");
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
            ClientMessage::Clear { session } => match gateway.find_session(&session) {
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

#[allow(clippy::too_many_lines)]
async fn cmd_chat(config_path: Option<&str>) -> Result<()> {
    let config_file = Config::find_config_path(config_path);
    let config = Config::load(&config_file)
        .with_context(|| format!("loading config from {}", config_file.display()))?;

    let socket = socket_path(&config.agent.id);
    let client = IpcClient::connect(&socket).await.with_context(|| {
        format!(
            "could not connect to gateway at {} (run 'coop start' first)",
            socket.display()
        )
    })?;
    let (mut reader, mut writer) = client.into_split();

    writer
        .send(ClientMessage::Hello {
            version: PROTOCOL_VERSION,
        })
        .await?;
    writer
        .send(ClientMessage::Subscribe {
            session: "main".to_string(),
        })
        .await?;

    let (server_tx, mut server_rx) = mpsc::channel::<ServerMessage>(128);
    tokio::spawn(async move {
        loop {
            match reader.recv().await {
                Ok(message) => {
                    if server_tx.send(message).await.is_err() {
                        return;
                    }
                }
                Err(error) => {
                    let _ = server_tx
                        .send(ServerMessage::Error {
                            session: "main".to_string(),
                            message: format!("disconnected: {error}"),
                        })
                        .await;
                    return;
                }
            }
        }
    });

    enable_raw_mode()?;
    let stdout = io::stdout();
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::with_options(
        backend,
        TerminalOptions {
            viewport: Viewport::Inline(coop_tui::ui::VIEWPORT_HEIGHT),
        },
    )?;

    let mut app = App::new(&config.agent.id, &config.agent.model, "main", 200_000);
    app.connection_status = "connecting".to_string();
    app.version = env!("CARGO_PKG_VERSION").to_string();

    let cwd = std::env::current_dir()
        .map(|path| path.display().to_string())
        .unwrap_or_default();
    let home = std::env::var("HOME").unwrap_or_default();
    app.working_dir = if !home.is_empty() && cwd.starts_with(&home) {
        format!("~{}", &cwd[home.len()..])
    } else {
        cwd
    };

    let welcome_lines =
        coop_tui::ui::format_welcome_header(&app.version, &app.model_name, &app.working_dir);
    let welcome_width = terminal.size()?.width;
    #[allow(clippy::cast_possible_truncation)]
    let welcome_height = welcome_lines.len() as u16;
    terminal.insert_before(welcome_height, |buf| {
        coop_tui::ui::render_scrollback(&welcome_lines, welcome_width, buf);
    })?;

    let mut tool_names: HashMap<String, String> = HashMap::new();
    let mut stream_prefix_printed = false;
    let mut turn_just_ended = false;

    loop {
        while let Ok(message) = server_rx.try_recv() {
            match message {
                ServerMessage::Hello { .. } => {
                    app.connection_status = "connected".to_string();
                }
                ServerMessage::TextDelta { text, .. } => {
                    app.append_or_create_assistant(&text);
                }
                ServerMessage::ToolStart {
                    id,
                    name,
                    arguments,
                    ..
                } => {
                    tool_names.insert(id, name.clone());
                    app.push_message(DisplayMessage::tool_call(&name, &arguments));
                }
                ServerMessage::ToolResult {
                    id,
                    output,
                    is_error,
                    ..
                } => {
                    let name = tool_names
                        .get(&id)
                        .cloned()
                        .unwrap_or_else(|| "unknown".to_string());
                    app.push_message(DisplayMessage::tool_output(&name, output, is_error));
                }
                ServerMessage::AssistantMessage { .. } | ServerMessage::Sessions { .. } => {}
                ServerMessage::Done { tokens, .. } => {
                    app.end_turn(tokens);
                    turn_just_ended = true;
                }
                ServerMessage::Error { message, .. } => {
                    if message.starts_with("disconnected") {
                        app.connection_status = "disconnected".to_string();
                    }
                    app.push_message(DisplayMessage::system(format!("Error: {message}")));
                    if app.is_loading {
                        app.end_turn(0);
                        turn_just_ended = true;
                    }
                }
            }
        }

        let stream_lines = app.take_stream_lines();
        for line in &stream_lines {
            if !stream_prefix_printed {
                print_stream_prefix(&mut terminal, &app)?;
                stream_prefix_printed = true;
            }
            terminal.insert_before(1, |buf| {
                let area = padded_area(buf.area);
                let span = ratatui::text::Line::from(ratatui::text::Span::styled(
                    format!("  {line}"),
                    ratatui::style::Style::default().fg(ratatui::style::Color::White),
                ));
                ratatui::widgets::Widget::render(ratatui::widgets::Paragraph::new(span), area, buf);
            })?;
        }

        if turn_just_ended {
            if let Some(partial) = app.flush_stream_buf() {
                if !stream_prefix_printed && !partial.is_empty() {
                    print_stream_prefix(&mut terminal, &app)?;
                }
                if !partial.is_empty() {
                    terminal.insert_before(1, |buf| {
                        let area = padded_area(buf.area);
                        let span = ratatui::text::Line::from(ratatui::text::Span::styled(
                            format!("  {partial}"),
                            ratatui::style::Style::default().fg(ratatui::style::Color::White),
                        ));
                        ratatui::widgets::Widget::render(
                            ratatui::widgets::Paragraph::new(span),
                            area,
                            buf,
                        );
                    })?;
                }
            }
            stream_prefix_printed = false;
            turn_just_ended = false;
        }

        flush_to_scrollback(&mut terminal, &mut app)?;
        terminal.draw(|frame| coop_tui::ui::draw(frame, &app))?;

        if let Some(event) = poll_event(Duration::from_millis(50)) {
            if let Event::Key(key_event) = event {
                app.clear_error();

                match handle_key_event(&mut app, key_event) {
                    InputAction::Submit(input) => {
                        if app.is_loading {
                            app.input = input;
                            app.cursor_pos = app.input.len();
                            app.set_error("Cannot send while agent is responding");
                        } else {
                            app.push_message(DisplayMessage::user(&input));
                            app.start_turn();

                            if let Err(error) = writer
                                .send(ClientMessage::Send {
                                    session: "main".to_string(),
                                    content: input,
                                })
                                .await
                            {
                                app.push_message(DisplayMessage::system(format!(
                                    "Error: {error:#}"
                                )));
                                app.connection_status = "disconnected".to_string();
                                app.end_turn(0);
                                turn_just_ended = true;
                            }
                        }
                    }
                    InputAction::Quit => {
                        app.should_quit = true;
                    }
                    InputAction::Clear => {
                        if !app.is_loading {
                            app.clear();
                            if let Err(error) = writer
                                .send(ClientMessage::Clear {
                                    session: "main".to_string(),
                                })
                                .await
                            {
                                app.push_message(DisplayMessage::system(format!(
                                    "Error: {error:#}"
                                )));
                                app.connection_status = "disconnected".to_string();
                            }
                        }
                    }
                    InputAction::ToggleVerbose => {
                        app.toggle_verbose();
                    }
                    InputAction::None => {}
                }
            }
        } else {
            app.tick_loading();
            app.tick_error();
        }

        if app.should_quit {
            break;
        }
    }

    flush_to_scrollback(&mut terminal, &mut app)?;
    disable_raw_mode()?;
    terminal.show_cursor()?;

    println!("👋 Goodbye!");
    Ok(())
}

fn cmd_signal(config_path: Option<&str>, command: SignalCommands) -> Result<()> {
    #[cfg(feature = "signal")]
    {
        cmd_signal_enabled(config_path, command)
    }

    #[cfg(not(feature = "signal"))]
    {
        let _ = (config_path, command);
        anyhow::bail!("signal support is not enabled in this build")
    }
}

#[cfg(feature = "signal")]
fn cmd_signal_enabled(config_path: Option<&str>, command: SignalCommands) -> Result<()> {
    let config_file = Config::find_config_path(config_path);
    let config = Config::load(&config_file)
        .with_context(|| format!("loading config from {}", config_file.display()))?;

    let signal = config
        .channels
        .signal
        .ok_or_else(|| anyhow::anyhow!("signal channel is not configured in coop.yaml"))?;
    let db_path = PathBuf::from(signal.db_path);

    match command {
        SignalCommands::Link { device_name } => {
            if let Some(parent) = db_path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(
                &db_path,
                format!("linked_device={device_name}\nlinked_at={}\n", Utc::now()),
            )?;
            println!(
                "linked signal device '{device_name}' using {}",
                db_path.display()
            );
        }
        SignalCommands::Unlink => {
            if db_path.exists() {
                std::fs::remove_file(&db_path)?;
                println!("removed signal registration at {}", db_path.display());
            } else {
                println!("no signal registration at {}", db_path.display());
            }
        }
    }

    Ok(())
}

fn flush_to_scrollback(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    app: &mut App,
) -> Result<()> {
    let drained = app.drain_flushed();
    if drained.is_empty() {
        return Ok(());
    }

    let lines = coop_tui::ui::format_messages(&drained, &app.agent_name, app.verbose);
    if lines.is_empty() {
        return Ok(());
    }

    let width = terminal.size()?.width;
    #[allow(clippy::cast_possible_truncation)]
    let height = lines.len() as u16;

    terminal.insert_before(height, |buf| {
        coop_tui::ui::render_scrollback(&lines, width, buf);
    })?;

    Ok(())
}

fn print_stream_prefix(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    app: &App,
) -> Result<()> {
    let time = chrono::Local::now().format("%H:%M").to_string();
    let prefix = format!("\n{time} {}: ", app.agent_name);

    terminal.insert_before(1, |buf| {
        let area = padded_area(buf.area);
        let span = ratatui::text::Line::from(ratatui::text::Span::styled(
            prefix,
            ratatui::style::Style::default().fg(ratatui::style::Color::White),
        ));
        ratatui::widgets::Widget::render(ratatui::widgets::Paragraph::new(span), area, buf);
    })?;

    Ok(())
}

fn padded_area(area: ratatui::layout::Rect) -> ratatui::layout::Rect {
    let pad = coop_tui::ui::SIDE_PADDING;
    ratatui::layout::Rect::new(pad, area.y, area.width.saturating_sub(pad * 2), area.height)
}
