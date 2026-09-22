mod access;
mod audio;
#[cfg(target_os = "linux")]
mod capture;
mod client;
#[cfg(target_os = "linux")]
mod clipboard;
mod hdr;
mod hdr_present;
#[cfg(target_os = "linux")]
mod host;
#[cfg(target_os = "linux")]
mod host_admin;
#[cfg(target_os = "linux")]
mod host_ui;
#[cfg(target_os = "linux")]
mod identity;
mod launcher;
mod media;
mod pairing;
mod profiles;
mod protocol;
mod security_key;
mod ssh_agent;
mod stats;

use anyhow::Result;
use clap::{Parser, Subcommand};
use std::io::IsTerminal;

#[derive(Parser)]
#[command(version, about)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Local FIDO2 diagnostics and offline proofs; not yet Teleport login.
    SecurityKey {
        #[command(subcommand)]
        command: security_key::Command,
    },
    /// Open the native saved-host connection window.
    Launcher,
    /// Pair once using the short-lived code shown by a host started with --pair.
    Pair { address: String },
    /// Log in with a Teleport account and save this device's trusted credential.
    Login {
        address: String,
        #[arg(long)]
        username: String,
        #[arg(long, default_value = "Teleport client")]
        device_name: String,
    },
    /// Open local host settings. Does not expose administration over the network.
    #[cfg(target_os = "linux")]
    HostManager,
    /// Manage the local host from its desktop or an authenticated SSH session.
    #[cfg(target_os = "linux")]
    HostAdmin(host_admin::Options),
    /// Share a Linux desktop (or a synthetic test screen).
    #[cfg(target_os = "linux")]
    Host(host::Options),
    /// Open a native remote desktop window.
    Client(client::Options),
    /// Check required media plugins without sharing the desktop.
    Doctor {
        /// Test synthetic HEVC Main 10 support; does not imply HDR desktop capture/display.
        #[arg(long)]
        hdr: bool,
    },
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_ansi(std::io::stdout().is_terminal())
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
        Some(Command::SecurityKey { command }) => security_key::run(command),
        #[cfg(target_os = "linux")]
        Some(Command::Host(options)) => runtime.block_on(host::run(options)),
        #[cfg(target_os = "linux")]
        Some(Command::HostManager) => host_ui::run(),
        #[cfg(target_os = "linux")]
        Some(Command::HostAdmin(options)) => host_admin::run(options),
        Some(Command::Login {
            address,
            username,
            device_name,
        }) => {
            let password =
                zeroize::Zeroizing::new(rpassword::prompt_password("Teleport password: ")?);
            let pairing =
                runtime.block_on(access::login(&address, &username, &password, &device_name))?;
            let profile = profiles::save_pairing(&address, &pairing, &mut profiles::load()?)?;
            println!("Logged in and saved: {}", profile.pairing_file.display());
            Ok(())
        }
        Some(Command::Client(options)) => client::run(options, &runtime),
        Some(Command::Doctor { hdr }) => {
            media::doctor()?;
            if hdr {
                crate::hdr::doctor()?;
            }
            Ok(())
        }
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
