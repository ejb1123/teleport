//! Client-local measurements, plus end-to-end latency from host timestamps
//! mapped onto this clock by an NTP-style offset estimate ([`ClockSync`]).
use crate::protocol::{FrameTiming, clock_us};
use std::{
    collections::{BTreeMap, VecDeque},
    sync::{
        Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

struct PendingFrame {
    received: Instant,
    entered: Option<Instant>,
    output: Option<Instant>,
    group: u64,
    /// Capture→receipt, only in test wall-clock mode (protocol::test_wall_clock).
    captured_age: Option<Duration>,
    /// The host's stamps, from hosts that send them (`Desktop::frame_timing`).
    host: Option<FrameTiming>,
}

/// One decoded frame's path so far: the host's stamps (host clock) and when
/// it was received and decoded here (`protocol::clock_us`, client clock).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameLatency {
    pub host: FrameTiming,
    pub received_us: u64,
    pub ready_us: u64,
    /// Capture→receipt on the Unix clock, in test wall-clock mode only: the
    /// reference the offset estimate is checked against on one machine.
    pub wall_capture_to_received_us: Option<u64>,
}

/// Microseconds per stage of one presented frame. Network and the total
/// cross machines and carry the clock estimate's error (at most half the
/// best probe's round trip, less on a symmetric path), so they are signed.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Stages {
    pub capture_encode: i64,
    pub encode_send: i64,
    pub network: i64,
    pub receive_decode: i64,
    pub decode_present: i64,
    pub total: i64,
}

impl Stages {
    fn values(&self) -> [i64; 6] {
        [
            self.capture_encode,
            self.encode_send,
            self.network,
            self.receive_decode,
            self.decode_present,
            self.total,
        ]
    }
}

impl FrameLatency {
    /// Stages of this frame shown at `presented_us` (client clock), with
    /// `offset_us` = host clock − client clock.
    pub fn stages(&self, presented_us: u64, offset_us: i64) -> Stages {
        let local = |host_us: u64| host_us as i64 - offset_us;
        let host = self.host;
        Stages {
            capture_encode: host.encoded_us as i64 - host.capture_us as i64,
            encode_send: host.sent_us as i64 - host.encoded_us as i64,
            network: self.received_us as i64 - local(host.sent_us),
            receive_decode: self.ready_us as i64 - self.received_us as i64,
            decode_present: presented_us as i64 - self.ready_us as i64,
            total: presented_us as i64 - local(host.capture_us),
        }
    }

    /// Estimated minus wall-clock capture→`presented_us`, in test
    /// wall-clock mode: the offset estimate's error for this frame.
    pub fn clock_error_us(&self, presented_us: u64, offset_us: i64) -> Option<i64> {
        let wall = self.wall_capture_to_received_us? as i64
            + (presented_us as i64 - self.received_us as i64);
        Some(self.stages(presented_us, offset_us).total - wall)
    }
}

/// Probe samples kept; at one probe a second the best of the last ~16 s,
/// short enough that clock drift (tens of ppm) stays well under 1 ms.
const CLOCK_SAMPLES: usize = 16;

/// NTP-style estimate of host clock − client clock from control probes:
/// each `Probe`/`Pong` round trip gives the host's time against the
/// midpoint of the client's send and receive (symmetric paths assumed). The
/// sample with the lowest round trip, the least queueing, wins.
#[derive(Default)]
pub struct ClockSync {
    /// (round trip µs, offset µs), oldest first.
    samples: VecDeque<(u64, i64)>,
}

impl ClockSync {
    pub fn observe(&mut self, sent: Instant, received: Instant, host_us: u64) {
        let (sent, received) = (clock_us(sent), clock_us(received));
        let Some(rtt) = received.checked_sub(sent) else {
            return;
        };
        let midpoint = sent + rtt / 2;
        self.samples
            .push_back((rtt, host_us as i64 - midpoint as i64));
        while self.samples.len() > CLOCK_SAMPLES {
            self.samples.pop_front();
        }
    }

