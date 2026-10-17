//! Virtual displays and privacy mode against a real KWin and
//! xdg-desktop-portal-kde, in a private headless KDE desktop
//! (`tests/support/nested-kwin.sh`): never the user's session.
//!
//! Each test re-runs itself inside its own nested desktop. They need
//! `kwin_wayland`, the KDE portal, PipeWire and `kscreen-doctor` (KDE
//! Plasma 6) and skip without them.
#![cfg(target_os = "linux")]

#[allow(dead_code)]
#[path = "../src/protocol.rs"]
mod protocol;

use anyhow::{Context, Result, ensure};
use std::{
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

const INNER: &str = "TELEPORT_IN_NESTED_KWIN";

/// Outside the nested desktop: run `name` inside one with `outputs`
/// "physical" outputs and report its result. Inside: `None`, run the body.
fn nested(name: &str, outputs: u32) -> Option<Result<()>> {
    if std::env::var_os(INNER).is_some() {
        return None;
    }
    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/support/nested-kwin.sh");
    let status = Command::new(script)
        .args(["--outputs", &outputs.to_string(), "--"])
        .arg(std::env::current_exe().unwrap())
        .args([
            name,
            "--exact",
            "--ignored",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(INNER, "1")
        .status();
    Some(match status {
        Ok(status) if status.code() == Some(77) => {
            eprintln!("skipped: no nested KWin desktop available");
            Ok(())
        }
        Ok(status) if status.success() => Ok(()),
        Ok(status) => Err(anyhow::anyhow!("nested test failed: {status}")),
        Err(error) => Err(error.into()),
    })
}

struct Process(Child);
impl Drop for Process {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[derive(Debug)]
struct Output {
    name: String,
    enabled: bool,
    width: u32,
    height: u32,
}

fn outputs() -> Result<Vec<Output>> {
    let json = Command::new("kscreen-doctor").arg("--json").output()?;
    ensure!(json.status.success(), "kscreen-doctor failed");
    let value: serde_json::Value = serde_json::from_slice(&json.stdout)?;
    let mut outputs: Vec<_> = value["outputs"]
        .as_array()
        .context("no outputs")?
        .iter()
        .filter(|output| output["connected"].as_bool() == Some(true))
        .map(|output| Output {
            name: output["name"].as_str().unwrap_or_default().to_owned(),
            enabled: output["enabled"].as_bool() == Some(true),
            width: output["size"]["width"].as_u64().unwrap_or(0) as u32,
            height: output["size"]["height"].as_u64().unwrap_or(0) as u32,
        })
        .collect();
    outputs.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(outputs)
}

fn ours(outputs: &[Output]) -> Vec<&Output> {
    outputs
        .iter()
        .filter(|output| {
            output
                .name
                .starts_with("Virtual-virtual-xdp-kde-dev.teleport.Host")
        })
        .collect()
}

/// The nested KWin's own outputs, standing in for physical monitors.
fn physical(outputs: &[Output]) -> Vec<&Output> {
    outputs
        .iter()
        .filter(|output| !output.name.starts_with("Virtual-virtual-"))
        .collect()
}

async fn eventually(what: &str, mut check: impl FnMut(&[Output]) -> bool) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let current = outputs()?;
        if check(&current) {
            return Ok(());
        }
        ensure!(Instant::now() < deadline, "{what}: outputs are {current:?}");
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

fn free_port() -> Result<String> {
    let socket = std::net::UdpSocket::bind("127.0.0.1:0")?;
    Ok(socket.local_addr()?.to_string())
}

fn start_host(directory: &Path, address: &str, args: &[&str]) -> Result<Process> {
    Ok(Process(
        Command::new(env!("CARGO_BIN_EXE_teleport"))
            .args(["host", "--source", "portal", "--encoder", "software"])
            .args(["--listen", address, "--pairing-file"])
            .arg(directory.join("pairing.json"))
            .args(args)
            .env("TELEPORT_PRIVACY_GUARD_SECS", "3")
            .stdout(Stdio::inherit())
            .spawn()?,
    ))
}

type Input = std::sync::Arc<std::sync::Mutex<(moq_net::track::Producer, u64)>>;

fn send(input: &Input, event: protocol::Event) -> Result<()> {
    let mut input = input.lock().unwrap();
    let sequence = input.1;
    let mut group = input.0.append_group()?;
    group.write_frame(
        moq_net::Timestamp::now(),
        serde_json::to_vec(&protocol::Input {
            sequence,
            event,
            motion: None,
        })?,
    )?;
    group.finish()?;
    input.1 += 1;
    Ok(())
}

impl Drop for Client {
    fn drop(&mut self) {
        self.heartbeat.abort();
    }
}

struct Client {
    _session: moq_net::Session,
    _client: moq_native::Client,
    _controls: moq_net::broadcast::Producer,
    _outgoing: moq_net::origin::Producer,
    /// The input track and its next sequence number, shared with the
    /// heartbeat task (the host drops clients silent for 3 s).
    input: Input,
    heartbeat: tokio::task::JoinHandle<()>,
    desktop: protocol::Desktop,
    updates: moq_net::track::Subscriber,
    primary: moq_net::track::Subscriber,
    remote: moq_net::broadcast::Consumer,
}

impl Client {
    async fn connect(directory: &Path, host: &mut Process, address: &str) -> Result<Self> {
        let pairing_path = directory.join("pairing.json");
        let pairing = tokio::time::timeout(Duration::from_secs(30), async {
            loop {
                if let Ok(pairing) = protocol::Pairing::read(&pairing_path) {
                    return Ok::<_, anyhow::Error>(pairing);
                }
                ensure!(host.0.try_wait()?.is_none(), "host exited");
                tokio::time::sleep(Duration::from_millis(50)).await;
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
        let input =
            controls.create_track("input", moq_net::track::Info::default().with_ordered(true))?;
        let input: Input = std::sync::Arc::new(std::sync::Mutex::new((input, 0)));
        let client = config
            .init()?
            .with_publisher(&outgoing)
            .with_subscriber(incoming.clone());
        let session = client
            .connect(format!("moqt://{address}/teleport/{}?codec=h264", pairing.token).parse()?)
            .await?;
        let remote = tokio::time::timeout(
            Duration::from_secs(20),
            incoming.consume().announced_broadcast("desktop"),
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
        let updates = remote
            .track("updates")?
            .subscribe(moq_net::track::Subscription::default().with_ordered(true))
            .await?;
        let primary = remote.track("h264")?.subscribe(None).await?;
        Ok(Self {
            _session: session,
            _client: client,
            _controls: controls,
            _outgoing: outgoing,
            heartbeat: tokio::spawn({
                let input = input.clone();
                async move {
                    loop {
                        tokio::time::sleep(Duration::from_millis(500)).await;
                        if send(&input, protocol::Event::Ping).is_err() {
                            break;
                        }
                    }
                }
            }),
            input,
            desktop,
            updates,
            primary,
            remote,
        })
    }

    fn send(&mut self, event: protocol::Event) -> Result<()> {
        send(&self.input, event)
    }

    fn features(&mut self, privacy: bool, virtual_monitor: bool) -> Result<()> {
        self.send(protocol::Event::ClientFeatures {
            keyframe_requests: true,
            pointer_datagrams: false,
            local_cursor: true,
            rumble: false,
            cursor_positions: true,
            frame_timing: false,
            privacy,
            virtual_monitor,
            gamepad_lights: false,
            microphone: false,
            guests: false,
        })
    }

    fn display(&mut self, width: u32, height: u32, refresh_mhz: u32) -> Result<()> {
        self.send(protocol::Event::ClientDisplay(protocol::ClientDisplay {
            width,
            height,
            refresh_mhz,
            stream_width: None,
            max_fps: None,
            bitrate: None,
        }))
    }

    /// The next update matching `wanted`, sending heartbeats meanwhile.
    async fn until<T>(
        &mut self,
        what: &str,
        mut wanted: impl FnMut(&protocol::Update) -> Option<T>,
    ) -> Result<T> {
        let deadline = Instant::now() + Duration::from_secs(40);
        loop {
            ensure!(Instant::now() < deadline, "timed out waiting for {what}");
            let next = tokio::time::timeout(Duration::from_secs(1), async {
                loop {
                    let mut group = self.updates.recv_group().await?.context("updates ended")?;
                    if let Some(frame) =
                        protocol::read_frame(&mut group, protocol::MAX_CONTROL).await?
                    {
                        return Ok::<_, anyhow::Error>(serde_json::from_slice::<protocol::Update>(
                            &frame,
                        )?);
                    }
                }
            })
            .await;
            if let Ok(update) = next {
                let update = update?;
                if let protocol::Update::Notice { text } = &update {
                    eprintln!("host notice: {text}");
                }
                if let Some(found) = wanted(&update) {
                    return Ok(found);
                }
            }
        }
    }

    async fn frame(&mut self, floor: u64) -> Result<()> {
        tokio::time::timeout(Duration::from_secs(20), async {
            loop {
                let mut group = self.primary.recv_group().await?.context("video ended")?;
                if group.sequence >= floor
                    && protocol::read_frame(&mut group, 16 * 1024 * 1024)
                        .await?
                        .is_some()
                {
                    return Ok(());
                }
            }
        })
        .await?
    }
}

#[tokio::test]
#[ignore = "runs a nested headless KWin with its own portal"]
async fn virtual_displays_follow_the_client_take_input_and_go_away() -> Result<()> {
    if let Some(result) = nested(
        "virtual_displays_follow_the_client_take_input_and_go_away",
        1,
    ) {
        return result;
    }
    let directory = tempfile::tempdir()?;
    let address = free_port()?;

    // 1. `--virtual-display always`: the stream is a virtual output that
    // takes the client's size and refresh rate, and input lands on it.
    let first = directory.path().join("first");
    std::fs::create_dir(&first)?;
    let mut host = start_host(&first, &address, &["--virtual-display", "always"])?;
    let mut client = Client::connect(&first, &mut host, &address).await?;
    let desktop = client.desktop.clone();
    assert!(desktop.source.contains("virtual"), "{}", desktop.source);
    assert_eq!(desktop.monitors.len(), 1);
    assert!(desktop.monitors[0].virtual_display);
    assert!(!desktop.privacy_mode, "privacy mode is opt-in");
    assert_eq!(ours(&outputs()?).len(), 1);
    client.features(false, false)?;
    client.display(1600, 900, 75_000)?;
    let resized = client
        .until(
            "the virtual display at the client's size",
            |update| match update {
                protocol::Update::Desktop { desktop }
                    if (desktop.native_width, desktop.native_height) == (1600, 900) =>
                {
                    Some(desktop.clone())
                }
                _ => None,
            },
        )
        .await?;
    assert_eq!(resized.monitors[0].refresh_mhz / 1000, 75);
    client.frame(resized.video_start_group).await?;
    eventually("the virtual output at the client's size", |outputs| {
        let ours = ours(outputs);
        ours.len() == 1 && (ours[0].width, ours[0].height) == (1600, 900)
    })
    .await?;
    // The compositor's pointer, read back from the screencast's cursor
    // metadata, follows absolute motion on the virtual output.
    client.send(protocol::Event::Motion { x: 0.25, y: 0.75 })?;
    client
        .until(
            "the pointer on the virtual display",
            |update| match update {
                protocol::Update::CursorPosition { monitor: 0, x, y }
                    if (x - 0.25).abs() < 0.02 && (y - 0.75).abs() < 0.02 =>
                {
                    Some(())
                }
                _ => None,
            },
        )
        .await?;
    // Killed, the host's portal session and its output go too.
    host.0.kill()?;
    host.0.wait()?;
    drop(client);
    eventually("no virtual output after the host", |outputs| {
        ours(outputs).is_empty()
    })
    .await?;

    // 2. The default (`fallback`) next to a monitor: the monitor is
    // streamed, and a client can add a virtual monitor at its size.
    let second = directory.path().join("second");
    std::fs::create_dir(&second)?;
    let mut host = start_host(&second, &address, &[])?;
    let mut client = Client::connect(&second, &mut host, &address).await?;
    assert!(!client.desktop.monitors[0].virtual_display);
    assert!(client.desktop.virtual_monitor);
    assert!(
        ours(&outputs()?).is_empty(),
        "no virtual output while a monitor exists"
    );
    client.features(false, true)?;
    client.display(1440, 900, 60_000)?;
    let added = client
        .until(
            "the virtual monitor at the client's size",
            |update| match update {
                protocol::Update::Desktop { desktop }
                    if desktop.monitors.len() == 2
                        && (desktop.monitors[1].width, desktop.monitors[1].height)
                            == (1440, 900) =>
                {
                    Some(desktop.clone())
                }
                _ => None,
            },
        )
        .await?;
    assert!(added.monitors[1].virtual_display);
    eventually("the virtual monitor's output", |outputs| {
        let ours = ours(outputs);
        ours.len() == 1 && (ours[0].width, ours[0].height) == (1440, 900)
    })
    .await?;
    assert!(physical(&outputs()?).iter().all(|o| o.enabled));
    client.send(protocol::Event::MonitorStream {
        index: 1,
        enabled: true,
    })?;
    client
        .until("the virtual monitor stream", |update| {
            matches!(update, protocol::Update::MonitorStream { index: 1, .. }).then_some(())
        })
        .await?;
    let mut auxiliary = client
        .remote
        .track("monitor-1-h264")?
        .subscribe(None)
        .await?;
    tokio::time::timeout(Duration::from_secs(20), auxiliary.recv_group())
        .await??
        .context("no virtual monitor video")?;
    client.send(protocol::Event::MonitorMotion {
        index: 1,
        x: 0.5,
        y: 0.5,
    })?;
    client
        .until(
            "the pointer on the virtual monitor",
            |update| match update {
                protocol::Update::CursorPosition { monitor: 1, x, y }
                    if (x - 0.5).abs() < 0.02 && (y - 0.5).abs() < 0.02 =>
                {
                    Some(())
                }
                _ => None,
            },
        )
        .await?;
    // Disconnecting removes it; the host keeps streaming the monitor.
    drop(client);
    eventually("the virtual monitor removed", |outputs| {
        ours(outputs).is_empty()
    })
    .await?;
    assert!(host.0.try_wait()?.is_none(), "host still serving");
    Ok(())
}

#[tokio::test]
#[ignore = "runs a nested headless KWin with its own portal"]
async fn privacy_mode_turns_monitors_off_and_always_restores_them() -> Result<()> {
    if let Some(result) = nested(
        "privacy_mode_turns_monitors_off_and_always_restores_them",
        2,
    ) {
        return result;
    }
    let directory = tempfile::tempdir()?;
    let address = free_port()?;
    let state = PathBuf::from(std::env::var("XDG_RUNTIME_DIR")?).join(format!(
        "teleport-privacy-{}.json",
        address.rsplit(':').next().unwrap()
    ));
    let all_on = |outputs: &[Output]| {
        physical(outputs).len() == 2 && physical(outputs).iter().all(|o| o.enabled)
    };
    let private = |outputs: &[Output]| {
        physical(outputs).iter().all(|o| !o.enabled) && ours(outputs).len() == 1
    };
    let mut host = start_host(directory.path(), &address, &["--privacy-mode", "request"])?;

    // 1. A session asks for privacy: monitors off, video from a virtual
    // output; disconnecting turns them back on and removes it.
    let mut client = Client::connect(directory.path(), &mut host, &address).await?;
    assert!(client.desktop.privacy_mode);
    assert!(!client.desktop.privacy_active);
    assert!(!client.desktop.monitors[0].virtual_display);
    client.features(true, false)?;
    client.display(1280, 800, 60_000)?;
    let desktop = client
        .until("privacy mode", |update| match update {
            protocol::Update::Desktop { desktop } if desktop.privacy_active => {
                Some(desktop.clone())
            }
            _ => None,
        })
        .await?;
    assert_eq!(desktop.monitors.len(), 1);
    assert!(desktop.monitors[0].virtual_display);
    client.frame(desktop.video_start_group).await?;
    eventually("physical monitors off", private).await?;
    assert!(state.exists(), "restore state saved");
    drop(client);
    eventually("monitors back on after disconnect", |outputs| {
        all_on(outputs) && ours(outputs).is_empty()
    })
    .await?;
    assert!(!state.exists());

    // 2. A hung host (stopped, then killed): its guard turns the monitors
    // back on while the virtual output still exists.
    drop(host);
    let second = directory.path().join("second");
    std::fs::create_dir(&second)?;
    let mut host = start_host(&second, &address, &["--privacy-mode", "request"])?;
    let mut client = Client::connect(&second, &mut host, &address).await?;
    client.features(true, false)?;
    client
        .until("privacy mode again", |update| match update {
            protocol::Update::Desktop { desktop } if desktop.privacy_active => Some(()),
            _ => None,
        })
        .await?;
    eventually("physical monitors off again", private).await?;
    unsafe { libc::kill(host.0.id() as libc::pid_t, libc::SIGSTOP) };
    eventually("the guard restores a hung host's monitors", |outputs| {
        all_on(outputs) && ours(outputs).len() == 1
    })
    .await?;
    drop(host);
    drop(client);
    eventually("monitors on, virtual output gone", |outputs| {
        all_on(outputs) && ours(outputs).is_empty()
    })
    .await?;

    // 3. A killed host: the monitors come back (its guard, and KWin's own
    // layout once the virtual output goes) ...
    let third = directory.path().join("third");
    std::fs::create_dir(&third)?;
    let mut host = start_host(&third, &address, &["--privacy-mode", "request"])?;
    let mut client = Client::connect(&third, &mut host, &address).await?;
    client.features(true, false)?;
    client
        .until("privacy mode a third time", |update| match update {
            protocol::Update::Desktop { desktop } if desktop.privacy_active => Some(()),
            _ => None,
        })
        .await?;
    eventually("physical monitors off a third time", private).await?;
    host.0.kill()?;
    host.0.wait()?;
    drop(client);
    eventually("monitors back on after a killed host", |outputs| {
        all_on(outputs) && ours(outputs).is_empty()
    })
    .await?;
    eventually("state removed by the guard", |_| !state.exists()).await?;
    // ... and a later virtual display does not turn them off again through
    // the layout KWin stored with them off.
    let fourth = directory.path().join("fourth");
    std::fs::create_dir(&fourth)?;
    let _host = start_host(&fourth, &address, &["--virtual-display", "always"])?;
    eventually("a later virtual display with the monitors on", |outputs| {
        ours(outputs).len() == 1 && all_on(outputs)
    })
    .await?;
    tokio::time::sleep(Duration::from_secs(2)).await;
    ensure!(
        all_on(&outputs()?),
        "monitors turned off by a stored layout"
    );
    Ok(())
}
