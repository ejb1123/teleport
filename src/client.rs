use crate::{
    media,
    protocol::{self, Desktop, Event, Input, Pairing},
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

#[derive(Args)]
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
}

struct Link {
    session: moq_net::Session,
    _origin: moq_net::origin::Producer,
    _broadcast: moq_net::broadcast::Producer,
    input: moq_net::track::Producer,
    video: moq_net::track::Subscriber,
    desktop: Desktop,
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
        &protocol::read_frame(&mut group, 4096)
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
    Ok(Link {
        session,
        _origin: outgoing,
        _broadcast: broadcast,
        input,
        video,
        desktop,
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
    let link = runtime
        .block_on(async { tokio::time::timeout(Duration::from_secs(15), connect(&options)).await })
        .context("connection timed out")??;
    tracing::info!(width = link.desktop.width, height = link.desktop.height, source = %link.desktop.source, "desktop connected; Ctrl+Alt+Q quits locally");
    let (pipeline, source, image) = media::decoder(options.software_decoder)?;
    let (events, receiver) = mpsc::channel(256);
    let mut network = Network(runtime.spawn(async move {
        let _origin = link._origin;
        let _broadcast = link._broadcast;
        tokio::select! {
            result = media::receive_video(link.video, source) => result,
            result = send_input(link.input, receiver) => result,
            error = link.session.closed() => anyhow::bail!("connection closed: {error}"),
        }
    }));
    if let Some(frames) = options.headless_frames {
        ensure!(frames > 0, "headless frame count must be positive");
        let deadline = Instant::now() + Duration::from_secs(20);
        let mut count = 0;
        while count < frames {
            pipeline.error()?;
            check_network(&mut network, runtime)?;
            ensure!(
                Instant::now() < deadline,
                "timed out decoding frames ({count}/{frames})"
            );
            if let Some(frame) = image.lock().unwrap().take() {
                ensure!(!frame.data.is_empty(), "empty decoded frame");
                count += 1;
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
        tracing::info!(
            frames = count,
            "SMOKE PASS: authenticated MoQ video decoded"
        );
        return Ok(());
    }
    let sdl = sdl2::init().map_err(anyhow::Error::msg)?;
    let video = sdl.video().map_err(anyhow::Error::msg)?;
    let window = video
        .window("Teleport — connecting video", 1280, 720)
        .position_centered()
        .resizable()
        .allow_highdpi()
        .build()?;
    let builder = window.into_canvas();
    let mut canvas = if options.software_renderer {
        builder.software()
    } else {
        builder.accelerated()
    }
    .build()?;
    let creator = canvas.texture_creator();
    let mut texture = None;
    let mut size = (0, 0);
    let mut event_pump = sdl.event_pump().map_err(anyhow::Error::msg)?;
    let mut last_frame = Instant::now();
    let mut frames = 0;
    let mut total_frames = 0;
    let mut stats = Instant::now();
    'running: loop {
        pipeline.error()?;
        check_network(&mut network, runtime)?;
        let mut motion = None;
        for event in event_pump.poll_iter() {
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
                    motion = pointer(x, y, canvas.window().size(), size)
                        .map(|(x, y)| Event::Motion { x, y });
                    None
                }
                SdlEvent::MouseButtonDown {
                    mouse_btn, x, y, ..
                } => {
                    motion = None;
                    if let Some((x, y)) = pointer(x, y, canvas.window().size(), size) {
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
                    if let Some((x, y)) = pointer(x, y, canvas.window().size(), size) {
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
            frames += 1;
            total_frames += 1;
        }
        ensure!(
            total_frames > 0 || last_frame.elapsed() < Duration::from_secs(15),
            "no initial decoded video for 15 seconds"
        );
        canvas.set_draw_color(sdl2::pixels::Color::RGB(12, 15, 20));
        canvas.clear();
        if let Some(texture) = &texture {
            canvas
                .copy(
                    texture,
                    None,
                    fit(canvas.output_size().map_err(anyhow::Error::msg)?, size),
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
                "Teleport — {}×{} · {fps:.0} fps · Ctrl+Alt+Q to quit",
                size.0, size.1
            ))?;
            stats = Instant::now();
            frames = 0;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    let _ = events.try_send(Event::ReleaseAll);
    // Disconnect also triggers host-side release of every held key/button.
    Ok(())
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