    /// (host − client offset, round trip of the sample it came from), both µs.
    pub fn offset(&self) -> Option<(i64, u64)> {
        self.samples
            .iter()
            .min_by_key(|(rtt, _)| *rtt)
            .map(|&(rtt, offset)| (offset, rtt))
    }

    /// Probe quickly until a few samples are in, then once a second.
    pub fn probe_interval(&self) -> Duration {
        if self.samples.len() < 8 {
            Duration::from_millis(200)
        } else {
            Duration::from_secs(1)
        }
    }
}

/// Frames in the F8 window (a few seconds at typical rates).
const RECENT_FRAMES: usize = 300;
/// Whole-run samples kept for summaries (tests, logs).
const RUN_FRAMES: usize = 100_000;

/// End-to-end stage samples of presented frames: a recent window for F8 and
/// the whole run for summaries.
#[derive(Default)]
pub struct LatencyStats {
    recent: VecDeque<Stages>,
    run: Vec<Stages>,
    clock_errors: Vec<i64>,
}

impl LatencyStats {
    /// A frame was presented (or taken by a headless client) at `presented`.
    pub fn presented(&mut self, latency: &FrameLatency, presented: Instant, clock: &ClockSync) {
        let Some((offset, _)) = clock.offset() else {
            return;
        };
        let presented_us = clock_us(presented);
        let stages = latency.stages(presented_us, offset);
        self.recent.push_back(stages);
        while self.recent.len() > RECENT_FRAMES {
            self.recent.pop_front();
        }
        if self.run.len() < RUN_FRAMES {
            self.run.push(stages);
            if let Some(error) = latency.clock_error_us(presented_us, offset) {
                self.clock_errors.push(error);
            }
        }
    }

    pub fn is_empty(&self) -> bool {
        self.recent.is_empty()
    }

    /// (p50, p95) µs of each stage in field order (capture→encode … total), over the
    /// recent window or the whole run.
    pub fn percentiles(&self, whole_run: bool) -> [(i64, i64); 6] {
        let samples: Vec<Stages> = if whole_run {
            self.run.clone()
        } else {
            self.recent.iter().copied().collect()
        };
        let mut result = [(0, 0); 6];
        for (index, slot) in result.iter_mut().enumerate() {
            let mut values: Vec<i64> = samples.iter().map(|s| s.values()[index]).collect();
            *slot = (percentile(&mut values, 50), percentile(&mut values, 95));
        }
        result
    }

    /// Whole-run frames measured.
    pub fn frames(&self) -> usize {
        self.run.len()
    }

    /// (frames checked, median, largest absolute) estimate − wall-clock
    /// error in µs; frames only in test wall-clock mode.
    pub fn clock_error(&self) -> (usize, i64, i64) {
        let mut errors = self.clock_errors.clone();
        let worst = errors.iter().map(|e| e.abs()).max().unwrap_or(0);
        (errors.len(), percentile(&mut errors, 50), worst)
    }
}

/// `p`th percentile (nearest rank); 0 when empty. Sorts `values`.
pub fn percentile(values: &mut [i64], p: usize) -> i64 {
    values.sort_unstable();
    values
        .get((values.len() * p / 100).min(values.len().saturating_sub(1)))
        .copied()
        .unwrap_or(0)
}

#[derive(Default, Clone, Copy)]
struct Timings {
    total_us: u64,
    queue_us: u64,
    decoder_us: u64,
    conversion_us: u64,
}

