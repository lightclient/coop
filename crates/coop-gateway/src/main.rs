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
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
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

    // Set up TUI
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let session_name = format!("{:?}", session_key.kind).to_lowercase();
    let mut app = App::new(&config.agent.id, &config.agent.model, session_name, 200_000);
    app.connection_status = "connected".to_string();
    app.push_message(DisplayMessage::system(format!(
        "Connected to {} ({}). Type a message or /quit to exit.",
        config.agent.id, config.agent.model
    )));

    // Track tool names by ID for correlating ToolResult events
    let mut tool_names: HashMap<String, String> = HashMap::new();

    // Channel for async turn events
    let (event_tx, mut event_rx) = mpsc::channel::<TurnEvent>(64);

    // Main event loop
    loop {
        // Draw
        terminal.draw(|f| coop_tui::ui::draw(f, &app))?;

        // Check for async turn events
        while let Ok(turn_event) = event_rx.try_recv() {
            match turn_event {
                TurnEvent::TextDelta(text) => {
                    app.append_or_create_assistant(&text);
                }
                TurnEvent::AssistantMessage(_) => {}
                TurnEvent::ToolStart { id, name } => {
                    tool_names.insert(id, name.clone());
                    app.push_message(DisplayMessage::tool_call(&name));
                }
                TurnEvent::ToolResult { id, message } => {
                    let name = tool_names
                        .get(&id)
                        .cloned()
                        .unwrap_or_else(|| "unknown".to_string());
                    let output = message.text();
                    app.push_message(DisplayMessage::tool_output(&name, output));
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

    // Restore terminal
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;

    println!("👋 Goodbye!");
    Ok(())
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
