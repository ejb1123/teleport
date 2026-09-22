#[cfg(target_os = "linux")]
mod capture;
mod client;
#[cfg(target_os = "linux")]
mod host;
mod media;
mod protocol;

use anyhow::Result;
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(version, about)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Share a Linux desktop (or a synthetic test screen).
    #[cfg(target_os = "linux")]
    Host(host::Options),
    /// Open a native remote desktop window.
    Client(client::Options),
    /// Check required media plugins without sharing the desktop.
    Doctor,
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "teleport=info,moq_native=warn,moq_net=warn".into()),
        )
        .init();
    let cli = Cli::parse();
    gstreamer::init()?;
    let runtime = tokio::runtime::Runtime::new()?;
    // SDL's macOS window/event loop must stay on the main OS thread.
    match cli.command {
        #[cfg(target_os = "linux")]
        Command::Host(options) => runtime.block_on(host::run(options)),
        Command::Client(options) => client::run(options, &runtime),
        Command::Doctor => media::doctor(),
    }
}