pub struct StreamStats {
    pub decoder: Mutex<String>,
    origin: Instant,
    pending: Mutex<BTreeMap<u64, PendingFrame>>,
    last_key: AtomicU64,
    timings: Mutex<Timings>,
    /// Sums over every matched frame, for whole-run means (Timings is latest-only).
    timing_sums: Mutex<(Timings, u64)>,
    /// Capture→ready per decoded frame (µs), in test wall-clock mode only.
    capture_to_ready_us: Mutex<Vec<u64>>,
    pub received_bytes: AtomicU64,
    pub received_frames: AtomicU64,
    pub decoded_frames: AtomicU64,
    pub ready_frames: AtomicU64,
    pub unmatched_frames: AtomicU64,
    pub skipped_groups: AtomicU64,
    pub overwritten_frames: AtomicU64,
    pub decoder_queue_bytes: AtomicU64,
    pub decoder_recoveries: AtomicU64,
    pub stale_frames: AtomicU64,
    pub decode_errors: AtomicU64,
    /// Frame rate the host is streaming at (0 until known); bounds how many
    /// queued frames count as a decoder backlog.
    pub stream_fps: AtomicU64,
}

#[derive(Default, Clone, Copy)]
pub struct Snapshot {
    pub received_bytes: u64,
    pub received_frames: u64,
    pub decoded_frames: u64,
    pub unmatched_frames: u64,
    pub skipped_groups: u64,
    pub overwritten_frames: u64,
    pub decoder_queue_bytes: u64,
    pub decode_us: u64,
    pub queue_us: u64,
    pub decoder_us: u64,
    pub conversion_us: u64,
    pub decoder_recoveries: u64,
    pub stale_frames: u64,
    pub decode_errors: u64,
}

impl Default for StreamStats {
    fn default() -> Self {
        Self {
            decoder: Mutex::new("Not selected".into()),
            origin: Instant::now(),
            pending: Mutex::default(),
            last_key: AtomicU64::new(0),
            timings: Mutex::default(),
            timing_sums: Mutex::default(),
            capture_to_ready_us: Mutex::default(),
            received_bytes: AtomicU64::new(0),
            received_frames: AtomicU64::new(0),
            decoded_frames: AtomicU64::new(0),
            ready_frames: AtomicU64::new(0),
            unmatched_frames: AtomicU64::new(0),
            skipped_groups: AtomicU64::new(0),
            overwritten_frames: AtomicU64::new(0),
            decoder_queue_bytes: AtomicU64::new(0),
            decoder_recoveries: AtomicU64::new(0),
            stale_frames: AtomicU64::new(0),
            decode_errors: AtomicU64::new(0),
            stream_fps: AtomicU64::new(0),
        }
    }
}

impl StreamStats {
    #[cfg(test)]
    pub fn received(&self, bytes: usize, video_group: u64) -> u64 {
        self.received_frame(bytes, video_group, None, None)
    }

    /// [`Self::received`] with the frame's capture→receipt age (test
    /// wall-clock mode) and the host's stamps, when known.
    pub fn received_frame(
        &self,
        bytes: usize,
        video_group: u64,
        captured_age: Option<Duration>,
        host: Option<FrameTiming>,
    ) -> u64 {
        self.received_bytes
            .fetch_add(bytes as u64, Ordering::Relaxed);
        self.received_frames.fetch_add(1, Ordering::Relaxed);
        let now = Instant::now();
        let mut pending = self.pending.lock().unwrap();
        let pts = (now
            .duration_since(self.origin)
            .as_nanos()
            .min(u64::MAX as u128 - 1) as u64)
            .max(self.last_key.load(Ordering::Relaxed).saturating_add(1));
        // Never reuse keys when a completed frame or recovery empties the map.
        self.last_key.store(pts, Ordering::Relaxed);
        pending.insert(
            pts,
            PendingFrame {
                received: now,
                entered: None,
                output: None,
                group: video_group,
                captured_age,
                host,
            },
        );
        while pending.len() > 256 {
            pending.pop_first();
        }
        pts
    }

    /// Appsrc output, before parsing: isolates client-side compressed input queueing.
    pub fn decoder_entered(&self, key: u64) {
        if let Some(frame) = self.pending.lock().unwrap().get_mut(&key)
            && frame.entered.is_none()
            && frame.output.is_none()
        {
            frame.entered = Some(Instant::now());
        }
    }

