use crate::{
    media,
    protocol::{self, Desktop, Event, Input, Pairing, Update},
};
use anyhow::{Context, Result, ensure};
use clap::Args;
use sdl2::{
    event::{Event as SdlEvent, WindowEvent},
    keyboard::{Keycode, Mod},
    mouse::MouseButton,
    pixels::PixelFormatEnum,
    rect::Rect,
};
use std::{
    path::PathBuf,
    sync::atomic::{AtomicBool, AtomicU32, Ordering},
    time::{Duration, Instant},
};
use tokio::{runtime::Runtime, sync::mpsc};

#[path = "controllers.rs"]
mod controllers;
#[path = "multi_display.rs"]
mod multi_display;

#[derive(Args, Clone)]
pub struct Options {
    /// Experimental: allow this host to request SSH authentication signatures from SSH_AUTH_SOCK.
    #[arg(long, conflicts_with = "reconnect")]
    pub forward_ssh_agent: bool,
    /// Host name or IP and UDP port, e.g. 192.168.1.20:4443.
    pub address: String,
    /// Private pairing file copied from the host.
    #[arg(long)]
    pub pairing_file: PathBuf,
    /// Decode this many frames without opening a window (smoke testing).
    #[arg(long, hide = true)]
    pub headless_frames: Option<u32>,
    /// Use SDL software rendering when a GPU renderer is unavailable.
    #[arg(long)]
    pub software_renderer: bool,
    /// Force software video decoding (useful when diagnosing hardware decoders).
    #[arg(long)]
    pub software_decoder: bool,
    /// Close the native window after displaying N frames (diagnostics).
    #[arg(long)]
    pub exit_after_frames: Option<u32>,
    #[arg(long, hide = true, requires = "headless_frames")]
    pub smoke_input: bool,
    #[arg(long, hide = true, requires = "headless_frames")]
    pub smoke_switch_monitor: bool,
    #[arg(long, hide = true, requires = "headless_frames")]
    pub smoke_audio: bool,
    #[arg(long, hide = true, requires = "headless_frames")]
    pub smoke_clipboard: bool,
    /// Stream pointer motion at a gaming poll rate while timing probe round
    /// trips on the input path (network-condition tests).
    #[arg(long, hide = true, requires = "headless_frames")]
    pub smoke_input_latency: bool,
    /// Send pointer motion on the ordered input track even when the host
    /// accepts datagrams (comparison and troubleshooting).
    #[arg(long, hide = true)]
    pub reliable_pointer: bool,
    /// Keep the host pointer drawn into the video instead of drawing it
    /// locally from cursor shapes (hosts that support it: X11).
    #[arg(long)]
    pub embedded_cursor: bool,
    /// Reach the host through this MoQ relay with end-to-end encryption
    /// (e.g. moqt://relay.example:4444). The host must run with the same --relay.
    /// The environment form keeps a provider token out of the process list.
    #[arg(long, env = "TELEPORT_RELAY_URL", hide_env_values = true)]
    pub relay: Option<url::Url>,
    /// How to reach the host: `direct`, `relay`, or `auto` (direct with a
    /// short timeout, then the relay). Default: `relay` with --relay, else
    /// `direct`. Reconnects choose again.
    #[arg(long, value_enum)]
    pub route: Option<crate::relay::Route>,
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
    /// Retry dropped sessions until the connection window is closed.
    #[arg(long)]
    pub reconnect: bool,
    /// Permit explicit text clipboard transfers using the toolbar.
    #[arg(long)]
    pub clipboard: bool,
    /// Start with remote audio muted.
    #[arg(long)]
    pub mute: bool,
    /// Select this monitor on connection (zero-based).
    #[arg(long)]
    pub monitor: Option<usize>,
    /// Requested stream width; 0 uses the monitor's native resolution.
    #[arg(long)]
    pub width: Option<u32>,
    /// Requested stream frame rate.
    #[arg(long)]
    pub fps: Option<u32>,
    /// Requested video bitrate in kbit/s.
    #[arg(long)]
    pub bitrate: Option<u32>,
    /// Show the streaming performance overlay immediately (F8 toggles it).
    #[arg(long)]
    pub stats: bool,
    /// Session video codec. H.265 requires support on both host and client.
    #[arg(long, value_enum, default_value = "h264")]
    pub codec: protocol::VideoCodec,
    /// Explicit experimental HDR10 requires patched capture and a macOS HDR display.
    #[arg(long, value_enum, default_value = "sdr")]
    pub dynamic_range: protocol::DynamicRange,
}

/// How long `--route auto` waits for the direct QUIC handshake before it
/// tries the relay.
const DIRECT_ATTEMPT: Duration = Duration::from_millis(2500);
/// Overall bound for one connection attempt on a single path.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);

/// The path a session actually took.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SessionPath {
    Direct,
    Relay,
}

impl SessionPath {
    fn as_str(self) -> &'static str {
        match self {
            Self::Direct => "direct",
            Self::Relay => "relay",
        }
    }

    fn description(self) -> &'static str {
        match self {
            Self::Direct => "direct (pinned TLS)",
            Self::Relay => "relay (end-to-end encrypted)",
        }
    }
}

/// The direct path could not be established at the transport level (no
/// answer, refused, or a different certificate), so nothing reached a host
/// that accepted our credential and the relay may be tried instead.
#[derive(Debug)]
struct DirectUnreachable;

impl std::fmt::Display for DirectUnreachable {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("direct path unavailable")
    }
}

impl Options {
    fn effective_route(&self) -> Result<crate::relay::Route> {
        use crate::relay::Route;
        let route = self.route.unwrap_or(if self.relay.is_some() {
            Route::Relay
        } else {
            Route::Direct
        });
        ensure!(
            route == Route::Direct || self.relay.is_some(),
            "--route {} needs --relay",
            route.as_str()
        );
        Ok(route)
    }

    /// Bound for a whole connection attempt, including a failed direct try.
    fn connect_timeout(&self) -> Duration {
        match self.effective_route() {
            Ok(crate::relay::Route::Auto) => CONNECT_TIMEOUT + DIRECT_ATTEMPT,
            _ => CONNECT_TIMEOUT,
        }
    }
}

struct Link {
    path: SessionPath,
    /// Relay handshake and bridges; dropping it ends a relayed session.
    _relay: Option<crate::relay::ClientSession>,
    remote: moq_net::broadcast::Consumer,
    ssh_agent: Option<crate::ssh_agent::Task>,
    session: moq_net::Session,
    _origin: moq_net::origin::Producer,
    _broadcast: moq_net::broadcast::Producer,
    input: moq_net::track::Producer,
    pointer: moq_net::track::Producer,
    video: moq_net::track::Subscriber,
    desktop: Desktop,
    updates: moq_net::track::Subscriber,
    audio: Option<moq_net::track::Subscriber>,
}

/// Connect along the configured route. `auto` tries the direct address for
/// [`DIRECT_ATTEMPT`], then the relay; it only falls back when the direct
/// transport itself failed, never after a host answered. Both paths
/// authenticate the host by its pinned certificate; the relay path always
/// adds end-to-end encryption.
async fn connect(options: &Options) -> Result<Link> {
    let link = match options.effective_route()? {
        crate::relay::Route::Direct => connect_via(options, SessionPath::Direct, None).await?,
        crate::relay::Route::Relay => connect_via(options, SessionPath::Relay, None).await?,
        crate::relay::Route::Auto => {
            match connect_via(options, SessionPath::Direct, Some(DIRECT_ATTEMPT)).await {
                Ok(link) => link,
                Err(error) if error.downcast_ref::<DirectUnreachable>().is_some() => {
                    tracing::info!(
                        error = format!("{error:#}"),
                        "direct path unavailable; trying the relay"
                    );
                    connect_via(options, SessionPath::Relay, None)
                        .await
                        .context("direct path unavailable and relay failed")?
                }
                Err(error) => return Err(error),
            }
        }
    };
    tracing::info!(
        path = link.path.as_str(),
        "connection path chosen: {}",
        link.path.description()
    );
    Ok(link)
}

async fn connect_via(
    options: &Options,
    path: SessionPath,
    direct_timeout: Option<Duration>,
) -> Result<Link> {
    let agent_path = options
        .forward_ssh_agent
        .then(crate::ssh_agent::agent_path)
        .transpose()?;
    let pairing = Pairing::read(&options.pairing_file)?;
    ensure!(
        !options.address.contains('/') && !options.address.contains('@'),
        "use host:port, not a URL"
    );
    let mut url: url::Url =
        format!("moqt://{}/teleport/{}", options.address, pairing.token).parse()?;
    url.query_pairs_mut()
        .append_pair("codec", options.codec.track());
    if options.dynamic_range == protocol::DynamicRange::Hdr10 {
        url.query_pairs_mut().append_pair("range", "hdr10");
    }
    if options.forward_ssh_agent {
        url.query_pairs_mut().append_pair("ssh-agent", "1");
    }
    let mut config = moq_native::ClientConfig::default();
    config.version = vec![protocol::WIRE_VERSION.parse().map_err(anyhow::Error::msg)?];
    config.quic.idle_timeout = Some(protocol::QUIC_IDLE_TIMEOUT);
    config.quic.keep_alive = Some(protocol::QUIC_KEEP_ALIVE);
    config.quic.mtu_discovery = Some(protocol::QUIC_MTU_DISCOVERY);
    // Relay mode: authenticate the host end to end, then let the unchanged
    // session code below use plaintext origins that the bridge seals.
    let relay = match (&options.relay, path) {
        (Some(relay), SessionPath::Relay) => Some(
            crate::relay::client(
                &crate::relay::Endpoint {
                    url: relay.clone(),
                    fingerprint: options.relay_fingerprint.clone(),
                    key: options.relay_key.clone(),
                },
                &pairing,
                url.query().unwrap_or_default(),
            )
            .await?,
        ),
        (None, SessionPath::Relay) => anyhow::bail!("the relay path needs --relay"),
        (_, SessionPath::Direct) => None,
    };
    config.tls.fingerprint = vec![pairing.fingerprint];
    let (outgoing, incoming) = match &relay {
        Some(relay) => (relay.outgoing.clone(), relay.incoming.clone()),
        None => (
            moq_net::Origin::random().produce(),
            moq_net::Origin::random().produce(),
        ),
    };
    let mut broadcast = outgoing.create_broadcast(
        "controls",
        moq_net::broadcast::Route::new().with_announce(true),
    )?;
    let input = broadcast.create_track(
        "input",
        moq_net::track::Info::default()
            .with_ordered(true)
            .with_priority(255)
            .with_latency_max(protocol::INPUT_TIMEOUT),
    )?;
    // Always published; a host only subscribes after ClientFeatures declares it.
    let pointer = broadcast.create_track(
        protocol::POINTER_TRACK,
        moq_net::track::Info::default().with_priority(255),
    )?;
    let agent_responses = if options.forward_ssh_agent {
        Some(crate::ssh_agent::create_track(
            &mut broadcast,
            crate::ssh_agent::RESPONSE_TRACK,
        )?)
    } else {
        None
    };
    let session = match &relay {
        Some(relay) => relay.link.session.clone(),
        None => {
            let client = config
                .init()?
                .with_publisher(&outgoing)
                .with_subscriber(incoming.clone());
            tracing::info!(address = %options.address, "connecting with pinned TLS certificate");
            let attempt = client.connect(url);
            let result = match direct_timeout {
                Some(limit) => match tokio::time::timeout(limit, attempt).await {
                    Ok(result) => result,
                    Err(_) => {
                        return Err(anyhow::anyhow!("no answer within {limit:?}")
                            .context(DirectUnreachable));
                    }
                },
                None => attempt.await,
            };
            match result {
                Ok(session) => session,
                Err(error) if direct_timeout.is_some() => {
                    return Err(anyhow::Error::from(error).context(DirectUnreachable));
                }
                Err(error) => {
                    return Err(anyhow::Error::from(error).context(
                        "QUIC connection failed; check address, pairing file and UDP firewall",
                    ));
                }
            }
        }
    };
    let incoming_broadcasts = incoming.consume();
    let remote = tokio::select! {
        result = incoming_broadcasts.announced_broadcast("desktop") => result.context("host did not announce desktop")?,
        error = session.closed() => anyhow::bail!("host rejected or closed this session: {error}; check saved credentials and requested features{}", if options.forward_ssh_agent { "; SSH forwarding requires a current host with --allow-ssh-agent" } else { "" }),
    };
    let desktop: Desktop = tokio::select! {
        result = async {
            let mut metadata = remote.track("desktop")?.subscribe(None).await?;
            let mut group = metadata.recv_group().await?.context("desktop metadata missing")?;
            Ok::<_, anyhow::Error>(serde_json::from_slice(&protocol::read_frame(&mut group, protocol::MAX_CONTROL).await?.context("empty desktop metadata")?)?)
        } => result?,
        error = session.closed() => anyhow::bail!("host closed the session before desktop negotiation: {error}"),
    };
    ensure!(
        desktop.version == protocol::VERSION,
        "incompatible Teleport version"
    );
    ensure!(
        !options.forward_ssh_agent || desktop.ssh_agent,
        "host does not support or allow SSH agent forwarding; update it and explicitly enable --allow-ssh-agent"
    );
    let ssh_agent = match (agent_path, agent_responses) {
        (Some(path), Some(responses)) => {
            let requests = tokio::time::timeout(
                Duration::from_secs(10),
                remote
                    .track(crate::ssh_agent::REQUEST_TRACK)?
                    .subscribe(crate::ssh_agent::subscription()),
            )
            .await
            .context("host SSH forwarding track missing")??;
            tracing::warn!(
                "SSH agent forwarding enabled: remote host may request SSH authentication signatures; disconnect to revoke"
            );
            Some(crate::ssh_agent::client(path, requests, responses))
        }
        _ => None,
    };
    protocol::validate_video_size(desktop.width, desktop.height)?;
    ensure!(
        desktop.dynamic_range == options.dynamic_range,
        "host did not negotiate requested dynamic range; HDR is never inferred from an SDR stream"
    );
    ensure!(
        desktop.codec == options.codec,
        "host did not negotiate {}; update the host or select H.264",
        options.codec.label()
    );
    let video = remote
        .track(desktop.codec.track())?
        .subscribe(media::video_subscription())
        .await?;
    let updates = remote
        .track("updates")?
        .subscribe(
            moq_net::track::Subscription::default()
                .with_ordered(true)
                .with_group_start(0)
                .with_priority(media::UPDATES_PRIORITY),
        )
        .await?;
    let audio = if desktop.audio {
        Some(
            remote
                .track("opus")?
                .subscribe(media::audio_subscription())
                .await?,
        )
    } else {
        None
    };
    Ok(Link {
        path,
        _relay: relay,
        remote,
        ssh_agent,
        session,
        _origin: outgoing,
        _broadcast: broadcast,
        input,
        pointer,
        video,
        desktop,
        updates,
        audio,
    })
}

