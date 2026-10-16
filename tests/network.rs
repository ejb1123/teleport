#![cfg(target_os = "linux")]
//! Real host/client sessions over impaired links. Each profile runs both
//! processes inside a private, unprivileged network namespace whose loopback
//! is shaped with `tc netem` (delay applies in each direction, so RTT is twice
//! the delay). Needs `unshare -rn`, `ip` and `tc` (iproute2, in the dev shell).
//!
//! The summary line for every profile is printed even on success, so runs can
//! be compared before and after transport/encoder changes.

use std::{collections::BTreeMap, process::Command};

const FRAMES: u32 = 180;
const FPS: u32 = 30;

struct Profile {
    name: &'static str,
    netem: &'static str,
    bitrate: u32,
    /// Seconds of 100% loss starting 3 s into the session.
    blackout: Option<u32>,
    /// `snow` fills the configured bitrate; `ball` compresses to almost nothing.
    pattern: &'static str,
    /// Stream ~500 Hz pointer motion and time input-path probe round trips.
    input_latency: bool,
    /// Ask the host to change the stream width every 1.5 s (encoder rebuilds).
    reconfigure: bool,
    /// Stream width; height follows the 16:9 test source.
    width: u32,
}

#[derive(Debug)]
struct Outcome {
    frames: u64,
    span_ms: u64,
    max_frame_gap_ms: u64,
    decoder_recoveries: u64,
    skipped_groups: u64,
    decode_errors: u64,
    received_bytes: u64,
    probes: u64,
    probe_p50_us: u64,
    probe_p95_us: u64,
    probe_max_us: u64,
    host_prepare_us: u64,
    host_encode_us: u64,
    mean_ready_us: u64,
    mean_queue_us: u64,
    mean_decode_us: u64,
    mean_convert_us: u64,
}

impl Outcome {
    fn fps(&self) -> f64 {
        self.frames.saturating_sub(1) as f64 * 1000.0 / self.span_ms.max(1) as f64
    }
    fn mbps(&self) -> f64 {
        self.received_bytes as f64 * 8.0 / 1000.0 / self.span_ms.max(1) as f64
    }
}

fn run(profile: &Profile) -> Outcome {
    let temp = tempfile::tempdir().unwrap();
    let blackout = profile
        .blackout
        .map(|seconds| {
            format!(
                "( sleep 3; tc qdisc change dev lo root netem loss 100%; sleep {seconds}; \
                 tc qdisc change dev lo root netem {} ) &",
                profile.netem
            )
        })
        .unwrap_or_default();
    let script = format!(
        r#"set -eu
ip link set lo up
tc qdisc add dev lo root netem {netem}
TELEPORT_TEST_PATTERN={pattern} "$TELEPORT" host --source test --encoder "${{TELEPORT_TEST_ENCODER:-software}}" --listen 127.0.0.1:4443 \
    --width {width} --fps {FPS} --bitrate {bitrate} --pairing-file "$DIR/pairing.json" \
    >"$DIR/host.log" 2>&1 &
host=$!
trap 'kill $host 2>/dev/null || true' EXIT
tries=0
until [ -s "$DIR/pairing.json" ]; do
    tries=$((tries + 1)); [ $tries -lt 300 ] || {{ echo "host not ready" >&2; exit 90; }}
    sleep 0.05
done
sleep 0.3
{blackout}
"$TELEPORT" client 127.0.0.1:4443 --pairing-file "$DIR/pairing.json" \
    --headless-frames {FRAMES} --software-decoder --bitrate {bitrate} {input} {reconfigure}
# Let the host notice the disconnect and log its encoder summary.
sleep 0.5
"#,
        netem = profile.netem,
        pattern = profile.pattern,
        width = profile.width,
        input = if profile.input_latency {
            "--smoke-input-latency"
        } else {
            ""
        },
        reconfigure = if profile.reconfigure {
            "--smoke-reconfigure"
        } else {
            ""
        },
        bitrate = profile.bitrate,
    );
    let output = Command::new("unshare")
        .args(["-rn", "sh", "-c", &script])
        .env("TELEPORT", teleport_binary())
        .env("DIR", temp.path())
        .output()
        .expect("unshare is required for network-condition tests");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let host_log = std::fs::read_to_string(temp.path().join("host.log")).unwrap_or_default();
    assert!(
        output.status.success() && stdout.contains("SMOKE PASS"),
        "[{}] session did not survive ({})\nclient stdout:\n{stdout}\nclient stderr:\n{}\nhost:\n{}",
        profile.name,
        output.status,
        String::from_utf8_lossy(&output.stderr),
        host_log
            .lines()
            .rev()
            .take(20)
            .collect::<Vec<_>>()
            .join("\n"),
    );
    let fields = summary_fields(&stdout);
    let field = |name: &str| -> u64 {
        fields
            .get(name)
            .and_then(|value| value.parse().ok())
            .unwrap_or_else(|| panic!("[{}] summary missing {name}: {stdout}", profile.name))
    };
    let outcome = Outcome {
        frames: field("frames"),
        span_ms: field("span_ms"),
        max_frame_gap_ms: field("max_frame_gap_ms"),
        decoder_recoveries: field("decoder_recoveries"),
        skipped_groups: field("skipped_groups"),
        decode_errors: field("decode_errors"),
        received_bytes: field("received_bytes"),
        probes: field("probes"),
        probe_p50_us: field("probe_p50_us"),
        probe_p95_us: field("probe_p95_us"),
        probe_max_us: field("probe_max_us"),
        host_prepare_us: host_field(&host_log, "mean_prepare_us"),
        host_encode_us: host_field(&host_log, "mean_encode_us"),
        mean_ready_us: field("mean_ready_us"),
        mean_queue_us: field("mean_queue_us"),
        mean_decode_us: field("mean_decode_us"),
        mean_convert_us: field("mean_convert_us"),
    };
    println!(
        "NETWORK {:<10} netem=[{}] fps={:.1} mbps={:.2} max_gap_ms={} recoveries={} skipped_groups={} decode_errors={}",
        profile.name,
        profile.netem,
        outcome.fps(),
        outcome.mbps(),
        outcome.max_frame_gap_ms,
        outcome.decoder_recoveries,
        outcome.skipped_groups,
        outcome.decode_errors,
    );
    println!(
        "DECODE  {:<10} ready_ms={:.2} queue_ms={:.2} decode_ms={:.2} convert_ms={:.2} host_prepare_ms={:.2} host_encode_ms={:.2}",
        profile.name,
        outcome.mean_ready_us as f64 / 1000.0,
        outcome.mean_queue_us as f64 / 1000.0,
        outcome.mean_decode_us as f64 / 1000.0,
        outcome.mean_convert_us as f64 / 1000.0,
        outcome.host_prepare_us as f64 / 1000.0,
        outcome.host_encode_us as f64 / 1000.0,
    );
    if profile.input_latency {
        println!(
            "INPUT   {:<10} netem=[{}] probes={} p50_ms={:.1} p95_ms={:.1} max_ms={:.1}",
            profile.name,
            profile.netem,
            outcome.probes,
            outcome.probe_p50_us as f64 / 1000.0,
            outcome.probe_p95_us as f64 / 1000.0,
            outcome.probe_max_us as f64 / 1000.0,
        );
    }
    outcome
}

