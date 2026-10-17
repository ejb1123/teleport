use crate::{
    capture::Capture,
    media,
    privacy::PrivacyMode,
    protocol::{self, Desktop, Event, Input, Pairing, Update},
    virtual_display::{Size, VirtualDisplay},
};
use anyhow::{Context, Result, ensure};
use clap::Args;
use std::{
    collections::BTreeMap,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};
use subtle::ConstantTimeEq;

/// Consecutive injection failures tolerated before the input path is treated
/// as gone (for example a revoked portal session) rather than a transient error.
const MAX_CONSECUTIVE_INPUT_FAILURES: u32 = 50;

/// In-session portal recreations allowed per minute before giving up.
const MAX_PORTAL_RECOVERIES_PER_MINUTE: usize = 3;
/// One-second attempts to recreate a lost portal session (displays settling).
const PORTAL_REOPEN_ATTEMPTS: u32 = 20;

/// Minimum spacing between client-requested IDRs per stream.
const KEYFRAME_REQUEST_INTERVAL: Duration = Duration::from_millis(100);

/// Safety-net IDR spacing for clients that request keyframes on loss. Rare
/// IDRs avoid the quarter-second bitrate spikes of the legacy interval while
/// still bounding recovery if a request itself is lost.
const LONG_KEY_INTERVAL_SECONDS: u32 = 4;

/// Session bitrate up to 60 FPS when `--bitrate` is left automatic (kbit/s).
const DEFAULT_BITRATE: u32 = 8000;

/// Minimum spacing of video reconfigurations (each may restart the encoder).
const CONFIGURE_INTERVAL: Duration = Duration::from_millis(500);
/// A portal session left in cursor-metadata mode by an earlier client goes
/// back to an embedded pointer if the new client has not opted in by then.
const CURSOR_CHOICE_TIMEOUT: Duration = Duration::from_secs(3);
/// How often privacy mode checks that the physical monitors are still off.
const PRIVACY_CHECK_INTERVAL: Duration = Duration::from_secs(2);
/// How often a fallback virtual display checks whether a monitor is back.
const MONITORS_CHECK_INTERVAL: Duration = Duration::from_secs(5);
/// Host pointer positions go to the client at most this often.
const CURSOR_POSITION_INTERVAL: Duration = Duration::from_millis(33);

/// The stream for the client's display on the active monitor.
fn negotiate_stream(
    display: &protocol::ClientDisplay,
    capture: &Capture,
) -> protocol::StreamChoice {
    let monitor = &capture.monitors[capture.active_monitor];
    protocol::negotiate(
        display,
        (monitor.width, monitor.height),
        monitor.refresh_mhz,
        protocol::MAX_FPS,
    )
}

#[derive(Args, Clone)]
pub struct Options {
    /// Also accept end-to-end encrypted sessions through this MoQ relay, e.g.
    /// moqt://relay.example:4444 (`teleport relay`). Requires --identity-dir.
    #[arg(long)]
    pub relay: Option<url::Url>,
    /// Pin a self-hosted relay's certificate (printed by `teleport relay`).
    #[arg(long, requires = "relay")]
    pub relay_fingerprint: Option<String>,
    /// Access key for a relay started with `teleport relay --key`.
    #[arg(
        long,
        requires = "relay",
        env = "TELEPORT_RELAY_KEY",
        hide_env_values = true
    )]
    pub relay_key: Option<String>,
    /// Accept relayed sessions only from devices enrolled with a device key
    /// (password login or code pairing by this version or later). Refuses the
    /// host pairing file's token and older device credentials through the
    /// relay, which anyone holding the host's identity directory could use.
    /// Direct connections are unaffected.
    #[arg(long, requires = "relay")]
    pub relay_require_device_key: bool,
    /// Peer-to-peer connections signalled through the relay (src/p2p.rs).
    #[command(flatten)]
    pub p2p: crate::p2p::HostArgs,
    /// Experimental: permit clients that explicitly opt in to forward an SSH agent.
    #[arg(long)]
    pub allow_ssh_agent: bool,
    /// Bind address (UDP). Use 0.0.0.0:4443 for LAN access.
    #[arg(long, default_value = "127.0.0.1:4443")]
    pub listen: String,
    #[arg(long, default_value = "auto", value_parser = ["auto", "portal", "x11", "test"])]
    pub source: String,
    /// With `--source test`: stream a black screen that turns white, and
    /// back, on every key or mouse-button press, to time input→photon with a
    /// photodiode or camera (docs/performance.md).
    #[arg(long)]
    pub flash_on_input: bool,
    /// New private pairing file; never overwritten. Copy securely to the client.
    #[arg(long, default_value = "pairing.json")]
    pub pairing_file: PathBuf,
    /// Stream width; 0 uses the selected monitor's reported native size.
    #[arg(long, default_value_t = 1280, value_parser = parse_width)]
    pub width: u32,
    #[arg(long, default_value_t = 60, value_parser = clap::value_parser!(u32).range(1..=240))]
    pub fps: u32,
    /// Video bitrate in kilobits per second. 0 (the default) picks one per
    /// session: 8000 up to 60 fps, scaled for size and rate above
    /// (`protocol::high_rate_bitrate`).
    #[arg(long, default_value_t = 0, value_parser = parse_bitrate)]
    pub bitrate: u32,
    /// Persistent private identity directory; keeps paired clients valid across restarts.
    #[arg(long, conflicts_with = "pairing_file")]
    pub identity_dir: Option<PathBuf>,
    /// Enable existing Linux-account login with an installed, host-native PAM helper.
    #[arg(long, requires = "identity_dir")]
    pub system_auth_helper: Option<PathBuf>,
    /// Opt in to saving/restoring compositor permissions (approval may still be required).
    #[arg(long)]
    pub restore_token: Option<PathBuf>,
    #[arg(long, default_value_t = 0)]
    pub monitor: usize,
    #[arg(long, value_enum, default_value_t = media::EncoderKind::Auto)]
    pub encoder: media::EncoderKind,
    /// Encoder speed/quality trade-off for clients that do not choose one:
    /// `latency` (the fastest encode), `balanced` or `quality`. None adds
    /// frame delay (docs/ffmpeg.md).
    #[arg(long, value_enum, default_value_t = protocol::EncoderPreset::Latency)]
    pub encoder_preset: protocol::EncoderPreset,
    /// Disable receiver-feedback bitrate adjustment.
    #[arg(long)]
    pub fixed_bitrate: bool,
    /// Always draw the pointer into the video. By default a desktop portal
    /// session sends it as cursor metadata (when the compositor offers it)
    /// so clients can draw it locally (docs/cursor.md).
    #[arg(long)]
    pub embedded_cursor: bool,
    /// Virtual displays through the desktop portal (KDE Plasma 6; GNOME
    /// untested): `fallback` streams one when no monitor is connected
    /// (headless) and lets clients add one as an extra monitor; `always`
    /// streams one sized to the client instead of the monitors; `never`
    /// turns both off (docs/virtual-display.md).
    #[arg(long, value_enum, default_value_t = VirtualDisplay::Fallback)]
    pub virtual_display: VirtualDisplay,
    /// Privacy mode (KDE): while a session lasts, turn the physical monitors
    /// off and stream a virtual display instead; `request` when the client
    /// asks (`teleport client --privacy`), `always` for every session. They
    /// come back on disconnect, on any failure, and through a guard process
    /// if the host dies (docs/virtual-display.md).
    #[arg(long, value_enum, default_value_t = PrivacyMode::Off)]
    pub privacy_mode: PrivacyMode,
    /// Privacy mode works here (`--privacy-mode`, a virtual-capable portal
    /// and KDE output control).
    #[arg(skip)]
    pub privacy_available: bool,
    /// Where privacy mode saves the monitors it turned off.
    #[arg(skip)]
    pub privacy_state: PathBuf,
    /// Explicit PulseAudio/PipeWire output .monitor name; never records a microphone.
    #[arg(long)]
    pub audio_source: Option<String>,
    /// Desktop audio encoding for clients that do not choose: `opus`, or
    /// `pcm` (uncompressed 48 kHz stereo, ~1.5 Mbit/s, for unconstrained
    /// links). Clients from before audio options always get Opus.
    #[arg(long, value_enum, default_value_t = protocol::AudioFormat::Opus)]
    pub audio_format: protocol::AudioFormat,
    /// Opus bitrate in kbit/s for clients that do not choose: 64, 96, 128,
    /// 192, 256 or 510.
    #[arg(long, default_value_t = protocol::DEFAULT_AUDIO_BITRATE, value_parser = protocol::parse_audio_bitrate)]
    pub audio_bitrate: u32,
    /// Offer clients a virtual microphone: while a client sends its
    /// microphone (`teleport client --mic`), applications here can record it
    /// from the "Teleport Microphone" input (PipeWire; docs/audio.md).
    #[arg(long)]
    pub microphone: bool,
    /// `--microphone` works here (PipeWire's `pw-cli`).
    #[arg(skip)]
    pub microphone_available: bool,
    /// Allow explicit text clipboard send/fetch requests from the paired client.
    #[arg(long)]
    pub clipboard: bool,
    /// Open one-time code enrollment on TCP at the same port for five minutes.
    #[arg(long)]
    pub pair: bool,
    /// Default codec for clients without an explicit selection. Prefer H.264 for compatibility.
    #[arg(long, value_enum, default_value_t = protocol::VideoCodec::H264)]
    pub codec: protocol::VideoCodec,
    /// Tests: advertise these hardware codecs instead of probing encoders.
    #[arg(long, hide = true, value_enum, value_delimiter = ',')]
    pub smoke_hardware_codecs: Option<Vec<protocol::VideoCodec>>,
    #[arg(skip)]
    pub dynamic_range: protocol::DynamicRange,
    /// Codecs `encoder` encodes in hardware here (`Desktop::hardware_codecs`).
    #[arg(skip)]
    pub hardware_codecs: Vec<protocol::VideoCodec>,
    /// Set while a network step-down streams below `width`/`fps`
    /// (`Desktop::stepped_down`); `width` and `fps` then hold the step.
    #[arg(skip)]
    pub stepped_down: Option<protocol::SteppedDown>,
    /// The session's client strips `protocol::FrameTiming` headers.
    #[arg(skip)]
    pub frame_timing: bool,
    /// Wake relay: send Wake-on-LAN packets on this host's networks when
    /// the owner's devices ask (`teleport wake`, the launcher's Wake), to
    /// wake their other machines here. Never at the MoQ relay's request
    /// (docs/wake-and-unattended.md).
    #[arg(long)]
    pub wake_relay: bool,
    /// With --wake-relay: only wake these hardware addresses (repeatable;
    /// default: any device address).
    #[arg(long, requires = "wake_relay")]
    pub wake_relay_allow: Vec<crate::wake::Mac>,
    /// Suspend this machine when the owner's device asks (`teleport
    /// suspend`, the launcher's Sleep); wake it again with Wake-on-LAN.
    #[arg(long)]
    pub allow_suspend: bool,
    /// Tests: run this instead of `systemctl suspend`.
    #[arg(long, hide = true, default_value = "systemctl suspend")]
    pub suspend_command: String,
    /// Unlock the desktop session (logind) when a client signs in with this
    /// account's Linux password (system login), which PAM has just
    /// verified. The password is never stored. For unattended hosts that
    /// log in automatically and lock at once.
    #[arg(long, requires = "system_auth_helper")]
    pub unlock_on_system_login: bool,
    /// What clients are told about waking this host (`Desktop::wake`).
    #[arg(skip)]
    pub wake: Option<crate::wake::WakeInfo>,
    /// The session's lock state can be read and changed (logind).
    #[arg(skip)]
    pub secure_attention: bool,
}

