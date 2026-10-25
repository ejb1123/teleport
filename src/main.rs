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
mod discovery;
mod docs;
mod e2e;
mod ffmpeg;
mod fullscreen;
#[cfg(target_os = "linux")]
mod gamepad;
#[cfg(target_os = "linux")]
mod guests;
mod hdr;
mod hdr_present;
#[cfg(target_os = "linux")]
mod host;
#[cfg(target_os = "linux")]
mod host_admin;
mod host_command;
#[cfg(target_os = "linux")]
mod host_ui;
// Unix-wide: `teleport relay` uses it too, and runs on macOS.
#[cfg(unix)]
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
mod identity;
mod key_options;
mod keyboard_capture;
mod launcher;
#[cfg(target_os = "macos")]
mod mac_scroll;
mod media;
mod natpmp;
mod overlay;
mod p2p;
mod pacing;
mod pairing;
mod presence;
#[cfg(target_os = "linux")]
mod privacy;
mod profiles;
mod protocol;
#[cfg(target_os = "linux")]
mod pw_cursor;
mod relay;
mod relay_enroll;
mod scroll;
mod security_key;
#[cfg(target_os = "linux")]
mod session_lock;
mod ssh_agent;
mod stats;
mod stun;
mod system_login;
#[cfg(target_os = "linux")]
mod system_pam;
#[cfg(test)]
mod test_env;
mod ticket;
#[cfg(target_os = "linux")]
mod virtual_display;
#[cfg(target_os = "linux")]
mod virtual_mic;
mod wake;
mod windowing;
#[cfg(target_os = "linux")]
mod x11_cursor;

use anyhow::{Context, Result};
use clap::{CommandFactory, FromArgMatches, Parser, Subcommand};
use std::io::IsTerminal;

/// The executable to start Teleport child processes (sessions, guards,
/// settings) with. Relocatable packages such as the AppImage run the binary
/// through a bundled dynamic loader, so `current_exe` is that loader; their
/// launcher names itself in `TELEPORT_SELF_EXE` instead.
pub(crate) fn self_exe() -> std::io::Result<std::path::PathBuf> {
    match std::env::var_os("TELEPORT_SELF_EXE") {
        Some(path) if !path.is_empty() => Ok(path.into()),
        _ => std::env::current_exe(),
    }
}

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
    /// A guest enters the invite link the host's owner shared (guests.md).
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
    /// Open a session with a ticket from the host's trusted issuer
    /// (tickets.md): redeemed once, inside TLS pinned to --fingerprint.
    LoginTicket {
        address: String,
        /// Host SHA-256 certificate fingerprint (the ticket's audience).
        #[arg(long)]
        fingerprint: String,
        /// File holding the ticket (`tkt1....`).
        #[arg(long)]
        ticket_file: std::path::PathBuf,
        /// The client key the ticket names (`teleport ticket client-keygen`).
        #[arg(long)]
        key_file: std::path::PathBuf,
        #[arg(long)]
        software_renderer: bool,
        /// Save the one-session credential here (0600) instead of starting
        /// the client (tests, supervisors that start it themselves).
        #[arg(long, hide = true)]
        save_credential: Option<std::path::PathBuf>,
        #[command(flatten)]
        relay: relay::RelayArgs,
    },
    /// Session ticket keys and signing for a self-hosted issuer (tickets.md).
    Ticket {
        #[command(subcommand)]
        command: ticket::Command,
    },
    /// Open local host settings. Does not expose administration over the network.
    #[cfg(target_os = "linux")]
    HostManager,
    /// Manage the local host from its desktop or an authenticated SSH session.
    #[cfg(target_os = "linux")]
    HostAdmin(host_admin::Options),
    /// Share a Linux desktop (or a synthetic test screen).
    #[cfg(target_os = "linux")]
    Host(Box<host::Options>),
    /// Open a native remote desktop window.
    Client(client::Options),
    /// Run a minimal MoQ relay (self-hosted alternative to a provider relay).
    /// It forwards only end-to-end encrypted objects it cannot read.
    Relay(relay::Options),
    /// Report whether a host is online through its relay (ONLINE, OFFLINE or
    /// UNKNOWN), from statements the host signs with its pinned identity.
    Presence(presence::Options),
    /// Turn the host's monitors back on after privacy mode (the host and its
    /// guard do this themselves; this is for a host killed without either).
    /// Uses the NixOS module's identity directory unless --state is given.
    #[cfg(target_os = "linux")]
    PrivacyRestore {
        /// The host's privacy-mode state file (`privacy-restore.json` in its
        /// identity directory).
        #[arg(long)]
        state: Option<std::path::PathBuf>,
        /// Without a state file, turn on every connected monitor that is off.
        #[arg(long)]
        all: bool,
    },
    /// Internal: restores the monitors if the host ends while in privacy mode.
    #[cfg(target_os = "linux")]
    #[command(hide = true)]
    PrivacyGuard {
        #[arg(long)]
        state: std::path::PathBuf,
        #[arg(long, default_value_t = 45)]
        timeout_secs: u64,
    },
    /// Wake a sleeping desktop with Wake-on-LAN: a magic packet on this
    /// network, and through a saved wake relay on the desktop's network.
    Wake(wake::Options),
    /// Print the Wake-on-LAN packets this machine receives (to check that
    /// wakes reach it).
    WakeListen(wake::ListenOptions),
    /// Ask a desktop to suspend (its host needs --allow-suspend).
    Suspend(host_command::SuspendOptions),
    /// Lock this user's graphical session and wait until it is locked (used
    /// after an automatic login; wake-and-unattended.md).
    #[cfg(target_os = "linux")]
    LockSession {
        /// Give up after this many seconds.
        #[arg(long, default_value_t = 60)]
        timeout: u64,
        /// Only lock a session the display manager logged in automatically.
        #[arg(long)]
        only_automatic_login: bool,
    },
    /// Check required media plugins without sharing the desktop.
    Doctor {
        /// Test synthetic HEVC Main 10 support; does not imply HDR desktop capture/display.
        #[arg(long)]
        hdr: bool,
    },
}

