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

#[derive(Args)]
pub struct Options {
    /// Bind address (UDP). Use 0.0.0.0:4443 for LAN access.
    #[arg(long, default_value = "127.0.0.1:4443")]
    pub listen: String,
    #[arg(long, default_value = "auto", value_parser = ["auto", "portal", "x11", "test"])]
    pub source: String,
    /// New private pairing file; never overwritten. Copy securely to the client.
    #[arg(long, default_value = "pairing.json")]
    pub pairing_file: PathBuf,
    #[arg(long, default_value_t = 1280, value_parser = clap::value_parser!(u32).range(320..=3840))]
    pub width: u32,
    #[arg(long, default_value_t = 60, value_parser = clap::value_parser!(u32).range(1..=120))]
    pub fps: u32,
    /// H.264 bitrate in kilobits per second.
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
    let _enrollment = if options.pair {
        if options.identity_dir.is_none() {
            tracing::warn!(
                "Use --identity-dir to keep paired clients trusted across host restarts"
            );
        }
        Some(crate::pairing::start(server.local_addr()?, pairing.clone()).await?)
    } else {
        None
    };
    let shutdown = shutdown_signal();
    tokio::pin!(shutdown);
    loop {
        let request = tokio::select! {
            request = server.accept() => match request { Some(request) => request, None => break },
            _ = &mut shutdown => break,
        };
        let expected = format!("/teleport/{}", pairing.token);
        if !bool::from(request.path().as_bytes().ct_eq(expected.as_bytes())) {
            let _ = request.close(403).await;
            tracing::warn!("rejected unauthorized client");
            continue;
        }
        let result = tokio::select! {
            result = connection(request, options, capture) => result,
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
    capture.select_monitor(capture.active_monitor).await?;
    let outgoing = moq_net::Origin::random().produce();
    let incoming = moq_net::Origin::random().produce();
    let mut broadcast = outgoing.create_broadcast(
        "desktop",
        moq_net::broadcast::Route::new().with_announce(true),
    )?;
    let video = broadcast.create_track(
        protocol::VIDEO,
        moq_net::track::Info::default().with_latency_max(Duration::from_millis(500)),
    )?;
    let mut metadata = broadcast.create_track("desktop", None)?;
    let mut updates = broadcast.create_track(
        "updates",
        moq_net::track::Info::default().with_ordered(true),
    )?;
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
    let info = desktop(options, capture)?;
    let mut pipeline = media::encoder(
        &capture.pipeline_source(),
        info.width,
        info.height,
        options.fps,
        options.bitrate,
        video.clone(),
        options.encoder,
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
    loop {
        tokio::select! {
            result = &mut incoming => return result,
            error = session.closed() => anyhow::bail!("{error}"),
            _ = check.tick() => { pipeline.error()?; if let Some(audio) = &audio { audio.error()?; } },
            event = receiver.recv() => {
                match event.context("input stream closed")? {
                    Event::SelectMonitor { index } => {
                        if index >= capture.monitors.len() || last_switch.elapsed() < Duration::from_millis(500) {
                            publish_update(&mut updates, Update::Desktop { desktop: desktop(options, capture)? })?;
                            continue;
                        }
                        last_switch = Instant::now();
                        capture.release_all().await;
                        // Stop the old source before opening another PipeWire node.
                        drop(pipeline);
                        capture.select_monitor(index).await?;
                        let info = desktop(options, capture)?;
                        pipeline = media::encoder(&capture.pipeline_source(), info.width, info.height, options.fps, options.bitrate, video.clone(), options.encoder)?;
                        adaptation = media::BitrateController::new(options.bitrate);
                        adaptive = !options.fixed_bitrate;
                        last_feedback = Instant::now();
                        publish_update(&mut updates, Update::Desktop { desktop: info })?;
                    }
                    Event::Feedback { queue_ms, dropped_groups } => {
                        if adaptive && last_feedback.elapsed() >= Duration::from_millis(900) {
                            last_feedback = Instant::now();
                            if let Some(bitrate) = adaptation.observe(queue_ms, dropped_groups) {
                                match pipeline.set_video_bitrate(bitrate) {
                                    Ok(()) => tracing::info!(bitrate, queue_ms, dropped_groups, "adapted video bitrate"),
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
                    event => capture.input(event).await?,
                }
            },
        }
    }
}

fn desktop(options: &Options, capture: &Capture) -> Result<Desktop> {
    let width = options.width / 2 * 2;
    let height =
        ((width as u64 * capture.height as u64 / capture.width as u64) as u32 / 2 * 2).max(2);
    ensure!(height <= 4320, "scaled desktop exceeds maximum height");
    Ok(Desktop {
        version: protocol::VERSION,
        width,
        height,
        fps: options.fps,
        source: capture.name.into(),
        monitors: capture.monitors.clone(),
        active_monitor: capture.active_monitor,
        audio: options.audio_source.is_some(),
        clipboard: options.clipboard,
    })
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
