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
    time::{Duration, Instant},
};
use tokio::{runtime::Runtime, sync::mpsc};

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

struct Link {
    ssh_agent: Option<crate::ssh_agent::Task>,
    session: moq_net::Session,
    _origin: moq_net::origin::Producer,
    _broadcast: moq_net::broadcast::Producer,
    input: moq_net::track::Producer,
    video: moq_net::track::Subscriber,
    desktop: Desktop,
    updates: moq_net::track::Subscriber,
    audio: Option<moq_net::track::Subscriber>,
}

async fn connect(options: &Options) -> Result<Link> {
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
    config.tls.fingerprint = vec![pairing.fingerprint];
    let outgoing = moq_net::Origin::random().produce();
    let incoming = moq_net::Origin::random().produce();
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
    let agent_responses = if options.forward_ssh_agent {
        Some(crate::ssh_agent::create_track(
            &mut broadcast,
            crate::ssh_agent::RESPONSE_TRACK,
        )?)
    } else {
        None
    };
    let client = config
        .init()?
        .with_publisher(&outgoing)
        .with_subscriber(incoming.clone());
    tracing::info!(address = %options.address, "connecting with pinned TLS certificate");
    let session = client
        .connect(url)
        .await
        .context("QUIC connection failed; check address, pairing file and UDP firewall")?;
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
                .with_group_start(0),
        )
        .await?;
    let audio = if desktop.audio {
        Some(
            remote
                .track("opus")?
                .subscribe(media::video_subscription())
                .await?,
        )
    } else {
        None
    };
    Ok(Link {
        ssh_agent,
        session,
        _origin: outgoing,
        _broadcast: broadcast,
        input,
        video,
        desktop,
        updates,
        audio,
    })
}

async fn send_input(
    mut track: moq_net::track::Producer,
    mut events: mpsc::Receiver<Event>,
) -> Result<()> {
    let mut sequence = 0u64;
    let mut group = track.append_group()?;
    let mut heartbeat = tokio::time::interval(Duration::from_millis(500));
    loop {
        let event = tokio::select! {
            event = events.recv() => match event { Some(event) => event, None => break },
            _ = heartbeat.tick() => Event::Ping,
        };
        if sequence > 0 && sequence.is_multiple_of(256) {
            group.finish()?;
            group = track.append_group()?;
        }
        group.write_frame(
            moq_net::Timestamp::now(),
            serde_json::to_vec(&Input { sequence, event })?,
        )?;
        sequence += 1;
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
                        tokio::time::timeout(Duration::from_secs(15), connect(&options)).await
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
                tokio::time::timeout(Duration::from_secs(15), connect(&options))
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
                        retry_at = Instant::now() + Duration::from_secs(3);
                    } else {
                        retry_at = Instant::now() + Duration::from_secs(86400);
                    }
                }
            }
        }
        canvas.set_draw_color(sdl2::pixels::Color::RGB(18, 23, 32));
        canvas.clear();
        for (row, text) in [
            format!("Connecting to {}", options.address),
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
        let update = serde_json::from_slice(&bytes)?;
        sender.send(update).await.context("session window closed")?;
    }
}

