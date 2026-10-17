//! Stream size and rate on the client side: what the user chose, what this
//! window's display is, and when the host needs to hear about a change.
//!
//! Hosts advertising `Desktop::display_negotiation` pick the stream from an
//! `Event::ClientDisplay` themselves (they know their monitor's refresh
//! rate). Older hosts get the same choice computed here as `ConfigureVideo`,
//! clamped to what they accept.
use crate::protocol::{self, ClientDisplay, Desktop, Event};
use std::time::{Duration, Instant};

/// A user's stream choice: `Match` follows the client (`match`, `auto`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Setting {
    Match,
    Value(u32),
}

fn parse(
    value: &str,
    keyword: &str,
    valid: impl Fn(u32) -> bool,
    range: &str,
) -> Result<Setting, String> {
    if value.eq_ignore_ascii_case(keyword) {
        return Ok(Setting::Match);
    }
    match value.parse() {
        Ok(number) if valid(number) => Ok(Setting::Value(number)),
        _ => Err(format!("expected `{keyword}` or {range}")),
    }
}

pub fn parse_width(value: &str) -> Result<Setting, String> {
    parse(
        value,
        "match",
        |width| width == 0 || (320..=7680).contains(&width),
        "0 (native) or 320-7680",
    )
}

pub fn parse_fps(value: &str) -> Result<Setting, String> {
    parse(
        value,
        "match",
        |fps| (1..=protocol::MAX_FPS).contains(&fps),
        "1-240",
    )
}

pub fn parse_bitrate(value: &str) -> Result<Setting, String> {
    parse(
        value,
        "auto",
        |kbps| (500..=100_000).contains(&kbps),
        "500-100000",
    )
}

/// The client's `--codec`: `auto` picks per host (see [`choose_codec`]).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, clap::ValueEnum)]
pub enum CodecChoice {
    #[default]
    Auto,
    H264,
    H265,
}

impl CodecChoice {
    /// The codec named in the connection request. `auto` asks for H.264 so
    /// hosts without `Desktop::codec_selection` keep streaming what they
    /// always did; HDR10 is H.265 only.
    pub fn requested(self, range: protocol::DynamicRange) -> protocol::VideoCodec {
        match self {
            Self::H265 => protocol::VideoCodec::H265,
            Self::Auto if range == protocol::DynamicRange::Hdr10 => protocol::VideoCodec::H265,
            Self::Auto | Self::H264 => protocol::VideoCodec::H264,
        }
    }
}

/// The session codec and why, for logs and Stats (F8).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CodecDecision {
    pub codec: protocol::VideoCodec,
    pub reason: String,
}

/// Picks the session codec once the host's `Desktop` is known.
/// `client_hevc` names this client's hardware H.265 decoder, if any.
/// `auto` prefers H.265 only when both ends do it in hardware: it needs
/// roughly half H.264's bitrate for the same quality, but a software encode
/// or decode costs more latency than the bitrate saves.
pub fn choose_codec(
    choice: CodecChoice,
    range: protocol::DynamicRange,
    desktop: &Desktop,
    client_hevc: Option<&str>,
) -> CodecDecision {
    use protocol::VideoCodec::{H264, H265};
    let decision = |codec, reason: &str| CodecDecision {
        codec,
        reason: reason.into(),
    };
    let requested = choice.requested(range);
    if choice != CodecChoice::Auto {
        return decision(requested, "chosen with --codec");
    }
    if desktop.shared_stream {
        // Another session already chose it; the host encodes once.
        return decision(desktop.codec, "auto: shared with another session");
    }
    if range == protocol::DynamicRange::Hdr10 {
        return decision(H265, "auto: HDR10 needs H.265");
    }
    if !desktop.codec_selection {
        return decision(requested, "auto: older host keeps H.264");
    }
    if !desktop.hardware_codecs.contains(&H265) {
        return decision(H264, "auto: no hardware H.265 encoder on host");
    }
    match client_hevc {
        Some(decoder) => CodecDecision {
            codec: H265,
            reason: format!("auto: hardware on both ends ({decoder} decoder)"),
        },
        None => decision(H264, "auto: no hardware H.265 decoder here"),
    }
}