async fn send_input(
    mut track: moq_net::track::Producer,
    mut pointer: moq_net::track::Producer,
    pointer_datagrams: bool,
    mut events: mpsc::Receiver<Event>,
) -> Result<()> {
    let mut sequence = 0u64;
    let mut group = track.append_group()?;
    let mut heartbeat = tokio::time::interval(Duration::from_millis(500));
    let mut motion_id = 0u64;
    // Latest datagram motion not yet repeated on the ordered track.
    let mut unconfirmed: Option<(u64, Event)> = None;
    let mut write = |group: &mut moq_net::group::Producer,
                     sequence: &mut u64,
                     event: Event,
                     motion: Option<u64>|
     -> Result<()> {
        if *sequence > 0 && sequence.is_multiple_of(256) {
            group.finish()?;
            *group = track.append_group()?;
        }
        group.write_frame(
            moq_net::Timestamp::now(),
            serde_json::to_vec(&Input {
                sequence: *sequence,
                event,
                motion,
            })?,
        )?;
        *sequence += 1;
        Ok(())
    };
    loop {
        let event = tokio::select! {
            event = events.recv() => match event { Some(event) => event, None => break },
            _ = heartbeat.tick() => Event::Ping,
        };
        if !pointer_datagrams {
            write(&mut group, &mut sequence, event, None)?;
            continue;
        }
        match event {
            Event::Motion { .. } | Event::MonitorMotion { .. } => {
                // Latest-wins and never retransmitted: a lost motion packet
                // cannot hold up keys queued behind it on the ordered track.
                motion_id += 1;
                pointer.append_datagram(
                    moq_net::Timestamp::now(),
                    serde_json::to_vec(&protocol::PointerInput {
                        motion: motion_id,
                        event: event.clone(),
                    })?,
                )?;
                unconfirmed = Some((motion_id, event));
            }
            Event::Button { .. } | Event::Scroll { .. } | Event::RelativeMotion { .. } => {
                // Clicks and wheel steps land where the pointer was: repeat the
                // latest position reliably, in order, right before them.
                if let Some((id, motion)) = unconfirmed.take() {
                    write(&mut group, &mut sequence, motion, Some(id))?;
                }
                write(&mut group, &mut sequence, event, None)?;
            }
            event => write(&mut group, &mut sequence, event, None)?,
        }
    }
    group.finish()?;
    Ok(())
}

struct Network(tokio::task::JoinHandle<Result<()>>);
impl Drop for Network {
    fn drop(&mut self) {
        self.0.abort();
    }
}

pub fn run(options: Options, runtime: &Runtime) -> Result<()> {
    // Hardware probing can take several seconds. Do it before opening the remote
    // session, whose safety watchdog requires an input heartbeat every 3 seconds.
    ensure!(
        options.dynamic_range != protocol::DynamicRange::Hdr10
            || options.codec == protocol::VideoCodec::H265,
        "HDR10 requires --codec h265"
    );
    ensure!(
        options.dynamic_range == protocol::DynamicRange::Sdr
            || options.headless_frames.is_some()
            || cfg!(target_os = "macos"),
        "HDR presentation currently requires macOS; select an SDR stream on Linux"
    );
    options.effective_route()?;
    media::prepare_decoder(
        options.software_decoder,
        media::VideoFormat {
            codec: options.codec,
            dynamic_range: options.dynamic_range,
        },
    )?;
    let mut last_error = None;
    loop {
        let link = if options.headless_frames.is_some() {
            Some(
                runtime
                    .block_on(async {
                        tokio::time::timeout(options.connect_timeout(), connect(&options)).await
                    })
                    .context("connection timed out")??,
            )
        } else {
            connect_window(&options, runtime, last_error.as_deref())?
        };
        let Some(link) = link else {
            return Ok(());
        };
        match run_session(&options, runtime, link) {
            Ok(false) => return Ok(()),
            Ok(true) if options.forward_ssh_agent => anyhow::bail!(
                "SSH forwarding was revoked; start a new client with --forward-ssh-agent to explicitly enable it again"
            ),
            Ok(true) => last_error = Some("Reconnecting at your request".into()),
            Err(error) if options.reconnect && options.headless_frames.is_none() => {
                tracing::warn!(%error, "session lost; reconnecting");
                last_error = Some(format!("{error:#}"));
            }
            Err(error) => return Err(error),
        }
    }
}

fn connect_window(
    options: &Options,
    runtime: &Runtime,
    previous: Option<&str>,
) -> Result<Option<Link>> {
    let (sdl, video) = crate::windowing::init()?;
    let ttf = sdl2::ttf::init().map_err(anyhow::Error::msg)?;
    let font = ttf
        .load_font(crate::launcher::font_path()?, 18)
        .map_err(anyhow::Error::msg)?;
    let window = video
        .window("Teleport - Connecting", 760, 275)
        .position_centered()
        .build()?;
    let mut canvas = crate::windowing::software(window)?;
    let creator = canvas.texture_creator();
    let mut pump = sdl.event_pump().map_err(anyhow::Error::msg)?;
    let mut status = previous
        .unwrap_or("Connecting securely to your host…")
        .to_owned();
    let mut retry_at = Instant::now();
    let mut failures = 0_u32;
    let mut task: Option<tokio::task::JoinHandle<Result<Link>>> = None;
    loop {
        for event in pump.poll_iter() {
            if let SdlEvent::MouseButtonDown {
                mouse_btn: MouseButton::Left,
                x,
                y,
                ..
            } = event
            {
                if Rect::new(18, 205, 150, 42).contains_point((x, y)) {
                    if let Some(task) = task.take() {
                        task.abort();
                    }
                    retry_at = Instant::now();
                    status = "Retrying connection…".into();
                }
                if Rect::new(188, 205, 150, 42).contains_point((x, y)) {
                    if let Some(task) = task.take() {
                        task.abort();
                    }
                    return Ok(None);
                }
            }
            if matches!(
                event,
                SdlEvent::Quit { .. }
                    | SdlEvent::KeyDown {
                        keycode: Some(Keycode::Escape),
                        ..
                    }
            ) {
                if let Some(task) = task {
                    task.abort();
                }
                return Ok(None);
            }
        }
        if task.is_none() && Instant::now() >= retry_at {
            let options = options.clone();
            task = Some(runtime.spawn(async move {
                tokio::time::timeout(options.connect_timeout(), connect(&options))
                    .await
                    .context("connection timed out; check UDP 4443 and host address")?
            }));
        }
        if task.as_ref().is_some_and(|t| t.is_finished()) {
            match runtime.block_on(task.take().unwrap())? {
                Ok(link) => return Ok(Some(link)),
                Err(error) => {
                    status = format!("{error:#}");
                    tracing::warn!(%error, "connection failed");
                    if options.reconnect {
                        // A brief network blip should cost well under a second;
                        // a host that is down is polled every 3 s at most.
                        let delay = Duration::from_millis(250)
                            .saturating_mul(1 << failures.min(4))
                            .min(Duration::from_secs(3));
                        failures += 1;
                        retry_at = Instant::now() + delay;
                    } else {
                        retry_at = Instant::now() + Duration::from_secs(86400);
                    }
                }
            }
        }
        canvas.set_draw_color(sdl2::pixels::Color::RGB(18, 23, 32));
        canvas.clear();
        for (row, text) in [
            match options.effective_route() {
                Ok(crate::relay::Route::Auto) => {
                    format!("Connecting to {} (direct, then relay)", options.address)
                }
                Ok(crate::relay::Route::Relay) => {
                    format!("Connecting to {} through the relay", options.address)
                }
                _ => format!("Connecting to {}", options.address),
            },
            status.chars().take(85).collect(),
            if options.reconnect {
                "Retries automatically. Close this window or press Esc to cancel.".into()
            } else {
                "Close this window to return. Use --reconnect for automatic retries.".into()
            },
        ]
        .iter()
        .enumerate()
        {
            let surface = font
                .render(text)
                .blended(sdl2::pixels::Color::RGB(225, 232, 245))?;
            let texture = creator.create_texture_from_surface(&surface)?;
            canvas
                .copy(
                    &texture,
                    None,
                    Rect::new(
                        18,
                        30 + row as i32 * 48,
                        surface.width().min(724),
                        surface.height(),
                    ),
                )
                .map_err(anyhow::Error::msg)?;
        }
        for (x, label) in [(18, "Retry now"), (188, "Cancel")] {
            canvas.set_draw_color(sdl2::pixels::Color::RGB(38, 61, 87));
            canvas
                .fill_rect(Rect::new(x, 205, 150, 42))
                .map_err(anyhow::Error::msg)?;
            let surface = font
                .render(label)
                .blended(sdl2::pixels::Color::RGB(230, 237, 248))?;
            let texture = creator.create_texture_from_surface(&surface)?;
            canvas
                .copy(
                    &texture,
                    None,
                    Rect::new(x + 12, 215, surface.width(), surface.height()),
                )
                .map_err(anyhow::Error::msg)?;
        }
        canvas.present();
        std::thread::sleep(Duration::from_millis(30));
    }
}

async fn receive_updates(
    mut track: moq_net::track::Subscriber,
    sender: mpsc::Sender<Update>,
) -> Result<()> {
    let mut sequence = 0;
    let mut pending = std::collections::BTreeMap::new();
    loop {
        let mut group = protocol::ordered_group(&mut track, &mut pending, sequence).await?;
        sequence += 1;
        let bytes = tokio::time::timeout(
            Duration::from_secs(3),
            protocol::read_frame(&mut group, protocol::MAX_CONTROL),
        )
        .await
        .context("host update timed out")??
        .context("empty host update")?;
        let update: Update = serde_json::from_slice(&bytes)?;
        update.validate()?;
        sender.send(update).await.context("session window closed")?;
        media::wake_presenter();
    }
}

type SessionWindow = (
    sdl2::Sdl,
    sdl2::VideoSubsystem,
    sdl2::render::Canvas<sdl2::video::Window>,
);

fn session_window(options: &Options) -> Result<SessionWindow> {
    let (sdl, video) = crate::windowing::init()?;
    let make_window = || -> Result<sdl2::video::Window> {
        let mut window = video
            .window("Teleport - Remote Desktop", 1280, 720)
            .position_centered()
            .resizable()
            .allow_highdpi()
            .build()?;
        window
            .set_minimum_size(700, 240)
            .map_err(anyhow::Error::msg)?;
        Ok(window)
    };
    let canvas = if options.software_renderer {
        crate::windowing::software(make_window()?)?
    } else {
        match make_window()?.into_canvas().accelerated().build() {
            Ok(canvas) => canvas,
            Err(error) => {
                tracing::warn!(%error, "Accelerated window renderer unavailable; trying software presentation");
                crate::windowing::software(make_window()?)?
            }
        }
    };
    tracing::info!(
        backend = video.current_video_driver(),
        renderer = canvas.info().name,
        accelerated = renderer_accelerated(&canvas),
        "session window renderer"
    );
    Ok((sdl, video, canvas))
}

fn renderer_accelerated(canvas: &sdl2::render::Canvas<sdl2::video::Window>) -> bool {
    canvas.info().flags & sdl2::sys::SDL_RendererFlags::SDL_RENDERER_ACCELERATED as u32 != 0
}

fn graphics_renderer(video: &sdl2::VideoSubsystem) -> Option<String> {
    // ACCELERATED describes the renderer API, not the physical GPU. Never take
    // ownership of this context: SDL's renderer owns and destroys it.
    #[cfg(target_os = "linux")]
    unsafe {
        if sdl2::sys::SDL_GL_GetCurrentContext().is_null() {
            return None;
        }
        let address = video.gl_get_proc_address("glGetString");
        if address.is_null() {
            return None;
        }
        let get_string: unsafe extern "system" fn(u32) -> *const u8 = std::mem::transmute(address);
        let renderer = get_string(0x1F01); // GL_RENDERER with SDL's current context.
        if renderer.is_null() {
            return None;
        }
        Some(
            std::ffi::CStr::from_ptr(renderer.cast())
                .to_string_lossy()
                .into_owned(),
        )
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = video;
        None
    }
}

fn supports_nv12(canvas: &sdl2::render::Canvas<sdl2::video::Window>) -> bool {
    if !renderer_accelerated(canvas)
        || !canvas
            .info()
            .texture_formats
            .contains(&PixelFormatEnum::NV12)
    {
        return false;
    }
    set_nv12_conversion();
    let creator = canvas.texture_creator();
    let probe = (|| -> Result<()> {
        let mut texture = creator.create_texture_streaming(PixelFormatEnum::NV12, 16, 16)?;
        update_nv12(
            &mut texture,
            &media::Nv12Image {
                y: vec![16; 256],
                uv: vec![128; 128],
                y_stride: 16,
                uv_stride: 16,
            },
            16,
            16,
        )
    })();
    if let Err(error) = probe {
        tracing::warn!(%error, "NV12 texture upload unavailable; retaining RGB presentation");
        false
    } else {
        true
    }
}

fn set_nv12_conversion() {
    // SDL2-compat can attach colorspace at texture creation, so set this before
    // creating the texture as well as before uploading it.
    unsafe {
        sdl2::sys::SDL_SetYUVConversionMode(
            sdl2::sys::SDL_YUV_CONVERSION_MODE::SDL_YUV_CONVERSION_BT709,
        );
    }
}

fn update_nv12(
    texture: &mut sdl2::render::Texture<'_>,
    frame: &media::Nv12Image,
    width: u32,
    height: u32,
) -> Result<()> {
    protocol::validate_video_size(width, height)?;
    ensure!(
        width.is_multiple_of(2) && height.is_multiple_of(2),
        "NV12 requires even dimensions"
    );
    for (plane, stride, rows) in [
        (&frame.y, frame.y_stride, height as usize),
        (&frame.uv, frame.uv_stride, height as usize / 2),
    ] {
        let required = stride
            .checked_mul(rows - 1)
            .and_then(|size| size.checked_add(width as usize))
            .context("NV12 plane size overflow")?;
        ensure!(
            stride >= width as usize && stride <= i32::MAX as usize && plane.len() >= required,
            "invalid NV12 plane layout"
        );
    }
    let query = texture.query();
    ensure!(
        query.format == PixelFormatEnum::NV12 && query.width == width && query.height == height,
        "NV12 texture does not match the frame"
    );
    // Decoder output explicitly negotiates limited-range BT.709. SDL's AUTO
    // heuristic chooses by dimensions, which is wrong for resized HD sources.
    // Safety: the live texture and both checked, stride-aware planes outlive
    // this synchronous upload; SDL receives no pointers to temporary storage.
    let result = unsafe {
        sdl2::sys::SDL_SetYUVConversionMode(
            sdl2::sys::SDL_YUV_CONVERSION_MODE::SDL_YUV_CONVERSION_BT709,
        );
        sdl2::sys::SDL_UpdateNVTexture(
            texture.raw(),
            std::ptr::null(),
            frame.y.as_ptr(),
            frame.y_stride as i32,
            frame.uv.as_ptr(),
            frame.uv_stride as i32,
        )
    };
    ensure!(result == 0, "NV12 upload failed: {}", sdl2::get_error());
    Ok(())
}

