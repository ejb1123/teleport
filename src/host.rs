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
    /// What to capture: `portal` (Wayland), `x11`, `kms` (the framebuffer a
    /// display shows, for kiosk compositors; needs CAP_SYS_ADMIN and is
    /// view-only, docs/kms-capture.md), `test`, or `auto` (portal or X11).
    #[arg(long, default_value = "auto", value_parser = ["auto", "portal", "x11", "kms", "test"])]
    pub source: String,
    /// With `--source kms`: the DRM device (default: the first /dev/dri/card*
    /// with an active display).
    #[arg(long)]
    pub kms_device: Option<PathBuf>,
    /// With `--source test`: stream a black screen that turns white, and
    /// back, on every key or mouse-button press, to time input→photon with a
    /// photodiode or camera (performance.md).
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
    /// Enable existing Linux-account login with an installed, host-native PAM
    /// helper: /usr/lib/teleport/teleport-pam from the .deb, .rpm and Arch
    /// packages (system-login.md).
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
    /// frame delay (ffmpeg.md).
    #[arg(long, value_enum, default_value_t = protocol::EncoderPreset::Latency)]
    pub encoder_preset: protocol::EncoderPreset,
    /// Disable receiver-feedback bitrate adjustment.
    #[arg(long)]
    pub fixed_bitrate: bool,
    /// Always draw the pointer into the video. By default a desktop portal
    /// session sends it as cursor metadata (when the compositor offers it)
    /// so clients can draw it locally (cursor.md).
    #[arg(long)]
    pub embedded_cursor: bool,
    /// Virtual displays through the desktop portal (KDE Plasma 6; GNOME
    /// untested): `fallback` streams one when no monitor is connected
    /// (headless) and lets clients add one as an extra monitor; `always`
    /// streams one sized to the client instead of the monitors; `never`
    /// turns both off (virtual-display.md).
    #[arg(long, value_enum, default_value_t = VirtualDisplay::Fallback)]
    pub virtual_display: VirtualDisplay,
    /// Privacy mode (KDE): while a session lasts, turn the physical monitors
    /// off and stream a virtual display instead; `request` when the client
    /// asks (`teleport client --privacy`), `always` for every session. They
    /// come back on disconnect, on any failure, and through a guard process
    /// if the host dies (virtual-display.md).
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
    /// from the "Teleport Microphone" input (PipeWire; audio.md).
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
    /// Codecs `encoder` encodes in hardware here (`Desktop::hardware_codecs`
    /// and `hardware_av1`).
    #[arg(skip)]
    pub hardware_codecs: Vec<protocol::VideoCodec>,
    /// Codecs `encoder` encodes 4:4:4 and Main 10 in hardware.
    #[arg(skip)]
    pub hardware_formats: Box<HardwareFormats>,
    /// The session streams 4:4:4 (the client asked in `SelectCodec`).
    #[arg(skip)]
    pub chroma_444: bool,
    /// The session streams 10-bit SDR (the client asked in `SelectCodec`).
    #[arg(skip)]
    pub ten_bit: bool,
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
    /// (wake-and-unattended.md).
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
    /// log in automatically and lock at once (wake-and-unattended.md).
    #[arg(long, requires = "system_auth_helper")]
    pub unlock_on_system_login: bool,
    /// What clients are told about waking this host (`Desktop::wake`).
    #[arg(skip)]
    pub wake: Option<crate::wake::WakeInfo>,
    /// The session's lock state can be read and changed (logind).
    #[arg(skip)]
    pub secure_attention: bool,
    /// Most sessions at once, owners and guests together (1-8). Sessions
    /// share one encoder and the host's input (guests.md).
    #[arg(long, default_value_t = crate::guests::MAX_SESSIONS, value_parser = parse_sessions)]
    pub max_sessions: usize,
    /// Keep guests connected when the last owner session leaves (by
    /// default they are disconnected with it).
    #[arg(long)]
    pub keep_guests_without_owner: bool,
    /// Lock the host's desktop (`loginctl lock-session`) when the last
    /// session disconnects.
    #[arg(long)]
    pub lock_on_last_disconnect: bool,
    /// Disconnect a guest that sent no keyboard, mouse or gamepad input for
    /// this many minutes (0: never).
    #[arg(long, default_value_t = 0, value_parser = clap::value_parser!(u64).range(0..=1440))]
    pub guest_idle_minutes: u64,
    /// Total video bitrate for all sessions in kbit/s (0: no cap). The
    /// shared encoder runs at most at this divided by the number of
    /// sessions, so each gets an equal share.
    #[arg(long, default_value_t = 0, value_parser = parse_bitrate)]
    pub bandwidth_cap: u32,
    /// Seconds a guest waits for the host's approval before it is denied.
    #[arg(long, default_value_t = 30, value_parser = clap::value_parser!(u64).range(5..=600))]
    pub guest_approval_timeout: u64,
}

impl Options {
    /// Session policy for several sessions (docs/guests.md).
    fn policy(&self) -> crate::guests::Policy {
        crate::guests::Policy {
            max_sessions: self.max_sessions,
            kick_guests_with_owner: !self.keep_guests_without_owner,
            lock_on_last_disconnect: self.lock_on_last_disconnect,
            guest_idle_kick: (self.guest_idle_minutes > 0)
                .then(|| Duration::from_secs(self.guest_idle_minutes * 60)),
            bandwidth_cap: (self.bandwidth_cap > 0).then_some(self.bandwidth_cap),
            approval_timeout: Duration::from_secs(self.guest_approval_timeout),
        }
    }
}

fn parse_sessions(value: &str) -> std::result::Result<usize, String> {
    match value.parse::<usize>() {
        Ok(count) if (1..=crate::guests::MAX_SESSIONS).contains(&count) => Ok(count),
        _ => Err(format!(
            "sessions must be 1-{}",
            crate::guests::MAX_SESSIONS
        )),
    }
}

/// The host can serve AV1: in hardware, or with SVT-AV1 when `--encoder`
/// allows software (explicit hardware encoders never fall back).
fn offers_av1(options: &Options) -> bool {
    options.hardware_codecs.contains(&protocol::VideoCodec::Av1)
        || (matches!(
            options.encoder,
            media::EncoderKind::Auto | media::EncoderKind::Software
        ) && media::av1_available())
}

/// The host can serve a format (4:4:4 or 10-bit SDR) its `hardware` probe
/// found, or any of them when `--encoder` allows software.
fn offers_format(options: &Options, hardware: &[protocol::VideoCodec]) -> bool {
    crate::ffmpeg::enabled().unwrap_or(false)
        && (!hardware.is_empty()
            || matches!(
                options.encoder,
                media::EncoderKind::Auto | media::EncoderKind::Software
            ))
}

/// Codecs the host encodes in hardware as 4:4:4 (`Desktop::hardware_444`)
/// and as Main 10 SDR (`Desktop::hardware_ten_bit`).
#[derive(Clone, Debug, Default)]
pub struct HardwareFormats {
    pub chroma_444: Vec<protocol::VideoCodec>,
    pub ten_bit: Vec<protocol::VideoCodec>,
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
    media::check_host_media()?;
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
    // Before the portal, which on a locked screen would wait for a consent
    // dialog nobody can answer.
    // The portal consent question does not arise for KMS or test capture.
    if !matches!(options.source.as_str(), "test" | "kms") {
        crate::session_lock::warn_unless_preauthorized().await;
    }
    crate::kms::set_device(options.kms_device.clone());
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
            "Wake-on-LAN is off on this network card; clients cannot wake this host ({})",
            crate::docs::page("wake-and-unattended.md")
        );
    }
    options.wake = Some(wake);
    options.secure_attention = secure_attention;
    // The session engine owns the capture while sessions run; it is back
    // here whenever serve() returns.
    let mut idle = Some(capture);
    let result = serve(&options, &mut idle).await;
    if let Some(mut capture) = idle {
        capture.close().await;
    }
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

