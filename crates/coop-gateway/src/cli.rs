use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "coop", version, about = "🐔 Coop — Personal Agent Gateway")]
pub(crate) struct Cli {
    #[command(subcommand)]
    pub command: Commands,

    #[arg(short, long, global = true)]
    pub config: Option<String>,
}

#[derive(Subcommand)]
pub(crate) enum Commands {
    Init {
        /// Directory to initialize (default: ~/.coop)
        #[arg(short, long)]
        dir: Option<String>,
    },
    Start,
    Gateway {
        #[command(subcommand)]
        command: GatewayCommands,
    },
    Check {
        /// Output format: human (default) or json
        #[arg(long, default_value = "human")]
        format: String,
    },
    Chat {
        /// User to load as (defaults to first user in config).
        #[arg(short, long)]
        user: Option<String>,
    },
    Attach {
        #[arg(short, long, default_value = "main")]
        session: String,
    },
    Signal {
        #[command(subcommand)]
        command: SignalCommands,
    },
    Memory {
        #[command(subcommand)]
        command: MemoryCommands,
    },
    Version,
}

#[derive(Subcommand)]
pub(crate) enum GatewayCommands {
    Install {
        /// Extra environment variables to persist for the gateway service (KEY=VALUE).
        /// API key variables (ANTHROPIC_API_KEY, etc.) are captured automatically
        /// from the current environment if set.
        #[arg(long = "env", value_name = "KEY=VALUE")]
        envs: Vec<String>,

        /// Override COOP_TRACE_FILE for the installed service.
        #[arg(long, value_name = "PATH")]
        trace_file: Option<String>,

        /// Override COOP_TRACE_MAX_SIZE for the installed service.
        #[arg(long, value_name = "BYTES")]
        trace_max_size: Option<u64>,

        /// Override RUST_LOG for the installed service.
        #[arg(long, value_name = "FILTER")]
        rust_log: Option<String>,

        /// Install + enable, but do not start immediately.
        #[arg(long)]
        no_start: bool,

        /// Print generated service config/script instead of installing.
        /// Useful on systems without systemd/launchd (OpenRC, runit, etc.)
        /// or when you want to customize files before installation.
        #[arg(long)]
        print: bool,

        /// When used with --print, include real secret values in env previews.
        /// Default is redacted output.
        #[arg(long, requires = "print")]
        print_secrets: bool,
    },
    Uninstall,
    Start,
    Stop,
    Restart,
    Rollback {
        /// Backup config path to restore. Defaults to `<config>.bak` layout
        /// used by config_write (`coop.toml.bak`).
        #[arg(long, value_name = "PATH")]
        backup: Option<String>,

        /// Restore config only. Do not restart gateway.
        #[arg(long)]
        no_restart: bool,

        /// Seconds to wait for gateway socket health after restart.
        #[arg(long, default_value = "10")]
        wait_seconds: u64,
    },
    Status,
    Logs {
        /// Number of recent lines to print.
        #[arg(short = 'n', long, default_value = "50")]
        lines: usize,

        /// Follow log output (like tail -f).
        #[arg(short, long)]
        follow: bool,
    },
}

#[derive(Subcommand)]
pub(crate) enum MemoryCommands {
    /// Rebuild the vector search index from stored embeddings.
    RebuildIndex,
}

#[derive(Subcommand)]
pub(crate) enum SignalCommands {
    Link {
        #[arg(long, default_value = "coop-agent")]
        device_name: String,
    },
    Unlink,
}