fn run_session(options: &Options, runtime: &Runtime, link: Link) -> Result<bool> {
    let session_path = link.path;
    tracing::info!(path = session_path.as_str(), width = link.desktop.width, height = link.desktop.height, source = %link.desktop.source, "desktop connected; Ctrl+Alt+Q quits locally");
    // Choose the presentation format from the actual renderer, not the decoder
    // name. NV12 is useful only when SDL can convert it with its GPU renderer.
    let mut window_state = if options.headless_frames.is_none() {
        Some(session_window(options)?)
    } else {
        None
    };
    let prefer_nv12 = if let Some((_, _, canvas)) = window_state.as_mut() {
        // Any accelerated renderer that passes the NV12 probe (OpenGL on Linux,
        // Metal on macOS) converts on the GPU; RGB24 would cost a CPU
        // conversion of every frame (6-9 ms at 1080p).
        let enabled =
            link.desktop.dynamic_range == protocol::DynamicRange::Sdr && supports_nv12(canvas);
        tracing::info!(
            renderer = canvas.info().name,
            accelerated = renderer_accelerated(canvas),
            nv12 = enabled,
            "local video presentation"
        );
        enabled
    } else {
        // Headless runs measure the path an accelerated window uses.
        link.desktop.dynamic_range == protocol::DynamicRange::Sdr
    };
    let stream_stats = std::sync::Arc::new(crate::stats::StreamStats::default());
    let format = media::VideoFormat {
        codec: link.desktop.codec,
        dynamic_range: link.desktop.dynamic_range,
    };
    #[cfg(target_os = "linux")]
    let mut zero_copy = match window_state.as_mut() {
        Some((_, _, canvas))
            if prefer_nv12
                && !options.software_decoder
                && canvas.info().name == "opengl"
                && !ZERO_COPY_FAILED.load(std::sync::atomic::Ordering::Relaxed)
                && std::env::var("TELEPORT_ZERO_COPY").as_deref() != Ok("0") =>
        {
            ZeroCopy::new(canvas)
        }
        _ => None,
    };
    #[cfg(not(target_os = "linux"))]
    let zero_copy: Option<()> = None;
    #[cfg(target_os = "linux")]
    tracing::info!(
        zero_copy = zero_copy.is_some(),
        decoder = zero_copy.as_ref().map(ZeroCopy::decoder_name),
        "GPU-resident video presentation"
    );
    #[cfg(not(target_os = "linux"))]
    tracing::info!(
        zero_copy = zero_copy.is_some(),
        "GPU-resident video presentation"
    );
    let native_surface = cfg!(target_os = "macos")
        && std::env::var("TELEPORT_MAC_GPU_VIDEO").as_deref() == Ok("1")
        && window_state.is_some()
        && !options.software_decoder
        && !options.software_renderer
        && format.dynamic_range == protocol::DynamicRange::Sdr;
    let (pipeline, source, image) = if native_surface {
        tracing::info!("experimental GPU-resident VideoToolbox / Metal video enabled");
        media::decoder_with_stats_presentation(false, stream_stats.clone(), format, false, true)?
    } else if zero_copy.is_some() {
        #[cfg(target_os = "linux")]
        {
            let decoder = zero_copy.as_ref().map(ZeroCopy::decoder_name);
            media::decoder_with_gpu_frames(
                stream_stats.clone(),
                format,
                decoder.as_deref().unwrap_or("cuda"),
            )?
        }
        #[cfg(not(target_os = "linux"))]
        unreachable!()
    } else if prefer_nv12 {
        media::decoder_with_stats_output(
            options.software_decoder,
            stream_stats.clone(),
            format,
            true,
        )?
    } else {
        media::decoder_with_stats(options.software_decoder, stream_stats.clone(), format)?
    };
    let (events, receiver) = mpsc::channel(256);
    let feedback = events.clone();
    let mut desktop = link.desktop.clone();
    ensure!(
        !options.smoke_audio || desktop.audio,
        "host did not offer audio"
    );
    let (updates_sender, mut updates) = mpsc::channel(32);
    let mut decoded_audio = None;
    let mut audio = if link.audio.is_some() {
        if options.headless_frames.is_some() {
            let (pipeline, source, counter) = crate::audio::decoder_for_headless()?;
            decoded_audio = Some(counter);
            Some((pipeline, source))
        } else {
            Some(crate::audio::decoder()?)
        }
    } else {
        None
    };
    if let Some((pipeline, _)) = &audio {
        pipeline.set_audio_muted(options.mute)?;
    }
    let audio_source = audio.as_ref().map(|(_, source)| source.clone());
    let audio_notices = updates_sender.clone();
    let network_stats = stream_stats.clone();
    let keyframe_requests = desktop.keyframe_requests;
    // IETF MoQ Transport sessions (provider relays) deliver no datagrams.
    let pointer_datagrams =
        desktop.pointer_datagrams && !options.reliable_pointer && link.session.version().is_lite();
    let remote = link.remote.clone();
    let mut network = Network(runtime.spawn(async move {
        let mut ssh_agent = link.ssh_agent;
        let _origin = link._origin;
        let _broadcast = link._broadcast;
        let result = tokio::select! {
            result = crate::ssh_agent::closed(&mut ssh_agent) => result,
            result = media::receive_video_with_stats(link.video, source, feedback, network_stats, keyframe_requests) => result,
            result = send_input(link.input, link.pointer, pointer_datagrams, receiver) => result,
            result = receive_updates(link.updates, updates_sender) => result,
            result = async {
                if let (Some(track), Some(source)) = (link.audio, audio_source)
                    && let Err(error) = crate::audio::receive(track, source).await {
                        tracing::warn!(%error, "remote audio stopped; desktop remains connected");
                        let _ = audio_notices.send(Update::Notice { text: "Audio stopped; reconnect to retry audio".into() }).await;
                        media::wake_presenter();
                }
                std::future::pending::<Result<()>>().await
            } => result,
            error = link.session.closed() => Err(anyhow::anyhow!("connection closed: {error}")),
        };
        // The render loop sleeps until woken; let it see the session end now.
        media::wake_presenter();
        result
    }));
    // The host restarts its video without the pointer once we opt in. That
    // extra `Update::Desktop` is not counted as an expected initial update
    // (it may never come if the host cannot watch its cursor); readiness
    // still waits for the requested monitor and video configuration.
    let local_cursor = desktop.local_cursor && !options.embedded_cursor;
    if desktop.keyframe_requests {
        send(
            &events,
            Event::ClientFeatures {
                keyframe_requests: true,
                pointer_datagrams,
                local_cursor,
            },
        )?;
    }
    if let Some(index) = options.monitor {
        send(&events, Event::SelectMonitor { index })?;
    }
    let configure_video =
        options.width.is_some() || options.fps.is_some() || options.bitrate.is_some();
    let requested_configuration = configure_video.then_some((
        options.width.unwrap_or(desktop.width),
        options.fps.unwrap_or(desktop.fps),
        options.bitrate.unwrap_or(if desktop.bitrate > 0 {
            desktop.bitrate
        } else {
            20_000
        }),
    ));
    let expected_initial_updates =
        usize::from(options.monitor.is_some()) + usize::from(configure_video);
    if configure_video {
        ensure!(
            desktop.configurable_video,
            "this host does not support video quality settings; update the host or omit --width/--fps/--bitrate"
        );
        send(
            &events,
            Event::ConfigureVideo {
                width: options.width.unwrap_or(desktop.width),
                fps: options.fps.unwrap_or(desktop.fps),
                bitrate: options.bitrate.unwrap_or(if desktop.bitrate > 0 {
                    desktop.bitrate
                } else {
                    20_000
                }),
            },
        )?;
    }
    if let Some(frames) = options.headless_frames {
        ensure!(frames > 0, "headless frame count must be positive");
        let deadline = Instant::now() + Duration::from_secs(20);
        let mut count = 0;
        // Delivery timing for network-condition tests: span and longest stall.
        let mut first_frame_at: Option<Instant> = None;
        let mut last_frame_at: Option<Instant> = None;
        let mut max_frame_gap = Duration::ZERO;
        let mut switched = false;
        let mut clipboard_ok = false;
        // --smoke-input-latency: probe send times by id, and completed round trips.
        let mut probes_in_flight = std::collections::BTreeMap::<u64, Instant>::new();
        let mut probe_rtts = Vec::<Duration>::new();
        let mut next_probe = Instant::now();
        let mut probe_id = 0_u64;
        let mut motion_step = 0_u32;
        let mut initial_updates = expected_initial_updates;
        let mut initial_ready = expected_initial_updates == 0;
        const CLIPBOARD_SMOKE: &str = "Teleport clipboard smoke ✓\nline 2";
        while count < frames {
            pipeline.error()?;
            if let Some((pipeline, _)) = &audio {
                pipeline.error()?;
            }
            check_network(&mut network, runtime)?;
            ensure!(
                Instant::now() < deadline,
                "timed out decoding frames ({count}/{frames})"
            );
            while let Ok(update) = updates.try_recv() {
                match update {
                    Update::Desktop { desktop: next } => {
                        ensure!(
                            next.version == protocol::VERSION && next.monitors.len() <= 64,
                            "invalid monitor metadata"
                        );
                        protocol::validate_video_size(next.width, next.height)?;
                        ensure!(
                            next.codec == options.codec
                                && next.dynamic_range == options.dynamic_range,
                            "host changed codec during an active session; reconnect to change codec"
                        );
                        desktop = next;
                        *image.lock().unwrap() = None;
                        initial_updates = initial_updates.saturating_sub(1);
                        if initial_updates == 0
                            && configuration_matches(&desktop, requested_configuration)
                            && options
                                .monitor
                                .is_none_or(|index| index == desktop.active_monitor)
                        {
                            initial_ready = true;
                        }
                    }
                    Update::Clipboard { text } => clipboard_ok = text == CLIPBOARD_SMOKE,
                    Update::Pong { id } => {
                        if let Some(sent) = probes_in_flight.remove(&id) {
                            probe_rtts.push(sent.elapsed());
                        }
                    }
                    Update::Notice { text } => tracing::info!(%text, "host notice"),
                    Update::Cursor {
                        cursor: Some(image),
                    } => {
                        tracing::info!("remote cursor {}x{}", image.width, image.height)
                    }
                    Update::Cursor { cursor: None } => tracing::info!("remote cursor hidden"),
                    _ => (),
                }
            }
            // Never hold the decoder's latest-frame lock during rendering or
            // subsequent network work: it would backpressure raw frame delivery.
            let next_frame = { image.lock().unwrap().take() };
            if let Some(frame) = next_frame.filter(|frame| {
                initial_ready
                    && frame.decoder_recovery
                        == stream_stats
                            .decoder_recoveries
                            .load(std::sync::atomic::Ordering::Relaxed)
                    && frame.width == desktop.width
                    && frame.height == desktop.height
                    && video_generation_matches(desktop.video_start_group, frame.video_group)
            }) {
                ensure!(
                    !frame.data.is_empty()
                        || frame
                            .nv12
                            .as_ref()
                            .is_some_and(|nv12| !nv12.y.is_empty() && !nv12.uv.is_empty())
                        || frame
                            .hdr
                            .as_ref()
                            .is_some_and(|hdr| !hdr.y.is_empty() && !hdr.uv.is_empty()),
                    "empty decoded frame"
                );
                protocol::validate_video_size(frame.width, frame.height)?;
                count += 1;
                let now = Instant::now();
                if let Some(last) = last_frame_at {
                    max_frame_gap = max_frame_gap.max(now - last);
                }
                first_frame_at.get_or_insert(now);
                last_frame_at = Some(now);
                if options.smoke_clipboard && count == 5 {
                    ensure!(
                        options.clipboard && desktop.clipboard,
                        "clipboard must be enabled on both ends"
                    );
                    runtime.block_on(events.send(Event::Clipboard {
                        text: CLIPBOARD_SMOKE.into(),
                    }))?;
                    runtime.block_on(events.send(Event::ClipboardRequest))?;
                }
                if options.smoke_switch_monitor {
                    if count == 5 {
                        runtime.block_on(events.send(Event::SelectMonitor { index: 1 }))?;
                    }
                    if count > 5 && frame.height > frame.width {
                        switched = true;
                    }
                }
                if options.smoke_input && count == 5 {
                    // Exercise several control group rotations before key events.
                    for _ in 0..600 {
                        runtime.block_on(events.send(Event::Ping))?;
                    }
                    let relative = desktop
                        .relative_pointer
                        .then_some(Event::RelativeMotion { dx: 25, dy: -15 });
                    // The test host accepts pad events as no-ops; a host that
                    // rejected them would end this session. Real hosts would
                    // create a system-wide uinput pad (visible to the host
                    // user's games), so smoke runs never press one there.
                    let gamepad = if desktop.gamepads && desktop.source == "test" {
                        vec![
                            Event::GamepadConnected {
                                pad: 0,
                                name: "Smoke pad".into(),
                            },
                            Event::GamepadButton {
                                pad: 0,
                                button: 0,
                                down: true,
                            },
                            Event::GamepadAxis {
                                pad: 0,
                                axis: 0,
                                value: -12000,
                            },
                            Event::GamepadButton {
                                pad: 0,
                                button: 0,
                                down: false,
                            },
                            Event::GamepadDisconnected { pad: 0 },
                        ]
                    } else {
                        Vec::new()
                    };
                    for event in [
                        Event::Motion { x: 0.5, y: 0.5 },
                        Event::Key {
                            code: 30,
                            down: true,
                        },
                        Event::Key {
                            code: 30,
                            down: false,
                        },
                        Event::Key {
                            code: 29,
                            down: true,
                        },
                    ]
                    .into_iter()
                    .chain(relative)
                    .chain(gamepad)
                    {
                        runtime.block_on(events.send(event))?;
                    }
                }
            }
            if options.smoke_input_latency && count > 0 {
                // ~500 Hz motion like a gaming mouse, plus a timed probe every
                // 50 ms riding the same input path as key events.
                motion_step = motion_step.wrapping_add(1);
                let x = f64::from(motion_step % 1000) / 1000.0;
                send(&events, Event::Motion { x, y: 1.0 - x })?;
                if Instant::now() >= next_probe {
                    next_probe = Instant::now() + Duration::from_millis(50);
                    probe_id += 1;
                    probes_in_flight.insert(probe_id, Instant::now());
                    send(&events, Event::Probe { id: probe_id })?;
                }
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        ensure!(
            !options.smoke_switch_monitor || switched,
            "monitor switch did not produce portrait frames"
        );
        ensure!(
            !options.smoke_audio
                || decoded_audio
                    .is_some_and(|counter| counter.load(std::sync::atomic::Ordering::Relaxed) > 0),
            "no network audio decoded"
        );
        ensure!(
            !options.smoke_clipboard || clipboard_ok,
            "clipboard did not roundtrip"
        );
        let snapshot = stream_stats.snapshot();
        let (mean_ready_us, mean_queue_us, mean_decode_us, mean_convert_us) =
            stream_stats.mean_timings_us();
        probe_rtts.sort();
        let probe_percentile = |p: usize| {
            probe_rtts
                .get((probe_rtts.len() * p / 100).min(probe_rtts.len().saturating_sub(1)))
                .map_or(0, |rtt| rtt.as_micros() as u64)
        };
        let (probe_p50_us, probe_p95_us) = (probe_percentile(50), probe_percentile(95));
        let probe_max_us = probe_rtts.last().map_or(0, |rtt| rtt.as_micros() as u64);
        let span_ms = first_frame_at
            .zip(last_frame_at)
            .map_or(0, |(first, last)| (last - first).as_millis() as u64);
        tracing::info!(
            frames = count,
            span_ms,
            max_frame_gap_ms = max_frame_gap.as_millis() as u64,
            probes = probe_rtts.len(),
            probe_p50_us,
            probe_p95_us,
            probe_max_us,
            mean_ready_us,
            mean_queue_us,
            mean_decode_us,
            mean_convert_us,
            width = desktop.width,
            height = desktop.height,
            received_bytes = snapshot.received_bytes,
            received_frames = snapshot.received_frames,
            decoded_frames = snapshot.decoded_frames,
            receive_to_decode_us = snapshot.decode_us,
            input_queue_us = snapshot.queue_us,
            parse_decode_download_us = snapshot.decoder_us,
            conversion_copy_us = snapshot.conversion_us,
            decoder_recoveries = snapshot.decoder_recoveries,
            stale_frames = snapshot.stale_frames,
            decode_errors = snapshot.decode_errors,
            skipped_groups = snapshot.skipped_groups,
            overwritten_frames = snapshot.overwritten_frames,
            unmatched_frames = snapshot.unmatched_frames,
            codec = desktop.codec.label(),
            dynamic_range = desktop.dynamic_range.label(),
            decoder = %stream_stats.decoder.lock().unwrap(),
            "SMOKE PASS: authenticated MoQ video decoded"
        );
        return Ok(false);
    }
    let (sdl, video, mut canvas) = window_state.take().context("session window missing")?;
    // Dropped with the session: the host removes its pads when it ends.
    let mut controllers = controllers::Controllers::new(&sdl);
    let mut graphics_name =
        graphics_renderer(&video).unwrap_or_else(|| "Not exposed by this backend".into());
    tracing::info!(graphics = %graphics_name, "local graphics device");
    if ["llvmpipe", "softpipe", "software rasterizer"]
        .iter()
        .any(|name| graphics_name.to_ascii_lowercase().contains(name))
    {
        tracing::warn!("OpenGL is running on the CPU; check local graphics driver integration");
    }
    let ttf = sdl2::ttf::init().map_err(anyhow::Error::msg)?;
    let mut session_fonts = SessionFonts {
        ttf: &ttf,
        fonts: std::collections::HashMap::new(),
    };
    let creator = canvas.texture_creator();
    let mut hdr_presenter = if native_surface {
        Some(crate::hdr_present::HdrPresenter::new_sdr(canvas.window())?)
    } else if desktop.dynamic_range == protocol::DynamicRange::Hdr10 {
        Some(crate::hdr_present::HdrPresenter::new(canvas.window())?)
    } else {
        None
    };
    if let Some(presenter) = &hdr_presenter {
        graphics_name = presenter.device_name();
        tracing::info!(graphics = %graphics_name, "native Metal presentation device");
    }
    let mut hdr_frame: Option<media::Image> = None;
    let mut hdr_frame_pending = false;
    let mut hdr_headroom: Option<f64> = None;
    let mut text_cache = std::collections::HashMap::new();
    let mut texture = None;
    // Newest GPU-resident frame (zero-copy path), redrawn until replaced.
    let mut gpu_frame: Option<crate::ffmpeg::GpuFrame> = None;
    let mut size = (0, 0);
    let mut event_pump = sdl.event_pump().map_err(anyhow::Error::msg)?;
    let mut last_frame = Instant::now();
    let mut frames = 0;
    let mut total_frames = 0;
    let mut stats = Instant::now();
    let mut muted = options.mute;
    let mut reconnect = false;
    let mut notice = String::new();
    let mut previous_notice = String::new();
    let mut notice_at = Instant::now();
    let mut clipboard_requested = false;
    let mut switching = expected_initial_updates > 0;
    let mut initial_updates = expected_initial_updates;
    let mut awaiting_initial = expected_initial_updates > 0;
    let initial_settings_started = Instant::now();
    let mut switch_ack = false;
    let mut first_draw = true;
    let mut show_stats = options.stats;
    let mut fps = 0.0;
    let mut mbps = 0.0;
    let mut previous_bytes = 0;
    let mut previous_received = 0_u64;
    let mut previous_decoded = 0_u64;
    let mut wheel = WheelAccumulator::default();
    let mut probe_id = 0_u64;
    let mut pending_probe: Option<(u64, Instant)> = None;
    let mut last_probe = Instant::now() - Duration::from_secs(1);
    let mut rtt: Option<Duration> = None;
    let mut decoded_age = Duration::ZERO;
    let mut stats_lines = Vec::new();
    let mut host_encode_us = 0;
    let mut host_bitrate = desktop.bitrate;
    let mut host_encoder = "Waiting / unavailable".to_owned();
    let mut displays = multi_display::Displays::new(remote, options, events.clone());
    let mut keyboard_capture = crate::keyboard_capture::Capture::default();
    // Pointer lock for games; the guard releases it on every exit path.
    let mut pointer_lock = PointerLock::default();
    // Host cursor shapes (X11 hosts); dropped before the video subsystem.
    let mut remote_cursor = RemoteCursor::default();
    let mut shortcuts = crate::keyboard_capture::Shortcuts::default();
    let mut chrome = crate::fullscreen::Chrome::new();
    let mut placement: Option<crate::fullscreen::Placement> = None;
    // Input-only redraws (pointer motion, hover) are capped so a 1000 Hz mouse
    // cannot drive a full present per event; new video frames never wait.
    let mut deferred_redraw = false;
    let mut last_present: Option<Instant> = None;
    // Decoded frames, host updates and the session ending push a wake event,
    // so the loop blocks between them instead of polling every millisecond.
    let woken = register_render_wake();
    let mut present_timer = PresentTimer::new();
    // Decoder store time of the frame drawn by the next present.
    let mut frame_decoded_at: Option<Instant> = None;
    'running: loop {
        present_timer.iterations += 1;
        WAKE_QUEUED.store(false, Ordering::Release);
        let mut redraw = first_draw || deferred_redraw;
        let mut new_frame = false;
        let was_bound = keyboard_capture.active();
        keyboard_capture.follow(displays.focused_window(canvas.window()));
        if was_bound && !keyboard_capture.active() {
            send(&events, Event::ReleaseAll)?;
            notice = "Keyboard released after leaving session focus".into();
            redraw = true;
        }
        let mouse = event_pump.mouse_state();
        // SDL mouse coordinates are relative to its mouse-focus window, not
        // necessarily this primary display in a multi-window session.
        let mouse_focus = unsafe { sdl2::sys::SDL_GetMouseFocus() };
        let pointer_here = mouse_focus == canvas.window().raw();
        redraw |= chrome.update(
            pointer_here.then_some((mouse.x(), mouse.y())),
            pointer_here && (mouse.left() || mouse.middle() || mouse.right()),
        );
        // Auxiliary monitor windows are all video; the primary has a toolbar.
        let over_video = !mouse_focus.is_null()
            && (!pointer_here
                || chrome
                    .pointer(mouse.x(), mouse.y(), canvas.window().size(), size)
                    .is_some());
        remote_cursor.apply(over_video, pointer_lock.active());
        displays.draw(runtime, &mut session_fonts)?;
        first_draw = false;
        pipeline.error()?;
        if let Some((pipeline, _)) = &audio
            && let Err(error) = pipeline.error()
        {
            tracing::warn!(%error, "audio output unavailable; desktop remains connected");
            notice = "Audio output unavailable; reconnect to retry".into();
            audio = None;
        }
        check_network(&mut network, runtime)?;
        ensure!(
            !awaiting_initial || initial_settings_started.elapsed() < Duration::from_secs(15),
            "host did not apply the requested display/video settings within 15 seconds"
        );
        if desktop.telemetry && last_probe.elapsed() >= Duration::from_secs(1) {
            if pending_probe.is_some_and(|(_, sent)| sent.elapsed() >= Duration::from_secs(3)) {
                pending_probe = None;
                rtt = None;
            }
            if pending_probe.is_none() {
                probe_id = probe_id.wrapping_add(1);
                send(&events, Event::Probe { id: probe_id })?;
                pending_probe = Some((probe_id, Instant::now()));
                last_probe = Instant::now();
            }
        }
        while let Ok(update) = updates.try_recv() {
            redraw = true;
            match update {
                Update::MonitorStream { index, desktop } => displays.update(index, desktop)?,
                // A queued close acknowledgement may arrive after a new mapping
                // has started. Local close already removed the old consumer.
                Update::MonitorStreamStopped {
                    index,
                    reason,
                    requested: false,
                } => displays.stopped(index, reason),
                Update::MonitorStreamStopped {
                    requested: true, ..
                } => (),
                Update::Telemetry {
                    encode_us,
                    bitrate,
                    encoder,
                } => {
                    host_encode_us = encode_us;
                    host_bitrate = bitrate;
                    host_encoder = encoder.chars().take(48).collect();
                }
                Update::Pong { id } => {
                    if let Some((expected, sent)) = pending_probe
                        && id == expected
                    {
                        rtt = Some(sent.elapsed());
                        pending_probe = None;
                    }
                }
                Update::Desktop { desktop: next } => {
                    ensure!(
                        next.version == protocol::VERSION && next.monitors.len() <= 64,
                        "invalid monitor metadata"
                    );
                    protocol::validate_video_size(next.width, next.height)?;
                    ensure!(
                        next.codec == options.codec && next.dynamic_range == options.dynamic_range,
                        "host changed codec during an active session; reconnect to change codec"
                    );
                    desktop = next;
                    host_bitrate = desktop.bitrate;
                    tracing::info!(
                        requested_kbps = options.bitrate,
                        accepted_cap_kbps = desktop.bitrate,
                        width = desktop.width,
                        height = desktop.height,
                        fps = desktop.fps,
                        "host acknowledged video configuration"
                    );
                    wheel = WheelAccumulator::default();
                    *image.lock().unwrap() = None;
                    hdr_frame = None;
                    hdr_frame_pending = false;
                    initial_updates = initial_updates.saturating_sub(1);
                    switch_ack = !awaiting_initial
                        || (initial_updates == 0
                            && configuration_matches(&desktop, requested_configuration)
                            && options
                                .monitor
                                .is_none_or(|index| index == desktop.active_monitor));
                    notice = format!("Monitor {}", desktop.active_monitor + 1);
                }
                Update::Clipboard { text } if options.clipboard && clipboard_requested => {
                    Event::Clipboard { text: text.clone() }.validate()?;
                    video
                        .clipboard()
                        .set_clipboard_text(&text)
                        .map_err(anyhow::Error::msg)?;
                    clipboard_requested = false;
                    notice = "Host text copied to this clipboard".into();
                }
                Update::Notice { text } => {
                    notice = text.chars().take(160).collect();
                }
                Update::Cursor { cursor } => remote_cursor.set_shape(cursor.as_ref()),
                _ => (),
            }
        }
        let mut motion = None;
        let mut relative = (0_i32, 0_i32);
        for mut event in event_pump.poll_iter() {
            if matches!(event, SdlEvent::User { type_, .. } if type_ == WAKE_EVENT.load(Ordering::Relaxed))
            {
                continue;
            }
            redraw = true;
            keyboard_capture.follow(displays.focused_window(canvas.window()));
            displays.route_pointer(&mut event, canvas.window());
            // Moving focus between session windows must not release a remote
            // drag or held modifier. Leaving the entire session still must.
            if matches!(
                event,
                SdlEvent::Window {
                    win_event: WindowEvent::FocusLost,
                    ..
                }
            ) && !displays.owns_keyboard_focus(canvas.window())
            {
                motion = None;
                if pointer_lock.set(false) {
                    notice = "Mouse released after leaving the session window".into();
                }
                shortcuts = crate::keyboard_capture::Shortcuts::default();
                // Also centres and releases every host gamepad.
                send(&events, Event::ReleaseAll)?;
            }
            // SDL delivers controller input only while a session window has
            // focus (SDL_JOYSTICK_ALLOW_BACKGROUND_EVENTS is off by default).
            if controllers.event(&event, &events, desktop.gamepads)? {
                continue;
            }
            if let Some(action) = shortcuts.event(&event, keyboard_capture.active()) {
                use crate::keyboard_capture::Action;
                match action {
                    Action::Toggle => {
                        motion = None;
                        send(&events, Event::ReleaseAll)?;
                        notice = toggle_keyboard_capture(
                            &mut keyboard_capture,
                            displays.focused_window(canvas.window()),
                        );
                    }
                    Action::PointerLock => {
                        motion = None;
                        notice = if pointer_lock.active() {
                            pointer_lock.set(false);
                            "Mouse released".into()
                        } else if !desktop.relative_pointer {
                            "Host does not support mouse lock; update the host".into()
                        } else if pointer_lock.set(true) {
                            "Mouse locked for games \u{2014} Ctrl+Alt+M releases".into()
                        } else {
                            "Mouse lock unavailable on this display".into()
                        };
                    }
                    Action::Disconnect => break 'running,
                    Action::Stats => show_stats = !show_stats,
                    Action::Swallow => (),
                }
                continue;
            }
            // Flush primary motion before an auxiliary event, preserving the
            // cross-monitor drag order instead of dropping its last position.
            if event
                .get_window_id()
                .is_some_and(|id| id != canvas.window().id())
                && let Some(pending) = motion.take()
            {
                send(&events, pending)?;
            }
            if displays.event(&event, &desktop, runtime)? {
                continue;
            }
            if let Some(id) = event.get_window_id()
                && id != canvas.window().id()
            {
                continue;
            }
            if let SdlEvent::MouseMotion {
                x, y, mousestate, ..
            } = event
            {
                redraw |= chrome.update(
                    Some((x, y)),
                    mousestate.left() || mousestate.right() || mousestate.middle(),
                );
            }
            // Toolbar is local UI. Never forward its clicks or held modifier keys.
            if let SdlEvent::MouseButtonDown {
                mouse_btn: MouseButton::Left,
                x,
                y,
                ..
            } = event
                && !pointer_lock.active()
                && chrome.visible()
                && chrome.blocks_pointer(y)
            {
                motion = None;
                wheel = WheelAccumulator::default();
                send(&events, Event::ReleaseAll)?;
                if keyboard_capture_rect().contains_point((x, y)) {
                    notice = toggle_keyboard_capture(
                        &mut keyboard_capture,
                        Some(canvas.window().clone()),
                    );
                    continue;
                }
                if display_mode_rect().contains_point((x, y)) {
                    if chrome.fullscreen() && displays.active() {
                        displays.stop();
                        notice = "Using this local display only".into();
                    } else if desktop.multimonitor
                        && desktop.monitors.len() > 1
                        && video.num_video_displays().map_err(anyhow::Error::msg)? > 1
                    {
                        let local = canvas
                            .window()
                            .display_index()
                            .map_err(anyhow::Error::msg)?;
                        if !chrome.fullscreen() {
                            placement =
                                Some(crate::fullscreen::Placement::capture(canvas.window()));
                            crate::fullscreen::enter(canvas.window_mut(), &video, local)?;
                            chrome.set_fullscreen(true);
                        }
                        if let Err(error) = displays.start(&video, local, &desktop, runtime) {
                            displays.stop();
                            notice = format!("Additional displays unavailable: {error}");
                        }
                    } else {
                        notice = if !desktop.multimonitor {
                            "Update the host to enable independent monitor streams"
                        } else {
                            "Connect another local display and share another remote monitor"
                        }
                        .into();
                    }
                    continue;
                }
                match toolbar_hit(x, y, canvas.window().size().0) {
                    Some(0) if !switching && desktop.monitors.len() > 1 => {
                        let index = (desktop.active_monitor + 1) % desktop.monitors.len();
                        send(&events, Event::SelectMonitor { index })?;
                        switching = true;
                        switch_ack = false;
                        notice = "Switching monitor…".into();
                    }
                    Some(1) => {
                        muted = !muted;
                        if let Some((pipeline, _)) = &audio {
                            pipeline.set_audio_muted(muted)?;
                        }
                        notice = if !desktop.audio {
                            "Host audio is disabled"
                        } else if muted {
                            "Audio muted"
                        } else {
                            "Audio on"
                        }
                        .into();
                    }
                    Some(2) if options.clipboard && desktop.clipboard => {
                        match video.clipboard().clipboard_text() {
                            Ok(text) => {
                                let event = Event::Clipboard { text };
                                match event.validate() {
                                    Ok(()) => {
                                        send(&events, event)?;
                                        notice = "Sending clipboard…".into();
                                    }
                                    Err(error) => notice = error.to_string(),
                                }
                            }
                            Err(error) => notice = error,
                        }
                    }
                    Some(3) if options.clipboard && desktop.clipboard => {
                        send(&events, Event::ClipboardRequest)?;
                        clipboard_requested = true;
                        notice = "Fetching clipboard…".into();
                    }
                    Some(2 | 3) => {
                        notice = "Clipboard requires --clipboard on both host and client".into()
                    }
                    Some(4) => {
                        if chrome.fullscreen() {
                            displays.stop();
                            if let Some(saved) = placement.take() {
                                saved.restore(canvas.window_mut())?;
                            }
                            chrome.set_fullscreen(false);
                        } else {
                            placement =
                                Some(crate::fullscreen::Placement::capture(canvas.window()));
                            let local = canvas
                                .window()
                                .display_index()
                                .map_err(anyhow::Error::msg)?;
                            crate::fullscreen::enter(canvas.window_mut(), &video, local)?;
                            chrome.set_fullscreen(true);
                            if desktop.multimonitor
                                && let Err(error) = displays.start(&video, local, &desktop, runtime)
                            {
                                displays.stop();
                                notice = format!("Additional displays unavailable: {error}");
                            }
                        }
                    }
                    Some(5) => {
                        reconnect = true;
                        break 'running;
                    }
                    Some(6) => break 'running,
                    Some(7) => show_stats = !show_stats,
                    _ => (),
                }
                continue;
            }
            if switching
                && matches!(
                    event,
                    SdlEvent::MouseMotion { .. }
                        | SdlEvent::MouseButtonDown { .. }
                        | SdlEvent::MouseButtonUp { .. }
                        | SdlEvent::MouseWheel { .. }
                        | SdlEvent::KeyDown { .. }
                        | SdlEvent::KeyUp { .. }
                )
            {
                continue;
            }
            let event = match event {
                SdlEvent::Quit { .. } => break 'running,
                SdlEvent::KeyDown {
                    keycode: Some(Keycode::Q),
                    keymod,
                    ..
                } if keymod.intersects(Mod::LCTRLMOD | Mod::RCTRLMOD)
                    && keymod.intersects(Mod::LALTMOD | Mod::RALTMOD) =>
                {
                    break 'running;
                }
                SdlEvent::KeyDown {
                    scancode: Some(code),
                    repeat: false,
                    ..
                } => protocol::evdev(code).map(|code| Event::Key { code, down: true }),
                SdlEvent::KeyUp {
                    scancode: Some(code),
                    ..
                } => protocol::evdev(code).map(|code| Event::Key { code, down: false }),
                SdlEvent::MouseMotion { xrel, yrel, .. } if pointer_lock.active() => {
                    relative.0 = relative.0.saturating_add(xrel);
                    relative.1 = relative.1.saturating_add(yrel);
                    None
                }
                SdlEvent::MouseButtonDown { mouse_btn, .. } if pointer_lock.active() => {
                    flush_relative(&events, &mut relative)?;
                    button(mouse_btn).map(|button| Event::Button { button, down: true })
                }
                SdlEvent::MouseButtonUp { mouse_btn, .. } if pointer_lock.active() => {
                    flush_relative(&events, &mut relative)?;
                    button(mouse_btn).map(|button| Event::Button {
                        button,
                        down: false,
                    })
                }
                SdlEvent::MouseMotion { x, y, .. } => {
                    motion = chrome
                        .pointer(x, y, canvas.window().size(), size)
                        .map(|(x, y)| Event::Motion { x, y });
                    None
                }
                SdlEvent::MouseButtonDown {
                    mouse_btn, x, y, ..
                } => {
                    motion = None;
                    if let Some((x, y)) = chrome.pointer(x, y, canvas.window().size(), size) {
                        send(&events, Event::Motion { x, y })?;
                        button(mouse_btn).map(|button| Event::Button { button, down: true })
                    } else {
                        None
                    }
                }
                SdlEvent::MouseButtonUp {
                    mouse_btn, x, y, ..
                } => {
                    motion = None;
                    if let Some((x, y)) = chrome.pointer(x, y, canvas.window().size(), size) {
                        send(&events, Event::Motion { x, y })?;
                    }
                    button(mouse_btn).map(|button| Event::Button {
                        button,
                        down: false,
                    })
                }
                SdlEvent::MouseWheel {
                    x,
                    y,
                    precise_x,
                    precise_y,
                    mouse_x,
                    mouse_y,
                    direction,
                    ..
                } => {
                    // Use this event's cursor position, not the end-of-batch
                    // mouse state. Never let a coalesced motion follow a wheel
                    // event and rewind the remote pointer after scrolling.
                    motion = None;
                    for event in wheel.events(
                        (x, y),
                        (precise_x, precise_y),
                        direction == sdl2::mouse::MouseWheelDirection::Flipped,
                        chrome.pointer(mouse_x, mouse_y, canvas.window().size(), size),
                    )? {
                        send(&events, event)?;
                    }
                    None
                }
                SdlEvent::Window {
                    win_event: WindowEvent::FocusLost | WindowEvent::FocusGained,
                    ..
                } => {
                    motion = None;
                    wheel = WheelAccumulator::default();
                    None
                }
                SdlEvent::RenderTargetsReset { .. } | SdlEvent::RenderDeviceReset { .. } => {
                    texture = None;
                    size = (0, 0);
                    text_cache.clear();
                    None
                }
                SdlEvent::Window {
                    win_event: WindowEvent::Close,
                    ..
                } => break 'running,
                _ => None,
            };
            if let Some(event) = event {
                send(&events, event)?;
            }
        }
        flush_relative(&events, &mut relative)?;
        if let Some(event) = motion {
            send(&events, event)?;
        }
        controllers.flush(&events, desktop.gamepads)?;
        if let Some(text) = controllers.take_notice() {
            notice = text;
            redraw = true;
        }
        let next_frame = { image.lock().unwrap().take() };
        if let Some(mut frame) = next_frame.filter(|frame| {
            frame.decoder_recovery
                == stream_stats
                    .decoder_recoveries
                    .load(std::sync::atomic::Ordering::Relaxed)
                && video_generation_matches(desktop.video_start_group, frame.video_group)
        }) {
            redraw = true;
            new_frame = true;
            decoded_age = frame.decoded_at.elapsed();
            frame_decoded_at = Some(frame.decoded_at);
            present_timer.picked_up(frame.decoded_at);
            protocol::validate_video_size(frame.width, frame.height)?;
            'frame: {
                if let Some(gpu) = frame.gpu.take() {
                    // Zero-copy: keep the decoded surface; it is drawn on the GPU below.
                    size = (frame.width, frame.height);
                    texture = None;
                    gpu_frame = Some(gpu);
                    last_frame = Instant::now();
                    if switch_ack && frame.width == desktop.width && frame.height == desktop.height
                    {
                        switching = false;
                        awaiting_initial = false;
                        switch_ack = false;
                    }
                    frames += 1;
                    total_frames += 1;
                    break 'frame;
                }
                let native_frame = frame.hdr.is_some() || frame.surface.is_some();
                if size != (frame.width, frame.height) && !native_frame {
                    size = (frame.width, frame.height);
                    if frame.nv12.is_some() {
                        set_nv12_conversion();
                    }
                    texture = Some(creator.create_texture_streaming(
                        if frame.nv12.is_some() {
                            PixelFormatEnum::NV12
                        } else {
                            PixelFormatEnum::RGB24
                        },
                        size.0,
                        size.1,
                    )?);
                }
                if let Some(nv12) = &frame.nv12 {
                    update_nv12(
                        texture.as_mut().context("missing NV12 texture")?,
                        nv12,
                        frame.width,
                        frame.height,
                    )?;
                } else if !native_frame {
                    texture.as_mut().context("missing SDR texture")?.update(
                        None,
                        &frame.data,
                        frame.stride,
                    )?;
                } else {
                    ensure!(
                        hdr_presenter.is_some(),
                        "received native video without a Metal presenter"
                    );
                    size = (frame.width, frame.height);
                }
                last_frame = Instant::now();
                if native_frame {
                    hdr_frame = Some(frame);
                    hdr_frame_pending = true;
                } else {
                    if switch_ack && frame.width == desktop.width && frame.height == desktop.height
                    {
                        switching = false;
                        awaiting_initial = false;
                        switch_ack = false;
                    }
                    frames += 1;
                    total_frames += 1;
                }
            }
        }
        ensure!(
            total_frames > 0 || last_frame.elapsed() < Duration::from_secs(15),
            "no initial decoded video for 15 seconds"
        );
        let throttled = redraw
            && !new_frame
            && last_present.is_some_and(|at| at.elapsed() < Duration::from_millis(4));
        if (!redraw || throttled) && stats.elapsed() < Duration::from_secs(1) {
            deferred_redraw = throttled;
            // Input, decoded frames, host updates and the session ending all
            // wake this at once. The timeout only paces throttled input redraws
            // and polled state (stats, probes, toolbar hiding, pipeline errors).
            // A null event only peeks, so poll_iter still sees every event.
            let wait = if !woken {
                Duration::from_millis(1)
            } else if throttled {
                Duration::from_millis(4)
                    .saturating_sub(last_present.map_or(Duration::ZERO, |at| at.elapsed()))
            } else {
                IDLE_WAIT
            }
            .min(Duration::from_secs(1).saturating_sub(stats.elapsed()));
            let wait_ms = wait.as_micros().div_ceil(1000).clamp(1, 1000) as i32;
            unsafe {
                sdl2::sys::SDL_WaitEventTimeout(std::ptr::null_mut(), wait_ms);
            }
            continue;
        }
        deferred_redraw = false;
        let window_size = canvas.window().size();
        canvas.set_logical_size(window_size.0.max(1), window_size.1.max(TOOLBAR_HEIGHT + 1))?;
        canvas.set_draw_color(sdl2::pixels::Color::RGB(12, 15, 20));
        canvas.clear();
        if let Some(texture) = &texture {
            canvas
                .copy(texture, None, chrome.rect(window_size, size))
                .map_err(anyhow::Error::msg)?;
        }
        #[cfg(target_os = "linux")]
        if let (Some(presenter), Some(frame)) = (zero_copy.as_mut(), gpu_frame.as_ref())
            && let Err(error) = presenter.draw(&mut canvas, frame, chrome.rect(window_size, size))
        {
            // Never show a frozen or black picture: switch this process to the
            // CPU NV12 path and reconnect.
            tracing::warn!(
                error = format!("{error:#}"),
                "zero-copy presentation failed; reconnecting with CPU presentation"
            );
            ZERO_COPY_FAILED.store(true, std::sync::atomic::Ordering::Relaxed);
            return Ok(true);
        }
        if stats.elapsed() >= Duration::from_secs(1) || stats_lines.is_empty() {
            let snapshot = stream_stats.snapshot();
            let elapsed = stats.elapsed().as_secs_f64().max(0.001);
            // The first HUD paint is not a rate sample: wait for a full interval.
            fps = if elapsed >= 1.0 {
                frames as f64 / elapsed
            } else {
                0.0
            };
            mbps = if elapsed >= 1.0 {
                snapshot.received_bytes.saturating_sub(previous_bytes) as f64 * 8.0
                    / elapsed
                    / 1_000_000.0
            } else {
                0.0
            };
            previous_bytes = snapshot.received_bytes;
            let received_fps = if elapsed >= 1.0 {
                snapshot.received_frames.saturating_sub(previous_received) as f64 / elapsed
            } else {
                0.0
            };
            let decoded_fps = if elapsed >= 1.0 {
                snapshot.decoded_frames.saturating_sub(previous_decoded) as f64 / elapsed
            } else {
                0.0
            };
            previous_received = snapshot.received_frames;
            previous_decoded = snapshot.decoded_frames;
            let latency = rtt.map_or_else(
                || "Waiting / unavailable".into(),
                |rtt| format!("{:.1} ms", rtt.as_secs_f64() * 1000.0),
            );
            let timing = |microseconds: u64| {
                if microseconds == 0 {
                    "Unavailable".into()
                } else {
                    format!("{:.2} ms (latest frame)", microseconds as f64 / 1000.0)
                }
            };
            let encode = if host_encode_us == 0 {
                "Unavailable".into()
            } else {
                format!("{:.2} ms (latest frame)", host_encode_us as f64 / 1000.0)
            };
            stats_lines = vec![
                if keyboard_capture.active() {
                    "STATS FOR NERDS                 Stats button to hide (F8 goes remote)"
                } else {
                    "STATS FOR NERDS                                     F8 to hide"
                }
                .into(),
                format!(
                    "Video                 {} x {}   |   {} / {}",
                    size.0,
                    size.1,
                    desktop.dynamic_range.label(),
                    desktop.codec.label()
                ),
                format!("Presented             {fps:.1} fps"),
                format!("Video payload (main)  {mbps:.2} Mbps (not wire bitrate)"),
                format!(
                    "Request / accepted cap {} / {:.2} Mbps",
                    options.bitrate.map_or_else(
                        || "host default".into(),
                        |rate| format!("{:.2}", rate as f64 / 1000.0)
                    ),
                    desktop.bitrate as f64 / 1000.0
                ),
                format!("Path                  {}", session_path.description()),
                format!("Control round trip    {latency}"),
                format!("Host encoder          {host_encoder}"),
                format!(
                    "Client decoder        {}",
                    stream_stats.decoder.lock().unwrap()
                ),
                format!(
                    "Renderer              {}",
                    if hdr_presenter.is_some() {
                        "Metal"
                    } else {
                        canvas.info().name
                    }
                ),
                format!("Graphics device       {graphics_name}"),
                format!(
                    "Pixel path            {}",
                    if native_surface {
                        "VideoToolbox surface / Metal (no video CPU copy)"
                    } else if gpu_frame.is_some() {
                        #[cfg(target_os = "linux")]
                        {
                            zero_copy.as_ref().map_or("", ZeroCopy::pixel_path)
                        }
                        #[cfg(not(target_os = "linux"))]
                        ""
                    } else if desktop.dynamic_range == protocol::DynamicRange::Hdr10 {
                        "P010 CPU copy / Metal upload"
                    } else if prefer_nv12 {
                        "NV12 upload / renderer color conversion"
                    } else {
                        "RGB CPU conversion / upload"
                    }
                ),
                format!(
                    "Encoder target        {:.2} Mbps",
                    host_bitrate as f64 / 1000.0
                ),
                format!("Host encode           {encode}"),
                format!("Receive to ready      {}", timing(snapshot.decode_us)),
                format!("Compressed queue      {}", timing(snapshot.queue_us)),
                format!("Parse/decode/download {}", timing(snapshot.decoder_us)),
                format!("Convert / copy        {}", timing(snapshot.conversion_us)),
                format!(
                    "Decoded frame wait    {:.2} ms (latest frame)",
                    decoded_age.as_secs_f64() * 1000.0
                ),
                format!(
                    "Decoder queue         {} bytes",
                    snapshot.decoder_queue_bytes
                ),
                format!("Received / decoded    {received_fps:.1} / {decoded_fps:.1} fps"),
                format!(
                    "Skipped groups        {}   |   Superseded frames  {}",
                    snapshot.skipped_groups, snapshot.overwritten_frames
                ),
                format!("Unmatched timestamps  {}", snapshot.unmatched_frames),
                format!(
                    "Decoder recoveries    {}   |   Stale frames {}   |   Decode errors {}",
                    snapshot.decoder_recoveries, snapshot.stale_frames, snapshot.decode_errors
                ),
                "Decode includes scheduling/download, not pure GPU time.".into(),
                "RTT includes control scheduling, not just network transit.".into(),
                "Capture, scanout and end-to-end latency: unmeasured.".into(),
            ];
            if let Some(headroom) = hdr_headroom {
                stats_lines.push(format!("HDR display headroom  {headroom:.2} × SDR white"));
            }
            stats = Instant::now();
            frames = 0;
        }
        use sdl2::pixels::Color;
        if chrome.visible() {
            canvas.set_draw_color(Color::RGB(21, 27, 38));
            canvas
                .fill_rect(Rect::new(0, 0, window_size.0, TOOLBAR_HEIGHT))
                .map_err(anyhow::Error::msg)?;
            canvas.set_draw_color(Color::RGB(46, 57, 74));
            canvas
                .fill_rect(Rect::new(0, TOOLBAR_HEIGHT as i32 - 1, window_size.0, 1))
                .map_err(anyhow::Error::msg)?;
            let mouse = event_pump.mouse_state();
            let hover = toolbar_hit(mouse.x(), mouse.y(), window_size.0);
            let fullscreen = canvas.window().fullscreen_state() != sdl2::video::FullscreenType::Off;
            let labels = [
                format!("Display {}", desktop.active_monitor + 1),
                if audio.is_none() {
                    "Audio"
                } else if muted {
                    "Muted"
                } else {
                    "Audio"
                }
                .into(),
                "Send".into(),
                "Get".into(),
                if fullscreen { "Window" } else { "Full" }.into(),
                "Retry".into(),
                "Disconnect".into(),
                "Stats".into(),
            ];
            for (index, label) in labels.iter().enumerate() {
                let rect = toolbar_rect(index, window_size.0);
                let enabled = match index {
                    0 => desktop.monitors.len() > 1 && !switching,
                    1 => audio.is_some(),
                    2 | 3 => options.clipboard && desktop.clipboard,
                    _ => true,
                };
                let active = (index == 1 && audio.is_some() && !muted)
                    || (index == 4 && fullscreen)
                    || (index == 7 && show_stats);
                canvas.set_draw_color(if hover == Some(index) && enabled {
                    if index == 6 {
                        Color::RGB(114, 47, 59)
                    } else {
                        Color::RGB(51, 69, 91)
                    }
                } else if active {
                    Color::RGB(27, 68, 75)
                } else {
                    Color::RGB(30, 38, 51)
                });
                canvas.fill_rect(rect).map_err(anyhow::Error::msg)?;
                canvas.set_draw_color(if active {
                    Color::RGB(71, 196, 172)
                } else {
                    Color::RGB(48, 61, 79)
                });
                canvas.draw_rect(rect).map_err(anyhow::Error::msg)?;
                session_text(
                    &mut canvas,
                    &creator,
                    &mut session_fonts,
                    &mut text_cache,
                    label,
                    Rect::new(rect.x() + 9, rect.y() + 9, rect.width() - 18, 20),
                    if !enabled {
                        Color::RGB(126, 137, 153)
                    } else if index == 6 {
                        Color::RGB(255, 161, 166)
                    } else {
                        Color::RGB(223, 233, 243)
                    },
                )?;
            }
            let hint = match hover {
                Some(0) => "Display · switch to the next remote monitor",
                Some(1) => "Audio · toggle playback (requires host audio)",
                Some(2) => "Send text · copy your clipboard to the remote desktop",
                Some(3) => "Get text · copy remote text to your clipboard",
                Some(4) => "Fullscreen · fill local displays; move to top to reveal controls",
                Some(5) => "Reconnect · release held keys and start a fresh connection",
                Some(6) => "Disconnect · end this session safely (Ctrl+Alt+Q)",
                Some(7) => "Stats for nerds · measured streaming performance (F8)",
                _ => "",
            };
            if previous_notice != notice {
                previous_notice.clone_from(&notice);
                notice_at = Instant::now();
            }
            let status = if !hint.is_empty() {
                hint.into()
            } else if !notice.is_empty() && notice_at.elapsed() < Duration::from_secs(5) {
                notice.clone()
            } else {
                format!(
                    "CONNECTED   ·   Display {} of {}   ·   {} × {}   ·   {}   ·   {fps:.0} fps   ·   {mbps:.1} Mbps",
                    desktop.active_monitor + 1,
                    desktop.monitors.len(),
                    size.0,
                    size.1,
                    desktop.dynamic_range.label()
                )
            };
            let status = if options.forward_ssh_agent {
                format!("SSH AGENT SHARED | {status}")
            } else {
                status
            };
            let status = if keyboard_capture.active() {
                format!("Ctrl+Alt+K releases keys | {status}")
            } else {
                status
            };
            session_text(
                &mut canvas,
                &creator,
                &mut session_fonts,
                &mut text_cache,
                &status,
                Rect::new(326, 55, window_size.0.saturating_sub(340), 20),
                Color::RGB(146, 167, 188),
            )?;
            let mode = display_mode_rect();
            let bind = keyboard_capture_rect();
            canvas.set_draw_color(if keyboard_capture.active() {
                Color::RGB(27, 68, 75)
            } else {
                Color::RGB(30, 48, 62)
            });
            canvas.fill_rect(bind).map_err(anyhow::Error::msg)?;
            session_text(
                &mut canvas,
                &creator,
                &mut session_fonts,
                &mut text_cache,
                if keyboard_capture.active() {
                    "Keys bound"
                } else {
                    "Bind keys"
                },
                Rect::new(bind.x() + 8, bind.y() + 4, bind.width() - 16, 20),
                Color::RGB(160, 223, 212),
            )?;
            canvas.set_draw_color(Color::RGB(30, 48, 62));
            canvas.fill_rect(mode).map_err(anyhow::Error::msg)?;
            session_text(
                &mut canvas,
                &creator,
                &mut session_fonts,
                &mut text_cache,
                if displays.active() {
                    "Use one display"
                } else {
                    "Use all displays"
                },
                Rect::new(mode.x() + 8, mode.y() + 4, mode.width() - 16, 20),
                Color::RGB(160, 223, 212),
            )?;
        } else {
            canvas.set_draw_color(Color::RGB(99, 218, 193));
            canvas
                .fill_rect(reveal_rect(window_size.0))
                .map_err(anyhow::Error::msg)?;
        }
        let mut stats_panel = None;
        if show_stats {
            let panel_width = 520.min(window_size.0.saturating_sub(24));
            let panel = Rect::new(
                window_size.0 as i32 - panel_width as i32 - 12,
                if chrome.visible() {
                    TOOLBAR_HEIGHT as i32 + 12
                } else {
                    12
                },
                panel_width,
                stats_lines.len() as u32 * 24 + 28,
            );
            stats_panel = Some(panel);
            canvas.set_draw_color(Color::RGB(16, 23, 33));
            canvas.fill_rect(panel).map_err(anyhow::Error::msg)?;
            canvas.set_draw_color(Color::RGB(67, 103, 114));
            canvas.draw_rect(panel).map_err(anyhow::Error::msg)?;
            for (index, line) in stats_lines.iter().enumerate() {
                session_text(
                    &mut canvas,
                    &creator,
                    &mut session_fonts,
                    &mut text_cache,
                    line,
                    Rect::new(
                        panel.x() + 14,
                        panel.y() + 14 + index as i32 * 24,
                        panel_width - 28,
                        20,
                    ),
                    if index == 0 {
                        Color::RGB(99, 218, 193)
                    } else {
                        Color::RGB(207, 220, 234)
                    },
                )?;
            }
        }
        if let (Some(presenter), Some(frame)) = (&mut hdr_presenter, &hdr_frame) {
            let mut ui = vec![read_hdr_overlay(
                &mut canvas,
                if chrome.visible() {
                    Rect::new(0, 0, window_size.0, TOOLBAR_HEIGHT)
                } else {
                    reveal_rect(window_size.0)
                },
                window_size,
            )?];
            if let Some(panel) = stats_panel {
                ui.push(read_hdr_overlay(&mut canvas, panel, window_size)?);
            }
            let overlays: Vec<_> = ui
                .iter()
                .map(|ui| crate::hdr_present::Overlay {
                    rect: ui.rect,
                    width: ui.width,
                    height: ui.height,
                    stride: ui.width as usize * 4,
                    rgba: &ui.pixels,
                })
                .collect();
            canvas.present();
            let outcome = if let Some(surface) = &frame.surface {
                presenter.present_surface(
                    surface,
                    chrome.rect(window_size, size),
                    &overlays,
                    window_size,
                )?
            } else {
                let hdr = frame
                    .hdr
                    .as_ref()
                    .context("missing high-precision planes")?;
                presenter.present(
                    &crate::hdr_present::P010Frame {
                        width: frame.width,
                        height: frame.height,
                        y: &hdr.y,
                        uv: &hdr.uv,
                        y_stride: hdr.y_stride,
                        uv_stride: hdr.uv_stride,
                    },
                    chrome.rect(window_size, size),
                    &overlays,
                    window_size,
                )?
            };
            match outcome {
                crate::hdr_present::PresentOutcome::Presented { headroom } => {
                    if desktop.dynamic_range == protocol::DynamicRange::Hdr10 {
                        hdr_headroom = Some(headroom);
                    }
                    if hdr_frame_pending {
                        frames += 1;
                        total_frames += 1;
                        hdr_frame_pending = false;
                        if switch_ack
                            && frame.width == desktop.width
                            && frame.height == desktop.height
                        {
                            switching = false;
                            awaiting_initial = false;
                            switch_ack = false;
                        }
                    }
                }
                crate::hdr_present::PresentOutcome::DrawableUnavailable => (),
            }
        } else {
            canvas.present();
        }
        if let Some(decoded_at) = frame_decoded_at.take() {
            present_timer.presented(decoded_at);
        }
        if options
            .exit_after_frames
            .is_some_and(|limit| total_frames >= limit)
        {
            break;
        }
        last_present = Some(Instant::now());
    }
    present_timer.log();
    let _ = events.try_send(Event::ReleaseAll);
    // Disconnect also triggers host-side release of every held key/button.
    Ok(reconnect)
}