async fn serve(options: &Options, idle: &mut Option<Capture>) -> Result<()> {
    // Warm the hardware encoder probe so the first client does not wait for it.
    let kind = options.encoder;
    if options.smoke_hardware_codecs.is_none() {
        std::thread::spawn(move || media::log_hardware_encoders(kind));
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
    // Sessions, approvals and policy, shared with the admin socket.
    let hub = crate::guests::Hub::new(options.policy());
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
                hub.clone(),
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
    // Authenticated connections (approved, if they needed it) wait here for
    // the session engine, which holds the receiver while sessions run.
    let (join_sender, join_receiver) =
        tokio::sync::mpsc::channel::<Member>(crate::guests::MAX_SESSIONS * 2);
    let mut joins = Some(join_receiver);
    // The session engine: owns the capture while at least one session runs.
    let mut engine: Option<std::pin::Pin<Box<dyn std::future::Future<Output = EngineExit> + '_>>> =
        None;
    // Connections finishing their handshake or waiting for approval.
    let mut admissions = tokio::task::JoinSet::new();
    loop {
        while admissions.try_join_next().is_some() {}
        // A provider relay grants each connection a fixed number of requests:
        // replace a worn connection once nothing uses it.
        if relay.as_ref().is_some_and(|(link, _, _)| link.renewable()) {
            tracing::info!("renewing the relay connection (provider request limit)");
            relay = None;
            relay_retry = Instant::now();
        }
        if let (Some(endpoint), Some(e2e), Some(access)) =
            (&relay_endpoint, &relay_identity, &access)
            && relay.is_none()
            && Instant::now() >= relay_retry
        {
            match crate::relay::connect_host(endpoint, &relay_namespace).await {
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
                        match relayed_member(link, accepted, access, &hub) {
                            Ok(member) => {
                                admissions.spawn(admit(member, hub.clone(), join_sender.clone()));
                            }
                            Err(error) => tracing::warn!(error = format!("{error:#}"), "relayed client refused"),
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
            _ = idle_check.tick(), if engine.is_none() => {
                if let Some(capture) = idle.as_mut()
                    && capture.portal_closed()
                {
                    restore_capture(capture).await;
                }
                continue;
            }
            // The last session left (or the engine failed): the capture is
            // idle again.
            exit = async {
                match engine.as_mut() {
                    Some(running) => running.await,
                    None => std::future::pending().await,
                }
            }, if engine.is_some() => {
                engine = None;
                let EngineExit { mut capture, joins: receiver, result, carry } = exit;
                capture.end_session().await;
                leave_privacy(&mut capture).await;
                if let Err(error) = result {
                    tracing::warn!(error = format!("{error:#}"), "client disconnected");
                }
                // A reconnect replaced the last session: start over with it.
                if let Some(member) = carry {
                    restore_capture(&mut capture).await;
                    engine = Some(Box::pin(engine_run(options, &hub, capture, member, receiver)));
                    continue;
                }
                if hub.policy.lock_on_last_disconnect {
                    match crate::guests::lock_session().await {
                        Ok(()) => tracing::info!("desktop locked after the last session left"),
                        Err(error) => tracing::warn!(error = format!("{error:#}"), "could not lock the desktop"),
                    }
                }
                *idle = Some(capture);
                joins = Some(receiver);
                continue;
            }
            // The first session: start the engine.
            member = async {
                match joins.as_mut() {
                    Some(receiver) => receiver.recv().await,
                    None => std::future::pending().await,
                }
            }, if engine.is_none() => {
                match (member, joins.take(), idle.take()) {
                    (Some(member), Some(receiver), Some(mut capture)) => {
                        restore_capture(&mut capture).await;
                        engine = Some(Box::pin(engine_run(options, &hub, capture, member, receiver)));
                    }
                    (_, receiver, capture) => (joins, *idle) = (receiver, capture),
                }
                continue;
            }
            _ = &mut shutdown => {
                if let Some(running) = engine.take() {
                    hub.poke(crate::guests::Command::Shutdown);
                    let exit = running.await;
                    let mut capture = exit.capture;
                    capture.end_session().await;
                    leave_privacy(&mut capture).await;
                    *idle = Some(capture);
                }
                break;
            }
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
        let lease = if let Some(access) = &access {
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
        // Only the owner role may ask for wake or suspend.
        match crate::host_command::Command::parse(request.query()) {
            Ok(Some(command)) => {
                let owner = lease.as_ref().is_none_or(|lease| lease.owner());
                tokio::spawn(crate::host_command::host::direct(
                    request,
                    command,
                    policy.clone(),
                    owner,
                    lease,
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
        // Who it is: an owner, or a guest with what it was granted.
        let role = match &lease {
            Some(lease) => match lease.access() {
                Some(role) => role,
                None => {
                    request.close().await;
                    continue;
                }
            },
            None => crate::access::SessionRole::owner("Host pairing file", None),
        };
        let kind = if grant.is_some() { "p2p" } else { "direct" };
        let credential = crate::e2e::credential_hash(&supplied);
        let member = Member::direct(hub.next_id(), request, lease, role, kind, credential);
        admissions.spawn(admit(member, hub.clone(), join_sender.clone()));
    }
    server.close().await;
    Ok(())
}

/// How a session's connection is closed.
enum Transport {
    /// Direct QUIC or peer-to-peer: `connecting` until the handshake is done.
    Direct {
        session: Option<moq_net::Session>,
        connecting: Option<
            std::pin::Pin<Box<dyn std::future::Future<Output = Result<moq_net::Session>> + Send>>,
        >,
    },
    /// Through the relay: `session` is the relay connection other relayed
    /// sessions share, so only dropping the bridges ends this one.
    Relayed {
        session: moq_net::Session,
        bridge: Box<(crate::relay::HostSession, crate::relay::Accepted)>,
    },
}

/// An authenticated connection on its way to (or in) the session engine.
pub struct Member {
    id: crate::guests::SessionId,
    /// As leased; the engine re-reads it (permission changes, revocation).
    role: crate::access::SessionRole,
    /// `None` without an access database (only the host pairing file).
    lease: Option<crate::access::SessionLease>,
    query: Option<String>,
    /// The session publishes `desktop` here and receives `controls` from
    /// `incoming`: the QUIC session's own origins, or plaintext origins
    /// bridged to a relay (see `crate::relay`).
    outgoing: moq_net::origin::Producer,
    incoming: moq_net::origin::Producer,
    transport: Transport,
    /// `direct`, `relay` or `p2p`.
    kind: &'static str,
    /// `SHA-256(token)` of the credential it presented: a new connection
    /// with the same credential replaces this one (a reconnect).
    credential: [u8; 32],
    /// A trusted guest admitted without approval because an owner was
    /// connected: refused if none is by the time it joins.
    needs_owner: bool,
}

impl Member {
    fn direct(
        id: crate::guests::SessionId,
        request: crate::p2p::Incoming,
        lease: Option<crate::access::SessionLease>,
        role: crate::access::SessionRole,
        kind: &'static str,
        credential: [u8; 32],
    ) -> Self {
        let query = request.query().map(str::to_owned);
        let outgoing = moq_net::Origin::random().produce();
        let incoming = moq_net::Origin::random().produce();
        let connecting = Box::pin(request.ok(&outgoing, incoming.clone()));
        Self {
            id,
            role,
            lease,
            query,
            outgoing,
            incoming,
            transport: Transport::Direct {
                session: None,
                connecting: Some(connecting),
            },
            kind,
            credential,
            needs_owner: false,
        }
    }

    /// Finish a direct session's handshake.
    async fn connect(&mut self) -> Result<()> {
        if let Transport::Direct {
            session,
            connecting,
        } = &mut self.transport
            && let Some(connecting) = connecting.take()
        {
            *session = Some(connecting.await?);
        }
        Ok(())
    }

    /// The transport whose closure and statistics apply.
    fn session(&self) -> Option<&moq_net::Session> {
        match &self.transport {
            Transport::Direct { session, .. } => session.as_ref(),
            Transport::Relayed { session, .. } => Some(session),
        }
    }

    /// Resolves (with the reason) when the client has left: its direct
    /// connection closed, or its relayed controls ended (the relay
    /// connection is shared with other sessions).
    fn departure(&self) -> impl std::future::Future<Output = String> + Send + 'static {
        let (direct, relayed) = match &self.transport {
            Transport::Direct { session, .. } => (session.clone(), None),
            Transport::Relayed { session, bridge } => {
                (None, Some((session.clone(), bridge.0.gone.clone())))
            }
        };
        async move {
            if let Some(session) = direct {
                return format!("connection closed: {}", session.closed().await);
            }
            if let Some((link, mut gone)) = relayed {
                return tokio::select! {
                    error = link.closed() => format!("relay connection closed: {error}"),
                    _ = gone.wait_for(|left| *left) => "left the relay".to_owned(),
                };
            }
            std::future::pending().await
        }
    }

    /// The current role, or `None` once the device is revoked.
    /// Leased from a system login (PAM just verified the account password).
    fn system_login(&self) -> bool {
        self.lease
            .as_ref()
            .is_some_and(crate::access::SessionLease::system_login)
    }

    fn current_role(&self) -> Option<crate::access::SessionRole> {
        match &self.lease {
            Some(lease) => lease.access(),
            None => Some(self.role.clone()),
        }
    }

    /// Disconnect (denied, kicked, revoked, refused).
    fn close(self) {
        if let Transport::Direct {
            session: Some(session),
            ..
        } = &self.transport
        {
            session.abort(moq_net::Error::Unauthorized);
        }
        // A relayed session ends when its bridges drop, here.
    }
}

/// A relayed client that has already proven the session keys (see
/// `relay::intake`). Only now is the credential leased (claiming a
/// single-use ticket); the desktop is captured once it joins.
fn relayed_member(
    link: &crate::relay::Link,
    accepted: crate::relay::Accepted,
    access: &Arc<crate::access::ServerState>,
    hub: &crate::guests::Hub,
) -> Result<Member> {
    let lease = access
        .begin_session_hash(accepted.credential.to_vec(), accepted.device_key.as_deref())
        .context("relayed credential not usable")?;
    let role = lease.access().context("relayed credential not usable")?;
    let bridged = crate::relay::host(link, &accepted)?;
    let credential = accepted.credential;
    Ok(Member {
        id: hub.next_id(),
        role,
        lease: Some(lease),
        query: Some(accepted.request.clone()),
        outgoing: bridged.outgoing.clone(),
        incoming: bridged.incoming.clone(),
        transport: Transport::Relayed {
            session: link.session.clone(),
            bridge: Box::new((bridged, accepted)),
        },
        kind: "relay",
        credential,
        needs_owner: false,
    })
}

/// Finish the handshake and, for a guest that is not trusted, wait for the
/// host's approval (denied on timeout) before the session may join.
async fn admit(
    mut member: Member,
    hub: Arc<crate::guests::Hub>,
    joins: tokio::sync::mpsc::Sender<Member>,
) {
    if let Err(error) = member.connect().await {
        tracing::warn!(error = format!("{error:#}"), "client connection failed");
        return;
    }
    // The role now, not when the connection was leased.
    let Some(role) = member.current_role() else {
        member.close();
        return;
    };
    member.role = role;
    let guest = !member.role.is_owner();
    if guest
        && let Some(device) = &member.role.device
        && hub.blocked(device)
    {
        tracing::info!(session = member.id, guest = %member.role.name, "guest refused for now (recently denied or kicked)");
        member.close();
        return;
    }
    // Trusted guests skip approval only while an owner is there, unless the
    // host keeps guests without an owner (`--keep-guests-without-owner`).
    let approval = guest
        && (!member.role.trusted || (hub.policy.kick_guests_with_owner && !hub.owner_connected()));
    member.needs_owner = guest && !approval && hub.policy.kick_guests_with_owner;
    if approval {
        tracing::info!(session = member.id, guest = %member.role.name, "guest asks to join; waiting for approval");
        // The guest leaving withdraws its request.
        let departure = member.departure();
        let gone = async move {
            departure.await;
        };
        let (name, device) = (member.role.name.clone(), member.role.device.clone());
        let outcome =
            crate::guests::await_approval(&hub, member.id, &name, device.clone(), gone).await;
        use crate::guests::Approval;
        if matches!(outcome, Approval::Denied | Approval::TimedOut)
            && let Some(device) = &device
        {
            hub.block(device, crate::guests::DENIED_FOR);
        }
        // Revoked while it waited: never admitted.
        if outcome != Approval::Allowed || member.current_role().is_none() {
            tracing::info!(session = member.id, guest = %member.role.name, ?outcome, "guest not admitted");
            member.close();
            return;
        }
        tracing::info!(session = member.id, guest = %member.role.name, "guest approved");
    }
    if let Err(error) = joins.send(member).await {
        error.0.close();
    }
}

/// The session engine finished: the capture and the joins receiver go back
/// to the accept loop.
struct EngineExit {
    capture: Capture,
    joins: tokio::sync::mpsc::Receiver<Member>,
    result: Result<()>,
    /// A reconnect that replaced the last session: the next engine's first.
    carry: Option<Member>,
}

async fn engine_run(
    options: &Options,
    hub: &crate::guests::Hub,
    mut capture: Capture,
    first: Member,
    mut joins: tokio::sync::mpsc::Receiver<Member>,
) -> EngineExit {
    let (sender, mut commands) = tokio::sync::mpsc::unbounded_channel();
    hub.attach(Some(sender));
    let (result, carry) =
        match run_session(options, hub, &mut capture, first, &mut joins, &mut commands).await {
            Ok(carry) => (Ok(()), carry),
            Err(error) => (Err(error), None),
        };
    hub.attach(None);
    EngineExit {
        capture,
        joins,
        result,
        carry,
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

/// How often a session may be told that something it sent was refused.
const REFUSAL_NOTICE_INTERVAL: Duration = Duration::from_secs(5);
/// A removed session's connection closes this long after its last notice,
/// so the notice can still reach the client.
const CLOSE_GRACE: Duration = Duration::from_millis(300);
/// How often running sessions' roles and permissions are re-read (also at
/// once on revocation or a permission change).
const ACCESS_CHECK_INTERVAL: Duration = Duration::from_secs(1);
/// Guests' keyframe requests at most this often (each costs every session).
const GUEST_KEYFRAME_INTERVAL: Duration = Duration::from_secs(1);
/// How long a client's microphone track may take to arrive.
const MICROPHONE_START_TIMEOUT: Duration = Duration::from_secs(10);

/// The stream every session shares: tracks the encoders write once. The
/// first session's broadcast holds them; later sessions' broadcasts forward
/// them (`forward_track`).
struct Shared {
    codec: protocol::VideoCodec,
    video: moq_net::track::Producer,
    monitors: Vec<moq_net::track::Producer>,
    /// Desktop audio in the first session's format and bitrate.
    audio: Option<(protocol::AudioSettings, moq_net::track::Producer)>,
}

/// What a session's client declared (`Event::ClientFeatures`).
#[derive(Default, Clone, Copy)]
struct Features {
    declared: bool,
    guests: bool,
    rumble: bool,
    lights: bool,
    frame_timing: bool,
}

/// One session inside the engine.
struct Session {
    member: Member,
    role: crate::access::SessionRole,
    updates: moq_net::track::Producer,
    metadata: moq_net::track::Producer,
    /// The session's `desktop` broadcast (for the first session it also
    /// holds the shared tracks, see [`Members::primary`]).
    broadcast: Option<moq_net::broadcast::Producer>,
    /// Tracks it announces but never gets data on (the other codec).
    _idle: Vec<moq_net::track::Producer>,
    /// Input, connection watcher, pointer datagrams, forwarders.
    tasks: Vec<AbortOnDrop>,
    pointer: bool,
    last_motion: Arc<AtomicU64>,
    /// Its host pad slots: `pad_block * MAX_GAMEPADS ..` (`crate::gamepad`).
    pad_block: u8,
    features: Features,
    /// Rate control from this session's own feedback; the leader's feedback
    /// drives the engine's controllers and step-down ladder instead.
    rates: Option<Rates>,
    /// Its latest display (applied when it leads the stream).
    display: Option<protocol::ClientDisplay>,
    joined: Instant,
    last_input: Instant,
    last_feedback: Instant,
    /// Its last keyframe request (guests are rate limited).
    last_keyframe: Option<Instant>,
    last_refusal: Option<Instant>,
    sent_access: Option<protocol::SessionAccess>,
    /// Joined after the stream started: it takes the session's codec.
    shared: bool,
    /// Forwards its SSH agent (the first session only).
    ssh_agent: bool,
}

/// Whether an event may go on, and what to release first.
enum Screen {
    Pass { release: bool },
    Drop,
}

/// Every session on the desktop, keyboard/mouse arbitration and the
/// session policy (docs/guests.md).
struct Members {
    sessions: BTreeMap<crate::guests::SessionId, Session>,
    /// Join order: the stream leader is the first owner, else the first
    /// session.
    order: Vec<crate::guests::SessionId>,
    arbiter: crate::guests::Arbiter,
    policy: crate::guests::Policy,
    /// Approval requests shown to owners' clients.
    announced: std::collections::BTreeSet<crate::guests::SessionId>,
    /// Sessions whose updates track failed: removed by the loop.
    failed: Vec<crate::guests::SessionId>,
    /// The first session's broadcast, kept (with the shared tracks) after it
    /// leaves.
    primary: Option<moq_net::broadcast::Producer>,
    /// The shared desktop audio's format and bitrate (the first session's
    /// choice), told to joining sessions.
    stream_audio: Option<protocol::AudioSettings>,
}

impl Members {
    fn new(policy: crate::guests::Policy) -> Self {
        Self {
            sessions: BTreeMap::new(),
            order: Vec::new(),
            arbiter: crate::guests::Arbiter::default(),
            policy,
            announced: Default::default(),
            failed: Vec::new(),
            primary: None,
            stream_audio: None,
        }
    }

    fn leader(&self) -> Option<crate::guests::SessionId> {
        self.order
            .iter()
            .copied()
            .find(|id| self.sessions.get(id).is_some_and(|s| s.role.is_owner()))
            .or_else(|| self.order.first().copied())
    }

    fn is_leader(&self, id: crate::guests::SessionId) -> bool {
        self.leader() == Some(id)
    }

    /// The transport whose statistics drive rate control without client
    /// feedback.
    fn leader_session(&self) -> Option<moq_net::Session> {
        self.leader()
            .and_then(|id| self.sessions.get(&id))
            .and_then(|session| session.member.session().cloned())
    }

    /// Whether session `id` may play its microphone into the host's virtual
    /// microphone: owners, and guests granted it (docs/guests.md).
    fn may_send_microphone(&self, id: crate::guests::SessionId) -> bool {
        self.sessions
            .get(&id)
            .is_some_and(|s| crate::guests::may_send_microphone(s.role.role, s.role.permissions))
    }

    fn role_of(&self, id: crate::guests::SessionId) -> Option<protocol::Role> {
        self.sessions.get(&id).map(|session| session.role.role)
    }

    fn send(session: &mut Session, update: Update, failed: &mut Vec<crate::guests::SessionId>) {
        if let Err(error) = publish_update(&mut session.updates, update) {
            tracing::debug!(%error, session = session.member.id, "session updates closed");
            failed.push(session.member.id);
        }
    }

    /// To every session. A session whose track failed leaves; the others
    /// go on, so this never fails the engine.
    fn all(&mut self, update: Update) -> Result<()> {
        for session in self.sessions.values_mut() {
            Self::send(session, update.clone(), &mut self.failed);
        }
        Ok(())
    }

    fn to(&mut self, id: crate::guests::SessionId, update: Update) -> Result<()> {
        if let Some(session) = self.sessions.get_mut(&id) {
            Self::send(session, update, &mut self.failed);
        }
        Ok(())
    }

    /// Answers to the stream leader's own requests (monitor streams,
    /// privacy, video settings).
    fn leader_update(&mut self, update: Update) -> Result<()> {
        match self.leader() {
            Some(id) => self.to(id, update),
            None => Ok(()),
        }
    }

    fn notice(&mut self, id: crate::guests::SessionId, text: impl Into<String>) {
        let _ = self.to(id, Update::Notice { text: text.into() });
    }

    fn notice_owners(&mut self, text: &str) {
        for id in self.order.clone() {
            if self.sessions.get(&id).is_some_and(|s| s.role.is_owner()) {
                self.notice(id, text);
            }
        }
    }

    /// `Update::Desktop` to every session, as each may see it.
    fn desktop(&mut self, info: &Desktop) -> Result<()> {
        for id in self.order.clone() {
            let desktop = self.customize(id, info);
            self.to(id, Update::Desktop { desktop })?;
        }
        Ok(())
    }

    /// `info` as session `id` sees it: its role and permissions, and only
    /// the stream leader is offered privacy mode and a virtual monitor.
    fn customize(&self, id: crate::guests::SessionId, info: &Desktop) -> Desktop {
        let mut desktop = info.clone();
        let Some(session) = self.sessions.get(&id) else {
            return desktop;
        };
        desktop.ssh_agent = info.ssh_agent && session.ssh_agent;
        desktop.clipboard = info.clipboard && session.role.permissions.clipboard;
        if !session.role.is_owner() {
            // Xbox pads only (`crate::guests::guest_pad`).
            desktop.gamepad_types.clear();
        }
        desktop.guests = true;
        desktop.access = Some(self.access(id));
        desktop.shared_stream = session.shared;
        desktop.codec_selection = info.codec_selection && !session.shared;
        if !self.is_leader(id) || !session.role.is_owner() {
            desktop.privacy_mode = false;
            desktop.virtual_monitor = false;
        }
        // Offered only to sessions that may use it (screened again on use).
        desktop.microphone = info.microphone
            && crate::guests::may_send_microphone(session.role.role, session.role.permissions);
        if !session.role.is_owner() {
            // Locking, unlocking and power are the owner's; the host's
            // hardware address and network are not a guest's business.
            desktop.secure_attention = false;
            desktop.wake = None;
        }
        // A joined session plays the shared desktop audio as it is.
        if session.shared
            && let Some(audio) = self.stream_audio
        {
            desktop.audio_settings = audio;
        }
        desktop
    }

    fn access(&self, id: crate::guests::SessionId) -> protocol::SessionAccess {
        let (role, permissions) = self.sessions.get(&id).map_or(
            (protocol::Role::Guest, protocol::Permissions::default()),
            |s| (s.role.role, s.role.permissions),
        );
        protocol::SessionAccess {
            role,
            permissions,
            control: self.arbiter.view(id, |other| self.role_of(other)),
            others: self.sessions.len().saturating_sub(1) as u32,
        }
    }

    /// `Update::Access` to sessions whose view changed (clients that read it).
    fn send_access(&mut self) {
        for id in self.order.clone() {
            let access = self.access(id);
            if let Some(session) = self.sessions.get_mut(&id)
                && session.features.guests
                && session.sent_access != Some(access)
            {
                session.sent_access = Some(access);
                Self::send(session, Update::Access { access }, &mut self.failed);
            }
        }
    }

    /// A refusal notice, at most one per session every few seconds (also
    /// logged then).
    fn refuse(&mut self, id: crate::guests::SessionId, text: &str) {
        let Some(session) = self.sessions.get_mut(&id) else {
            return;
        };
        if session
            .last_refusal
            .is_some_and(|at| at.elapsed() < REFUSAL_NOTICE_INTERVAL)
        {
            return;
        }
        session.last_refusal = Some(Instant::now());
        tracing::info!(session = id, name = %session.role.name, role = session.role.role.label(), reason = text, "refused input from session");
        Self::send(
            session,
            Update::Notice { text: text.into() },
            &mut self.failed,
        );
    }

    /// Whether `from` may send `event` now: permissions, keyboard/mouse
    /// control and stream leadership, decided here before anything is
    /// injected or changed.
    fn screen(&mut self, from: crate::guests::SessionId, event: &Event, now: Instant) -> Screen {
        use crate::guests::{Admission, Need, need};
        let Some(session) = self.sessions.get(&from) else {
            // Left (or removed) while this was queued.
            return Screen::Drop;
        };
        let (role, permissions) = (session.role.role, session.role.permissions);
        let holder = self.arbiter.holder_session();
        if !crate::guests::permitted(role, permissions, event) {
            let text = match need(event) {
                Need::KeyboardMouse => "The host has not allowed you keyboard and mouse",
                Need::Gamepad => "The host has not allowed you gamepads",
                Need::Clipboard => "The host has not allowed you the clipboard",
                _ => "Only the host's owner can do that",
            };
            self.refuse(from, text);
            return Screen::Drop;
        }
        match need(event) {
            Need::KeyboardMouse => match self.arbiter.admit(from, role, now) {
                Admission::Allowed { taken_from } => {
                    let changed = holder != Some(from);
                    if let Some(session) = self.sessions.get_mut(&from) {
                        session.last_input = now;
                    }
                    if changed {
                        let name = self.sessions[&from].role.name.clone();
                        tracing::info!(session = from, name = %name, role = role.label(), previous = ?taken_from, "session took keyboard and mouse control");
                        if let Some(previous) = taken_from {
                            self.notice(
                                previous,
                                format!("{name} took keyboard and mouse control"),
                            );
                        }
                        self.send_access();
                    }
                    Screen::Pass {
                        release: taken_from.is_some(),
                    }
                }
                Admission::Busy => {
                    self.refuse(from, "Another session has keyboard and mouse control");
                    Screen::Drop
                }
            },
            Need::Gamepad => {
                if let Some(session) = self.sessions.get_mut(&from) {
                    session.last_input = now;
                }
                Screen::Pass { release: false }
            }
            Need::Stream if !self.is_leader(from) => {
                match event {
                    // Kept for when it leads the stream.
                    Event::ClientDisplay(display) => {
                        if let Some(session) = self.sessions.get_mut(&from) {
                            session.display = Some(*display);
                        }
                    }
                    // Joined sessions take the stream's codec.
                    Event::SelectCodec { .. } => (),
                    _ => self.refuse(from, "Only the session that leads the stream can change it"),
                }
                Screen::Drop
            }
            _ => Screen::Pass { release: false },
        }
    }

    /// The host's session list (settings window, `host-admin sessions`).
    fn publish(&self, hub: &crate::guests::Hub) {
        let now = Instant::now();
        hub.set_sessions(
            self.order
                .iter()
                .filter_map(|id| self.sessions.get(id))
                .map(|session| crate::guests::SessionSummary {
                    id: session.member.id,
                    name: session.role.name.clone(),
                    device: session.role.device.clone(),
                    role: session.role.role,
                    permissions: session.role.permissions,
                    control: self.arbiter.holder_session() == Some(session.member.id),
                    transport: session.member.kind.into(),
                    connected_s: now.duration_since(session.joined).as_secs(),
                    idle_s: now.duration_since(session.last_input).as_secs(),
                })
                .collect(),
        );
    }

    /// Tell owners' clients about guests waiting for approval.
    fn sync_approvals(&mut self, hub: &crate::guests::Hub) {
        let pending = hub.approvals();
        let current: std::collections::BTreeSet<_> = pending.iter().map(|p| p.id).collect();
        let fresh: Vec<_> = pending
            .into_iter()
            .filter(|p| !self.announced.contains(&p.id))
            .collect();
        for request in &fresh {
            for id in self.order.clone() {
                let Some(session) = self.sessions.get(&id) else {
                    continue;
                };
                if !session.role.is_owner() {
                    continue;
                }
                let update = if session.features.guests {
                    Update::GuestRequest {
                        id: request.id,
                        name: request.name.clone(),
                        timeout_s: request.remaining_s.min(3600) as u32,
                    }
                } else {
                    Update::Notice {
                        text: format!(
                            "Guest \u{201c}{}\u{201d} asks to join: approve it in the host settings",
                            request.name
                        ),
                    }
                };
                let _ = self.to(id, update);
            }
        }
        for closed in self
            .announced
            .difference(&current)
            .copied()
            .collect::<Vec<_>>()
        {
            for id in self.order.clone() {
                if self
                    .sessions
                    .get(&id)
                    .is_some_and(|s| s.role.is_owner() && s.features.guests)
                {
                    let _ = self.to(id, Update::GuestRequestClosed { id: closed });
                }
            }
        }
        self.announced = current;
    }

    /// Requests still pending, for an owner's client that just declared it
    /// reads them.
    fn announce_to(&mut self, id: crate::guests::SessionId, hub: &crate::guests::Hub) {
        for request in hub.approvals() {
            let _ = self.to(
                id,
                Update::GuestRequest {
                    id: request.id,
                    name: request.name.clone(),
                    timeout_s: request.remaining_s.min(3600) as u32,
                },
            );
        }
    }

    /// The encoder bitrate allowed for `target` (the leader's): every other
    /// session's own rate control, each pulling it down to a quarter at
    /// most (one bad link must not ruin everyone's picture; that session
    /// skips frames instead), and the bandwidth cap's share.
    fn limit(&self, target: u32) -> u32 {
        let leader = self.leader();
        let mut limit = target;
        for (id, session) in &self.sessions {
            if Some(*id) != leader
                && let Some(rates) = &session.rates
            {
                limit = limit.min(rates.target().max(target / 4));
            }
        }
        if let Some(share) = self.policy.share(self.sessions.len()) {
            limit = limit.min(share);
        }
        limit.max(protocol::MIN_BITRATE.min(target))
    }

    /// The lowest pad block no session uses.
    fn free_pad_block(&self) -> Option<u8> {
        (0..crate::guests::MAX_SESSIONS as u8)
            .find(|block| !self.sessions.values().any(|s| s.pad_block == *block))
    }

    /// Route a virtual pad's feedback to the session owning its block, with
    /// its own pad number.
    fn pad_feedback(&mut self, feedback: crate::gamepad::PadFeedback) {
        let mut update = pad_feedback_update(feedback);
        let (block, rumble) = match &mut update {
            Update::Rumble { pad, .. } => {
                let block = *pad / protocol::MAX_GAMEPADS;
                *pad %= protocol::MAX_GAMEPADS;
                (block, true)
            }
            Update::GamepadLights { pad, .. } | Update::GamepadTriggers { pad, .. } => {
                let block = *pad / protocol::MAX_GAMEPADS;
                *pad %= protocol::MAX_GAMEPADS;
                (block, false)
            }
            _ => return,
        };
        let target = self.sessions.values().find(|s| {
            s.pad_block == block
                && if rumble {
                    s.features.rumble
                } else {
                    s.features.lights
                }
        });
        if let Some(id) = target.map(|s| s.member.id) {
            let _ = self.to(id, update);
        }
    }

    /// Take session `id` out: release what it held, remove its pads and
    /// close its connection shortly after `notice`.
    async fn detach(
        &mut self,
        id: crate::guests::SessionId,
        reason: &str,
        notice: Option<String>,
        capture: &mut Capture,
    ) -> Option<crate::access::SessionRole> {
        let mut session = self.sessions.remove(&id)?;
        self.order.retain(|other| *other != id);
        if let Some(text) = notice {
            let _ = publish_update(&mut session.updates, Update::Notice { text });
        }
        if self.arbiter.forget(id) {
            capture.release_keyboard_mouse().await;
        }
        capture.destroy_pads(session.pad_block);
        tracing::info!(session = id, name = %session.role.name, role = session.role.role.label(), transport = session.member.kind, reason, "session left");
        // Its streams stop now; the first session's broadcast (holding the
        // shared tracks) stays with the engine.
        session.tasks.clear();
        if let Some(mut broadcast) = session.broadcast.take() {
            if session.shared {
                broadcast.finish();
            } else {
                self.primary = Some(broadcast);
            }
        }
        let role = session.role.clone();
        let member = session.member;
        tokio::spawn(async move {
            tokio::time::sleep(CLOSE_GRACE).await;
            member.close();
        });
        Some(role)
    }

    /// [`Self::detach`], then the session policy: guests leave with the
    /// last owner (unless kept).
    async fn remove(
        &mut self,
        id: crate::guests::SessionId,
        reason: &str,
        notice: Option<String>,
        capture: &mut Capture,
    ) {
        let Some(role) = self.detach(id, reason, notice, capture).await else {
            return;
        };
        if role.is_owner()
            && self.policy.kick_guests_with_owner
            && !self.sessions.values().any(|s| s.role.is_owner())
        {
            for guest in self.order.clone() {
                self.detach(
                    guest,
                    "the last owner left",
                    Some("The host's owner left; guests are disconnected".into()),
                    capture,
                )
                .await;
            }
        }
        if !self.sessions.is_empty() {
            let name = role.name.clone();
            for other in self.order.clone() {
                if self.sessions.get(&other).is_some_and(|s| s.role.is_owner()) && !role.is_owner()
                {
                    self.notice(other, format!("{name} left"));
                }
            }
        }
        self.send_access();
    }

    /// Before injecting during an encoder restart, when the loop's checks
    /// and commands wait: whether `id` is still authorized, with its
    /// current role.
    async fn recheck(&mut self, id: crate::guests::SessionId, capture: &mut Capture) -> bool {
        let Some(session) = self.sessions.get(&id) else {
            return false;
        };
        match session.member.current_role() {
            Some(role) if role == session.role => true,
            _ => {
                self.refresh(capture).await;
                self.sessions.contains_key(&id)
            }
        }
    }

    /// Re-read every session's role and permissions: revoked devices leave,
    /// changed permissions apply at once.
    async fn refresh(&mut self, capture: &mut Capture) {
        let mut revoked = Vec::new();
        for id in self.order.clone() {
            let Some(session) = self.sessions.get(&id) else {
                continue;
            };
            let Some(role) = session.member.current_role() else {
                revoked.push(id);
                continue;
            };
            if role == session.role {
                continue;
            }
            let before = session.role.clone();
            let block = session.pad_block;
            if let Some(session) = self.sessions.get_mut(&id) {
                session.role = role.clone();
            }
            if !role.is_owner() && !role.permissions.keyboard_mouse && self.arbiter.forget(id) {
                capture.release_keyboard_mouse().await;
            }
            if !role.is_owner() && !role.permissions.gamepad {
                capture.destroy_pads(block);
            }
            if (before.role, before.permissions) != (role.role, role.permissions) {
                tracing::info!(session = id, name = %role.name, permissions = %role.permissions.label(), "session permissions changed");
                self.notice(
                    id,
                    format!(
                        "The host changed what you may do: {}",
                        role.permissions.label()
                    ),
                );
            }
        }
        for id in revoked {
            self.remove(
                id,
                "device access revoked",
                Some("This device's access was revoked".into()),
                capture,
            )
            .await;
        }
        self.send_access();
    }
}

/// Copy a shared track into a session's own track, group by group, from
/// the latest group on (later sessions' video and audio).
async fn forward_track(
    source: moq_net::track::Producer,
    mut destination: moq_net::track::Producer,
) -> Result<()> {
    let mut groups = source.subscribe(None);
    if let Some(latest) = source.latest() {
        groups.start_at(latest);
    }
    let mut copies = tokio::task::JoinSet::new();
    loop {
        tokio::select! {
            group = groups.recv_group() => {
                let Some(mut group) = group? else { break };
                let mut copy = match destination.create_group(group.sequence.into()) {
                    Ok(copy) => copy,
                    // A sequence the session's track already has: skip it.
                    Err(_) => continue,
                };
                copies.spawn(async move {
                    while let Some(frame) = group.read_frame().await? {
                        copy.write_frame(frame.timestamp, frame.payload)?;
                    }
                    copy.finish()?;
                    Ok::<_, anyhow::Error>(())
                });
            }
            Some(_) = copies.join_next(), if !copies.is_empty() => (),
        }
    }
    Ok(())
}

/// Subscribe to a session's input track (it announces `controls` first).
async fn subscribe_input(
    incoming: &moq_net::origin::Producer,
) -> Result<moq_net::track::Subscriber> {
    let remote = incoming
        .consume()
        .announced_broadcast("controls")
        .await
        .context("input broadcast missing")?;
    Ok(remote
        .track("input")?
        .subscribe(
            moq_net::track::Subscription::default()
                .with_ordered(true)
                .with_priority(255)
                .with_latency_max(protocol::INPUT_TIMEOUT)
                .with_group_start(0),
        )
        .await?)
}

/// Departure reports from sessions' input and connection watchers.
type Departures = tokio::sync::mpsc::UnboundedSender<(crate::guests::SessionId, String)>;

/// Watch a session's connection and run its input task; either ending
/// reports a departure.
fn session_tasks(
    member: &Member,
    input: Option<moq_net::track::Subscriber>,
    events: &tokio::sync::mpsc::Sender<(crate::guests::SessionId, Event)>,
    departures: &Departures,
    last_motion: &Arc<AtomicU64>,
) -> Vec<AbortOnDrop> {
    let id = member.id;
    let mut tasks = Vec::new();
    {
        let (incoming, events, departures, last_motion) = (
            member.incoming.clone(),
            events.clone(),
            departures.clone(),
            last_motion.clone(),
        );
        tasks.push(AbortOnDrop(tokio::spawn(async move {
            let result = async {
                let input = match input {
                    Some(input) => input,
                    None => {
                        tokio::time::timeout(Duration::from_secs(10), subscribe_input(&incoming))
                            .await
                            .context("client did not open input track")??
                    }
                };
                receive_input(id, input, events, last_motion).await
            }
            .await;
            let reason = match &result {
                Ok(()) => "input ended".to_owned(),
                Err(error) => format!("{error:#}"),
            };
            let _ = departures.send((id, reason));
            Ok(())
        })));
    }
    {
        let (departure, departures) = (member.departure(), departures.clone());
        tasks.push(AbortOnDrop(tokio::spawn(async move {
            let reason = departure.await;
            let _ = departures.send((id, reason));
            Ok(())
        })));
    }
    tasks
}

impl Members {
    /// A session joining a running stream: its own broadcast forwards the
    /// shared tracks, with its own metadata and updates. Refused (and
    /// closed) when the host is full, the device was revoked, or it asks
    /// for something the shared stream cannot give it.
    #[allow(clippy::too_many_arguments)]
    fn join(
        &mut self,
        member: Member,
        shared: &Shared,
        info: &Desktop,
        options: &Options,
        hub: &crate::guests::Hub,
        events: &tokio::sync::mpsc::Sender<(crate::guests::SessionId, Event)>,
        departures: &Departures,
    ) -> Option<crate::guests::SessionId> {
        let refuse = |member: Member, reason: &str| {
            tracing::warn!(session = member.id, name = %member.role.name, reason, "session refused");
            member.close();
            None
        };
        if self.sessions.len() >= self.policy.max_sessions {
            return refuse(member, "the host's session limit is reached");
        }
        let Some(role) = member.current_role() else {
            return refuse(member, "device access revoked");
        };
        if member.needs_owner && !self.sessions.values().any(|s| s.role.is_owner()) {
            return refuse(member, "trusted guest, but no owner is connected any more");
        }
        let query = member.query.clone().unwrap_or_default();
        let format = match requested_format(Some(&query), options.codec) {
            Ok(format) => format,
            Err(_) => return refuse(member, "invalid connection request"),
        };
        if url::form_urlencoded::parse(query.as_bytes()).any(|(key, _)| key == "ssh-agent") {
            return refuse(member, "SSH agent forwarding is only for the first session");
        }
        if format.dynamic_range != options.dynamic_range {
            return refuse(member, "the shared stream's dynamic range differs");
        }
        let Some(pad_block) = self.free_pad_block() else {
            return refuse(member, "no gamepad slots left");
        };
        let built = (|| -> Result<_> {
            let mut broadcast = member.outgoing.create_broadcast(
                "desktop",
                moq_net::broadcast::Route::new().with_announce(true),
            )?;
            let mut idle = Vec::new();
            let mut tasks = Vec::new();
            for codec in protocol::VideoCodec::ALL {
                let track = broadcast.create_track(
                    codec.track(),
                    moq_net::track::Info::default().with_latency_max(Duration::from_millis(500)),
                )?;
                if codec == shared.codec {
                    tasks.push(AbortOnDrop(tokio::spawn(forward_track(
                        shared.video.clone(),
                        track,
                    ))));
                } else {
                    idle.push(track);
                }
            }
            for (index, source) in shared.monitors.iter().enumerate() {
                let track = broadcast.create_track(
                    format!("monitor-{index}-{}", shared.codec.track()),
                    moq_net::track::Info::default().with_latency_max(Duration::from_millis(500)),
                )?;
                tasks.push(AbortOnDrop(tokio::spawn(forward_track(
                    source.clone(),
                    track,
                ))));
            }
            let metadata = broadcast.create_track("desktop", None)?;
            let updates = broadcast.create_track(
                "updates",
                moq_net::track::Info::default().with_ordered(true),
            )?;
            // Both audio tracks, as for the first session: the shared
            // format's is forwarded, whichever the client would have chosen
            // (it is told the stream's settings in `Desktop::audio_settings`).
            if let Some((settings, audio)) = &shared.audio {
                for format in [protocol::AudioFormat::Opus, protocol::AudioFormat::Pcm] {
                    let track = broadcast.create_track(format.track(), None)?;
                    if format == settings.format {
                        tasks.push(AbortOnDrop(tokio::spawn(forward_track(
                            audio.clone(),
                            track,
                        ))));
                    } else {
                        idle.push(track);
                    }
                }
            }
            Ok((broadcast, idle, tasks, metadata, updates))
        })();
        let (broadcast, idle, mut tasks, metadata, updates) = match built {
            Ok(built) => built,
            Err(error) => {
                tracing::warn!(
                    error = format!("{error:#}"),
                    "could not publish to a joining session"
                );
                return refuse(member, "publishing failed");
            }
        };
        let id = member.id;
        let last_motion = Arc::new(AtomicU64::new(0));
        tasks.extend(session_tasks(
            &member,
            None,
            events,
            departures,
            &last_motion,
        ));
        let now = Instant::now();
        let mut session = Session {
            member,
            role: role.clone(),
            updates,
            metadata,
            broadcast: Some(broadcast),
            _idle: idle,
            tasks,
            pointer: false,
            last_motion,
            pad_block,
            features: Features::default(),
            rates: None,
            display: None,
            joined: now,
            last_input: now,
            last_feedback: now,
            last_keyframe: None,
            last_refusal: None,
            sent_access: None,
            shared: true,
            ssh_agent: false,
        };
        let mut rates_options = options.clone();
        session.rates = Some(Rates::new(&mut rates_options, info));
        self.sessions.insert(id, session);
        self.order.push(id);
        let desktop = self.customize(id, info);
        if let Some(session) = self.sessions.get_mut(&id) {
            let written = (|| -> Result<()> {
                let mut group = session.metadata.append_group()?;
                group.write_frame(moq_net::Timestamp::now(), serde_json::to_vec(&desktop)?)?;
                group.finish()?;
                Ok(())
            })();
            if let Err(error) = written {
                tracing::warn!(%error, session = id, "could not send the joining session its desktop");
                self.failed.push(id);
            }
        }
        tracing::info!(session = id, name = %role.name, role = role.role.label(), permissions = %role.permissions.label(), transport = self.sessions[&id].member.kind, "session joined the shared desktop");
        if !role.is_owner() {
            self.notice_owners(&format!(
                "{} joined as a guest ({})",
                role.name,
                role.permissions.label()
            ));
        }
        self.send_access();
        self.publish(hub);
        Some(id)
    }
}

/// The session engine: one capture and one encoder (per stream) for every
/// session on the desktop (docs/guests.md). `first` chooses the codec and
/// leads the stream (size, monitor, privacy) until an owner is there to.
/// Sessions join through `joins`; the engine ends when the last one leaves.
async fn run_session(
    options: &Options,
    hub: &crate::guests::Hub,
    capture: &mut Capture,
    first: Member,
    joins: &mut tokio::sync::mpsc::Receiver<Member>,
    commands: &mut tokio::sync::mpsc::UnboundedReceiver<crate::guests::Command>,
) -> Result<Option<Member>> {
    use crate::guests::Command;
    // Revoked while it waited for the engine: never started.
    let mut first = first;
    first.role = match first.current_role() {
        Some(role) => role,
        None => {
            first.close();
            anyhow::bail!("device access revoked");
        }
    };
    // A trusted guest let in because an owner was there: not alone.
    if first.needs_owner {
        first.close();
        anyhow::bail!("trusted guest, but no owner is connected any more");
    }
    // Admitted: a Linux account sign-in unlocks the session now.
    unlock_after_system_login(options, first.system_login()).await;
    // Stream preferences belong to this connection, not the next paired client.
    let mut negotiated = options.clone();
    let query = first.query.clone();
    let requested = requested_format(query.as_deref(), options.codec)?;
    let ssh_requested =
        url::form_urlencoded::parse(query.as_deref().unwrap_or_default().as_bytes())
            .any(|(key, value)| key == "ssh-agent" && value == "1");
    ensure!(
        !ssh_requested || options.allow_ssh_agent,
        "SSH agent forwarding is disabled; host requires explicit --allow-ssh-agent"
    );
    ensure!(
        !ssh_requested || first.role.is_owner(),
        "guests cannot forward an SSH agent"
    );
    negotiated.allow_ssh_agent = ssh_requested && options.allow_ssh_agent;
    negotiated.codec = requested.codec;
    negotiated.dynamic_range = requested.dynamic_range;
    let options = &mut negotiated;
    // Each client opts in again; a new session starts with the pointer in video.
    capture.local_cursor = false;
    capture.select_monitor(capture.active_monitor).await?;
    let mut broadcast = first.outgoing.create_broadcast(
        "desktop",
        moq_net::broadcast::Route::new().with_announce(true),
    )?;
    // Every codec's track exists until the client has chosen (SelectCodec):
    // it subscribes to its choice right after sending it.
    let mut video_tracks = Vec::new();
    for codec in protocol::VideoCodec::ALL {
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
    for codec in protocol::VideoCodec::ALL {
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
    let metadata = broadcast.create_track("desktop", None)?;
    let updates = broadcast.create_track(
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
    let input = tokio::time::timeout(Duration::from_secs(10), subscribe_input(&first.incoming))
        .await
        .context("client did not open input track")??;
    let first_id = first.id;
    let mut ssh_agent = if let Some(requests) = agent_requests {
        let responses = tokio::time::timeout(Duration::from_secs(10), async {
            let remote = first
                .incoming
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
    let (codecs, formats) = match &options.smoke_hardware_codecs {
        Some(codecs) => (codecs.clone(), HardwareFormats::default()),
        None => tokio::task::spawn_blocking(move || {
            (
                media::hardware_codecs(kind),
                HardwareFormats {
                    chroma_444: media::hardware_444_codecs(kind),
                    ten_bit: media::hardware_ten_bit_codecs(kind),
                },
            )
        })
        .await
        .context("hardware encoder probe failed")?,
    };
    options.hardware_codecs = codecs;
    *options.hardware_formats = formats;
    options.chroma_444 = false;
    options.ten_bit = false;
    let (sender, receiver) = tokio::sync::mpsc::channel(256);
    let mut events = SessionEvents {
        receiver,
        deferred: Default::default(),
    };
    let (departures_sender, mut departures) = tokio::sync::mpsc::unbounded_channel();
    let mut members = Members::new(hub.policy.clone());
    {
        let last_motion = Arc::new(AtomicU64::new(0));
        // A task of its own, so input keeps arriving while the session loop
        // waits for an encoder to start or stop (see while_starting).
        let tasks = session_tasks(
            &first,
            Some(input),
            &sender,
            &departures_sender,
            &last_motion,
        );
        let now = Instant::now();
        let role = first.role.clone();
        members.sessions.insert(
            first_id,
            Session {
                member: first,
                role: role.clone(),
                updates,
                metadata,
                broadcast: Some(broadcast),
                _idle: Vec::new(),
                tasks,
                pointer: false,
                last_motion,
                pad_block: 0,
                features: Features::default(),
                rates: None,
                display: None,
                joined: now,
                last_input: now,
                last_feedback: now,
                last_keyframe: None,
                last_refusal: None,
                sent_access: None,
                shared: false,
                ssh_agent: options.allow_ssh_agent,
            },
        );
        members.order.push(first_id);
        tracing::info!(session = first_id, name = %role.name, role = role.role.label(), "session started the desktop");
        members.publish(hub);
        members.sync_approvals(hub);
    }
    // The metadata goes out before video starts: a client that chooses the
    // codec (Desktop::codec_selection) needs it to decide.
    let mut info = desktop(options, capture, video_start_group)?;
    {
        let desktop = members.customize(first_id, &info);
        let session = members
            .sessions
            .get_mut(&first_id)
            .context("first session missing")?;
        let mut group = session.metadata.append_group()?;
        group.write_frame(moq_net::Timestamp::now(), serde_json::to_vec(&desktop)?)?;
        group.finish()?;
    }
    let mut check = tokio::time::interval(Duration::from_millis(200));
    // A client choosing the codec sends SelectCodec first. Older clients send
    // something else (at least a heartbeat within INPUT_TIMEOUT) and get the
    // codec from their connection request, as before.
    let first_event = tokio::time::timeout(protocol::INPUT_TIMEOUT, events.receiver.recv()).await;
    // Older clients play Opus from the "opus" track only.
    let mut audio_settings = protocol::AudioSettings {
        format: protocol::AudioFormat::Opus,
        bitrate: options.audio_bitrate,
    };
    let chosen_by_client = match first_event {
        Ok(Some((
            _,
            Event::SelectCodec {
                codec,
                encoder_preset,
                audio,
                chroma_444,
                ten_bit,
            },
        ))) => {
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
            ensure!(
                codec != protocol::VideoCodec::Av1 || offers_av1(options),
                "this host has no AV1 encoder"
            );
            ensure!(
                !chroma_444
                    || (codec.supports_444()
                        && options.dynamic_range == protocol::DynamicRange::Sdr
                        && crate::ffmpeg::enabled()?),
                "4:4:4 needs H.264 or H.265 SDR with the FFmpeg backend"
            );
            ensure!(
                !ten_bit
                    || (codec.supports_ten_bit()
                        && !chroma_444
                        && options.dynamic_range == protocol::DynamicRange::Sdr
                        && crate::ffmpeg::enabled()?),
                "10-bit SDR needs H.265 or AV1 without 4:4:4 and the FFmpeg backend"
            );
            options.codec = codec;
            options.chroma_444 = chroma_444;
            options.ten_bit = ten_bit;
            true
        }
        Ok(Some(tagged)) => {
            events.deferred.push_back(tagged);
            false
        }
        // The input task reports a closed or silent input stream.
        Ok(None) | Err(_) => false,
    };
    tracing::info!(
        codec = options.codec.label(),
        chroma_444 = options.chroma_444,
        ten_bit = options.ten_bit,
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
    // Desktop audio in the first session's format and bitrate; later
    // sessions forward this one track (docs/guests.md).
    let audio_track = match options.audio_source {
        Some(_) => Some(
            audio_tracks
                .into_iter()
                .find_map(|(format, track)| (format == audio_settings.format).then_some(track))
                .context("no track for the session audio format")?,
        ),
        None => None,
    };
    let shared = Shared {
        codec: options.codec,
        video: video.clone(),
        monitors: monitor_tracks.clone(),
        audio: audio_track.clone().map(|track| (audio_settings, track)),
    };
    members.stream_audio = shared.audio.as_ref().map(|(settings, _)| *settings);
    let started = start_encoder(
        options,
        capture.pipeline_source_for_range(options.dynamic_range)?,
        (info.width, info.height),
        options.fps,
        members.limit(options.bitrate),
        &video,
    )
    .await
    .context("encoder start failed")?;
    let mut pipeline = match started {
        Ok(pipeline) => pipeline,
        // KDE can drop the portal's PipeWire stream without closing the
        // session (displays hotplugging as they wake): PipeWire then has no
        // such node and the capture never starts. Recreate it once.
        Err(error) if capture.is_portal() => {
            tracing::warn!(
                error = format!("{error:#}"),
                "capture would not start; recreating the desktop portal session"
            );
            capture.release_all().await;
            reopen_capture(capture).await?;
            info = desktop(options, capture, video_start_group)?;
            members.desktop(&info)?;
            start_encoder(
                options,
                capture.pipeline_source_for_range(options.dynamic_range)?,
                (info.width, info.height),
                options.fps,
                members.limit(options.bitrate),
                &video,
            )
            .await
            .context("encoder start failed")??
        }
        Err(error) => return Err(error),
    };
    let audio = match (options.audio_source.as_deref(), audio_track) {
        (Some(source), Some(track)) => {
            tracing::info!(audio = audio_settings.label(), "session desktop audio");
            Some(crate::audio::capture(source, audio_settings, track)?)
        }
        _ => None,
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
    // The leading client's latest display (size, refresh, choices). Applied
    // once the video-settings rate limit allows, coalescing bursts into the
    // newest.
    let mut client_display: Option<protocol::ClientDisplay> = None;
    let mut display_pending = false;
    // Feedback from the virtual pads (rumble, PlayStation lights and trigger
    // effects), once a client plays it.
    let mut rumble: Option<tokio::sync::mpsc::UnboundedReceiver<crate::gamepad::PadFeedback>> =
        None;
    let mut feedback_sender: Option<crate::gamepad::FeedbackSender> = None;
    // The session that asked for the auxiliary monitor streams (the leader
    // when it did): their restarts are reported to it.
    let mut monitor_viewer = first_id;
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
    // One session's microphone, played into the virtual microphone (G20):
    // there is one virtual microphone, so one session at a time. Dropped
    // (and the virtual microphone removed) when that session leaves, loses
    // the permission, or the engine ends.
    let mut microphone: Option<(crate::guests::SessionId, SessionMicrophone)> = None;
    let mut microphone_starting: Option<(
        crate::guests::SessionId,
        AbortOnDrop<Result<SessionMicrophone>>,
    )> = None;
    // The stream leader, to notice when another session takes over.
    let mut leader = members.leader();
    let mut access_check = Instant::now();
    loop {
        // Sessions whose updates failed, then the end of the engine.
        for id in std::mem::take(&mut members.failed) {
            members
                .remove(id, "session updates closed", None, capture)
                .await;
        }
        if members.sessions.is_empty() {
            tracing::info!("last session left");
            return Ok(None);
        }
        // A guest who may type could use the forwarded agent like any
        // program of the host user: forwarding ends.
        if ssh_agent.is_some()
            && members
                .sessions
                .values()
                .any(|s| !s.role.is_owner() && s.role.permissions.keyboard_mouse)
        {
            ssh_agent = None;
            tracing::warn!("SSH agent forwarding stopped: a guest may use the keyboard and mouse");
            members.notice(
                first_id,
                "SSH agent forwarding stopped: a guest with keyboard and mouse could use it",
            );
        }
        if members.leader() != leader {
            leader = members.leader();
            // The new leader's display decides the stream from now on.
            if let Some(display) = leader
                .and_then(|id| members.sessions.get_mut(&id))
                .and_then(|session| session.display.take())
            {
                client_display = Some(display);
                display_pending = true;
            }
            tracing::info!(leader = ?leader, "another session leads the stream now");
            members.send_access();
        }
        if cursor_restart.is_some() || display_change.is_some() {
            let local = cursor_restart.take();
            let change = display_change.take();
            // Restart capture as for a monitor switch: without the pointer
            // (X11), or in the other portal cursor mode (a new ScreenCast
            // session, so every stream stops first).
            capture.release_all().await;
            while_starting(retire(pipeline), &mut events, capture, &mut members).await?;
            let active: Vec<_> = monitor_streams.keys().copied().collect();
            while_starting(
                retire(std::mem::take(&mut monitor_streams)),
                &mut events,
                capture,
                &mut members,
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
                    members.leader_update(Update::Notice { text })?;
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
                    members.limit(options.bitrate),
                    &video,
                ),
                &mut events,
                capture,
                &mut members,
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
                    &mut members,
                )
                .await
                {
                    Ok(stream) => {
                        members.to(
                            monitor_viewer,
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
                    Err(error) => members.to(
                        monitor_viewer,
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
            members.desktop(&info)?;
            // Portal shapes arrive through the feed (`pw_cursor::subscribe`).
            if local == Some(true)
                && let Some(CursorFeed::X11(watcher)) = cursor_watcher.as_mut()
                && let Some(cursor) = watcher.shapes.borrow_and_update().clone()
            {
                members.all(Update::Cursor { cursor })?;
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
            while_starting(retire(pipeline), &mut events, capture, &mut members).await?;
            for index in std::mem::take(&mut monitor_streams).into_keys() {
                members.to(
                    monitor_viewer,
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
                    members.limit(options.bitrate),
                    &video,
                ),
                &mut events,
                capture,
                &mut members,
            )
            .await?;
            session_video(&pipeline, options.fps, long_gop);
            rates = Rates::new(options, &info);
            encoder_metrics = pipeline.encoder_metrics()?;
            adaptive = !options.fixed_bitrate;
            last_feedback = Instant::now();
            input_failures = 0;
            members.desktop(&info)?;
            members.all(Update::Notice {
                text: "Host displays changed; video restarted".into(),
            })?;
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
            result = crate::ssh_agent::closed(&mut ssh_agent) => {
                // The agent belongs to the first session: it leaves with it.
                ssh_agent = None;
                let reason = match result { Ok(()) => "SSH agent forwarding ended".to_owned(), Err(error) => format!("{error:#}") };
                members.remove(first_id, &reason, None, capture).await;
            },
            effect = async {
                match rumble.as_mut() {
                    Some(receiver) => receiver.recv().await,
                    None => std::future::pending().await,
                }
            } => match effect {
                Some(feedback) => members.pad_feedback(feedback),
                None => rumble = None,
            },
            departure = departures.recv() => {
                if let Some((id, reason)) = departure {
                    members.remove(id, &reason, None, capture).await;
                    members.publish(hub);
                    // Its share and its rate control no longer bound the rest.
                    if !members.sessions.is_empty() {
                        let target = members.limit(rates.target());
                        if target != pipeline.video_bitrate()? {
                            let _ = pipeline.set_video_bitrate(target);
                        }
                    }
                }
            },
            member = joins.recv() => {
                if let Some(member) = member {
                    // A new connection with a session's credential is that
                    // device reconnecting: the old session (often already
                    // dead, not yet timed out) gives way.
                    let stale: Vec<_> = members.order.iter().copied().filter(|id| {
                        members.sessions.get(id).is_some_and(|s| s.member.credential == member.credential)
                    }).collect();
                    for id in stale {
                        members.detach(id, "replaced by a new connection with the same credential", None, capture).await;
                    }
                    if members.sessions.is_empty() {
                        // It was the only one: start over with the new
                        // connection as the first session (it chooses the
                        // codec and leads), as for a fresh connection.
                        return Ok(Some(member));
                    }
                    members.send_access();
                    let info = desktop(options, capture, video_start_group)?;
                    let system_login = member.system_login();
                    if members.join(member, &shared, &info, options, hub, &sender, &departures_sender).is_some() {
                        unlock_after_system_login(options, system_login).await;
                        // The new session starts at the next keyframe, not
                        // at the end of a long GOP.
                        let _ = pipeline.request_keyframe();
                        for stream in monitor_streams.values() {
                            let _ = stream.pipeline.request_keyframe();
                        }
                        let target = members.limit(rates.target());
                        if target != pipeline.video_bitrate()? {
                            let _ = pipeline.set_video_bitrate(target);
                        }
                    }
                }
            },
            command = commands.recv() => match command {
                Some(Command::Kick(id)) => {
                    // A kicked guest cannot simply reconnect at once.
                    if let Some(device) = members.sessions.get(&id).filter(|s| !s.role.is_owner()).and_then(|s| s.role.device.clone()) {
                        hub.block(&device, crate::guests::KICKED_FOR);
                    }
                    members.remove(id, "disconnected by the host", Some("The host disconnected this session".into()), capture).await;
                    members.publish(hub);
                }
                Some(Command::Refresh) => {
                    members.refresh(capture).await;
                    members.publish(hub);
                }
                Some(Command::TakeControl) => {
                    if let Some(previous) = members.arbiter.take_for_host(Instant::now()) {
                        capture.release_keyboard_mouse().await;
                        members.notice(previous, "The host took keyboard and mouse control");
                    }
                    tracing::info!("the host took keyboard and mouse control");
                    members.send_access();
                    members.publish(hub);
                }
                Some(Command::ReleaseControl) => {
                    members.arbiter.release();
                    tracing::info!("guests may take keyboard and mouse control again");
                    members.send_access();
                    members.publish(hub);
                }
                Some(Command::Approvals) => members.sync_approvals(hub),
                Some(Command::Shutdown) | None => return Ok(None),
            },
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
                        members.all(Update::Notice { text: "Host cursor updates stopped".into() })?;
                        None
                    }
                };
                if let Some(cursor) = shape {
                    members.all(Update::Cursor { cursor })?;
                }
            },
            // Latest host pointer position, rate limited (portal metadata).
            _ = position_tick.tick(), if cursor_positions => {
                if let Some(CursorFeed::Portal(feed)) = cursor_watcher.as_mut()
                    && feed.positions.has_changed().unwrap_or(false)
                    && let Some(position) = *feed.positions.borrow_and_update() {
                    members.all(Update::CursorPosition { monitor: position.monitor, x: position.x, y: position.y })?;
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
                            members.desktop(&desktop(options, capture, video_start_group)?)?;
                            format!("Virtual monitor added as monitor {}", index + 1)
                        }
                        Err(error) => {
                            virtual_requested = false;
                            tracing::warn!(error = format!("{error:#}"), "virtual monitor failed");
                            format!("Virtual monitor unavailable: {error:#}")
                        }
                    };
                    members.leader_update(Update::Notice { text })?;
                } else if let (Some(size), Some(index)) = (size, capture.virtual_monitor())
                    && capture.virtual_monitor_resizable()
                    && (capture.monitors[index].width, capture.monitors[index].height) != (size.width, size.height)
                {
                    // Follow the client's display: stop its stream, resize, restart it.
                    let running = monitor_streams.remove(&index);
                    let was_running = running.is_some();
                    if let Some(stream) = running { while_starting(retire(stream), &mut events, capture, &mut members).await?; }
                    if let Err(error) = capture.resize_virtual(index, size).await {
                        tracing::warn!(error = format!("{error:#}"), "virtual monitor resize failed");
                    }
                    members.desktop(&desktop(options, capture, video_start_group)?)?;
                    if was_running {
                        match monitor_encoder(options, capture, index, &monitor_tracks[index], &mut events, &mut members).await {
                            Ok(stream) => {
                                members.to(monitor_viewer, Update::MonitorStream { index, desktop: stream.desktop.clone() })?;
                                if long_gop { stream.pipeline.set_key_interval(options.fps.saturating_mul(LONG_KEY_INTERVAL_SECONDS)); }
                                monitor_streams.insert(index, stream);
                            }
                            Err(error) => members.to(monitor_viewer, Update::MonitorStreamStopped { index, reason: format!("Monitor restart failed: {error}"), requested: false })?,
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
                    if let Some(leader) = members.leader() {
                        events.deferred.push_back((leader, Event::ConfigureVideo { width: choice.width, fps: choice.fps, bitrate: choice.bitrate }));
                    }
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
                // Revocation, permission changes, idle guests; the session list.
                if access_check.elapsed() >= ACCESS_CHECK_INTERVAL {
                    access_check = Instant::now();
                    members.refresh(capture).await;
                    if let Some(idle) = members.policy.guest_idle_kick {
                        let now = Instant::now();
                        let idle_guests: Vec<_> = members.order.iter().copied().filter(|id| {
                            members.sessions.get(id).is_some_and(|s| !s.role.is_owner() && now.duration_since(s.last_input) >= idle)
                        }).collect();
                        for id in idle_guests {
                            members.remove(id, "idle", Some(format!("Disconnected after {} minutes without input", idle.as_secs() / 60)), capture).await;
                        }
                    }
                    members.publish(hub);
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
                            members.leader_update(Update::Notice { text: format!("Privacy mode could not keep a host monitor off: {error:#}") })?;
                        }
                    }
                }
                let failed: Vec<_> = monitor_streams.iter().filter_map(|(&index, stream)| stream.pipeline.error().err().map(|error| (index, error))).collect();
                for (index, error) in failed {
                    monitor_streams.remove(&index);
                    capture.release_all().await;
                    members.to(monitor_viewer, Update::MonitorStreamStopped { index, reason: format!("Monitor stream stopped: {error}"), requested: false })?;
                }
                if let Some(audio) = &audio { audio.error()?; }
                if let Some((id, mut task)) = microphone_starting.take_if(|(_, task)| task.0.is_finished()) {
                    let started = (&mut task.0).await.map_err(anyhow::Error::from).and_then(|started| started);
                    match started {
                        // Kept only if it may still send one.
                        Ok(started) if members.may_send_microphone(id) => {
                            tracing::info!(session = id, "session's microphone plays into the virtual microphone");
                            microphone = Some((id, started));
                            members.notice(id, format!("Your microphone is \"{}\" on the host", crate::virtual_mic::DESCRIPTION));
                        }
                        Ok(_) => (),
                        Err(error) => {
                            tracing::warn!(session = id, error = format!("{error:#}"), "client microphone unavailable");
                            members.notice(id, format!("Microphone unavailable on the host: {error:#}"));
                        }
                    }
                }
                // The microphone stops when its session leaves or may no
                // longer send one (permission changed, revoked).
                if let Some((id, _)) = &microphone
                    && !members.may_send_microphone(*id)
                {
                    let id = *id;
                    microphone = None;
                    tracing::info!(session = id, "client microphone stopped: the session left or lost the permission");
                    members.notice(id, "Your microphone no longer reaches the host");
                }
                if let Some((id, Err(error))) = microphone.as_mut().map(|(id, microphone)| (*id, microphone.check())) {
                    microphone = None;
                    tracing::warn!(session = id, error = format!("{error:#}"), "client microphone stopped");
                    members.notice(id, format!("Microphone stopped on the host: {error:#}"));
                }
                if adaptive && let Some(session) = members.leader_session() {
                    let stats = session.stats();
                    // Clients sending NetworkFeedback are rate-controlled from it.
                    if rates.delay.is_none()
                        && let (Some(rtt), Some(sent), Some(lost)) = (stats.rtt, stats.packets_sent, stats.packets_lost)
                        && rates.network.observe(Instant::now(), rtt, sent, lost).is_some() {
                        let bitrate = members.limit(rates.target());
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
                        while_starting(retire(pipeline), &mut events, capture, &mut members).await?;
                        video_start_group = video_barrier(&video)?;
                        let info = desktop(options, capture, video_start_group)?;
                        capture.select_monitor(capture.active_monitor).await?;
                        pipeline = while_starting(start_encoder(options, capture.pipeline_source_for_range(options.dynamic_range)?, (info.width, info.height), options.fps, members.limit(kbps), &video), &mut events, capture, &mut members).await?;
                        session_video(&pipeline, options.fps, long_gop);
                        encoder_metrics = pipeline.encoder_metrics()?;
                        (rates.encoded_bytes, rates.key_bytes) = (0, 0);
                        members.desktop(&info)?;
                    }
                }
                if telemetry_enabled && last_telemetry.elapsed() >= Duration::from_secs(1) {
                    last_telemetry = Instant::now();
                    members.all(Update::Telemetry {
                        encode_us: encoder_metrics.encode_us.load(std::sync::atomic::Ordering::Relaxed),
                        bitrate: pipeline.video_bitrate()?,
                        encoder: encoder_metrics.encoder.clone(),
                    })?;
                }
            },
            tagged = events.next() => {
                let (from, event) = tagged.context("input stream closed")?;
                // Owner-only events act on the role now, not the one cached
                // until the next refresh (a demotion or revocation racing).
                if matches!(crate::guests::need(&event), crate::guests::Need::Owner)
                    && !members.recheck(from, capture).await
                {
                    continue;
                }
                let release = match members.screen(from, &event, Instant::now()) {
                    Screen::Drop => continue,
                    Screen::Pass { release } => release,
                };
                if release {
                    // Control changed hands: what the previous holder held
                    // is released before the new holder's input.
                    capture.release_keyboard_mouse().await;
                }
                match event {
                    Event::MonitorStream { index, enabled } => {
                        monitor_viewer = from;
                        if !enabled {
                            if let Some(stream) = monitor_streams.remove(&index) { while_starting(retire(stream), &mut events, capture, &mut members).await?; }
                            capture.release_all().await;
                            members.to(from, Update::MonitorStreamStopped { index, reason: "Monitor stream disabled".into(), requested: true })?;
                            continue;
                        }
                        if let Some(stream) = monitor_streams.get(&index) {
                            let update = Update::MonitorStream { index, desktop: stream.desktop.clone() };
                            members.to(from, update)?;
                            continue;
                        }
                        if monitor_streams.len() >= 4 || index >= monitor_tracks.len() {
                            members.to(from, Update::MonitorStreamStopped { index, reason: "Monitor unavailable or four additional streams already active".into(), requested: false })?;
                            continue;
                        }
                        match monitor_encoder(options, capture, index, &monitor_tracks[index], &mut events, &mut members).await {
                            Ok(stream) => {
                                members.to(from, Update::MonitorStream { index, desktop: stream.desktop.clone() })?;
                                if long_gop { stream.pipeline.set_key_interval(options.fps.saturating_mul(LONG_KEY_INTERVAL_SECONDS)); }
                                monitor_streams.insert(index, stream);
                            }
                            Err(error) => members.to(from, Update::MonitorStreamStopped { index, reason: format!("Monitor unavailable: {error}"), requested: false })?,
                        }
                    }
                    Event::MonitorMotion { index, x, y } => {
                        if monitor_streams.contains_key(&index)
                            && let Err(error) = capture.monitor_motion(index, x, y).await {
                            monitor_streams.remove(&index);
                            capture.release_all().await;
                            members.to(monitor_viewer, Update::MonitorStreamStopped { index, reason: format!("Monitor input unavailable: {error}"), requested: false })?;
                        }
                    }
                    // Auxiliary streams currently use fixed bitrate. Their
                    // congestion must never adapt the independent primary.
                    Event::MonitorFeedback { .. } => {}
                    Event::ClientFeatures { keyframe_requests , pointer_datagrams, local_cursor, rumble: plays_rumble, cursor_positions: wants_positions, frame_timing, privacy: wants_privacy, virtual_monitor: wants_virtual, gamepad_lights, guests, microphone: wants_microphone, video_formats } => {
                        let is_leader = members.is_leader(from);
                        let first_declaration = members.sessions.get(&from).is_some_and(|s| !s.features.declared);
                        if let Some(session) = members.sessions.get_mut(&from) {
                            session.features = Features {
                                declared: true,
                                guests: session.features.guests || guests,
                                rumble: session.features.rumble || plays_rumble,
                                lights: session.features.lights || gamepad_lights,
                                frame_timing: session.features.frame_timing || frame_timing,
                            };
                        }
                        if guests && first_declaration {
                            members.send_access();
                            if members.sessions.get(&from).is_some_and(|s| s.role.is_owner()) {
                                members.announce_to(from, hub);
                            }
                        }
                        if plays_rumble || gamepad_lights {
                            // One channel for every session's pads; later
                            // declarations only widen what new pads offer.
                            let sender = match &feedback_sender {
                                Some(sender) => sender.clone(),
                                None => {
                                    let (sender, receiver) = tokio::sync::mpsc::unbounded_channel();
                                    rumble = Some(receiver);
                                    feedback_sender = Some(sender.clone());
                                    sender
                                }
                            };
                            capture.enable_pad_feedback(sender, plays_rumble, gamepad_lights);
                        }
                        if pointer_datagrams
                            && let Some(session) = members.sessions.get_mut(&from)
                            && !session.pointer
                        {
                            session.pointer = true;
                            let task = AbortOnDrop(tokio::spawn(receive_pointer(from, session.member.incoming.clone(), sender.clone(), session.last_motion.clone())));
                            session.tasks.push(task);
                        }
                        // A shared stream already stamped for others cannot
                        // be read by a client that cannot strip the stamps.
                        if !is_leader && options.frame_timing && !frame_timing {
                            members.remove(from, "client cannot read the shared stream", Some("Update Teleport to join this shared desktop".into()), capture).await;
                            continue;
                        }
                        // Nor one in AV1, 4:4:4 or 10-bit by a client that
                        // does not know those formats (newer clients check
                        // they can decode it, and leave when not).
                        if !is_leader
                            && !video_formats
                            && (options.chroma_444 || options.ten_bit || options.codec == protocol::VideoCodec::Av1)
                        {
                            let shared = crate::client::stream_settings::format_label(options.codec, options.chroma_444, options.ten_bit);
                            members.remove(from, "client cannot decode the shared stream", Some(format!("This desktop is shared as {shared}; update Teleport to join it")), capture).await;
                            continue;
                        }
                        // One virtual microphone: owners' clients, or a guest
                        // granted the microphone permission (a guest's voice
                        // in the host's voice apps would speak as the host
                        // user). The first session to send one keeps it until
                        // it stops.
                        if wants_microphone
                            && microphone.as_ref().is_none_or(|(id, _)| *id != from)
                            && microphone_starting.as_ref().is_none_or(|(id, _)| *id != from)
                        {
                            // A holder that left (a reconnect replaced it)
                            // or lost the permission frees the slot now, not
                            // at the next tick; an owner takes it from a
                            // guest.
                            let requester_owner = members.sessions.get(&from).is_some_and(|s| s.role.is_owner());
                            let holder = microphone.as_ref().map(|(id, _)| *id).or(microphone_starting.as_ref().map(|(id, _)| *id));
                            if let Some(holder) = holder
                                && (!members.may_send_microphone(holder)
                                    || (requester_owner && members.sessions.get(&holder).is_some_and(|s| !s.role.is_owner())))
                            {
                                microphone = None;
                                microphone_starting = None;
                                if members.sessions.contains_key(&holder) {
                                    tracing::info!(session = holder, by = from, "client microphone stopped: an owner took the virtual microphone");
                                    members.notice(holder, "The host's owner took the microphone; yours no longer reaches the host");
                                }
                            }
                            let refused = if !options.microphone_available {
                                Some("This host does not take a microphone (teleport host --microphone)")
                            } else if !members.may_send_microphone(from) {
                                Some("The host has not allowed you to send your microphone")
                            } else if microphone.is_some() || microphone_starting.is_some() {
                                Some("Another session's microphone is playing on the host; only one at a time")
                            } else {
                                None
                            };
                            match refused {
                                Some(text) => members.notice(from, text),
                                // Started beside the loop: the client's track
                                // may take a while (or never come).
                                None => if let Some(incoming) = members.sessions.get(&from).map(|s| s.member.incoming.clone()) {
                                    microphone_starting = Some((from, AbortOnDrop(tokio::spawn(async move {
                                        tokio::time::timeout(MICROPHONE_START_TIMEOUT, SessionMicrophone::start(incoming))
                                            .await
                                            .context("the client's microphone track did not arrive")?
                                    }))));
                                },
                            }
                        }
                        if !is_leader {
                            // The rest belongs to the session leading the stream.
                            continue;
                        }
                        if frame_timing && !options.frame_timing {
                            // Encoders started later inherit it from options.
                            options.frame_timing = true;
                            pipeline.set_frame_timing();
                            for stream in monitor_streams.values() { stream.pipeline.set_frame_timing(); }
                        }
                        // The host's monitors are the owner's business.
                        let owner = members.sessions.get(&from).is_some_and(|s| s.role.is_owner());
                        let (wants_privacy, wants_virtual) = (wants_privacy && owner, wants_virtual && owner);
                        if wants_privacy && privacy.is_none() && !capture.privacy() {
                            if options.privacy_available {
                                display_change = Some(DisplayChange::Privacy);
                            } else {
                                members.to(from, Update::Notice { text: "Privacy mode is off on this host (teleport host --privacy-mode request)".into() })?;
                            }
                        }
                        if wants_virtual && !virtual_requested {
                            if capture.virtual_monitors_available() {
                                virtual_requested = true;
                                // Added by the display handler, at the client's size.
                                display_pending = true;
                            } else {
                                members.to(from, Update::Notice { text: "This host cannot add a virtual monitor".into() })?;
                            }
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
                    Event::SelectCodec { codec, .. } => if codec != options.codec {
                        members.remove(from, "asked for another codec", Some("The video codec is chosen before video starts; reconnect to change it".into()), capture).await;
                    },
                    Event::RequestKeyframe { monitor } => {
                        // Bound IDR bursts: a lossy link must not turn every
                        // request into a full keyframe and deepen congestion.
                        // A guest's requests are spaced further apart (they
                        // cost every session bitrate).
                        let now = Instant::now();
                        let guest_ready = members.sessions.get_mut(&from).is_none_or(|session| {
                            if session.role.is_owner() {
                                return true;
                            }
                            let ready = session.last_keyframe.is_none_or(|at| now.duration_since(at) >= GUEST_KEYFRAME_INTERVAL);
                            if ready {
                                session.last_keyframe = Some(now);
                            }
                            ready
                        });
                        if guest_ready && last_keyframe.get(&monitor).is_none_or(|at| now.duration_since(*at) >= KEYFRAME_REQUEST_INTERVAL) {
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
                            members.to(from, Update::Notice { text: "Video settings changed too quickly; try again".into() })?;
                            continue;
                        }
                        let mut requested = options.clone();
                        requested.width = width;
                        requested.fps = fps;
                        requested.bitrate = bitrate;
                        let mut info = match desktop(&requested, capture, video_start_group) {
                            Ok(info) => info,
                            Err(error) => {
                                members.to(from, Update::Notice { text: format!("Unsupported video settings: {error}") })?;
                                continue;
                            }
                        };
                        last_configure = Instant::now();
                        // A bitrate-only change retargets the running encoder (in
                        // place, or reopened in the background): no capture restart,
                        // no gap. Auxiliary monitors keep their fixed bitrate.
                        if width == options.width && fps == options.fps && pipeline.set_video_bitrate(members.limit(bitrate)).is_ok() {
                            *options = requested;
                            rates = Rates::new(options, &info);
                            adaptive = !options.fixed_bitrate;
                            last_feedback = Instant::now();
                            members.desktop(&info)?;
                            continue;
                        }
                        capture.release_all().await;
                        while_starting(retire(pipeline), &mut events, capture, &mut members).await?;
                        video_start_group = video_barrier(&video)?;
                        info.video_start_group = video_start_group;
                        capture.select_monitor(capture.active_monitor).await?;
                        pipeline = while_starting(start_encoder(options, capture.pipeline_source_for_range(options.dynamic_range)?, (info.width, info.height), fps, members.limit(bitrate), &video), &mut events, capture, &mut members).await?;
                        session_video(&pipeline, fps, long_gop);
                        *options = requested;
                        let active: Vec<_> = monitor_streams.keys().copied().collect();
                        while_starting(retire(std::mem::take(&mut monitor_streams)), &mut events, capture, &mut members).await?;
                        for index in active {
                            match monitor_encoder(options, capture, index, &monitor_tracks[index], &mut events, &mut members).await {
                                Ok(stream) => {
                                    members.to(monitor_viewer, Update::MonitorStream { index, desktop: stream.desktop.clone() })?;
                                    if long_gop { stream.pipeline.set_key_interval(options.fps.saturating_mul(LONG_KEY_INTERVAL_SECONDS)); }
                                    monitor_streams.insert(index, stream);
                                }
                                Err(error) => members.to(monitor_viewer, Update::MonitorStreamStopped { index, reason: format!("Monitor reconfiguration failed: {error}"), requested: false })?,
                            }
                        }
                        encoder_metrics = pipeline.encoder_metrics()?;
                        rates = Rates::new(options, &info);
                        adaptive = !options.fixed_bitrate;
                        last_feedback = Instant::now();
                        members.desktop(&info)?;
                    }
                    Event::SelectMonitor { index } => {
                        if index >= capture.monitors.len() || last_switch.elapsed() < Duration::from_millis(500) {
                            let desktop = members.customize(from, &desktop(options, capture, video_start_group)?);
                            members.to(from, Update::Desktop { desktop })?;
                            continue;
                        }
                        last_switch = Instant::now();
                        capture.release_all().await;
                        // Stop the old source before opening another PipeWire node.
                        while_starting(retire(pipeline), &mut events, capture, &mut members).await?;
                        video_start_group = video_barrier(&video)?;
                        capture.select_monitor(index).await?;
                        rates.restore(options);
                        if let Some(display) = &client_display {
                            // Fit the new monitor's size and refresh rate.
                            let choice = negotiate_stream(display, capture);
                            (options.width, options.fps, options.bitrate) = (choice.width, choice.fps, choice.bitrate);
                        }
                        let info = desktop(options, capture, video_start_group)?;
                        pipeline = while_starting(start_encoder(options, capture.pipeline_source_for_range(options.dynamic_range)?, (info.width, info.height), options.fps, members.limit(options.bitrate), &video), &mut events, capture, &mut members).await?;
                        session_video(&pipeline, options.fps, long_gop);
                        rates = Rates::new(options, &info);
                        encoder_metrics = pipeline.encoder_metrics()?;
                        adaptive = !options.fixed_bitrate;
                        last_feedback = Instant::now();
                        members.desktop(&info)?;
                    }
                    Event::Feedback { queue_ms, dropped_groups } => {
                        let changed = if members.is_leader(from) {
                            adaptive && last_feedback.elapsed() >= Duration::from_millis(900) && {
                                last_feedback = Instant::now();
                                rates.adaptation.observe(queue_ms, dropped_groups).is_some()
                            }
                        } else {
                            // Its own controller bounds the shared bitrate.
                            adaptive && members.sessions.get_mut(&from).is_some_and(|session| {
                                session.last_feedback.elapsed() >= Duration::from_millis(900) && {
                                    session.last_feedback = Instant::now();
                                    session.rates.as_mut().is_some_and(|rates| rates.adaptation.observe(queue_ms, dropped_groups).is_some())
                                }
                            })
                        };
                        let bitrate = members.limit(rates.target());
                        if changed && bitrate != pipeline.video_bitrate()? {
                            match pipeline.set_video_bitrate(bitrate) {
                                Ok(()) => { tracing::info!(bitrate, queue_ms, dropped_groups, "adapted video bitrate"); },
                                Err(error) => { adaptive = false; tracing::warn!(%error, "encoder does not support live bitrate changes; retaining fixed bitrate"); }
                            }
                        }
                    }
                    Event::NetworkFeedback { delay_ms, received_kbps } => {
                        if adaptive {
                            let sizes = *encoder_metrics.sizes.lock().unwrap();
                            let changed = if members.is_leader(from) {
                                let stats = members.leader_session().map(|session| session.stats());
                                let packets = stats.and_then(|stats| stats.packets_sent.zip(stats.packets_lost));
                                rates.feedback(Instant::now(), delay_ms, received_kbps, sizes, packets).is_some()
                            } else {
                                let packets = members.sessions.get(&from).and_then(|s| s.member.session()).map(|session| session.stats()).and_then(|stats| stats.packets_sent.zip(stats.packets_lost));
                                members.sessions.get_mut(&from).and_then(|s| s.rates.as_mut()).is_some_and(|rates| rates.feedback(Instant::now(), delay_ms, received_kbps, sizes, packets).is_some())
                            };
                            let bitrate = members.limit(rates.target());
                            if changed && bitrate != pipeline.video_bitrate()? {
                                match pipeline.set_video_bitrate(bitrate) {
                                    Ok(()) => tracing::info!(bitrate, ?delay_ms, received_kbps, capacity = ?rates.delay.as_ref().and_then(|rate| rate.capacity()), "adapted video bitrate to network"),
                                    Err(error) => { adaptive = false; tracing::warn!(%error, "encoder does not support live bitrate changes; retaining fixed bitrate"); }
                                }
                            }
                        }
                    }
                    Event::Clipboard { text } if options.clipboard => {
                        let text = match crate::clipboard::write(&text).await { Ok(()) => "Clipboard sent to host".into(), Err(error) => format!("Clipboard unavailable: {error}") };
                        members.to(from, Update::Notice { text })?;
                    }
                    Event::ClipboardRequest if options.clipboard => {
                        let update = match crate::clipboard::read().await { Ok(text) => Update::Clipboard { text }, Err(error) => Update::Notice { text: format!("Clipboard unavailable: {error}") } };
                        members.to(from, update)?;
                    }
                    Event::Clipboard { .. } | Event::ClipboardRequest => members.to(from, Update::Notice { text: "Host clipboard is disabled; start host with --clipboard to opt in".into() })?,
                    // Owners only (screened): locking or unlocking the
                    // host's session is the owner's business.
                    Event::SecureAttention => {
                        let text = secure_attention(options, capture).await;
                        members.to(from, Update::Notice { text })?;
                    }
                    Event::Probe { id } => {
                        telemetry_enabled = true;
                        let host_us = options.frame_timing.then(|| protocol::clock_us(Instant::now()));
                        members.to(from, Update::Pong { id, host_us })?;
                    },
                    Event::TakeControl { take } => {
                        // Owners only (screened).
                        if take {
                            if let Some(previous) = members.arbiter.take(from, Instant::now()) {
                                capture.release_keyboard_mouse().await;
                                members.notice(previous, "The host's owner took keyboard and mouse control");
                            }
                            members.notice(from, "You have keyboard and mouse control; guests cannot take it until you release it");
                        } else {
                            members.arbiter.release();
                            members.notice(from, "Guests may take keyboard and mouse control again");
                        }
                        members.send_access();
                        members.publish(hub);
                    }
                    Event::ApproveGuest { id, allow } => {
                        // Owners only (screened).
                        let text = match hub.answer(id, allow) {
                            Ok(()) => if allow { "Guest approved" } else { "Guest denied" }.to_owned(),
                            Err(error) => format!("{error}"),
                        };
                        tracing::info!(session = from, request = id, allow, "guest request answered by an owner's client");
                        members.notice(from, text);
                    }
                    Event::ReleaseAll => {
                        // What this session holds: keys and buttons if it has
                        // control, and its own pads.
                        if members.arbiter.holder_session() == Some(from) {
                            capture.release_keyboard_mouse().await;
                        }
                        if let Some(session) = members.sessions.get(&from) {
                            capture.neutralise_pads(session.pad_block);
                        }
                    }
                    event @ (Event::GamepadConnected { .. }
                    | Event::GamepadDisconnected { .. }
                    | Event::GamepadButton { .. }
                    | Event::GamepadAxis { .. }
                    | Event::GamepadTouch { .. }
                    | Event::GamepadMotion { .. }) => {
                        let Some((block, role)) = members.sessions.get(&from).map(|s| (s.pad_block, s.role.role)) else { continue };
                        // Guests: Xbox pads, no touchpad pointer.
                        let Some(event) = crate::guests::guest_pad(role, event) else { continue };
                        if let Err(error) = capture.gamepad_input(block, &event) {
                            tracing::warn!(%error, session = from, "dropping gamepad event that could not be injected");
                        }
                    }
                    event => {
                        // Invalid events still fail closed. An injection error
                        // (portal/D-Bus hiccup) drops that one event; only a
                        // persistently broken input path ends the session.
                        if event.validate().is_err() {
                            members.remove(from, "invalid input", None, capture).await;
                            continue;
                        }
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
                                    members.to(from, Update::Notice { text: format!("Host could not inject input: {error}") })?;
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
                // H.265 and AV1 need about two thirds of H.264's bits.
                options.codec != protocol::VideoCodec::H264,
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

/// The session's `Desktop`, with the session-wide offers (microphone, wake,
/// secure attention) that `Members::customize` withholds from guests.
fn desktop(options: &Options, capture: &Capture, video_start_group: u64) -> Result<Desktop> {
    let mut desktop =
        desktop_for_monitor(options, capture, capture.active_monitor, video_start_group)?;
    desktop.microphone = options.microphone_available;
    desktop.wake = options.wake.clone();
    desktop.secure_attention = options.secure_attention;
    Ok(desktop)
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
        // Older clients reject names they do not know.
        hardware_codecs: options
            .hardware_codecs
            .iter()
            .copied()
            .filter(|&codec| codec != protocol::VideoCodec::Av1)
            .collect(),
        av1: offers_av1(options),
        hardware_av1: options.hardware_codecs.contains(&protocol::VideoCodec::Av1),
        chroma_444_selection: offers_format(options, &options.hardware_formats.chroma_444),
        hardware_444: options.hardware_formats.chroma_444.clone(),
        chroma_444: options.chroma_444,
        ten_bit_selection: offers_format(options, &options.hardware_formats.ten_bit),
        hardware_ten_bit: options.hardware_formats.ten_bit.clone(),
        ten_bit: options.ten_bit,
        extra_mouse_buttons: true,
        smooth_scroll: capture.smooth_scroll(),
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
        microphone: false,
        audio_options: true,
        audio_settings: protocol::AudioSettings {
            format: options.audio_format,
            bitrate: options.audio_bitrate,
        },
        // Session-wide offers are in the session's own `Desktop`
        // (`Members::customize`); an auxiliary monitor's, which may go to a
        // guest leading the stream, repeats none of them.
        wake: None,
        secure_attention: false,
        guests: true,
        access: None,
        shared_stream: false,
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
    members: &mut Members,
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
        members,
    )
    .await?;
    Ok(MonitorEncoder {
        pipeline,
        _source: source,
        desktop: info,
    })
}

/// Input events from every session, tagged with the session that sent them,
/// plus control events set aside while an encoder was starting (handled
/// first, in arrival order).
struct SessionEvents {
    receiver: tokio::sync::mpsc::Receiver<(crate::guests::SessionId, Event)>,
    deferred: std::collections::VecDeque<(crate::guests::SessionId, Event)>,
}

impl SessionEvents {
    async fn next(&mut self) -> Option<(crate::guests::SessionId, Event)> {
        match self.deferred.pop_front() {
            Some(tagged) => Some(tagged),
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
        chroma444: options.chroma_444,
        ten_bit: options.ten_bit,
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
/// retries it with its error handling. Input is screened here exactly as in
/// the loop (permissions, keyboard/mouse control).
async fn while_starting<T: Send + 'static>(
    work: tokio::task::JoinHandle<Result<T>>,
    events: &mut SessionEvents,
    capture: &mut Capture,
    members: &mut Members,
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
    let mut ordered = events.deferred.iter().any(|(_, event)| !reorderable(event));
    tokio::pin!(work);
    loop {
        tokio::select! {
            result = &mut work => return result.context("encoder task failed")?,
            tagged = events.receiver.recv(), if !ordered => {
                let Some((from, event)) = tagged else {
                    // The session loop reports the closed stream.
                    return work.await.context("encoder task failed")?;
                };
                match event {
                    // No host clock here; the client's next probe gets one.
                    Event::Probe { id } => members.to(from, Update::Pong { id, host_us: None })?,
                    event if reorderable(&event) => events.deferred.push_back((from, event)),
                    Event::MonitorStream { .. }
                    | Event::MonitorMotion { .. }
                    | Event::ClientFeatures { .. }
                    | Event::ConfigureVideo { .. }
                    | Event::ClientDisplay(_)
                    | Event::SelectCodec { .. }
                    | Event::SelectMonitor { .. }
                    | Event::Clipboard { .. }
                    | Event::ClipboardRequest
                    | Event::SecureAttention
                    | Event::TakeControl { .. }
                    | Event::ApproveGuest { .. } => {
                        // Only the leader's controls hold up everyone's
                        // input (another session cannot freeze it).
                        ordered |= members.is_leader(from);
                        events.deferred.push_back((from, event));
                    }
                    event => {
                        if !members.recheck(from, capture).await {
                            continue;
                        }
                        let release = match members.screen(from, &event, Instant::now()) {
                            Screen::Drop => continue,
                            Screen::Pass { release } => release,
                        };
                        if release {
                            capture.release_keyboard_mouse().await;
                        }
                        let session = members.sessions.get(&from).map(|s| (s.pad_block, s.role.role));
                        let injected = match (&event, session) {
                            (_, None) => continue,
                            (Event::ReleaseAll, Some((block, _))) => {
                                if members.arbiter.holder_session() == Some(from) {
                                    capture.release_keyboard_mouse().await;
                                }
                                capture.neutralise_pads(block);
                                true
                            }
                            (
                                Event::GamepadConnected { .. }
                                | Event::GamepadDisconnected { .. }
                                | Event::GamepadButton { .. }
                                | Event::GamepadAxis { .. }
                                | Event::GamepadTouch { .. }
                                | Event::GamepadMotion { .. },
                                Some((block, role)),
                            ) => crate::guests::guest_pad(role, event.clone())
                                .is_none_or(|event| capture.gamepad_input(block, &event).is_ok()),
                            _ => event.validate().is_ok() && capture.input(event.clone()).await.is_ok(),
                        };
                        if injected {
                            flash_on_input(&event);
                        } else {
                            ordered = true;
                            events.deferred.push_back((from, event));
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
        chroma444: false,
        ten_bit: false,
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
struct AbortOnDrop<T = Result<()>>(tokio::task::JoinHandle<T>);
impl<T> Drop for AbortOnDrop<T> {
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
    id: crate::guests::SessionId,
    controls: moq_net::origin::Producer,
    sender: tokio::sync::mpsc::Sender<(crate::guests::SessionId, Event)>,
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
            let _ = sender.try_send((id, pointer.event));
        }
    }
    Ok(())
}

async fn receive_input(
    id: crate::guests::SessionId,
    mut track: moq_net::track::Subscriber,
    sender: tokio::sync::mpsc::Sender<(crate::guests::SessionId, Event)>,
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
                .send((id, input.event))
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
