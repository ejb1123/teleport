mod access;
mod audio;
mod bridge;
#[cfg(target_os = "linux")]
mod capture;
mod client;
#[cfg(target_os = "linux")]
mod clipboard;
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
mod congestion;
mod e2e;
mod ffmpeg;
mod fullscreen;
#[cfg(target_os = "linux")]
mod gamepad;
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
mod keyboard_capture;
mod launcher;
mod media;
mod natpmp;
mod overlay;
mod p2p;
mod pacing;
mod pairing;
mod presence;
mod profiles;
mod protocol;
#[cfg(target_os = "linux")]
mod pw_cursor;
mod relay;
mod relay_enroll;
mod security_key;
mod ssh_agent;
mod stats;
mod stun;
mod system_login;
#[cfg(target_os = "linux")]
mod system_pam;
mod windowing;
#[cfg(target_os = "linux")]
mod x11_cursor;

use anyhow::{Context, Result};
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
    /// With --relay, enter the host's relay pairing link instead of the code.
    Pair {
        address: String,
        #[command(flatten)]
        relay: relay::RelayArgs,
    },
    /// Log in with a Teleport account and save this device's trusted credential.
    Login {
        address: String,
        #[arg(long)]
        username: String,
        #[arg(long, default_value = "Teleport client")]
        device_name: String,
        /// With --relay: the host's relay id (`relay_host` in
        /// `teleport host-admin status`).
        #[arg(long, requires = "relay")]
        relay_host: Option<String>,
        #[command(flatten)]
        relay: relay::RelayArgs,
    },
    /// Log in once using the Linux host user's existing password (requires PAM setup).
    LoginSystem {
        address: String,
        #[arg(long)]
        username: String,
        /// Host SHA-256 certificate fingerprint obtained over a trusted channel.
        #[arg(long)]
        fingerprint: String,
        #[arg(long)]
        software_renderer: bool,
        #[command(flatten)]
        relay: relay::RelayArgs,
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
    /// Run a minimal MoQ relay (self-hosted alternative to a provider relay).
    /// It forwards only end-to-end encrypted objects it cannot read.
    Relay(relay::Options),
    /// Report whether a host is online through its relay (ONLINE, OFFLINE or
    /// UNKNOWN), from statements the host signs with its pinned identity.
    Presence(presence::Options),
    /// Check required media plugins without sharing the desktop.
    Doctor {
        /// Test synthetic HEVC Main 10 support; does not imply HDR desktop capture/display.
        #[arg(long)]
        hdr: bool,
    },
}

fn main() -> Result<()> {
    // Start the frame-timing clock before any instant it may be asked about.
    protocol::clock_us(std::time::Instant::now());
    tracing_subscriber::fmt()
        .with_ansi(std::io::stdout().is_terminal())
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "teleport=info,moq_native=warn,moq_net=warn".into()),
        )
        .init();
    let cli = Cli::parse();
    // Key diagnostics and enrollment do not use media or scan desktop plugins.
    // SDL is still linked: its load-time library selection is isolated by the
    // Linux package wrapper before this process starts.
    if !matches!(&cli.command, Some(Command::SecurityKey { .. })) {
        gstreamer::init()?;
    }
    let runtime = tokio::runtime::Runtime::new()?;
    // SDL's macOS window/event loop must stay on the main OS thread.
    match cli.command {
        Some(Command::LoginSystem {
            address,
            username,
            fingerprint,
            software_renderer,
            relay,
        }) => {
            use std::io::Write;
            moq_native::tls::parse_fingerprint(&fingerprint)?;
            let password =
                zeroize::Zeroizing::new(rpassword::prompt_password("Linux account password: ")?);
            let pairing = match relay.endpoint() {
                Some(endpoint) => runtime.block_on(relay_enroll::login_system(
                    &endpoint,
                    &username,
                    &password,
                    &fingerprint,
                ))?,
                None => runtime.block_on(system_login::login(
                    &address,
                    &username,
                    &password,
                    &fingerprint,
                ))?,
            };
            drop(password);
            let mut credential = tempfile::NamedTempFile::new()?;
            let data = zeroize::Zeroizing::new(serde_json::to_vec(&pairing)?);
            credential.write_all(&data)?;
            credential.flush()?;
            let mut child = std::process::Command::new(std::env::current_exe()?);
            child
                .arg("client")
                .arg(address)
                .arg("--pairing-file")
                .arg(credential.path());
            if software_renderer {
                child.arg("--software-renderer");
            }
            if let Some(url) = &relay.relay {
                child.arg("--relay").arg(url.as_str());
                if let Some(pin) = &relay.relay_fingerprint {
                    child.arg("--relay-fingerprint").arg(pin);
                }
                // Through the environment, not argv (visible to other users).
                if let Some(key) = &relay.relay_key {
                    child.env("TELEPORT_RELAY_KEY", key);
                }
            }
            anyhow::ensure!(
                child.status()?.success(),
                "desktop client exited unsuccessfully"
            );
            Ok(())
        }
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
            relay_host,
            relay,
        }) => {
            let endpoint = relay.endpoint();
            let relay_host = match &endpoint {
                Some(_) => Some(relay_host.context(
                    "--relay needs --relay-host (relay_host in `teleport host-admin status` on the host)",
                )?),
                None => None,
            };
            let password =
                zeroize::Zeroizing::new(rpassword::prompt_password("Teleport password: ")?);
            let pairing = match (endpoint, relay_host) {
                (Some(endpoint), Some(host)) => runtime.block_on(relay_enroll::login(
                    &endpoint,
                    &host,
                    &username,
                    &password,
                    &device_name,
                ))?,
                _ => {
                    runtime.block_on(access::login(&address, &username, &password, &device_name))?
                }
            };
            let profile = profiles::save_pairing(&address, &pairing, &mut profiles::load()?)?;
            println!(
                "Logged in and saved: {}",
                profile
                    .pairing_file
                    .as_ref()
                    .context("login did not save credentials")?
                    .display()
            );
            Ok(())
        }
        Some(Command::Client(options)) => client::run(options, &runtime),
        Some(Command::Relay(options)) => runtime.block_on(relay::serve(options)),
        Some(Command::Presence(options)) => runtime.block_on(presence::run(options)),
        Some(Command::Doctor { hdr }) => {
            media::doctor()?;
            if hdr {
                crate::hdr::doctor()?;
            }
            Ok(())
        }
        Some(Command::Pair { address, relay }) => {
            use std::io::{BufRead, Read, Write};
            let endpoint = relay.endpoint();
            // Check the profile name before spending a one-time code.
            pairing::validate_address(&address)?;
            // The code (or link) is read from stdin, never argv.
            print!(
                "{}",
                if endpoint.is_some() {
                    "Relay pairing link from the host: "
                } else {
                    "Pairing code from the host: "
                }
            );
            std::io::stdout().flush()?;
            let mut code = zeroize::Zeroizing::new(String::new());
            std::io::stdin().lock().take(256).read_line(&mut code)?;
            let pairing = match endpoint {
                Some(endpoint) => runtime.block_on(relay_enroll::pair(
                    &endpoint,
                    &relay_enroll::Invite::parse(&code)?,
                ))?,
                None => runtime.block_on(pairing::pair(&address, &code))?,
            };
            let profile = profiles::save_pairing(&address, &pairing, &mut profiles::load()?)?;
            println!(
                "Paired and saved: {}",
                profile
                    .pairing_file
                    .as_ref()
                    .context("pairing did not save credentials")?
                    .display()
            );
            Ok(())
        }
        Some(Command::Launcher) | None => launcher::run(),
    }
}
