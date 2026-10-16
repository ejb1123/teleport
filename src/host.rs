use crate::{
    capture::Capture,
    media,
    protocol::{self, Desktop, Event, Input, Pairing, Update},
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
    /// Experimental: permit clients that explicitly opt in to forward an SSH agent.
    #[arg(long)]
    pub allow_ssh_agent: bool,
    /// Bind address (UDP). Use 0.0.0.0:4443 for LAN access.
    #[arg(long, default_value = "127.0.0.1:4443")]
    pub listen: String,
    #[arg(long, default_value = "auto", value_parser = ["auto", "portal", "x11", "test"])]
    pub source: String,
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
    /// Disable receiver-feedback bitrate adjustment.
    #[arg(long)]
    pub fixed_bitrate: bool,
    /// Explicit PulseAudio/PipeWire output .monitor name; never records a microphone.
    #[arg(long)]
    pub audio_source: Option<String>,
    /// Allow explicit text clipboard send/fetch requests from the paired client.
    #[arg(long)]
    pub clipboard: bool,
    /// Open one-time code enrollment on TCP at the same port for five minutes.
    #[arg(long)]
    pub pair: bool,
    /// Default codec for clients without an explicit selection. Prefer H.264 for compatibility.
    #[arg(long, value_enum, default_value_t = protocol::VideoCodec::H264)]
    pub codec: protocol::VideoCodec,
    #[arg(skip)]
    pub dynamic_range: protocol::DynamicRange,
}

pub async fn run(options: Options) -> Result<()> {
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
    // Create the private directory before a portal restore token inside it is written.
    if let Some(directory) = &options.identity_dir {
        crate::identity::open(directory)?;
    }
    let mut capture = Capture::open_with_options(
        &options.source,
        options.restore_token.as_deref(),
        options.monitor,
    )
    .await?;
    let result = serve(&options, &mut capture).await;
    capture.close().await;
    result
}

async fn serve(options: &Options, capture: &mut Capture) -> Result<()> {
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
    let relay_identity = match (&relay_endpoint, &identity, &access) {
        (Some(_), Some(identity), Some(_)) => Some(Arc::new(identity.e2e()?)),
        (Some(_), _, _) => {
            anyhow::bail!("--relay requires --identity-dir (a persistent host identity)")
        }
        (None, _, _) => None,
    };
    let relay_namespace = crate::relay::namespace(&pairing.fingerprint)?;
    // Connected relay, confirmed sessions from its intake task, and the tasks
    // (session intake, presence, enrollment).
    let mut relay: Option<(
        crate::relay::Link,
        tokio::sync::mpsc::Receiver<crate::relay::Accepted>,
        Vec<crate::relay::AbortOnDrop>,
    )> = None;
    let mut relay_retry = Instant::now();
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
            request = server.accept() => match request { Some(request) => request, None => break },
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
                        restore_capture(capture).await;
                        let result = tokio::select! {
                            result = relayed(link, accepted, access, options, capture) => result,
                            _ = &mut shutdown => { capture.end_session().await; break; }
                        };
                        capture.end_session().await;
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
            _ = &mut shutdown => break,
        };
        let supplied = request
            .path()
            .strip_prefix("/teleport/")
            .unwrap_or("")
            .to_owned();
        let authorized = access.as_ref().map_or_else(
            || bool::from(supplied.as_bytes().ct_eq(pairing.token.as_bytes())),
            |access| access.authorized(&supplied),
        );
        if !authorized {
            let _ = request.close(403).await;
            tracing::warn!("rejected unauthorized client");
            continue;
        }
        let _session_lease = if let Some(access) = &access {
            match access.begin_session(&supplied) {
                Ok(lease) => Some(lease),
                Err(_) => {
                    let _ = request.close(403).await;
                    continue;
                }
            }
        } else {
            None
        };
        restore_capture(capture).await;
        let result = tokio::select! {
            result = connection(request, options, capture) => result,
            _ = async { loop { tokio::time::sleep(Duration::from_secs(1)).await; if access.as_ref().is_some_and(|access| !access.authorized(&supplied)) { break; } } } => Err(anyhow::anyhow!("device access revoked")),
            _ = &mut shutdown => { capture.end_session().await; break; }
        };
        capture.end_session().await;
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

