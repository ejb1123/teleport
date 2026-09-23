#![cfg(target_os = "linux")]

#[allow(dead_code)]
#[path = "../src/protocol.rs"]
mod protocol;

use anyhow::{Context, Result};
use std::{
    process::{Child, Command, Stdio},
    time::Duration,
};

struct Process(Child);
impl Drop for Process {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

async fn update(track: &mut moq_net::track::Subscriber) -> Result<protocol::Update> {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let mut group = track.recv_group().await?.context("updates ended")?;
            if let Some(frame) = protocol::read_frame(&mut group, protocol::MAX_CONTROL).await? {
                return Ok(serde_json::from_slice(&frame)?);
            }
        }
    })
    .await?
}

fn send(
    track: &mut moq_net::track::Producer,
    sequence: &mut u64,
    event: protocol::Event,
) -> Result<()> {
    let mut group = track.append_group()?;
    group.write_frame(
        moq_net::Timestamp::now(),
        serde_json::to_vec(&protocol::Input {
            sequence: *sequence,
            event,
        })?,
    )?;
    group.finish()?;
    *sequence += 1;
    Ok(())
}

async fn video_frame(track: &mut moq_net::track::Subscriber, floor: u64) -> Result<u64> {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let mut group = track.recv_group().await?.context("video ended")?;
            if group.sequence >= floor
                && protocol::read_frame(&mut group, 8 * 1024 * 1024)
                    .await?
                    .is_some()
            {
                return Ok(group.sequence);
            }
        }
    })
    .await?
}

