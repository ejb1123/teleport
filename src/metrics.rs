//! Local recording of a session's metrics, for review later or to attach
//! to a bug report (docs/performance.md, "Recording metrics").
//!
//! `teleport client --record-metrics` writes one file per session under
//! [`directory`] (or `--metrics-dir`): one JSON object per line, flushed as
//! it is written, so a crash loses at most the summary. The records are
//!
//! - `session`, first: the app version, the platform, the transport and
//!   whether the run was headless;
//! - `stream`, whenever the stream's description changes: codec, size,
//!   rate and bitrate, the decoder, the host's encoder and the renderer;
//! - `sample`, once a second: the numbers the F8 overlay shows, as rates and
//!   deltas over that second, with the warnings raised in it;
//! - `summary`, last: whole-run totals and percentiles, and why it ended.
//!
//! Nothing in a file identifies the host or the user: no address, name,
//! fingerprint or pixels. Nothing is sent anywhere; `teleport metrics
//! export` makes one file to attach to an issue by hand. Writing is best
//! effort, like the host's events file: a failure logs once and the session
//! goes on.
use crate::stats::Snapshot;
use anyhow::{Context, Result, bail, ensure};
use serde_json::{Map, Value, json};
use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

/// Where sessions are recorded without `--metrics-dir`.
pub fn directory() -> Result<PathBuf> {
    let home = std::env::var_os("HOME").context("HOME is not set")?;
    #[cfg(target_os = "macos")]
    let path = PathBuf::from(home).join("Library/Application Support/Teleport/metrics");
    #[cfg(not(target_os = "macos"))]
    let path = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(home).join(".local/state"))
        .join("teleport/metrics");
    Ok(path)
}

fn resolve(dir: Option<&Path>) -> Result<PathBuf> {
    dir.map_or_else(directory, |dir| Ok(dir.to_path_buf()))
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_millis() as u64)
}

/// `2026-11-01T22:33:34Z` for a Unix time in milliseconds.
pub fn utc(ms: u64) -> String {
    let secs = ms / 1000;
    let (hour, minute, second) = ((secs / 3600) % 24, (secs / 60) % 60, secs % 60);
    // Civil date from days since the epoch (Howard Hinnant's algorithm).
    let z = (secs / 86_400) as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z")
}

/// The constant facts of one session.
pub struct Session<'a> {
    /// `direct`, `relay` or `p2p`.
    pub transport: &'a str,
    pub headless: bool,
    /// The host's capture source (`portal`, `kms`, `test`, ...).
    pub source: &'a str,
}

/// What the stream looks like; a `stream` record goes out when it changes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Stream {
    pub codec: String,
    pub dynamic_range: String,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub bitrate_kbps: u32,
    pub decoder: String,
    pub encoder: String,
    pub renderer: String,
}

/// The stage names of [`crate::stats::Stages`], in field order.
const STAGES: [&str; 6] = [
    "capture_encode",
    "encode_send",
    "network",
    "receive_decode",
    "decode_present",
    "total",
];

/// `{"capture_encode": [p50, p95], ...}` in µs.
fn stages_json(stages: &[(i64, i64); 6]) -> Value {
    let mut object = Map::new();
    for (name, (p50, p95)) in STAGES.iter().zip(stages) {
        object.insert((*name).into(), json!([p50, p95]));
    }
    Value::Object(object)
}

/// One second of the session, from the client's counters.
pub struct Second<'a> {
    pub snapshot: &'a Snapshot,
    /// Frames shown so far (decoded, when headless).
    pub displayed_frames: u64,
    /// The latest control round trip.
    pub rtt: Option<Duration>,
    /// The queueing delay last reported to the host, when it sends one.
    pub queue_delay_ms: Option<u32>,
    /// The host's encode time for its latest frame, µs (0 without telemetry).
    pub host_encode_us: u64,
    /// The longest gap between arriving frames this second, ms.
    pub stall_ms: u64,
    /// Recent-window (p50, p95) per stage, µs; `None` without host frame
    /// timing or before the clock estimate.
    pub e2e: Option<[(i64, i64); 6]>,
    /// Warnings raised this second (`overlay::Warnings`).
    pub warnings: &'a [String],
}