/// A self-contained macOS `Teleport.app` (tools/macos-app.sh) has no wrapper
/// script to point Teleport at its bundled GStreamer plugins, font and
/// libfido2, so derive them from the bundle layout:
/// `Contents/MacOS/teleport` next to the GStreamer plugins in `Contents/PlugIns`.
/// Must run before any thread starts (it sets environment variables).
#[cfg(target_os = "macos")]
fn app_bundle_environment() {
    let Some(contents) = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.canonicalize().ok())
        .and_then(|exe| Some(exe.parent()?.parent()?.to_path_buf()))
    else {
        return;
    };
    let plugins = contents.join("PlugIns");
    if !plugins.is_dir() {
        return;
    }
    let registry = std::env::var_os("HOME").map(|home| {
        std::path::PathBuf::from(home).join(format!(
            "Library/Caches/Teleport/gstreamer-registry-{}.bin",
            env!("CARGO_PKG_VERSION")
        ))
    });
    let set = |name: &str, value: std::ffi::OsString, replace: bool| {
        if replace || std::env::var_os(name).is_none() {
            // SAFETY: called first thing in main, before any other thread.
            unsafe { std::env::set_var(name, value) };
        }
    };
    // Only bundled plugins: Homebrew's or another GStreamer's would not load.
    set("GST_PLUGIN_SYSTEM_PATH_1_0", plugins.into(), true);
    set(
        "GST_PLUGIN_SCANNER_1_0",
        contents.join("MacOS/gst-plugin-scanner").into(),
        true,
    );
    if let Some(registry) = registry {
        set("GST_REGISTRY_1_0", registry.into(), false);
    }
    set(
        "TELEPORT_FONT",
        contents.join("Resources/fonts/DejaVuSans.ttf").into(),
        false,
    );
    set(
        "TELEPORT_LIBFIDO2",
        contents.join("Frameworks/libfido2.dylib").into(),
        false,
    );
    set("TELEPORT_PACKAGED", "1".into(), true);
}

fn main() -> Result<()> {
    #[cfg(target_os = "macos")]
    app_bundle_environment();
    // SVT-AV1 prints its whole configuration to stderr on every encoder open;
    // keep only its errors unless asked.
    if std::env::var_os("SVT_LOG").is_none() {
        // Safety: no other thread exists yet to read the environment.
        unsafe { std::env::set_var("SVT_LOG", "1") };
    }
    // libva prints "libva info: ..." driver-loading lines to stderr on every
    // VA display FFmpeg opens (each hardware encoder probe); keep its errors.
    #[cfg(target_os = "linux")]
    if std::env::var_os("LIBVA_MESSAGING_LEVEL").is_none() {
        // Safety: no other thread exists yet to read the environment.
        unsafe { std::env::set_var("LIBVA_MESSAGING_LEVEL", "1") };
    }
    // Start the frame-timing clock before any instant it may be asked about.
    protocol::clock_us(std::time::Instant::now());
    tracing_subscriber::fmt()
        .with_ansi(std::io::stdout().is_terminal())
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "teleport=info,moq_native=warn,moq_net=warn".into()),
        )
        .init();
    // FFmpeg's own messages through tracing (expected probe failures at debug).
    ffmpeg::route_log();
    // `Cli::parse`, with every --help ending in where the documentation it
    // cites is installed (docs.rs).
    let mut command = docs::with_help(Cli::command());
    let cli = Cli::from_arg_matches_mut(&mut command.get_matches_mut())
        .unwrap_or_else(|error| error.format(&mut command).exit());
    // Ticket keys and signing need no media and no desktop.
    let cli = match cli.command {
        Some(Command::Ticket { command }) => return ticket::run(command),
        command => Cli { command },
    };
    // The privacy-mode restore paths must work with nothing but the
    // compositor; they never touch media.
    #[cfg(target_os = "linux")]
    match &cli.command {
        Some(Command::PrivacyGuard {
            state,
            timeout_secs,
        }) => {
            return privacy::guard(state, std::time::Duration::from_secs(*timeout_secs));
        }
        Some(Command::PrivacyRestore { state, all }) => {
            let state = state.clone().unwrap_or_else(privacy::default_state_path);
            if privacy::restore(&state)? {
                println!("Monitors restored from {}", state.display());
            } else if *all {
                println!("Turned on {} monitor(s)", privacy::enable_all()?);
            } else {
                println!(
                    "No privacy-mode state at {}; use --all to turn on every monitor",
                    state.display()
                );
            }
            return Ok(());
        }
        Some(Command::LockSession {
            timeout,
            only_automatic_login,
        }) => {
            return session_lock::lock_and_wait(
                std::time::Duration::from_secs(*timeout),
                *only_automatic_login,
            );
        }
        _ => {}
    }
    // Power commands use no media either.
    match cli.command {
        Some(Command::WakeListen(options)) => wake::listen(options),
        Some(Command::Wake(options)) => {
            tokio::runtime::Runtime::new()?.block_on(wake::run(options))
        }
        Some(Command::Suspend(options)) => {
            tokio::runtime::Runtime::new()?.block_on(host_command::suspend(options))
        }
        command => run(command),
    }
}

