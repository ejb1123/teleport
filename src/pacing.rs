//! Frame pacing (parsec-comparison G4): present each frame as soon as it is
//! decoded (lowest latency, the default) or hold frames in a small adaptive
//! jitter buffer and present them at the cadence they were captured
//! (smooth).
use crate::protocol::clock_us;
use std::{
    collections::VecDeque,
    time::{Duration, Instant},
};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, clap::ValueEnum)]
pub enum Pacing {
    /// Present every frame the moment it is decoded.
    #[default]
    Latency,
    /// Delay frames just enough to absorb network and decode jitter, and
    /// present them evenly spaced as they were captured.
    Smooth,
}

impl Pacing {
    pub fn label(self) -> &'static str {
        match self {
            Self::Latency => "lowest latency",
            Self::Smooth => "smooth",
        }
    }
}

/// Transit samples (arrival − capture) used to size the buffer: about two
/// seconds at 60 fps.
const WINDOW: usize = 120;
/// Never buffer more than this many frame intervals, nor more than
/// [`MAX_DELAY`]: beyond that, a late frame is shown late instead.
const MAX_FRAMES: u32 = 3;
const MAX_DELAY: Duration = Duration::from_millis(50);
/// Frames waiting at most; older ones are dropped when more arrive.
const QUEUE: usize = 4;

struct Queued<T> {
    frame: T,
    arrived: Instant,
    /// When to present, `None` for frames without a capture time.
    due: Option<Instant>,
}

/// The jitter buffer. In [`Pacing::Latency`] it only passes frames through.
pub struct Pacer<T> {
    mode: Pacing,
    queue: VecDeque<Queued<T>>,
    /// Arrival − host capture in µs; the host clock offset is the same in
    /// every sample, so it cancels against their minimum.
    transits: VecDeque<i64>,
    /// Frames dropped from the buffer because a newer one was due.
    pub dropped: u64,
    /// Frames that arrived after their slot (shown at once, late).
    pub late: u64,
}

impl<T> Pacer<T> {
    pub fn new(mode: Pacing) -> Self {
        Self {
            mode,
            queue: VecDeque::new(),
            transits: VecDeque::new(),
            dropped: 0,
            late: 0,
        }
    }

    pub fn mode(&self) -> Pacing {
        self.mode
    }

    /// The playout delay above the fastest recent frame, in µs: how long a
    /// frame that arrived as fast as the best one waits. Zero when not
    /// smoothing.
    pub fn buffer_us(&self, fps: u32) -> u64 {
        if self.mode == Pacing::Latency || self.transits.is_empty() {
            return 0;
        }
        let mut sorted: Vec<i64> = self.transits.iter().copied().collect();
        sorted.sort_unstable();
        let fastest = sorted[0];
        let p95 = sorted[(sorted.len() * 95 / 100).min(sorted.len() - 1)];
        let cap = (MAX_DELAY.as_micros() as i64)
            .min(i64::from(MAX_FRAMES) * 1_000_000 / i64::from(fps.max(1)));
        (p95 - fastest).clamp(0, cap) as u64
    }

    /// A decoded frame arrived. `capture_us` is its host capture time on
    /// the host's `clock_us` (frame timing), if known.
    pub fn push(&mut self, frame: T, arrived: Instant, capture_us: Option<u64>, fps: u32) {
        let due = match (self.mode, capture_us) {
            (Pacing::Smooth, Some(capture)) => {
                let transit = clock_us(arrived) as i64 - capture as i64;
                self.transits.push_back(transit);
                while self.transits.len() > WINDOW {
                    self.transits.pop_front();
                }
                let fastest = self.transits.iter().copied().min().unwrap_or(transit);
                // Present at capture + the fastest transit + the buffer:
                // the same capture→present delay for every frame, so the
                // capture cadence comes out unchanged.
                let target = fastest + self.buffer_us(fps) as i64;
                if transit > target {
                    self.late += 1;
                }
                let wait = (target - transit).max(0) as u64;
                Some(arrived + Duration::from_micros(wait))
            }
            _ => None,
        };
        self.queue.push_back(Queued {
            frame,
            arrived,
            due,
        });
        while self.queue.len() > QUEUE {
            self.queue.pop_front();
            self.dropped += 1;
        }
    }

    /// The frame to present now, if one is due: the newest due frame, with
    /// older due frames dropped. Returns it with its wait in the buffer.
    pub fn pop(&mut self, now: Instant) -> Option<(T, Duration)> {
        let due = self
            .queue
            .iter()
            .rposition(|queued| queued.due.is_none_or(|due| due <= now))?;
        for _ in 0..due {
            self.queue.pop_front();
            self.dropped += 1;
        }
        let queued = self.queue.pop_front()?;
        Some((queued.frame, now.saturating_duration_since(queued.arrived)))
    }

    /// When the next waiting frame is due, so the caller can sleep until then.
    pub fn next_due(&self) -> Option<Instant> {
        self.queue.iter().filter_map(|queued| queued.due).min()
    }

    pub fn clear(&mut self) {
        self.queue.clear();
    }
}

/// Present-to-present intervals and time spent in the buffer, for F8 and
/// summaries.
#[derive(Default)]
pub struct PaceStats {
    last: Option<Instant>,
    intervals_us: Vec<i64>,
    held_us: Vec<i64>,
}

impl PaceStats {
    pub fn presented(&mut self, now: Instant, held: Duration) {
        if let Some(last) = self.last
            && self.intervals_us.len() < 1 << 20
        {
            self.intervals_us
                .push(now.saturating_duration_since(last).as_micros() as i64);
            self.held_us.push(held.as_micros() as i64);
        }
        self.last = Some(now);
    }