/// What the client shows video on.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct LocalDisplay {
    /// Window drawable in physical pixels (HiDPI-aware); 0 when unknown.
    pub width: u32,
    pub height: u32,
    /// Refresh rate of the window's display in millihertz; 0 when unknown.
    pub refresh_mhz: u32,
}

/// `1280x720@60000` (size in pixels, refresh in millihertz).
pub fn parse_display(value: &str) -> Result<LocalDisplay, String> {
    let invalid = || "expected WIDTHxHEIGHT@MILLIHERTZ, e.g. 1280x720@60000".to_owned();
    let (size, refresh) = value.split_once('@').ok_or_else(invalid)?;
    let (width, height) = size.split_once('x').ok_or_else(invalid)?;
    let display = LocalDisplay {
        width: width.parse().map_err(|_| invalid())?,
        height: height.parse().map_err(|_| invalid())?,
        refresh_mhz: refresh.parse().map_err(|_| invalid())?,
    };
    ClientDisplay {
        width: display.width,
        height: display.height,
        refresh_mhz: display.refresh_mhz,
        ..Default::default()
    }
    .validate()
    .map_err(|error| error.to_string())?;
    Ok(display)
}

impl LocalDisplay {
    /// The window's drawable size and its current display's refresh rate.
    pub fn of_window(canvas: &sdl2::render::Canvas<sdl2::video::Window>) -> Self {
        let (width, height) = canvas.output_size().unwrap_or_default();
        let window = canvas.window();
        let refresh_mhz = window
            .display_index()
            .and_then(|index| window.subsystem().current_display_mode(index))
            .map_or(0, |mode| mode.refresh_rate.clamp(0, 1000) as u32 * 1000);
        Self {
            width,
            height,
            refresh_mhz,
        }
    }
}

/// How long a new window size or display must hold before the stream
/// follows it (a window drag or a mode switch passes through many states).
const SETTLE: Duration = Duration::from_millis(1000);
/// Smallest stream-width change worth an encoder restart (a fullscreen
/// toggle or a move to a different display always qualifies).
const MIN_WIDTH_CHANGE: f64 = 0.10;

/// The user's choices and the display last reported to the host.
pub struct Negotiation {
    request: ClientDisplay,
    candidate: Option<(LocalDisplay, Instant)>,
}

/// What to send for a display, and what to tell the user.
pub struct Outgoing {
    pub event: Event,
    /// The `(width, fps, bitrate)` an older host must acknowledge; `None`
    /// when the host negotiates (any acknowledgement counts).
    pub requested: Option<(u32, u32, u32)>,
    /// Set when the host cannot stream as fast as asked.
    pub notice: Option<String>,
}

impl Negotiation {
    /// `None` when the user chose nothing: the host keeps its own settings.
    /// Unset choices keep the host's current values, as they always have.
    pub fn new(
        width: Option<Setting>,
        fps: Option<Setting>,
        bitrate: Option<Setting>,
        desktop: &Desktop,
    ) -> Option<Self> {
        if width.is_none() && fps.is_none() && bitrate.is_none() {
            return None;
        }
        let choose = |setting: Option<Setting>, current: u32| match setting {
            None => Some(current),
            Some(Setting::Match) => None,
            Some(Setting::Value(value)) => Some(value),
        };
        let current_bitrate = if desktop.bitrate > 0 {
            desktop.bitrate
        } else {
            protocol::AUTO_BITRATE
        };
        Some(Self {
            request: ClientDisplay {
                stream_width: choose(width, desktop.width),
                max_fps: choose(fps, desktop.fps.clamp(1, protocol::MAX_FPS)),
                bitrate: choose(bitrate, current_bitrate),
                ..Default::default()
            },
            candidate: None,
        })
    }