/// The whole run, written last.
pub struct Summary<'a> {
    pub snapshot: &'a Snapshot,
    pub displayed_frames: u64,
    /// Whole-run (p50, p95) per stage, µs.
    pub e2e: Option<[(i64, i64); 6]>,
    pub e2e_frames: usize,
    /// (mean, standard deviation, p95) of present intervals, µs.
    pub frame_time_us: (i64, i64, i64),
    /// Warnings raised in the session.
    pub warnings: usize,
    /// `closed` (the user ended it), `reconnect` (the client is retrying)
    /// or `error`.
    pub reason: &'a str,
}

/// Writes one session's file. Dropping it unfinished writes a summary that
/// says so, so a session that ends in an error still has one.
pub struct Recorder {
    file: fs::File,
    path: PathBuf,
    started: Instant,
    /// When the previous sample was taken, its counters and frames shown.
    previous: Option<(Instant, Snapshot, u64)>,
    stream: Option<Stream>,
    samples: u64,
    failed: bool,
    finished: bool,
}

impl Recorder {
    /// Opens `<dir>/<start time>.jsonl` (0600; a suffix when that second
    /// already has one) and writes the `session` record.
    pub fn start(dir: Option<&Path>, session: Session) -> Result<Self> {
        use std::os::unix::fs::OpenOptionsExt;
        let dir = resolve(dir)?;
        fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&dir, fs::Permissions::from_mode(0o700))?;
        }
        let at = now_ms();
        let stem = utc(at).replace(':', "-");
        let mut path = dir.join(format!("{stem}.jsonl"));
        let mut file = None;
        for attempt in 1..100 {
            match fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&path)
            {
                Ok(opened) => {
                    file = Some(opened);
                    break;
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    path = dir.join(format!("{stem}-{attempt}.jsonl"));
                }
                Err(error) => {
                    return Err(error).with_context(|| format!("creating {}", path.display()));
                }
            }
        }
        let Some(file) = file else {
            bail!("too many sessions started this second in {}", dir.display());
        };
        let mut recorder = Self {
            file,
            path,
            started: Instant::now(),
            previous: None,
            stream: None,
            samples: 0,
            failed: false,
            finished: false,
        };
        recorder.write(json!({
            "record": "session",
            "at": utc(at),
            "version": env!("CARGO_PKG_VERSION"),
            "os": std::env::consts::OS,
            "arch": std::env::consts::ARCH,
            "transport": session.transport,
            "headless": session.headless,
            "source": session.source,
        }));
        tracing::info!(path = %recorder.path.display(), "recording session metrics");
        Ok(recorder)
    }

    /// Seconds since the session started, to a tenth.
    fn t(&self) -> f64 {
        (self.started.elapsed().as_secs_f64() * 10.0).round() / 10.0
    }

    fn write(&mut self, value: Value) {
        if self.failed {
            return;
        }
        let mut text = value.to_string();
        text.push('\n');
        if let Err(error) = self
            .file
            .write_all(text.as_bytes())
            .and_then(|()| self.file.flush())
        {
            self.failed = true;
            tracing::warn!(
                %error,
                path = %self.path.display(),
                "could not write the metrics file; continuing without it"
            );
        }
    }

    /// Records the stream's description when it differs from the last one.
    pub fn stream(&mut self, stream: Stream) {
        if self.stream.as_ref() == Some(&stream) {
            return;
        }
        self.write(json!({
            "record": "stream",
            "t": self.t(),
            "codec": stream.codec,
            "dynamic_range": stream.dynamic_range,
            "width": stream.width,
            "height": stream.height,
            "fps": stream.fps,
            "bitrate_kbps": stream.bitrate_kbps,
            "decoder": stream.decoder,
            "encoder": stream.encoder,
            "renderer": stream.renderer,
        }));
        self.stream = Some(stream);
    }

    /// Records one second: rates and deltas since the previous sample.
    pub fn second(&mut self, second: Second) {
        let now = Instant::now();
        let (since, before, shown_before) = match &self.previous {
            Some((at, snapshot, shown)) => (now.saturating_duration_since(*at), *snapshot, *shown),
            None => (
                now.saturating_duration_since(self.started),
                Snapshot::default(),
                0,
            ),
        };
        let secs = since.as_secs_f64().max(1e-3);
        let rate = |now: u64, before: u64| {
            (now.saturating_sub(before) as f64 / secs * 10.0).round() / 10.0
        };
        let delta = |now: u64, before: u64| now.saturating_sub(before);
        let snapshot = *second.snapshot;
        let rtt_ms = second
            .rtt
            .map(|rtt| (rtt.as_secs_f64() * 10_000.0).round() / 10.0);
        self.write(json!({
            "record": "sample",
            "t": self.t(),
            "received_fps": rate(snapshot.received_frames, before.received_frames),
            "decoded_fps": rate(snapshot.decoded_frames, before.decoded_frames),
            "displayed_fps": rate(second.displayed_frames, shown_before),
            "video_mbps": (delta(snapshot.received_bytes, before.received_bytes) as f64 * 8.0
                / secs / 100_000.0)
                .round()
                / 10.0,
            "rtt_ms": rtt_ms,
            "queue_delay_ms": second.queue_delay_ms,
            "decode_us": snapshot.decode_us,
            "host_encode_us": second.host_encode_us,
            "skipped_groups": delta(snapshot.skipped_groups, before.skipped_groups),
            "decoder_recoveries": delta(snapshot.decoder_recoveries, before.decoder_recoveries),
            "decode_errors": delta(snapshot.decode_errors, before.decode_errors),
            "stale_frames": delta(snapshot.stale_frames, before.stale_frames),
            "stall_ms": second.stall_ms,
            "e2e_us": second.e2e.as_ref().map(stages_json),
            "warnings": second.warnings,
        }));
        self.previous = Some((now, snapshot, second.displayed_frames));
        self.samples += 1;
    }

    /// Writes the `summary` record and closes the file.
    pub fn finish(mut self, summary: Summary) {
        let snapshot = summary.snapshot;
        let (mean, sd, p95) = summary.frame_time_us;
        self.write(json!({
            "record": "summary",
            "t": self.t(),
            "reason": summary.reason,
            "samples": self.samples,
            "displayed_frames": summary.displayed_frames,
            "received_frames": snapshot.received_frames,
            "decoded_frames": snapshot.decoded_frames,
            "received_bytes": snapshot.received_bytes,
            "skipped_groups": snapshot.skipped_groups,
            "decoder_recoveries": snapshot.decoder_recoveries,
            "decode_errors": snapshot.decode_errors,
            "stale_frames": snapshot.stale_frames,
            "unmatched_frames": snapshot.unmatched_frames,
            "e2e_frames": summary.e2e_frames,
            "e2e_us": summary.e2e.as_ref().map(stages_json),
            "frame_time_us": {"mean": mean, "sd": sd, "p95": p95},
            "warnings": summary.warnings,
        }));
        self.finished = true;
    }
}