/// Mean from the host's last "encoder summary" line (0 if absent).
fn host_field(log: &str, name: &str) -> u64 {
    log.lines()
        .rev()
        .find(|line| line.contains("encoder summary"))
        .and_then(|line| {
            line.split_whitespace()
                .filter_map(|pair| pair.split_once('='))
                .find(|(key, _)| *key == name)
                .and_then(|(_, value)| value.parse().ok())
        })
        .unwrap_or(0)
}

/// Parses `key=value` pairs from the client's SMOKE PASS log line.
fn summary_fields(stdout: &str) -> BTreeMap<String, String> {
    let line = stdout
        .lines()
        .find(|line| line.contains("SMOKE PASS"))
        .unwrap_or_default();
    line.split_whitespace()
        .filter_map(|pair| pair.split_once('='))
        .map(|(key, value)| (key.to_owned(), value.trim_matches('"').to_owned()))
        .collect()
}

fn teleport_binary() -> std::ffi::OsString {
    std::env::var_os("TELEPORT_TEST_BINARY")
        .unwrap_or_else(|| env!("CARGO_BIN_EXE_teleport").into())
}

#[test]
#[ignore = "requires unprivileged user/network namespaces, iproute2 and GStreamer plugins"]
fn clean_link_streams_at_full_rate() {
    let outcome = run(&Profile {
        name: "clean",
        netem: "delay 1ms",
        bitrate: 2000,
        blackout: None,
        pattern: "ball",
        input_latency: false,
        reconfigure: false,
        width: 640,
    });
    assert!(outcome.fps() >= 25.0, "{outcome:?}");
    assert!(outcome.max_frame_gap_ms < 250, "{outcome:?}");
}

#[test]
#[ignore = "requires unprivileged user/network namespaces, iproute2 and GStreamer plugins"]
fn high_latency_link_keeps_full_rate() {
    let outcome = run(&Profile {
        name: "rtt100",
        netem: "delay 50ms",
        bitrate: 2000,
        blackout: None,
        pattern: "ball",
        input_latency: false,
        reconfigure: false,
        width: 640,
    });
    assert!(outcome.fps() >= 25.0, "{outcome:?}");
}