pub async fn run(mut options: Options) -> Result<()> {
    if let Some(helper) = &options.system_auth_helper {
        crate::system_pam::validate_helper(helper)?;
        ensure!(
            unsafe { libc::getuid() } != 0,
            "system login requires a non-root desktop host"
        );
    }
    ensure!(
        options.identity_dir.is_some() || !options.pairing_file.exists(),
        "pairing file already exists; choose a new --pairing-file (credentials rotate each host start)"
    );
    media::doctor()?;
    if options.flash_on_input {
        ensure!(
            options.source == "test",
            "--flash-on-input needs --source test"
        );
        media::test_flash::enable();
    }
    // Create the private directory before a portal restore token inside it is written.
    if let Some(directory) = &options.identity_dir {
        crate::identity::open(directory)?;
    }
    // A host that died in privacy mode may have left the monitors off (its
    // guard normally turns them back on).
    options.privacy_state =
        crate::privacy::state_path(options.identity_dir.as_deref(), &options.listen);
    if options.privacy_state.exists() {
        let state = options.privacy_state.clone();
        match tokio::task::spawn_blocking(move || crate::privacy::restore(&state)).await? {
            Ok(_) => tracing::warn!("restored monitors left off by an earlier privacy session"),
            Err(error) => tracing::error!(
                error = format!("{error:#}"),
                "could not restore monitors left off by an earlier privacy session"
            ),
        }
    }
    let mut capture = Capture::open_host(
        &options.source,
        options.restore_token.as_deref(),
        options.monitor,
        !options.embedded_cursor,
        options.virtual_display,
    )
    .await?;
    // No monitor now: a virtual display only while a client is connected.
    capture.release_fallback().await;
    if options.privacy_mode != PrivacyMode::Off {
        options.privacy_available = capture.virtual_available()
            && tokio::task::spawn_blocking(crate::virtual_display::kscreen_available)
                .await
                .unwrap_or(false);
        if !options.privacy_available {
            tracing::warn!(
                "privacy mode unavailable: it needs KDE Plasma (kscreen-doctor) and a portal with virtual displays"
            );
        }
    }
    if options.microphone {
        // Virtual microphones of an earlier host that did not exit cleanly.
        options.microphone_available = tokio::task::spawn_blocking(|| {
            let available = crate::virtual_mic::available();
            if available {
                crate::virtual_mic::cleanup_stale();
            }
            available
        })
        .await
        .unwrap_or(false);
        if !options.microphone_available {
            tracing::warn!("microphone passthrough unavailable: it needs PipeWire (pw-cli)");
        }
    }
    // Wake-on-LAN details and the session's lock control (G23, G24).
    let (listen, wake_relay, suspend) = (
        options.listen.clone(),
        options.wake_relay,
        options.allow_suspend,
    );
    let test_source = options.source == "test";
    let (wake, secure_attention) = tokio::task::spawn_blocking(move || {
        (
            crate::wake::host::info(&listen, wake_relay, suspend),
            !test_source && crate::session_lock::available(),
        )
    })
    .await?;
    tracing::info!(
        mac = wake.mac.as_deref().unwrap_or("unknown"),
        wake_on_lan = ?wake.wake_on_lan,
        network = wake.ipv4.as_deref().unwrap_or("unknown"),
        wake_relay,
        suspend,
        secure_attention,
        "power and session control"
    );
    if wake.mac.is_some() && wake.wake_on_lan == Some(false) {
        tracing::warn!(
            "Wake-on-LAN is off on this network card; clients cannot wake this host (docs/wake-and-unattended.md)"
        );
    }
    options.wake = Some(wake);
    options.secure_attention = secure_attention;
    let result = serve(&options, &mut capture).await;
    capture.close().await;
    result
}

/// The client's Ctrl+Alt+End: lock the session, or show the lock screen's
/// password prompt (a Shift tap wakes the screen and the prompt). Returns
/// the notice for the client.
async fn secure_attention(options: &Options, capture: &mut Capture) -> String {
    if !options.secure_attention {
        return "This host cannot lock or unlock its session (no logind session)".into();
    }
    match tokio::task::spawn_blocking(crate::session_lock::secure_attention).await {
        Ok(Ok(crate::session_lock::Attention::Lock)) => {
            tracing::info!("session locked at the client's request");
            "Host session locked. Ctrl+Alt+End again shows the password prompt".into()
        }
        Ok(Ok(crate::session_lock::Attention::Prompt)) => {
            for down in [true, false] {
                // KEY_LEFTSHIFT: changes nothing in the password field.
                let _ = capture.input(Event::Key { code: 42, down }).await;
            }
            "Host is locked: type the password to unlock".into()
        }
        Ok(Err(error)) => format!("Host session lock unavailable: {error:#}"),
        Err(error) => format!("Host session lock unavailable: {error}"),
    }
}

/// After a system login (PAM just checked the account password), unlock the
/// session if the host is set to (`--unlock-on-system-login`).
async fn unlock_after_system_login(options: &Options, system_login: bool) {
    if !(options.unlock_on_system_login && system_login) {
        return;
    }
    let result = tokio::task::spawn_blocking(|| {
        let session = crate::session_lock::display_session()?;
        crate::session_lock::unlock(&session)
    })
    .await;
    match result {
        Ok(Ok(())) => tracing::info!("session unlocked after system login"),
        Ok(Err(error)) => tracing::warn!(
            error = format!("{error:#}"),
            "could not unlock the session after system login"
        ),
        Err(error) => tracing::warn!(%error, "could not unlock the session after system login"),
    }
}

async fn serve(options: &Options, capture: &mut Capture) -> Result<()> {
    // Warm the hardware encoder probe so the first client does not wait for it.
    let kind = options.encoder;
    if options.smoke_hardware_codecs.is_none() {
        std::thread::spawn(move || media::hardware_codecs(kind));
    }
    let mut config = moq_native::ServerConfig::default();
    config.bind = Some(options.listen.clone());
    config.version = vec![protocol::WIRE_VERSION.parse().map_err(anyhow::Error::msg)?];
    config.quic.idle_timeout = Some(protocol::QUIC_IDLE_TIMEOUT);
    config.quic.keep_alive = Some(protocol::QUIC_KEEP_ALIVE);
    config.quic.mtu_discovery = Some(protocol::QUIC_MTU_DISCOVERY);
    let identity = options
        .identity_dir
        .as_deref()
        .map(crate::identity::open)
        .transpose()?;
    if let Some(identity) = &identity {
        config.tls.cert = vec![identity.certificate.clone()];
        config.tls.key = vec![identity.key.clone()];
    } else {
        config.tls.generate = vec!["teleport.local".into()];
    }
    let mut server = config.init()?;
    let token = identity
        .as_ref()
        .map(|i| i.token.clone())
        .unwrap_or_else(crate::identity::token);
    let pairing = Pairing {
        token,
        fingerprint: server
            .certificates()
            .fingerprints()
            .first()
            .context("no certificate")?
            .clone(),
        device_key: None,
    };
    let pairing_path = identity
        .as_ref()
        .map(|i| &i.pairing)
        .unwrap_or(&options.pairing_file);
    if identity.is_some() && pairing_path.exists() {
        let saved = Pairing::read(pairing_path)?;
        ensure!(
            saved.fingerprint == pairing.fingerprint && saved.token == pairing.token,
            "persistent pairing does not match host certificate"
        );
    } else {
        crate::identity::write_new(pairing_path, &serde_json::to_vec_pretty(&pairing)?)?;
    }
    tracing::info!(address = %server.local_addr()?, pairing_file = %pairing_path.display(), "Host ready; use saved trust, a --pair code, or securely import the pairing file. UDP port must be reachable.");
    let access = options
        .identity_dir
        .as_deref()
        .map(|directory| crate::access::ServerState::open(directory, pairing.clone()))
        .transpose()?;
    let relay_endpoint = options.relay.clone().map(|url| crate::relay::Endpoint {
        url,
        fingerprint: options.relay_fingerprint.clone(),
        key: options.relay_key.clone(),
    });
    let admin = if let (Some(directory), Some(access)) =
        (options.identity_dir.as_deref(), access.as_ref())
    {
        Some(
            crate::host_admin::start_with_system(
                directory,
                server.local_addr()?,
                pairing.clone(),
                access.clone(),
                options.system_auth_helper.clone(),
                relay_endpoint.clone(),
            )
            .await?,
        )
    } else {
        None
    };
    let _enrollment = if options.pair && admin.is_none() {
        if options.identity_dir.is_none() {
            tracing::warn!(
                "Use --identity-dir to keep paired clients trusted across host restarts"
            );
        }
        Some(crate::pairing::start(server.local_addr()?, pairing.clone()).await?)
    } else {
        None
    };
    if options.pair
        && let Some((_, control)) = &admin
    {
        let opened = control.open_pairing().await?;
        println!(
            "Pairing code: {} (one use, expires in 5 minutes)",
            opened.code
        );
        if let Some(link) = opened.relay_link {
            // As secret as the code; clients read it from stdin, never argv.
            println!("Relay pairing link: {link} (same code; enter it in `teleport pair --relay`)");
        }
    }
    let shutdown = shutdown_signal();
    tokio::pin!(shutdown);
    let policy = crate::host_command::host::Policy::new(
        options.wake_relay,
        options.wake_relay_allow.clone(),
        options.allow_suspend,
        &options.suspend_command,
    );
    let relay_identity = match (&relay_endpoint, &identity, &access) {
        (Some(_), Some(identity), Some(_)) => Some(Arc::new(identity.e2e()?)),
        (Some(_), _, _) => {
            anyhow::bail!("--relay requires --identity-dir (a persistent host identity)")
        }
        (None, _, _) => None,
    };
    let relay_namespace = crate::relay::namespace(&pairing.fingerprint)?;
    // Direct peer-to-peer sessions, signalled through the relay; they reach
    // the same authorization below as the main listener's.
    let (p2p, mut p2p_requests) = match (&relay_endpoint, &identity, &relay_identity) {
        (Some(endpoint), Some(identity), Some(_)) if !options.p2p.no_p2p => {
            let (host, requests) =
                crate::p2p::Host::start(&options.p2p, endpoint.clone(), identity)?;
            (Some(host), Some(requests))
        }
        _ => (None, None),
    };
    // Closes the socket and any gateway mapping however serve() returns.
    let _p2p_guard = p2p.clone().map(crate::p2p::CloseOnDrop);
    // Connected relay, confirmed sessions from its intake task, and the tasks
    // (session intake, presence, enrollment).
    let mut relay: Option<(
        crate::relay::Link,
        tokio::sync::mpsc::Receiver<crate::relay::Accepted>,
        Vec<crate::relay::AbortOnDrop>,
    )> = None;
    let mut relay_retry = Instant::now();
    let mut idle_check = tokio::time::interval(Duration::from_secs(15));
    idle_check.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        if let (Some(endpoint), Some(e2e), Some(access)) =
            (&relay_endpoint, &relay_identity, &access)
            && relay.is_none()
            && Instant::now() >= relay_retry
        {
            match crate::relay::connect(endpoint, &relay_namespace).await {
                Ok(link) => {
                    tracing::info!(relay = %endpoint.display_url(), namespace = %relay_namespace, "connected to relay; accepting end-to-end encrypted sessions");
                    // Signed "online" statements for launchers (see presence.rs).
                    let presence = tokio::spawn(crate::presence::publish(
                        link.outgoing.clone(),
                        e2e.clone(),
                        relay_namespace.clone(),
                    ));
                    let (sender, receiver) = tokio::sync::mpsc::channel(4);
                    let intake = tokio::spawn(crate::relay::intake(
                        link.session.clone(),
                        link.incoming.clone(),
                        link.outgoing.clone(),
                        access.clone(),
                        e2e.clone(),
                        sender,
                        options.relay_require_device_key,
                        p2p.clone(),
                    ));
                    let mut tasks = vec![
                        crate::relay::AbortOnDrop(intake),
                        crate::relay::AbortOnDrop(presence),
                    ];
                    // Password and system-login enrollment through the relay.
                    if let Some((_, control)) = &admin {
                        tasks.push(crate::relay::AbortOnDrop(tokio::spawn(
                            control.clone().serve_relay(
                                link.session.clone(),
                                link.incoming.clone(),
                                link.outgoing.clone(),
                            ),
                        )));
                    }
                    relay = Some((link, receiver, tasks));
                }
                Err(error) => {
                    tracing::warn!(%error, "relay unavailable; retrying in 5 s");
                    relay_retry = Instant::now() + Duration::from_secs(5);
                }
            }
        }
        let request = tokio::select! {
            request = server.accept() => match request { Some(request) => crate::p2p::Incoming::Listener(request), None => break },
            Some(request) = async {
                match p2p_requests.as_mut() {
                    Some(requests) => requests.recv().await,
                    None => std::future::pending().await,
                }
            } => {
                tracing::info!(remote = %request.remote, "peer-to-peer connection; checking its credential");
                crate::p2p::Incoming::P2p(request)
            }
            accepted = async {
                match relay.as_mut() {
                    Some((_, receiver, _)) => receiver.recv().await,
                    None => {
                        tokio::time::sleep_until(relay_retry.max(Instant::now() + Duration::from_millis(100)).into()).await;
                        None
                    }
                }
            }, if relay_endpoint.is_some() => {
                match (accepted, &access, &relay) {
                    (Some(accepted), Some(access), Some((link, _, _))) => {
                        // Owner commands need no desktop; they run beside
                        // sessions.
                        match crate::host_command::Command::parse(Some(&accepted.request)) {
                            Ok(Some(command)) => {
                                match access.begin_session_hash(accepted.credential.to_vec(), accepted.device_key.as_deref()).and_then(|lease| Ok((crate::relay::host(link, &accepted)?, lease))) {
                                    Ok((bridged, lease)) => {
                                        let owner = lease.owner();
                                        tokio::spawn(crate::host_command::host::relayed(bridged, accepted, command, policy.clone(), owner, lease));
                                    }
                                    Err(error) => tracing::warn!(%error, "relayed command refused"),
                                }
                                continue;
                            }
                            Ok(None) => (),
                            Err(error) => {
                                tracing::warn!(%error, "relayed client sent a malformed command");
                                continue;
                            }
                        }
                        restore_capture(capture).await;
                        let result = tokio::select! {
                            result = relayed(link, accepted, access, options, capture) => result,
                            _ = &mut shutdown => { capture.end_session().await; break; }
                        };
                        capture.end_session().await;
                        leave_privacy(capture).await;
                        if let Err(error) = result {
                            tracing::warn!(%error, "relayed client disconnected");
                        }
                    }
                    (None, _, Some(_)) => {
                        tracing::warn!("relay connection lost; reconnecting");
                        relay = None;
                        relay_retry = Instant::now() + Duration::from_secs(1);
                    }
                    _ => {}
                }
                continue;
            }
            // Between clients: a portal session the compositor closed (e.g.
            // displays woke from power saving, or across a lock) is
            // recreated now rather than when the next client arrives.
            _ = idle_check.tick() => {
                if capture.portal_closed() {
                    restore_capture(capture).await;
                }
                continue;
            }
            _ = &mut shutdown => break,
        };
        let supplied = request
            .path()
            .strip_prefix("/teleport/")
            .unwrap_or("")
            .to_owned();
        // A peer-to-peer request must present the very credential its
        // signalling exchange authenticated (with its device key, if any).
        let grant = request.grant().cloned();
        let authorized = access.as_ref().map_or_else(
            || bool::from(supplied.as_bytes().ct_eq(pairing.token.as_bytes())),
            |access| access.authorized(&supplied),
        ) && grant.as_ref().is_none_or(|grant| {
            bool::from(crate::e2e::credential_hash(&supplied).ct_eq(&grant.credential))
        });
        if !authorized {
            request.close().await;
            tracing::warn!("rejected unauthorized client");
            continue;
        }
        let session_lease = if let Some(access) = &access {
            // Peer-to-peer: lease with the key proof of the signalling
            // exchange, so stored device keys bind it as on the relay.
            let lease = match &grant {
                Some(grant) => access
                    .begin_session_hash(grant.credential.to_vec(), grant.device_key.as_deref()),
                None => access.begin_session(&supplied),
            };
            match lease {
                Ok(lease) => Some(lease),
                Err(_) => {
                    request.close().await;
                    continue;
                }
            }
        } else {
            None
        };
        // Owner commands (wake relay, suspend, ping): no desktop capture.
        match crate::host_command::Command::parse(request.query()) {
            Ok(Some(command)) => {
                let owner = session_lease.as_ref().is_none_or(|lease| lease.owner());
                tokio::spawn(crate::host_command::host::direct(
                    request,
                    command,
                    policy.clone(),
                    owner,
                    session_lease,
                ));
                continue;
            }
            Ok(None) => (),
            Err(error) => {
                tracing::warn!(%error, "client sent a malformed command");
                request.close().await;
                continue;
            }
        }
        unlock_after_system_login(
            options,
            session_lease
                .as_ref()
                .is_some_and(|lease| lease.system_login()),
        )
        .await;
        restore_capture(capture).await;
        let result = tokio::select! {
            result = connection(request, options, capture) => result,
            _ = async { loop {
                tokio::time::sleep(Duration::from_secs(1)).await;
                let revoked = match (&session_lease, &grant) {
                    (Some(lease), Some(_)) => !lease.authorized(),
                    _ => access.as_ref().is_some_and(|access| !access.authorized(&supplied)),
                };
                if revoked { break; }
            } } => Err(anyhow::anyhow!("device access revoked")),
            _ = &mut shutdown => { capture.end_session().await; break; }
        };
        capture.end_session().await;
        leave_privacy(capture).await;
        if let Err(error) = result {
            tracing::warn!(%error, "client disconnected");
        }
    }
    server.close().await;
    Ok(())
}