impl Drop for Recorder {
    fn drop(&mut self) {
        if !self.finished {
            self.write(json!({
                "record": "summary",
                "t": self.t(),
                "reason": "error",
                "samples": self.samples,
            }));
        }
    }
}

/// `teleport metrics`: the recorded sessions.
#[derive(clap::Subcommand)]
pub enum Command {
    /// List the recorded sessions, newest first.
    Ls {
        /// The directory `--metrics-dir` named; default: the standard one.
        #[arg(long)]
        dir: Option<PathBuf>,
    },
    /// One session: its stream, its summary and its worst seconds.
    Show {
        #[arg(long)]
        dir: Option<PathBuf>,
        /// A file name from `ls`, a path, or `latest` (the default).
        file: Option<String>,
        /// How many of the worst seconds to list.
        #[arg(long, default_value_t = 5)]
        worst: usize,
    },
    /// Write the recorded sessions into one file to attach to a bug report,
    /// each line tagged with its session. Nothing is uploaded.
    Export {
        #[arg(long)]
        dir: Option<PathBuf>,
        /// Only the newest N sessions.
        #[arg(long)]
        last: Option<usize>,
        out: PathBuf,
    },
}

/// The session files in `dir`, newest first (their names sort by start time).
fn sessions(dir: &Path) -> Result<Vec<PathBuf>> {
    let mut files: Vec<PathBuf> = match fs::read_dir(dir) {
        Ok(entries) => entries
            .filter_map(|entry| entry.ok().map(|entry| entry.path()))
            .filter(|path| path.extension().is_some_and(|ext| ext == "jsonl"))
            .collect(),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(error) => return Err(error).with_context(|| format!("reading {}", dir.display())),
    };
    files.sort();
    files.reverse();
    Ok(files)
}

