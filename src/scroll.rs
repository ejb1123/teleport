//! Client wheel and trackpad scrolling (docs/scrolling.md).
//!
//! SDL reports every scroll as a `MouseWheel` event with precise (fractional)
//! deltas in "wheel notch" units, and nothing about the device or gesture:
//!
//! - macOS (SDL3 Cocoa, also behind sdl2-compat) rounds deltas from devices
//!   without precise scrolling (mouse wheels) away from zero to whole notches
//!   and passes trackpad and Magic Mouse deltas through as fractions
//!   (`NSEvent.deltaY`). Momentum (coasting) after the fingers lift arrives
//!   as more wheel events; SDL drops the gesture phases. On macOS the client
//!   reads them itself (src/mac_scroll.rs): [`Device`] per wheel event and
//!   [`Marker`]s for the phases.
//! - Wayland (SDL3) reports wheels in whole notches (value120 / 120) and
//!   fingers in pixels / 10; X11 XInput2 reports smooth valuators the same
//!   way, one unit per notch. There the device is guessed (a gesture whose
//!   first delta is fractional is a trackpad) and a gesture ends after
//!   [`GESTURE_END`] without deltas.
//!
//! Trackpad gestures travel as smooth pixel deltas (`Event::ScrollSmooth`,
//! [`PIXELS_PER_UNIT`] host pixels per unit times `--scroll-speed`) to hosts
//! that inject them, and as whole notches every [`PIXELS_PER_NOTCH`] pixels
//! to hosts that cannot (X11 hosts: XTest has only buttons 4-7; older
//! hosts). Wheels stay whole notches everywhere.
//!
//! Inertia: macOS's own momentum deltas are forwarded, so coasting feels and
//! stops exactly as in a Mac window. `finish` (the axis stop that lets host
//! applications start *their* kinetic scrolling) goes out only once the
//! momentum has ended, the fingers lifted without any, or a new touch
//! stopped it; by then the velocity is about zero, so the host adds no
//! second inertia.
//!
//! Direction: SDL's values are already in "content" terms, i.e. they follow
//! the local natural-scrolling preference (SDL marks such events `FLIPPED`).
//! `--scroll-direction auto` sends them as they are, so host content moves
//! as in a native window on this computer. `natural` and `traditional`
//! force one convention from the device's physical direction. Host-side
//! natural scrolling settings apply to the host's own touchpads (libinput
//! device options), not to the portal's or XTest's virtual pointer, so
//! nothing is inverted twice.
use crate::protocol::{Event, MAX_SMOOTH_SCROLL};
use anyhow::{Result, ensure};
use std::time::{Duration, Instant};

/// Host pixels per SDL precise unit at `--scroll-speed 1` (SDL's own
/// Wayland conversion is ten pixels per unit, and a Mac trackpad unit is
/// about ten points).
pub const PIXELS_PER_UNIT: f64 = 10.0;
/// Smooth input to a discrete host: one wheel notch (about three lines in
/// most applications) per this many pixels.
pub const PIXELS_PER_NOTCH: f64 = 50.0;
/// Without gesture phases, a gesture ends after this long without deltas.
pub const GESTURE_END: Duration = Duration::from_millis(100);
/// With phases: how long after the fingers lift momentum may still begin.
pub const LIFT_GRACE: Duration = Duration::from_millis(50);
/// With phases: a safety end if a gesture's end marker never arrives.
pub const PHASED_IDLE: Duration = Duration::from_millis(500);
/// Smooth deltas are coalesced to at most one event per interval (~120 Hz).
pub const SMOOTH_INTERVAL: Duration = Duration::from_millis(8);
/// SDL's per-event deltas are far below this; larger values are corrupt.
const MAX_UNITS: f64 = 200.0;

/// `--scroll-direction`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, clap::ValueEnum)]
pub enum Direction {
    /// As on this computer (its natural-scrolling setting).
    #[default]
    Auto,
    /// Content follows the fingers (and the wheel), whatever this computer does.
    Natural,
    /// Wheel down / fingers up scrolls down, whatever this computer does.
    Traditional,
}

