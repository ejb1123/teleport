//! Client-local measurements. No cross-machine clock subtraction.
use std::{
    collections::BTreeMap,
    sync::{
        Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::Instant,
};

pub struct StreamStats {
    pub decoder: Mutex<String>,
    origin: Instant,
    pending: Mutex<BTreeMap<u64, (Instant, u64)>>,
    pub received_bytes: AtomicU64,
    pub received_frames: AtomicU64,
    pub decoded_frames: AtomicU64,
    pub unmatched_frames: AtomicU64,
    pub skipped_groups: AtomicU64,
    pub overwritten_frames: AtomicU64,
    pub decoder_queue_bytes: AtomicU64,
    pub decode_us: AtomicU64,
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
}

impl Default for StreamStats {
    fn default() -> Self {
        Self {
            decoder: Mutex::new("Not selected".into()),
            origin: Instant::now(),
            pending: Mutex::default(),
            received_bytes: AtomicU64::new(0),
            received_frames: AtomicU64::new(0),
            decoded_frames: AtomicU64::new(0),
            unmatched_frames: AtomicU64::new(0),
            skipped_groups: AtomicU64::new(0),
            overwritten_frames: AtomicU64::new(0),
            decoder_queue_bytes: AtomicU64::new(0),
            decode_us: AtomicU64::new(0),
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
            .max(
                pending
                    .last_key_value()
                    .map_or(1, |(last, _)| last.saturating_add(1)),
            );
        pending.insert(pts, (now, video_group));
        while pending.len() > 256 {
            pending.pop_first();
        }
        pts
    }

    pub fn decoded(&self, pts: Option<u64>) -> Option<u64> {
        self.decoded_frames.fetch_add(1, Ordering::Relaxed);
        let matched = pts.and_then(|pts| self.pending.lock().unwrap().remove(&pts));
        if matched.is_none() {
            self.unmatched_frames.fetch_add(1, Ordering::Relaxed);
            let pending = self.pending.lock().unwrap();
            let before = pts.and_then(|pts| pending.range(..=pts).next_back().map(|(pts, _)| *pts));
            let after = pts.and_then(|pts| pending.range(pts..).next().map(|(pts, _)| *pts));
            tracing::debug!(
                ?pts,
                ?before,
                ?after,
                "decoded timestamp did not match input ledger"
            );
        }
        let elapsed = matched.map(|(at, _)| at.elapsed());
        // Zero means timing unavailable (e.g. decoder did not preserve PTS).
        self.decode_us.store(
            elapsed.map_or(0, |d| d.as_micros().max(1).min(u64::MAX as u128) as u64),
            Ordering::Relaxed,
        );
        matched.map(|(_, group)| group)
    }

    pub fn snapshot(&self) -> Snapshot {
        Snapshot {
            received_bytes: self.received_bytes.load(Ordering::Relaxed),
            received_frames: self.received_frames.load(Ordering::Relaxed),
            decoded_frames: self.decoded_frames.load(Ordering::Relaxed),
            unmatched_frames: self.unmatched_frames.load(Ordering::Relaxed),
            skipped_groups: self.skipped_groups.load(Ordering::Relaxed),
            overwritten_frames: self.overwritten_frames.load(Ordering::Relaxed),
            decoder_queue_bytes: self.decoder_queue_bytes.load(Ordering::Relaxed),
            decode_us: self.decode_us.load(Ordering::Relaxed),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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