/// Run a relayed client that has already proven the session keys (see
/// `relay::intake`). Only now is the credential leased (claiming a single-use
/// ticket) and the desktop captured.
async fn relayed(
    link: &crate::relay::Link,
    accepted: crate::relay::Accepted,
    access: &Arc<crate::access::ServerState>,
    options: &Options,
    capture: &mut Capture,
) -> Result<()> {
    let hash = accepted.credential;
    let lease = access
        .begin_session_hash(hash.to_vec(), accepted.device_key.as_deref())
        .context("relayed credential not usable")?;
    unlock_after_system_login(options, lease.system_login()).await;
    let bridged = crate::relay::host(link, &accepted)?;
    let session = link.session.clone();
    tokio::select! {
        result = run_session(
            Some(accepted.request.as_str()),
            bridged.outgoing.clone(),
            bridged.incoming.clone(),
            async { Ok(session) },
            options,
            capture,
        ) => result,
        // Also ends if the device's stored key no longer is the one proven.
        _ = async { loop { tokio::time::sleep(Duration::from_secs(1)).await; if !lease.authorized() { break; } } } => Err(anyhow::anyhow!("device access revoked")),
    }
}

/// Before a new session, recreate a portal session an earlier recovery could
/// not (the displays may have been off). Failures are logged; the session then
/// reports the capture error to the client.
async fn restore_capture(capture: &mut Capture) {
    for attempt in 1..=5 {
        if !capture.is_lost() {
            return;
        }
        match capture.reopen().await {
            Ok(()) => tracing::info!("desktop portal session recreated before the next client"),
            Err(error) => {
                tracing::warn!(
                    attempt,
                    error = format!("{error:#}"),
                    "desktop portal still unavailable"
                );
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        }
    }
}

/// After a session. Privacy mode (its monitors are already back on): stream
/// the physical monitors again; a failure leaves the capture lost, and
/// `restore_capture` retries before the next client. A fallback virtual
/// display is given up until the next client.
async fn leave_privacy(capture: &mut Capture) {
    if capture.privacy()
        && let Err(error) = capture.set_privacy(false, None).await
    {
        tracing::warn!(
            error = format!("{error:#}"),
            "desktop portal not ready after privacy mode"
        );
    }
    capture.release_fallback().await;
}

/// Recreate the portal session, retrying while displays settle.
async fn reopen_capture(capture: &mut Capture) -> Result<()> {
    let mut attempt = 0;
    loop {
        attempt += 1;
        match capture.reopen().await {
            Ok(()) => return Ok(()),
            Err(error) if attempt < PORTAL_REOPEN_ATTEMPTS => {
                tracing::warn!(
                    attempt,
                    error = format!("{error:#}"),
                    "desktop portal not ready yet; retrying"
                );
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
            Err(error) => return Err(error.context("recreating the desktop portal session")),
        }
    }
}

/// A capture change applied while every pipeline is stopped.
enum DisplayChange {
    /// Privacy mode on: stream a virtual output, physical monitors off.
    Privacy,
    /// The primary virtual output follows the client's display.
    Resize(Size),
}

/// Privacy mode on. Saves the monitors and starts the guard first, then
/// switches capture to a virtual output, then turns the monitors off. On any
/// failure the monitors are restored and capture returns to them.
async fn enter_privacy(
    capture: &mut Capture,
    size: Option<Size>,
    state: &std::path::Path,
) -> Result<crate::privacy::Privacy> {
    let mut privacy = crate::privacy::Privacy::begin(state.to_owned()).await?;
    let result = match capture.set_privacy(true, size).await {
        Ok(()) => privacy.enforce().await,
        Err(error) => Err(error),
    };
    match result {
        Ok(count) => {
            tracing::info!(monitors = count, "privacy mode on: physical monitors off");
            Ok(privacy)
        }
        Err(error) => {
            if let Err(restore) = privacy.end().await {
                tracing::error!(
                    error = format!("{restore:#}"),
                    "could not restore the monitors; the privacy guard retries"
                );
            }
            if capture.set_privacy(false, None).await.is_err() {
                reopen_capture(capture).await?;
            }
            Err(error)
        }
    }
}

async fn shutdown_signal() -> Result<()> {
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    tokio::select! {
        result = tokio::signal::ctrl_c() => result?,
        _ = terminate.recv() => (),
    }
    Ok(())
}

async fn connection(
    request: crate::p2p::Incoming,
    options: &Options,
    capture: &mut Capture,
) -> Result<()> {
    let query = request.query().map(str::to_owned);
    let outgoing = moq_net::Origin::random().produce();
    let incoming = moq_net::Origin::random().produce();
    let session = request.ok(&outgoing, incoming.clone());
    run_session(
        query.as_deref(),
        outgoing,
        incoming,
        session,
        options,
        capture,
    )
    .await
}

/// One authenticated client session. `outgoing`/`incoming` are the origins the
/// session publishes `desktop` to and receives `controls` from: the QUIC
/// session's own origins on a direct connection, or plaintext origins bridged
/// to a relay (see `crate::relay`). `session` resolves to the transport whose
/// closure and statistics apply.
async fn run_session(
    query: Option<&str>,
    outgoing: moq_net::origin::Producer,
    incoming: moq_net::origin::Producer,
    session: impl std::future::Future<Output = Result<moq_net::Session>>,
    options: &Options,
    capture: &mut Capture,
) -> Result<()> {
    // Stream preferences belong to this connection, not the next paired client.
    let mut negotiated = options.clone();
    let requested = requested_format(query, options.codec)?;
    let ssh_requested = url::form_urlencoded::parse(query.unwrap_or_default().as_bytes())
        .any(|(key, value)| key == "ssh-agent" && value == "1");
    ensure!(
        !ssh_requested || options.allow_ssh_agent,
        "SSH agent forwarding is disabled; host requires explicit --allow-ssh-agent"
    );
    negotiated.allow_ssh_agent = ssh_requested && options.allow_ssh_agent;
    negotiated.codec = requested.codec;
    negotiated.dynamic_range = requested.dynamic_range;
    let options = &mut negotiated;
    // Each client opts in again; a new session starts with the pointer in video.
    capture.local_cursor = false;
    capture.select_monitor(capture.active_monitor).await?;
    let mut broadcast = outgoing.create_broadcast(
        "desktop",
        moq_net::broadcast::Route::new().with_announce(true),
    )?;
    // Both codecs' tracks exist until the client has chosen (SelectCodec):
    // it subscribes to its choice right after sending it.
    let mut video_tracks = Vec::new();
    for codec in [protocol::VideoCodec::H264, protocol::VideoCodec::H265] {
        video_tracks.push((
            codec,
            broadcast.create_track(
                codec.track(),
                moq_net::track::Info::default().with_latency_max(Duration::from_millis(500)),
            )?,
        ));
    }
    // Likewise for the per-monitor tracks, which clients may subscribe to
    // before they send anything; one more for a virtual monitor the client
    // may add.
    let monitor_slots =
        (capture.monitors.len() + usize::from(capture.virtual_monitors_available())).min(64);
    let mut monitor_codec_tracks = Vec::new();
    for codec in [protocol::VideoCodec::H264, protocol::VideoCodec::H265] {
        let mut tracks = Vec::new();
        for index in 0..monitor_slots {
            tracks.push(broadcast.create_track(
                format!("monitor-{index}-{}", codec.track()),
                moq_net::track::Info::default().with_latency_max(Duration::from_millis(500)),
            )?);
        }
        monitor_codec_tracks.push((codec, tracks));
    }
    let mut monitor_streams: BTreeMap<usize, MonitorEncoder> = BTreeMap::new();
    let mut metadata = broadcast.create_track("desktop", None)?;
    let mut updates = broadcast.create_track(
        "updates",
        moq_net::track::Info::default().with_ordered(true),
    )?;
    let agent_requests = if options.allow_ssh_agent {
        Some(crate::ssh_agent::create_track(
            &mut broadcast,
            crate::ssh_agent::REQUEST_TRACK,
        )?)
    } else {
        None
    };
    // Both formats' tracks exist until the client has chosen (SelectCodec).
    let mut audio_tracks = Vec::new();
    if options.audio_source.is_some() {
        for format in [protocol::AudioFormat::Opus, protocol::AudioFormat::Pcm] {
            audio_tracks.push((format, broadcast.create_track(format.track(), None)?));
        }
    }
    let session = session.await?;
    let input = tokio::time::timeout(Duration::from_secs(10), async {
        let remote = incoming
            .consume()
            .announced_broadcast("controls")
            .await
            .context("input broadcast missing")?;
        Ok::<_, anyhow::Error>(
            remote
                .track("input")?
                .subscribe(
                    moq_net::track::Subscription::default()
                        .with_ordered(true)
                        .with_priority(255)
                        .with_latency_max(protocol::INPUT_TIMEOUT)
                        .with_group_start(0),
                )
                .await?,
        )
    })
    .await
    .context("client did not open input track")??;
    let mut ssh_agent = if let Some(requests) = agent_requests {
        let responses = tokio::time::timeout(Duration::from_secs(10), async {
            let remote = incoming
                .consume()
                .announced_broadcast("controls")
                .await
                .context("controls missing")?;
            Ok::<_, anyhow::Error>(
                remote
                    .track(crate::ssh_agent::RESPONSE_TRACK)?
                    .subscribe(crate::ssh_agent::subscription())
                    .await?,
            )
        })
        .await
        .context("client SSH forwarding track missing")??;
        Some(crate::ssh_agent::host(requests, responses)?)
    } else {
        None
    };
    let mut video_start_group = 0;
    if options.bitrate == 0 {
        let size = desktop(options, capture, video_start_group)?;
        options.bitrate =
            protocol::high_rate_bitrate(DEFAULT_BITRATE, size.width, size.height, options.fps);
    }
    // Probed once per process (src/ffmpeg_media.rs), off the async loop.
    let kind = options.encoder;
    options.hardware_codecs = match &options.smoke_hardware_codecs {
        Some(codecs) => codecs.clone(),
        None => tokio::task::spawn_blocking(move || media::hardware_codecs(kind))
            .await
            .context("hardware encoder probe failed")?,
    };
    // The metadata goes out before video starts: a client that chooses the
    // codec (Desktop::codec_selection) needs it to decide.
    let info = desktop(options, capture, video_start_group)?;
    let mut group = metadata.append_group()?;
    group.write_frame(moq_net::Timestamp::now(), serde_json::to_vec(&info)?)?;
    group.finish()?;
    let mut check = tokio::time::interval(Duration::from_millis(200));
    let (sender, receiver) = tokio::sync::mpsc::channel(256);
    let mut events = SessionEvents {
        receiver,
        deferred: Default::default(),
    };
    // Highest pointer motion ID applied from either the ordered input track or
    // pointer datagrams; older or duplicate positions are dropped.
    let last_motion = Arc::new(AtomicU64::new(0));
    let controls = incoming.clone();
    let pointer_sender = sender.clone();
    let mut pointer_task: Option<AbortOnDrop> = None;
    // A task of its own, so input keeps arriving while the session loop waits
    // for an encoder to start or stop (see while_starting).
    let mut incoming = AbortOnDrop(tokio::spawn(receive_input(
        input,
        sender,
        last_motion.clone(),
    )));
    // A client choosing the codec sends SelectCodec first. Older clients send
    // something else (at least a heartbeat within INPUT_TIMEOUT) and get the
    // codec from their connection request, as before.
    let first = tokio::time::timeout(protocol::INPUT_TIMEOUT, events.receiver.recv()).await;
    // Older clients play Opus from the "opus" track only.
    let mut audio_settings = protocol::AudioSettings {
        format: protocol::AudioFormat::Opus,
        bitrate: options.audio_bitrate,
    };
    let chosen_by_client = match first {
        Ok(Some(Event::SelectCodec {
            codec,
            encoder_preset,
            audio,
        })) => {
            if let Some(preset) = encoder_preset {
                options.encoder_preset = preset;
            }
            if let Some(audio) = audio {
                audio_settings = audio;
            }
            ensure!(
                options.dynamic_range != protocol::DynamicRange::Hdr10
                    || codec == protocol::VideoCodec::H265,
                "HDR10 requires H.265"
            );
            options.codec = codec;
            true
        }
        Ok(Some(event)) => {
            events.deferred.push_back(event);
            false
        }
        // The input task reports a closed or silent input stream.
        Ok(None) | Err(_) => false,
    };
    tracing::info!(
        codec = options.codec.label(),
        chosen_by_client,
        encoder_preset = options.encoder_preset.label(),
        hardware = ?options.hardware_codecs,
        "session video codec"
    );
    let video = video_tracks
        .into_iter()
        .find_map(|(codec, track)| (codec == options.codec).then_some(track))
        .context("no track for the session codec")?;
    let monitor_tracks = monitor_codec_tracks
        .into_iter()
        .find_map(|(codec, tracks)| (codec == options.codec).then_some(tracks))
        .context("no monitor tracks for the session codec")?;
    let mut pipeline = start_encoder(
        options,
        capture.pipeline_source_for_range(options.dynamic_range)?,
        (info.width, info.height),
        options.fps,
        options.bitrate,
        &video,
    )
    .await
    .context("encoder start failed")??;
    let audio = match options.audio_source.as_deref() {
        Some(source) => {
            let track = audio_tracks
                .into_iter()
                .find_map(|(format, track)| (format == audio_settings.format).then_some(track))
                .context("no track for the session audio format")?;
            tracing::info!(audio = audio_settings.label(), "session desktop audio");
            Some(crate::audio::capture(source, audio_settings, track)?)
        }
        None => None,
    };
    tracing::info!(
        width = info.width,
        height = info.height,
        fps = options.fps,
        "native client connected"
    );
    let mut rates = Rates::new(options, &info);
    let mut adaptive = !options.fixed_bitrate;
    let mut last_feedback = Instant::now();
    let mut last_switch = Instant::now() - Duration::from_secs(1);
    let mut last_configure = Instant::now() - Duration::from_secs(1);
    let mut encoder_metrics = pipeline.encoder_metrics()?;
    let mut telemetry_enabled = false;
    let mut last_telemetry = Instant::now();
    let mut input_failures = 0_u32;
    let mut last_keyframe: BTreeMap<Option<usize>, Instant> = BTreeMap::new();
    // Set once the client declares it requests keyframes on loss; every encoder
    // in this session then spaces periodic IDRs LONG_KEY_INTERVAL_SECONDS apart.
    let mut long_gop = false;
    // Set when the compositor drops our portal session (e.g. all outputs
    // vanished while sleeping monitors woke). Recovered in place below.
    let mut portal_lost = false;
    let mut portal_recoveries: Vec<Instant> = Vec::new();
    // Cursor shapes for a client drawing the pointer locally: XFixes (X11)
    // or PipeWire cursor metadata (portal). Dropping an X11 watcher at
    // session end stops its thread.
    let mut cursor_watcher: Option<CursorFeed> = None;
    // The client takes host pointer positions (portal metadata only).
    let mut cursor_positions = false;
    // Capture must restart for the client's cursor choice: `Some(true)`
    // leaves the pointer out of the video, `Some(false)` draws it in again.
    let mut cursor_restart: Option<bool> = None;
    // A portal session still in metadata mode from an earlier client: the
    // pointer goes back into the video unless this client opts in by then.
    let mut cursor_choice_deadline = capture
        .cursor_metadata()
        .then(|| Instant::now() + CURSOR_CHOICE_TIMEOUT);
    let mut position_tick = tokio::time::interval(CURSOR_POSITION_INTERVAL);
    position_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // The client's latest display (size, refresh, choices). Applied once the
    // video-settings rate limit allows, coalescing bursts into the newest.
    let mut client_display: Option<protocol::ClientDisplay> = None;
    let mut display_pending = false;
    // Feedback from the virtual pads (rumble, PlayStation lights and trigger
    // effects), once the client plays it.
    let mut rumble: Option<tokio::sync::mpsc::UnboundedReceiver<crate::gamepad::PadFeedback>> =
        None;
    // Privacy mode for this session: dropping it (however the session ends)
    // turns the monitors back on while the virtual output still exists.
    let mut privacy: Option<crate::privacy::Privacy> = None;
    let mut privacy_check = Instant::now();
    let mut monitors_check = Instant::now();
    // Capture changes waiting for a full restart (privacy, virtual resize).
    let mut display_change = (options.privacy_mode == PrivacyMode::Always
        && options.privacy_available)
        .then_some(DisplayChange::Privacy);
    // The client asked for a virtual monitor: added (and kept at its display
    // size) by the display handler.
    let mut virtual_requested = false;
    // The client's microphone, played into the virtual microphone (G20).
    // Dropped (and the virtual microphone removed) when the session ends.
    let mut microphone: Option<SessionMicrophone> = None;
    loop {
        if cursor_restart.is_some() || display_change.is_some() {
            let local = cursor_restart.take();
            let change = display_change.take();
            // Restart capture as for a monitor switch: without the pointer
            // (X11), or in the other portal cursor mode (a new ScreenCast
            // session, so every stream stops first).
            capture.release_all().await;
            while_starting(retire(pipeline), &mut events, capture, &mut updates).await?;
            let active: Vec<_> = monitor_streams.keys().copied().collect();
            while_starting(
                retire(std::mem::take(&mut monitor_streams)),
                &mut events,
                capture,
                &mut updates,
            )
            .await?;
            if let Some(local) = local {
                if capture.is_portal() {
                    let mut attempt = 0;
                    while let Err(error) = capture.set_cursor_metadata(local).await {
                        attempt += 1;
                        ensure!(
                            attempt < PORTAL_REOPEN_ATTEMPTS,
                            error.context("changing the portal cursor mode")
                        );
                        tracing::warn!(
                            attempt,
                            error = format!("{error:#}"),
                            "desktop portal not ready yet; retrying"
                        );
                        tokio::time::sleep(Duration::from_secs(1)).await;
                    }
                } else {
                    capture.local_cursor = local;
                }
            }
            let client_size = client_display.as_ref().map(Size::for_client);
            match &change {
                Some(DisplayChange::Privacy) => {
                    let text =
                        match enter_privacy(capture, client_size, &options.privacy_state).await {
                            Ok(session) => {
                                let text = format!(
                                    "Privacy mode on: the host's monitors are off ({})",
                                    session.monitors()
                                );
                                privacy = Some(session);
                                privacy_check = Instant::now();
                                text
                            }
                            Err(error) => {
                                tracing::warn!(error = format!("{error:#}"), "privacy mode failed");
                                format!("Privacy mode unavailable: {error:#}")
                            }
                        };
                    publish_update(&mut updates, Update::Notice { text })?;
                }
                Some(DisplayChange::Resize(size)) => {
                    if let Err(error) = capture.resize_virtual(0, *size).await {
                        tracing::warn!(
                            error = format!("{error:#}"),
                            "virtual display resize failed"
                        );
                    }
                }
                None => {}
            }
            // A virtual output may have a new size: fit the client again.
            if change.is_some()
                && let Some(display) = &client_display
            {
                let choice = negotiate_stream(display, capture);
                (options.width, options.fps, options.bitrate) =
                    (choice.width, choice.fps, choice.bitrate);
            }
            // Recreating the portal session removed the client's virtual
            // monitor: the display handler adds it again.
            if virtual_requested && capture.virtual_monitor().is_none() {
                display_pending = true;
            }
            rates.restore(options);
            video_start_group = video_barrier(&video)?;
            let info = desktop(options, capture, video_start_group)?;
            pipeline = while_starting(
                start_encoder(
                    options,
                    capture.pipeline_source_for_range(options.dynamic_range)?,
                    (info.width, info.height),
                    options.fps,
                    options.bitrate,
                    &video,
                ),
                &mut events,
                capture,
                &mut updates,
            )
            .await?;
            if long_gop {
                pipeline.set_key_interval(options.fps.saturating_mul(LONG_KEY_INTERVAL_SECONDS));
            }
            for index in active {
                match monitor_encoder(
                    options,
                    capture,
                    index,
                    &monitor_tracks[index],
                    &mut events,
                    &mut updates,
                )
                .await
                {
                    Ok(stream) => {
                        publish_update(
                            &mut updates,
                            Update::MonitorStream {
                                index,
                                desktop: stream.desktop.clone(),
                            },
                        )?;
                        if long_gop {
                            stream.pipeline.set_key_interval(
                                options.fps.saturating_mul(LONG_KEY_INTERVAL_SECONDS),
                            );
                        }
                        monitor_streams.insert(index, stream);
                    }
                    Err(error) => publish_update(
                        &mut updates,
                        Update::MonitorStreamStopped {
                            index,
                            reason: format!("Monitor restart failed: {error}"),
                            requested: false,
                        },
                    )?,
                }
            }
            rates = Rates::new(options, &info);
            encoder_metrics = pipeline.encoder_metrics()?;
            adaptive = !options.fixed_bitrate;
            last_feedback = Instant::now();
            publish_update(&mut updates, Update::Desktop { desktop: info })?;
            // Portal shapes arrive through the feed (`pw_cursor::subscribe`).
            if local == Some(true)
                && let Some(CursorFeed::X11(watcher)) = cursor_watcher.as_mut()
                && let Some(cursor) = watcher.shapes.borrow_and_update().clone()
            {
                publish_update(&mut updates, Update::Cursor { cursor })?;
            }
            tracing::info!(
                ?local,
                display = change.is_some(),
                "capture restarted for the client's cursor or display"
            );
        }
        if portal_lost {
            portal_lost = false;
            let now = Instant::now();
            portal_recoveries.retain(|at| now.duration_since(*at) < Duration::from_secs(60));
            ensure!(
                portal_recoveries.len() < MAX_PORTAL_RECOVERIES_PER_MINUTE,
                "desktop portal session keeps failing; check the compositor and reconnect"
            );
            portal_recoveries.push(now);
            tracing::warn!(
                "desktop portal session lost (displays changed or session revoked); recreating it"
            );
            capture.release_all().await;
            // Stop the dead PipeWire stream before opening the new session.
            while_starting(retire(pipeline), &mut events, capture, &mut updates).await?;
            for index in std::mem::take(&mut monitor_streams).into_keys() {
                publish_update(
                    &mut updates,
                    Update::MonitorStreamStopped {
                        index,
                        reason: "Host display changed".into(),
                        requested: false,
                    },
                )?;
            }
            // Waking displays hotplug for a few seconds; until they are back the
            // compositor has nothing to share, so keep retrying. Input heartbeats
            // are buffered meanwhile and QUIC keep-alives run on other threads.
            let mut attempt = 0;
            loop {
                attempt += 1;
                match capture.reopen().await {
                    Ok(()) => break,
                    Err(error) if attempt < PORTAL_REOPEN_ATTEMPTS => {
                        tracing::warn!(
                            attempt,
                            error = format!("{error:#}"),
                            "desktop portal not ready yet; retrying"
                        );
                        tokio::time::sleep(Duration::from_secs(1)).await;
                    }
                    Err(error) => {
                        return Err(error.context("recreating the desktop portal session"));
                    }
                }
            }
            rates.restore(options);
            video_start_group = video_barrier(&video)?;
            let info = desktop(options, capture, video_start_group)?;
            pipeline = while_starting(
                start_encoder(
                    options,
                    capture.pipeline_source_for_range(options.dynamic_range)?,
                    (info.width, info.height),
                    options.fps,
                    options.bitrate,
                    &video,
                ),
                &mut events,
                capture,
                &mut updates,
            )
            .await?;
            session_video(&pipeline, options.fps, long_gop);
            rates = Rates::new(options, &info);
            encoder_metrics = pipeline.encoder_metrics()?;
            adaptive = !options.fixed_bitrate;
            last_feedback = Instant::now();
            input_failures = 0;
            publish_update(&mut updates, Update::Desktop { desktop: info })?;
            publish_update(
                &mut updates,
                Update::Notice {
                    text: "Host displays changed; video restarted".into(),
                },
            )?;
            if virtual_requested && capture.virtual_monitor().is_none() {
                display_pending = true;
            }
            // Still no monitor the portal would share: ask again in a minute,
            // not every few seconds (each try restarts the video).
            if capture.fallback_virtual() {
                monitors_check = Instant::now() + Duration::from_secs(55);
            }
            tracing::info!("desktop portal session recreated");
        }
        tokio::select! {
            result = crate::ssh_agent::closed(&mut ssh_agent) => return result,
            result = &mut incoming.0 => return result.context("input task failed")?,
            effect = async {
                match rumble.as_mut() {
                    Some(receiver) => receiver.recv().await,
                    None => std::future::pending().await,
                }
            } => match effect {
                Some(feedback) => publish_update(&mut updates, pad_feedback_update(feedback))?,
                None => rumble = None,
            },
            error = session.closed() => anyhow::bail!("{error}"),
            shape = async {
                match cursor_watcher.as_mut() {
                    Some(feed) => feed.shapes().changed().await,
                    None => std::future::pending().await,
                }
            } => {
                let shape = match (shape, cursor_watcher.as_mut()) {
                    (Ok(()), Some(feed)) => feed.shapes().borrow_and_update().clone(),
                    _ => {
                        // The pointer stays out of the video; the client keeps its last shape.
                        cursor_watcher = None;
                        publish_update(&mut updates, Update::Notice { text: "Host cursor updates stopped".into() })?;
                        None
                    }
                };
                if let Some(cursor) = shape {
                    publish_update(&mut updates, Update::Cursor { cursor })?;
                }
            },
            // Latest host pointer position, rate limited (portal metadata).
            _ = position_tick.tick(), if cursor_positions => {
                if let Some(CursorFeed::Portal(feed)) = cursor_watcher.as_mut()
                    && feed.positions.has_changed().unwrap_or(false)
                    && let Some(position) = *feed.positions.borrow_and_update() {
                    publish_update(&mut updates, Update::CursorPosition { monitor: position.monitor, x: position.x, y: position.y })?;
                }
            },
            // The client never opted in: draw the pointer into the video again.
            _ = tokio::time::sleep_until(cursor_choice_deadline.unwrap_or_else(Instant::now).into()), if cursor_choice_deadline.is_some() => {
                cursor_choice_deadline = None;
                if capture.cursor_metadata() && cursor_watcher.is_none() {
                    tracing::info!("client did not choose a local cursor; embedding the pointer in the video");
                    cursor_restart = Some(false);
                }
            },
            _ = tokio::time::sleep_until((last_configure + CONFIGURE_INTERVAL).into()), if display_pending => {
                display_pending = false;
                let size = client_display.as_ref().map(Size::for_client);
                if virtual_requested && capture.virtual_monitor().is_none() {
                    let text = match capture.add_virtual_monitor(size.unwrap_or(Size::DEFAULT)).await {
                        Ok(index) => {
                            publish_update(&mut updates, Update::Desktop { desktop: desktop(options, capture, video_start_group)? })?;
                            format!("Virtual monitor added as monitor {}", index + 1)
                        }
                        Err(error) => {
                            virtual_requested = false;
                            tracing::warn!(error = format!("{error:#}"), "virtual monitor failed");
                            format!("Virtual monitor unavailable: {error:#}")
                        }
                    };
                    publish_update(&mut updates, Update::Notice { text })?;
                } else if let (Some(size), Some(index)) = (size, capture.virtual_monitor())
                    && capture.virtual_monitor_resizable()
                    && (capture.monitors[index].width, capture.monitors[index].height) != (size.width, size.height)
                {
                    // Follow the client's display: stop its stream, resize, restart it.
                    let running = monitor_streams.remove(&index);
                    let was_running = running.is_some();
                    if let Some(stream) = running { while_starting(retire(stream), &mut events, capture, &mut updates).await?; }
                    if let Err(error) = capture.resize_virtual(index, size).await {
                        tracing::warn!(error = format!("{error:#}"), "virtual monitor resize failed");
                    }
                    publish_update(&mut updates, Update::Desktop { desktop: desktop(options, capture, video_start_group)? })?;
                    if was_running {
                        match monitor_encoder(options, capture, index, &monitor_tracks[index], &mut events, &mut updates).await {
                            Ok(stream) => {
                                publish_update(&mut updates, Update::MonitorStream { index, desktop: stream.desktop.clone() })?;
                                if long_gop { stream.pipeline.set_key_interval(options.fps.saturating_mul(LONG_KEY_INTERVAL_SECONDS)); }
                                monitor_streams.insert(index, stream);
                            }
                            Err(error) => publish_update(&mut updates, Update::MonitorStreamStopped { index, reason: format!("Monitor restart failed: {error}"), requested: false })?,
                        }
                    }
                }
                // A virtual primary display takes the client's size and
                // refresh rate (a full restart, which negotiates again).
                if let (Some(size), Some(current)) = (size, capture.primary_virtual())
                    && size != current
                    && display_change.is_none()
                {
                    last_configure = Instant::now();
                    display_change = Some(DisplayChange::Resize(size));
                    continue;
                }
                if let Some(requested) = client_display {
                    // The portal states no refresh rate, but the running
                    // screencast does (KWin caps it at the monitor's).
                    let active = capture.active_monitor;
                    if capture.monitors[active].refresh_mhz == 0
                        && let Some(refresh) = pipeline.source_refresh_mhz()
                    {
                        capture.monitors[active].refresh_mhz = refresh;
                    }
                    let choice = negotiate_stream(&requested, capture);
                    tracing::info!(client = ?requested, ?choice, "negotiated stream for client display");
                    // Through the ConfigureVideo path: a bitrate-only change
                    // retargets the encoder in place, anything else restarts
                    // it off the session loop; either way the client gets
                    // `Update::Desktop`.
                    events.deferred.push_back(Event::ConfigureVideo { width: choice.width, fps: choice.fps, bitrate: choice.bitrate });
                }
            },
            _ = check.tick() => {
                // The compositor closed the portal session (e.g. monitors
                // woke from power saving): recreate it before input fails.
                if capture.is_portal() && capture.is_lost() {
                    portal_lost = true;
                    continue;
                }
                if let Err(error) = pipeline.error() {
                    // On Wayland a capture failure is usually the portal stream
                    // going away with its outputs; try a fresh session first.
                    ensure!(capture.is_portal(), error);
                    tracing::warn!(error = format!("{error:#}"), "capture pipeline failed");
                    portal_lost = true;
                    continue;
                }
                // Streaming a virtual display for lack of a monitor: switch
                // to the monitors once one is back (KDE; elsewhere the
                // session keeps the virtual display).
                if capture.fallback_virtual() && monitors_check.elapsed() >= MONITORS_CHECK_INTERVAL {
                    monitors_check = Instant::now();
                    let back = tokio::task::spawn_blocking(|| !crate::virtual_display::enabled_physical().is_empty())
                        .await
                        .unwrap_or(false);
                    if back {
                        tracing::info!("a monitor is back; streaming it instead of the virtual display");
                        portal_lost = true;
                        continue;
                    }
                }
                // Monitors that came back while private (plugged in, woken
                // by a hotplug) go off again.
                if let Some(session) = privacy.as_mut()
                    && privacy_check.elapsed() >= PRIVACY_CHECK_INTERVAL
                {
                    privacy_check = Instant::now();
                    match session.enforce().await {
                        Ok(0) => {}
                        Ok(count) => tracing::warn!(count, "privacy mode turned returning monitors off again"),
                        Err(error) => {
                            tracing::warn!(error = format!("{error:#}"), "privacy mode could not keep the monitors off");
                            publish_update(&mut updates, Update::Notice { text: format!("Privacy mode could not keep a host monitor off: {error:#}") })?;
                        }
                    }
                }
                let failed: Vec<_> = monitor_streams.iter().filter_map(|(&index, stream)| stream.pipeline.error().err().map(|error| (index, error))).collect();
                for (index, error) in failed {
                    monitor_streams.remove(&index);
                    capture.release_all().await;
                    publish_update(&mut updates, Update::MonitorStreamStopped { index, reason: format!("Monitor stream stopped: {error}"), requested: false })?;
                }
                if let Some(audio) = &audio { audio.error()?; }
                if let Some(Err(error)) = microphone.as_mut().map(SessionMicrophone::check) {
                    microphone = None;
                    tracing::warn!(error = format!("{error:#}"), "client microphone stopped");
                    publish_update(&mut updates, Update::Notice { text: format!("Microphone stopped on the host: {error:#}") })?;
                }
                if adaptive {
                    let stats = session.stats();
                    // Clients sending NetworkFeedback are rate-controlled from it.
                    if rates.delay.is_none()
                        && let (Some(rtt), Some(sent), Some(lost)) = (stats.rtt, stats.packets_sent, stats.packets_lost)
                        && rates.network.observe(Instant::now(), rtt, sent, lost).is_some() {
                        let bitrate = rates.target();
                        if bitrate != pipeline.video_bitrate()? {
                            match pipeline.set_video_bitrate(bitrate) {
                                Ok(()) => tracing::info!(bitrate, rtt_ms = rtt.as_millis() as u64, "adapted video bitrate to network"),
                                Err(error) => { adaptive = false; tracing::warn!(%error, "encoder does not support live bitrate changes; retaining fixed bitrate"); }
                            }
                        }
                    }
                    if rates.delay.is_some() && let Some(step) = rates.ladder.observe(Instant::now(), rates.target()) {
                        // Restart capture and encoder at the new step, as for
                        // ConfigureVideo, but keep the rate controllers: the
                        // link has not changed.
                        let top = rates.ladder.top();
                        let kbps = rates.target();
                        options.stepped_down = (rates.ladder.level() > 0).then_some(protocol::SteppedDown { width: top.width, fps: top.fps, kbps });
                        (options.width, options.fps) = if rates.ladder.level() == 0 { rates.negotiated } else { (step.width, step.fps) };
                        tracing::info!(width = step.width, fps = step.fps, kbps, level = rates.ladder.level(), "stepped video for the network");
                        capture.release_all().await;
                        while_starting(retire(pipeline), &mut events, capture, &mut updates).await?;
                        video_start_group = video_barrier(&video)?;
                        let info = desktop(options, capture, video_start_group)?;
                        capture.select_monitor(capture.active_monitor).await?;
                        pipeline = while_starting(start_encoder(options, capture.pipeline_source_for_range(options.dynamic_range)?, (info.width, info.height), options.fps, kbps, &video), &mut events, capture, &mut updates).await?;
                        session_video(&pipeline, options.fps, long_gop);
                        encoder_metrics = pipeline.encoder_metrics()?;
                        (rates.encoded_bytes, rates.key_bytes) = (0, 0);
                        publish_update(&mut updates, Update::Desktop { desktop: info })?;
                    }
                }
                if telemetry_enabled && last_telemetry.elapsed() >= Duration::from_secs(1) {
                    last_telemetry = Instant::now();
                    publish_update(&mut updates, Update::Telemetry {
                        encode_us: encoder_metrics.encode_us.load(std::sync::atomic::Ordering::Relaxed),
                        bitrate: pipeline.video_bitrate()?,
                        encoder: encoder_metrics.encoder.clone(),
                    })?;
                }
            },
            event = events.next() => {
                match event.context("input stream closed")? {
                    Event::MonitorStream { index, enabled } => {
                        if !enabled {
                            if let Some(stream) = monitor_streams.remove(&index) { while_starting(retire(stream), &mut events, capture, &mut updates).await?; }
                            capture.release_all().await;
                            publish_update(&mut updates, Update::MonitorStreamStopped { index, reason: "Monitor stream disabled".into(), requested: true })?;
                            continue;
                        }
                        if let Some(stream) = monitor_streams.get(&index) {
                            publish_update(&mut updates, Update::MonitorStream { index, desktop: stream.desktop.clone() })?;
                            continue;
                        }
                        if monitor_streams.len() >= 4 || index >= monitor_tracks.len() {
                            publish_update(&mut updates, Update::MonitorStreamStopped { index, reason: "Monitor unavailable or four additional streams already active".into(), requested: false })?;
                            continue;
                        }
                        match monitor_encoder(options, capture, index, &monitor_tracks[index], &mut events, &mut updates).await {
                            Ok(stream) => {
                                publish_update(&mut updates, Update::MonitorStream { index, desktop: stream.desktop.clone() })?;
                                if long_gop { stream.pipeline.set_key_interval(options.fps.saturating_mul(LONG_KEY_INTERVAL_SECONDS)); }
                                monitor_streams.insert(index, stream);
                            }
                            Err(error) => publish_update(&mut updates, Update::MonitorStreamStopped { index, reason: format!("Monitor unavailable: {error}"), requested: false })?,
                        }
                    }
                    Event::MonitorMotion { index, x, y } => {
                        if monitor_streams.contains_key(&index)
                            && let Err(error) = capture.monitor_motion(index, x, y).await {
                            monitor_streams.remove(&index);
                            capture.release_all().await;
                            publish_update(&mut updates, Update::MonitorStreamStopped { index, reason: format!("Monitor input unavailable: {error}"), requested: false })?;
                        }
                    }
                    // Auxiliary streams currently use fixed bitrate. Their
                    // congestion must never adapt the independent primary.
                    Event::MonitorFeedback { .. } => {}
                    Event::ClientFeatures { keyframe_requests , pointer_datagrams, local_cursor, rumble: plays_rumble, cursor_positions: wants_positions, frame_timing, privacy: wants_privacy, virtual_monitor: wants_virtual, gamepad_lights, microphone: wants_microphone } => {
                        // Every paired client is an owner today (no guest
                        // roles), so any of them may send its microphone.
                        if wants_microphone && microphone.is_none() {
                            let text = if !options.microphone_available {
                                "This host does not take a microphone (teleport host --microphone)".to_owned()
                            } else {
                                match SessionMicrophone::start(controls.clone()).await {
                                    Ok(started) => {
                                        microphone = Some(started);
                                        format!("Your microphone is \"{}\" on the host", crate::virtual_mic::DESCRIPTION)
                                    }
                                    Err(error) => {
                                        tracing::warn!(error = format!("{error:#}"), "client microphone unavailable");
                                        format!("Microphone unavailable on the host: {error:#}")
                                    }
                                }
                            };
                            publish_update(&mut updates, Update::Notice { text })?;
                        }
                        if frame_timing && !options.frame_timing {
                            // Encoders started later inherit it from options.
                            options.frame_timing = true;
                            pipeline.set_frame_timing();
                            for stream in monitor_streams.values() { stream.pipeline.set_frame_timing(); }
                        }
                        if wants_privacy && privacy.is_none() && !capture.privacy() {
                            if options.privacy_available {
                                display_change = Some(DisplayChange::Privacy);
                            } else {
                                publish_update(&mut updates, Update::Notice { text: "Privacy mode is off on this host (teleport host --privacy-mode request)".into() })?;
                            }
                        }
                        if wants_virtual && !virtual_requested {
                            if capture.virtual_monitors_available() {
                                virtual_requested = true;
                                // Added by the display handler, at the client's size.
                                display_pending = true;
                            } else {
                                publish_update(&mut updates, Update::Notice { text: "This host cannot add a virtual monitor".into() })?;
                            }
                        }
                        if (plays_rumble || gamepad_lights) && rumble.is_none() {
                            let (sender, receiver) = tokio::sync::mpsc::unbounded_channel();
                            capture.enable_pad_feedback(sender, plays_rumble, gamepad_lights);
                            rumble = Some(receiver);
                        }
                        if pointer_datagrams && pointer_task.is_none() {
                            pointer_task = Some(AbortOnDrop(tokio::spawn(receive_pointer(controls.clone(), pointer_sender.clone(), last_motion.clone()))));
                        }
                        if keyframe_requests && !long_gop {
                            long_gop = true;
                            let frames = options.fps.saturating_mul(LONG_KEY_INTERVAL_SECONDS);
                            pipeline.set_key_interval(frames);
                            for stream in monitor_streams.values() { stream.pipeline.set_key_interval(frames); }
                        }
                        cursor_choice_deadline = None;
                        let local = local_cursor && capture.supports_local_cursor();
                        if capture.is_portal() {
                            // Shapes come from the capture pipelines' cursor
                            // metadata; a session in the other mode restarts.
                            if local && cursor_watcher.is_none() {
                                cursor_watcher = Some(CursorFeed::Portal(crate::pw_cursor::subscribe()));
                                cursor_positions = wants_positions;
                                tracing::info!("client draws the cursor locally (portal cursor metadata)");
                            }
                            if local != capture.cursor_metadata() {
                                cursor_restart = Some(local);
                            }
                        } else if local && cursor_watcher.is_none() {
                            // Watch first: without shapes the client would have no pointer at all.
                            match crate::x11_cursor::watch() {
                                Ok(watcher) => {
                                    cursor_watcher = Some(CursorFeed::X11(watcher));
                                    cursor_restart = Some(true);
                                    tracing::info!("client draws the cursor locally");
                                }
                                Err(error) => tracing::warn!(error = format!("{error:#}"), "local cursor unavailable; keeping the pointer in the video"),
                            }
                        }
                    }
                    Event::ClientDisplay(display) => {
                        client_display = Some(display);
                        display_pending = true;
                    }
                    // Only the first event chooses (see above); video is running now.
                    Event::SelectCodec { codec, .. } => ensure!(
                        codec == options.codec,
                        "the video codec is chosen before video starts; reconnect to change it"
                    ),
                    Event::RequestKeyframe { monitor } => {
                        // Bound IDR bursts: a lossy link must not turn every
                        // request into a full keyframe and deepen congestion.
                        let now = Instant::now();
                        if last_keyframe.get(&monitor).is_none_or(|at| now.duration_since(*at) >= KEYFRAME_REQUEST_INTERVAL) {
                            last_keyframe.insert(monitor, now);
                            let target = match monitor {
                                None => Some(&pipeline),
                                Some(index) => monitor_streams.get(&index).map(|stream| &stream.pipeline),
                            };
                            if let Some(target) = target
                                && let Err(error) = target.request_keyframe() {
                                tracing::warn!(%error, ?monitor, "keyframe request failed");
                            }
                        }
                    }
                    Event::ConfigureVideo { width, fps, bitrate } => {
                        if last_configure.elapsed() < CONFIGURE_INTERVAL {
                            publish_update(&mut updates, Update::Notice { text: "Video settings changed too quickly; try again".into() })?;
                            continue;
                        }
                        let mut requested = options.clone();
                        requested.width = width;
                        requested.fps = fps;
                        requested.bitrate = bitrate;
                        let mut info = match desktop(&requested, capture, video_start_group) {
                            Ok(info) => info,
                            Err(error) => {
                                publish_update(&mut updates, Update::Notice { text: format!("Unsupported video settings: {error}") })?;
                                continue;
                            }
                        };
                        last_configure = Instant::now();
                        // A bitrate-only change retargets the running encoder (in
                        // place, or reopened in the background): no capture restart,
                        // no gap. Auxiliary monitors keep their fixed bitrate.
                        if width == options.width && fps == options.fps && pipeline.set_video_bitrate(bitrate).is_ok() {
                            *options = requested;
                            rates = Rates::new(options, &info);
                            adaptive = !options.fixed_bitrate;
                            last_feedback = Instant::now();
                            publish_update(&mut updates, Update::Desktop { desktop: info })?;
                            continue;
                        }
                        capture.release_all().await;
                        while_starting(retire(pipeline), &mut events, capture, &mut updates).await?;
                        video_start_group = video_barrier(&video)?;
                        info.video_start_group = video_start_group;
                        capture.select_monitor(capture.active_monitor).await?;
                        pipeline = while_starting(start_encoder(options, capture.pipeline_source_for_range(options.dynamic_range)?, (info.width, info.height), fps, bitrate, &video), &mut events, capture, &mut updates).await?;
                        session_video(&pipeline, fps, long_gop);
                        *options = requested;
                        let active: Vec<_> = monitor_streams.keys().copied().collect();
                        while_starting(retire(std::mem::take(&mut monitor_streams)), &mut events, capture, &mut updates).await?;
                        for index in active {
                            match monitor_encoder(options, capture, index, &monitor_tracks[index], &mut events, &mut updates).await {
                                Ok(stream) => {
                                    publish_update(&mut updates, Update::MonitorStream { index, desktop: stream.desktop.clone() })?;
                                    if long_gop { stream.pipeline.set_key_interval(options.fps.saturating_mul(LONG_KEY_INTERVAL_SECONDS)); }
                                    monitor_streams.insert(index, stream);
                                }
                                Err(error) => publish_update(&mut updates, Update::MonitorStreamStopped { index, reason: format!("Monitor reconfiguration failed: {error}"), requested: false })?,
                            }
                        }
                        encoder_metrics = pipeline.encoder_metrics()?;
                        rates = Rates::new(options, &info);
                        adaptive = !options.fixed_bitrate;
                        last_feedback = Instant::now();
                        publish_update(&mut updates, Update::Desktop { desktop: info })?;
                    }
                    Event::SelectMonitor { index } => {
                        if index >= capture.monitors.len() || last_switch.elapsed() < Duration::from_millis(500) {
                            publish_update(&mut updates, Update::Desktop { desktop: desktop(options, capture, video_start_group)? })?;
                            continue;
                        }
                        last_switch = Instant::now();
                        capture.release_all().await;
                        // Stop the old source before opening another PipeWire node.
                        while_starting(retire(pipeline), &mut events, capture, &mut updates).await?;
                        video_start_group = video_barrier(&video)?;
                        capture.select_monitor(index).await?;
                        rates.restore(options);
                        if let Some(display) = &client_display {
                            // Fit the new monitor's size and refresh rate.
                            let choice = negotiate_stream(display, capture);
                            (options.width, options.fps, options.bitrate) = (choice.width, choice.fps, choice.bitrate);
                        }
                        let info = desktop(options, capture, video_start_group)?;
                        pipeline = while_starting(start_encoder(options, capture.pipeline_source_for_range(options.dynamic_range)?, (info.width, info.height), options.fps, options.bitrate, &video), &mut events, capture, &mut updates).await?;
                        session_video(&pipeline, options.fps, long_gop);
                        rates = Rates::new(options, &info);
                        encoder_metrics = pipeline.encoder_metrics()?;
                        adaptive = !options.fixed_bitrate;
                        last_feedback = Instant::now();
                        publish_update(&mut updates, Update::Desktop { desktop: info })?;
                    }
                    Event::Feedback { queue_ms, dropped_groups } => {
                        if adaptive && last_feedback.elapsed() >= Duration::from_millis(900) {
                            last_feedback = Instant::now();
                            if rates.adaptation.observe(queue_ms, dropped_groups).is_some()
                                && rates.target() != pipeline.video_bitrate()? {
                                let bitrate = rates.target();
                                match pipeline.set_video_bitrate(bitrate) {
                                    Ok(()) => { tracing::info!(bitrate, queue_ms, dropped_groups, "adapted video bitrate"); },
                                    Err(error) => { adaptive = false; tracing::warn!(%error, "encoder does not support live bitrate changes; retaining fixed bitrate"); }
                                }
                            }
                        }
                    }
                    Event::NetworkFeedback { delay_ms, received_kbps } => {
                        if adaptive {
                            let stats = session.stats();
                            let sizes = *encoder_metrics.sizes.lock().unwrap();
                            if let Some(bitrate) = rates.feedback(Instant::now(), delay_ms, received_kbps, sizes, stats.packets_sent.zip(stats.packets_lost))
                                && bitrate != pipeline.video_bitrate()? {
                                match pipeline.set_video_bitrate(bitrate) {
                                    Ok(()) => tracing::info!(bitrate, ?delay_ms, received_kbps, capacity = ?rates.delay.as_ref().and_then(|rate| rate.capacity()), "adapted video bitrate to network"),
                                    Err(error) => { adaptive = false; tracing::warn!(%error, "encoder does not support live bitrate changes; retaining fixed bitrate"); }
                                }
                            }
                        }
                    }
                    Event::Clipboard { text } if options.clipboard => {
                        let text = match crate::clipboard::write(&text).await { Ok(()) => "Clipboard sent to host".into(), Err(error) => format!("Clipboard unavailable: {error}") };
                        publish_update(&mut updates, Update::Notice { text })?;
                    }
                    Event::ClipboardRequest if options.clipboard => {
                        let update = match crate::clipboard::read().await { Ok(text) => Update::Clipboard { text }, Err(error) => Update::Notice { text: format!("Clipboard unavailable: {error}") } };
                        publish_update(&mut updates, update)?;
                    }
                    Event::Clipboard { .. } | Event::ClipboardRequest => publish_update(&mut updates, Update::Notice { text: "Host clipboard is disabled; start host with --clipboard to opt in".into() })?,
                    Event::SecureAttention => {
                        let text = secure_attention(options, capture).await;
                        publish_update(&mut updates, Update::Notice { text })?;
                    }
                    Event::Probe { id } => {
                        telemetry_enabled = true;
                        let host_us = options.frame_timing.then(|| protocol::clock_us(Instant::now()));
                        publish_update(&mut updates, Update::Pong { id, host_us })?;
                    },
                    event => {
                        // Invalid events still fail closed. An injection error
                        // (portal/D-Bus hiccup) drops that one event; only a
                        // persistently broken input path ends the session.
                        event.validate()?;
                        flash_on_input(&event);
                        match capture.input(event).await {
                            Ok(()) => input_failures = 0,
                            Err(error) if crate::capture::portal_session_lost(&error) => {
                                // Injecting into a dead session only fails again (and
                                // KDE may revoke it entirely): recreate it now.
                                tracing::warn!(error = format!("{error:#}"), "portal rejected input");
                                portal_lost = true;
                            }
                            Err(error) => {
                                input_failures += 1;
                                ensure!(
                                    input_failures < MAX_CONSECUTIVE_INPUT_FAILURES,
                                    "host input injection failed {input_failures} consecutive times: {error:#}"
                                );
                                tracing::warn!(%error, input_failures, "dropping input event that could not be injected");
                                if input_failures == 1 {
                                    publish_update(&mut updates, Update::Notice { text: format!("Host could not inject input: {error}") })?;
                                }
                            }
                        }
                    }
                }
            },
        }
    }
}

/// Applied to every new primary encoder: long GOPs once the client requests
/// keyframes itself.
/// `--flash-on-input`: each key or mouse-button press switches the test
/// picture between black and white.
fn flash_on_input(event: &Event) {
    if matches!(
        event,
        Event::Key { down: true, .. } | Event::Button { down: true, .. }
    ) {
        media::test_flash::toggle();
    }
}

fn session_video(pipeline: &media::Pipeline, fps: u32, long_gop: bool) {
    if long_gop {
        pipeline.set_key_interval(fps.saturating_mul(LONG_KEY_INTERVAL_SECONDS));
    }
}

/// Bitrate control of the primary stream: client decoder backlog, the
/// network (client delay feedback, or the host's QUIC RTT for clients that
/// send none), and the resolution/FPS ladder under the negotiated stream.
struct Rates {
    adaptation: media::BitrateController,
    network: media::NetworkRateController,
    delay: Option<crate::congestion::DelayRateController>,
    ladder: crate::congestion::Ladder,
    /// `options.width`/`fps` as negotiated, restored at the top of the ladder.
    negotiated: (u32, u32),
    /// The user's (or negotiated) bitrate: never exceeded.
    ceiling: u32,
    started: Instant,
    /// Encoder output and QUIC packet counters at the last feedback.
    encoded_bytes: u64,
    key_bytes: u64,
    feedback_at: Instant,
    packets: Option<(u64, u64)>,
}

impl Rates {
    /// For a newly negotiated stream (`info` is its `Desktop`).
    fn new(options: &mut Options, info: &Desktop) -> Self {
        options.stepped_down = None;
        let now = Instant::now();
        Self {
            adaptation: media::BitrateController::new(options.bitrate),
            network: media::NetworkRateController::new(options.bitrate, now),
            delay: None,
            ladder: crate::congestion::Ladder::new(
                info.width,
                info.height,
                options.fps,
                options.codec == protocol::VideoCodec::H265,
            ),
            negotiated: (options.width, options.fps),
            ceiling: options.bitrate,
            started: now,
            // Unknown until the first feedback.
            encoded_bytes: u64::MAX,
            key_bytes: u64::MAX,
            feedback_at: now,
            packets: None,
        }
    }

    /// Back to the negotiated stream before a restart that keeps it (the
    /// new `Rates` takes `options` as the negotiated stream).
    fn restore(&self, options: &mut Options) {
        if options.stepped_down.take().is_some() {
            (options.width, options.fps) = self.negotiated;
        }
    }

    /// The bitrate the encoder should run at.
    fn target(&self) -> u32 {
        let network = match &self.delay {
            Some(delay) => delay.current(),
            None => self.network.current(),
        };
        self.adaptation.current().min(network)
    }

    /// `Event::NetworkFeedback`; `encoded` is the encoder's cumulative output
    /// (bytes) and `packets` the QUIC (sent, lost) counters. Returns the new
    /// target when it changed.
    fn feedback(
        &mut self,
        now: Instant,
        delay_ms: Option<u32>,
        received_kbps: u32,
        sizes: media::PacketSizes,
        packets: Option<(u64, u64)>,
    ) -> Option<u32> {
        let encoded = sizes.total_bytes();
        let keyframe_bytes = sizes.key_bytes.saturating_sub(self.key_bytes);
        self.key_bytes = sizes.key_bytes;
        if self.delay.is_none() {
            self.delay = Some(crate::congestion::DelayRateController::new(
                self.ceiling,
                self.started,
            ));
        }
        let elapsed = now.duration_since(self.feedback_at).as_millis().max(1) as u64;
        // A restarted encoder counts from zero.
        // Encoders that do not count their output (legacy GStreamer) are
        // taken to produce their target.
        let encoded_kbps = if encoded == 0 {
            self.target()
        } else {
            (encoded.saturating_sub(self.encoded_bytes) * 8 / elapsed) as u32
        };
        self.encoded_bytes = encoded;
        self.feedback_at = now;
        let heavy_loss = match (packets, self.packets) {
            (Some((sent, lost)), Some((sent_before, lost_before))) => {
                let (sent, lost) = (
                    sent.saturating_sub(sent_before),
                    lost.saturating_sub(lost_before),
                );
                sent >= 50 && lost * 10 > sent
            }
            _ => false,
        };
        self.packets = packets;
        let before = self.target();
        if let Some(delay) = &mut self.delay {
            delay.observe(
                now,
                delay_ms,
                received_kbps,
                encoded_kbps,
                keyframe_bytes,
                heavy_loss,
            );
        }
        let after = self.target();
        (after != before).then_some(after)
    }
}

fn desktop(options: &Options, capture: &Capture, video_start_group: u64) -> Result<Desktop> {
    desktop_for_monitor(options, capture, capture.active_monitor, video_start_group)
}

fn desktop_for_monitor(
    options: &Options,
    capture: &Capture,
    index: usize,
    video_start_group: u64,
) -> Result<Desktop> {
    let monitor = capture.monitors.get(index).context("monitor unavailable")?;
    let width = if options.width == 0 {
        monitor.width
    } else {
        options.width
    } / 2
        * 2;
    let height =
        ((width as u64 * monitor.height as u64 / monitor.width as u64) as u32 / 2 * 2).max(2);
    protocol::validate_video_size(width, height)?;
    Ok(Desktop {
        multimonitor: true,
        ssh_agent: options.allow_ssh_agent,
        dynamic_range: options.dynamic_range,
        codec: options.codec,
        codecs: vec![protocol::VideoCodec::H264, protocol::VideoCodec::H265],
        version: protocol::VERSION,
        width,
        height,
        fps: options.fps,
        source: capture.name.into(),
        monitors: capture.monitors.clone(),
        active_monitor: index,
        audio: options.audio_source.is_some(),
        clipboard: options.clipboard,
        telemetry: true,
        configurable_video: true,
        native_width: monitor.width,
        native_height: monitor.height,
        bitrate: options.bitrate,
        video_start_group,
        keyframe_requests: true,
        pointer_datagrams: true,
        relative_pointer: true,
        local_cursor: capture.supports_local_cursor(),
        gamepads: capture.gamepads_available(),
        gamepad_types: capture.gamepad_types(),
        max_fps: protocol::MAX_FPS,
        display_negotiation: true,
        codec_selection: true,
        hardware_codecs: options.hardware_codecs.clone(),
        extra_mouse_buttons: true,
        encoder_presets: true,
        encoder_preset: options.encoder_preset,
        // `TELEPORT_NETWORK_FEEDBACK=0`: the RTT-only controller, no step-down
        // (for comparisons).
        network_feedback: std::env::var_os("TELEPORT_NETWORK_FEEDBACK")
            .is_none_or(|value| value != "0"),
        stepped_down: options.stepped_down,
        frame_timing: true,
        virtual_monitor: capture.virtual_monitors_available(),
        privacy_mode: options.privacy_available,
        privacy_active: capture.privacy(),
        microphone: options.microphone_available,
        audio_options: true,
        audio_settings: protocol::AudioSettings {
            format: options.audio_format,
            bitrate: options.audio_bitrate,
        },
        wake: options.wake.clone(),
        secure_attention: options.secure_attention,
    })
}

/// Where a locally drawn cursor's shapes come from.
enum CursorFeed {
    X11(crate::x11_cursor::Watcher),
    Portal(crate::pw_cursor::Feed),
}

impl CursorFeed {
    fn shapes(&mut self) -> &mut tokio::sync::watch::Receiver<crate::x11_cursor::Shape> {
        match self {
            Self::X11(watcher) => &mut watcher.shapes,
            Self::Portal(feed) => &mut feed.shapes,
        }
    }
}

struct MonitorEncoder {
    // Declaration order deliberately stops the pipeline before closing its fd.
    pipeline: media::Pipeline,
    _source: crate::capture::MonitorSource,
    desktop: Desktop,
}

async fn monitor_encoder(
    options: &Options,
    capture: &mut Capture,
    index: usize,
    track: &moq_net::track::Producer,
    events: &mut SessionEvents,
    updates: &mut moq_net::track::Producer,
) -> Result<MonitorEncoder> {
    let info = desktop_for_monitor(options, capture, index, video_barrier(track)?)?;
    let source = capture.monitor_source(index, options.dynamic_range).await?;
    let pipeline = while_starting(
        start_encoder(
            options,
            source.pipeline.clone(),
            (info.width, info.height),
            options.fps,
            options.bitrate,
            track,
        ),
        events,
        capture,
        updates,
    )
    .await?;
    Ok(MonitorEncoder {
        pipeline,
        _source: source,
        desktop: info,
    })
}

/// Input events from the client, plus control events set aside while an
/// encoder was starting (handled first, in arrival order).
struct SessionEvents {
    receiver: tokio::sync::mpsc::Receiver<Event>,
    deferred: std::collections::VecDeque<Event>,
}

impl SessionEvents {
    async fn next(&mut self) -> Option<Event> {
        match self.deferred.pop_front() {
            Some(event) => Some(event),
            None => self.receiver.recv().await,
        }
    }
}

/// Opens an encoder (hardware session, probing, capture start; NVENC takes
/// ~0.5 s) on a blocking thread instead of the session task.
fn start_encoder(
    options: &Options,
    source: String,
    size: (u32, u32),
    fps: u32,
    bitrate: u32,
    track: &moq_net::track::Producer,
) -> tokio::task::JoinHandle<Result<media::Pipeline>> {
    let (track, kind) = (
        track.clone(),
        media::EncoderChoice {
            kind: options.encoder,
            preset: options.encoder_preset,
            frame_timing: options.frame_timing,
        },
    );
    let format = media::VideoFormat {
        codec: options.codec,
        dynamic_range: options.dynamic_range,
    };
    tokio::task::spawn_blocking(move || {
        media::encoder(&source, size, fps, bitrate, track, kind, format)
    })
}

/// Stops pipelines on a blocking thread: closing an NVENC session takes about
/// half a second, during which the session task would not read input.
fn retire<T: Send + 'static>(old: T) -> tokio::task::JoinHandle<Result<()>> {
    tokio::task::spawn_blocking(move || {
        drop(old);
        Ok(())
    })
}

/// Waits for `work` while still injecting input and answering probes, so a
/// quality, monitor or cursor change does not freeze the remote pointer while
/// an encoder stops or starts. Other events wait in `events.deferred` for the
/// session loop. Rate feedback and keyframe requests do not hold up input;
/// anything else (clipboard, monitor and video changes) does, to keep order.
/// An input event that fails to inject is also left to the loop, which
/// retries it with its error handling.
async fn while_starting<T: Send + 'static>(
    work: tokio::task::JoinHandle<Result<T>>,
    events: &mut SessionEvents,
    capture: &mut Capture,
    updates: &mut moq_net::track::Producer,
) -> Result<T> {
    fn reorderable(event: &Event) -> bool {
        matches!(
            event,
            Event::Feedback { .. }
                | Event::NetworkFeedback { .. }
                | Event::MonitorFeedback { .. }
                | Event::RequestKeyframe { .. }
        )
    }
    let mut ordered = events.deferred.iter().any(|event| !reorderable(event));
    tokio::pin!(work);
    loop {
        tokio::select! {
            result = &mut work => return result.context("encoder task failed")?,
            event = events.receiver.recv(), if !ordered => {
                let Some(event) = event else {
                    // The session loop reports the closed stream.
                    return work.await.context("encoder task failed")?;
                };
                match event {
                    // No host clock here; the client's next probe gets one.
                    Event::Probe { id } => publish_update(updates, Update::Pong { id, host_us: None })?,
                    event if reorderable(&event) => events.deferred.push_back(event),
                    Event::MonitorStream { .. }
                    | Event::MonitorMotion { .. }
                    | Event::ClientFeatures { .. }
                    | Event::ConfigureVideo { .. }
                    | Event::ClientDisplay(_)
                    | Event::SelectCodec { .. }
                    | Event::SelectMonitor { .. }
                    | Event::Clipboard { .. }
                    | Event::ClipboardRequest
                    | Event::SecureAttention => {
                        ordered = true;
                        events.deferred.push_back(event);
                    }
                    event => {
                        if event.validate().is_err() || capture.input(event.clone()).await.is_err() {
                            ordered = true;
                            events.deferred.push_back(event);
                        } else {
                            flash_on_input(&event);
                        }
                    }
                }
            }
        }
    }
}