/// The records of one file; lines that are not JSON objects are skipped
/// (a crash mid-write leaves at most one).
fn read_records(path: &Path) -> Result<Vec<Value>> {
    let text = fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    Ok(text
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter(|value| value.is_object())
        .collect())
}

fn record<'a>(records: &'a [Value], kind: &str) -> Option<&'a Value> {
    records.iter().find(|value| value["record"] == kind)
}

fn stream_label(stream: &Value) -> String {
    format!(
        "{} {}x{}@{}",
        stream["codec"].as_str().unwrap_or("?"),
        stream["width"],
        stream["height"],
        stream["fps"]
    )
}

fn duration_label(seconds: f64) -> String {
    let seconds = seconds.round() as u64;
    if seconds >= 3600 {
        format!("{}h{:02}m", seconds / 3600, (seconds % 3600) / 60)
    } else if seconds >= 60 {
        format!("{}m{:02}s", seconds / 60, seconds % 60)
    } else {
        format!("{seconds}s")
    }
}

/// The `trouble` of one lost group, reset, warning or long stall.
const TROUBLED: f64 = 1000.0;

/// A second's badness, for `show --worst`: lost video, decoder resets and
/// warnings first, then stalls (arrival gaps of 500 ms or more), then
/// end-to-end latency as a tie-break. [`TROUBLED`] is the least that lists it.
fn trouble(sample: &Value) -> f64 {
    let n = |key: &str| sample[key].as_f64().unwrap_or(0.0);
    (n("skipped_groups") + n("decoder_recoveries") + n("decode_errors")) * 10_000.0
        + if n("stall_ms") >= 500.0 {
            n("stall_ms") * 10.0
        } else {
            0.0
        }
        + sample["e2e_us"]["total"][1].as_f64().unwrap_or(0.0) / 1000.0
        + sample["warnings"]
            .as_array()
            .map_or(0.0, |w| w.len() as f64)
            * 1000.0
}