    /// Raw system-memory output, before conversion/copy into the display frame.
    /// The preceding interval includes parsing, decode scheduling and download;
    /// it is not a measurement of GPU kernel execution time.
    pub fn decoder_output(&self, key: u64) {
        if let Some(frame) = self.pending.lock().unwrap().get_mut(&key)
            && frame.output.is_none()
        {
            frame.output = Some(Instant::now());
        }
    }

    pub fn frame_age(&self, key: u64) -> Option<Duration> {
        self.pending
            .lock()
            .unwrap()
            .get(&key)
            .map(|frame| frame.received.elapsed())
    }

    /// Oldest outstanding input; dropped frames may remain until eviction/reset.
    #[allow(dead_code)]
    pub fn oldest_pending_age(&self) -> Option<Duration> {
        self.pending
            .lock()
            .unwrap()
            .values()
            .map(|frame| frame.received.elapsed())
            .max()
    }

    /// Only frames which have not left appsrc. Correlate with appsrc's actual
    /// queue occupancy: dropped buffers can leave uncompleted ledger entries.
    pub fn oldest_queued_age(&self) -> Option<Duration> {
        self.pending
            .lock()
            .unwrap()
            .values()
            .filter(|frame| frame.entered.is_none() && frame.output.is_none())
            .map(|frame| frame.received.elapsed())
            .max()
    }

    /// Call after recovery flushes old buffers, before receiving the next group.
    pub fn reset_pending(&self) {
        let mut pending = self.pending.lock().unwrap();
        pending.clear();
        *self.timings.lock().unwrap() = Timings::default();
        self.decoder_queue_bytes.store(0, Ordering::Relaxed);
        self.decoder_recoveries.fetch_add(1, Ordering::Relaxed);
    }

    pub fn decoded(&self, pts: Option<u64>) -> Option<u64> {
        self.decoded_with_latency(pts).0
    }

    /// [`Self::decoded`], plus the frame's end-to-end record when the host
    /// stamped it.
    pub fn decoded_with_latency(&self, pts: Option<u64>) -> (Option<u64>, Option<FrameLatency>) {
        self.decoded_frames.fetch_add(1, Ordering::Relaxed);
        let now = Instant::now();
        let mut pending = self.pending.lock().unwrap();
        let matched = pts.and_then(|pts| pending.remove(&pts));
        if matched.is_none() {
            self.unmatched_frames.fetch_add(1, Ordering::Relaxed);
            let before = pts.and_then(|pts| pending.range(..=pts).next_back().map(|(pts, _)| *pts));
            let after = pts.and_then(|pts| pending.range(pts..).next().map(|(pts, _)| *pts));
            tracing::debug!(
                ?pts,
                ?before,
                ?after,
                "decoded timestamp did not match input ledger"
            );
        }
        let micros = |duration: Duration| duration.as_micros().max(1).min(u64::MAX as u128) as u64;
        // All stages refer to the same exact completed frame. Missing timestamps
        // remain unavailable, never estimated from another frame or FIFO order.
        let timings = matched
            .as_ref()
            .map_or_else(Timings::default, |frame| Timings {
                total_us: micros(now.saturating_duration_since(frame.received)),
                queue_us: frame.entered.map_or(0, |entered| {
                    micros(entered.saturating_duration_since(frame.received))
                }),
                decoder_us: frame
                    .entered
                    .zip(frame.output)
                    .map_or(0, |(entered, output)| {
                        micros(output.saturating_duration_since(entered))
                    }),
                conversion_us: frame
                    .output
                    .map_or(0, |output| micros(now.saturating_duration_since(output))),
            });
        if let Some(age) = matched.as_ref().and_then(|frame| frame.captured_age) {
            let mut latencies = self.capture_to_ready_us.lock().unwrap();
            if latencies.len() < 100_000 {
                latencies.push(age.as_micros() as u64 + timings.total_us);
            }
        }
        if matched.is_some() {
            let mut sums = self.timing_sums.lock().unwrap();
            sums.0.total_us += timings.total_us;
            sums.0.queue_us += timings.queue_us;
            sums.0.decoder_us += timings.decoder_us;
            sums.0.conversion_us += timings.conversion_us;
            sums.1 += 1;
        }
        *self.timings.lock().unwrap() = timings;
        let latency = matched.as_ref().and_then(|frame| {
            Some(FrameLatency {
                host: frame.host?,
                received_us: clock_us(frame.received),
                ready_us: clock_us(now),
                wall_capture_to_received_us: frame.captured_age.map(|age| age.as_micros() as u64),
            })
        });
        (matched.map(|frame| frame.group), latency)
    }

