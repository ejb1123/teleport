#![cfg(target_os = "linux")]
//! End-to-end audio latency and dropouts, through private PipeWire null
//! sinks (never the default device, removed afterwards).
//!
//! Desktop audio (M8): a 440 Hz tone with a loud 2 kHz tick every 500 ms is
//! played into a null sink; a real host captures its monitor and a windowed
//! client (Xvfb) plays into a second null sink. Both monitors are recorded
//! in one pipeline: the delay between a tick on each is host capture +
//! encode + network + decode + client playout, and silent gaps in the tone
//! on the played side are dropouts.
//!
//! Microphone (G20): the same signal is the client's microphone (its
//! `--mic-device` records a null sink's monitor) and is recorded from the
//! host's virtual microphone. Echo cancellation: the host's desktop audio,
//! played by the client, is also the client's microphone (a perfect
//! acoustic echo); the virtual microphone is measured with and without it.
//!
//! Needs the user's PipeWire session (`pw-cli`, pipewire-pulse).
//! `TELEPORT_TEST_NETEM` (e.g. `delay 20ms 10ms`) runs host and client in a
//! private network namespace shaped with `tc netem`, like tests/network.rs.
//! `TELEPORT_TEST_AUDIO_LOOPBACK=1` replaces Teleport with a plain
//! pulsesrc → pulsesink loop, to see what the audio server adds by itself.
//! `TELEPORT_TEST_BINARY` runs another build (e.g. a release binary).