#[test]
#[ignore = "requires unprivileged user/network namespaces, iproute2 and GStreamer plugins"]
fn one_percent_loss_survives_with_usable_rate() {
    let outcome = run(&Profile {
        name: "loss1",
        netem: "delay 20ms loss 1%",
        bitrate: 4000,
        blackout: None,
        pattern: "snow",
        input_latency: false,
        reconfigure: false,
        width: 640,
    });
    assert!(outcome.fps() >= 20.0, "{outcome:?}");
}

#[test]
#[ignore = "requires unprivileged user/network namespaces, iproute2 and GStreamer plugins"]
fn five_percent_loss_survives() {
    run(&Profile {
        name: "loss5",
        netem: "delay 20ms loss 5%",
        bitrate: 4000,
        blackout: None,
        pattern: "snow",
        input_latency: false,
        reconfigure: false,
        width: 640,
    });
}

#[test]
#[ignore = "requires unprivileged user/network namespaces, iproute2 and GStreamer plugins"]
fn jitter_and_reordering_survive() {
    let outcome = run(&Profile {
        name: "jitter",
        netem: "delay 20ms 15ms distribution normal",
        bitrate: 4000,
        blackout: None,
        pattern: "snow",
        input_latency: false,
        reconfigure: false,
        width: 640,
    });
    assert!(outcome.fps() >= 20.0, "{outcome:?}");
}

#[test]
#[ignore = "requires unprivileged user/network namespaces, iproute2 and GStreamer plugins"]
fn bitrate_above_link_capacity_survives() {
    // Configured 6 Mbps into a 3 Mbps link: adaptation must keep it alive.
    run(&Profile {
        name: "capped",
        netem: "delay 10ms rate 3mbit",
        bitrate: 6000,
        blackout: None,
        pattern: "snow",
        input_latency: false,
        reconfigure: false,
        width: 640,
    });
}

#[test]
#[ignore = "requires unprivileged user/network namespaces, iproute2 and GStreamer plugins"]
fn two_second_blackout_recovers_without_reconnect() {
    let outcome = run(&Profile {
        name: "blackout",
        netem: "delay 10ms",
        bitrate: 2000,
        blackout: Some(2),
        pattern: "ball",
        input_latency: false,
        reconfigure: false,
        width: 640,
    });
    // The stall must cover the outage, and video must resume promptly after.
    assert!(outcome.max_frame_gap_ms >= 1500, "{outcome:?}");
    assert!(outcome.max_frame_gap_ms < 4000, "{outcome:?}");
}

/// Key-path latency while the pointer streams at a gaming poll rate. Probes
/// share the ordered input path with key events, so their round trip is what
/// a key press waits for (plus the return trip) when the mouse is moving.
fn input_profile(name: &'static str, netem: &'static str) -> Outcome {
    input_profile_with(name, netem, false)
}

fn input_profile_with(name: &'static str, netem: &'static str, reconfigure: bool) -> Outcome {
    let outcome = run(&Profile {
        name,
        netem,
        bitrate: 2000,
        blackout: None,
        pattern: "ball",
        input_latency: true,
        reconfigure,
        width: 640,
    });
    assert!(outcome.probes >= 50, "too few probe replies: {outcome:?}");
    outcome
}

#[test]
#[ignore = "requires unprivileged user/network namespaces, iproute2 and GStreamer plugins"]
fn input_round_trip_with_streaming_pointer_on_clean_link() {
    let outcome = input_profile("input", "delay 20ms");
    // 40 ms RTT link: probes should not queue behind pointer motion.
    assert!(outcome.probe_p95_us < 80_000, "{outcome:?}");
}

#[test]
#[ignore = "requires unprivileged user/network namespaces, iproute2 and GStreamer plugins"]
fn input_round_trip_with_streaming_pointer_under_loss() {
    input_profile("input_loss", "delay 20ms loss 5%");
}

#[test]
#[ignore = "requires unprivileged user/network namespaces, iproute2 and GStreamer plugins"]
fn full_hd_clean_link_decode_path() {
    // Client-side receive→ready cost at 1080p (software decode, NV12 output).
    let outcome = run(&Profile {
        name: "hd",
        netem: "delay 1ms",
        bitrate: 8000,
        blackout: None,
        pattern: "snow",
        input_latency: false,
        reconfigure: false,
        width: 1920,
    });
    assert!(outcome.fps() >= 25.0, "{outcome:?}");
}

#[test]
#[ignore = "requires unprivileged user/network namespaces, iproute2 and GStreamer plugins"]
fn input_keeps_flowing_while_the_encoder_restarts() {
    // H7: the host rebuilds its encoder for every width change. The probe
    // tail shows how long input waits behind that. TELEPORT_TEST_ENCODER
    // selects another encoder kind (e.g. nvidia, vaapi).
    input_profile_with("reconfig", "delay 1ms", true);
}
