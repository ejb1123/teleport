#![cfg(target_os = "linux")]
//! End-to-end desktop audio latency and dropouts (M8). A 440 Hz tone with a
//! loud 2 kHz tick every 500 ms is played into a private null sink; a real
//! host captures its monitor and a windowed client (Xvfb) plays into a second
//! null sink. Both monitors are recorded in one pipeline: the delay between a
//! tick on each is host capture + encode + network + decode + client playout,
//! and silent gaps in the tone on the played side are dropouts.
//!
//! Needs the user's PipeWire session (`pw-cli`, pipewire-pulse); the sinks
//! are virtual, never the default, and removed afterwards.
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
        let status = Command::new("pw-cli")
            .args([
                "create-node",
                "adapter",
                &format!(
                    "{{ factory.name=support.null-audio-sink node.name={name} \
                     media.class=Audio/Sink object.linger=true audio.position=[FL FR] \
                     priority.session=1 priority.driver=1 }}"
                ),
            ])
            .stdout(Stdio::null())
            .status()
            .expect("pw-cli is required for the audio latency test");
        assert!(status.success(), "could not create null sink {name}");
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
            let _ = Command::new("pw-cli")
                .args(["destroy", &id.to_string()])
                .stdout(Stdio::null())
                .status();
        }
    }
}

fn node_id(name: &str) -> Option<u32> {
    let output = Command::new("pw-cli").args(["ls", "Node"]).output().ok()?;
    let listing = String::from_utf8_lossy(&output.stdout);
    let mut id = None;
    for line in listing.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("id ") {
            id = rest.split(',').next().and_then(|id| id.parse().ok());
        } else if line == format!("node.name = \"{name}\"") {
            return id;
        }
    }
    None
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

#[test]
#[ignore = "requires a PipeWire session (pw-cli), Xvfb, UDP and GStreamer plugins; prints timings"]
fn audio_end_to_end_latency() {
    gst::init().unwrap();
    let tag = std::process::id();
    let source = NullSink::new(format!("teleport_latency_source_{tag}"));
    let output = NullSink::new(format!("teleport_latency_output_{tag}"));
    let temp = tempfile::tempdir().unwrap();
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
    let netem = std::env::var("TELEPORT_TEST_NETEM").unwrap_or_default();
    let loopback = std::env::var_os("TELEPORT_TEST_AUDIO_LOOPBACK").is_some();
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
    --audio-source "$SOURCE.monitor" --pairing-file "$DIR/pairing.json" >"$DIR/host.log" 2>&1 &
until [ -s "$DIR/pairing.json" ]; do sleep 0.05; done
sleep 0.3
PULSE_SINK="$OUTPUT" "$TELEPORT" client {address} --software-decoder \
    --pairing-file "$DIR/pairing.json" >"$DIR/client.log" 2>&1
"#
    );
    let mut command = if netem.is_empty() {
        Command::new("sh")
    } else {
        let mut command = Command::new("unshare");
        command.args(["-rn", "sh"]);
        command
    };
    let session = (!loopback).then(|| {
        Group(
            command
                .args(["-c", &script])
                .env(
                    "TELEPORT",
                    std::env::var_os("TELEPORT_TEST_BINARY")
                        .unwrap_or_else(|| env!("CARGO_BIN_EXE_teleport").into()),
                )
                .env("DIR", temp.path())
                .env("NETEM", &netem)
                .env("SOURCE", &source.name)
                .env("OUTPUT", &output.name)
                .env("DISPLAY", format!(":{}", number.trim()))
                .env("SDL_VIDEODRIVER", "x11")
                .env_remove("WAYLAND_DISPLAY")
                .process_group(0)
                .spawn()
                .unwrap(),
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

    let recorder = gst::parse::launch(&format!(
        "pulsesrc device={0}.monitor provide-clock=false ! audioconvert ! audio/x-raw,format=S16LE,rate={RATE},channels=1 ! appsink name=sent sync=false \
         pulsesrc device={1}.monitor provide-clock=false ! audioconvert ! audio/x-raw,format=S16LE,rate={RATE},channels=1 ! appsink name=played sync=false",
        source.name, output.name
    ))
    .unwrap()
    .downcast::<gst::Pipeline>()
    .unwrap();
    let mut recordings: Vec<Recording> = Vec::new();
    for name in ["sent", "played"] {
        let recorded = Recording::default();
        let store = recorded.clone();
        recorder
            .by_name(name)
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
    let player = PipelineGuard(
        gst::parse::launch(&format!(
            "audiomixer name=mix ! audio/x-raw,rate={RATE},channels=2 ! pulsesink device={} \
             audiotestsrc is-live=true wave=ticks freq=2000 tick-interval={} volume=0.9 ! audio/x-raw,rate={RATE},channels=1 ! mix. \
             audiotestsrc is-live=true wave=sine freq=440 volume=0.1 ! audio/x-raw,rate={RATE},channels=1 ! mix.",
            source.name,
            TICK_INTERVAL_MS * 1_000_000,
        ))
        .unwrap(),
    );
    recorder.set_state(gst::State::Playing).unwrap();
    player.0.set_state(gst::State::Playing).unwrap();
    std::thread::sleep(Duration::from_secs(12));
    drop(player);
    recorder.set_state(gst::State::Null).unwrap();
    let survived = session
        .map(|mut session| session.0.try_wait().unwrap().is_none())
        .unwrap_or(true);
    let client_log = std::fs::read_to_string(temp.path().join("client.log")).unwrap_or_default();
    assert!(
        survived,
        "session ended during the measurement:\n{client_log}"
    );

    let sent = onsets(&recordings[0].lock().unwrap());
    let played = onsets(&recordings[1].lock().unwrap());
    let mut delays: Vec<u64> = sent
        .iter()
        .filter_map(|&at| {
            played
                .iter()
                .find(|&&heard| heard > at && heard - at < 450_000_000)
                .map(|heard| (heard - at) / 1000)
        })
        .collect();
    // Skip the first second of the tone: the played side may still be starting.
    let from = played
        .first()
        .copied()
        .unwrap_or(u64::MAX)
        .saturating_add(1_000_000_000);
    let gaps = dropouts(&recordings[1].lock().unwrap(), from);
    let control_gaps = dropouts(&recordings[0].lock().unwrap(), from);
    delays.sort_unstable();
    let at = |p: usize| delays.get((delays.len() * p / 100).min(delays.len().saturating_sub(1)));
    let ms = |us: Option<&u64>| us.copied().unwrap_or(0) as f64 / 1000.0;
    println!(
        "AUDIO netem=[{netem}] ticks={} heard={} latency_ms p50={:.1} p95={:.1} min={:.1} max={:.1} dropouts={gaps} (source {control_gaps})",
        sent.len(),
        delays.len(),
        ms(at(50)),
        ms(at(95)),
        ms(delays.first()),
        ms(delays.last()),
    );
    assert!(sent.len() >= 15, "too few ticks recorded: {}", sent.len());
    assert!(
        delays.len() * 10 >= sent.len() * 9,
        "ticks lost on the way: {} of {}",
        delays.len(),
        sent.len()
    );
}

struct PipelineGuard(gst::Element);
impl Drop for PipelineGuard {
    fn drop(&mut self) {
        let _ = self.0.set_state(gst::State::Null);
    }
}
