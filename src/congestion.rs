//! Rate control from the client's delay feedback, and the resolution/FPS
//! ladder for links too narrow for the negotiated stream (parsec-comparison
//! G6).
//!
//! The client times when the first byte of every video frame arrives against
//! the host's send timestamp. Only differences matter: the lowest value over
//! the last 10-20 s is the path without queues, and anything above it is
//! queueing, at the bottleneck or in the host's own QUIC send buffer (which the
//! host's RTT never sees). Every [`FEEDBACK_INTERVAL`] the client reports the
//! mean queueing delay and the rate it received (`Event::NetworkFeedback`).
//!
//! The host ([`DelayRateController`]) holds the encoder below the received
//! rate when the queue grows (the received rate of an overflowing link is its
//! capacity), probes upwards quickly while far from the last capacity and
//! slowly near it, and [`Ladder`] steps the stream down when even that rate
//! is below what the resolution needs.
use std::{
    collections::VecDeque,
    time::{Duration, Instant},
};

/// How often the client reports delay and received rate.
pub const FEEDBACK_INTERVAL: Duration = Duration::from_millis(100);

/// Two windows, so the minimum always covers 10-20 s of history.
const MIN_WINDOW: Duration = Duration::from_secs(10);

/// Client side: queueing delay of each frame's first byte, and received bytes.
pub struct DelayMeter {
    origin: Instant,
    /// Lowest arrival-minus-send (ms, arbitrary offset) per window.
    min: [Option<i64>; 2],
    window_start: Instant,
    /// Lowest queueing delay this interval: a standing queue delays every
    /// frame, while a burst (a keyframe) or a late wakeup delays only some.
    interval_min: Option<i64>,
    first_frame: Option<Instant>,
    /// (completion, bytes) of the frames received in the last [`RECEIVE_WINDOW`].
    received: VecDeque<(Instant, u64)>,
}

/// The reported rate is over this window: single frames (a keyframe) arrive
/// in bursts, which a 100 ms interval would turn into spikes.
const RECEIVE_WINDOW: Duration = Duration::from_millis(500);

impl DelayMeter {
    pub fn new(now: Instant) -> Self {
        Self {
            origin: now,
            min: [None, None],
            window_start: now,
            interval_min: None,
            first_frame: None,
            received: VecDeque::new(),
        }
    }

    /// A frame whose first byte arrived at `arrived` and its last at
    /// `completed`, sent at `sent` on the host's clock (any fixed offset).
    pub fn frame(&mut self, arrived: Instant, completed: Instant, sent: Duration, bytes: usize) {
        self.received.push_back((completed, bytes as u64));
        if arrived.duration_since(self.window_start) >= MIN_WINDOW {
            self.min = [None, self.min[0]];
            self.window_start = arrived;
        }
        let one_way =
            arrived.duration_since(self.origin).as_millis() as i64 - sent.as_millis() as i64;
        self.min[0] = Some(self.min[0].map_or(one_way, |min| min.min(one_way)));
        let base = self.min.iter().flatten().min().copied().unwrap_or(one_way);
        let delay = one_way - base;
        self.interval_min = Some(self.interval_min.map_or(delay, |min| min.min(delay)));
        self.first_frame.get_or_insert(arrived);
    }

    /// The interval's lowest queueing delay (ms; `None` without frames) and
    /// the rate received over the last [`RECEIVE_WINDOW`] (kbps); starts the
    /// next interval.
    pub fn report(&mut self, now: Instant) -> (Option<u32>, u32) {
        while self
            .received
            .front()
            .is_some_and(|(at, _)| now.duration_since(*at) > RECEIVE_WINDOW)
        {
            self.received.pop_front();
        }
        let window = now
            .duration_since(self.first_frame.unwrap_or(now))
            .clamp(FEEDBACK_INTERVAL, RECEIVE_WINDOW)
            .as_millis() as u64;
        let bytes: u64 = self.received.iter().map(|(_, bytes)| bytes).sum();
        let kbps = (bytes * 8 / window).min(u64::from(u32::MAX)) as u32;
        let delay = self
            .interval_min
            .take()
            .map(|delay| delay.clamp(0, 60_000) as u32);
        (delay, kbps)
    }
}