use gstreamer as gst;
use gstreamer::prelude::*;
use std::{
    io::BufRead,
    os::unix::process::CommandExt,
    process::{Child, Command, Stdio},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

const RATE: u64 = 48_000;
const TICK_INTERVAL_MS: u64 = 500;

struct Process(Child);
impl Drop for Process {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// The host/client script: a process group, killed as a whole.
struct Group(Child);
impl Drop for Group {
    fn drop(&mut self) {
        // SAFETY: signals only the process group this test created.
        unsafe {
            libc::kill(-(self.0.id() as i32), libc::SIGKILL);
        }
        let _ = self.0.wait();
    }
}

/// A virtual PipeWire sink, destroyed on drop.
struct NullSink {
    name: String,
    id: Option<u32>,
}

impl NullSink {
    fn new(name: String) -> Self {
        create_node(&format!(
            "{{ factory.name=support.null-audio-sink node.name={name} \
             media.class=Audio/Sink object.linger=true audio.position=[FL FR] \
             priority.session=1 priority.driver=1 }}"
        ));
        let mut sink = Self { name, id: None };
        let deadline = Instant::now() + Duration::from_secs(5);
        while sink.id.is_none() {
            assert!(Instant::now() < deadline, "null sink never appeared");
            sink.id = node_id(&sink.name);
            std::thread::sleep(Duration::from_millis(50));
        }
        sink
    }
}

impl Drop for NullSink {
    fn drop(&mut self) {
        if let Some(id) = self.id {
            destroy(id);
        }
    }
}

fn create_node(properties: &str) {
    let status = Command::new("pw-cli")
        .args(["create-node", "adapter", properties])
        .stdout(Stdio::null())
        .status()
        .expect("pw-cli is required for the audio tests");
    assert!(status.success(), "could not create {properties}");
}

fn destroy(id: u32) {
    let _ = Command::new("pw-cli")
        .args(["destroy", &id.to_string()])
        .stdout(Stdio::null())
        .status();
}

fn node_id(name: &str) -> Option<u32> {
    node_ids(|node| node == name).first().copied()
}

/// Ids of nodes whose name matches.
fn node_ids(matches: impl Fn(&str) -> bool) -> Vec<u32> {
    let Ok(output) = Command::new("pw-cli").args(["ls", "Node"]).output() else {
        return Vec::new();
    };
    let listing = String::from_utf8_lossy(&output.stdout);
    let mut id = None;
    let mut found = Vec::new();
    for line in listing.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("id ") {
            id = rest.split(',').next().and_then(|id| id.parse().ok());
        } else if let Some(name) = line.strip_prefix("node.name = \"")
            && matches(name.trim_end_matches('"'))
            && let Some(id) = id
        {
            found.push(id);
        }
    }
    found
}

/// Wait until `condition` holds, for at most `limit`.
fn eventually(limit: Duration, condition: impl Fn() -> bool) -> bool {
    let deadline = Instant::now() + limit;
    while Instant::now() < deadline {
        if condition() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    condition()
}

/// The user's default devices (the tests must never change them).
fn defaults() -> String {
    let output = Command::new("pw-metadata")
        .args(["-n", "default", "0"])
        .output()
        .map(|output| String::from_utf8_lossy(&output.stdout).into_owned())
        .unwrap_or_default();
    let mut lines: Vec<_> = output
        .lines()
        .filter(|line| line.contains("default.audio."))
        .map(str::to_owned)
        .collect();
    lines.sort();
    lines.join("\n")
}

type Recording = Arc<Mutex<Vec<(u64, Vec<i16>)>>>;

/// Times (ns, recorder running time) at which a tick starts.
fn onsets(recorded: &[(u64, Vec<i16>)]) -> Vec<u64> {
    let mut onsets = Vec::new();
    let mut last_loud: Option<u64> = None;
    for (pts, samples) in recorded {
        for (index, sample) in samples.iter().enumerate() {
            if sample.unsigned_abs() < 12_000 {
                continue;
            }
            let at = pts + index as u64 * 1_000_000_000 / RATE;
            if last_loud.is_none_or(|last| at - last > 200_000_000) {
                onsets.push(at);
            }
            last_loud = Some(at);
        }
    }
    onsets
}

/// Silent gaps of at least 2 ms in the tone after `from` (ns). The tone is
/// near zero for under 0.1 ms per zero crossing, so longer silence is a gap
/// (dropped or late audio played as silence, or a resync).
fn dropouts(recorded: &[(u64, Vec<i16>)], from: u64) -> usize {
    let mut gaps = 0;
    let mut quiet = 0;
    for (pts, samples) in recorded {
        for (index, sample) in samples.iter().enumerate() {
            if pts + index as u64 * 1_000_000_000 / RATE < from {
                continue;
            }
            if sample.unsigned_abs() < 300 {
                quiet += 1;
                if quiet == RATE as usize / 500 {
                    gaps += 1;
                }
            } else {
                quiet = 0;
            }
        }
    }
    gaps
}

/// RMS of the samples recorded after `from` (ns), leaving out 150 ms from
/// each of `ticks` (onsets): the echo left between near-end ticks.
fn rms(recorded: &[(u64, Vec<i16>)], from: u64, ticks: &[u64]) -> f64 {
    let (mut sum, mut count) = (0.0, 0_u64);
    for (pts, samples) in recorded {
        for (index, sample) in samples.iter().enumerate() {
            let at = pts + index as u64 * 1_000_000_000 / RATE;
            if at >= from
                && !ticks
                    .iter()
                    .any(|&tick| at + 20_000_000 >= tick && at < tick + 150_000_000)
            {
                sum += f64::from(*sample).powi(2);
                count += 1;
            }
        }
    }
    (sum / count.max(1) as f64).sqrt()
}

struct PipelineGuard(gst::Element);
impl Drop for PipelineGuard {
    fn drop(&mut self) {
        let _ = self.0.set_state(gst::State::Null);
    }
}

/// Record mono S16 from each PulseAudio source, in one pipeline (one clock).
struct Recorder {
    pipeline: gst::Pipeline,
    recordings: Vec<Recording>,
}

impl Recorder {
    fn new(sources: &[&str]) -> Self {
        let description = sources
            .iter()
            .enumerate()
            .map(|(index, source)| {
                format!(
                    "pulsesrc device={source} provide-clock=false ! audioconvert ! audio/x-raw,format=S16LE,rate={RATE},channels=1 ! appsink name=recorded{index} sync=false"
                )
            })
            .collect::<Vec<_>>()
            .join(" ");
        let pipeline = gst::parse::launch(&description)
            .unwrap()
            .downcast::<gst::Pipeline>()
            .unwrap();
        let mut recordings = Vec::new();
        for index in 0..sources.len() {
            let recorded = Recording::default();
            let store = recorded.clone();
            pipeline
                .by_name(&format!("recorded{index}"))
                .unwrap()
                .downcast::<gstreamer_app::AppSink>()
                .unwrap()
                .set_callbacks(
                    gstreamer_app::AppSinkCallbacks::builder()
                        .new_sample(move |sink| {
                            let sample = sink.pull_sample().map_err(|_| gst::FlowError::Eos)?;
                            let buffer = sample.buffer().ok_or(gst::FlowError::Error)?;
                            let pts = buffer.pts().ok_or(gst::FlowError::Error)?.nseconds();
                            let map = buffer.map_readable().map_err(|_| gst::FlowError::Error)?;
                            let samples = map
                                .chunks_exact(2)
                                .map(|pair| i16::from_le_bytes([pair[0], pair[1]]))
                                .collect();
                            store.lock().unwrap().push((pts, samples));
                            Ok(gst::FlowSuccess::Ok)
                        })
                        .build(),
                );
            recordings.push(recorded);
        }
        pipeline.set_state(gst::State::Playing).unwrap();
        Self {
            pipeline,
            recordings,
        }
    }

    fn stop(&self) -> Vec<Vec<(u64, Vec<i16>)>> {
        self.pipeline.set_state(gst::State::Null).unwrap();
        self.recordings
            .iter()
            .map(|recording| recording.lock().unwrap().clone())
            .collect()
    }
}

/// Play the test signal into a sink: a quiet 440 Hz `tone` and/or a loud
/// 2 kHz tick every 500 ms (`ticks`).
fn play(sink: &str, tone: bool, ticks: bool) -> PipelineGuard {
    let mut sources = String::new();
    if ticks {
        sources += &format!(
            "audiotestsrc is-live=true wave=ticks freq=2000 tick-interval={} volume=0.9 ! audio/x-raw,rate={RATE},channels=1 ! mix. ",
            TICK_INTERVAL_MS * 1_000_000,
        );
    }
    if tone {
        sources += &format!(
            "audiotestsrc is-live=true wave=sine freq=440 volume=0.1 ! audio/x-raw,rate={RATE},channels=1 ! mix."
        );
    }
    let player = gst::parse::launch(&format!(
        "audiomixer name=mix ! audio/x-raw,rate={RATE},channels=2 ! pulsesink device={sink} {sources}"
    ))
    .unwrap();
    player.set_state(gst::State::Playing).unwrap();
    PipelineGuard(player)
}

/// Delay of each tick from `sent` to `received` (µs), and the dropouts in
/// `received` after its first second.
fn latency(sent: &[(u64, Vec<i16>)], received: &[(u64, Vec<i16>)]) -> (Vec<u64>, usize, usize) {
    let sent_onsets = onsets(sent);
    let heard = onsets(received);
    let mut delays: Vec<u64> = sent_onsets
        .iter()
        .filter_map(|&at| {
            heard
                .iter()
                .find(|&&heard| heard > at && heard - at < 450_000_000)
                .map(|heard| (heard - at) / 1000)
        })
        .collect();
    // Skip the first second of the tone: the played side may still be starting.
    let from = heard
        .first()
        .copied()
        .unwrap_or(u64::MAX)
        .saturating_add(1_000_000_000);
    delays.sort_unstable();
    (delays, dropouts(received, from), sent_onsets.len())
}

fn report(label: &str, netem: &str, delays: &[u64], ticks: usize, gaps: usize) {
    let at = |p: usize| delays.get((delays.len() * p / 100).min(delays.len().saturating_sub(1)));
    let ms = |us: Option<&u64>| us.copied().unwrap_or(0) as f64 / 1000.0;
    println!(
        "{label} netem=[{netem}] ticks={ticks} heard={} latency_ms p50={:.1} p95={:.1} min={:.1} max={:.1} dropouts={gaps}",
        delays.len(),
        ms(at(50)),
        ms(at(95)),
        ms(delays.first()),
        ms(delays.last()),
    );
    assert!(ticks >= 15, "too few ticks recorded: {ticks}");
    assert!(
        delays.len() * 10 >= ticks * 9,
        "ticks lost on the way: {} of {ticks}",
        delays.len(),
    );
}

/// An Xvfb display for windowed clients.
fn display() -> (Process, String) {
    let mut display = Process(
        Command::new("Xvfb")
            .args([
                "-displayfd",
                "1",
                "-screen",
                "0",
                "1280x800x24",
                "-nolisten",
                "tcp",
            ])
            .stdout(Stdio::piped())
            .spawn()
            .unwrap(),
    );
    let mut number = String::new();
    std::io::BufReader::new(display.0.stdout.take().unwrap())
        .read_line(&mut number)
        .unwrap();
    (display, format!(":{}", number.trim()))
}

fn binary() -> std::ffi::OsString {
    std::env::var_os("TELEPORT_TEST_BINARY")
        .unwrap_or_else(|| env!("CARGO_BIN_EXE_teleport").into())
}

/// Host and windowed client in one process group (in a shaped network
/// namespace with `netem`). Their pids are written to `host.pid` and
/// `client.pid` in `directory`; the client plays into `output`.
fn session(
    directory: &std::path::Path,
    display: &str,
    netem: &str,
    host_args: &str,
    client_args: &str,
    output: &str,
) -> Group {
    // A private namespace has its own loopback; otherwise take a free port.
    let address = if netem.is_empty() {
        let socket = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        socket.local_addr().unwrap().to_string()
    } else {
        "127.0.0.1:47431".into()
    };
    let script = format!(
        r#"set -eu
if [ -n "$NETEM" ]; then ip link set lo up; tc qdisc add dev lo root netem $NETEM; fi
"$TELEPORT" host --source test --encoder software --width 640 --fps 30 --listen {address} \
    {host_args} --pairing-file "$DIR/pairing.json" >"$DIR/host.log" 2>&1 &
echo $! >"$DIR/host.pid"
echo {address} >"$DIR/address"
until [ -s "$DIR/pairing.json" ]; do sleep 0.05; done
sleep 0.3
PULSE_SINK="$OUTPUT" "$TELEPORT" client {address} --software-decoder {client_args} \
    --pairing-file "$DIR/pairing.json" >"$DIR/client.log" 2>&1 &
echo $! >"$DIR/client.pid"
wait
"#
    );
    let mut command = if netem.is_empty() {
        Command::new("sh")
    } else {
        let mut command = Command::new("unshare");
        command.args(["-rn", "sh"]);
        command
    };
    Group(
        command
            .args(["-c", &script])
            .env("TELEPORT", binary())
            .env("DIR", directory)
            .env("NETEM", netem)
            .env("OUTPUT", output)
            .env("DISPLAY", display)
            .env("SDL_VIDEODRIVER", "x11")
            .env_remove("WAYLAND_DISPLAY")
            .process_group(0)
            .spawn()
            .unwrap(),
    )
}

fn pid(directory: &std::path::Path, name: &str) -> i32 {
    let path = directory.join(format!("{name}.pid"));
    assert!(
        eventually(Duration::from_secs(10), || path.exists()),
        "{name} did not start"
    );
    std::fs::read_to_string(path)
        .unwrap()
        .trim()
        .parse()
        .unwrap()
}

fn logs(directory: &std::path::Path) -> String {
    ["host.log", "client.log"]
        .iter()
        .map(|name| {
            format!(
                "--- {name}\n{}",
                std::fs::read_to_string(directory.join(name)).unwrap_or_default()
            )
        })
        .collect()
}

#[test]
#[ignore = "requires a PipeWire session (pw-cli), Xvfb, UDP and GStreamer plugins; prints timings"]
fn audio_end_to_end_latency() {
    gst::init().unwrap();
    let tag = std::process::id();
    let source = NullSink::new(format!("teleport_latency_source_{tag}"));
    let output = NullSink::new(format!("teleport_latency_output_{tag}"));
    let temp = tempfile::tempdir().unwrap();
    let (_display, display) = display();
    let netem = std::env::var("TELEPORT_TEST_NETEM").unwrap_or_default();
    let loopback = std::env::var_os("TELEPORT_TEST_AUDIO_LOOPBACK").is_some();
    let session = (!loopback).then(|| {
        session(
            temp.path(),
            &display,
            &netem,
            &format!("--audio-source {}.monitor", source.name),
            "",
            &output.name,
        )
    });
    let _loop = loopback.then(|| {
        let pipeline = gst::parse::launch(&format!(
            "pulsesrc device={}.monitor buffer-time=20000 latency-time=10000 ! pulsesink device={}",
            source.name, output.name
        ))
        .unwrap();
        pipeline.set_state(gst::State::Playing).unwrap();
        PipelineGuard(pipeline)
    });
    // Let the session and its audio settle before measuring.
    std::thread::sleep(Duration::from_secs(5));
    let recorder = Recorder::new(&[
        &format!("{}.monitor", source.name),
        &format!("{}.monitor", output.name),
    ]);
    let player = play(&source.name, true, true);
    std::thread::sleep(Duration::from_secs(12));
    drop(player);
    let recorded = recorder.stop();
    let survived = session
        .map(|mut session| session.0.try_wait().unwrap().is_none())
        .unwrap_or(true);
    assert!(
        survived,
        "session ended during the measurement:\n{}",
        logs(temp.path())
    );
    let (delays, gaps, ticks) = latency(&recorded[0], &recorded[1]);
    let from = onsets(&recorded[1])
        .first()
        .copied()
        .unwrap_or(u64::MAX)
        .saturating_add(1_000_000_000);
    println!("(source dropouts {})", dropouts(&recorded[0], from));
    report("AUDIO", &netem, &delays, ticks, gaps);
}

/// The host's virtual microphone nodes (`src/virtual_mic.rs`).
fn virtual_microphone(host: i32) -> Vec<u32> {
    let suffix = format!(".{host}.");
    node_ids(|name| name.starts_with("teleport_microphone") && name.contains(&suffix))
}

#[test]
#[ignore = "requires a PipeWire session (pw-cli), Xvfb, UDP and GStreamer plugins; prints timings"]
fn microphone_end_to_end_latency() {
    gst::init().unwrap();
    let tag = std::process::id();
    let defaults_before = defaults();
    let spoken = NullSink::new(format!("teleport_mic_spoken_{tag}"));
    let output = NullSink::new(format!("teleport_mic_output_{tag}"));
    // A virtual microphone left by a host that no longer runs.
    let stale = format!("teleport_microphone.{}.1", i32::MAX - 1);
    create_node(&format!(
        "{{ factory.name=support.null-audio-sink node.name={stale} \
         media.class=Audio/Sink object.linger=true audio.position=[FL FR] \
         priority.session=1 priority.driver=1 teleport.host.pid={} }}",
        i32::MAX - 1
    ));
    assert!(eventually(Duration::from_secs(5), || node_id(&stale).is_some()));
    let temp = tempfile::tempdir().unwrap();
    let (_display, display) = display();
    let netem = std::env::var("TELEPORT_TEST_NETEM").unwrap_or_default();
    let mut session = session(
        temp.path(),
        &display,
        &netem,
        "--microphone",
        &format!("--mic --mic-device {}.monitor", spoken.name),
        &output.name,
    );
    let host = pid(temp.path(), "host");
    let cleaned = eventually(Duration::from_secs(10), || node_id(&stale).is_none());
    if let Some(id) = node_id(&stale) {
        destroy(id);
    }
    assert!(
        cleaned,
        "the stale virtual microphone was not removed:\n{}",
        logs(temp.path())
    );
    assert!(
        eventually(Duration::from_secs(20), || virtual_microphone(host).len()
            >= 2),
        "no virtual microphone:\n{}",
        logs(temp.path())
    );
    let source = format!("teleport_microphone.{host}.1");
    assert!(
        node_id(&source).is_some(),
        "virtual source {source} missing"
    );
    assert_eq!(
        defaults(),
        defaults_before,
        "the virtual microphone changed a default device"
    );
    std::thread::sleep(Duration::from_secs(3));
    let recorder = Recorder::new(&[&format!("{}.monitor", spoken.name), &source]);
    let player = play(&spoken.name, true, true);
    std::thread::sleep(Duration::from_secs(12));
    drop(player);
    let recorded = recorder.stop();
    assert!(
        session.0.try_wait().unwrap().is_none(),
        "session ended during the measurement:\n{}",
        logs(temp.path())
    );
    let (delays, gaps, ticks) = latency(&recorded[0], &recorded[1]);
    report("MICROPHONE", &netem, &delays, ticks, gaps);

    // Session end: a client that quits takes the virtual microphone with it.
    let client = pid(temp.path(), "client");
    // SAFETY: the client this test started.
    unsafe {
        libc::kill(client, libc::SIGTERM);
    }
    assert!(
        eventually(Duration::from_secs(15), || virtual_microphone(host)
            .is_empty()),
        "virtual microphone left after the session:\n{}",
        logs(temp.path())
    );
    if !netem.is_empty() {
        return;
    }
    // Host crash: a second session, then SIGKILL the host.
    let address = std::fs::read_to_string(temp.path().join("address")).unwrap();
    let address = address.trim();
    let _client = Process(
        Command::new(binary())
            .args([
                "client",
                address,
                "--software-decoder",
                "--mic",
                "--mic-device",
            ])
            .arg(format!("{}.monitor", spoken.name))
            .arg("--pairing-file")
            .arg(temp.path().join("pairing.json"))
            .env("PULSE_SINK", &output.name)
            .env("DISPLAY", &display)
            .env("SDL_VIDEODRIVER", "x11")
            .env_remove("WAYLAND_DISPLAY")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    assert!(
        eventually(Duration::from_secs(20), || virtual_microphone(host).len()
            >= 2),
        "no virtual microphone for the second session:\n{}",
        logs(temp.path())
    );
    // SAFETY: the host this test started.
    unsafe {
        libc::kill(host, libc::SIGKILL);
    }
    assert!(
        eventually(Duration::from_secs(5), || virtual_microphone(host)
            .is_empty()),
        "virtual microphone left after the host was killed"
    );
    assert_eq!(defaults(), defaults_before);
    session.0.kill().ok();
}

struct Echo {
    /// What the client's speakers played (echo plus near-end ticks).
    played: f64,
    /// The virtual microphone between ticks: the echo left.
    echo: f64,
    /// Near-end ticks sent and heard on the virtual microphone.
    ticks: usize,
    heard: usize,
    /// Loudest sample on the virtual microphone while the near end talks.
    peak: u16,
}

/// The host's desktop audio (a 440 Hz tone), played by the client into a
/// "room" sink, is the client's microphone together with near-end ticks
/// played into the same room: the tone on the virtual microphone is echo,
/// the ticks are speech that must get through (double talk).
fn echo(echo_cancel: bool) -> Echo {
    let tag = std::process::id();
    let desktop = NullSink::new(format!("teleport_echo_desktop_{tag}"));
    let room = NullSink::new(format!("teleport_echo_room_{tag}"));
    let temp = tempfile::tempdir().unwrap();
    let (_display, display) = display();
    let _session = session(
        temp.path(),
        &display,
        "",
        &format!("--microphone --audio-source {}.monitor", desktop.name),
        &format!(
            "--mic --mic-device {}.monitor{}",
            room.name,
            if echo_cancel { "" } else { " --no-echo-cancel" }
        ),
        &room.name,
    );
    let host = pid(temp.path(), "host");
    assert!(
        eventually(Duration::from_secs(20), || virtual_microphone(host).len()
            >= 2),
        "no virtual microphone:\n{}",
        logs(temp.path())
    );
    let source = format!("teleport_microphone.{host}.1");
    let far = play(&desktop.name, true, false);
    let recorder = Recorder::new(&[&format!("{}.monitor", room.name), &source]);
    // The canceller adapts within a few seconds; then the near end talks.
    std::thread::sleep(Duration::from_secs(7));
    let near = play(&room.name, false, true);
    std::thread::sleep(Duration::from_secs(8));
    let recorded = recorder.stop();
    drop((far, near));
    let from = recorded[1]
        .first()
        .map_or(0, |(pts, _)| *pts)
        .saturating_add(9_000_000_000);
    let sent: Vec<u64> = onsets(&recorded[0])
        .into_iter()
        .filter(|&at| at >= from)
        .collect();
    let heard: Vec<u64> = onsets(&recorded[1])
        .into_iter()
        .filter(|&at| at >= from)
        .collect();
    // Echo is measured away from the ticks on both sides.
    let all: Vec<u64> = sent.iter().chain(&heard).copied().collect();
    Echo {
        played: rms(&recorded[0], from, &all),
        echo: rms(&recorded[1], from, &all),
        ticks: sent.len(),
        heard: heard.len(),
        peak: recorded[1]
            .iter()
            .flat_map(|(_, samples)| samples)
            .map(|sample| sample.unsigned_abs())
            .max()
            .unwrap_or(0),
    }
}

#[test]
#[ignore = "requires a PipeWire session (pw-cli), Xvfb, UDP and GStreamer plugins (webrtcdsp); prints levels"]
fn microphone_echo_cancellation() {
    gst::init().unwrap();
    let without = echo(false);
    let with = echo(true);
    let db = |level: f64| 20.0 * (level.max(1.0) / 32768.0).log10();
    println!(
        "ECHO played={:.1} dBFS virtual mic echo without={:.1} dBFS with={:.1} dBFS reduction={:.1} dB; near-end ticks heard without={}/{} with={}/{} (peak {} / {})",
        db(without.played),
        db(without.echo),
        db(with.echo),
        db(without.echo) - db(with.echo),
        without.heard,
        without.ticks,
        with.heard,
        with.ticks,
        without.peak,
        with.peak,
    );
    assert!(without.echo > 100.0, "the echo path carried no audio");
    assert!(
        db(without.echo) - db(with.echo) >= 10.0,
        "echo cancellation removed less than 10 dB"
    );
    // Near-end speech during continuous far-end audio (double talk) is only
    // reported: WebRTC's suppressor removes much of it (docs/audio.md).
    assert!(
        with.ticks >= 8 && without.heard * 10 >= without.ticks * 8,
        "the near-end ticks did not reach the virtual microphone"
    );
}
