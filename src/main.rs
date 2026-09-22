mod audio;
#[cfg(target_os = "linux")]
mod capture;
mod client;
#[cfg(target_os = "linux")]
mod clipboard;
#[cfg(target_os = "linux")]
mod host;
#[cfg(target_os = "linux")]
mod identity;
mod launcher;
mod media;
mod pairing;
mod profiles;
mod protocol;

use anyhow::Result;
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(version, about)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Open the native saved-host connection window.
    Launcher,
    /// Pair once using the short-lived code shown by a host started with --pair.
    Pair { address: String },
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
        Some(Command::Host(options)) => runtime.block_on(host::run(options)),
        Some(Command::Client(options)) => client::run(options, &runtime),
        Some(Command::Doctor) => media::doctor(),
        Some(Command::Pair { address }) => {
            use std::io::{BufRead, Read, Write};
            print!("Pairing code from the host: ");
            std::io::stdout().flush()?;
            let mut code = String::new();
            std::io::stdin().lock().take(128).read_line(&mut code)?;
            let pairing = runtime.block_on(pairing::pair(&address, &code))?;
            let profile = profiles::save_pairing(&address, &pairing, &mut profiles::load()?)?;
            println!("Paired and saved: {}", profile.pairing_file.display());
            Ok(())
        }
        Some(Command::Launcher) | None => launcher::run(),
    }
}
