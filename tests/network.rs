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
TELEPORT_TEST_PATTERN={pattern} "$TELEPORT" host --source test --encoder software --listen 127.0.0.1:4443 \
    --width 640 --fps {FPS} --bitrate {bitrate} --pairing-file "$DIR/pairing.json" \
    >/dev/null 2>"$DIR/host.log" &
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
    --headless-frames {FRAMES} --software-decoder --bitrate {bitrate}
"#,
        netem = profile.netem,
        pattern = profile.pattern,
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
    outcome
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
    });
    // The stall must cover the outage, and video must resume promptly after.
    assert!(outcome.max_frame_gap_ms >= 1500, "{outcome:?}");
    assert!(outcome.max_frame_gap_ms < 4000, "{outcome:?}");
}
