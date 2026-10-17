//! On-screen warnings when the network, the decoder or the host degrades,
//! and the F8 metrics graph (parsec-comparison G22). Both read the same
//! once-a-second session data as the stats overlay.
use crate::protocol::SteppedDown;
use std::{
    collections::{BTreeMap, VecDeque},
    time::{Duration, Instant},
};

/// How long a warning stays on screen.
const SHOWN_FOR: Duration = Duration::from_secs(6);
/// The same warning again only after this long.
const COOLDOWN: Duration = Duration::from_secs(30);
/// At most one new warning this often, so a bad second cannot fill the screen.
const SPACING: Duration = Duration::from_secs(2);
/// Warnings on screen at once.
const VISIBLE: usize = 2;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Kind {
    NetworkLoss,
    NetworkDelay,
    StepDown,
    DecoderBacklog,
    DecoderSlow,
    DecoderFallback,
    EncoderOverload,
    SoftwareEncoder,
    CaptureStall,
}

/// One second of session health, as deltas where noted.
#[derive(Clone, Debug, Default)]
pub struct Sample {
    /// The stream's frame rate (0 until known).
    pub stream_fps: u32,
    /// MoQ groups skipped or abandoned during this second.
    pub skipped_groups: u64,
    /// Decoder resets during this second (backlog or decode errors).
    pub decoder_recoveries: u64,
    /// Queueing delay of video arrivals the client last reported to the
    /// host (`NetworkFeedback`), when it sends them.
    pub queue_delay_ms: Option<u32>,
    pub stepped_down: Option<SteppedDown>,
    /// Receive→ready of the latest decoded frame, µs.
    pub decode_us: u64,
    /// The client replaced its hardware decoder with software this session.
    pub software_decoder_fallback: bool,
    /// The host's encode time for its latest frame (telemetry), µs.
    pub host_encode_us: u64,
    /// The host encodes with x264/x265 rather than a GPU.
    pub host_software_encoder: bool,
    /// Longest gap between frames arriving this second, ms.
    pub stall_ms: u64,
    /// How much longer than the session's fastest the frame ending that gap
    /// took from capture to arrival, ms; `None` without host frame timing.
    /// Late: the network held it. On time: the host captured nothing.
    pub stall_late_ms: Option<u64>,
}

/// Arrival gaps from this long count as a stall (static desktops still
/// send a frame every 250 ms).
const STALL_MS: u64 = 500;

/// Raised warnings with hysteresis and rate limits.
pub struct Warnings {
    enabled: bool,
    /// Consecutive seconds each weak condition has held.
    streaks: BTreeMap<Kind, u32>,
    raised_at: BTreeMap<Kind, Instant>,
    last_raised: Option<Instant>,
    shown: VecDeque<(Instant, String)>,
    stepped_down: Option<SteppedDown>,
    fallback: bool,
    /// The previous second showed network trouble.
    network_trouble: bool,
}

/// A condition's strength this second: a strong one raises at once, a weak
/// one only after two seconds in a row.
#[derive(PartialEq)]
enum Level {
    Clear,
    Weak,
    Strong,
}

impl Warnings {
    pub fn new(enabled: bool) -> Self {
        Self {
            enabled,
            streaks: BTreeMap::new(),
            raised_at: BTreeMap::new(),
            last_raised: None,
            shown: VecDeque::new(),
            stepped_down: None,
            fallback: false,
            network_trouble: false,
        }
    }