fn show(path: &Path, worst: usize, out: &mut impl Write) -> Result<()> {
    let records = read_records(path)?;
    let session = record(&records, "session").context("no session record")?;
    writeln!(out, "{}", path.display())?;
    writeln!(
        out,
        "Started {}  Teleport {} on {}/{}  transport {}  source {}{}",
        session["at"].as_str().unwrap_or("?"),
        session["version"].as_str().unwrap_or("?"),
        session["os"].as_str().unwrap_or("?"),
        session["arch"].as_str().unwrap_or("?"),
        session["transport"].as_str().unwrap_or("?"),
        session["source"].as_str().unwrap_or("?"),
        if session["headless"] == true {
            "  (headless)"
        } else {
            ""
        }
    )?;
    for stream in records.iter().filter(|r| r["record"] == "stream") {
        writeln!(
            out,
            "Stream at {}s: {} {} {} kbps  decoder {}  encoder {}  renderer {}",
            stream["t"],
            stream_label(stream),
            stream["dynamic_range"].as_str().unwrap_or("?"),
            stream["bitrate_kbps"],
            stream["decoder"]
                .as_str()
                .filter(|s| !s.is_empty())
                .unwrap_or("-"),
            stream["encoder"]
                .as_str()
                .filter(|s| !s.is_empty())
                .unwrap_or("-"),
            stream["renderer"]
                .as_str()
                .filter(|s| !s.is_empty())
                .unwrap_or("-"),
        )?;
    }
    let samples: Vec<&Value> = records.iter().filter(|r| r["record"] == "sample").collect();
    let stat = |key: &str| {
        let values: Vec<f64> = samples.iter().filter_map(|s| s[key].as_f64()).collect();
        if values.is_empty() {
            return "n/a".to_owned();
        }
        let min = values.iter().copied().fold(f64::INFINITY, f64::min);
        let max = values.iter().copied().fold(f64::NEG_INFINITY, f64::max);
        let mean = values.iter().sum::<f64>() / values.len() as f64;
        format!("{min:.1} / {mean:.1} / {max:.1}")
    };
    writeln!(
        out,
        "Samples {} (min / mean / max per second):",
        samples.len()
    )?;
    writeln!(out, "  displayed fps   {}", stat("displayed_fps"))?;
    writeln!(out, "  received fps    {}", stat("received_fps"))?;
    writeln!(out, "  video Mbps      {}", stat("video_mbps"))?;
    writeln!(out, "  control RTT ms  {}", stat("rtt_ms"))?;
    writeln!(out, "  decode µs       {}", stat("decode_us"))?;
    writeln!(out, "  host encode µs  {}", stat("host_encode_us"))?;
    match record(&records, "summary") {
        Some(summary) => {
            writeln!(
                out,
                "Summary: {} after {}, {} frames displayed, {} received, {} decoded, {:.1} MB",
                summary["reason"].as_str().unwrap_or("?"),
                duration_label(summary["t"].as_f64().unwrap_or(0.0)),
                summary["displayed_frames"],
                summary["received_frames"],
                summary["decoded_frames"],
                summary["received_bytes"].as_f64().unwrap_or(0.0) / 1e6,
            )?;
            writeln!(
                out,
                "  skipped groups {}  decoder recoveries {}  decode errors {}  stale frames {}  warnings {}",
                summary["skipped_groups"],
                summary["decoder_recoveries"],
                summary["decode_errors"],
                summary["stale_frames"],
                summary["warnings"],
            )?;
            if let Some(stages) = summary["e2e_us"].as_object() {
                let ms =
                    |name: &str, index: usize| stages[name][index].as_f64().unwrap_or(0.0) / 1000.0;
                writeln!(
                    out,
                    "  capture to present p50/p95 {:.1} / {:.1} ms over {} frames (capture→encode {:.1}, encode→send {:.1}, network {:.1}, receive→decode {:.1}, decode→present {:.1} ms p50)",
                    ms("total", 0),
                    ms("total", 1),
                    summary["e2e_frames"],
                    ms("capture_encode", 0),
                    ms("encode_send", 0),
                    ms("network", 0),
                    ms("receive_decode", 0),
                    ms("decode_present", 0),
                )?;
            }
            let frame_time = &summary["frame_time_us"];
            writeln!(
                out,
                "  frame time mean {:.1} ms, sd {:.1} ms, p95 {:.1} ms",
                frame_time["mean"].as_f64().unwrap_or(0.0) / 1000.0,
                frame_time["sd"].as_f64().unwrap_or(0.0) / 1000.0,
                frame_time["p95"].as_f64().unwrap_or(0.0) / 1000.0,
            )?;
        }
        None => writeln!(out, "Summary: none (the session is running or was killed)")?,
    }
    let mut ranked: Vec<&Value> = samples
        .iter()
        .copied()
        .filter(|s| trouble(s) >= TROUBLED)
        .collect();
    ranked.sort_by(|a, b| trouble(b).total_cmp(&trouble(a)));
    ranked.truncate(worst);
    if !ranked.is_empty() {
        writeln!(out, "Worst seconds:")?;
        for sample in ranked {
            let warnings: Vec<&str> = sample["warnings"]
                .as_array()
                .map(|w| w.iter().filter_map(Value::as_str).collect())
                .unwrap_or_default();
            writeln!(
                out,
                "  t={}s displayed {} fps, {} Mbps, RTT {} ms, e2e p95 {} ms, skipped {}, recoveries {}, stall {} ms{}",
                sample["t"],
                sample["displayed_fps"],
                sample["video_mbps"],
                sample["rtt_ms"]
                    .as_f64()
                    .map_or("n/a".to_owned(), |ms| format!("{ms:.1}")),
                sample["e2e_us"]["total"][1]
                    .as_f64()
                    .map_or("n/a".to_owned(), |us| format!("{:.1}", us / 1000.0)),
                sample["skipped_groups"],
                sample["decoder_recoveries"],
                sample["stall_ms"],
                if warnings.is_empty() {
                    String::new()
                } else {
                    format!("; {}", warnings.join("; "))
                },
            )?;
        }
    }
    Ok(())
}