const TOOLBAR_HEIGHT: u32 = crate::fullscreen::HEIGHT;

/// Longest render-loop sleep when nothing wakes it: bounds how late polled
/// state (pipeline errors, toolbar hiding, probes) is noticed.
const IDLE_WAIT: Duration = Duration::from_millis(10);

/// SDL user event type that wakes the session render loop (decoded frame,
/// host update or session end); 0 until registered.
static WAKE_EVENT: AtomicU32 = AtomicU32::new(0);
/// At most one wake event is queued; the loop clears this before it looks for
/// new work, so a frame stored after that always queues another wake.
static WAKE_QUEUED: AtomicBool = AtomicBool::new(false);

fn wake_render_loop() {
    let kind = WAKE_EVENT.load(Ordering::Acquire);
    if kind == 0 || WAKE_QUEUED.swap(true, Ordering::AcqRel) {
        return;
    }
    // SAFETY: an all-zero SDL_Event is valid; only its type is set.
    let mut event: sdl2::sys::SDL_Event = unsafe { std::mem::zeroed() };
    event.type_ = kind;
    // SAFETY: SDL_PushEvent is thread-safe and copies the event.
    if unsafe { sdl2::sys::SDL_PushEvent(&mut event) } != 1 {
        WAKE_QUEUED.store(false, Ordering::Release);
    }
}

