mod config;
mod gateway;
#[allow(dead_code)]
mod router;
#[allow(dead_code)]
mod trust;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use coop_agent::AnthropicProvider;
use coop_core::tools::DefaultExecutor;
use coop_core::{Provider, TurnEvent};
use coop_tui::{App, DisplayMessage, InputAction, handle_key_event, poll_event};
use crossterm::{
    event::{Event, KeyEvent},
    terminal::{disable_raw_mode, enable_raw_mode},
};
use ratatui::backend::CrosstermBackend;
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
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

#[derive(Parser)]
#[command(name = "coop", version, about = "🐔 Coop — Personal Agent Gateway")]
struct Cli {
    #[command(subcommand)]
    command: Commands,

    /// Path to config file
    #[arg(short, long, global = true)]
    config: Option<String>,
}

#[derive(Subcommand)]
enum Commands {
    /// Start the gateway daemon (foreground)
    Start,
    /// Open terminal TUI connected to the gateway
    Chat,
    /// Print version info
    Version,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    // Initialize tracing
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_target(false)
        .init();

    match cli.command {
        Commands::Start => cmd_start(cli.config.as_deref()).await,
        Commands::Chat => cmd_chat(cli.config.as_deref()),
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

    info!(
        agent = %config.agent.id,
        model = %config.agent.model,
        "gateway starting"
    );

    // Just wait for shutdown signal
    info!("gateway running. press ctrl-c to stop.");
    tokio::signal::ctrl_c().await?;
    info!("shutting down");
    Ok(())
}

#[allow(clippy::too_many_lines)]
fn cmd_chat(config_path: Option<&str>) -> Result<()> {
    let config_file = Config::find_config_path(config_path);

    // Resolve config file relative to its directory for system prompt paths
    let config_dir = config_file
        .parent()
        .unwrap_or(&PathBuf::from("."))
        .to_path_buf();

    let config = Config::load(&config_file)
        .with_context(|| format!("loading config from {}", config_file.display()))?;

    let system_prompt = config.build_system_prompt(&config_dir)?;

    // Create the provider
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
    let gw = Arc::new(Gateway::new(
        config.clone(),
        system_prompt,
        provider,
        executor,
    ));
    let session_key = gw.default_session_key();

    // Set up TUI with inline viewport (content persists in scrollback)
    enable_raw_mode()?;
    let stdout = io::stdout();
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::with_options(
        backend,
        TerminalOptions {
            viewport: Viewport::Inline(coop_tui::ui::VIEWPORT_HEIGHT),
        },
    )?;

    let session_name = format!("{:?}", session_key.kind).to_lowercase();
    let mut app = App::new(&config.agent.id, &config.agent.model, session_name, 200_000);
    app.connection_status = "connected".to_string();
    app.push_message(DisplayMessage::system(format!(
        "Connected to {} ({}). Type a message or /quit to exit.",
        config.agent.id, config.agent.model
    )));

    // Track whether we've printed the assistant prefix for the current streaming message
    let mut stream_prefix_printed = false;

    // Track tool names by ID for correlating ToolResult events
    let mut tool_names: HashMap<String, String> = HashMap::new();

    // Channel for async turn events
    let (event_tx, mut event_rx) = mpsc::channel::<TurnEvent>(64);

    // Main event loop
    loop {
        // Check for async turn events
        while let Ok(turn_event) = event_rx.try_recv() {
            match turn_event {
                TurnEvent::TextDelta(text) => {
                    app.append_or_create_assistant(&text);
                }
                TurnEvent::AssistantMessage(_) => {}
                TurnEvent::ToolStart {
                    id,
                    name,
                    arguments,
                } => {
                    tool_names.insert(id, name.clone());
                    app.push_message(DisplayMessage::tool_call(&name, &arguments));
                }
                TurnEvent::ToolResult { id, message } => {
                    let name = tool_names
                        .get(&id)
                        .cloned()
                        .unwrap_or_else(|| "unknown".to_string());
                    let (output, is_error) = message
                        .content
                        .iter()
                        .find_map(|c| match c {
                            coop_core::Content::ToolResult {
                                output, is_error, ..
                            } => Some((output.clone(), *is_error)),
                            _ => None,
                        })
                        .unwrap_or_else(|| (message.text(), false));
                    app.push_message(DisplayMessage::tool_output(&name, output, is_error));
                }
                TurnEvent::Done(result) => {
                    app.end_turn(result.usage.total_tokens());
                }
                TurnEvent::Error(err) => {
                    app.push_message(DisplayMessage::system(format!("Error: {err}")));
                    app.end_turn(0);
                }
            }
        }

        // Print streaming text (complete lines only) above the viewport
        if let Some(lines_text) = app.take_stream_lines() {
            if !stream_prefix_printed {
                print_stream_prefix(&mut terminal, &app)?;
                stream_prefix_printed = true;
            }
            let lines: Vec<Line<'static>> = lines_text
                .lines()
                .map(|s| {
                    Line::from(Span::styled(
                        s.to_string(),
                        Style::default().fg(Color::White),
                    ))
                })
                .collect();
            if !lines.is_empty() {
                print_above(&mut terminal, &lines)?;
            }
        }

        // When turn ends, flush remaining partial line before draining messages
        if !app.is_loading && app.assistant_streamed {
            if let Some(remainder) = app.flush_stream_buf()
                && !remainder.is_empty()
            {
                if !stream_prefix_printed {
                    print_stream_prefix(&mut terminal, &app)?;
                }
                print_above(
                    &mut terminal,
                    &[Line::from(Span::styled(
                        remainder,
                        Style::default().fg(Color::White),
                    ))],
                )?;
            }
            stream_prefix_printed = false;
        }

        // Flush completed messages to scrollback above the viewport
        flush_to_scrollback(&mut terminal, &mut app)?;

        // Draw the viewport (input + status bars only)
        terminal.draw(|f| coop_tui::ui::draw(f, &app))?;

        // Poll for input events
        if let Some(event) = poll_event(Duration::from_millis(50)) {
            if let Event::Key(key_event) = event {
                // Don't accept input while loading
                if app.is_loading && !is_quit_key(&key_event) {
                    continue;
                }

                match handle_key_event(&mut app, key_event) {
                    InputAction::Submit(input) => {
                        app.push_message(DisplayMessage::user(&input));
                        app.start_turn();

                        // Spawn async task for agent turn
                        let gw = gw.clone();
                        let sk = session_key.clone();
                        let tx = event_tx.clone();
                        tokio::spawn(async move {
                            if let Err(e) = gw.run_turn(&sk, &input, tx.clone()).await {
                                let _ = tx.send(TurnEvent::Error(format!("{e:#}"))).await;
                            }
                        });
                    }
                    InputAction::Quit => {
                        app.should_quit = true;
                    }
                    InputAction::Clear => {
                        app.clear();
                    }
                    InputAction::ToggleVerbose => {
                        app.toggle_verbose();
                    }
                    InputAction::None => {}
                }
            }
        } else {
            // Tick loading animation
            app.tick_loading();
        }

        if app.should_quit {
            break;
        }
    }

    // Flush remaining messages before exit
    flush_to_scrollback(&mut terminal, &mut app)?;

    // Restore terminal (no LeaveAlternateScreen — content is already in scrollback)
    disable_raw_mode()?;
    terminal.show_cursor()?;

    println!("👋 Goodbye!");
    Ok(())
}

/// Flush completed messages from the app into terminal scrollback via `insert_before`.
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

    print_above(terminal, &lines)
}

/// Print styled lines above the viewport via `insert_before`.
fn print_above(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    lines: &[Line<'static>],
) -> Result<()> {
    let width = terminal.size()?.width;
    #[allow(clippy::cast_possible_truncation)]
    let height = lines.len() as u16;
    terminal.insert_before(height, |buf| {
        coop_tui::ui::render_scrollback(lines, width, buf);
    })?;
    Ok(())
}

/// Print the assistant name prefix before streamed content.
fn print_stream_prefix(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    app: &App,
) -> Result<()> {
    let prefix = format!(
        "\n{} {}: ",
        app.messages
            .last()
            .map(DisplayMessage::local_time)
            .unwrap_or_default(),
        app.agent_name
    );
    print_above(
        terminal,
        &[Line::from(Span::styled(
            prefix,
            Style::default().fg(Color::White),
        ))],
    )
}

fn is_quit_key(key: &KeyEvent) -> bool {
    matches!(
        (key.modifiers, key.code),
        (
            crossterm::event::KeyModifiers::CONTROL,
            crossterm::event::KeyCode::Char('c')
        )
    )
}