    fn request(&self, display: LocalDisplay) -> ClientDisplay {
        ClientDisplay {
            width: display.width,
            height: display.height,
            refresh_mhz: display.refresh_mhz,
            ..self.request
        }
    }

    /// The host's active monitor size and refresh rate, as far as known.
    fn monitor(desktop: &Desktop) -> ((u32, u32), u32) {
        let size = if desktop.native_width > 0 && desktop.native_height > 0 {
            (desktop.native_width, desktop.native_height)
        } else {
            (desktop.width, desktop.height)
        };
        let refresh = desktop
            .monitors
            .get(desktop.active_monitor)
            .map_or(0, |monitor| monitor.refresh_mhz);
        (size, refresh)
    }

    /// The stream this display gets from this host (what the host will pick).
    pub fn choice(&self, desktop: &Desktop, display: LocalDisplay) -> protocol::StreamChoice {
        let (monitor, refresh) = Self::monitor(desktop);
        protocol::negotiate(
            &self.request(display),
            monitor,
            refresh,
            desktop.accepted_max_fps(),
        )
    }

    /// The event that applies `display` on this host.
    pub fn outgoing(&self, desktop: &Desktop, display: LocalDisplay) -> Outgoing {
        let (monitor, refresh) = Self::monitor(desktop);
        let choice = self.choice(desktop, display);
        let wanted =
            protocol::negotiate(&self.request(display), monitor, refresh, protocol::MAX_FPS).fps;
        let notice = (wanted > choice.fps).then(|| {
            format!(
                "This host streams at most {} fps; update it for {wanted} fps",
                choice.fps
            )
        });
        if desktop.display_negotiation {
            Outgoing {
                event: Event::ClientDisplay(self.request(display)),
                requested: None,
                notice,
            }
        } else {
            Outgoing {
                event: Event::ConfigureVideo {
                    width: choice.width,
                    fps: choice.fps,
                    bitrate: choice.bitrate,
                },
                requested: Some((choice.width, choice.fps, choice.bitrate)),
                notice,
            }
        }
    }

    /// Record `display` as the one the host now knows.
    pub fn sent(&mut self, display: LocalDisplay) {
        self.request.width = display.width;
        self.request.height = display.height;
        self.request.refresh_mhz = display.refresh_mhz;
        self.candidate = None;
    }

    /// Feed the window's current display (cheap to call often). Returns it
    /// once it differs from what the host knows in a way that changes the
    /// stream, and has held for [`SETTLE`].
    pub fn poll(
        &mut self,
        desktop: &Desktop,
        display: LocalDisplay,
        now: Instant,
    ) -> Option<LocalDisplay> {
        let known = LocalDisplay {
            width: self.request.width,
            height: self.request.height,
            refresh_mhz: self.request.refresh_mhz,
        };
        if !self.changes_stream(desktop, known, display) {
            self.candidate = None;
            return None;
        }
        match self.candidate {
            Some((candidate, since)) if candidate == display => {
                if now.duration_since(since) >= SETTLE {
                    self.candidate = None;
                    return Some(display);
                }
            }
            _ => self.candidate = Some((display, now)),
        }
        None
    }