    /// Takes one second's sample; returns the warnings raised now, which are
    /// also shown on screen for a few seconds unless disabled.
    pub fn observe(&mut self, sample: &Sample, now: Instant) -> Vec<String> {
        let interval_us = 1_000_000 / u64::from(sample.stream_fps.max(1));
        // Lost groups, a queue or a step-down now or a second ago: decoder
        // resets and capture gaps are then the network's doing (a burst
        // after a stall, frames that never arrived), not the decoder's or
        // the host's.
        let trouble = sample.skipped_groups > 0
            || sample.queue_delay_ms.unwrap_or(0) >= 50
            || sample.stepped_down != self.stepped_down;
        // No video for a while: the host stopped capturing only if the
        // frame that ended the gap came through as fast as usual.
        let stalled = sample.stall_ms >= STALL_MS;
        let capture_stall = stalled
            && !trouble
            && !self.network_trouble
            && sample.decoder_recoveries == 0
            && sample.stall_late_ms.is_some_and(|late| late < 100);
        let outage = stalled && !capture_stall;
        let network = trouble || self.network_trouble || outage;
        self.network_trouble = trouble;
        let mut conditions = vec![
            (
                Kind::NetworkLoss,
                match (sample.skipped_groups, sample.decoder_recoveries) {
                    _ if outage => Level::Strong,
                    (0, 0) => Level::Clear,
                    (0, _) if network => Level::Strong,
                    (0, _) => Level::Clear,
                    (1, _) => Level::Weak,
                    _ => Level::Strong,
                },
                if outage {
                    format!("Network: video stalled for {} ms", sample.stall_ms)
                } else {
                    "Network: video lost, expect stutter".to_owned()
                },
            ),
            (
                Kind::NetworkDelay,
                match sample.queue_delay_ms.unwrap_or(0) {
                    0..50 => Level::Clear,
                    50..100 => Level::Weak,
                    _ => Level::Strong,
                },
                format!(
                    "Network: delay rising ({} ms queued)",
                    sample.queue_delay_ms.unwrap_or(0)
                ),
            ),
            (
                Kind::DecoderBacklog,
                if sample.decoder_recoveries > 0 && !network {
                    Level::Strong
                } else {
                    Level::Clear
                },
                "Decoder fell behind and skipped ahead".to_owned(),
            ),
            (
                Kind::DecoderSlow,
                if sample.stream_fps > 0 && sample.decode_us > interval_us {
                    Level::Weak
                } else {
                    Level::Clear
                },
                format!(
                    "Decoding takes {:.0} ms per frame, over the {} fps budget",
                    sample.decode_us as f64 / 1000.0,
                    sample.stream_fps
                ),
            ),
            (
                Kind::EncoderOverload,
                if sample.stream_fps > 0 && sample.host_encode_us * 10 > interval_us * 9 {
                    Level::Weak
                } else {
                    Level::Clear
                },
                format!(
                    "Host encoder overloaded ({:.0} ms per frame at {} fps)",
                    sample.host_encode_us as f64 / 1000.0,
                    sample.stream_fps
                ),
            ),
            (
                Kind::SoftwareEncoder,
                if sample.host_software_encoder {
                    Level::Strong
                } else {
                    Level::Clear
                },
                "Host is encoding in software: higher latency".to_owned(),
            ),
            (
                Kind::CaptureStall,
                if capture_stall {
                    Level::Strong
                } else {
                    Level::Clear
                },
                format!("Host capture stalled for {} ms", sample.stall_ms),
            ),
        ];
        // Changes rather than states: a step-down, or the decoder fallback.
        if sample.stepped_down != self.stepped_down {
            if let Some(step) = sample.stepped_down {
                conditions.push((
                    Kind::StepDown,
                    Level::Strong,
                    format!(
                        "Network: stream lowered (link ~{:.1} Mbps)",
                        f64::from(step.kbps) / 1000.0
                    ),
                ));
            }
            self.stepped_down = sample.stepped_down;
        }
        if sample.software_decoder_fallback && !self.fallback {
            self.fallback = true;
            conditions.push((
                Kind::DecoderFallback,
                Level::Strong,
                "Hardware decoder failed: decoding in software".to_owned(),
            ));
        }
        let mut raised = Vec::new();
        for (kind, level, text) in conditions {
            let streak = self.streaks.entry(kind).or_default();
            *streak = if level == Level::Clear {
                0
            } else {
                *streak + 1
            };
            let due = level == Level::Strong || *streak >= 2;
            if !due {
                continue;
            }
            if self
                .raised_at
                .get(&kind)
                .is_some_and(|at| now.saturating_duration_since(*at) < COOLDOWN)
                || self
                    .last_raised
                    .is_some_and(|at| now.saturating_duration_since(at) < SPACING)
            {
                continue;
            }
            self.raised_at.insert(kind, now);
            self.last_raised = Some(now);
            if self.enabled {
                self.shown.push_back((now, text.clone()));
            }
            raised.push(text);
        }
        raised
    }