impl Direction {
    pub fn label(self) -> &'static str {
        match self {
            Self::Auto => "as this computer",
            Self::Natural => "natural",
            Self::Traditional => "traditional",
        }
    }

    /// The value to send for SDL's `value`, which is inverted from the
    /// device when `flipped` (local natural scrolling).
    fn apply(self, value: f64, flipped: bool) -> f64 {
        let physical = if flipped { -value } else { value };
        match self {
            Self::Auto => value,
            Self::Traditional => physical,
            Self::Natural => -physical,
        }
    }
}

/// Where the pointer is for a wheel event.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Target {
    /// Over the remote desktop at this normalised position.
    Desktop(f64, f64),
    /// Pointer lock (relative mode): no absolute position is sent.
    Locked,
    /// Over local UI or outside the video: nothing reaches the host.
    Outside,
}

/// What the platform says about one wheel event (macOS).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Device {
    /// Trackpad or Magic Mouse; false for a notched mouse wheel.
    pub precise: bool,
    /// Momentum (coasting) after the fingers lifted.
    pub momentum: bool,
}

/// Gesture phases (macOS), in order with the wheel events.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Marker {
    /// Fingers touched (or rested on) the trackpad: stops any coasting.
    Touch,
    /// Fingers lifted; momentum may follow.
    Lift,
    MomentumStart,
    MomentumEnd,
    /// The system cancelled the gesture.
    Cancel,
}

#[derive(Debug)]
struct Gesture {
    smooth: bool,
    /// Driven by platform phases rather than the idle timeout.
    phased: bool,
    last: Instant,
    /// Fingers lifted: end unless momentum begins by then.
    lift_deadline: Option<Instant>,
    /// Smooth pixels not yet sent.
    pending: [f64; 2],
    last_sent: Option<Instant>,
    /// A smooth delta went out: the gesture owes the host a `finish`.
    sent: bool,
    /// Last position sent before this gesture's deltas.
    position: Option<(f64, f64)>,
}

impl Gesture {
    fn new(smooth: bool, phased: bool, now: Instant) -> Self {
        Self {
            smooth,
            phased,
            last: now,
            lift_deadline: None,
            pending: [0.0; 2],
            last_sent: None,
            sent: false,
            position: None,
        }
    }

    fn end_at(&self) -> Instant {
        if self.phased {
            let idle = self.last + PHASED_IDLE;
            self.lift_deadline.map_or(idle, |lift| lift.min(idle))
        } else {
            self.last + GESTURE_END
        }
    }
}

#[derive(Debug)]
pub struct Scroller {
    smooth_host: bool,
    pixels_per_unit: f64,
    direction: Direction,
    /// Whole-notch remainder (wheels), in notches.
    notches: [f64; 2],
    /// Smooth input to a discrete host, in pixels.
    pixels: [f64; 2],
    gesture: Option<Gesture>,
}

impl Default for Scroller {
    fn default() -> Self {
        Self::new(false, 1.0, Direction::Auto)
    }
}

impl Scroller {
    /// `smooth_host`: the host advertises `Desktop::smooth_scroll`.
    pub fn new(smooth_host: bool, speed: f64, direction: Direction) -> Self {
        Self {
            smooth_host,
            pixels_per_unit: PIXELS_PER_UNIT * speed.clamp(0.1, 10.0),
            direction,
            notches: [0.0; 2],
            pixels: [0.0; 2],
            gesture: None,
        }
    }

    /// Forgets partial deltas (focus changes, toolbar clicks, new streams),
    /// finishing an open smooth gesture.
    pub fn reset(&mut self) -> Vec<Event> {
        let events = self.end(Instant::now());
        self.notches = [0.0; 2];
        self.pixels = [0.0; 2];
        events
    }