    /// Whole-run means in microseconds: (receive→ready, compressed queue,
    /// parse/decode/download, convert/copy). Zero before any matched frame.
    pub fn mean_timings_us(&self) -> (u64, u64, u64, u64) {
        let (sum, count) = *self.timing_sums.lock().unwrap();
        let mean = |total: u64| total.checked_div(count).unwrap_or(0);
        (
            mean(sum.total_us),
            mean(sum.queue_us),
            mean(sum.decoder_us),
            mean(sum.conversion_us),
        )
    }

    /// Capture→ready (p50, p95, max) in µs over every decoded frame; zero
    /// unless both ends run in test wall-clock mode.
    pub fn capture_to_ready_us(&self) -> (u64, u64, u64) {
        let mut latencies = self.capture_to_ready_us.lock().unwrap().clone();
        latencies.sort_unstable();
        let at = |p: usize| {
            latencies
                .get((latencies.len() * p / 100).min(latencies.len().saturating_sub(1)))
                .copied()
                .unwrap_or(0)
        };
        (at(50), at(95), latencies.last().copied().unwrap_or(0))
    }

    pub fn snapshot(&self) -> Snapshot {
        let timings = *self.timings.lock().unwrap();
        Snapshot {
            received_bytes: self.received_bytes.load(Ordering::Relaxed),
            received_frames: self.received_frames.load(Ordering::Relaxed),
            decoded_frames: self.decoded_frames.load(Ordering::Relaxed),
            unmatched_frames: self.unmatched_frames.load(Ordering::Relaxed),
            skipped_groups: self.skipped_groups.load(Ordering::Relaxed),
            overwritten_frames: self.overwritten_frames.load(Ordering::Relaxed),
            decoder_queue_bytes: self.decoder_queue_bytes.load(Ordering::Relaxed),
            decode_us: timings.total_us,
            queue_us: timings.queue_us,
            decoder_us: timings.decoder_us,
            conversion_us: timings.conversion_us,
            decoder_recoveries: self.decoder_recoveries.load(Ordering::Relaxed),
            stale_frames: self.stale_frames.load(Ordering::Relaxed),
            decode_errors: self.decode_errors.load(Ordering::Relaxed),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stages_are_exact_keyed_and_do_not_mix_reordered_frames() {
        let stats = StreamStats::default();
        let old = stats.received(10, 2);
        let new = stats.received(10, 4);
        let now = Instant::now();
        {
            let mut pending = stats.pending.lock().unwrap();
            let frame = pending.get_mut(&new).unwrap();
            frame.received = now - Duration::from_millis(12);
            frame.entered = Some(now - Duration::from_millis(8));
            frame.output = Some(now - Duration::from_millis(3));
        }
        assert_eq!(stats.decoded(Some(new)), Some(4));
        let snapshot = stats.snapshot();
        assert_eq!(snapshot.queue_us, 4000);
        assert_eq!(snapshot.decoder_us, 5000);
        assert!(snapshot.conversion_us >= 3000);
        assert_eq!(
            snapshot.decode_us,
            snapshot.queue_us + snapshot.decoder_us + snapshot.conversion_us
        );
        // Older frame never passed probes. No borrowing newer frame's timings.
        assert_eq!(stats.decoded(Some(old)), Some(2));
        let snapshot = stats.snapshot();
        assert!(snapshot.decode_us > 0);
        assert_eq!(
            (
                snapshot.queue_us,
                snapshot.decoder_us,
                snapshot.conversion_us
            ),
            (0, 0, 0)
        );
    }

    #[test]
    fn probes_are_idempotent_and_ages_exclude_completed_frames() {
        let stats = StreamStats::default();
        let key = stats.received(10, 3);
        assert!(stats.oldest_pending_age().is_some());
        assert!(stats.oldest_queued_age().is_some());
        assert!(stats.frame_age(key).is_some());
        stats.decoder_entered(key + 100);
        assert!(stats.oldest_queued_age().is_some());
        stats.decoder_entered(key);
        let entered = stats.pending.lock().unwrap()[&key].entered;
        stats.decoder_entered(key);
        assert_eq!(stats.pending.lock().unwrap()[&key].entered, entered);
        assert!(stats.oldest_queued_age().is_none());
        stats.decoder_output(key);
        let output = stats.pending.lock().unwrap()[&key].output;
        stats.decoder_output(key);
        assert_eq!(stats.pending.lock().unwrap()[&key].output, output);
        assert_eq!(stats.decoded(Some(key)), Some(3));
        let snapshot = stats.snapshot();
        assert!(snapshot.queue_us > 0 && snapshot.decoder_us > 0 && snapshot.conversion_us > 0);
        assert!(stats.frame_age(key).is_none());
        assert!(stats.oldest_pending_age().is_none());
        assert!(stats.oldest_queued_age().is_none());
        stats.decoded(Some(key));
        let snapshot = stats.snapshot();
        assert_eq!(
            (
                snapshot.decode_us,
                snapshot.queue_us,
                snapshot.decoder_us,
                snapshot.conversion_us
            ),
            (0, 0, 0, 0)
        );
    }

    #[test]
    fn recovery_discards_stale_keys_and_timings_without_reusing_keys() {
        let stats = StreamStats::default();
        let key = stats.received(10, 5);
        stats.decoder_entered(key);
        stats.decoder_output(key);
        stats.decoded(Some(key));
        let stale = stats.received(10, 6);
        stats.decoder_queue_bytes.store(123, Ordering::Relaxed);
        stats.reset_pending();
        let snapshot = stats.snapshot();
        assert_eq!(snapshot.decoder_recoveries, 1);
        assert_eq!(snapshot.decoder_queue_bytes, 0);
        assert_eq!(
            (
                snapshot.decode_us,
                snapshot.queue_us,
                snapshot.decoder_us,
                snapshot.conversion_us
            ),
            (0, 0, 0, 0)
        );
        assert!(stats.frame_age(stale).is_none());
        let fresh = stats.received(10, 7);
        assert!(fresh > stale);
        stats.decoder_entered(stale);
        stats.decoder_output(stale);
        assert_eq!(stats.decoded(Some(stale)), None);
        assert_eq!(stats.decoded(Some(fresh)), Some(7));
    }
    #[test]
    fn timestamps_are_bounded_and_match_decoded_frames() {
        let stats = StreamStats::default();
        let first = stats.received(42, 7);
        assert_eq!(stats.decoded(Some(first)), Some(7));
        assert!(stats.snapshot().decode_us > 0);
        assert_eq!(stats.snapshot().received_bytes, 42);
        assert_eq!(stats.snapshot().decoded_frames, 1);
        assert_eq!(stats.decoded(None), None);
        assert_eq!(stats.snapshot().decode_us, 0);
        for _ in 0..1000 {
            stats.received(1, 9);
        }
        assert_eq!(stats.pending.lock().unwrap().len(), 256);
    }

    #[test]
    fn reordered_decodes_keep_their_video_generation() {
        let stats = StreamStats::default();
        let old = stats.received(10, 2);
        let new = stats.received(10, 4);
        assert_eq!(stats.decoded(Some(new)), Some(4));
        assert_eq!(stats.decoded(Some(old)), Some(2));
        assert_eq!(stats.decoded(Some(new)), None);
        let evicted = stats.received(10, 4);
        for _ in 0..257 {
            stats.received(10, 5);
        }
        assert_eq!(stats.decoded(Some(evicted)), None);
    }

    #[test]
    fn clock_offset_uses_the_least_queued_probe() {
        let mut clock = ClockSync::default();
        assert_eq!(clock.offset(), None);
        assert_eq!(clock.probe_interval(), Duration::from_millis(200));
        // After the clock's origin, which its first use sets.
        let base = Instant::now() + Duration::from_secs(1);
        let at = |ms: u64| base + Duration::from_millis(ms);
        // Host clock runs 5 s ahead. A 40 ms round trip that took 30 ms out
        // and 10 ms back reads 10 ms too far ahead.
        let host = |client_ms: u64| clock_us(at(client_ms)) + 5_000_000;
        clock.observe(at(0), at(40), host(30));
        assert_eq!(clock.offset(), Some((5_000_000 + 10_000, 40_000)));
        // A clean 10 ms round trip (5 ms each way) replaces it.
        clock.observe(at(100), at(110), host(105));
        assert_eq!(clock.offset(), Some((5_000_000, 10_000)));
        // A reply "before" its probe is ignored; old samples age out.
        clock.observe(at(200), at(150), host(0));
        for index in 0..CLOCK_SAMPLES as u64 {
            clock.observe(
                at(1000 + index * 100),
                at(1020 + index * 100),
                host(1012 + index * 100),
            );
        }
        assert_eq!(clock.offset(), Some((5_000_000 + 2_000, 20_000)));
        assert_eq!(clock.probe_interval(), Duration::from_secs(1));
    }

    #[test]
    fn stages_add_up_to_capture_to_present() {
        let mut clock = ClockSync::default();
        let base = Instant::now() + Duration::from_secs(1);
        let client = |ms: u64| clock_us(base + Duration::from_millis(ms));
        // Host clock 2 s ahead of this one; a symmetric 4 ms round trip.
        let offset = 2_000_000_i64;
        let host = |client_us: u64| (client_us as i64 + offset) as u64;
        clock.observe(base, base + Duration::from_millis(4), host(client(2)));
        let latency = FrameLatency {
            host: FrameTiming {
                capture_us: host(client(0)),
                encoded_us: host(client(6)),
                sent_us: host(client(7)),
            },
            received_us: client(10),
            ready_us: client(14),
            wall_capture_to_received_us: Some(9_000),
        };
        let presented = base + Duration::from_millis(17);
        let stages = latency.stages(clock_us(presented), offset);
        assert_eq!(
            stages,
            Stages {
                capture_encode: 6_000,
                encode_send: 1_000,
                network: 3_000,
                receive_decode: 4_000,
                decode_present: 3_000,
                total: 17_000,
            }
        );
        // The wall clock puts capture 1 ms later than the estimate does.
        assert_eq!(
            latency.clock_error_us(clock_us(presented), offset),
            Some(1_000)
        );
        let mut stats = LatencyStats::default();
        stats.presented(&latency, presented, &ClockSync::default());
        assert!(stats.is_empty(), "no estimate yet, nothing recorded");
        stats.presented(&latency, presented, &clock);
        assert_eq!(stats.percentiles(false)[5], (17_000, 17_000));
        assert_eq!(stats.percentiles(true)[2], (3_000, 3_000));
        assert_eq!(stats.clock_error(), (1, 1_000, 1_000));
        // The decoded frame carries the host stamps to the presenter.
        let stats = StreamStats::default();
        let key = stats.received_frame(10, 1, None, Some(latency.host));
        let (group, decoded) = stats.decoded_with_latency(Some(key));
        assert_eq!(group, Some(1));
        let decoded = decoded.unwrap();
        assert_eq!(decoded.host, latency.host);
        assert!(decoded.ready_us >= decoded.received_us);
        let plain = stats.received(10, 2);
        assert_eq!(stats.decoded_with_latency(Some(plain)), (Some(2), None));
    }
}
