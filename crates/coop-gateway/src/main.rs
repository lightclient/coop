mod config;
mod gateway;
mod router;
mod trust;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use coop_agent::GooseRuntime;
use coop_tui::{App, DisplayMessage, InputAction, handle_key_event, poll_event};
use crossterm::{
    event::{Event, KeyEvent},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;
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
        Commands::Chat => cmd_chat(cli.config.as_deref()).await,
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

async fn cmd_chat(config_path: Option<&str>) -> Result<()> {
    let config_file = Config::find_config_path(config_path);

    // Resolve config file relative to its directory for system prompt paths
    let config_dir = config_file
        .parent()
        .unwrap_or(&PathBuf::from("."))
        .to_path_buf();

    let config = Config::load(&config_file)
        .with_context(|| format!("loading config from {}", config_file.display()))?;

    let system_prompt = config.build_system_prompt(&config_dir)?;

    // Create the Goose runtime
    let runtime = Arc::new(
        GooseRuntime::new(&config.agent.model, &config.provider.name)
            .context("failed to initialize Goose runtime")?,
    );

    let gw = Arc::new(Gateway::new(config.clone(), system_prompt, runtime));
    let session_key = gw.default_session_key();

    // Set up TUI
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let mut app = App::new(&config.agent.id, &config.agent.model);
    app.push_message(DisplayMessage::system(format!(
        "Connected to {} ({}). Type a message or /quit to exit.",
        config.agent.id, config.agent.model
    )));

    // Channel for async responses
    let (response_tx, mut response_rx) = mpsc::channel::<Result<String, String>>(16);

    // Main event loop
    loop {
        // Draw
        terminal.draw(|f| coop_tui::ui::draw(f, &app))?;

        // Check for async responses
        while let Ok(result) = response_rx.try_recv() {
            app.is_loading = false;
            match result {
                Ok(content) => {
                    app.push_message(DisplayMessage::assistant(content));
                }
                Err(err) => {
                    app.push_message(DisplayMessage::system(format!("Error: {}", err)));
                }
            }
        }

        // Poll for input events
        if let Some(event) = poll_event(Duration::from_millis(50)) {
            match event {
                Event::Key(key_event) => {
                    // Don't accept input while loading
                    if app.is_loading && !is_quit_key(&key_event) {
                        continue;
                    }

                    match handle_key_event(&mut app, key_event) {
                        InputAction::Submit(input) => {
                            app.push_message(DisplayMessage::user(&input));
                            app.is_loading = true;

                            // Spawn async task for agent turn
                            let gw = gw.clone();
                            let sk = session_key.clone();
                            let tx = response_tx.clone();
                            tokio::spawn(async move {
                                let result = gw.handle_message(&sk, &input).await;
                                let _ = tx
                                    .send(result.map_err(|e| format!("{:#}", e)))
                                    .await;
                            });
                        }
                        InputAction::Quit => {
                            app.should_quit = true;
                        }
                        InputAction::Clear => {
                            app.clear();
                        }
                        InputAction::None => {}
                    }
                }
                Event::Resize(_, _) => {
                    // Terminal will re-render on next loop
                }
                _ => {}
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
        (crossterm::event::KeyModifiers::CONTROL, crossterm::event::KeyCode::Char('c'))
    )
}