    /// Translates one SDL wheel event. `integer` and `precise` are SDL's
    /// fields (precise replaces integer when present); `flipped` is SDL's
    /// direction; `device` comes from the platform where it can tell.
    pub fn wheel(
        &mut self,
        integer: (i32, i32),
        precise: (f32, f32),
        flipped: bool,
        target: Target,
        device: Option<Device>,
        now: Instant,
    ) -> Result<Vec<Event>> {
        let mut events = Vec::new();
        if target == Target::Outside {
            return Ok(self.reset());
        }
        let mut delta = [0.0_f64; 2];
        for (slot, (integer, precise)) in delta
            .iter_mut()
            .zip([(integer.0, precise.0), (integer.1, precise.1)])
        {
            // Precise deltas replace, not supplement, SDL's integer values.
            // Older backends may supply only the integer field.
            let value = if precise.is_finite() && precise != 0.0 {
                f64::from(precise)
            } else {
                f64::from(integer)
            };
            ensure!(
                value.abs() <= MAX_UNITS,
                "wheel event exceeds bounded input budget"
            );
            *slot = self.direction.apply(value, flipped);
        }
        if delta == [0.0, 0.0] {
            return Ok(events);
        }
        if self.gesture.as_ref().is_some_and(|gesture| {
            now >= gesture.end_at()
                // A mouse wheel during trackpad coasting, or the other way.
                || device.is_some_and(|device| device.precise != gesture.smooth)
        }) {
            events.extend(self.end(now));
        }
        let smooth = match device {
            Some(device) => device.precise,
            None => delta
                .iter()
                .any(|value| (value - value.round()).abs() > 1e-3),
        };
        let gesture = self
            .gesture
            .get_or_insert_with(|| Gesture::new(smooth, false, now));
        gesture.last = now;
        if let Some(device) = device
            && gesture.smooth
        {
            gesture.phased = true;
            if device.momentum {
                gesture.lift_deadline = None;
            }
        }
        let position = match target {
            Target::Desktop(x, y) => Some((x, y)),
            _ => None,
        };
        // Clicks and wheel steps land where the pointer is: the host gets
        // its position first, never a stale coalesced motion afterwards.
        let motion = |events: &mut Vec<Event>, gesture: &mut Gesture| {
            if let Some((x, y)) = position
                && gesture.position != Some((x, y))
            {
                events.push(Event::Motion { x, y });
                gesture.position = Some((x, y));
            }
        };
        let mut steps = [0_i32; 2];
        if gesture.smooth && self.smooth_host {
            for (pending, value) in gesture.pending.iter_mut().zip(delta) {
                *pending += value * self.pixels_per_unit;
            }
            if gesture
                .last_sent
                .is_none_or(|sent| now.saturating_duration_since(sent) >= SMOOTH_INTERVAL)
            {
                motion(&mut events, gesture);
                Self::send_smooth(gesture, now, false, &mut events);
            }
            return Ok(events);
        } else if gesture.smooth {
            for (index, value) in delta.into_iter().enumerate() {
                self.pixels[index] += value * self.pixels_per_unit;
                steps[index] = whole(self.pixels[index] / PIXELS_PER_NOTCH);
                self.pixels[index] -= f64::from(steps[index]) * PIXELS_PER_NOTCH;
            }
        } else {
            for (index, value) in delta.into_iter().enumerate() {
                self.notches[index] += value;
                steps[index] = whole(self.notches[index]);
                self.notches[index] -= f64::from(steps[index]);
                if self.notches[index].abs() < 1e-6 {
                    self.notches[index] = 0.0;
                }
            }
        }
        if steps == [0, 0] {
            return Ok(events);
        }
        motion(&mut events, gesture);
        while steps != [0, 0] {
            let x = steps[0].clamp(-20, 20);
            let y = steps[1].clamp(-20, 20);
            events.push(Event::Scroll { x, y });
            steps[0] -= x;
            steps[1] -= y;
        }
        Ok(events)
    }

    /// A platform gesture phase, in order with the wheel events.
    pub fn marker(&mut self, marker: Marker, now: Instant) -> Vec<Event> {
        match marker {
            Marker::Touch => {
                // A touch stops coasting at once, as in a Mac window: end
                // what is running (its `finish`), then expect new deltas.
                let events = self.end(now);
                self.gesture = Some(Gesture::new(true, true, now));
                events
            }
            Marker::Lift => {
                if let Some(gesture) = &mut self.gesture
                    && gesture.smooth
                {
                    gesture.phased = true;
                    gesture.last = now;
                    gesture.lift_deadline = Some(now + LIFT_GRACE);
                }
                Vec::new()
            }
            Marker::MomentumStart => {
                let gesture = self
                    .gesture
                    .get_or_insert_with(|| Gesture::new(true, true, now));
                gesture.phased = true;
                gesture.last = now;
                gesture.lift_deadline = None;
                Vec::new()
            }
            Marker::MomentumEnd | Marker::Cancel => self.end(now),
        }
    }