fn ls(dir: &Path, out: &mut impl Write) -> Result<()> {
    let files = sessions(dir)?;
    if files.is_empty() {
        writeln!(
            out,
            "No recorded sessions in {}. Record one with `teleport client --record-metrics` (launcher: Advanced, Metrics).",
            dir.display()
        )?;
        return Ok(());
    }
    let row = |cells: [&str; 7]| {
        let [started, length, frames, stream, transport, warnings, file] = cells;
        format!(
            "{started:<20} {length:<8} {frames:>7} {stream:<22} {transport:<9} {warnings:>8}  {file}"
        )
    };
    writeln!(
        out,
        "{}",
        row([
            "Started",
            "Length",
            "Frames",
            "Stream",
            "Transport",
            "Warnings",
            "File"
        ])
    )?;
    for path in files {
        let records = read_records(&path)?;
        let session = record(&records, "session");
        let summary = record(&records, "summary");
        let last_sample = records.iter().rev().find(|r| r["record"] == "sample");
        let seconds = summary
            .or(last_sample)
            .and_then(|r| r["t"].as_f64())
            .unwrap_or(0.0);
        let stream = records
            .iter()
            .rev()
            .find(|r| r["record"] == "stream")
            .map_or_else(|| "?".to_owned(), stream_label);
        let text = |key: &str| summary.map_or_else(|| "-".to_owned(), |s| s[key].to_string());
        let (frames, warnings) = (text("displayed_frames"), text("warnings"));
        writeln!(
            out,
            "{}",
            row([
                session.and_then(|s| s["at"].as_str()).unwrap_or("?"),
                &duration_label(seconds),
                &frames,
                &stream,
                session.and_then(|s| s["transport"].as_str()).unwrap_or("?"),
                &warnings,
                &path.file_name().unwrap_or_default().to_string_lossy(),
            ])
        )?;
    }
    Ok(())
}

fn export(dir: &Path, last: Option<usize>, out_path: &Path) -> Result<()> {
    let mut files = sessions(dir)?;
    ensure!(
        !files.is_empty(),
        "no recorded sessions in {}",
        dir.display()
    );
    if let Some(last) = last {
        files.truncate(last);
    }
    let mut out =
        fs::File::create(out_path).with_context(|| format!("creating {}", out_path.display()))?;
    let mut lines = 0_usize;
    for path in files.iter().rev() {
        let stem = path
            .file_stem()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned();
        for mut value in read_records(path)? {
            if let Value::Object(object) = &mut value {
                object.insert("session".into(), json!(stem));
            }
            writeln!(out, "{value}")?;
            lines += 1;
        }
    }
    out.flush()?;
    println!(
        "Wrote {} sessions ({lines} lines) to {}.",
        files.len(),
        out_path.display()
    );
    println!(
        "It holds rates, latencies, counters, codec, decoder, encoder and GPU names, the Teleport version and OS; no addresses, names, fingerprints or pixels. Read it before attaching it to an issue at https://github.com/ejb1123/teleport/issues."
    );
    Ok(())
}