fn requested_format(
    query: Option<&str>,
    default: protocol::VideoCodec,
) -> Result<media::VideoFormat> {
    let query = query.unwrap_or_default();
    ensure!(query.len() <= 128, "video request too large");
    let mut codec = None;
    let mut range = None;
    let mut ssh_agent = false;
    for (key, value) in url::form_urlencoded::parse(query.as_bytes()) {
        match key.as_ref() {
            "ssh-agent" if !ssh_agent && value == "1" => ssh_agent = true,
            "codec" if codec.is_none() => {
                codec = Some(match value.as_ref() {
                    "h264" => protocol::VideoCodec::H264,
                    "h265" => protocol::VideoCodec::H265,
                    _ => anyhow::bail!("unsupported codec"),
                })
            }
            "range" if range.is_none() => {
                range = Some(match value.as_ref() {
                    "sdr" => protocol::DynamicRange::Sdr,
                    "hdr10" => protocol::DynamicRange::Hdr10,
                    _ => anyhow::bail!("unsupported dynamic range"),
                })
            }
            _ => anyhow::bail!("unknown or duplicate video request field"),
        }
    }
    let format = media::VideoFormat {
        codec: codec.unwrap_or(default),
        dynamic_range: range.unwrap_or_default(),
    };
    ensure!(
        format.dynamic_range != protocol::DynamicRange::Hdr10
            || format.codec == protocol::VideoCodec::H265,
        "HDR10 requires H.265"
    );
    Ok(format)
}

