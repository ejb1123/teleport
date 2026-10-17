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
    /// Hosts advertising `Desktop::av1` only.
    Av1,
}

impl CodecChoice {
    /// The codec named in the connection request. `auto` asks for H.264 so
    /// hosts without `Desktop::codec_selection` keep streaming what they
    /// always did; HDR10 is H.265 only. AV1 is only ever chosen through
    /// `SelectCodec` (older hosts reject the name), so it asks for H.264.
    pub fn requested(self, range: protocol::DynamicRange) -> protocol::VideoCodec {
        match self {
            Self::H265 => protocol::VideoCodec::H265,
            Self::Auto if range == protocol::DynamicRange::Hdr10 => protocol::VideoCodec::H265,
            Self::Auto | Self::H264 | Self::Av1 => protocol::VideoCodec::H264,
        }
    }
    /// The explicitly chosen codec.
    fn explicit(self) -> Option<protocol::VideoCodec> {
        match self {
            Self::Auto => None,
            Self::H264 => Some(protocol::VideoCodec::H264),
            Self::H265 => Some(protocol::VideoCodec::H265),
            Self::Av1 => Some(protocol::VideoCodec::Av1),
        }
    }
}

/// The client's `--chroma-444` (full-resolution chroma: sharper coloured
/// text, H.264/H.265) and `--ten-bit` (Main 10 from 8-bit sources: less
/// banding, H.265/AV1), for SDR sessions with hosts advertising
/// `Desktop::chroma_444_selection` / `ten_bit_selection`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, clap::ValueEnum)]
pub enum FormatChoice {
    /// 4:2:0 / 8-bit, as always.
    #[default]
    Off,
    /// When the host encodes and this client decodes it in hardware.
    Auto,
    /// Whenever the host can, in software if need be.
    On,
}

impl FormatChoice {
    pub fn label(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Auto => "auto",
            Self::On => "on",
        }
    }
}

/// This client's hardware decoders, probed before connecting, for the
/// automatic choices (`None`: software only, or not probed).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ClientDecoders {
    pub hevc: Option<String>,
    pub av1: Option<String>,
    /// H.265 4:4:4.
    pub hevc_444: Option<String>,
    /// H.265 and AV1 Main 10 (SDR).
    pub hevc_10: Option<String>,
    pub av1_10: Option<String>,
}

/// The session codec, chroma and depth and why, for logs and Stats (F8).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CodecDecision {
    pub codec: protocol::VideoCodec,
    pub chroma_444: bool,
    pub ten_bit: bool,
    pub reason: String,
}

/// Picks the session codec, chroma and depth once the host's `Desktop` is
/// known: the codec and chroma as [`choose_codec_and_chroma`] does, then
/// Main 10 (`ten_bit`) for H.265/AV1 SDR without 4:4:4. `auto` needs Main 10
/// in hardware on both ends; `on` also moves an automatic H.264 choice to
/// H.265.
pub fn choose_codec(
    choice: CodecChoice,
    chroma: FormatChoice,
    ten_bit: FormatChoice,
    range: protocol::DynamicRange,
    desktop: &Desktop,
    decoders: &ClientDecoders,
) -> CodecDecision {
    use protocol::VideoCodec::{Av1, H264, H265};
    let mut decision = choose_codec_and_chroma(choice, chroma, range, desktop, decoders);
    if range != protocol::DynamicRange::Sdr
        || !desktop.ten_bit_selection
        || decision.chroma_444
        || ten_bit == FormatChoice::Off
    {
        return decision;
    }
    let hardware = |codec| {
        desktop.hardware_ten_bit.contains(&codec)
            && match codec {
                H265 => decoders.hevc_10.is_some(),
                Av1 => decoders.av1_10.is_some(),
                H264 => false,
            }
    };
    decision.ten_bit = match ten_bit {
        FormatChoice::Auto => hardware(decision.codec),
        FormatChoice::On if decision.codec.supports_ten_bit() => true,
        FormatChoice::On if choice == CodecChoice::Auto => {
            decision.codec = H265;
            decision.reason = "auto: H.265 for 10-bit".into();
            true
        }
        _ => false,
    };
    if decision.ten_bit {
        decision.reason += &format!("; 10-bit ({})", ten_bit.label());
    }
    decision
}

