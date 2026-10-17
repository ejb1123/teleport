#![cfg(target_os = "linux")]
//! Real host/client sessions over impaired links. Each profile runs both
//! processes inside a private, unprivileged network namespace whose loopback
//! is shaped with `tc netem` (delay applies in each direction, so RTT is twice
//! the delay). Needs `unshare -rn`, `ip` and `tc` (iproute2, in the dev shell).
//! A `rate` limit shapes both directions together (loopback is one device);
//! `limit` is the bottleneck queue in packets.
//!
//! The summary lines for every profile are printed even on success, so runs
//! can be compared before and after transport/encoder changes. Both ends run
//! with `TELEPORT_TEST_WALL_CLOCK=1`, so the client measures capture→ready per
//! frame (one machine, one clock). `TELEPORT_TEST_LOG_DIR` keeps both logs.

use std::{collections::BTreeMap, process::Command};

const FRAMES: u32 = 180;
const FPS: u32 = 30;

struct Profile {
    name: &'static str,
    netem: &'static str,
    /// Bottleneck capacity in kbps (`rate` in `netem`). Utilisation is the
    /// tail rate over the lower of this and `bitrate`.
    capacity: Option<u32>,
    bitrate: u32,
    /// Another netem setting (e.g. `loss 100%`) for this many milliseconds,
    /// starting 3 s into the session; then `netem` again.
    burst: Option<(&'static str, u32)>,
    /// `snow` fills the configured bitrate; `ball` compresses to almost nothing.
    pattern: &'static str,
    /// Stream ~500 Hz pointer motion and time input-path probe round trips.
    input_latency: bool,
    /// Ask the host to change the stream width every 1.5 s (encoder rebuilds).
    reconfigure: bool,
    /// Stream width; height follows the 16:9 test source.
    width: u32,
    frames: u32,
    /// Host `--flash-on-input`, client `--smoke-flash`: click→changed frame.
    flash: bool,
    /// Extra client arguments.
    client_args: &'static str,
}

const PROFILE: Profile = Profile {
    name: "",
    netem: "delay 1ms",
    capacity: None,
    bitrate: 2000,
    burst: None,
    pattern: "ball",
    input_latency: false,
    reconfigure: false,
    width: 640,
    frames: FRAMES,
    flash: false,
    client_args: "",
};

#[derive(Debug)]
struct Outcome {
    frames: u64,
    span_ms: u64,
    max_frame_gap_ms: u64,
    freezes: u64,
    freeze_ms: u64,
    latency_p50_us: u64,
    latency_p95_us: u64,
    decoder_recoveries: u64,
    skipped_groups: u64,
    decode_errors: u64,
    received_bytes: u64,
    /// Bytes and time over the second half of the run (after convergence).
    tail_bytes: u64,
    tail_ms: u64,
    width: u64,
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
    sizes: Sizes,
    /// Host bitrate changes and resolution/FPS steps.
    rate_changes: usize,
    steps: usize,
    /// End-to-end stage (p50, p95) µs from host stamps and the
    /// clock estimate, in `E2E_STAGES` order.
    e2e: Vec<(i64, i64)>,
    e2e_frames: u64,
    /// Frames whose estimate was checked against the wall clock, and the
    /// estimate's error: median and largest absolute, µs.
    clock_checked: u64,
    clock_error_p50_us: i64,
    clock_error_max_us: i64,
    clock_rtt_us: u64,
    flash_presses: u64,
    flash_p50_us: u64,
    flash_p95_us: u64,
    /// Session warnings the client raised (G22), as logged.
    warnings: Vec<String>,
}

const E2E_STAGES: [&str; 6] = [
    "capture_encode",
    "encode_send",
    "network",
    "receive_decode",
    "decode_present",
    "total",
];

/// Encoded packet sizes over every encoder the host ran.
#[derive(Debug, Default)]
struct Sizes {
    delta_frames: u64,
    delta_bytes: u64,
    keyframes: u64,
    key_bytes: u64,
    max_key_bytes: u64,
    max_delta_bytes: u64,
}

impl Sizes {
    fn mean_delta(&self) -> u64 {
        self.delta_bytes.checked_div(self.delta_frames).unwrap_or(0)
    }
    fn mean_key(&self) -> u64 {
        self.key_bytes.checked_div(self.keyframes).unwrap_or(0)
    }
}

impl Outcome {
    fn fps(&self) -> f64 {
        self.frames.saturating_sub(1) as f64 * 1000.0 / self.span_ms.max(1) as f64
    }
    fn mbps(&self) -> f64 {
        self.received_bytes as f64 * 8.0 / 1000.0 / self.span_ms.max(1) as f64
    }
    fn tail_mbps(&self) -> f64 {
        self.tail_bytes as f64 * 8.0 / 1000.0 / self.tail_ms.max(1) as f64
    }
}

fn run(profile: &Profile) -> Outcome {
    let temp = tempfile::tempdir().unwrap();
    let burst = profile
        .burst
        .map(|(netem, ms)| {
            format!(
                "( sleep 3; tc qdisc change dev lo root netem {netem}; sleep {}; \
                 tc qdisc change dev lo root netem {} ) &",
                f64::from(ms) / 1000.0,
                profile.netem
            )
        })
        .unwrap_or_default();
    let script = format!(
        r#"set -eu
ip link set lo up
tc qdisc add dev lo root netem {netem}
TELEPORT_TEST_PATTERN={pattern} "$TELEPORT" host --source test --encoder "${{TELEPORT_TEST_ENCODER:-software}}" --listen 127.0.0.1:4443 \
    --width {width} --fps {FPS} --bitrate {bitrate} --pairing-file "$DIR/pairing.json" {host_flash} \
    >"$DIR/host.log" 2>&1 &
host=$!
trap 'kill $host 2>/dev/null || true' EXIT
tries=0
until [ -s "$DIR/pairing.json" ]; do
    tries=$((tries + 1)); [ $tries -lt 300 ] || {{ echo "host not ready" >&2; exit 90; }}
    sleep 0.05
done
sleep 0.3
{burst}
"$TELEPORT" client 127.0.0.1:4443 --pairing-file "$DIR/pairing.json" \
    --headless-frames {frames} --software-decoder --bitrate {bitrate} {input} {reconfigure} {client_flash} {client_args}
# Let the host notice the disconnect and log its encoder summary.
sleep 1.5
"#,
        netem = profile.netem,
        pattern = profile.pattern,
        width = profile.width,
        frames = profile.frames,
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
        host_flash = if profile.flash {
            "--flash-on-input"
        } else {
            ""
        },
        client_flash = if profile.flash { "--smoke-flash" } else { "" },
        client_args = profile.client_args,
    );
    let output = Command::new("unshare")
        .args(["-rn", "sh", "-c", &script])
        .env("TELEPORT", teleport_binary())
        .env("DIR", temp.path())
        .env("TELEPORT_TEST_WALL_CLOCK", "1")
        .output()
        .expect("unshare is required for network-condition tests");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let host_log = std::fs::read_to_string(temp.path().join("host.log")).unwrap_or_default();
    if let Some(directory) = std::env::var_os("TELEPORT_TEST_LOG_DIR") {
        let directory = std::path::Path::new(&directory);
        let _ = std::fs::write(
            directory.join(format!("{}.host.log", profile.name)),
            &host_log,
        );
        let _ = std::fs::write(
            directory.join(format!("{}.client.log", profile.name)),
            stdout.as_bytes(),
        );
    }
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
    let count = |needle: &str| {
        host_log
            .lines()
            .filter(|line| line.contains(needle))
            .count()
    };
    let outcome = Outcome {
        frames: field("frames"),
        span_ms: field("span_ms"),
        max_frame_gap_ms: field("max_frame_gap_ms"),
        freezes: field("freezes"),
        freeze_ms: field("freeze_ms"),
        latency_p50_us: field("latency_p50_us"),
        latency_p95_us: field("latency_p95_us"),
        decoder_recoveries: field("decoder_recoveries"),
        skipped_groups: field("skipped_groups"),
        decode_errors: field("decode_errors"),
        received_bytes: field("received_bytes"),
        tail_bytes: field("tail_bytes"),
        tail_ms: field("tail_ms"),
        width: field("width"),
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
        sizes: host_sizes(&host_log),
        rate_changes: count("adapted video bitrate"),
        steps: count("stepped video"),
        e2e: E2E_STAGES
            .iter()
            .map(|stage| {
                let pair = fields
                    .get(&format!("e2e_{stage}_us"))
                    .unwrap_or_else(|| panic!("[{}] summary missing {stage}", profile.name));
                let (p50, p95) = pair.split_once('/').unwrap();
                (p50.parse().unwrap(), p95.parse().unwrap())
            })
            .collect(),
        e2e_frames: field("e2e_frames"),
        clock_checked: field("clock_checked"),
        clock_error_p50_us: signed(&fields, "clock_error_p50_us"),
        clock_error_max_us: signed(&fields, "clock_error_max_us"),
        clock_rtt_us: field("clock_rtt_us"),
        flash_presses: field("flash_presses"),
        flash_p50_us: field("flash_p50_us"),
        flash_p95_us: field("flash_p95_us"),
        warnings: stdout
            .lines()
            .filter(|line| line.contains("session warning"))
            .filter_map(|line| {
                line.split_once("text=")
                    .map(|(_, text)| text.trim_matches('"').to_owned())
            })
            .collect(),
    };
    let utilisation = profile
        .capacity
        .map(|kbps| {
            format!(
                " util={:.0}%",
                outcome.tail_mbps() * 100_000.0 / f64::from(kbps.min(profile.bitrate))
            )
        })
        .unwrap_or_default();
    println!(
        "NETWORK {:<10} netem=[{}] fps={:.1} mbps={:.2} tail_mbps={:.2}{utilisation} max_gap_ms={} freezes={} freeze_ms={} latency_p50_ms={:.1} latency_p95_ms={:.1} recoveries={} skipped_groups={} decode_errors={} width={} rate_changes={} steps={}",
        profile.name,
        profile.netem,
        outcome.fps(),
        outcome.mbps(),
        outcome.tail_mbps(),
        outcome.max_frame_gap_ms,
        outcome.freezes,
        outcome.freeze_ms,
        outcome.latency_p50_us as f64 / 1000.0,
        outcome.latency_p95_us as f64 / 1000.0,
        outcome.decoder_recoveries,
        outcome.skipped_groups,
        outcome.decode_errors,
        outcome.width,
        outcome.rate_changes,
        outcome.steps,
    );
    println!(
        "SIZES   {:<10} keyframes={} mean_key_bytes={} max_key_bytes={} mean_delta_bytes={} max_delta_bytes={} max_key_ratio={:.1}",
        profile.name,
        outcome.sizes.keyframes,
        outcome.sizes.mean_key(),
        outcome.sizes.max_key_bytes,
        outcome.sizes.mean_delta(),
        outcome.sizes.max_delta_bytes,
        outcome.sizes.max_key_bytes as f64 / outcome.sizes.mean_delta().max(1) as f64,
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
    let ms = |us: i64| us as f64 / 1000.0;
    println!(
        "E2E     {:<10} frames={} {} clock_rtt_ms={:.2} clock_checked={} clock_error_p50_ms={:.2} clock_error_max_ms={:.2}",
        profile.name,
        outcome.e2e_frames,
        E2E_STAGES
            .iter()
            .zip(&outcome.e2e)
            .map(|(stage, (p50, p95))| format!("{stage}_ms={:.1}/{:.1}", ms(*p50), ms(*p95)))
            .collect::<Vec<_>>()
            .join(" "),
        outcome.clock_rtt_us as f64 / 1000.0,
        outcome.clock_checked,
        ms(outcome.clock_error_p50_us),
        ms(outcome.clock_error_max_us),
    );
    println!(
        "WARN    {:<10} {}",
        profile.name,
        if outcome.warnings.is_empty() {
            "none".to_owned()
        } else {
            outcome.warnings.join(" | ")
        }
    );
    if profile.flash {
        println!(
            "FLASH   {:<10} netem=[{}] presses={} click_to_frame_p50_ms={:.1} p95_ms={:.1}",
            profile.name,
            profile.netem,
            outcome.flash_presses,
            outcome.flash_p50_us as f64 / 1000.0,
            outcome.flash_p95_us as f64 / 1000.0,
        );
    }
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
        .and_then(|line| line_field(line, name))
        .unwrap_or(0)
}

fn line_field(line: &str, name: &str) -> Option<u64> {
    line.split_whitespace()
        .filter_map(|pair| pair.split_once('='))
        .find(|(key, _)| *key == name)
        .and_then(|(_, value)| value.parse().ok())
}

/// Packet sizes summed over every "encoder summary" line.
fn host_sizes(log: &str) -> Sizes {
    let mut sizes = Sizes::default();
    for line in log.lines().filter(|line| line.contains("encoder summary")) {
        let value = |name| line_field(line, name).unwrap_or(0);
        sizes.delta_frames += value("delta_frames");
        sizes.delta_bytes += value("delta_bytes");
        sizes.keyframes += value("keyframes");
        sizes.key_bytes += value("key_bytes");
        sizes.max_key_bytes = sizes.max_key_bytes.max(value("max_key_bytes"));
        sizes.max_delta_bytes = sizes.max_delta_bytes.max(value("max_delta_bytes"));
    }
    sizes
}

fn signed(fields: &BTreeMap<String, String>, name: &str) -> i64 {
    fields
        .get(name)
        .and_then(|value| value.parse().ok())
        .unwrap_or_else(|| panic!("summary missing {name}"))
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
        ..PROFILE
    });
    assert!(outcome.fps() >= 25.0, "{outcome:?}");
    assert!(outcome.max_frame_gap_ms < 250, "{outcome:?}");
    // G22: a clean link raises no network or decoder warnings (the test
    // host does encode in software, which is reported).
    assert!(
        outcome
            .warnings
            .iter()
            .all(|text| text.starts_with("Host is encoding in software")),
        "{:?}",
        outcome.warnings
    );
}