    /// Coalesced smooth deltas that are due, and the end of an idle gesture.
    /// Call once per event batch, before sending coalesced pointer motion.
    pub fn poll(&mut self, now: Instant) -> Vec<Event> {
        let Some(gesture) = &mut self.gesture else {
            return Vec::new();
        };
        if now >= gesture.end_at() {
            return self.end(now);
        }
        let mut events = Vec::new();
        if gesture.pending != [0.0, 0.0]
            && gesture
                .last_sent
                .is_none_or(|sent| now.saturating_duration_since(sent) >= SMOOTH_INTERVAL)
        {
            Self::send_smooth(gesture, now, false, &mut events);
        }
        events
    }

    /// Sends pending smooth deltas now (before pointer movement or a click,
    /// so they land after the scroll that preceded them locally).
    pub fn flush(&mut self, now: Instant) -> Vec<Event> {
        let mut events = Vec::new();
        if let Some(gesture) = &mut self.gesture
            && gesture.pending != [0.0, 0.0]
        {
            Self::send_smooth(gesture, now, false, &mut events);
        }
        events
    }

    /// When `poll` next has work: the render loop sleeps no longer.
    pub fn deadline(&self) -> Option<Instant> {
        let gesture = self.gesture.as_ref()?;
        let end = gesture.end_at();
        Some(match gesture.last_sent {
            Some(sent) if gesture.pending != [0.0, 0.0] => end.min(sent + SMOOTH_INTERVAL),
            _ => end,
        })
    }

    fn end(&mut self, now: Instant) -> Vec<Event> {
        let mut events = Vec::new();
        if let Some(mut gesture) = self.gesture.take() {
            if gesture.smooth && self.smooth_host && (gesture.sent || gesture.pending != [0.0; 2]) {
                Self::send_smooth(&mut gesture, now, true, &mut events);
            }
            if gesture.smooth {
                // A trackpad's leftover fraction of a notch is not owed to
                // the next gesture.
                self.pixels = [0.0; 2];
            }
        }
        events
    }

    fn send_smooth(gesture: &mut Gesture, now: Instant, finish: bool, events: &mut Vec<Event>) {
        let [mut dx, mut dy] = gesture.pending;
        gesture.pending = [0.0; 2];
        gesture.last_sent = Some(now);
        gesture.sent = true;
        // Split the (rare) burst larger than one event may carry.
        loop {
            let x = dx.clamp(-MAX_SMOOTH_SCROLL, MAX_SMOOTH_SCROLL);
            let y = dy.clamp(-MAX_SMOOTH_SCROLL, MAX_SMOOTH_SCROLL);
            dx -= x;
            dy -= y;
            let last = dx == 0.0 && dy == 0.0;
            events.push(Event::ScrollSmooth {
                dx: x,
                dy: y,
                finish: finish && last,
            });
            if last {
                break;
            }
        }
    }
}

/// Whole steps toward zero, tolerating f32 rounding at a step boundary.
fn whole(value: f64) -> i32 {
    (value + value.signum() * 1e-6).trunc() as i32
}

/// Host side: smooth pixels to whole notches for backends without smooth
/// scrolling (XTest buttons 4-7), if a client sends them anyway.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
#[derive(Debug, Default)]
pub struct Notches {
    pixels: [f64; 2],
}

#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
impl Notches {
    /// Whole notches (SDL signs: +y up, +x right) for these pixels.
    pub fn add(&mut self, dx: f64, dy: f64, finish: bool) -> (i32, i32) {
        let mut steps = [0; 2];
        for (index, value) in [dx, dy].into_iter().enumerate() {
            self.pixels[index] += value;
            steps[index] = whole(self.pixels[index] / PIXELS_PER_NOTCH).clamp(-20, 20);
            self.pixels[index] -= f64::from(steps[index]) * PIXELS_PER_NOTCH;
        }
        if finish {
            self.pixels = [0.0; 2];
        }
        (steps[0], steps[1])
    }
}