/// Reserve a completed, empty group after the old pipeline has stopped. All
/// frames from the next encoder have a strictly greater group sequence, even
/// when the dimensions are unchanged or video/control delivery reorders.
fn video_barrier(video: &moq_net::track::Producer) -> Result<u64> {
    let mut barrier = video.clone().append_group()?;
    let first = barrier
        .sequence
        .checked_add(1)
        .context("video sequence exhausted")?;
    barrier.finish()?;
    Ok(first)
}

fn parse_width(value: &str) -> std::result::Result<u32, String> {
    let width: u32 = value
        .parse()
        .map_err(|_| "width must be a number".to_owned())?;
    if width == 0 || (320..=7680).contains(&width) {
        Ok(width)
    } else {
        Err("width must be 0 (native), or 320–7680".into())
    }
}

fn parse_bitrate(value: &str) -> std::result::Result<u32, String> {
    let bitrate: u32 = value
        .parse()
        .map_err(|_| "bitrate must be a number".to_owned())?;
    if bitrate == 0 || (500..=100_000).contains(&bitrate) {
        Ok(bitrate)
    } else {
        Err("bitrate must be 0 (automatic), or 500–100000".into())
    }
}

/// The updates-track message for a virtual pad's feedback.
fn pad_feedback_update(feedback: crate::gamepad::PadFeedback) -> Update {
    use crate::gamepad::PadFeedback;
    match feedback {
        PadFeedback::Rumble(crate::gamepad::Rumble { pad, motors }) => Update::Rumble {
            pad,
            low: motors.low,
            high: motors.high,
            duration_ms: motors.duration_ms,
        },
        PadFeedback::Lights {
            pad,
            red,
            green,
            blue,
            player,
        } => Update::GamepadLights {
            pad,
            red,
            green,
            blue,
            player,
        },
        PadFeedback::Triggers { pad, right, left } => Update::GamepadTriggers { pad, right, left },
    }
}

