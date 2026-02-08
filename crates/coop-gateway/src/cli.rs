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
    Start,
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
    Version,
}

#[derive(Subcommand)]
pub(crate) enum SignalCommands {
    Link {
        #[arg(long, default_value = "coop-agent")]
        device_name: String,
    },
    Unlink,
}