pub fn run(command: Command) -> Result<()> {
    let mut stdout = std::io::stdout().lock();
    match command {
        Command::Ls { dir } => ls(&resolve(dir.as_deref())?, &mut stdout),
        Command::Show { dir, file, worst } => {
            let dir = resolve(dir.as_deref())?;
            let path = match file.as_deref() {
                None | Some("latest") => sessions(&dir)?
                    .into_iter()
                    .next()
                    .with_context(|| format!("no recorded sessions in {}", dir.display()))?,
                Some(name) if Path::new(name).is_file() => PathBuf::from(name),
                Some(name) => dir.join(name),
            };
            show(&path, worst, &mut stdout)
        }
        Command::Export { dir, last, out } => export(&resolve(dir.as_deref())?, last, &out),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utc_renders_civil_dates() {
        assert_eq!(utc(0), "1970-01-01T00:00:00Z");
        assert_eq!(utc(951_868_800_000), "2000-03-01T00:00:00Z");
        assert_eq!(utc(1_793_572_414_999), "2026-11-01T22:33:34Z");
        assert_eq!(utc(4_107_542_400_000), "2100-03-01T00:00:00Z");
    }

    fn snapshot(frames: u64, bytes: u64, skipped: u64) -> Snapshot {
        Snapshot {
            received_frames: frames,
            decoded_frames: frames,
            received_bytes: bytes,
            skipped_groups: skipped,
            ..Snapshot::default()
        }
    }

    #[test]
    fn a_session_file_has_session_stream_samples_and_summary() {
        let dir = tempfile::tempdir().unwrap();
        let mut recorder = Recorder::start(
            Some(dir.path()),
            Session {
                transport: "direct",
                headless: true,
                source: "test",
            },
        )
        .unwrap();
        let path = recorder.path.clone();
        assert!(
            path.file_name()
                .unwrap()
                .to_str()
                .unwrap()
                .ends_with("Z.jsonl")
        );
        let stream = Stream {
            codec: "H.264".into(),
            dynamic_range: "SDR".into(),
            width: 640,
            height: 360,
            fps: 60,
            bitrate_kbps: 8000,
            decoder: "ffmpeg:h264".into(),
            encoder: "x264".into(),
            renderer: "headless".into(),
        };
        recorder.stream(stream.clone());
        recorder.stream(stream.clone());
        let first = snapshot(60, 1_000_000, 0);
        recorder.second(Second {
            snapshot: &first,
            displayed_frames: 60,
            rtt: Some(Duration::from_micros(2_345)),
            queue_delay_ms: None,
            host_encode_us: 1500,
            stall_ms: 0,
            e2e: None,
            warnings: &[],
        });
        recorder.stream(Stream {
            width: 1280,
            height: 720,
            ..stream
        });
        let second = snapshot(120, 2_000_000, 2);
        let warnings = ["Network: video lost, expect stutter".to_owned()];
        recorder.second(Second {
            snapshot: &second,
            displayed_frames: 118,
            rtt: None,
            queue_delay_ms: Some(12),
            host_encode_us: 1600,
            stall_ms: 510,
            e2e: Some([(1, 2), (3, 4), (5, 6), (7, 8), (9, 10), (25, 30)]),
            warnings: &warnings,
        });
        recorder.finish(Summary {
            snapshot: &second,
            displayed_frames: 118,
            e2e: Some([(1, 2), (3, 4), (5, 6), (7, 8), (9, 10), (25, 30)]),
            e2e_frames: 100,
            frame_time_us: (16_600, 900, 18_000),
            warnings: 1,
            reason: "closed",
        });
        let records = read_records(&path).unwrap();
        let kinds: Vec<&str> = records
            .iter()
            .map(|r| r["record"].as_str().unwrap())
            .collect();
        assert_eq!(
            kinds,
            ["session", "stream", "sample", "stream", "sample", "summary"]
        );
        assert_eq!(records[0]["transport"], "direct");
        assert_eq!(records[0]["version"], env!("CARGO_PKG_VERSION"));
        assert_eq!(records[1]["width"], 640);
        assert_eq!(records[3]["width"], 1280);
        let sample = &records[2];
        assert_eq!(sample["rtt_ms"], 2.3);
        assert!(sample["queue_delay_ms"].is_null());
        assert!(sample["e2e_us"].is_null());
        assert_eq!(sample["skipped_groups"], 0);
        let sample = &records[4];
        assert_eq!(sample["skipped_groups"], 2);
        assert_eq!(sample["queue_delay_ms"], 12);
        assert_eq!(sample["stall_ms"], 510);
        assert_eq!(sample["e2e_us"]["total"], json!([25, 30]));
        assert_eq!(sample["warnings"][0], warnings[0]);
        // Rates are per elapsed second: 60 more frames in well under one.
        assert!(sample["received_fps"].as_f64().unwrap() >= 60.0);
        let summary = &records[5];
        assert_eq!(summary["reason"], "closed");
        assert_eq!(summary["samples"], 2);
        assert_eq!(summary["skipped_groups"], 2);
        assert_eq!(summary["frame_time_us"]["p95"], 18_000);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
            assert_eq!(
                fs::metadata(dir.path()).unwrap().permissions().mode() & 0o777,
                0o700
            );
        }

        // ls and show read it back.
        let mut listing = Vec::new();
        ls(dir.path(), &mut listing).unwrap();
        let listing = String::from_utf8(listing).unwrap();
        assert!(listing.contains("H.264 1280x720@60"), "{listing}");
        assert!(listing.contains("direct"), "{listing}");
        let mut shown = Vec::new();
        show(&path, 5, &mut shown).unwrap();
        let shown = String::from_utf8(shown).unwrap();
        assert!(shown.contains("Summary: closed"), "{shown}");
        assert!(
            shown.contains("capture to present p50/p95 0.0 / 0.0 ms over 100 frames"),
            "{shown}"
        );
        assert!(shown.contains("Worst seconds:"), "{shown}");
        assert!(shown.contains("skipped 2"), "{shown}");
        assert!(shown.contains("Network: video lost"), "{shown}");

        // export tags every line with its session.
        let out = dir.path().join("export.jsonl");
        export(dir.path(), Some(1), &out).unwrap();
        let exported = read_records(&out).unwrap();
        assert_eq!(exported.len(), 6);
        let stem = path.file_stem().unwrap().to_str().unwrap();
        assert!(exported.iter().all(|r| r["session"] == stem));
    }

    #[test]
    fn an_unfinished_recorder_writes_an_error_summary_and_names_do_not_collide() {
        let dir = tempfile::tempdir().unwrap();
        let session = || Session {
            transport: "relay",
            headless: false,
            source: "portal",
        };
        let first = Recorder::start(Some(dir.path()), session()).unwrap();
        let second = Recorder::start(Some(dir.path()), session()).unwrap();
        assert_ne!(first.path, second.path);
        let path = first.path.clone();
        drop(first);
        let records = read_records(&path).unwrap();
        assert_eq!(records.len(), 2);
        assert_eq!(records[1]["record"], "summary");
        assert_eq!(records[1]["reason"], "error");
        drop(second);
        assert_eq!(sessions(dir.path()).unwrap().len(), 2);
        // An empty or missing directory lists nothing and fails export.
        let empty = tempfile::tempdir().unwrap();
        let mut listing = Vec::new();
        ls(&empty.path().join("none"), &mut listing).unwrap();
        assert!(
            String::from_utf8(listing)
                .unwrap()
                .contains("No recorded sessions")
        );
        assert!(export(empty.path(), None, &empty.path().join("x.jsonl")).is_err());
    }

    #[test]
    fn durations_and_trouble_read_well() {
        assert_eq!(duration_label(42.4), "42s");
        assert_eq!(duration_label(125.0), "2m05s");
        assert_eq!(duration_label(3_725.0), "1h02m");
        let calm = json!({"skipped_groups": 0, "stall_ms": 0, "warnings": []});
        let lost = json!({"skipped_groups": 1, "stall_ms": 0, "warnings": []});
        let slow =
            json!({"skipped_groups": 0, "stall_ms": 0, "e2e_us": {"total": [20_000, 90_000]}});
        assert_eq!(trouble(&calm), 0.0);
        assert!(trouble(&lost) > trouble(&slow) && trouble(&slow) > 0.0);
    }
}