#[test]
#[ignore = "requires unprivileged user/network namespaces, iproute2 and GStreamer plugins"]
fn high_latency_link_keeps_full_rate() {
    let outcome = run(&Profile {
        name: "rtt100",
        netem: "delay 50ms",
        ..PROFILE
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
        pattern: "snow",
        ..PROFILE
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
        pattern: "snow",
        ..PROFILE
    });
}

#[test]
#[ignore = "requires unprivileged user/network namespaces, iproute2 and GStreamer plugins"]
fn jitter_and_reordering_survive() {
    let outcome = run(&Profile {
        name: "jitter",
        netem: "delay 20ms 15ms distribution normal",
        bitrate: 4000,
        pattern: "snow",
        ..PROFILE
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
        capacity: Some(3000),
        bitrate: 6000,
        pattern: "snow",
        ..PROFILE
    });
}

#[test]
#[ignore = "requires unprivileged user/network namespaces, iproute2 and GStreamer plugins"]
fn two_second_blackout_recovers_without_reconnect() {
    let outcome = run(&Profile {
        name: "blackout",
        netem: "delay 10ms",
        burst: Some(("loss 100%", 2000)),
        ..PROFILE
    });
    // The stall must cover the outage, and video must resume promptly after.
    assert!(outcome.max_frame_gap_ms >= 1500, "{outcome:?}");
    assert!(outcome.max_frame_gap_ms < 4000, "{outcome:?}");
    // G22: blamed on the network, not the host's capture.
    assert!(
        outcome
            .warnings
            .iter()
            .any(|text| text.starts_with("Network: video stalled")),
        "{:?}",
        outcome.warnings
    );
}