/// Picks the session codec once the host's `Desktop` is known. `auto`
/// prefers AV1, then H.265, each only when both ends do it in hardware: they
/// need roughly half H.264's bitrate for the same quality, but a software
/// encode or decode costs more latency than the bitrate saves (SVT-AV1 more
/// than most). 4:4:4 (`chroma`) is H.264/H.265 only; with `auto` codec and
/// chroma it takes H.265 4:4:4 over AV1 when both ends have the hardware.
pub fn choose_codec_and_chroma(
    choice: CodecChoice,
    chroma: FormatChoice,
    range: protocol::DynamicRange,
    desktop: &Desktop,
    decoders: &ClientDecoders,
) -> CodecDecision {
    use protocol::VideoCodec::{Av1, H264, H265};
    let sdr = range == protocol::DynamicRange::Sdr;
    let can_444 = sdr && desktop.chroma_444_selection;
    // Only H.265 4:4:4 is decoded in hardware anywhere here (NVDEC has no
    // H.264 4:4:4), so `auto` never picks H.264 4:4:4.
    let hardware_444 = |codec| {
        codec == H265 && desktop.hardware_444.contains(&codec) && decoders.hevc_444.is_some()
    };
    let decision = |codec: protocol::VideoCodec, chroma_444: bool, reason: String| CodecDecision {
        codec,
        chroma_444,
        ten_bit: false,
        reason: if chroma_444 {
            format!("{reason}; 4:4:4 ({})", chroma.label())
        } else {
            reason
        },
    };
    if let Some(codec) = choice.explicit() {
        let chroma_444 = can_444
            && codec.supports_444()
            && match chroma {
                FormatChoice::Off => false,
                FormatChoice::Auto => hardware_444(codec),
                FormatChoice::On => true,
            };
        return decision(codec, chroma_444, "chosen with --codec".into());
    }
    if !sdr {
        return decision(H265, false, "auto: HDR10 needs H.265".into());
    }
    if !desktop.codec_selection {
        return decision(H264, false, "auto: older host keeps H.264".into());
    }
    if can_444 && chroma != FormatChoice::Off && hardware_444(H265) {
        return decision(
            H265,
            true,
            format!(
                "auto: 4:4:4 hardware on both ends ({} decoder)",
                decoders.hevc_444.as_deref().unwrap_or("software")
            ),
        );
    }
    let (codec, reason) = match (&decoders.av1, &decoders.hevc) {
        (Some(decoder), _) if desktop.av1 && desktop.hardware_av1 => (
            Av1,
            format!("auto: AV1 hardware on both ends ({decoder} decoder)"),
        ),
        (_, Some(decoder)) if desktop.hardware_codecs.contains(&H265) => (
            H265,
            format!("auto: hardware on both ends ({decoder} decoder)"),
        ),
        (_, Some(_)) => (H264, "auto: no hardware H.265 encoder on host".into()),
        (_, None) => (H264, "auto: no hardware H.265 decoder here".into()),
    };
    if can_444 && chroma == FormatChoice::On {
        // 4:4:4 wanted even in software: the best 4:4:4-capable codec.
        let codec = if codec == Av1 || desktop.hardware_444.contains(&H265) {
            H265
        } else {
            codec
        };
        return decision(codec, true, format!("auto: {}", codec.label()));
    }
    decision(codec, false, reason)
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

    fn decoders(hevc: Option<&str>) -> ClientDecoders {
        ClientDecoders {
            hevc: hevc.map(Into::into),
            ..Default::default()
        }
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
            choose_codec(
                choice,
                FormatChoice::Off,
                FormatChoice::Off,
                range,
                desktop,
                &decoders(decoder),
            )
            .codec
        };
        // Older hosts keep H.264 unless asked; the request says H.264 too.
        assert_eq!(CodecChoice::Auto.requested(Sdr), H264);
        assert_eq!(pick(CodecChoice::Auto, Sdr, &old, Some("cuda")), H264);
        // Both ends in hardware: H.265, and the reason names the decoder.
        let both = choose_codec(
            CodecChoice::Auto,
            FormatChoice::Off,
            FormatChoice::Off,
            Sdr,
            &hevc_host,
            &decoders(Some("vaapi")),
        );
        assert_eq!(both.codec, H265);
        assert!(!both.chroma_444);
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

    #[test]
    fn auto_codec_prefers_av1_only_with_hardware_on_both_ends() {
        use protocol::{DynamicRange::*, VideoCodec::*};
        let mut host = desktop(true);
        host.codec_selection = true;
        host.hardware_codecs = vec![H264, H265];
        host.av1 = true;
        let both = ClientDecoders {
            hevc: Some("cuda".into()),
            av1: Some("cuda".into()),
            ..Default::default()
        };
        let pick = |choice, desktop: &Desktop, decoders: &ClientDecoders| {
            choose_codec(
                choice,
                FormatChoice::Off,
                FormatChoice::Off,
                Sdr,
                desktop,
                decoders,
            )
            .codec
        };
        // SVT-AV1 only on the host: H.265 in hardware instead.
        assert_eq!(pick(CodecChoice::Auto, &host, &both), H265);
        let mut av1_host = host.clone();
        av1_host.hardware_av1 = true;
        let chosen = choose_codec(
            CodecChoice::Auto,
            FormatChoice::Off,
            FormatChoice::Off,
            Sdr,
            &av1_host,
            &both,
        );
        assert_eq!(chosen.codec, Av1);
        assert!(chosen.reason.contains("AV1"), "{}", chosen.reason);
        // No AV1 decoder here, or an older host: never AV1.
        assert_eq!(
            pick(CodecChoice::Auto, &av1_host, &decoders(Some("cuda"))),
            H265
        );
        let mut old = av1_host.clone();
        old.av1 = false;
        old.hardware_av1 = false;
        assert_eq!(pick(CodecChoice::Auto, &old, &both), H265);
        // Explicit AV1 is used as is (the client refuses hosts without it)
        // and connects asking for H.264, which older hosts understand.
        assert_eq!(
            pick(CodecChoice::Av1, &host, &ClientDecoders::default()),
            Av1
        );
        assert_eq!(CodecChoice::Av1.requested(Sdr), H264);
    }

    #[test]
    fn chroma_444_is_off_by_default_and_gated_on_the_host() {
        use protocol::{DynamicRange::*, VideoCodec::*};
        let mut host = desktop(true);
        host.codec_selection = true;
        host.hardware_codecs = vec![H264, H265];
        host.av1 = true;
        host.hardware_av1 = true;
        let client = ClientDecoders {
            hevc: Some("cuda".into()),
            av1: Some("cuda".into()),
            hevc_444: Some("cuda".into()),
            ..Default::default()
        };
        let pick = |codec, chroma, desktop: &Desktop, decoders: &ClientDecoders| {
            let decision = choose_codec(codec, chroma, FormatChoice::Off, Sdr, desktop, decoders);
            (decision.codec, decision.chroma_444)
        };
        // Older hosts: never 4:4:4, whatever was asked.
        for chroma in [FormatChoice::Off, FormatChoice::Auto, FormatChoice::On] {
            assert!(!pick(CodecChoice::Auto, chroma, &host, &client).1);
        }
        host.chroma_444_selection = true;
        assert_eq!(
            pick(CodecChoice::Auto, FormatChoice::Off, &host, &client),
            (Av1, false)
        );
        // Auto needs hardware 4:4:4 on both ends, then prefers it to AV1.
        assert_eq!(
            pick(CodecChoice::Auto, FormatChoice::Auto, &host, &client),
            (Av1, false)
        );
        host.hardware_444 = vec![H264, H265];
        let chosen = choose_codec(
            CodecChoice::Auto,
            FormatChoice::Auto,
            FormatChoice::Off,
            Sdr,
            &host,
            &client,
        );
        assert_eq!((chosen.codec, chosen.chroma_444), (H265, true));
        assert!(chosen.reason.contains("4:4:4"), "{}", chosen.reason);
        let no_444_decoder = ClientDecoders {
            hevc_444: None,
            ..client.clone()
        };
        assert_eq!(
            pick(
                CodecChoice::Auto,
                FormatChoice::Auto,
                &host,
                &no_444_decoder
            ),
            (Av1, false)
        );
        // On: 4:4:4 even in software, never on AV1 or HDR.
        assert_eq!(
            pick(CodecChoice::Auto, FormatChoice::On, &host, &no_444_decoder),
            (H265, true)
        );
        assert_eq!(
            pick(CodecChoice::H264, FormatChoice::On, &host, &no_444_decoder),
            (H264, true)
        );
        assert_eq!(
            pick(CodecChoice::Av1, FormatChoice::On, &host, &client),
            (Av1, false)
        );
        assert!(
            !choose_codec(
                CodecChoice::H265,
                FormatChoice::On,
                FormatChoice::Off,
                Hdr10,
                &host,
                &client
            )
            .chroma_444
        );
    }

    #[test]
    fn ten_bit_is_off_by_default_and_gated_on_the_host() {
        use protocol::{DynamicRange::*, VideoCodec::*};
        let mut host = desktop(true);
        host.codec_selection = true;
        host.hardware_codecs = vec![H264, H265];
        host.chroma_444_selection = true;
        let client = ClientDecoders {
            hevc: Some("cuda".into()),
            hevc_10: Some("cuda".into()),
            ..Default::default()
        };
        let pick = |codec, ten_bit, chroma, desktop: &Desktop| {
            let decision = choose_codec(codec, chroma, ten_bit, Sdr, desktop, &client);
            (decision.codec, decision.ten_bit, decision.reason)
        };
        // Older hosts never.
        assert!(
            !pick(
                CodecChoice::Auto,
                FormatChoice::On,
                FormatChoice::Off,
                &host
            )
            .1
        );
        host.ten_bit_selection = true;
        assert!(
            !pick(
                CodecChoice::Auto,
                FormatChoice::Off,
                FormatChoice::Off,
                &host
            )
            .1
        );
        // Auto: hardware Main 10 on both ends.
        assert!(
            !pick(
                CodecChoice::Auto,
                FormatChoice::Auto,
                FormatChoice::Off,
                &host
            )
            .1
        );
        host.hardware_ten_bit = vec![H265];
        let (codec, ten_bit, reason) = pick(
            CodecChoice::Auto,
            FormatChoice::Auto,
            FormatChoice::Off,
            &host,
        );
        assert_eq!((codec, ten_bit), (H265, true));
        assert!(reason.contains("10-bit (auto)"), "{reason}");
        // On: H.264 moves to H.265 unless chosen explicitly; never with 4:4:4.
        let mut software = host.clone();
        software.hardware_codecs = vec![H264];
        software.hardware_ten_bit.clear();
        let (codec, ten_bit, _) = pick(
            CodecChoice::Auto,
            FormatChoice::On,
            FormatChoice::Off,
            &software,
        );
        assert_eq!((codec, ten_bit), (H265, true));
        let (codec, ten_bit, _) = pick(
            CodecChoice::H264,
            FormatChoice::On,
            FormatChoice::Off,
            &software,
        );
        assert_eq!((codec, ten_bit), (H264, false));
        let (codec, ten_bit, _) =
            pick(CodecChoice::H265, FormatChoice::On, FormatChoice::On, &host);
        assert_eq!((codec, ten_bit), (H265, false));
        assert!(
            !choose_codec(
                CodecChoice::H265,
                FormatChoice::Off,
                FormatChoice::On,
                Hdr10,
                &host,
                &client
            )
            .ten_bit
        );
    }
}