/// Queueing delay that counts as the link overflowing (two samples in a row).
const OVERUSE_MS: u32 = 30;
/// Queueing delay under which the rate may grow.
const CLEAR_MS: u32 = 20;
/// Rates are averaged over this window (several frames even at low FPS).
const RATE_WINDOW: Duration = Duration::from_millis(500);
/// The bottleneck estimate averages received rates over busy intervals of
/// this window.
const CAPACITY_WINDOW: Duration = Duration::from_secs(1);
/// Probing stays cautious (~6%/s) near the bottleneck estimate for this long
/// after the last overflow, then fast (~30%/s) again.
const CAPACITY_MEMORY: Duration = Duration::from_secs(5);

/// One feedback interval.
struct Sample {
    at: Instant,
    received: u32,
    encoded: u32,
    /// A queue was building, so the link was busy the whole interval.
    saturated: bool,
}

/// Host side: delay-based rate control (in the spirit of WebRTC's GCC, with
/// BBR-like probing), fed by `Event::NetworkFeedback`.
pub struct DelayRateController {
    current: u32,
    ceiling: u32,
    floor: u32,
    /// What an overflowing link delivered last: the bottleneck estimate.
    capacity: Option<u32>,
    rates: VecDeque<Sample>,
    over: u32,
    clear_since: Option<Instant>,
    last_decrease: Option<(Instant, u32)>,
    last_increase: Instant,
    /// Until then the queue is our own keyframe draining.
    burst_until: Option<Instant>,
}

impl DelayRateController {
    pub fn new(ceiling: u32, now: Instant) -> Self {
        Self {
            current: ceiling,
            ceiling,
            floor: ceiling.min(300),
            capacity: None,
            rates: VecDeque::new(),
            over: 0,
            clear_since: None,
            last_decrease: None,
            last_increase: now,
            burst_until: None,
        }
    }

    pub fn current(&self) -> u32 {
        self.current
    }

    /// The bottleneck estimate, when the link has overflowed recently.
    pub fn capacity(&self) -> Option<u32> {
        self.capacity
    }

    /// Mean received or encoded rate over the last [`RATE_WINDOW`]; with
    /// `saturated`, received only over intervals with a queue (the link was
    /// busy, so what arrived is what it carries) in the last second.
    fn mean(&self, now: Instant, received: bool, saturated: bool) -> u32 {
        let (sum, count) = self
            .rates
            .iter()
            .filter(|rate| {
                let age = now.duration_since(rate.at);
                if saturated {
                    rate.saturated && age <= CAPACITY_WINDOW
                } else {
                    age <= RATE_WINDOW
                }
            })
            .fold((0_u64, 0_u64), |(sum, count), rate| {
                let value = if received {
                    rate.received
                } else {
                    rate.encoded
                };
                (sum + u64::from(value), count + 1)
            });
        sum.checked_div(count).unwrap_or(0) as u32
    }

    /// Sums of received and encoded rates over the last [`CAPACITY_WINDOW`].
    fn totals(&self, now: Instant) -> (u64, u64) {
        self.rates
            .iter()
            .filter(|rate| now.duration_since(rate.at) <= CAPACITY_WINDOW)
            .fold((0, 0), |(received, encoded), rate| {
                (
                    received + u64::from(rate.received),
                    encoded + u64::from(rate.encoded),
                )
            })
    }