fn publish_update(track: &mut moq_net::track::Producer, update: Update) -> Result<()> {
    let mut group = track.append_group()?;
    group.write_frame(moq_net::Timestamp::now(), serde_json::to_vec(&update)?)?;
    group.finish()?;
    Ok(())
}

/// Aborts a session-scoped helper task when the session ends.
struct AbortOnDrop(tokio::task::JoinHandle<Result<()>>);
impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// A client's microphone playing into this host's virtual microphone (G20).
/// Fields drop in order: playback stops before the virtual microphone goes.
struct SessionMicrophone {
    receive: AbortOnDrop,
    player: media::Pipeline,
    virtual_mic: crate::virtual_mic::VirtualMic,
}

impl SessionMicrophone {
    async fn start(controls: moq_net::origin::Producer) -> Result<Self> {
        let remote = tokio::time::timeout(
            Duration::from_secs(10),
            controls.consume().announced_broadcast("controls"),
        )
        .await
        .context("client controls missing")?
        .context("controls missing")?;
        let track = remote
            .track(protocol::MICROPHONE_TRACK)?
            .subscribe(media::audio_subscription())
            .await?;
        let virtual_mic = tokio::task::spawn_blocking(crate::virtual_mic::VirtualMic::create)
            .await
            .context("virtual microphone task failed")??;
        let (player, source) = crate::audio::microphone_player(virtual_mic.sink())?;
        tracing::info!(
            source = virtual_mic.source(),
            "playing the client's microphone into the virtual microphone"
        );
        Ok(Self {
            receive: AbortOnDrop(tokio::spawn(crate::audio::receive(
                track,
                source,
                protocol::AudioFormat::Opus,
            ))),
            player,
            virtual_mic,
        })
    }