fn run_session(options: &Options, runtime: &Runtime, link: Link) -> Result<bool> {
    tracing::info!(width = link.desktop.width, height = link.desktop.height, source = %link.desktop.source, "desktop connected; Ctrl+Alt+Q quits locally");
    let stream_stats = std::sync::Arc::new(crate::stats::StreamStats::default());
    let (pipeline, source, image) = media::decoder_with_stats(
        options.software_decoder,
        stream_stats.clone(),
        media::VideoFormat {
            codec: link.desktop.codec,
            dynamic_range: link.desktop.dynamic_range,
        },
    )?;
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
    let mut network = Network(runtime.spawn(async move {
        let mut ssh_agent = link.ssh_agent;
        let _origin = link._origin;
        let _broadcast = link._broadcast;
        tokio::select! {
            result = crate::ssh_agent::closed(&mut ssh_agent) => result,
            result = media::receive_video_with_stats(link.video, source, feedback, network_stats) => result,
            result = send_input(link.input, receiver) => result,
            result = receive_updates(link.updates, updates_sender) => result,
            result = async {
                if let (Some(track), Some(source)) = (link.audio, audio_source)
                    && let Err(error) = crate::audio::receive(track, source).await {
                        tracing::warn!(%error, "remote audio stopped; desktop remains connected");
                        let _ = audio_notices.send(Update::Notice { text: "Audio stopped; reconnect to retry audio".into() }).await;
                }
                std::future::pending::<Result<()>>().await
            } => result,
            error = link.session.closed() => anyhow::bail!("connection closed: {error}"),
        }
    }));
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
        let mut switched = false;
        let mut clipboard_ok = false;
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
                    Update::Notice { text } => tracing::info!(%text, "host notice"),
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
                            .hdr
                            .as_ref()
                            .is_some_and(|hdr| !hdr.y.is_empty() && !hdr.uv.is_empty()),
                    "empty decoded frame"
                );
                protocol::validate_video_size(frame.width, frame.height)?;
                count += 1;
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
                    ] {
                        runtime.block_on(events.send(event))?;
                    }
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
        tracing::info!(
            frames = count,
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
    let ttf = sdl2::ttf::init().map_err(anyhow::Error::msg)?;
    let font = ttf
        .load_font(crate::launcher::font_path()?, 28)
        .map_err(anyhow::Error::msg)?;
    let mut canvas = if options.software_renderer {
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
    let creator = canvas.texture_creator();
    let mut hdr_presenter = if desktop.dynamic_range == protocol::DynamicRange::Hdr10 {
        Some(crate::hdr_present::HdrPresenter::new(canvas.window())?)
    } else {
        None
    };
    let mut hdr_frame: Option<media::Image> = None;
    let mut hdr_frame_pending = false;
    let mut hdr_headroom: Option<f64> = None;
    let mut text_cache = std::collections::HashMap::new();
    let mut texture = None;
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
    let mut probe_id = 0_u64;
    let mut pending_probe: Option<(u64, Instant)> = None;
    let mut last_probe = Instant::now() - Duration::from_secs(1);
    let mut rtt: Option<Duration> = None;
    let mut decoded_age = Duration::ZERO;
    let mut stats_lines = Vec::new();
    let mut host_encode_us = 0;
    let mut host_bitrate = desktop.bitrate;
    let mut host_encoder = "Waiting / unavailable".to_owned();
    'running: loop {
        let mut redraw = first_draw;
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
                _ => (),
            }
        }
        let mut motion = None;
        let pointer_in_toolbar = event_pump.mouse_state().y() < TOOLBAR_HEIGHT as i32;
        for event in event_pump.poll_iter() {
            redraw = true;
            // Both edges stay local, so the remote never receives a stray F8 release.
            if matches!(
                event,
                SdlEvent::KeyDown {
                    keycode: Some(Keycode::F8),
                    ..
                } | SdlEvent::KeyUp {
                    keycode: Some(Keycode::F8),
                    ..
                }
            ) {
                if matches!(event, SdlEvent::KeyDown { repeat: false, .. }) {
                    show_stats = !show_stats;
                }
                continue;
            }
            if pointer_in_toolbar && matches!(event, SdlEvent::MouseWheel { .. }) {
                continue;
            }
            if matches!(event, SdlEvent::KeyDown { keycode: Some(Keycode::Q), keymod, .. } if keymod.intersects(Mod::LCTRLMOD | Mod::RCTRLMOD) && keymod.intersects(Mod::LALTMOD | Mod::RALTMOD))
            {
                break 'running;
            }
            // Toolbar is local UI. Never forward its clicks or held modifier keys.
            if let SdlEvent::MouseButtonDown {
                mouse_btn: MouseButton::Left,
                x,
                y,
                ..
            } = event
                && y < TOOLBAR_HEIGHT as i32
            {
                motion = None;
                send(&events, Event::ReleaseAll)?;
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
                        let state = if canvas.window().fullscreen_state()
                            == sdl2::video::FullscreenType::Off
                        {
                            sdl2::video::FullscreenType::Desktop
                        } else {
                            sdl2::video::FullscreenType::Off
                        };
                        canvas
                            .window_mut()
                            .set_fullscreen(state)
                            .map_err(anyhow::Error::msg)?;
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
                SdlEvent::MouseMotion { x, y, .. } => {
                    motion = desktop_pointer(x, y, canvas.window().size(), size)
                        .map(|(x, y)| Event::Motion { x, y });
                    None
                }
                SdlEvent::MouseButtonDown {
                    mouse_btn, x, y, ..
                } => {
                    motion = None;
                    if let Some((x, y)) = desktop_pointer(x, y, canvas.window().size(), size) {
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
                    if let Some((x, y)) = desktop_pointer(x, y, canvas.window().size(), size) {
                        send(&events, Event::Motion { x, y })?;
                    }
                    button(mouse_btn).map(|button| Event::Button {
                        button,
                        down: false,
                    })
                }
                SdlEvent::MouseWheel {
                    x, y, direction, ..
                } => {
                    let sign = if direction == sdl2::mouse::MouseWheelDirection::Flipped {
                        -1
                    } else {
                        1
                    };
                    Some(Event::Scroll {
                        x: x.clamp(-20, 20) * sign,
                        y: y.clamp(-20, 20) * sign,
                    })
                }
                SdlEvent::Window {
                    win_event: WindowEvent::FocusLost,
                    ..
                } => {
                    motion = None;
                    Some(Event::ReleaseAll)
                }
                _ => None,
            };
            if let Some(event) = event {
                send(&events, event)?;
            }
        }
        if let Some(event) = motion {
            send(&events, event)?;
        }
        let next_frame = { image.lock().unwrap().take() };
        if let Some(frame) = next_frame.filter(|frame| {
            frame.decoder_recovery
                == stream_stats
                    .decoder_recoveries
                    .load(std::sync::atomic::Ordering::Relaxed)
                && video_generation_matches(desktop.video_start_group, frame.video_group)
        }) {
            redraw = true;
            decoded_age = frame.decoded_at.elapsed();
            protocol::validate_video_size(frame.width, frame.height)?;
            if size != (frame.width, frame.height) && frame.hdr.is_none() {
                size = (frame.width, frame.height);
                texture = Some(creator.create_texture_streaming(
                    PixelFormatEnum::RGB24,
                    size.0,
                    size.1,
                )?);
            }
            if frame.hdr.is_none() {
                texture.as_mut().context("missing SDR texture")?.update(
                    None,
                    &frame.data,
                    frame.stride,
                )?;
            } else {
                ensure!(
                    hdr_presenter.is_some(),
                    "received HDR without an HDR presenter"
                );
                size = (frame.width, frame.height);
            }
            last_frame = Instant::now();
            if frame.hdr.is_some() {
                hdr_frame = Some(frame);
                hdr_frame_pending = true;
            } else {
                if switch_ack && frame.width == desktop.width && frame.height == desktop.height {
                    switching = false;
                    awaiting_initial = false;
                    switch_ack = false;
                }
                frames += 1;
                total_frames += 1;
            }
        }
        ensure!(
            total_frames > 0 || last_frame.elapsed() < Duration::from_secs(15),
            "no initial decoded video for 15 seconds"
        );
        if !redraw && stats.elapsed() < Duration::from_secs(1) {
            std::thread::sleep(Duration::from_millis(2));
            continue;
        }
        let window_size = canvas.window().size();
        canvas.set_logical_size(window_size.0.max(1), window_size.1.max(TOOLBAR_HEIGHT + 1))?;
        canvas.set_draw_color(sdl2::pixels::Color::RGB(12, 15, 20));
        canvas.clear();
        if let Some(texture) = &texture {
            canvas
                .copy(texture, None, desktop_rect(window_size, size))
                .map_err(anyhow::Error::msg)?;
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
                "STATS FOR NERDS                                     F8 to hide".into(),
                format!(
                    "Video                 {} x {}   |   {} / {}",
                    size.0,
                    size.1,
                    desktop.dynamic_range.label(),
                    desktop.codec.label()
                ),
                format!("Presented             {fps:.1} fps"),
                format!("Video payload         {mbps:.2} Mbps (not wire bitrate)"),
                format!("Control round trip    {latency}"),
                format!("Host encoder          {host_encoder}"),
                format!(
                    "Client decoder        {}",
                    stream_stats.decoder.lock().unwrap()
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
                    "Decoder recoveries    {}   |   Stale frames {}",
                    snapshot.decoder_recoveries, snapshot.stale_frames
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
                &font,
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
            Some(4) => "Full screen · toggle the native window",
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
        session_text(
            &mut canvas,
            &creator,
            &font,
            &mut text_cache,
            &status,
            Rect::new(14, 55, window_size.0.saturating_sub(28), 20),
            Color::RGB(146, 167, 188),
        )?;
        let mut stats_panel = None;
        if show_stats {
            let panel_width = 520.min(window_size.0.saturating_sub(24));
            let panel = Rect::new(
                window_size.0 as i32 - panel_width as i32 - 12,
                TOOLBAR_HEIGHT as i32 + 12,
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
                    &font,
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
            let hdr = frame
                .hdr
                .as_ref()
                .context("missing high-precision planes")?;
            let mut ui = vec![read_hdr_overlay(
                &mut canvas,
                Rect::new(0, 0, window_size.0, TOOLBAR_HEIGHT),
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
            match presenter.present(
                &crate::hdr_present::P010Frame {
                    width: frame.width,
                    height: frame.height,
                    y: &hdr.y,
                    uv: &hdr.uv,
                    y_stride: hdr.y_stride,
                    uv_stride: hdr.uv_stride,
                },
                desktop_rect(window_size, size),
                &overlays,
                window_size,
            )? {
                crate::hdr_present::PresentOutcome::Presented { headroom } => {
                    hdr_headroom = Some(headroom);
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
        if options
            .exit_after_frames
            .is_some_and(|limit| total_frames >= limit)
        {
            break;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    let _ = events.try_send(Event::ReleaseAll);
    // Disconnect also triggers host-side release of every held key/button.
    Ok(reconnect)
}

const TOOLBAR_HEIGHT: u32 = 82;

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

fn session_text<'a>(
    canvas: &mut sdl2::render::Canvas<sdl2::video::Window>,
    creator: &'a sdl2::render::TextureCreator<sdl2::video::WindowContext>,
    font: &sdl2::ttf::Font<'_, '_>,
    cache: &mut std::collections::HashMap<String, sdl2::render::Texture<'a>>,
    text: &str,
    rect: Rect,
    color: sdl2::pixels::Color,
) -> Result<()> {
    if text.is_empty() {
        return Ok(());
    }
    if !cache.contains_key(text) {
        if cache.len() >= 128 {
            cache.clear();
        }
        let surface = font.render(text).blended(sdl2::pixels::Color::WHITE)?;
        cache.insert(text.into(), creator.create_texture_from_surface(&surface)?);
    }
    let texture = cache.get_mut(text).unwrap();
    texture.set_color_mod(color.r, color.g, color.b);
    let query = texture.query();
    let width = (query.width / 2).min(rect.width());
    let height = query.height / 2;
    // Render at twice logical resolution for crisp Retina/HiDPI text. Clip, never stretch.
    canvas
        .copy(
            texture,
            Rect::new(0, 0, width * 2, query.height),
            Rect::new(rect.x(), rect.y(), width, height),
        )
        .map_err(anyhow::Error::msg)
}

fn desktop_rect(window: (u32, u32), image: (u32, u32)) -> Rect {
    let mut rect = fit(
        (window.0, window.1.saturating_sub(TOOLBAR_HEIGHT).max(1)),
        image,
    );
    rect.set_y(rect.y() + TOOLBAR_HEIGHT as i32);
    rect
}

fn desktop_pointer(x: i32, y: i32, window: (u32, u32), image: (u32, u32)) -> Option<(f64, f64)> {
    if y < TOOLBAR_HEIGHT as i32 {
        return None;
    }
    pointer(
        x,
        y - TOOLBAR_HEIGHT as i32,
        (window.0, window.1.saturating_sub(TOOLBAR_HEIGHT).max(1)),
        image,
    )
}

fn check_network(network: &mut Network, runtime: &Runtime) -> Result<()> {
    if network.0.is_finished() {
        runtime.block_on(&mut network.0)??;
        anyhow::bail!("network session ended");
    }
    Ok(())
}

fn send(events: &mpsc::Sender<Event>, event: Event) -> Result<()> {
    event.validate()?;
    events
        .try_send(event)
        .context("input queue full or disconnected; ending session rather than losing key events")
}
fn button(button: MouseButton) -> Option<u8> {
    match button {
        MouseButton::Left => Some(1),
        MouseButton::Middle => Some(2),
        MouseButton::Right => Some(3),
        _ => None,
    }
}

fn fit(window: (u32, u32), image: (u32, u32)) -> Rect {
    let scale =
        (window.0 as f64 / image.0.max(1) as f64).min(window.1 as f64 / image.1.max(1) as f64);
    let w = (image.0 as f64 * scale) as u32;
    let h = (image.1 as f64 * scale) as u32;
    Rect::new(
        ((window.0 - w) / 2) as i32,
        ((window.1 - h) / 2) as i32,
        w,
        h,
    )
}
fn pointer(x: i32, y: i32, window: (u32, u32), image: (u32, u32)) -> Option<(f64, f64)> {
    if image.0 == 0 || image.1 == 0 {
        return None;
    }
    let rect = fit(window, image);
    if !rect.contains_point((x, y)) {
        return None;
    }
    Some((
        (x - rect.x()) as f64 / rect.width() as f64,
        (y - rect.y()) as f64 / rect.height() as f64,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn letterbox_coordinates() {
        assert!(pointer(0, 0, (1000, 1000), (1920, 1080)).is_none());
        let (x, y) = pointer(500, 500, (1000, 1000), (1920, 1080)).unwrap();
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
        for y in 0..TOOLBAR_HEIGHT as i32 {
            assert!(desktop_pointer(350, y, (700, 600), (1920, 1080)).is_none());
        }
        let rect = desktop_rect((700, 600), (1920, 1080));
        let (x, y) = desktop_pointer(
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
}