fn run(command: Option<Command>) -> Result<()> {
    let cli = Cli { command };
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
            launch_client(&address, &pairing, software_renderer, &relay)
        }
        Some(Command::LoginTicket {
            address,
            fingerprint,
            ticket_file,
            key_file,
            save_credential,
            software_renderer,
            relay,
        }) => {
            moq_native::tls::parse_fingerprint(&fingerprint)?;
            let ticket = zeroize::Zeroizing::new(std::fs::read_to_string(&ticket_file)?);
            let device = ticket::read_client_seed(&key_file)?;
            let mut pairing = match relay.endpoint() {
                Some(endpoint) => runtime.block_on(relay_enroll::redeem_ticket(
                    &endpoint,
                    ticket.trim(),
                    &device,
                    &fingerprint,
                ))?,
                None => runtime.block_on(ticket::redeem(
                    &address,
                    ticket.trim(),
                    &device,
                    &fingerprint,
                ))?,
            };
            let result = match save_credential {
                Some(path) => crate::identity::write_new(
                    &path,
                    &zeroize::Zeroizing::new(serde_json::to_vec(&pairing)?),
                ),
                None => launch_client(&address, &pairing, software_renderer, &relay),
            };
            pairing.zeroize_secrets();
            result
        }
        Some(Command::SecurityKey { command }) => security_key::run(command),
        Some(Command::Ticket { command }) => ticket::run(command),
        #[cfg(target_os = "linux")]
        Some(Command::Host(options)) => runtime.block_on(host::run(*options)),
        #[cfg(target_os = "linux")]
        Some(Command::HostManager) => host_ui::run(),
        // Handled before media initialisation.
        #[cfg(target_os = "linux")]
        Some(
            Command::PrivacyRestore { .. }
            | Command::PrivacyGuard { .. }
            | Command::LockSession { .. },
        ) => Ok(()),
        // Handled before media initialisation.
        Some(Command::Wake(_) | Command::WakeListen(_) | Command::Suspend(_)) => Ok(()),
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
                    "Relay pairing link or guest invite link from the host: "
                } else {
                    "Pairing code or guest invite link from the host: "
                }
            );
            std::io::stdout().flush()?;
            let mut code = zeroize::Zeroizing::new(String::new());
            std::io::stdin().lock().take(256).read_line(&mut code)?;
            let invite = code
                .trim()
                .starts_with(pairing::INVITE_PREFIX)
                .then(|| pairing::InviteLink::parse(&code))
                .transpose()?;
            let pairing = match (endpoint, invite) {
                (Some(endpoint), Some(invite)) => {
                    runtime.block_on(relay_enroll::join(&endpoint, &invite))?
                }
                (None, Some(invite)) => runtime.block_on(pairing::join(&address, &invite))?,
                (Some(endpoint), None) => runtime.block_on(relay_enroll::pair(
                    &endpoint,
                    &relay_enroll::Invite::parse(&code)?,
                ))?,
                (None, None) => runtime.block_on(pairing::pair(&address, &code))?,
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

/// Run `teleport client` with a one-session credential (system login, a
/// redeemed ticket), handed over in a private temporary file.
fn launch_client(
    address: &str,
    pairing: &protocol::Pairing,
    software_renderer: bool,
    relay: &relay::RelayArgs,
) -> Result<()> {
    use std::io::Write;
    let mut credential = tempfile::NamedTempFile::new()?;
    let data = zeroize::Zeroizing::new(serde_json::to_vec(pairing)?);
    credential.write_all(&data)?;
    credential.flush()?;
    let mut child = std::process::Command::new(crate::self_exe()?);
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