    /// One feedback interval: the client's queueing delay (`None` when no
    /// frame arrived), its received rate, and what the encoder produced
    /// meanwhile. `heavy_loss` (from the host's QUIC counters) counts as
    /// overflow too.
    pub fn observe(
        &mut self,
        now: Instant,
        delay_ms: Option<u32>,
        received_kbps: u32,
        encoded_kbps: u32,
        keyframe_bytes: u64,
        heavy_loss: bool,
    ) -> Option<u32> {
        let delay = delay_ms.unwrap_or(0);
        if keyframe_bytes > 0 {
            // Our own keyframe queues for its size over the link; that is
            // not congestion (and says nothing about capacity).
            let kbps = u64::from(self.capacity.unwrap_or(self.current).max(100));
            let drain = Duration::from_millis(keyframe_bytes * 8 / kbps + 100);
            self.burst_until = Some(now + drain);
        }
        let bursting = self.burst_until.is_some_and(|until| now < until);
        self.rates.push_back(Sample {
            at: now,
            received: received_kbps,
            encoded: encoded_kbps,
            saturated: delay >= OVERUSE_MS && !bursting,
        });
        while self
            .rates
            .front()
            .is_some_and(|rate| now.duration_since(rate.at) > CAPACITY_WINDOW)
        {
            self.rates.pop_front();
        }
        let previous = self.current;
        self.over = if delay >= OVERUSE_MS || heavy_loss {
            self.over + 1
        } else {
            0
        };
        if bursting && !heavy_loss {
            self.clear_since = None;
        } else if self.over >= 2 || (heavy_loss && self.over >= 1) || delay >= 4 * OVERUSE_MS {
            self.clear_since = None;
            let received = self.mean(now, true, false);
            let encoded = self.mean(now, false, false);
            // More arrives than is produced: the queue is already shrinking.
            let draining = u64::from(received) * 20 > u64::from(encoded) * 21;
            // One cut per queue: the backlog takes a while to drain, so cut
            // again only if it keeps growing, or persists without draining.
            let allowed = self.last_decrease.is_none_or(|(at, at_delay)| {
                let since = now.duration_since(at);
                !draining
                    && ((since >= Duration::from_millis(400) && delay > at_delay + OVERUSE_MS / 2)
                        || since >= Duration::from_millis(1200))
            });
            // Over the last second, less got through than was produced: the
            // link is the limit, and what a busy link delivered is what it
            // carries. A jitter or retransmission spike delays frames too,
            // but they catch up within the second.
            let limited = {
                let (received, encoded) = self.totals(now);
                received * 10 < encoded * 9
            };
            let carried = self.mean(now, true, true);
            let cut = if limited && carried > 0 {
                (self.over >= 2 || delay >= 4 * OVERUSE_MS).then(|| {
                    self.capacity = Some(carried);
                    carried * 85 / 100
                })
            } else {
                // A queue that is not ours (cross traffic) and persists:
                // back off gently.
                (self.over >= 5).then_some(self.current * 9 / 10)
            };
            if let Some(target) = cut.filter(|_| allowed) {
                // Later events never take more than half: a client that stops
                // reading (CPU starved) looks like a very narrow link. The
                // first goes all the way: a session starting far above its
                // link must not build seconds of backlog.
                let limit = if self.last_decrease.is_some() {
                    self.current / 2
                } else {
                    0
                };
                self.current = self.current.min(target.max(limit)).max(self.floor);
                self.last_decrease = Some((now, delay));
            }
        } else if delay_ms.is_some() && delay < CLEAR_MS {
            // (No frame arrived: nothing is known, so the rate holds.)
            let clear_since = *self.clear_since.get_or_insert(now);
            let encoded = self.mean(now, false, false);
            if now.duration_since(clear_since) >= Duration::from_millis(300)
                && self
                    .last_decrease
                    .is_none_or(|(at, _)| now.duration_since(at) >= Duration::from_millis(600))
                && now.duration_since(self.last_increase) >= Duration::from_millis(300)
                // Content that does not fill the target says nothing about
                // the link; do not let the target creep up meanwhile.
                && u64::from(encoded) * 10 >= u64::from(self.current) * 6
            {
                let stale = self
                    .last_decrease
                    .is_none_or(|(at, _)| now.duration_since(at) >= CAPACITY_MEMORY);
                if stale
                    || self.capacity.is_some_and(|capacity| {
                        u64::from(self.current) * 4 > u64::from(capacity) * 5
                    })
                {
                    // Well past the old bottleneck, or long without reaching
                    // it: it moved (or was a CPU stall), probe fast again.
                    self.capacity = None;
                }
                let near = self
                    .capacity
                    .is_some_and(|capacity| u64::from(self.current) * 5 >= u64::from(capacity) * 4);
                let step = if near {
                    (self.current / 50).max(20)
                } else {
                    self.current / 12 + 50
                };
                self.current = (self.current + step).min(self.ceiling);
                self.last_increase = now;
            }
        } else {
            self.clear_since = None;
        }
        (previous != self.current).then_some(self.current)
    }
}

/// Bits per pixel below which a lower resolution looks better than a starved
/// one (H.264; H.265 needs about two thirds). At 30 FPS: 720p 1.4 Mbps,
/// 1080p 3.1 Mbps; at 60 FPS twice that.
const FLOOR_BITS_PER_PIXEL: f64 = 0.05;
/// A lower step must hold this long before stepping down, and the rate must
/// clear the step above by [`STEP_UP_MARGIN`] this long before stepping up.
const STEP_DOWN_AFTER: Duration = Duration::from_secs(4);
const STEP_UP_AFTER: Duration = Duration::from_secs(10);
const STEP_UP_MARGIN: f64 = 1.4;
/// Every step restarts the encoder (a short video gap); never more often.
const MIN_STEP_INTERVAL: Duration = Duration::from_secs(8);