/// Registers the wake event (once per process) and routes decoder frame
/// notifications to it. False when SDL has no user event types left; the
/// loop then keeps its 1 ms poll.
fn register_render_wake() -> bool {
    if WAKE_EVENT.load(Ordering::Acquire) == 0 {
        // SAFETY: called on the SDL thread after SDL_Init.
        let kind = unsafe { sdl2::sys::SDL_RegisterEvents(1) };
        if kind == u32::MAX {
            return false;
        }
        WAKE_EVENT.store(kind, Ordering::Release);
    }
    media::set_presenter_wake(wake_render_loop);
    true
}

/// Decoder-ready → presented latency and render-loop cost, logged when the
/// window closes so loop scheduling changes can be compared (H9).
struct PresentTimer {
    started: Instant,
    cpu_start: Duration,
    iterations: u64,
    /// Microseconds from a frame being stored by the decoder to its present.
    samples: Vec<u32>,
    /// Microseconds from a frame being stored to the loop taking it.
    pickups: Vec<u32>,
}

impl PresentTimer {
    fn new() -> Self {
        Self {
            started: Instant::now(),
            cpu_start: thread_cpu_time(),
            iterations: 0,
            samples: Vec::new(),
            pickups: Vec::new(),
        }
    }
    fn picked_up(&mut self, decoded_at: Instant) {
        Self::sample(&mut self.pickups, decoded_at);
    }
    fn presented(&mut self, decoded_at: Instant) {
        Self::sample(&mut self.samples, decoded_at);
    }
    fn sample(samples: &mut Vec<u32>, since: Instant) {
        if samples.len() < 1 << 20 {
            samples.push(since.elapsed().as_micros().min(u32::MAX as u128) as u32);
        }
    }
    fn log(self) {
        let wall = self.started.elapsed().as_secs_f64().max(1e-3);
        let cpu = thread_cpu_time()
            .saturating_sub(self.cpu_start)
            .as_secs_f64();
        // Mean, p50, p95 and max in microseconds.
        let summary = |mut samples: Vec<u32>| {
            samples.sort_unstable();
            let at = |p: usize| {
                samples
                    .get((samples.len() * p / 100).min(samples.len().saturating_sub(1)))
                    .copied()
                    .unwrap_or(0)
            };
            let mean = samples.iter().map(|&us| u64::from(us)).sum::<u64>()
                / (samples.len() as u64).max(1);
            format!(
                "{mean}/{}/{}/{}",
                at(50),
                at(95),
                samples.last().copied().unwrap_or(0)
            )
        };
        tracing::info!(
            frames = self.samples.len(),
            ready_to_pickup_us = summary(self.pickups),
            ready_to_present_us = summary(self.samples),
            loop_iterations_per_s = (self.iterations as f64 / wall).round() as u64,
            loop_cpu_percent = format!("{:.1}", cpu * 100.0 / wall),
            "video presentation summary (mean/p50/p95/max)"
        );
    }
}

