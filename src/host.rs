use crate::{
    capture::Capture,
    media,
    protocol::{self, Desktop, Event, Input, Pairing, Update},
};
use anyhow::{Context, Result, ensure};
use clap::Args;
use std::{
    path::PathBuf,
    time::{Duration, Instant},
};
use subtle::ConstantTimeEq;

#[derive(Args, Clone)]
pub struct Options {
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
    #[arg(long, default_value_t = 60, value_parser = clap::value_parser!(u32).range(1..=120))]
    pub fps: u32,
    /// Video bitrate in kilobits per second.
    #[arg(long, default_value_t = 8000, value_parser = clap::value_parser!(u32).range(500..=100000))]
    pub bitrate: u32,
    /// Persistent private identity directory; keeps paired clients valid across restarts.
    #[arg(long, conflicts_with = "pairing_file")]
    pub identity_dir: Option<PathBuf>,
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
    let admin = if let (Some(directory), Some(access)) =
        (options.identity_dir.as_deref(), access.as_ref())
    {
        Some(
            crate::host_admin::start(
                directory,
                server.local_addr()?,
                pairing.clone(),
                access.clone(),
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
        println!(
            "Pairing code: {} (one use, expires in 5 minutes)",
            control.open_pairing().await?
        );
    }
    let shutdown = shutdown_signal();
    tokio::pin!(shutdown);
    loop {
        let request = tokio::select! {
            request = server.accept() => match request { Some(request) => request, None => break },
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
        let result = tokio::select! {
            result = connection(request, options, capture) => result,
            _ = async { loop { tokio::time::sleep(Duration::from_secs(1)).await; if access.as_ref().is_some_and(|access| !access.authorized(&supplied)) { break; } } } => Err(anyhow::anyhow!("device access revoked")),
            _ = &mut shutdown => { capture.release_all().await; break; }
        };
        capture.release_all().await;
        if let Err(error) = result {
            tracing::warn!(%error, "client disconnected");
        }
    }
    server.close().await;
    Ok(())
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
    // Stream preferences belong to this connection, not the next paired client.
    let mut negotiated = options.clone();
    let requested = requested_format(request.query(), options.codec)?;
    let ssh_requested = url::form_urlencoded::parse(request.query().unwrap_or_default().as_bytes())
        .any(|(key, value)| key == "ssh-agent" && value == "1");
    ensure!(
        !ssh_requested || options.allow_ssh_agent,
        "SSH agent forwarding is disabled; host requires explicit --allow-ssh-agent"
    );
    negotiated.allow_ssh_agent = ssh_requested && options.allow_ssh_agent;
    negotiated.codec = requested.codec;
    negotiated.dynamic_range = requested.dynamic_range;
    let options = &mut negotiated;
    capture.select_monitor(capture.active_monitor).await?;
    let outgoing = moq_net::Origin::random().produce();
    let incoming = moq_net::Origin::random().produce();
    let mut broadcast = outgoing.create_broadcast(
        "desktop",
        moq_net::broadcast::Route::new().with_announce(true),
    )?;
    let video = broadcast.create_track(
        options.codec.track(),
        moq_net::track::Info::default().with_latency_max(Duration::from_millis(500)),
    )?;
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
    let session = request
        .with_publisher(&outgoing)
        .with_subscriber(incoming.clone())
        .ok()
        .await?;
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
    let info = desktop(options, capture, video_start_group)?;
    let mut pipeline = media::encoder(
        &capture.pipeline_source_for_range(options.dynamic_range)?,
        (info.width, info.height),
        options.fps,
        options.bitrate,
        video.clone(),
        options.encoder,
        media::VideoFormat {
            codec: options.codec,
            dynamic_range: options.dynamic_range,
        },
    )?;
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
    let (sender, mut receiver) = tokio::sync::mpsc::channel(256);
    let incoming = receive_input(input, sender);
    tokio::pin!(incoming);
    let mut adaptation = media::BitrateController::new(options.bitrate);
    let mut adaptive = !options.fixed_bitrate;
    let mut last_feedback = Instant::now();
    let mut last_switch = Instant::now() - Duration::from_secs(1);
    let mut last_configure = Instant::now() - Duration::from_secs(1);
    let mut encoder_metrics = pipeline.encoder_metrics()?;
    let mut telemetry_enabled = false;
    let mut last_telemetry = Instant::now();
    let mut actual_bitrate = options.bitrate;
    loop {
        tokio::select! {
            result = crate::ssh_agent::closed(&mut ssh_agent) => return result,
            result = &mut incoming => return result,
            error = session.closed() => anyhow::bail!("{error}"),
            _ = check.tick() => {
                pipeline.error()?;
                if let Some(audio) = &audio { audio.error()?; }
                if telemetry_enabled && last_telemetry.elapsed() >= Duration::from_secs(1) {
                    last_telemetry = Instant::now();
                    publish_update(&mut updates, Update::Telemetry {
                        encode_us: encoder_metrics.encode_us.load(std::sync::atomic::Ordering::Relaxed),
                        bitrate: actual_bitrate,
                        encoder: encoder_metrics.encoder.clone(),
                    })?;
                }
            },
            event = receiver.recv() => {
                match event.context("input stream closed")? {
                    Event::ConfigureVideo { width, fps, bitrate } => {
                        if last_configure.elapsed() < Duration::from_millis(500) {
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
                        capture.release_all().await;
                        drop(pipeline);
                        video_start_group = video_barrier(&video)?;
                        info.video_start_group = video_start_group;
                        capture.select_monitor(capture.active_monitor).await?;
                        pipeline = media::encoder(&capture.pipeline_source_for_range(options.dynamic_range)?, (info.width, info.height), fps, bitrate, video.clone(), options.encoder, media::VideoFormat { codec: options.codec, dynamic_range: options.dynamic_range })?;
                        *options = requested;
                        encoder_metrics = pipeline.encoder_metrics()?;
                        actual_bitrate = bitrate;
                        adaptation = media::BitrateController::new(bitrate);
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
                        drop(pipeline);
                        video_start_group = video_barrier(&video)?;
                        capture.select_monitor(index).await?;
                        let info = desktop(options, capture, video_start_group)?;
                        pipeline = media::encoder(&capture.pipeline_source_for_range(options.dynamic_range)?, (info.width, info.height), options.fps, options.bitrate, video.clone(), options.encoder, media::VideoFormat { codec: options.codec, dynamic_range: options.dynamic_range })?;
                        adaptation = media::BitrateController::new(options.bitrate);
                        encoder_metrics = pipeline.encoder_metrics()?;
                        actual_bitrate = options.bitrate;
                        adaptive = !options.fixed_bitrate;
                        last_feedback = Instant::now();
                        publish_update(&mut updates, Update::Desktop { desktop: info })?;
                    }
                    Event::Feedback { queue_ms, dropped_groups } => {
                        if adaptive && last_feedback.elapsed() >= Duration::from_millis(900) {
                            last_feedback = Instant::now();
                            if let Some(bitrate) = adaptation.observe(queue_ms, dropped_groups) {
                                match pipeline.set_video_bitrate(bitrate) {
                                    Ok(()) => { actual_bitrate = bitrate; tracing::info!(bitrate, queue_ms, dropped_groups, "adapted video bitrate"); },
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
                    event => capture.input(event).await?,
                }
            },
        }
    }
}

fn desktop(options: &Options, capture: &Capture, video_start_group: u64) -> Result<Desktop> {
    let width = if options.width == 0 {
        capture.width
    } else {
        options.width
    } / 2
        * 2;
    let height =
        ((width as u64 * capture.height as u64 / capture.width as u64) as u32 / 2 * 2).max(2);
    protocol::validate_video_size(width, height)?;
    Ok(Desktop {
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
        active_monitor: capture.active_monitor,
        audio: options.audio_source.is_some(),
        clipboard: options.clipboard,
        telemetry: true,
        configurable_video: true,
        native_width: capture.width,
        native_height: capture.height,
        bitrate: options.bitrate,
        video_start_group,
    })
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

fn publish_update(track: &mut moq_net::track::Producer, update: Update) -> Result<()> {
    let mut group = track.append_group()?;
    group.write_frame(moq_net::Timestamp::now(), serde_json::to_vec(&update)?)?;
    group.finish()?;
    Ok(())
}

async fn receive_input(
    mut track: moq_net::track::Subscriber,
    sender: tokio::sync::mpsc::Sender<Event>,
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
