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
    /// Force software H.264 decoding (useful when diagnosing VideoToolbox).
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
}

struct Link {
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
    let pairing = Pairing::read(&options.pairing_file)?;
    ensure!(
        !options.address.contains('/') && !options.address.contains('@'),
        "use host:port, not a URL"
    );
    let url: url::Url = format!("moqt://{}/teleport/{}", options.address, pairing.token).parse()?;
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
    let client = config
        .init()?
        .with_publisher(&outgoing)
        .with_subscriber(incoming.clone());
    tracing::info!(address = %options.address, "connecting with pinned TLS certificate");
    let session = client
        .connect(url)
        .await
        .context("QUIC connection failed; check address, pairing file and UDP firewall")?;
    let remote = incoming
        .consume()
        .announced_broadcast("desktop")
        .await
        .context("host did not announce desktop")?;
    let mut metadata = remote.track("desktop")?.subscribe(None).await?;
    let mut group = metadata
        .recv_group()
        .await?
        .context("desktop metadata missing")?;
    let desktop: Desktop = serde_json::from_slice(
        &protocol::read_frame(&mut group, protocol::MAX_CONTROL)
            .await?
            .context("empty desktop metadata")?,
    )?;
    ensure!(
        desktop.version == protocol::VERSION,
        "incompatible Teleport version"
    );
    ensure!(
        (2..=3840).contains(&desktop.width) && (2..=4320).contains(&desktop.height),
        "invalid desktop size"
    );
    let video = remote
        .track(protocol::VIDEO)?
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
    let sdl = sdl2::init().map_err(anyhow::Error::msg)?;
    let video = sdl.video().map_err(anyhow::Error::msg)?;
    let ttf = sdl2::ttf::init().map_err(anyhow::Error::msg)?;
    let font = ttf
        .load_font(crate::launcher::font_path()?, 18)
        .map_err(anyhow::Error::msg)?;
    let mut canvas = video
        .window("Teleport — connecting", 760, 275)
        .position_centered()
        .build()?
        .into_canvas()
        .software()
        .build()?;
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
    let (pipeline, source, image) = media::decoder(options.software_decoder)?;
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
    let mut network = Network(runtime.spawn(async move {
        let _origin = link._origin;
        let _broadcast = link._broadcast;
        tokio::select! {
            result = media::receive_video_with_feedback(link.video, source, feedback) => result,
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
    if let Some(frames) = options.headless_frames {
        ensure!(frames > 0, "headless frame count must be positive");
        let deadline = Instant::now() + Duration::from_secs(20);
        let mut count = 0;
        let mut switched = false;
        let mut clipboard_ok = false;
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
            if let Some(frame) = image.lock().unwrap().take() {
                ensure!(!frame.data.is_empty(), "empty decoded frame");
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
            while let Ok(update) = updates.try_recv() {
                if let Update::Clipboard { text } = update {
                    clipboard_ok = text == CLIPBOARD_SMOKE;
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
        tracing::info!(
            frames = count,
            "SMOKE PASS: authenticated MoQ video decoded"
        );
        return Ok(false);
    }
    let sdl = sdl2::init().map_err(anyhow::Error::msg)?;
    let video = sdl.video().map_err(anyhow::Error::msg)?;
    let mut window = video
        .window("Teleport — connecting video", 1280, 720)
        .position_centered()
        .resizable()
        .allow_highdpi()
        .build()?;
    window
        .set_minimum_size(700, 240)
        .map_err(anyhow::Error::msg)?;
    let ttf = sdl2::ttf::init().map_err(anyhow::Error::msg)?;
    let font = ttf
        .load_font(crate::launcher::font_path()?, 14)
        .map_err(anyhow::Error::msg)?;
    let builder = window.into_canvas();
    let mut canvas = if options.software_renderer {
        builder.software()
    } else {
        builder.accelerated()
    }
    .build()?;
    let creator = canvas.texture_creator();
    let toolbar_labels = [
        "Monitor >",
        "Audio",
        "Send text",
        "Get text",
        "Fullscreen",
        "Reconnect",
        "Disconnect",
    ];
    let toolbar_textures = toolbar_labels
        .iter()
        .map(|text| {
            let surface = font
                .render(text)
                .blended(sdl2::pixels::Color::RGB(230, 237, 248))?;
            Ok::<_, anyhow::Error>((
                creator.create_texture_from_surface(&surface)?,
                surface.width(),
                surface.height(),
            ))
        })
        .collect::<Result<Vec<_>>>()?;
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
    let mut clipboard_requested = false;
    let mut switching = false;
    let mut switch_ack = false;
    let mut first_draw = true;
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
        while let Ok(update) = updates.try_recv() {
            redraw = true;
            match update {
                Update::Desktop { desktop: next } => {
                    ensure!(
                        next.version == protocol::VERSION && next.monitors.len() <= 64,
                        "invalid monitor metadata"
                    );
                    desktop = next;
                    *image.lock().unwrap() = None;
                    switch_ack = true;
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
        if let Some(frame) = image.lock().unwrap().take() {
            redraw = true;
            ensure!(
                frame.width <= 3840 && frame.height <= 4320,
                "decoded image exceeds bounds"
            );
            if size != (frame.width, frame.height) {
                size = (frame.width, frame.height);
                texture = Some(creator.create_texture_streaming(
                    PixelFormatEnum::RGB24,
                    size.0,
                    size.1,
                )?);
            }
            texture
                .as_mut()
                .unwrap()
                .update(None, &frame.data, frame.stride)?;
            last_frame = Instant::now();
            if switch_ack && frame.width == desktop.width && frame.height == desktop.height {
                switching = false;
                switch_ack = false;
            }
            frames += 1;
            total_frames += 1;
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
        for (index, (texture, width, height)) in toolbar_textures.iter().enumerate() {
            let rect = toolbar_rect(index, window_size.0);
            let enabled = match index {
                0 => desktop.monitors.len() > 1,
                1 => desktop.audio && !muted,
                2 | 3 => options.clipboard && desktop.clipboard,
                _ => true,
            };
            canvas.set_draw_color(if enabled {
                sdl2::pixels::Color::RGB(38, 61, 87)
            } else {
                sdl2::pixels::Color::RGB(35, 38, 43)
            });
            canvas.fill_rect(rect).map_err(anyhow::Error::msg)?;
            let w = (*width).min(rect.width().saturating_sub(8));
            canvas
                .copy(
                    texture,
                    None,
                    Rect::new(
                        rect.x() + ((rect.width() - w) / 2) as i32,
                        12,
                        w.max(1),
                        *height,
                    ),
                )
                .map_err(anyhow::Error::msg)?;
        }
        canvas.present();
        if options
            .exit_after_frames
            .is_some_and(|limit| total_frames >= limit)
        {
            break;
        }
        if stats.elapsed() >= Duration::from_secs(1) {
            let fps = frames as f64 / stats.elapsed().as_secs_f64();
            canvas.window_mut().set_title(&format!(
                "Teleport — monitor {}/{} · {}×{} · {fps:.0} fps · {}",
                desktop.active_monitor + 1,
                desktop.monitors.len(),
                size.0,
                size.1,
                notice
            ))?;
            stats = Instant::now();
            frames = 0;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    let _ = events.try_send(Event::ReleaseAll);
    // Disconnect also triggers host-side release of every held key/button.
    Ok(reconnect)
}

const TOOLBAR_HEIGHT: u32 = 44;

fn toolbar_rect(index: usize, width: u32) -> Rect {
    let left = width * index as u32 / 7;
    let right = width * (index as u32 + 1) / 7;
    Rect::new(
        left as i32 + 2,
        2,
        right.saturating_sub(left + 4).max(1),
        TOOLBAR_HEIGHT - 4,
    )
}

fn toolbar_hit(x: i32, y: i32, width: u32) -> Option<usize> {
    (0..7).find(|&index| toolbar_rect(index, width).contains_point((x, y)))
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
}