/// Lowest bitrate (kbps) worth streaming `width`x`height` at `fps`.
pub fn floor_kbps(width: u32, height: u32, fps: u32, hevc: bool) -> u32 {
    let codec = if hevc { 2.0 / 3.0 } else { 1.0 };
    (f64::from(width) * f64::from(height) * f64::from(fps) * FLOOR_BITS_PER_PIXEL * codec / 1000.0)
        as u32
}

/// One rung: stream width and frame rate.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Step {
    pub width: u32,
    pub fps: u32,
}

/// Resolution first (100%, 75%, 50% width), then half the frame rate. The
/// negotiated stream is the top rung; nothing above it is ever chosen.
pub struct Ladder {
    steps: Vec<Step>,
    /// Source aspect: height = width * aspect.
    aspect: f64,
    hevc: bool,
    level: usize,
    below_since: Option<Instant>,
    above_since: Option<Instant>,
    last_step: Option<Instant>,
}

impl Ladder {
    /// `width`x`height` at `fps` is the negotiated stream.
    pub fn new(width: u32, height: u32, fps: u32, hevc: bool) -> Self {
        let mut steps = vec![Step { width, fps }];
        for scale in [3, 2] {
            let scaled = (width * scale / 4) / 8 * 8;
            if scaled >= 320 && scaled < steps.last().unwrap().width {
                steps.push(Step { width: scaled, fps });
            }
        }
        let last = *steps.last().unwrap();
        if fps >= 30 {
            steps.push(Step {
                width: last.width,
                fps: fps / 2,
            });
        }
        Self {
            steps,
            aspect: f64::from(height) / f64::from(width.max(1)),
            hevc,
            level: 0,
            below_since: None,
            above_since: None,
            last_step: None,
        }
    }

    pub fn level(&self) -> usize {
        self.level
    }

    pub fn top(&self) -> Step {
        self.steps[0]
    }

    fn floor(&self, level: usize) -> u32 {
        let step = self.steps[level];
        let height = (f64::from(step.width) * self.aspect) as u32;
        floor_kbps(step.width, height, step.fps, self.hevc)
    }

    /// The sustainable rate (kbps) now; returns the rung to switch to.
    pub fn observe(&mut self, now: Instant, kbps: u32) -> Option<Step> {
        let settled = self
            .last_step
            .is_none_or(|at| now.duration_since(at) >= MIN_STEP_INTERVAL);
        if self.level + 1 < self.steps.len() && kbps < self.floor(self.level) {
            self.above_since = None;
            let since = *self.below_since.get_or_insert(now);
            if settled && now.duration_since(since) >= STEP_DOWN_AFTER {
                return Some(self.go(now, self.level + 1));
            }
            return None;
        }
        self.below_since = None;
        if self.level > 0
            && f64::from(kbps) >= f64::from(self.floor(self.level - 1)) * STEP_UP_MARGIN
        {
            let since = *self.above_since.get_or_insert(now);
            if settled && now.duration_since(since) >= STEP_UP_AFTER {
                return Some(self.go(now, self.level - 1));
            }
        } else {
            self.above_since = None;
        }
        None
    }