#[tokio::test]
#[ignore = "requires local UDP sockets and GStreamer software encoder"]
async fn independent_monitors_share_one_authenticated_session_and_restart_safely() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let pairing_path = temp.path().join("pairing.json");
    let socket = std::net::UdpSocket::bind("127.0.0.1:0")?;
    let address = socket.local_addr()?.to_string();
    drop(socket);
    let mut host = Process(
        Command::new(env!("CARGO_BIN_EXE_teleport"))
            .args([
                "host",
                "--source",
                "test",
                "--encoder",
                "software",
                "--width",
                "640",
                "--fps",
                "30",
                "--bitrate",
                "20000",
                "--listen",
                &address,
                "--pairing-file",
            ])
            .arg(&pairing_path)
            .stdout(Stdio::null())
            .spawn()?,
    );
    let pairing = tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            if let Ok(pairing) = protocol::Pairing::read(&pairing_path) {
                return Ok::<_, anyhow::Error>(pairing);
            }
            anyhow::ensure!(host.0.try_wait()?.is_none(), "test host exited");
            tokio::time::sleep(Duration::from_millis(30)).await;
        }
    })
    .await??;
    let mut config = moq_native::ClientConfig::default();
    config.version = vec![protocol::WIRE_VERSION.parse().map_err(anyhow::Error::msg)?];
    config.tls.fingerprint = vec![pairing.fingerprint];
    let outgoing = moq_net::Origin::random().produce();
    let incoming = moq_net::Origin::random().produce();
    let mut controls = outgoing.create_broadcast(
        "controls",
        moq_net::broadcast::Route::new().with_announce(true),
    )?;
    let mut input =
        controls.create_track("input", moq_net::track::Info::default().with_ordered(true))?;
    let client = config
        .init()?
        .with_publisher(&outgoing)
        .with_subscriber(incoming.clone());
    let session = client
        .connect(format!("moqt://{address}/teleport/{}?codec=h264", pairing.token).parse()?)
        .await?;
    let broadcasts = incoming.consume();
    let remote = tokio::time::timeout(
        Duration::from_secs(10),
        broadcasts.announced_broadcast("desktop"),
    )
    .await?
    .context("no desktop")?;
    let mut metadata = remote.track("desktop")?.subscribe(None).await?;
    let mut group = metadata.recv_group().await?.context("no metadata")?;
    let desktop: protocol::Desktop = serde_json::from_slice(
        &protocol::read_frame(&mut group, protocol::MAX_CONTROL)
            .await?
            .context("empty metadata")?,
    )?;
    assert!(desktop.multimonitor);
    assert_eq!(desktop.bitrate, 20_000);
    assert_eq!(desktop.active_monitor, 0);
    let mut updates = remote
        .track("updates")?
        .subscribe(moq_net::track::Subscription::default().with_ordered(true))
        .await?;
    let mut primary = remote.track("h264")?.subscribe(None).await?;
    let mut auxiliary = remote.track("monitor-1-h264")?.subscribe(None).await?;
    let mut sequence = 0;
    send(
        &mut input,
        &mut sequence,
        protocol::Event::MonitorStream {
            index: 1,
            enabled: true,
        },
    )?;
    let protocol::Update::MonitorStream {
        index,
        desktop: extra,
    } = update(&mut updates).await?
    else {
        anyhow::bail!("missing auxiliary metadata")
    };
    assert_eq!(index, 1);
    assert_eq!((extra.native_width, extra.native_height), (720, 1280));
    assert_eq!(extra.active_monitor, 1);
    assert_eq!(extra.bitrate, 20_000);
    let (_, old_group) = tokio::try_join!(
        video_frame(&mut primary, 0),
        video_frame(&mut auxiliary, extra.video_start_group)
    )?;
    send(
        &mut input,
        &mut sequence,
        protocol::Event::MonitorMotion {
            index: 1,
            x: 0.9,
            y: 0.2,
        },
    )?;
    send(
        &mut input,
        &mut sequence,
        protocol::Event::MonitorStream {
            index: 1,
            enabled: false,
        },
    )?;
    // Reopen without waiting for the old close acknowledgement. Clients can
    // identify that stale acknowledgement and keep the new consumer alive.
    send(
        &mut input,
        &mut sequence,
        protocol::Event::MonitorStream {
            index: 1,
            enabled: true,
        },
    )?;
    assert!(matches!(
        update(&mut updates).await?,
        protocol::Update::MonitorStreamStopped {
            index: 1,
            requested: true,
            ..
        }
    ));
    let protocol::Update::MonitorStream {
        desktop: restarted, ..
    } = update(&mut updates).await?
    else {
        anyhow::bail!("missing restart metadata")
    };
    assert!(restarted.video_start_group > old_group);
    tokio::try_join!(
        video_frame(&mut primary, 0),
        video_frame(&mut auxiliary, restarted.video_start_group)
    )?;
    send(
        &mut input,
        &mut sequence,
        protocol::Event::ConfigureVideo {
            width: 320,
            fps: 30,
            bitrate: 40_000,
        },
    )?;
    let protocol::Update::MonitorStream {
        desktop: resized, ..
    } = update(&mut updates).await?
    else {
        anyhow::bail!("missing auxiliary reconfigure")
    };
    let protocol::Update::Desktop { desktop: main } = update(&mut updates).await? else {
        anyhow::bail!("missing primary reconfigure")
    };
    assert_eq!((main.width, main.active_monitor), (320, 0));
    assert_eq!((resized.width, resized.active_monitor), (320, 1));
    assert_eq!(main.bitrate, 40_000);
    assert_eq!(resized.bitrate, 40_000);
    assert!(resized.video_start_group > restarted.video_start_group);
    tokio::try_join!(
        video_frame(&mut primary, main.video_start_group),
        video_frame(&mut auxiliary, resized.video_start_group)
    )?;
    send(
        &mut input,
        &mut sequence,
        protocol::Event::MonitorStream {
            index: 63,
            enabled: true,
        },
    )?;
    assert!(matches!(
        update(&mut updates).await?,
        protocol::Update::MonitorStreamStopped {
            index: 63,
            requested: false,
            ..
        }
    ));
    send(&mut input, &mut sequence, protocol::Event::Probe { id: 99 })?;
    assert!(matches!(
        update(&mut updates).await?,
        protocol::Update::Pong { id: 99 }
    ));
    assert!(matches!(
        update(&mut updates).await?,
        protocol::Update::Telemetry {
            bitrate: 40_000,
            ..
        }
    ));
    send(&mut input, &mut sequence, protocol::Event::ReleaseAll)?;
    drop(session);
    Ok(())
}