    /// Warnings to draw now, oldest first.
    pub fn visible(&mut self, now: Instant) -> Vec<&str> {
        while self
            .shown
            .front()
            .is_some_and(|(at, _)| now.saturating_duration_since(*at) >= SHOWN_FOR)
        {
            self.shown.pop_front();
        }
        while self.shown.len() > VISIBLE {
            self.shown.pop_front();
        }
        self.shown.iter().map(|(_, text)| text.as_str()).collect()
    }
}

/// Encoder names (`ffmpeg:<name>`) that run on the CPU.
pub fn software_encoder(name: &str) -> bool {
    ["libx264", "libx265", "libsvtav1", "x264enc", "x265enc"]
        .iter()
        .any(|software| name.contains(software))
}

/// Samples kept per graph series (four seconds at 60 fps).
const GRAPH_POINTS: usize = 240;

/// Rolling series for the F8 graph: frame time and capture→present latency
/// per presented frame, and video bitrate per quarter second.
#[derive(Default)]
pub struct Graph {
    pub frame_ms: VecDeque<f32>,
    pub latency_ms: VecDeque<f32>,
    pub mbps: VecDeque<f32>,
}

impl Graph {
    fn push(series: &mut VecDeque<f32>, value: f32) {
        series.push_back(value);
        while series.len() > GRAPH_POINTS {
            series.pop_front();
        }
    }

    pub fn frame(&mut self, frame_ms: f32, latency_ms: Option<f32>) {
        Self::push(&mut self.frame_ms, frame_ms);
        if let Some(latency) = latency_ms {
            Self::push(&mut self.latency_ms, latency);
        }
    }

    pub fn bitrate(&mut self, mbps: f32) {
        Self::push(&mut self.mbps, mbps);
    }