/// G6/G7 profiles: 20 s of `snow` (always fills its target) so the rate
/// controller converges and the tail half measures the steady state.
const LONG: Profile = Profile {
    pattern: "snow",
    frames: 600,
    ..PROFILE
};

/// 720p into a 10 Mbps bottleneck with a ~120 ms queue (150 packets).
#[test]
#[ignore = "requires unprivileged user/network namespaces, iproute2 and GStreamer plugins"]
fn capped_ten_megabit_link_is_used_without_queueing() {
    let outcome = run(&Profile {
        name: "cap10",
        netem: "delay 15ms rate 10mbit limit 150",
        capacity: Some(10_000),
        bitrate: 16_000,
        width: 1280,
        ..LONG
    });
    assert!(outcome.fps() >= 20.0, "{outcome:?}");
}

/// 720p into a 5 Mbps bottleneck with a ~120 ms queue (75 packets).
#[test]
#[ignore = "requires unprivileged user/network namespaces, iproute2 and GStreamer plugins"]
fn capped_five_megabit_link_is_used_without_queueing() {
    let outcome = run(&Profile {
        name: "cap5",
        netem: "delay 15ms rate 5mbit limit 75",
        capacity: Some(5_000),
        bitrate: 16_000,
        width: 1280,
        ..LONG
    });
    assert!(outcome.fps() >= 20.0, "{outcome:?}");
}