    fn check(&mut self) -> Result<()> {
        ensure!(self.virtual_mic.alive(), "the virtual microphone went away");
        self.player.error()?;
        ensure!(
            !self.receive.0.is_finished(),
            "the client stopped sending its microphone"
        );
        Ok(())
    }
}

/// Apply a motion only if it is newer than any already applied, whichever
/// path (ordered track or datagram) delivered it.
fn newer_motion(last_motion: &AtomicU64, motion: u64) -> bool {
    last_motion.fetch_max(motion, Ordering::Relaxed) < motion
}

/// Pointer motion datagrams: unreliable and unordered, so stale or duplicate
/// positions are dropped by ID and anything but motion is ignored. A full
/// session queue drops the sample; the next one supersedes it.
async fn receive_pointer(
    controls: moq_net::origin::Producer,
    sender: tokio::sync::mpsc::Sender<Event>,
    last_motion: Arc<AtomicU64>,
) -> Result<()> {
    let remote = tokio::time::timeout(
        Duration::from_secs(10),
        controls.consume().announced_broadcast("controls"),
    )
    .await
    .context("pointer broadcast missing")?
    .context("controls missing")?;
    let mut track = remote
        .track(protocol::POINTER_TRACK)?
        .subscribe(moq_net::track::Subscription::default().with_priority(255))
        .await?;
    while let Some(datagram) = track.recv_datagram().await? {
        let Ok(pointer) = serde_json::from_slice::<protocol::PointerInput>(&datagram.payload)
        else {
            tracing::debug!("ignoring malformed pointer datagram");
            continue;
        };
        if !matches!(
            pointer.event,
            Event::Motion { .. } | Event::MonitorMotion { .. }
        ) || pointer.event.validate().is_err()
        {
            tracing::debug!("ignoring non-motion pointer datagram");
            continue;
        }
        if newer_motion(&last_motion, pointer.motion) {
            let _ = sender.try_send(pointer.event);
        }
    }
    Ok(())
}