    /// Points of `series` scaled into `rect` (x left to right, newest at
    /// the right edge; y from 0 at the bottom to `max` at the top), and the
    /// scale's top value. `floor` keeps a quiet series from filling the box.
    pub fn points(
        series: &VecDeque<f32>,
        rect: sdl2::rect::Rect,
        floor: f32,
    ) -> (Vec<sdl2::rect::Point>, f32) {
        let max = series.iter().copied().fold(floor, f32::max);
        let step = rect.width() as f32 / GRAPH_POINTS as f32;
        let start = GRAPH_POINTS - series.len();
        let points = series
            .iter()
            .enumerate()
            .map(|(index, value)| {
                let x = rect.x() as f32 + (start + index) as f32 * step;
                let y = rect.bottom() as f32
                    - 1.0
                    - (value.max(0.0) / max) * (rect.height() as f32 - 2.0);
                sdl2::rect::Point::new(x as i32, y as i32)
            })
            .collect();
        (points, max)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn quiet() -> Sample {
        Sample {
            stream_fps: 60,
            decode_us: 3_000,
            host_encode_us: 4_000,
            ..Sample::default()
        }
    }

    fn second(start: Instant, n: u64) -> Instant {
        start + Duration::from_secs(n)
    }

    #[test]
    fn a_healthy_session_raises_nothing() {
        let mut warnings = Warnings::new(true);
        let start = Instant::now();
        for n in 0..10 {
            assert!(warnings.observe(&quiet(), second(start, n)).is_empty());
        }
        assert!(warnings.visible(second(start, 10)).is_empty());
    }

    #[test]
    fn weak_conditions_need_two_seconds_and_strong_ones_raise_at_once() {
        let mut warnings = Warnings::new(true);
        let start = Instant::now();
        // One skipped group is weak: a single second of it is ignored.
        let lossy = Sample {
            skipped_groups: 1,
            ..quiet()
        };
        assert!(warnings.observe(&lossy, second(start, 0)).is_empty());
        assert!(warnings.observe(&quiet(), second(start, 1)).is_empty());
        assert!(warnings.observe(&lossy, second(start, 2)).is_empty());
        let raised = warnings.observe(&lossy, second(start, 3));
        assert_eq!(raised, ["Network: video lost, expect stutter"]);
        // A decoder reset is strong.
        let mut warnings = Warnings::new(true);
        let backlog = Sample {
            decoder_recoveries: 1,
            ..quiet()
        };
        assert_eq!(
            warnings.observe(&backlog, start),
            ["Decoder fell behind and skipped ahead"]
        );
    }

    #[test]
    fn every_trigger_has_its_threshold() {
        let start = Instant::now();
        let raise = |sample: Sample| {
            let mut warnings = Warnings::new(true);
            let mut raised = warnings.observe(&sample, start);
            raised.extend(warnings.observe(&sample, start + SPACING));
            raised
        };
        let delayed = |ms| Sample {
            queue_delay_ms: Some(ms),
            ..quiet()
        };
        assert!(raise(delayed(40)).is_empty());
        assert_eq!(raise(delayed(60)), ["Network: delay rising (60 ms queued)"]);
        // 60 fps leaves 16.7 ms per frame.
        let slow = Sample {
            decode_us: 17_000,
            ..quiet()
        };
        assert_eq!(
            raise(slow),
            ["Decoding takes 17 ms per frame, over the 60 fps budget"]
        );
        let busy = Sample {
            host_encode_us: 15_500,
            ..quiet()
        };
        assert_eq!(
            raise(busy),
            ["Host encoder overloaded (16 ms per frame at 60 fps)"]
        );
        let fine = Sample {
            host_encode_us: 14_000,
            ..quiet()
        };
        assert!(raise(fine).is_empty());
        let software = Sample {
            host_software_encoder: true,
            ..quiet()
        };
        assert_eq!(
            raise(software),
            ["Host is encoding in software: higher latency"]
        );
        // No video for 800 ms, and the next frame came through on time:
        // the host stopped capturing.
        let stalled = Sample {
            stall_ms: 800,
            stall_late_ms: Some(3),
            ..quiet()
        };
        assert_eq!(raise(stalled), ["Host capture stalled for 800 ms"]);
        // The same gap with the frame held up in transit, with lost groups,
        // or without host stamps to tell: the network's.
        let held = Sample {
            stall_ms: 800,
            stall_late_ms: Some(750),
            ..quiet()
        };
        assert_eq!(raise(held), ["Network: video stalled for 800 ms"]);
        let lost = Sample {
            stall_ms: 800,
            stall_late_ms: Some(3),
            skipped_groups: 3,
            ..quiet()
        };
        assert_eq!(raise(lost), ["Network: video stalled for 800 ms"]);
        let unknown = Sample {
            stall_ms: 800,
            ..quiet()
        };
        assert_eq!(raise(unknown), ["Network: video stalled for 800 ms"]);
        // A decoder reset after an outage, or a second after lost groups, is
        // the network's.
        let outage = Sample {
            stall_ms: 2_000,
            stall_late_ms: Some(3),
            decoder_recoveries: 1,
            ..quiet()
        };
        assert_eq!(raise(outage), ["Network: video stalled for 2000 ms"]);
        let mut warnings = Warnings::new(true);
        let lossy = Sample {
            skipped_groups: 1,
            ..quiet()
        };
        let reset = Sample {
            decoder_recoveries: 1,
            ..quiet()
        };
        assert!(warnings.observe(&lossy, start).is_empty());
        assert_eq!(
            warnings.observe(&reset, start + SPACING),
            ["Network: video lost, expect stutter"]
        );
    }

    #[test]
    fn step_down_and_fallback_raise_once_per_change() {
        let mut warnings = Warnings::new(true);
        let start = Instant::now();
        let step = SteppedDown {
            width: 1920,
            fps: 60,
            kbps: 4_200,
        };
        let stepped = Sample {
            stepped_down: Some(step),
            ..quiet()
        };
        assert_eq!(
            warnings.observe(&stepped, start),
            ["Network: stream lowered (link ~4.2 Mbps)"]
        );
        // Staying stepped down is not news.
        assert!(warnings.observe(&stepped, second(start, 40)).is_empty());
        assert!(warnings.observe(&quiet(), second(start, 41)).is_empty());
        assert_eq!(warnings.observe(&stepped, second(start, 80)).len(), 1);
        let fallback = Sample {
            software_decoder_fallback: true,
            ..quiet()
        };
        assert_eq!(
            warnings.observe(&fallback, second(start, 90)),
            ["Hardware decoder failed: decoding in software"]
        );
        assert!(warnings.observe(&fallback, second(start, 200)).is_empty());
    }

    #[test]
    fn warnings_are_rate_limited_expire_and_can_be_disabled() {
        let start = Instant::now();
        let bad = Sample {
            queue_delay_ms: Some(150),
            skipped_groups: 5,
            host_software_encoder: true,
            ..quiet()
        };
        let mut warnings = Warnings::new(true);
        // Three strong conditions, one warning per SPACING.
        assert_eq!(warnings.observe(&bad, start).len(), 1);
        assert!(
            warnings
                .observe(&bad, start + Duration::from_secs(1))
                .is_empty()
        );
        assert_eq!(warnings.observe(&bad, start + SPACING).len(), 1);
        assert_eq!(warnings.observe(&bad, start + SPACING * 2).len(), 1);
        // All three are cooling down now.
        assert!(warnings.observe(&bad, start + SPACING * 3).is_empty());
        assert_eq!(warnings.visible(start + SPACING * 3).len(), VISIBLE);
        assert!(warnings.visible(start + SPACING * 2 + SHOWN_FOR).is_empty());
        assert_eq!(warnings.observe(&bad, start + COOLDOWN).len(), 1);
        // Disabled: still raised (for the log), never shown.
        let mut off = Warnings::new(false);
        assert_eq!(off.observe(&bad, start).len(), 1);
        assert!(off.visible(start).is_empty());
    }

    #[test]
    fn graph_keeps_a_window_and_scales_into_its_box() {
        let mut graph = Graph::default();
        for frame in 0..GRAPH_POINTS + 10 {
            graph.frame(16.0 + (frame % 2) as f32, Some(20.0));
        }
        assert_eq!(graph.frame_ms.len(), GRAPH_POINTS);
        let rect = sdl2::rect::Rect::new(10, 100, 240, 42);
        let (points, max) = Graph::points(&graph.frame_ms, rect, 10.0);
        assert_eq!(max, 17.0);
        assert!(points.iter().all(|point| rect.contains_point(*point)));
        assert_eq!(points.last().unwrap().x(), 10 + 239);
        // A quiet series stays low under its floor.
        let mut quiet = VecDeque::new();
        quiet.push_back(1.0);
        let (points, max) = Graph::points(&quiet, rect, 10.0);
        assert_eq!(max, 10.0);
        assert!(points[0].y() > rect.y() + 30);
        assert!(software_encoder("ffmpeg:libx264"));
        assert!(software_encoder("ffmpeg:libsvtav1"));
        assert!(!software_encoder("ffmpeg:h264_nvenc DMA-BUF zero-copy"));
    }
}