/// CPU time consumed by the calling thread (the render loop).
fn thread_cpu_time() -> Duration {
    let mut now = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: valid out-pointer; the clock is supported on Linux and macOS.
    if unsafe { libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &mut now) } != 0 {
        return Duration::ZERO;
    }
    Duration::new(now.tv_sec as u64, now.tv_nsec as u32)
}

fn display_mode_rect() -> Rect {
    Rect::new(12, 51, 172, 27)
}

fn reveal_rect(width: u32) -> Rect {
    Rect::new(
        width.saturating_sub(120) as i32 / 2,
        0,
        width.clamp(1, 120),
        crate::fullscreen::REVEAL_HEIGHT,
    )
}

#[derive(Default)]
struct WheelAccumulator {
    remainder: [f64; 2],
}

impl WheelAccumulator {
    fn events(
        &mut self,
        integer: (i32, i32),
        precise: (f32, f32),
        flipped: bool,
        position: Option<(f64, f64)>,
    ) -> Result<Vec<Event>> {
        let Some((x, y)) = position else {
            *self = Self::default();
            return Ok(Vec::new());
        };
        let mut steps = [0_i32; 2];
        for (index, (integer, precise)) in [(integer.0, precise.0), (integer.1, precise.1)]
            .into_iter()
            .enumerate()
        {
            // Precise deltas replace, not supplement, SDL's integer values.
            // Older backends may supply only the integer field.
            let delta = if precise.is_finite() && precise != 0.0 {
                f64::from(precise)
            } else {
                f64::from(integer)
            };
            ensure!(
                delta.abs() <= 200.0,
                "wheel event exceeds bounded input budget"
            );
            let total = self.remainder[index] + if flipped { -delta } else { delta };
            // SDL supplies f32 values: tolerate rounding at a whole-notch
            // boundary without repeatedly discarding small touchpad deltas.
            steps[index] = (total + total.signum() * 1e-6).trunc() as i32;
            self.remainder[index] = total - f64::from(steps[index]);
            if self.remainder[index].abs() < 1e-6 {
                self.remainder[index] = 0.0;
            }
        }
        if steps == [0, 0] {
            return Ok(Vec::new());
        }
        let mut events = vec![Event::Motion { x, y }];
        while steps != [0, 0] {
            let x = steps[0].clamp(-20, 20);
            let y = steps[1].clamp(-20, 20);
            events.push(Event::Scroll { x, y });
            steps[0] -= x;
            steps[1] -= y;
        }
        Ok(events)
    }
}

