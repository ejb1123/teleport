use crate::{
    capture::Capture,
    media,
    protocol::{self, Desktop, Input, Pairing},
};
use anyhow::{Context, Result, ensure};
use clap::Args;
use std::{io::Write, path::PathBuf, time::Duration};
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
}

pub async fn run(options: Options) -> Result<()> {
    ensure!(
        !options.pairing_file.exists(),
        "pairing file already exists; choose a new --pairing-file (credentials rotate each host start)"
    );
    media::doctor()?;
    let mut capture = Capture::open(&options.source).await?;
    let result = serve(&options, &mut capture).await;
    capture.close().await;
    result
}

async fn serve(options: &Options, capture: &mut Capture) -> Result<()> {
    let mut config = moq_native::ServerConfig::default();
    config.bind = Some(options.listen.clone());
    config.version = vec![protocol::WIRE_VERSION.parse().map_err(anyhow::Error::msg)?];
    config.tls.generate = vec!["teleport.local".into()];
    let mut server = config.init()?;
    let token = rand::random::<[u8; 32]>()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<String>();
    let pairing = Pairing {
        token,
        fingerprint: server
            .certificates()
            .fingerprints()
            .first()
            .context("no certificate")?
            .clone(),
    };
    use std::os::unix::fs::OpenOptionsExt;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&options.pairing_file)?;
    file.write_all(&serde_json::to_vec_pretty(&pairing)?)?;
    file.sync_all()?;
    tracing::info!(address = %server.local_addr()?, pairing_file = %options.pairing_file.display(), "Host ready; copy pairing file securely to your client. UDP port must be reachable.");
    while let Some(request) = server.accept().await {
        let expected = format!("/teleport/{}", pairing.token);
        if !bool::from(request.path().as_bytes().ct_eq(expected.as_bytes())) {
            let _ = request.close(403).await;
            tracing::warn!("rejected unauthorized client");
            continue;
        }
        let result = tokio::select! {
            result = connection(request, options, capture) => result,
            _ = tokio::signal::ctrl_c() => { capture.release_all().await; break; }
        };
        capture.release_all().await;
        if let Err(error) = result {
            tracing::warn!(%error, "client disconnected");
        }
    }
    server.close().await;
    Ok(())
}

async fn connection(
    request: moq_native::Request,
    options: &Options,
    capture: &mut Capture,
) -> Result<()> {
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
    let width = options.width / 2 * 2;
    let height =
        ((width as u64 * capture.height as u64 / capture.width as u64) as u32 / 2 * 2).max(2);
    ensure!(height <= 4320, "scaled desktop exceeds maximum height");
    let info = Desktop {
        version: protocol::VERSION,
        width,
        height,
        fps: options.fps,
        source: capture.name.into(),
    };
    let pipeline = media::encoder(
        &capture.pipeline_source(),
        width,
        height,
        options.fps,
        options.bitrate,
        video,
    )?;
    let mut group = metadata.append_group()?;
    group.write_frame(moq_net::Timestamp::now(), serde_json::to_vec(&info)?)?;
    group.finish()?;
    tracing::info!(width, height, fps = options.fps, "native client connected");
    let mut check = tokio::time::interval(Duration::from_millis(200));
    tokio::select! {
        result = receive_input(input, capture) => result,
        error = session.closed() => anyhow::bail!("{error}"),
        result = async { loop { check.tick().await; pipeline.error()?; } #[allow(unreachable_code)] Ok::<(), anyhow::Error>(()) } => result,
    }
}

async fn receive_input(mut track: moq_net::track::Subscriber, capture: &mut Capture) -> Result<()> {
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
                protocol::read_frame(&mut group, 1024),
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
            capture.input(input.event).await?;
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
    if let Some(group) = pending.remove(&expected) {
        return Ok(group);
    }
    loop {
        let group = track.recv_group().await?.context("input track closed")?;
        ensure!(
            group.sequence >= expected && group.sequence - expected <= 16,
            "control group outside reorder window"
        );
        if group.sequence == expected {
            return Ok(group);
        }
        ensure!(
            pending.insert(group.sequence, group).is_none(),
            "duplicate control group"
        );
    }
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