/// 720p at 8 Mbps into 1.2 Mbps: below the 720p floor, so the host should
/// step the resolution down rather than starve every frame.
#[test]
#[ignore = "requires unprivileged user/network namespaces, iproute2 and GStreamer plugins"]
fn narrow_link_steps_resolution_down() {
    let outcome = run(&Profile {
        name: "cap1",
        netem: "delay 15ms rate 1200kbit limit 30",
        capacity: Some(1_200),
        bitrate: 8_000,
        width: 1280,
        ..LONG
    });
    assert!(outcome.fps() >= 15.0, "{outcome:?}");
    // G22: the user is told the link is the problem.
    assert!(
        outcome
            .warnings
            .iter()
            .any(|text| text.starts_with("Network:")),
        "{:?}",
        outcome.warnings
    );
}

/// Random loss on a 10 Mbps bottleneck: loss alone must not starve the rate.
#[test]
#[ignore = "requires unprivileged user/network namespaces, iproute2 and GStreamer plugins"]
fn capped_link_with_random_loss() {
    let outcome = run(&Profile {
        name: "cap10loss2",
        netem: "delay 15ms rate 10mbit limit 150 loss 2%",
        capacity: Some(10_000),
        bitrate: 8_000,
        width: 1280,
        ..LONG
    });
    assert!(outcome.fps() >= 20.0, "{outcome:?}");
}