async fn receive_input(
    mut track: moq_net::track::Subscriber,
    sender: tokio::sync::mpsc::Sender<Event>,
    last_motion: Arc<AtomicU64>,
) -> Result<()> {
    let mut sequence = 0;
    let mut group_sequence = 0;
    let mut pending = std::collections::BTreeMap::new();
    loop {
        let mut group = tokio::time::timeout(
            protocol::INPUT_TIMEOUT,
            ordered_group(&mut track, &mut pending, group_sequence),
        )
        .await
        .context("input group missing or heartbeat expired")??;
        group_sequence += 1;
        loop {
            let frame = tokio::time::timeout(
                protocol::INPUT_TIMEOUT,
                protocol::read_frame(&mut group, protocol::MAX_CONTROL),
            )
            .await
            .context("input heartbeat expired")??;
            let Some(frame) = frame else { break };
            let input: Input = serde_json::from_slice(&frame)?;
            ensure!(
                input.sequence == sequence,
                "input sequence gap; disconnecting to avoid stuck or reordered keys"
            );
            sequence += 1;
            input.event.validate()?;
            if let Some(motion) = input.motion
                && !newer_motion(&last_motion, motion)
            {
                continue; // a datagram already delivered this or a later position
            }
            sender
                .send(input.event)
                .await
                .context("input handler closed")?;
        }
    }
}

// MoQ priority is not a delivery-order guarantee. Reorder control groups locally;
// unlike video, keyboard/button history must never be skipped.
async fn ordered_group(
    track: &mut moq_net::track::Subscriber,
    pending: &mut std::collections::BTreeMap<u64, moq_net::group::Consumer>,
    expected: u64,
) -> Result<moq_net::group::Consumer> {
    protocol::ordered_group(track, pending, expected).await
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn pointer_motion_applies_only_newer_positions_from_either_path() {
        let last = AtomicU64::new(0);
        assert!(newer_motion(&last, 3)); // datagram 3
        assert!(!newer_motion(&last, 3)); // ordered repeat of 3 before a click
        assert!(!newer_motion(&last, 2)); // late, reordered datagram
        assert!(newer_motion(&last, 4));
        assert_eq!(last.load(Ordering::Relaxed), 4);
    }
    #[test]
    fn codec_request_is_explicit_and_bounded() {
        use protocol::VideoCodec::*;
        assert_eq!(requested_format(None, H264).unwrap().codec, H264);
        assert_eq!(
            requested_format(Some("codec=h265"), H264).unwrap().codec,
            H265
        );
        assert!(requested_format(Some("codec=h265&codec=h264"), H264).is_err());
        assert!(requested_format(Some("codec=unknown"), H264).is_err());
        assert!(requested_format(Some("codec=h264&range=hdr10"), H264).is_err());
        assert!(requested_format(Some("codec=h265&range=hdr10&range=sdr"), H264).is_err());
        assert!(requested_format(Some("codec=h265&range=hlg"), H264).is_err());
        assert!(requested_format(Some("unexpected=1"), H264).is_err());
        assert!(requested_format(Some("ssh-agent=1"), H264).is_ok());
        assert!(requested_format(Some("ssh-agent=0"), H264).is_err());
        assert!(requested_format(Some("ssh-agent=1&ssh-agent=1"), H264).is_err());
        assert!(requested_format(Some(&"x".repeat(129)), H264).is_err());
        assert_eq!(
            requested_format(None, H265).unwrap().dynamic_range,
            protocol::DynamicRange::Sdr
        );
        assert_eq!(
            requested_format(Some("codec=h265&range=hdr10"), H264)
                .unwrap()
                .dynamic_range,
            protocol::DynamicRange::Hdr10
        );
    }

    #[tokio::test]
    async fn video_restart_barrier_separates_equal_size_generations() -> Result<()> {
        let mut broadcast = moq_net::broadcast::Info::new().produce();
        let mut video = broadcast.create_track("video", None)?;
        let mut consumer = video
            .consume()
            .subscribe(moq_net::track::Subscription::default().with_group_start(0))
            .await?;
        consumer.start_at(0);
        let mut old = video.append_group()?;
        old.write_frame(moq_net::Timestamp::now(), b"old".as_slice())?;
        old.finish()?;
        let floor = video_barrier(&video)?;
        let mut fresh = video.append_group()?;
        fresh.write_frame(moq_net::Timestamp::now(), b"fresh".as_slice())?;
        fresh.finish()?;
        assert!(old.sequence < floor);
        assert_eq!(fresh.sequence, floor);
        let mut groups = std::collections::BTreeMap::new();
        for _ in 0..3 {
            let group = consumer
                .recv_group()
                .await?
                .context("missing video group")?;
            groups.insert(group.sequence, group);
        }
        let barrier = groups.get_mut(&(floor - 1)).context("missing barrier")?;
        assert!(protocol::read_frame(barrier, 100).await?.is_none());
        let next = groups.get_mut(&floor).context("missing fresh group")?;
        assert_eq!(
            protocol::read_frame(next, 100).await?.unwrap(),
            &b"fresh"[..]
        );
        Ok(())
    }

    #[tokio::test]
    async fn control_groups_are_reordered_without_dropping_events() -> Result<()> {
        let mut broadcast = moq_net::broadcast::Info::new().produce();
        let mut producer = broadcast.create_track("input", None)?;
        let mut consumer = producer
            .consume()
            .subscribe(moq_net::track::Subscription::default().with_group_start(0))
            .await?;
        consumer.start_at(0);
        let mut second = producer.create_group(1u64.into())?;
        second.write_frame(moq_net::Timestamp::now(), b"release".as_slice())?;
        second.finish()?;
        let mut first = producer.create_group(0u64.into())?;
        first.write_frame(moq_net::Timestamp::now(), b"press".as_slice())?;
        first.finish()?;
        let mut pending = std::collections::BTreeMap::new();
        let mut first = ordered_group(&mut consumer, &mut pending, 0).await?;
        let mut second = ordered_group(&mut consumer, &mut pending, 1).await?;
        assert_eq!(
            protocol::read_frame(&mut first, 1024).await?.unwrap(),
            &b"press"[..]
        );
        assert_eq!(
            protocol::read_frame(&mut second, 1024).await?.unwrap(),
            &b"release"[..]
        );
        producer.finish()?;
        broadcast.finish();
        Ok(())
    }
}