async fn shutdown_signal() -> Result<()> {
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    tokio::select! {
        result = tokio::signal::ctrl_c() => result?,
        _ = terminate.recv() => (),
    }
    Ok(())
}

async fn connection(
    request: moq_native::Request,
    options: &Options,
    capture: &mut Capture,
) -> Result<()> {
    let query = request.query().map(str::to_owned);
    let outgoing = moq_net::Origin::random().produce();
    let incoming = moq_net::Origin::random().produce();
    let session = request
        .with_publisher(&outgoing)
        .with_subscriber(incoming.clone())
        .ok();
    run_session(
        query.as_deref(),
        outgoing,
        incoming,
        async { Ok(session.await?) },
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
    let video = broadcast.create_track(
        options.codec.track(),
        moq_net::track::Info::default().with_latency_max(Duration::from_millis(500)),
    )?;
    let mut monitor_tracks = Vec::new();
    for index in 0..capture.monitors.len().min(64) {
        monitor_tracks.push(broadcast.create_track(
            format!("monitor-{index}-{}", options.codec.track()),
            moq_net::track::Info::default().with_latency_max(Duration::from_millis(500)),
        )?);
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
    let audio_track = if options.audio_source.is_some() {
        Some(broadcast.create_track("opus", None)?)
    } else {
        None
    };
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
    let info = desktop(options, capture, video_start_group)?;
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
    let audio = match (options.audio_source.as_deref(), audio_track) {
        (Some(source), Some(track)) => Some(crate::audio::capture(source, track)?),
        _ => None,
    };
    let mut group = metadata.append_group()?;
    group.write_frame(moq_net::Timestamp::now(), serde_json::to_vec(&info)?)?;
    group.finish()?;
    tracing::info!(
        width = info.width,
        height = info.height,
        fps = options.fps,
        "native client connected"
    );
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
    let mut adaptation = media::BitrateController::new(options.bitrate);
    let mut network_rate = media::NetworkRateController::new(options.bitrate, Instant::now());
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
    // XFixes cursor shapes for a client drawing the pointer locally (X11);
    // dropping it at session end stops the watcher thread.
    let mut cursor_watcher: Option<crate::x11_cursor::Watcher> = None;
    // The client's latest display (size, refresh, choices). Applied once the
    // video-settings rate limit allows, coalescing bursts into the newest.
    let mut client_display: Option<protocol::ClientDisplay> = None;
    let mut display_pending = false;
    loop {
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
            adaptation = media::BitrateController::new(options.bitrate);
            network_rate = media::NetworkRateController::new(options.bitrate, Instant::now());
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
            tracing::info!("desktop portal session recreated");
        }
        tokio::select! {
            result = crate::ssh_agent::closed(&mut ssh_agent) => return result,
            result = &mut incoming.0 => return result.context("input task failed")?,
            error = session.closed() => anyhow::bail!("{error}"),
            shape = async {
                match cursor_watcher.as_mut() {
                    Some(watcher) => watcher.shapes.changed().await,
                    None => std::future::pending().await,
                }
            } => {
                let shape = match (shape, cursor_watcher.as_mut()) {
                    (Ok(()), Some(watcher)) => watcher.shapes.borrow_and_update().clone(),
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
            _ = tokio::time::sleep_until((last_configure + CONFIGURE_INTERVAL).into()), if display_pending => {
                display_pending = false;
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
                if let Err(error) = pipeline.error() {
                    // On Wayland a capture failure is usually the portal stream
                    // going away with its outputs; try a fresh session first.
                    ensure!(capture.is_portal(), error);
                    tracing::warn!(error = format!("{error:#}"), "capture pipeline failed");
                    portal_lost = true;
                    continue;
                }
                let failed: Vec<_> = monitor_streams.iter().filter_map(|(&index, stream)| stream.pipeline.error().err().map(|error| (index, error))).collect();
                for (index, error) in failed {
                    monitor_streams.remove(&index);
                    capture.release_all().await;
                    publish_update(&mut updates, Update::MonitorStreamStopped { index, reason: format!("Monitor stream stopped: {error}"), requested: false })?;
                }
                if let Some(audio) = &audio { audio.error()?; }
                if adaptive {
                    let stats = session.stats();
                    if let (Some(rtt), Some(sent), Some(lost)) = (stats.rtt, stats.packets_sent, stats.packets_lost)
                        && network_rate.observe(Instant::now(), rtt, sent, lost).is_some() {
                        let bitrate = adaptation.current().min(network_rate.current());
                        if bitrate != pipeline.video_bitrate()? {
                            match pipeline.set_video_bitrate(bitrate) {
                                Ok(()) => tracing::info!(bitrate, rtt_ms = rtt.as_millis() as u64, "adapted video bitrate to network"),
                                Err(error) => { adaptive = false; tracing::warn!(%error, "encoder does not support live bitrate changes; retaining fixed bitrate"); }
                            }
                        }
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
                    Event::ClientFeatures { keyframe_requests , pointer_datagrams, local_cursor } => {
                        if pointer_datagrams && pointer_task.is_none() {
                            pointer_task = Some(AbortOnDrop(tokio::spawn(receive_pointer(controls.clone(), pointer_sender.clone(), last_motion.clone()))));
                        }
                        if keyframe_requests && !long_gop {
                            long_gop = true;
                            let frames = options.fps.saturating_mul(LONG_KEY_INTERVAL_SECONDS);
                            pipeline.set_key_interval(frames);
                            for stream in monitor_streams.values() { stream.pipeline.set_key_interval(frames); }
                        }
                        if local_cursor && capture.supports_local_cursor() && cursor_watcher.is_none() {
                            // Watch first: without shapes the client would have no pointer at all.
                            let mut watcher = match crate::x11_cursor::watch() {
                                Ok(watcher) => watcher,
                                Err(error) => {
                                    tracing::warn!(error = format!("{error:#}"), "local cursor unavailable; keeping the pointer in the video");
                                    continue;
                                }
                            };
                            // Restart capture without the pointer, as for a monitor switch.
                            capture.release_all().await;
                            while_starting(retire(pipeline), &mut events, capture, &mut updates).await?;
                            capture.local_cursor = true;
                            video_start_group = video_barrier(&video)?;
                            let info = desktop(options, capture, video_start_group)?;
                            pipeline = while_starting(start_encoder(options, capture.pipeline_source_for_range(options.dynamic_range)?, (info.width, info.height), options.fps, options.bitrate, &video), &mut events, capture, &mut updates).await?;
                            if long_gop { pipeline.set_key_interval(options.fps.saturating_mul(LONG_KEY_INTERVAL_SECONDS)); }
                            let active: Vec<_> = monitor_streams.keys().copied().collect();
                            while_starting(retire(std::mem::take(&mut monitor_streams)), &mut events, capture, &mut updates).await?;
                            for index in active {
                                match monitor_encoder(options, capture, index, &monitor_tracks[index], &mut events, &mut updates).await {
                                    Ok(stream) => {
                                        publish_update(&mut updates, Update::MonitorStream { index, desktop: stream.desktop.clone() })?;
                                        if long_gop { stream.pipeline.set_key_interval(options.fps.saturating_mul(LONG_KEY_INTERVAL_SECONDS)); }
                                        monitor_streams.insert(index, stream);
                                    }
                                    Err(error) => publish_update(&mut updates, Update::MonitorStreamStopped { index, reason: format!("Monitor restart failed: {error}"), requested: false })?,
                                }
                            }
                            adaptation = media::BitrateController::new(options.bitrate);
                            network_rate = media::NetworkRateController::new(options.bitrate, Instant::now());
                            encoder_metrics = pipeline.encoder_metrics()?;
                            adaptive = !options.fixed_bitrate;
                            last_feedback = Instant::now();
                            publish_update(&mut updates, Update::Desktop { desktop: info })?;
                            if let Some(cursor) = watcher.shapes.borrow_and_update().clone() {
                                publish_update(&mut updates, Update::Cursor { cursor })?;
                            }
                            cursor_watcher = Some(watcher);
                            tracing::info!("client draws the cursor locally");
                        }
                    }
                    Event::ClientDisplay(display) => {
                        client_display = Some(display);
                        display_pending = true;
                    }
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
                            adaptation = media::BitrateController::new(bitrate);
                            network_rate = media::NetworkRateController::new(bitrate, Instant::now());
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
                        if long_gop { pipeline.set_key_interval(fps.saturating_mul(LONG_KEY_INTERVAL_SECONDS)); }
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
                        adaptation = media::BitrateController::new(bitrate);
                        network_rate = media::NetworkRateController::new(bitrate, Instant::now());
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
                        if let Some(display) = &client_display {
                            // Fit the new monitor's size and refresh rate.
                            let choice = negotiate_stream(display, capture);
                            (options.width, options.fps, options.bitrate) = (choice.width, choice.fps, choice.bitrate);
                        }
                        let info = desktop(options, capture, video_start_group)?;
                        pipeline = while_starting(start_encoder(options, capture.pipeline_source_for_range(options.dynamic_range)?, (info.width, info.height), options.fps, options.bitrate, &video), &mut events, capture, &mut updates).await?;
                        if long_gop { pipeline.set_key_interval(options.fps.saturating_mul(LONG_KEY_INTERVAL_SECONDS)); }
                        adaptation = media::BitrateController::new(options.bitrate);
                        network_rate = media::NetworkRateController::new(options.bitrate, Instant::now());
                        encoder_metrics = pipeline.encoder_metrics()?;
                        adaptive = !options.fixed_bitrate;
                        last_feedback = Instant::now();
                        publish_update(&mut updates, Update::Desktop { desktop: info })?;
                    }
                    Event::Feedback { queue_ms, dropped_groups } => {
                        if adaptive && last_feedback.elapsed() >= Duration::from_millis(900) {
                            last_feedback = Instant::now();
                            if adaptation.observe(queue_ms, dropped_groups).is_some()
                                && adaptation.current().min(network_rate.current()) != pipeline.video_bitrate()? {
                                let bitrate = adaptation.current().min(network_rate.current());
                                match pipeline.set_video_bitrate(bitrate) {
                                    Ok(()) => { tracing::info!(bitrate, queue_ms, dropped_groups, "adapted video bitrate"); },
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
                    Event::Probe { id } => {
                        telemetry_enabled = true;
                        publish_update(&mut updates, Update::Pong { id })?;
                    },
                    event => {
                        // Invalid events still fail closed. An injection error
                        // (portal/D-Bus hiccup) drops that one event; only a
                        // persistently broken input path ends the session.
                        event.validate()?;
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
        max_fps: protocol::MAX_FPS,
        display_negotiation: true,
    })
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
    let (track, kind) = (track.clone(), options.encoder);
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
            Event::Feedback { .. } | Event::MonitorFeedback { .. } | Event::RequestKeyframe { .. }
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
                    Event::Probe { id } => publish_update(updates, Update::Pong { id })?,
                    event if reorderable(&event) => events.deferred.push_back(event),
                    Event::MonitorStream { .. }
                    | Event::MonitorMotion { .. }
                    | Event::ClientFeatures { .. }
                    | Event::ConfigureVideo { .. }
                    | Event::ClientDisplay(_)
                    | Event::SelectMonitor { .. }
                    | Event::Clipboard { .. }
                    | Event::ClipboardRequest => {
                        ordered = true;
                        events.deferred.push_back(event);
                    }
                    event => {
                        if event.validate().is_err() || capture.input(event.clone()).await.is_err() {
                            ordered = true;
                            events.deferred.push_back(event);
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