struct HdrOverlayPixels {
    rect: Rect,
    width: u32,
    height: u32,
    pixels: Vec<u8>,
}

fn read_hdr_overlay(
    canvas: &mut sdl2::render::Canvas<sdl2::video::Window>,
    rect: Rect,
    window: (u32, u32),
) -> Result<HdrOverlayPixels> {
    let rect = rect
        .intersection(Rect::new(0, 0, window.0, window.1))
        .context("HDR overlay is outside the window")?;
    let (output_width, output_height) = canvas.output_size().map_err(anyhow::Error::msg)?;
    let sx = f64::from(output_width) / f64::from(window.0.max(1));
    let sy = f64::from(output_height) / f64::from(window.1.max(1));
    let x = (f64::from(rect.x()) * sx).floor() as i32;
    let y = (f64::from(rect.y()) * sy).floor() as i32;
    let right = (f64::from(rect.right()) * sx)
        .ceil()
        .min(f64::from(output_width)) as i32;
    let bottom = (f64::from(rect.bottom()) * sy)
        .ceil()
        .min(f64::from(output_height)) as i32;
    let pixels_rect = Rect::new(x, y, (right - x).max(1) as u32, (bottom - y).max(1) as u32);
    // ReadPixels uses physical coordinates but clips to the renderer viewport.
    // Remove logical-size letterboxing during readback so fractional scaling
    // cannot leave a clipped, uninitialized edge in SDL's returned buffer.
    let logical = canvas.logical_size();
    canvas.set_logical_size(0, 0)?;
    let pixels = canvas.read_pixels(pixels_rect, PixelFormatEnum::RGBA32);
    canvas.set_logical_size(logical.0, logical.1)?;
    let pixels = pixels.map_err(anyhow::Error::msg)?;
    Ok(HdrOverlayPixels {
        rect,
        width: pixels_rect.width(),
        height: pixels_rect.height(),
        pixels,
    })
}

fn configuration_matches(desktop: &Desktop, requested: Option<(u32, u32, u32)>) -> bool {
    requested.is_none_or(|(width, fps, bitrate)| {
        let width = if width == 0 {
            desktop.native_width
        } else {
            width
        } / 2
            * 2;
        desktop.width == width && desktop.fps == fps && desktop.bitrate == bitrate
    })
}

fn video_generation_matches(start_group: u64, frame_group: Option<u64>) -> bool {
    // A legacy host has no generation barrier. New hosts require positively
    // attributed frames; unknown PTS/group metadata must never unlock input.
    start_group == 0 || frame_group.is_some_and(|group| group >= start_group)
}

fn keyboard_capture_rect() -> Rect {
    Rect::new(194, 51, 120, 27)
}

fn toggle_keyboard_capture(
    capture: &mut crate::keyboard_capture::Capture,
    window: Option<sdl2::video::Window>,
) -> String {
    if capture.active() {
        capture.release();
        "Keyboard released locally".into()
    } else if window.is_some_and(|window| capture.bind(window)) {
        "Keyboard capture requested; Ctrl+Alt+K releases, Ctrl+Alt+Q disconnects".into()
    } else {
        "Keyboard capture unavailable on this window/backend".into()
    }
}

fn toolbar_rect(index: usize, width: u32) -> Rect {
    // Fixed, legible controls with a flexible gap between session and window actions.
    // Visual order differs from action IDs to preserve existing button semantics.
    let widths = [88, 76, 78, 76, 88, 88, 98, 54];
    let left = match index {
        0..=3 => 12 + widths[..index].iter().sum::<u32>() + index as u32 * 4,
        7 => width.saturating_sub(12 + 54 + 88 + 88 + 98 + 12),
        4 => width.saturating_sub(12 + 88 + 88 + 98 + 8),
        5 => width.saturating_sub(12 + 88 + 98 + 4),
        _ => width.saturating_sub(12 + 98),
    };
    Rect::new(left as i32, 10, widths[index], 36)
}

fn toolbar_hit(x: i32, y: i32, width: u32) -> Option<usize> {
    (0..8).find(|&index| toolbar_rect(index, width).contains_point((x, y)))
}

struct SessionFonts<'ttf> {
    ttf: &'ttf sdl2::ttf::Sdl2TtfContext,
    fonts: std::collections::HashMap<u16, sdl2::ttf::Font<'ttf, 'static>>,
}

impl SessionFonts<'_> {
    fn get(&mut self, points: u16) -> Result<&sdl2::ttf::Font<'_, 'static>> {
        if !self.fonts.contains_key(&points) {
            if self.fonts.len() >= 16 {
                self.fonts.clear();
            }
            self.fonts.insert(
                points,
                self.ttf
                    .load_font(crate::launcher::font_path()?, points)
                    .map_err(anyhow::Error::msg)?,
            );
        }
        Ok(self.fonts.get(&points).unwrap())
    }
}

fn session_text<'a>(
    canvas: &mut sdl2::render::Canvas<sdl2::video::Window>,
    creator: &'a sdl2::render::TextureCreator<sdl2::video::WindowContext>,
    fonts: &mut SessionFonts<'_>,
    cache: &mut std::collections::HashMap<String, sdl2::render::Texture<'a>>,
    text: &str,
    rect: Rect,
    color: sdl2::pixels::Color,
) -> Result<()> {
    if text.is_empty() {
        return Ok(());
    }
    let logical = canvas.logical_size();
    let logical = if logical.0 == 0 {
        canvas.window().size()
    } else {
        logical
    };
    let output = canvas.output_size().map_err(anyhow::Error::msg)?;
    let scale =
        (output.0 as f64 / logical.0.max(1) as f64).min(output.1 as f64 / logical.1.max(1) as f64);
    let points = (14.0 * scale).round().clamp(1.0, 256.0) as u16;
    let key = format!("{points}:{text}");
    if !cache.contains_key(&key) {
        if cache.len() >= 128 {
            cache.clear();
        }
        let font = fonts.get(points)?;
        let surface = font.render(text).blended(sdl2::pixels::Color::WHITE)?;
        cache.insert(key.clone(), creator.create_texture_from_surface(&surface)?);
    }
    let texture = cache.get_mut(&key).unwrap();
    texture.set_color_mod(color.r, color.g, color.b);
    let query = texture.query();
    let destination = text_pixel_rect(rect, logical, output);
    let width = query.width.min(destination.width());
    let height = query.height.min(destination.height());
    if width == 0 || height == 0 {
        return Ok(());
    }
    // Rasterize at the actual drawable DPI and copy 1:1 onto pixel-aligned
    // coordinates. Do not downsample a fixed 2x font or stretch at fractional DPI.
    let saved_logical = canvas.logical_size();
    canvas.set_logical_size(0, 0)?;
    let result = canvas.copy(
        texture,
        Rect::new(0, 0, width, height),
        Rect::new(destination.x(), destination.y(), width, height),
    );
    canvas.set_logical_size(saved_logical.0, saved_logical.1)?;
    result.map_err(anyhow::Error::msg)
}

fn text_pixel_rect(rect: Rect, logical: (u32, u32), output: (u32, u32)) -> Rect {
    let scale =
        (output.0 as f64 / logical.0.max(1) as f64).min(output.1 as f64 / logical.1.max(1) as f64);
    let offset_x = (output.0 as f64 - logical.0 as f64 * scale) / 2.0;
    let offset_y = (output.1 as f64 - logical.1 as f64 * scale) / 2.0;
    Rect::new(
        (offset_x + rect.x() as f64 * scale).round() as i32,
        (offset_y + rect.y() as f64 * scale).round() as i32,
        (rect.width() as f64 * scale).floor() as u32,
        (rect.height() as f64 * scale).floor() as u32,
    )
}

fn check_network(network: &mut Network, runtime: &Runtime) -> Result<()> {
    if network.0.is_finished() {
        runtime.block_on(&mut network.0)??;
        anyhow::bail!("network session ended");
    }
    Ok(())
}

/// Set when zero-copy presentation failed in this process; later sessions
/// use the CPU NV12 path instead of retrying a broken driver path.
#[cfg(target_os = "linux")]
static ZERO_COPY_FAILED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Zero-copy presenter bound to the session window's GL context: CUDA→GL on
/// an NVIDIA context, or VA-API dma-buf→EGLImage on an EGL context (Wayland,
/// or X11 with `TELEPORT_X11_EGL=1`), with the VA-API decoder opened on that
/// EGL display's own render node. GLX contexts never get the VA-API path.
/// Other windows (monitor windows) create their own renderer contexts, so
/// the session context is made current explicitly before every GL use.
#[cfg(target_os = "linux")]
struct ZeroCopy {
    presenter: Option<crate::ffmpeg::GlPresenter>,
    decoder: String,
    interop: crate::ffmpeg::GpuInterop,
    window: *mut sdl2::sys::SDL_Window,
    context: sdl2::sys::SDL_GLContext,
}

#[cfg(target_os = "linux")]
impl ZeroCopy {
    /// Must run right after the session renderer is created, while its GL
    /// context is current.
    fn new(canvas: &sdl2::render::Canvas<sdl2::video::Window>) -> Option<Self> {
        let context = unsafe { sdl2::sys::SDL_GL_GetCurrentContext() };
        if context.is_null() {
            return None;
        }
        let get = sdl2::sys::SDL_GL_GetProcAddress;
        let presenter = crate::ffmpeg::GlPresenter::cuda(get)
            .or_else(|| crate::ffmpeg::GlPresenter::vaapi(get))?;
        Some(Self {
            decoder: presenter.decoder_name(),
            interop: presenter.interop(),
            presenter: Some(presenter),
            window: canvas.window().raw(),
            context,
        })
    }

    /// The decoder whose frames this presenter draws ("cuda" or
    /// "vaapi@<render node>").
    fn decoder_name(&self) -> String {
        self.decoder.clone()
    }

    fn pixel_path(&self) -> &'static str {
        match self.interop {
            crate::ffmpeg::GpuInterop::Cuda => {
                "CUDA surface / OpenGL zero-copy (no video CPU copy)"
            }
            crate::ffmpeg::GpuInterop::Vaapi => {
                "VA-API DMA-BUF / EGLImage zero-copy (no video CPU copy)"
            }
        }
    }

    /// Draw `frame` at `rect` (logical pixels), after SDL has flushed what it
    /// queued so far; SDL then draws the toolbar and overlays on top.
    fn draw(
        &mut self,
        canvas: &mut sdl2::render::Canvas<sdl2::video::Window>,
        frame: &crate::ffmpeg::GpuFrame,
        rect: Rect,
    ) -> Result<()> {
        let logical = canvas.window().size();
        let output = canvas.output_size().map_err(anyhow::Error::msg)?;
        let scale_x = output.0 as f64 / logical.0.max(1) as f64;
        let scale_y = output.1 as f64 / logical.1.max(1) as f64;
        unsafe {
            sdl2::sys::SDL_RenderFlush(canvas.raw());
            ensure!(
                sdl2::sys::SDL_GL_MakeCurrent(self.window, self.context) == 0,
                "session GL context unavailable"
            );
        }
        self.presenter.as_mut().context("presenter released")?.draw(
            frame,
            (
                (rect.x() as f64 * scale_x).round() as i32,
                (rect.y() as f64 * scale_y).round() as i32,
                (rect.width() as f64 * scale_x).round() as u32,
                (rect.height() as f64 * scale_y).round() as u32,
            ),
            output.1,
        )
    }
}

#[cfg(target_os = "linux")]
impl Drop for ZeroCopy {
    fn drop(&mut self) {
        // Release GL objects in the context that owns them.
        unsafe {
            sdl2::sys::SDL_GL_MakeCurrent(self.window, self.context);
        }
        self.presenter.take();
    }
}

/// SDL relative mouse mode (pointer lock) for games. Never traps the user:
/// released on focus loss, by the same shortcut, and on drop.
#[derive(Default)]
struct PointerLock {
    active: bool,
}

impl PointerLock {
    fn active(&self) -> bool {
        self.active
    }

    /// Returns whether the state changed.
    fn set(&mut self, on: bool) -> bool {
        if on == self.active {
            return false;
        }
        let bool = if on {
            sdl2::sys::SDL_bool::SDL_TRUE
        } else {
            sdl2::sys::SDL_bool::SDL_FALSE
        };
        // Safety: plain SDL state call on the thread that owns the video
        // subsystem (the session event loop).
        let applied = unsafe { sdl2::sys::SDL_SetRelativeMouseMode(bool) } == 0;
        self.active = on && applied;
        applied || !on
    }
}

impl Drop for PointerLock {
    fn drop(&mut self) {
        self.set(false);
    }
}

#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
enum CursorState {
    /// SDL's default arrow (toolbar, letterbox, before any host shape).
    #[default]
    System,
    Remote,
    Hidden,
}

/// Host pointer drawn by the local window system from `Update::Cursor`, so it
/// moves with no network or encode latency. Shown only over remote video;
/// local UI keeps the default arrow. Pointer lock hides it independently.
#[derive(Default)]
struct RemoteCursor {
    /// Latest host shape: `Some(None)` is a hidden host pointer.
    shape: Option<Option<sdl2::mouse::Cursor>>,
    applied: CursorState,
    /// A new shape must be applied even if the state is unchanged.
    stale: bool,
}

impl RemoteCursor {
    fn set_shape(&mut self, image: Option<&protocol::CursorImage>) {
        let cursor = match image.map(color_cursor).transpose() {
            Ok(cursor) => cursor,
            Err(error) => {
                // Better the local arrow than no pointer at all.
                tracing::warn!(%error, "could not create the host cursor; using the local arrow");
                self.shape = None;
                self.stale = true;
                return;
            }
        };
        let previous = self.shape.replace(cursor);
        self.stale = true;
        // Swap a visible host cursor before freeing the old one, so SDL never
        // falls back to its arrow in between.
        if self.applied == CursorState::Remote {
            self.apply(true, false);
        }
        drop(previous);
    }