    fn go(&mut self, now: Instant, level: usize) -> Step {
        self.level = level;
        self.last_step = Some(now);
        self.below_since = None;
        self.above_since = None;
        self.steps[level]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MS: Duration = Duration::from_millis(1);

    #[test]
    fn meter_reports_delay_above_the_lowest_one_way_time() {
        let start = Instant::now();
        let mut meter = DelayMeter::new(start);
        // Host clock 5 s ahead: only differences matter.
        meter.frame(
            start + 20 * MS,
            start + 20 * MS,
            Duration::from_secs(5),
            1000,
        );
        meter.frame(
            start + 60 * MS,
            start + 60 * MS,
            Duration::from_secs(5) + 30 * MS,
            1000,
        );
        let (delay, kbps) = meter.report(start + 100 * MS);
        assert_eq!(delay, Some(0));
        assert_eq!(kbps, 160);
        assert_eq!(meter.report(start + 200 * MS), (None, 88));
    }

    fn feed(
        rate: &mut DelayRateController,
        start: Instant,
        from: u64,
        to: u64,
        delay: u32,
        received: u32,
        encoded: u32,
    ) {
        for t in (from..to).step_by(100) {
            rate.observe(
                start + MS * t as u32,
                Some(delay),
                received,
                encoded,
                0,
                false,
            );
        }
    }

    #[test]
    fn overflow_cuts_to_what_the_link_delivered_and_probes_back_slowly() {
        let start = Instant::now();
        let mut rate = DelayRateController::new(16_000, start);
        // 16 Mbps into 5: the queue grows; the client receives 5.
        assert_eq!(rate.observe(start, Some(40), 5000, 16_000, 0, false), None);
        // The first cut goes straight to what the link carried.
        assert_eq!(
            rate.observe(start + 100 * MS, Some(60), 5000, 16_000, 0, false),
            Some(4250)
        );
        assert_eq!(rate.capacity(), Some(5000));
        // Later ones take at most half (a stalled client looks like a narrow link).
        let mut later = DelayRateController::new(16_000, start);
        later.last_decrease = Some((start, 0));
        later.observe(start + 1500 * MS, Some(40), 1000, 16_000, 0, false);
        assert_eq!(
            later.observe(start + 1600 * MS, Some(60), 1000, 16_000, 0, false),
            Some(8000)
        );
        feed(&mut rate, start, 200, 600, 60, 5000, 4250);
        // Draining: no further cut while the queue shrinks.
        feed(&mut rate, start, 600, 1000, 50, 5000, 4250);
        assert_eq!(rate.current(), 4250);
        // Clear: grows near the old capacity only by ~2% steps.
        feed(&mut rate, start, 1000, 2000, 2, 4250, 4250);
        assert!(
            rate.current() > 4250 && rate.current() < 4600,
            "{}",
            rate.current()
        );
    }

    #[test]
    fn queue_without_our_overflow_backs_off_gently_and_far_probes_are_fast() {
        let start = Instant::now();
        let mut rate = DelayRateController::new(8000, start);
        feed(&mut rate, start, 0, 500, 40, 8000, 8000);
        assert_eq!(rate.current(), 7200);
        assert_eq!(rate.capacity(), None);
        feed(&mut rate, start, 500, 3000, 0, 7200, 7200);
        assert_eq!(rate.current(), 8000);
    }

    #[test]
    fn idle_content_does_not_raise_the_target() {
        let start = Instant::now();
        let mut rate = DelayRateController::new(8000, start);
        feed(&mut rate, start, 0, 400, 40, 2000, 4000);
        assert_eq!(rate.current(), 1700);
        // Once the busy samples have left the window, nothing grows.
        feed(&mut rate, start, 400, 1000, 0, 200, 200);
        let cut = rate.current();
        feed(&mut rate, start, 1000, 6000, 0, 200, 200);
        assert_eq!(rate.current(), cut);
    }

    #[test]
    fn ladder_steps_down_resolution_then_fps_and_back_with_hysteresis() {
        let start = Instant::now();
        let mut ladder = Ladder::new(1280, 720, 30, false);
        assert_eq!(ladder.floor(0), 1382);
        let at = |s: u64| start + Duration::from_secs(s);
        assert_eq!(ladder.observe(at(0), 1000), None);
        assert_eq!(ladder.observe(at(3), 1000), None);
        assert_eq!(
            ladder.observe(at(4), 1000),
            Some(Step {
                width: 960,
                fps: 30
            })
        );
        // 960 needs ~780 kbps: 1000 holds it.
        for s in 5..30 {
            assert_eq!(ladder.observe(at(s), 1000), None);
        }
        // Up again only well above the 1280 floor, for 10 s.
        assert_eq!(ladder.observe(at(30), 2000), None);
        assert_eq!(
            ladder.observe(at(40), 2000),
            Some(Step {
                width: 1280,
                fps: 30
            })
        );
        // Starved: 75%, 50%, then half rate, each after the minimum interval.
        let mut steps = Vec::new();
        for s in 41..120 {
            steps.extend(ladder.observe(at(s), 100));
        }
        assert_eq!(
            steps,
            [
                Step {
                    width: 960,
                    fps: 30
                },
                Step {
                    width: 640,
                    fps: 30
                },
                Step {
                    width: 640,
                    fps: 15
                }
            ]
        );
        assert_eq!(
            ladder.top(),
            Step {
                width: 1280,
                fps: 30
            }
        );
    }
}