/// Wi-Fi-like: 20–30 Mbps with aggregation bursts (netem `slot`), jitter and
/// a little loss.
#[test]
#[ignore = "requires unprivileged user/network namespaces, iproute2 and GStreamer plugins"]
fn wifi_like_link() {
    let outcome = run(&Profile {
        name: "wifi",
        netem: "delay 5ms 5ms distribution paretonormal rate 25mbit limit 300 slot 2ms 12ms loss 0.5%",
        capacity: Some(25_000),
        bitrate: 8_000,
        width: 1280,
        ..LONG
    });
    assert!(outcome.fps() >= 20.0, "{outcome:?}");
}

/// A 400 ms loss burst (a Wi-Fi roam or a microwave) on an otherwise clean
/// 20 ms RTT link: recovery time is the longest gap minus the burst.
#[test]
#[ignore = "requires unprivileged user/network namespaces, iproute2 and GStreamer plugins"]
fn loss_burst_recovers_quickly() {
    let outcome = run(&Profile {
        name: "burst",
        netem: "delay 10ms",
        burst: Some(("delay 10ms loss 100%", 400)),
        bitrate: 4_000,
        width: 1280,
        ..LONG
    });
    assert!(outcome.max_frame_gap_ms < 3000, "{outcome:?}");
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
        input_latency: true,
        reconfigure,
        ..PROFILE
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
        bitrate: 8000,
        pattern: "snow",
        width: 1920,
        ..PROFILE
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

/// G3: the client's NTP-style clock-offset estimate (control probes, RTT/2,
/// min-filtered) against the Unix clock both processes share here. Each
/// frame's estimated capture→present is compared with the wall-clock one;
/// the difference is the estimate's error. 40 ms RTT, symmetric.
#[test]
#[ignore = "requires unprivileged user/network namespaces, iproute2 and GStreamer plugins"]
fn clock_offset_estimate_matches_the_wall_clock() {
    let outcome = run(&Profile {
        name: "clock",
        netem: "delay 20ms",
        ..PROFILE
    });
    assert!(outcome.clock_checked >= 100, "{outcome:?}");
    assert!(
        outcome.clock_error_p50_us.abs() < 1_500,
        "estimate off by {} µs: {outcome:?}",
        outcome.clock_error_p50_us
    );
    // The stages see the shaped link: ~20 ms one way.
    let network_p50 = outcome.e2e[2].0;
    assert!((15_000..40_000).contains(&network_p50), "{outcome:?}");
    let total_p50 = outcome.e2e[5].0;
    assert!(total_p50 > network_p50, "{outcome:?}");
}

/// `--flash-on-input` host and `--smoke-flash` client: software
/// click→changed frame (no display), the part of input→photon a glass-to-glass
/// rig cannot split out.
#[test]
#[ignore = "requires unprivileged user/network namespaces, iproute2 and GStreamer plugins"]
fn flash_on_input_follows_clicks() {
    let outcome = run(&Profile {
        name: "flash",
        flash: true,
        frames: 300,
        ..PROFILE
    });
    assert!(outcome.flash_presses >= 10, "{outcome:?}");
    assert!(outcome.flash_p50_us < 200_000, "{outcome:?}");
}