    fn desired(&self, over_video: bool) -> CursorState {
        match (over_video, &self.shape) {
            (true, Some(Some(_))) => CursorState::Remote,
            (true, Some(None)) => CursorState::Hidden,
            _ => CursorState::System,
        }
    }

    /// `over_video`: the pointer is over remote video in a session window.
    fn apply(&mut self, over_video: bool, locked: bool) {
        // Relative mode hides the pointer itself and restores it on release.
        if locked {
            return;
        }
        let desired = self.desired(over_video);
        if desired == self.applied && !self.stale {
            return;
        }
        self.stale = false;
        self.applied = desired;
        // Safety: plain SDL cursor calls on the thread that owns the video
        // subsystem (the session event loop); cursors outlive their use.
        unsafe {
            match desired {
                CursorState::Remote => {
                    if let Some(Some(cursor)) = &self.shape {
                        cursor.set();
                    }
                    sdl2::sys::SDL_ShowCursor(sdl2::sys::SDL_ENABLE as i32);
                }
                CursorState::Hidden => {
                    sdl2::sys::SDL_ShowCursor(sdl2::sys::SDL_DISABLE as i32);
                }
                CursorState::System => {
                    sdl2::sys::SDL_SetCursor(sdl2::sys::SDL_GetDefaultCursor());
                    sdl2::sys::SDL_ShowCursor(sdl2::sys::SDL_ENABLE as i32);
                }
            }
        }
    }
}

impl Drop for RemoteCursor {
    fn drop(&mut self) {
        // Leave the launcher (or a reconnect) with a normal visible arrow
        // before the host cursor is freed.
        if self.applied != CursorState::System {
            self.shape = None;
            self.stale = true;
            self.apply(false, false);
        }
    }
}

fn color_cursor(image: &protocol::CursorImage) -> Result<sdl2::mouse::Cursor> {
    image.validate()?;
    // SDL_CreateColorCursor copies the pixels; the surface is temporary.
    let mut pixels = image.rgba.clone();
    let width = u32::from(image.width);
    let surface = sdl2::surface::Surface::from_data(
        &mut pixels,
        width,
        u32::from(image.height),
        width * 4,
        sdl2::pixels::PixelFormatEnum::RGBA32,
    )
    .map_err(anyhow::Error::msg)?;
    sdl2::mouse::Cursor::from_surface(&surface, image.hot_x.into(), image.hot_y.into())
        .map_err(anyhow::Error::msg)
}

/// Send coalesced pointer-lock movement, split into protocol-sized steps.
fn flush_relative(events: &mpsc::Sender<Event>, relative: &mut (i32, i32)) -> Result<()> {
    let max = protocol::MAX_RELATIVE_MOTION as i32;
    while *relative != (0, 0) {
        let dx = relative.0.clamp(-max, max);
        let dy = relative.1.clamp(-max, max);
        send(events, Event::RelativeMotion { dx, dy })?;
        relative.0 -= dx;
        relative.1 -= dy;
    }
    Ok(())
}

fn send(events: &mpsc::Sender<Event>, event: Event) -> Result<()> {
    event.validate()?;
    match events.try_send(event) {
        Ok(()) => Ok(()),
        // A network stall must not end the session over pointer motion or a
        // wheel burst; those are superseded or harmless to lose.
        Err(mpsc::error::TrySendError::Full(event)) if event.is_lossy() => {
            tracing::debug!(?event, "input queue full; dropping superseded event");
            Ok(())
        }
        Err(error) => Err(anyhow::anyhow!(
            "input queue full or disconnected; ending session rather than losing key events: {error}"
        )),
    }
}
fn button(button: MouseButton) -> Option<u8> {
    match button {
        MouseButton::Left => Some(1),
        MouseButton::Middle => Some(2),
        MouseButton::Right => Some(3),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn large_pointer_lock_movement_is_split_without_loss() {
        let (sender, mut receiver) = mpsc::channel(16);
        let max = protocol::MAX_RELATIVE_MOTION as i32;
        let mut relative = (2 * max + 7, -max - 1);
        flush_relative(&sender, &mut relative).unwrap();
        assert_eq!(relative, (0, 0));
        let (mut dx, mut dy) = (0, 0);
        while let Ok(event) = receiver.try_recv() {
            let Event::RelativeMotion { dx: x, dy: y } = event else {
                panic!("unexpected {event:?}")
            };
            assert!(event.validate().is_ok());
            dx += x;
            dy += y;
        }
        assert_eq!((dx, dy), (2 * max + 7, -max - 1));
    }

    #[test]
    fn session_text_maps_to_native_pixels_at_integer_and_fractional_dpi() {
        let rect = Rect::new(10, 20, 100, 20);
        assert_eq!(text_pixel_rect(rect, (800, 600), (800, 600)), rect);
        assert_eq!(
            text_pixel_rect(rect, (800, 600), (1600, 1200)),
            Rect::new(20, 40, 200, 40)
        );
        assert_eq!(
            text_pixel_rect(rect, (800, 600), (1000, 750)),
            Rect::new(13, 25, 125, 25)
        );
        assert_eq!(
            text_pixel_rect(rect, (800, 600), (1000, 600)),
            Rect::new(110, 20, 100, 20)
        );
        assert!(!keyboard_capture_rect().has_intersection(display_mode_rect()));
    }

    #[test]
    fn wheel_notches_preserve_direction_amount_and_motion_order() -> Result<()> {
        let mut wheel = WheelAccumulator::default();
        for (integer, precise, flipped, expected) in [
            ((0, 1), (0.0, 1.0), false, (0, 1)),
            ((1, -1), (1.0, -1.0), true, (-1, 1)),
            ((45, -23), (0.0, 0.0), false, (45, -23)),
        ] {
            let events = wheel.events(integer, precise, flipped, Some((0.3, 0.7)))?;
            assert!(matches!(events[0], Event::Motion { x, y } if x == 0.3 && y == 0.7));
            let mut total = (0, 0);
            for event in &events[1..] {
                event.validate()?;
                if let Event::Scroll { x, y } = event {
                    total.0 += x;
                    total.1 += y;
                } else {
                    panic!("unexpected wheel translation");
                }
            }
            assert_eq!(total, expected);
        }
        Ok(())
    }

    #[test]
    fn wheel_fractions_accumulate_without_double_counting_and_reset_outside_desktop() -> Result<()>
    {
        let mut wheel = WheelAccumulator::default();
        for _ in 0..3 {
            // The integer field must not get counted in addition to precise.
            assert!(
                wheel
                    .events((0, 1), (0.0, 0.25), false, Some((0.5, 0.5)))?
                    .is_empty()
            );
        }
        let events = wheel.events((0, 1), (0.0, 0.25), false, Some((0.5, 0.5)))?;
        assert!(matches!(events[1], Event::Scroll { x: 0, y: 1 }));
        assert!(
            wheel
                .events((0, 0), (0.0, 0.75), false, Some((0.5, 0.5)))?
                .is_empty()
        );
        assert!(wheel.events((0, 1), (0.0, 1.0), false, None)?.is_empty());
        assert!(
            wheel
                .events((0, 0), (0.0, 0.25), false, Some((0.5, 0.5)))?
                .is_empty()
        );
        assert!(
            wheel
                .events((i32::MIN, 0), (0.0, 0.0), true, Some((0.5, 0.5)))
                .is_err()
        );
        Ok(())
    }

    #[test]
    #[ignore = "requires an isolated X11 display and SDL OpenGL renderer"]
    fn nv12_renderer_upload_preserves_color_and_rejects_invalid_planes() -> Result<()> {
        let sdl = sdl2::init().map_err(anyhow::Error::msg)?;
        let video = sdl.video().map_err(anyhow::Error::msg)?;
        let window = video
            .window("Teleport NV12 test", 16, 16)
            .hidden()
            .build()?;
        let mut canvas = window.into_canvas().accelerated().build()?;
        ensure!(
            supports_nv12(&canvas),
            "test renderer must expose native NV12 textures"
        );
        let creator = canvas.texture_creator();
        set_nv12_conversion();
        let mut texture = creator.create_texture_streaming(PixelFormatEnum::NV12, 16, 16)?;
        let mut frame = media::Nv12Image {
            y: vec![63; 32 * 15 + 16],
            uv: [102, 240].repeat((32 * 7 + 16) / 2),
            y_stride: 32,
            uv_stride: 32,
        };
        update_nv12(&mut texture, &frame, 16, 16)?;
        canvas
            .copy(&texture, None, None)
            .map_err(anyhow::Error::msg)?;
        let pixels = canvas
            .read_pixels(None, PixelFormatEnum::RGB24)
            .map_err(anyhow::Error::msg)?;
        for pixel in pixels.chunks_exact(3) {
            ensure!(
                pixel[0] > 240 && pixel[1] < 15 && pixel[2] < 15,
                "BT709 red rendered incorrectly: {pixel:?}"
            );
        }
        // Borrowing the GL diagnostic context must not destroy it.
        let _ = graphics_renderer(&video);
        canvas
            .copy(&texture, None, None)
            .map_err(anyhow::Error::msg)?;
        frame.uv.pop();
        assert!(update_nv12(&mut texture, &frame, 16, 16).is_err());
        frame.uv.push(240);
        frame.y_stride = usize::MAX;
        assert!(update_nv12(&mut texture, &frame, 16, 16).is_err());
        assert!(update_nv12(&mut texture, &frame, 15, 16).is_err());
        Ok(())
    }
    #[test]
    fn letterbox_coordinates() {
        let mut chrome = crate::fullscreen::Chrome::new();
        chrome.set_fullscreen(true);
        assert!(chrome.pointer(0, 0, (1000, 1000), (1920, 1080)).is_none());
        let (x, y) = chrome
            .pointer(500, 500, (1000, 1000), (1920, 1080))
            .unwrap();
        assert!((x - 0.5).abs() < 0.01 && (y - 0.5).abs() < 0.01);
    }

    #[test]
    fn toolbar_controls_fit_and_hit_at_all_supported_widths() {
        for width in [700, 1024, 1280, 2560] {
            for index in 0..8 {
                let rect = toolbar_rect(index, width);
                assert!(rect.x() >= 0 && rect.right() <= width as i32);
                assert_eq!(
                    toolbar_hit(rect.center().x(), rect.center().y(), width),
                    Some(index)
                );
                for other in (index + 1)..8 {
                    assert!(!rect.has_intersection(toolbar_rect(other, width)));
                }
            }
            assert_eq!(toolbar_hit(2, 60, width), None);
        }
    }

    #[test]
    fn session_chrome_never_maps_to_remote_desktop() {
        let chrome = crate::fullscreen::Chrome::new();
        for y in 0..TOOLBAR_HEIGHT as i32 {
            assert!(chrome.pointer(350, y, (700, 600), (1920, 1080)).is_none());
        }
        let rect = chrome.rect((700, 600), (1920, 1080));
        let (x, y) = chrome
            .pointer(
                rect.center().x(),
                rect.center().y(),
                (700, 600),
                (1920, 1080),
            )
            .unwrap();
        assert!((x - 0.5).abs() < 0.01 && (y - 0.5).abs() < 0.01);
    }

    #[test]
    fn video_configuration_ack_matches_native_even_width_and_quality() {
        let desktop: Desktop = serde_json::from_value(serde_json::json!({
            "version": protocol::VERSION, "width": 1920, "height": 1080,
            "fps": 60, "source": "test", "monitors": [], "active_monitor": 0,
            "audio": false, "clipboard": false, "native_width": 1921,
            "native_height": 1080, "bitrate": 20000
        }))
        .unwrap();
        assert_eq!(desktop.codec, protocol::VideoCodec::H264);
        assert!(desktop.codecs.is_empty());
        assert!(configuration_matches(&desktop, None));
        assert!(configuration_matches(&desktop, Some((0, 60, 20000))));
        assert!(configuration_matches(&desktop, Some((1921, 60, 20000))));
        assert!(!configuration_matches(&desktop, Some((1280, 60, 20000))));
        assert!(!configuration_matches(&desktop, Some((0, 120, 20000))));
        assert!(!configuration_matches(&desktop, Some((0, 60, 8000))));
    }

    #[test]
    fn video_generation_barrier_rejects_old_and_unattributed_frames() {
        assert!(video_generation_matches(0, None));
        assert!(video_generation_matches(0, Some(0)));
        assert!(!video_generation_matches(42, None));
        assert!(!video_generation_matches(42, Some(41)));
        assert!(video_generation_matches(42, Some(42)));
        assert!(video_generation_matches(42, Some(100)));
        assert!(!video_generation_matches(u64::MAX, Some(u64::MAX - 1)));
        assert!(video_generation_matches(u64::MAX, Some(u64::MAX)));
    }

    #[test]
    fn codec_cli_defaults_to_h264_and_accepts_explicit_h265() {
        use clap::Parser;
        #[derive(Parser)]
        struct Arguments {
            #[command(flatten)]
            options: Options,
        }
        let base = ["test", "localhost:4443", "--pairing-file", "unused.json"];
        let parsed = Arguments::try_parse_from(base).unwrap();
        assert_eq!(parsed.options.codec, protocol::VideoCodec::H264);
        assert!(!parsed.options.forward_ssh_agent);
        assert!(
            Arguments::try_parse_from(
                base.into_iter()
                    .chain(["--forward-ssh-agent", "--reconnect"])
            )
            .is_err()
        );
        let parsed =
            Arguments::try_parse_from(base.into_iter().chain(["--codec", "h265"])).unwrap();
        assert_eq!(parsed.options.codec, protocol::VideoCodec::H265);
        assert!(Arguments::try_parse_from(base.into_iter().chain(["--codec", "av1"])).is_err());
    }

    #[test]
    fn route_defaults_keep_existing_behaviour_and_relay_routes_need_a_relay() {
        use crate::relay::Route;
        use clap::Parser;
        #[derive(Parser)]
        struct Arguments {
            #[command(flatten)]
            options: Options,
        }
        let parse = |extra: &[&str]| {
            Arguments::try_parse_from(
                ["test", "localhost:4443", "--pairing-file", "unused.json"]
                    .into_iter()
                    .chain(extra.iter().copied()),
            )
            .unwrap()
            .options
        };
        let relay = ["--relay", "moqt://relay.example:4444"];
        // Skip if a developer's environment supplies a relay.
        if std::env::var_os("TELEPORT_RELAY_URL").is_none() {
            assert_eq!(parse(&[]).effective_route().unwrap(), Route::Direct);
            assert!(parse(&["--route", "auto"]).effective_route().is_err());
            assert!(parse(&["--route", "relay"]).effective_route().is_err());
            assert_eq!(parse(&[]).connect_timeout(), CONNECT_TIMEOUT);
        }
        assert_eq!(parse(&relay).effective_route().unwrap(), Route::Relay);
        let auto = parse(&[relay[0], relay[1], "--route", "auto"]);
        assert_eq!(auto.effective_route().unwrap(), Route::Auto);
        assert_eq!(auto.connect_timeout(), CONNECT_TIMEOUT + DIRECT_ATTEMPT);
        let direct = parse(&[relay[0], relay[1], "--route", "direct"]);
        assert_eq!(direct.effective_route().unwrap(), Route::Direct);
        // Auto falls back only on the marked transport failure.
        let unreachable = anyhow::anyhow!("timeout").context(DirectUnreachable);
        assert!(unreachable.downcast_ref::<DirectUnreachable>().is_some());
        assert!(
            anyhow::anyhow!("host rejected")
                .downcast_ref::<DirectUnreachable>()
                .is_none()
        );
    }
}
