//! Client-local measurements. No cross-machine clock subtraction.
use std::{
    collections::BTreeMap,
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
}

impl Default for StreamStats {
    fn default() -> Self {
        Self {
            decoder: Mutex::new("Not selected".into()),
            origin: Instant::now(),
            pending: Mutex::default(),
            last_key: AtomicU64::new(0),
            timings: Mutex::default(),
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
        }
    }
}

impl StreamStats {
    pub fn received(&self, bytes: usize, video_group: u64) -> u64 {
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
        *self.timings.lock().unwrap() =
            matched
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
        matched.map(|frame| frame.group)
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
}