    fn changes_stream(&self, desktop: &Desktop, from: LocalDisplay, to: LocalDisplay) -> bool {
        let (before, after) = (self.choice(desktop, from), self.choice(desktop, to));
        if before.fps != after.fps {
            return true;
        }
        if before.width == after.width {
            return false;
        }
        let native = Self::monitor(desktop).0.0;
        let width = |width: u32| if width == 0 { native } else { width };
        let (before, after) = (width(before.width), width(after.width));
        after == native || f64::from(before.abs_diff(after)) >= MIN_WIDTH_CHANGE * f64::from(before)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn desktop(negotiates: bool) -> Desktop {
        serde_json::from_value(serde_json::json!({ "version": 2,
            "width": 1280, "height": 720, "fps": 60, "bitrate": 8000,
            "native_width": 2560, "native_height": 1440, "source": "portal",
            "monitors": [{ "id": 0, "name": "A", "width": 2560, "height": 1440 }],
            "active_monitor": 0, "audio": false, "clipboard": false,
            "configurable_video": true,
            "max_fps": if negotiates { 240 } else { 0 },
            "display_negotiation": negotiates }))
        .unwrap()
    }

    const QHD_180: LocalDisplay = LocalDisplay {
        width: 2560,
        height: 1440,
        refresh_mhz: 180_000,
    };

    #[test]
    fn settings_parse() {
        assert_eq!(parse_fps("match"), Ok(Setting::Match));
        assert_eq!(parse_fps("180"), Ok(Setting::Value(180)));
        assert!(parse_fps("241").is_err() && parse_fps("0").is_err());
        assert_eq!(parse_width("MATCH"), Ok(Setting::Match));
        assert_eq!(parse_width("0"), Ok(Setting::Value(0)));
        assert!(parse_width("100").is_err());
        assert_eq!(parse_bitrate("auto"), Ok(Setting::Match));
        assert!(parse_bitrate("match").is_err());
        assert_eq!(
            parse_display("1280x720@60000"),
            Ok(LocalDisplay {
                width: 1280,
                height: 720,
                refresh_mhz: 60_000
            })
        );
        assert!(parse_display("1280x720").is_err());
    }

    #[test]
    fn nothing_chosen_leaves_the_host_alone() {
        assert!(Negotiation::new(None, None, None, &desktop(true)).is_none());
    }

    #[test]
    fn negotiating_hosts_get_the_display_and_old_hosts_a_clamped_configuration() {
        let negotiation = Negotiation::new(
            Some(Setting::Match),
            Some(Setting::Match),
            Some(Setting::Match),
            &desktop(true),
        )
        .unwrap();
        let new = negotiation.outgoing(&desktop(true), QHD_180);
        assert!(matches!(
            new.event,
            Event::ClientDisplay(ClientDisplay {
                width: 2560,
                refresh_mhz: 180_000,
                stream_width: None,
                max_fps: None,
                bitrate: None,
                ..
            })
        ));
        assert!(new.requested.is_none() && new.notice.is_none());
        // An old host: 120 at most, said so, at native size and auto bitrate.
        let old = negotiation.outgoing(&desktop(false), QHD_180);
        let bitrate = protocol::auto_bitrate(2560, 1440, 120);
        assert!(matches!(
            old.event,
            Event::ConfigureVideo { width: 0, fps: 120, bitrate: b } if b == bitrate
        ));
        assert_eq!(old.requested, Some((0, 120, bitrate)));
        assert!(old.notice.unwrap().contains("at most 120 fps"));
        // Unset choices keep the host's current values, exactly as before.
        let legacy = Negotiation::new(Some(Setting::Value(1920)), None, None, &desktop(false))
            .unwrap()
            .outgoing(&desktop(false), LocalDisplay::default());
        assert_eq!(legacy.requested, Some((1920, 60, 8000)));
        assert!(legacy.notice.is_none());
        // An explicit rate is a cap: a 60 Hz window gets 60.
        let capped = Negotiation::new(None, Some(Setting::Value(120)), None, &desktop(false))
            .unwrap()
            .outgoing(
                &desktop(false),
                LocalDisplay {
                    refresh_mhz: 59_940,
                    ..QHD_180
                },
            );
        assert_eq!(capped.requested, Some((1280, 60, 8000)));
    }

    #[test]
    fn renegotiates_only_settled_changes_that_alter_the_stream() {
        let host = desktop(true);
        let mut negotiation =
            Negotiation::new(Some(Setting::Match), Some(Setting::Match), None, &host).unwrap();
        let window = LocalDisplay {
            width: 1280,
            height: 720,
            refresh_mhz: 180_000,
        };
        negotiation.sent(window);
        let start = Instant::now();
        let at = |ms| start + Duration::from_millis(ms);
        // Unchanged, and a resize too small to matter: nothing to send.
        assert_eq!(negotiation.poll(&host, window, at(0)), None);
        let nudged = LocalDisplay {
            width: 1300,
            ..window
        };
        assert_eq!(negotiation.poll(&host, nudged, at(0)), None);
        assert_eq!(negotiation.poll(&host, nudged, at(5000)), None);
        // Fullscreen: sent once it has held for a second, not while dragging.
        assert_eq!(negotiation.poll(&host, QHD_180, at(100)), None);
        let dragging = LocalDisplay {
            width: 2000,
            height: 1200,
            ..window
        };
        assert_eq!(negotiation.poll(&host, dragging, at(600)), None);
        assert_eq!(negotiation.poll(&host, QHD_180, at(700)), None);
        assert_eq!(negotiation.poll(&host, QHD_180, at(1600)), None);
        assert_eq!(negotiation.poll(&host, QHD_180, at(1700)), Some(QHD_180));
        negotiation.sent(QHD_180);
        assert_eq!(negotiation.poll(&host, QHD_180, at(5000)), None);
        // Moving to a 60 Hz display changes the rate.
        let slow = LocalDisplay {
            refresh_mhz: 60_000,
            ..QHD_180
        };
        assert_eq!(negotiation.poll(&host, slow, at(6000)), None);
        assert_eq!(negotiation.poll(&host, slow, at(7000)), Some(slow));
        // With explicit size and rate, the window does not matter.
        let mut fixed = Negotiation::new(
            Some(Setting::Value(1920)),
            Some(Setting::Value(60)),
            None,
            &host,
        )
        .unwrap();
        fixed.sent(window);
        assert_eq!(fixed.poll(&host, QHD_180, at(0)), None);
        assert_eq!(fixed.poll(&host, QHD_180, at(5000)), None);
    }

    #[test]
    fn auto_codec_prefers_h265_only_with_hardware_on_both_ends() {
        use protocol::{DynamicRange::*, VideoCodec::*};
        let old = desktop(true);
        let mut new = desktop(true);
        new.codec_selection = true;
        let mut hevc_host = new.clone();
        hevc_host.hardware_codecs = vec![H264, H265];
        let pick = |choice, range, desktop: &Desktop, decoder| {
            choose_codec(choice, range, desktop, decoder).codec
        };
        // Older hosts keep H.264 unless asked; the request says H.264 too.
        assert_eq!(CodecChoice::Auto.requested(Sdr), H264);
        assert_eq!(pick(CodecChoice::Auto, Sdr, &old, Some("cuda")), H264);
        // Both ends in hardware: H.265, and the reason names the decoder.
        let both = choose_codec(CodecChoice::Auto, Sdr, &hevc_host, Some("vaapi"));
        assert_eq!(both.codec, H265);
        assert!(both.reason.contains("vaapi"), "{}", both.reason);
        // Either end in software: H.264.
        assert_eq!(pick(CodecChoice::Auto, Sdr, &new, Some("cuda")), H264);
        assert_eq!(pick(CodecChoice::Auto, Sdr, &hevc_host, None), H264);
        // An explicit choice always wins, in both directions.
        assert_eq!(pick(CodecChoice::H264, Sdr, &hevc_host, Some("cuda")), H264);
        assert_eq!(pick(CodecChoice::H265, Sdr, &old, None), H265);
        // HDR10 is H.265 only, whatever the hardware.
        assert_eq!(CodecChoice::Auto.requested(Hdr10), H265);
        assert_eq!(pick(CodecChoice::Auto, Hdr10, &old, None), H265);
    }
}