/// Platform gesture information for SDL wheel events: macOS reads the
/// phases itself (src/mac_scroll.rs); elsewhere there is none.
pub mod platform {
    #[cfg(not(target_os = "macos"))]
    use super::{Device, Marker};

    #[cfg(target_os = "macos")]
    pub use crate::mac_scroll::{Monitor, take};

    /// No gesture information here: [`take`] always returns none.
    #[cfg(not(target_os = "macos"))]
    pub struct Monitor;

    #[cfg(not(target_os = "macos"))]
    impl Monitor {
        pub fn install() -> Option<Self> {
            None
        }
    }

    /// Markers queued before the next wheel event, and that event's device
    /// (`wheel`: true for an SDL wheel event, false for the batch's end).
    #[cfg(not(target_os = "macos"))]
    pub fn take(_wheel: bool) -> (Vec<Marker>, Option<Device>) {
        (Vec::new(), None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CENTRE: Target = Target::Desktop(0.5, 0.5);
    const TRACKPAD: Option<Device> = Some(Device {
        precise: true,
        momentum: false,
    });
    const MOMENTUM: Option<Device> = Some(Device {
        precise: true,
        momentum: true,
    });
    const WHEEL: Option<Device> = Some(Device {
        precise: false,
        momentum: false,
    });

    fn at(start: Instant, ms: u64) -> Instant {
        start + Duration::from_millis(ms)
    }

    fn smooth_total(events: &[Event]) -> (f64, f64, usize) {
        let mut total = (0.0, 0.0, 0);
        for event in events {
            event.validate().unwrap();
            if let Event::ScrollSmooth { dx, dy, finish } = event {
                total.0 += dx;
                total.1 += dy;
                total.2 += usize::from(*finish);
            }
        }
        total
    }

    fn notch_total(events: &[Event]) -> (i32, i32) {
        events.iter().fold((0, 0), |total, event| match event {
            Event::Scroll { x, y } => (total.0 + x, total.1 + y),
            _ => total,
        })
    }

    fn finishes(events: &[Event]) -> usize {
        smooth_total(events).2
    }

    #[test]
    fn wheel_notches_stay_discrete_with_motion_first() -> Result<()> {
        let start = Instant::now();
        for (smooth_host, device) in [(false, None), (true, None), (true, WHEEL)] {
            let mut scroller = Scroller::new(smooth_host, 1.0, Direction::Auto);
            let events = scroller.wheel(
                (0, 1),
                (0.0, 1.0),
                false,
                Target::Desktop(0.3, 0.7),
                device,
                start,
            )?;
            assert!(matches!(events[0], Event::Motion { x, y } if x == 0.3 && y == 0.7));
            assert_eq!(notch_total(&events), (0, 1));
            // The same position is not repeated within the gesture.
            let events = scroller.wheel(
                (45, -23),
                (0.0, 0.0),
                false,
                Target::Desktop(0.3, 0.7),
                device,
                at(start, 10),
            )?;
            assert!(
                events
                    .iter()
                    .all(|event| matches!(event, Event::Scroll { .. }))
            );
            assert_eq!(notch_total(&events), (45, -23));
            for event in &events {
                event.validate()?;
            }
            // Wheels never send smooth deltas or a finish.
            assert!(scroller.poll(at(start, 1000)).is_empty());
        }
        Ok(())
    }

    /// The matrix: local natural scrolling on/off x trackpad/wheel x
    /// `--scroll-direction`. "Down" is the gesture that shows content
    /// further down in a traditional setup (wheel towards the user, two
    /// fingers up): physically -1 in SDL's signs. With natural scrolling
    /// the OS reports the opposite value and SDL marks it flipped. Host
    /// natural-scrolling settings do not touch virtual pointers, so what
    /// leaves here is what the host applies.
    #[test]
    fn direction_matrix_never_inverts_twice() -> Result<()> {
        let start = Instant::now();
        for local_natural in [false, true] {
            for device in [TRACKPAD, WHEEL] {
                for direction in [Direction::Auto, Direction::Natural, Direction::Traditional] {
                    let physical = -1.0_f32;
                    let reported = if local_natural { -physical } else { physical };
                    let mut scroller = Scroller::new(true, 1.0, direction);
                    let events = scroller.wheel(
                        (0, reported as i32),
                        (0.0, reported * if device == TRACKPAD { 0.5 } else { 1.0 }),
                        local_natural,
                        CENTRE,
                        device,
                        start,
                    )?;
                    let sent = if device == TRACKPAD {
                        smooth_total(&events).1.signum()
                    } else {
                        f64::from(notch_total(&events).1.signum())
                    };
                    // What a native window on this computer does: the
                    // reported value, natural scrolling included.
                    let native = f64::from(reported.signum());
                    let expected = match direction {
                        Direction::Auto => native,
                        Direction::Traditional => f64::from(physical.signum()),
                        Direction::Natural => -f64::from(physical.signum()),
                    };
                    assert_eq!(
                        sent, expected,
                        "natural={local_natural} {device:?} {direction:?}"
                    );
                }
            }
        }
        Ok(())
    }

    #[test]
    fn horizontal_and_diagonal_pans_keep_both_axes() -> Result<()> {
        let start = Instant::now();
        let mut scroller = Scroller::new(true, 1.0, Direction::Auto);
        let events = scroller.wheel((0, 0), (0.75, -0.5), false, CENTRE, TRACKPAD, start)?;
        let (dx, dy, _) = smooth_total(&events);
        assert!((dx - 7.5).abs() < 1e-6 && (dy + 5.0).abs() < 1e-6);
        let mut scroller = Scroller::new(false, 1.0, Direction::Traditional);
        let events = scroller.wheel((-2, 0), (-2.0, 0.0), true, CENTRE, WHEEL, start)?;
        assert_eq!(notch_total(&events), (2, 0));
        Ok(())
    }

    #[test]
    fn trackpad_deltas_are_scaled_coalesced_and_finished_by_timeout() -> Result<()> {
        let start = Instant::now();
        let mut scroller = Scroller::new(true, 1.5, Direction::Auto);
        let mut events = scroller.wheel((0, 0), (0.0, 0.25), false, CENTRE, None, start)?;
        assert!(matches!(events[0], Event::Motion { .. }));
        // Within the coalescing interval: held, then sent by poll.
        for ms in [2, 4, 6] {
            let more = scroller.wheel((0, 0), (0.1, 0.25), false, CENTRE, None, at(start, ms))?;
            assert!(more.is_empty(), "{more:?}");
        }
        assert!(scroller.poll(at(start, 7)).is_empty());
        assert!(scroller.deadline().unwrap() <= at(start, 8));
        events.extend(scroller.poll(at(start, 8)));
        let (dx, dy, finished) = smooth_total(&events);
        assert!((dx - 0.3 * 15.0).abs() < 1e-3 && (dy - 1.0 * 15.0).abs() < 1e-3);
        assert_eq!(finished, 0);
        // Without phases idleness finishes it, once.
        assert!(scroller.poll(at(start, 90)).is_empty());
        let end = scroller.poll(at(start, 6 + 100));
        assert_eq!(end.len(), 1);
        assert!(
            matches!(end[0], Event::ScrollSmooth { dx, dy, finish: true } if dx == 0.0 && dy == 0.0)
        );
        assert!(scroller.poll(at(start, 400)).is_empty());
        assert!(scroller.deadline().is_none());
        Ok(())
    }

    /// macOS phases: finger scrolling, lift, momentum (forwarded), end. The
    /// finish waits for the momentum's end: no second inertia on the host.
    #[test]
    fn mac_momentum_is_forwarded_and_finished_only_when_it_ends() -> Result<()> {
        let start = Instant::now();
        let mut scroller = Scroller::new(true, 1.0, Direction::Auto);
        let mut events = scroller.marker(Marker::Touch, start);
        for ms in (0..60).step_by(10) {
            events.extend(scroller.wheel(
                (0, 0),
                (0.0, -1.5),
                false,
                CENTRE,
                TRACKPAD,
                at(start, ms),
            )?);
        }
        events.extend(scroller.marker(Marker::Lift, at(start, 60)));
        // A slow start of momentum: within the lift grace.
        assert!(scroller.poll(at(start, 100)).is_empty());
        events.extend(scroller.marker(Marker::MomentumStart, at(start, 105)));
        // Long coasting with gaps beyond the timeout heuristic's.
        for ms in (110..1400).step_by(130) {
            events.extend(scroller.wheel(
                (0, 0),
                (0.0, -0.25),
                false,
                CENTRE,
                MOMENTUM,
                at(start, ms),
            )?);
            events.extend(scroller.poll(at(start, ms + 120)));
        }
        assert_eq!(finishes(&events), 0, "finish during momentum: {events:?}");
        let end = scroller.marker(Marker::MomentumEnd, at(start, 1500));
        assert_eq!(finishes(&end), 1);
        events.extend(end);
        let (_, dy, _) = smooth_total(&events);
        assert!((dy - (-15.0 * 6.0 - 2.5 * 10.0)).abs() < 1e-6, "{dy}");
        assert!(scroller.poll(at(start, 3000)).is_empty());
        Ok(())
    }

    #[test]
    fn mac_touch_stops_coasting_and_lift_without_momentum_finishes() -> Result<()> {
        let start = Instant::now();
        let mut scroller = Scroller::new(true, 1.0, Direction::Auto);
        scroller.marker(Marker::Touch, start);
        scroller.wheel((0, 0), (0.0, 2.5), false, CENTRE, TRACKPAD, start)?;
        scroller.marker(Marker::Lift, at(start, 20));
        scroller.marker(Marker::MomentumStart, at(start, 25));
        scroller.wheel((0, 0), (0.0, 1.5), false, CENTRE, MOMENTUM, at(start, 40))?;
        // Touching again ends the coasting at once.
        let events = scroller.marker(Marker::Touch, at(start, 60));
        assert_eq!(finishes(&events), 1);
        // A resting touch that never scrolls owes nothing.
        assert!(scroller.marker(Marker::Cancel, at(start, 80)).is_empty());
        // Lift without momentum: finished after the grace period.
        scroller.marker(Marker::Touch, at(start, 100));
        scroller.wheel((0, 0), (0.0, 0.5), false, CENTRE, TRACKPAD, at(start, 100))?;
        scroller.marker(Marker::Lift, at(start, 110));
        assert_eq!(scroller.deadline(), Some(at(start, 110) + LIFT_GRACE));
        assert!(scroller.poll(at(start, 150)).is_empty());
        assert_eq!(finishes(&scroller.poll(at(start, 160))), 1);
        // A mouse wheel during coasting ends the trackpad gesture first.
        scroller.marker(Marker::Touch, at(start, 200));
        scroller.wheel((0, 0), (0.0, 0.5), false, CENTRE, TRACKPAD, at(start, 200))?;
        let events = scroller.wheel((0, 1), (0.0, 1.0), false, CENTRE, WHEEL, at(start, 210))?;
        assert_eq!(finishes(&events), 1);
        assert_eq!(notch_total(&events), (0, 1));
        Ok(())
    }

    #[test]
    fn a_new_gesture_after_a_pause_finishes_the_old_one_first() -> Result<()> {
        let start = Instant::now();
        let mut scroller = Scroller::new(true, 1.0, Direction::Auto);
        scroller.wheel((0, 0), (0.0, 0.5), false, CENTRE, None, start)?;
        // A whole notch 150 ms later: the trackpad gesture finished, then a
        // wheel gesture (discrete) begins at a new position.
        let events = scroller.wheel(
            (0, 1),
            (0.0, 1.0),
            false,
            Target::Desktop(0.6, 0.5),
            None,
            at(start, 150),
        )?;
        assert!(matches!(
            events[0],
            Event::ScrollSmooth { finish: true, .. }
        ));
        assert!(matches!(events[1], Event::Motion { x, .. } if x == 0.6));
        assert!(matches!(events[2], Event::Scroll { x: 0, y: 1 }));
        // Integral deltas inside a guessed trackpad gesture stay smooth.
        let mut scroller = Scroller::new(true, 1.0, Direction::Auto);
        scroller.wheel((0, 0), (0.0, 0.5), false, CENTRE, None, start)?;
        let events = scroller.wheel((0, 1), (0.0, 1.0), false, CENTRE, None, at(start, 20))?;
        assert_eq!(smooth_total(&events).1, 10.0);
        Ok(())
    }

    #[test]
    fn trackpad_to_discrete_hosts_accumulates_whole_notches() -> Result<()> {
        let start = Instant::now();
        let mut scroller = Scroller::new(false, 1.0, Direction::Auto);
        let mut events = Vec::new();
        // 0.25 units = 2.5 px per event: a notch every 20 events.
        for ms in 0..40 {
            events.extend(scroller.wheel(
                (0, 0),
                (0.0, -0.25),
                false,
                CENTRE,
                None,
                at(start, ms),
            )?);
        }
        assert_eq!(notch_total(&events), (0, -2));
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, Event::Motion { .. }))
                .count(),
            1
        );
        // No smooth events or finish reach a discrete host, and a gesture's
        // leftover fraction is not carried into the next.
        assert!(scroller.poll(at(start, 500)).is_empty());
        let events = scroller.wheel((0, 0), (0.0, -2.4), false, CENTRE, None, at(start, 600))?;
        assert_eq!(notch_total(&events), (0, 0));
        Ok(())
    }

    #[test]
    fn leaving_the_desktop_and_pointer_lock() -> Result<()> {
        let start = Instant::now();
        let mut scroller = Scroller::new(true, 1.0, Direction::Auto);
        scroller.wheel((0, 0), (0.0, 0.5), false, CENTRE, None, start)?;
        // Over local UI: the open gesture finishes; nothing else is sent.
        let events = scroller.wheel(
            (0, 0),
            (0.0, 0.5),
            false,
            Target::Outside,
            None,
            at(start, 1),
        )?;
        assert_eq!(events.len(), 1);
        assert!(matches!(
            events[0],
            Event::ScrollSmooth { finish: true, .. }
        ));
        assert!(scroller.reset().is_empty());
        // Pointer lock: deltas without an absolute position.
        let events = scroller.wheel(
            (0, 1),
            (0.0, 1.0),
            false,
            Target::Locked,
            None,
            at(start, 2),
        )?;
        assert_eq!(events.len(), 1);
        assert!(matches!(events[0], Event::Scroll { x: 0, y: 1 }));
        // Wheel remainders are dropped outside the desktop.
        let mut scroller = Scroller::new(false, 1.0, Direction::Auto);
        scroller.wheel((0, 0), (0.0, 0.75), false, CENTRE, None, start)?;
        scroller.wheel((0, 0), (0.0, 0.0), false, Target::Outside, None, start)?;
        assert!(
            scroller
                .wheel((0, 0), (0.0, 0.25), false, CENTRE, None, at(start, 1))?
                .is_empty()
        );
        assert!(
            scroller
                .wheel((i32::MIN, 0), (0.0, 0.0), false, CENTRE, None, start)
                .is_err()
        );
        Ok(())
    }

    #[test]
    fn large_bursts_split_within_the_protocol_bound() {
        let start = Instant::now();
        let mut scroller = Scroller::new(true, 10.0, Direction::Auto);
        let events = scroller
            .wheel((0, 0), (0.5, 150.5), false, Target::Locked, None, start)
            .unwrap();
        let (dx, dy, _) = smooth_total(&events);
        assert!(events.len() > 1);
        assert!((dx - 50.0).abs() < 1e-6 && (dy - 15050.0).abs() < 1e-6);
    }

    #[test]
    fn host_notches_from_pixels() {
        let mut notches = Notches::default();
        assert_eq!(notches.add(0.0, 30.0, false), (0, 0));
        assert_eq!(notches.add(0.0, 30.0, false), (0, 1));
        assert_eq!(notches.add(-120.0, 0.0, false), (-2, 0));
        // Finish drops the remainder (10 px down, -20 px right).
        assert_eq!(notches.add(0.0, 0.0, true), (0, 0));
        assert_eq!(notches.add(0.0, 45.0, false), (0, 0));
    }
}