    /// (mean, standard deviation, p95) of present intervals, µs.
    pub fn frame_time_us(&self) -> (i64, i64, i64) {
        let count = self.intervals_us.len() as i64;
        if count == 0 {
            return (0, 0, 0);
        }
        let mean = self.intervals_us.iter().sum::<i64>() / count;
        let variance = self
            .intervals_us
            .iter()
            .map(|interval| (interval - mean).pow(2))
            .sum::<i64>()
            / count;
        let mut sorted = self.intervals_us.clone();
        (
            mean,
            (variance as f64).sqrt() as i64,
            crate::stats::percentile(&mut sorted, 95),
        )
    }

    /// (p50, p95) of the time frames waited in the buffer, µs.
    pub fn held_us(&self) -> (i64, i64) {
        let mut held = self.held_us.clone();
        (
            crate::stats::percentile(&mut held, 50),
            crate::stats::percentile(&mut held, 95),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FPS: u32 = 60;
    const FRAME_US: u64 = 16_667;

    /// Frames captured every 16.7 ms arriving after `transit(i)` ms.
    fn run(mode: Pacing, transit: impl Fn(u64) -> u64) -> (PaceStats, Pacer<u64>) {
        let start = Instant::now() + Duration::from_secs(1);
        let capture = |i: u64| clock_us(start) + i * FRAME_US;
        let mut arrivals: Vec<(Instant, u64)> = (0..300)
            .map(|i| {
                (
                    start + Duration::from_micros(i * FRAME_US + transit(i) * 1000),
                    i,
                )
            })
            .collect();
        arrivals.sort();
        let mut pacer = Pacer::new(mode);
        let mut stats = PaceStats::default();
        let mut now = start;
        let mut next = 0;
        let end = start + Duration::from_secs(7);
        // A loop waking every 250 µs, as a render loop sleeping until due.
        while now < end {
            while next < arrivals.len() && arrivals[next].0 <= now {
                let (arrived, i) = arrivals[next];
                pacer.push(i, arrived, Some(capture(i)), FPS);
                next += 1;
            }
            if let Some((_, held)) = pacer.pop(now) {
                stats.presented(now, held);
            }
            now += Duration::from_micros(250);
        }
        (stats, pacer)
    }

    /// Two frames in three arrive 12 ms late: a jittery Wi-Fi link.
    fn jittery(i: u64) -> u64 {
        5 + if i.is_multiple_of(3) { 0 } else { 12 }
    }

    #[test]
    fn lowest_latency_passes_frames_straight_through() {
        let (stats, pacer) = run(Pacing::Latency, jittery);
        assert!(stats.held_us().1 < 300, "{:?}", stats.held_us());
        assert_eq!(pacer.buffer_us(FPS), 0);
        // The jitter shows in the frame times.
        let (_, deviation, _) = stats.frame_time_us();
        assert!(deviation > 4_000, "{deviation}");
    }

    #[test]
    fn smooth_pacing_evens_out_jitter_with_a_bounded_buffer() {
        let (stats, pacer) = run(Pacing::Smooth, jittery);
        let (mean, deviation, _) = stats.frame_time_us();
        assert!((16_000..17_400).contains(&mean), "{mean}");
        // Only the first frames, before the buffer has grown, are uneven.
        assert!(deviation < 1_000, "{deviation}");
        // The buffer covers the 12 ms spread and frames wait at most that.
        let buffer = pacer.buffer_us(FPS);
        assert!((11_000..13_000).contains(&buffer), "{buffer}");
        assert!(stats.held_us().1 <= 12_500, "{:?}", stats.held_us());
        assert_eq!(pacer.dropped, 0);
    }

    #[test]
    fn smooth_pacing_caps_its_delay_and_shows_late_frames_late() {
        // A 200 ms stall every second; the frames behind it arrive in a
        // burst (decoding is in order). Never buffered for in full.
        let (stats, pacer) = run(Pacing::Smooth, |i| match i % 60 {
            stalled @ 30..43 => 200 - (stalled - 30) * 16,
            _ => 5,
        });
        assert!(pacer.buffer_us(FPS) <= 50_000);
        // So many stalled frames keep the buffer at its cap (plus the
        // 250 µs loop step), never more.
        assert!(stats.held_us().1 <= 50_500, "{:?}", stats.held_us());
        assert!(pacer.late > 0 || pacer.dropped > 0);
        // A steady link needs no buffer at all.
        let (stats, pacer) = run(Pacing::Smooth, |_| 5);
        assert_eq!(pacer.buffer_us(FPS), 0);
        assert!(stats.held_us().1 < 300, "{:?}", stats.held_us());
    }

    #[test]
    fn frames_without_capture_times_and_overflow_are_handled() {
        let mut pacer = Pacer::new(Pacing::Smooth);
        let now = Instant::now() + Duration::from_secs(1);
        pacer.push(1, now, None, FPS);
        assert_eq!(pacer.next_due(), None);
        assert_eq!(pacer.pop(now).map(|(frame, _)| frame), Some(1));
        for frame in 0..6 {
            pacer.push(frame, now, None, FPS);
        }
        assert_eq!(pacer.dropped, 2);
        // The newest due frame wins; older due frames are dropped.
        assert_eq!(pacer.pop(now).map(|(frame, _)| frame), Some(5));
        assert_eq!(pacer.dropped, 5);
        assert!(pacer.pop(now).is_none());
    }
}
