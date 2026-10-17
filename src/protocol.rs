use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use std::{path::Path, time::Duration};

pub const VERSION: u32 = 2;
pub const WIRE_VERSION: &str = "moq-lite-05";
/// Versions offered to and accepted by relays. Provider relays (Cloudflare)
/// speak only IETF MoQ Transport; moq-net adapts. IETF sessions carry no
/// datagrams, so pointer motion then uses the ordered input track.
pub const RELAY_VERSIONS: [&str; 3] = [WIRE_VERSION, "moq-transport-16", "moq-transport-14"];
/// moq-native defaults (30 s idle, 5 s keep-alive) leave a dead peer undetected
/// for half a minute. A 1 s keep-alive holds a healthy idle session open while
/// an unreachable peer is dropped after 8 s, so reconnect can start.
pub const QUIC_IDLE_TIMEOUT: Duration = Duration::from_secs(8);
pub const QUIC_KEEP_ALIVE: Duration = Duration::from_secs(1);
/// Path MTU discovery (off by default) lifts packets above the 1200-byte QUIC
/// floor, only after a probe of that size is acknowledged, and never past
/// quinn's 1452-byte upper bound. Overlays with smaller MTUs (Tailscale 1280,
/// WireGuard ~1420, VXLAN ~1450) just stay at what they can carry; black-hole
/// detection drops back to 1200 if a path shrinks mid-session (e.g. a VPN).
pub const QUIC_MTU_DISCOVERY: bool = true;
#[derive(Serialize, Deserialize, Debug, Clone, Copy, Default, PartialEq, Eq, clap::ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum VideoCodec {
    #[default]
    H264,
    H265,
}

/// Encoder speed/quality trade-off (parsec-comparison G10). None adds frame
/// delay; slower presets spend more encode time per frame on quality.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, Default, PartialEq, Eq, clap::ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum EncoderPreset {
    /// Fastest encode (NVENC p1/ull, x264 ultrafast): the historical setting.
    #[default]
    Latency,
    Balanced,
    Quality,
}

impl EncoderPreset {
    pub fn label(self) -> &'static str {
        match self {
            Self::Latency => "lowest latency",
            Self::Balanced => "balanced",
            Self::Quality => "quality",
        }
    }

    /// `tp_codec_open`'s preset number.
    pub fn level(self) -> i32 {
        match self {
            Self::Latency => 0,
            Self::Balanced => 1,
            Self::Quality => 2,
        }
    }
}

/// HDR10 here specifies PQ/BT.2020 limited-range 10-bit video, not mastering metadata.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, Default, PartialEq, Eq, clap::ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum DynamicRange {
    #[default]
    Sdr,
    Hdr10,
}

impl DynamicRange {
    pub fn label(self) -> &'static str {
        match self {
            Self::Sdr => "SDR",
            Self::Hdr10 => "HDR10 · PQ/BT.2020",
        }
    }
}

impl VideoCodec {
    pub fn track(self) -> &'static str {
        match self {
            Self::H264 => "h264",
            Self::H265 => "h265",
        }
    }
    pub fn label(self) -> &'static str {
        match self {
            Self::H264 => "H.264",
            Self::H265 => "H.265",
        }
    }
}
pub const MAX_FRAME: usize = 8 * 1024 * 1024;
pub const INPUT_TIMEOUT: Duration = Duration::from_secs(3);
pub const MAX_TEXT: usize = 64 * 1024;
pub const MAX_CONTROL: usize = MAX_TEXT * 6 + 1024;

#[derive(Serialize, Deserialize, Clone)]
pub struct Pairing {
    pub token: String,
    pub fingerprint: String,
    /// Hex Ed25519 seed of this device's relay key (`crate::e2e::DeviceKey`),
    /// present when the host registered its public half at enrollment. Older
    /// pairing files have none and keep working as hash-authenticated
    /// credentials.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device_key: Option<String>,
}

impl Pairing {
    pub fn read(path: &Path) -> Result<Self> {
        let pairing: Self = serde_json::from_slice(&std::fs::read(path)?)?;
        pairing.validate()?;
        Ok(pairing)
    }

    /// Wipe the secrets held in memory (token and device key seed).
    pub fn zeroize_secrets(&mut self) {
        use zeroize::Zeroize;
        self.token.zeroize();
        if let Some(key) = &mut self.device_key {
            key.zeroize();
        }
    }

    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.token.len() == 64 && self.token.bytes().all(|b| b.is_ascii_hexdigit()),
            "invalid pairing token"
        );
        moq_native::tls::parse_fingerprint(&self.fingerprint)?;
        // Any 32-byte seed is a valid Ed25519 key (`crate::e2e::DeviceKey`).
        ensure!(
            self.device_key
                .as_ref()
                .is_none_or(|key| key.len() == 64 && key.bytes().all(|b| b.is_ascii_hexdigit())),
            "invalid device key"
        );
        Ok(())
    }
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct Desktop {
    /// Independent monitor tracks within this authenticated session.
    #[serde(default)]
    pub multimonitor: bool,
    /// SSH forwarding is separately negotiated; older hosts default to unsupported.
    #[serde(default)]
    pub ssh_agent: bool,
    #[serde(default)]
    pub dynamic_range: DynamicRange,
    #[serde(default)]
    pub codec: VideoCodec,
    #[serde(default)]
    pub codecs: Vec<VideoCodec>,
    pub version: u32,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub source: String,
    pub monitors: Vec<Monitor>,
    pub active_monitor: usize,
    pub audio: bool,
    pub clipboard: bool,
    #[serde(default)]
    pub telemetry: bool,
    #[serde(default)]
    pub configurable_video: bool,
    #[serde(default)]
    pub native_width: u32,
    #[serde(default)]
    pub native_height: u32,
    #[serde(default)]
    pub bitrate: u32,
    /// First video group belonging to these settings; zero for older hosts.
    #[serde(default)]
    pub video_start_group: u64,
    /// Host accepts `Event::RequestKeyframe`; older hosts reject unknown events.
    #[serde(default)]
    pub keyframe_requests: bool,
    /// Host reads pointer datagrams once the client declares them.
    #[serde(default)]
    pub pointer_datagrams: bool,
    /// Host injects `Event::RelativeMotion` (pointer lock for games).
    #[serde(default)]
    pub relative_pointer: bool,
    /// Host can leave the pointer out of the video and send its shape as
    /// `Update::Cursor` once the client declares `ClientFeatures::local_cursor`.
    #[serde(default)]
    pub local_cursor: bool,
    /// Host accepts the `Gamepad*` events and backs them with virtual pads.
    /// Older hosts reject unknown events, so clients must check this first.
    #[serde(default)]
    pub gamepads: bool,
    /// Highest frame rate `ConfigureVideo` accepts; zero for hosts that
    /// predate the field, which reject anything above [`LEGACY_MAX_FPS`].
    #[serde(default)]
    pub max_fps: u32,
    /// Host accepts `Event::ClientDisplay` and picks the stream size and rate
    /// from it ([`negotiate`]); older hosts reject unknown events.
    #[serde(default)]
    pub display_negotiation: bool,
    /// Host waits for `Event::SelectCodec` as the client's first input event
    /// before it starts video, and encodes the codec chosen there. Older
    /// hosts reject unknown events, so clients must check this first.
    #[serde(default)]
    pub codec_selection: bool,
    /// Codecs the host encodes in hardware (its encoder probe), so a client
    /// choosing automatically prefers H.265 only when it is cheap to encode.
    /// Empty for older and software-only hosts.
    #[serde(default)]
    pub hardware_codecs: Vec<VideoCodec>,
    /// Host injects the back and forward mouse buttons
    /// ([`BUTTON_BACK`], [`BUTTON_FORWARD`]); older hosts reject them.
    #[serde(default)]
    pub extra_mouse_buttons: bool,
    /// Host applies `SelectCodec::encoder_preset`.
    #[serde(default)]
    pub encoder_presets: bool,
    /// The host's preset for clients that do not choose one.
    #[serde(default)]
    pub encoder_preset: EncoderPreset,
    /// Host accepts `Event::NetworkFeedback` and controls the bitrate from
    /// it (src/congestion.rs); older hosts reject unknown events.
    #[serde(default)]
    pub network_feedback: bool,
    /// Set while the host streams below the negotiated size or rate because
    /// the link cannot carry it; `None` at the negotiated stream.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stepped_down: Option<SteppedDown>,
}

/// A network step-down: the negotiated stream, and the bitrate that made the
/// host leave it.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub struct SteppedDown {
    pub width: u32,
    pub fps: u32,
    pub kbps: u32,
}

impl Desktop {
    /// Highest frame rate this host accepts in `ConfigureVideo`.
    pub fn accepted_max_fps(&self) -> u32 {
        if self.max_fps == 0 {
            LEGACY_MAX_FPS
        } else {
            self.max_fps.min(MAX_FPS)
        }
    }
}

/// Largest cursor side accepted on the wire (cursors are usually 16-64 px;
/// 256 covers large HiDPI themes).
pub const MAX_CURSOR_SIZE: u16 = 256;

/// Host cursor shape for a client that draws the pointer locally.
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq)]
pub struct CursorImage {
    pub width: u16,
    pub height: u16,
    pub hot_x: u16,
    pub hot_y: u16,
    /// Straight (not premultiplied) RGBA, row-major, `width * height * 4` bytes.
    #[serde(with = "base64_bytes")]
    pub rgba: Vec<u8>,
}

impl std::fmt::Debug for CursorImage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CursorImage")
            .field("width", &self.width)
            .field("height", &self.height)
            .field("hot_x", &self.hot_x)
            .field("hot_y", &self.hot_y)
            .finish_non_exhaustive()
    }
}

impl CursorImage {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            (1..=MAX_CURSOR_SIZE).contains(&self.width)
                && (1..=MAX_CURSOR_SIZE).contains(&self.height),
            "cursor size out of range"
        );
        ensure!(
            self.hot_x < self.width && self.hot_y < self.height,
            "cursor hot spot outside the image"
        );
        ensure!(
            self.rgba.len() == usize::from(self.width) * usize::from(self.height) * 4,
            "cursor pixel data does not match its size"
        );
        Ok(())
    }
}

/// Base64 keeps a 256x256 cursor (256 KiB) at ~350 KB of JSON, inside
/// `MAX_CONTROL`; a JSON number array would need up to four times the bytes.
mod base64_bytes {
    use base64::Engine;
    use serde::{Deserialize, Deserializer, Serializer};

    const ENGINE: base64::engine::GeneralPurpose = base64::engine::general_purpose::STANDARD;
    /// Encoded length of the largest valid image.
    const LIMIT: usize =
        (super::MAX_CURSOR_SIZE as usize * super::MAX_CURSOR_SIZE as usize * 4).div_ceil(3) * 4;

    pub fn serialize<S: Serializer>(bytes: &[u8], serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&ENGINE.encode(bytes))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Vec<u8>, D::Error> {
        let text = <std::borrow::Cow<'de, str>>::deserialize(deserializer)?;
        if text.len() > LIMIT {
            return Err(serde::de::Error::custom("cursor image too large"));
        }
        ENGINE
            .decode(text.as_bytes())
            .map_err(serde::de::Error::custom)
    }
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct Monitor {
    pub id: usize,
    pub name: String,
    pub width: u32,
    pub height: u32,
    /// Refresh rate in millihertz when the host knows it (X11 RandR); zero
    /// when unknown (the desktop portal does not report it).
    #[serde(default)]
    pub refresh_mhz: u32,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Update {
    MonitorStreamStopped {
        index: usize,
        reason: String,
        /// An acknowledgement of an explicit disable, not an encoder failure.
        #[serde(default)]
        requested: bool,
    },
    MonitorStream {
        index: usize,
        desktop: Desktop,
    },
    Telemetry {
        encode_us: u64,
        bitrate: u32,
        encoder: String,
    },
    Pong {
        id: u64,
    },
    Desktop {
        desktop: Desktop,
    },
    Clipboard {
        text: String,
    },
    Notice {
        text: String,
    },
    /// New host cursor shape; `None` hides the pointer (e.g. a game set a
    /// fully transparent cursor). Only sent after the client declared
    /// `ClientFeatures::local_cursor` to a host advertising
    /// `Desktop::local_cursor`.
    Cursor {
        cursor: Option<CursorImage>,
    },
    /// Where the host pointer is on monitor `monitor`, normalised like
    /// `Event::Motion` (0..=1). Latest wins, at most ~30 per second; only
    /// sent after the client declared `ClientFeatures::cursor_positions`.
    CursorPosition {
        monitor: usize,
        x: f64,
        y: f64,
    },
    /// Force feedback a game started on virtual pad `pad`: play `low`
    /// (strong, low-frequency motor) and `high` (weak, high-frequency) for
    /// `duration_ms` unless a newer update replaces it; zeros stop it. Only
    /// sent after the client declared `ClientFeatures::rumble` (older
    /// clients reject unknown updates).
    Rumble {
        pad: u8,
        low: u16,
        high: u16,
        duration_ms: u32,
    },
}

/// Longest `Update::Rumble` (SDL's `SDL_GameControllerRumble` limit); hosts
/// re-send effects that last longer.
pub const MAX_RUMBLE_MS: u32 = 0xFFFF;

impl Update {
    /// Rejects malformed host data before the client acts on it.
    pub fn validate(&self) -> Result<()> {
        match self {
            Self::Cursor {
                cursor: Some(image),
            } => image.validate()?,
            Self::CursorPosition { monitor, x, y } => ensure!(
                *monitor < 64 && (0.0..=1.0).contains(x) && (0.0..=1.0).contains(y),
                "invalid cursor position"
            ),
            Self::Rumble {
                pad, duration_ms, ..
            } => ensure!(
                *pad < MAX_GAMEPADS && *duration_ms <= MAX_RUMBLE_MS,
                "invalid rumble"
            ),
            _ => (),
        }
        Ok(())
    }
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct Input {
    pub sequence: u64,
    pub event: Event,
    /// Pointer motion ID when the client also sends motion as datagrams; the
    /// host applies a motion only if its ID is newer than the last applied.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub motion: Option<u64>,
}

/// Track carrying pointer motion as unreliable, latest-wins QUIC datagrams so a
/// lost packet never stalls key events queued behind it on the input track.
pub const POINTER_TRACK: &str = "pointer";

/// Largest per-event pointer delta; clients split larger movements.
pub const MAX_RELATIVE_MOTION: u32 = 4096;

/// Side mouse buttons in X11 numbering (SDL X1/X2, evdev BTN_SIDE/BTN_EXTRA).
pub const BUTTON_BACK: u8 = 8;
pub const BUTTON_FORWARD: u8 = 9;

/// Most gamepads one session may attach (Xbox-style player slots).
pub const MAX_GAMEPADS: u8 = 4;
/// SDL2 `SDL_GameControllerButton` values (A = 0 … TOUCHPAD = 20).
pub const GAMEPAD_BUTTONS: u8 = 21;
/// SDL2 `SDL_GameControllerAxis` values (LEFTX = 0 … TRIGGERRIGHT = 5).
pub const GAMEPAD_AXES: u8 = 6;
/// First trigger axis; triggers range 0..=32767, sticks the full i16 range.
pub const GAMEPAD_FIRST_TRIGGER: u8 = 4;
/// Longest controller name forwarded for host logs.
pub const MAX_GAMEPAD_NAME: usize = 128;

/// One pointer datagram. Only `Motion` and `MonitorMotion` are accepted.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct PointerInput {
    pub motion: u64,
    pub event: Event,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    MonitorStream {
        index: usize,
        enabled: bool,
    },
    MonitorMotion {
        index: usize,
        x: f64,
        y: f64,
    },
    MonitorFeedback {
        index: usize,
        queue_ms: u32,
        dropped_groups: u32,
    },
    Motion {
        x: f64,
        y: f64,
    },
    /// X11 numbering: 1 left, 2 middle, 3 right; [`BUTTON_BACK`] and
    /// [`BUTTON_FORWARD`] only to hosts advertising
    /// `Desktop::extra_mouse_buttons` (older hosts reject them).
    Button {
        button: u8,
        down: bool,
    },
    Key {
        code: u16,
        down: bool,
    },
    Scroll {
        x: i32,
        y: i32,
    },
    ReleaseAll,
    Ping,
    Probe {
        id: u64,
    },
    ConfigureVideo {
        width: u32,
        fps: u32,
        bitrate: u32,
    },
    SelectMonitor {
        index: usize,
    },
    Feedback {
        queue_ms: u32,
        dropped_groups: u32,
    },
    Clipboard {
        text: String,
    },
    ClipboardRequest,
    /// The client lost decoder state; ask for an IDR now instead of waiting for
    /// the next scheduled GOP. Only sent when `Desktop::keyframe_requests`.
    RequestKeyframe {
        /// Auxiliary monitor stream index; `None` is the primary stream.
        #[serde(default)]
        monitor: Option<usize>,
    },
    /// Raw pointer movement while the client has locked the pointer (games).
    /// Reliable and ordered: unlike absolute positions, a lost delta is lost
    /// movement. Only sent to hosts advertising `Desktop::relative_pointer`.
    RelativeMotion {
        dx: i32,
        dy: i32,
    },
    /// A local game controller was attached as pad `pad`. The host creates a
    /// virtual pad (or neutralises an existing one). Only sent to hosts
    /// advertising `Desktop::gamepads`, like every `Gamepad*` event.
    GamepadConnected {
        pad: u8,
        name: String,
    },
    /// The controller in slot `pad` went away; the host destroys its pad.
    GamepadDisconnected {
        pad: u8,
    },
    /// `button` is an SDL2 GameController button number.
    GamepadButton {
        pad: u8,
        button: u8,
        down: bool,
    },
    /// `axis` is an SDL2 GameController axis number; `value` uses SDL's range.
    GamepadAxis {
        pad: u8,
        axis: u8,
        value: i16,
    },
    /// Client capabilities the host may rely on for this session. Only sent to
    /// hosts advertising `Desktop::keyframe_requests`.
    ClientFeatures {
        /// The client sends `RequestKeyframe` on loss, so the host can space
        /// periodic IDRs far apart instead of every quarter second.
        #[serde(default)]
        keyframe_requests: bool,
        /// The client publishes pointer motion on [`POINTER_TRACK`] datagrams.
        #[serde(default)]
        pointer_datagrams: bool,
        /// The client draws the pointer itself from `Update::Cursor`, so the
        /// host leaves it out of the video. Only sent when
        /// `Desktop::local_cursor`.
        #[serde(default)]
        local_cursor: bool,
        /// The client plays `Update::Rumble`, so the host gives the pads it
        /// creates from now on force feedback (`FF_RUMBLE`). Older hosts
        /// ignore the field.
        #[serde(default)]
        rumble: bool,
        /// The client takes `Update::CursorPosition` (it places its pointer
        /// there when it leaves relative mode). Older hosts ignore the field.
        #[serde(default)]
        cursor_positions: bool,
    },
    /// The session's video codec. Sent as the very first input event to hosts
    /// advertising `Desktop::codec_selection`, which wait for it before
    /// starting video; it overrides the connection's `codec` parameter.
    SelectCodec {
        codec: VideoCodec,
        /// Encoder preset for this session, only to hosts advertising
        /// `Desktop::encoder_presets` (older hosts ignore the field); `None`
        /// keeps the host's `--encoder-preset`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        encoder_preset: Option<EncoderPreset>,
    },
    /// The client's display and stream choices; the host picks the stream
    /// size, rate and bitrate from them ([`negotiate`]) and answers with
    /// `Update::Desktop`. Sent on connect and again when the window's
    /// display, size or refresh rate changes. Only sent to hosts advertising
    /// `Desktop::display_negotiation`.
    ClientDisplay(ClientDisplay),
    /// Every ~100 ms of video: the mean queueing delay of frame arrivals
    /// above the lowest one-way time seen recently (`None` when no frame
    /// arrived) and the rate received. Only sent to hosts advertising
    /// `Desktop::network_feedback`.
    NetworkFeedback {
        delay_ms: Option<u32>,
        received_kbps: u32,
    },
}

/// What the client shows video on, and what the user asked for.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ClientDisplay {
    /// Video area in physical pixels; zero when unknown (headless).
    pub width: u32,
    pub height: u32,
    /// Refresh rate of the display showing the window, in millihertz; zero
    /// when unknown.
    pub refresh_mhz: u32,
    /// Explicit stream width (0 = the host monitor's native width); `None`
    /// matches the client's video area.
    #[serde(default)]
    pub stream_width: Option<u32>,
    /// Frame-rate cap chosen by the user; `None` matches the display.
    #[serde(default)]
    pub max_fps: Option<u32>,
    /// Bitrate in kbit/s; `None` lets the host size it ([`auto_bitrate`]).
    #[serde(default)]
    pub bitrate: Option<u32>,
}

impl ClientDisplay {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.width <= 32_768
                && self.height <= 32_768
                && self.refresh_mhz <= 1_000_000
                && self
                    .stream_width
                    .is_none_or(|width| width == 0 || (320..=7680).contains(&width))
                && self.max_fps.is_none_or(|fps| (1..=MAX_FPS).contains(&fps))
                && self
                    .bitrate
                    .is_none_or(|bitrate| (500..=100_000).contains(&bitrate)),
            "invalid client display"
        );
        Ok(())
    }
}

impl Event {
    /// Events that may be dropped under backpressure: a later event supersedes
    /// them (absolute motion, periodic feedback/probes, keyframe requests) or
    /// losing one is harmless (a wheel step). Keys and buttons (mouse and
    /// gamepad) are never lossy: a lost release would leave input held on the
    /// host. A gamepad axis is an absolute position that the next value fully
    /// supersedes. A dropped *final* value (a stick back at centre) would leave
    /// the pad deflected, so the client keeps an axis whose latest value was
    /// dropped dirty and re-sends it on the next loop iteration: under
    /// backpressure an axis is delayed and coalesced, never lost, and a stall
    /// cannot end the session the way a full queue of reliable events does.
    pub fn is_lossy(&self) -> bool {
        matches!(
            self,
            Self::Motion { .. }
                | Self::MonitorMotion { .. }
                | Self::Scroll { .. }
                | Self::Feedback { .. }
                | Self::NetworkFeedback { .. }
                | Self::MonitorFeedback { .. }
                | Self::Probe { .. }
                | Self::Ping
                | Self::RequestKeyframe { .. }
                | Self::GamepadAxis { .. }
        )
    }

    pub fn validate(&self) -> Result<()> {
        match self {
            Self::MonitorStream { index, .. } => ensure!(*index < 64, "invalid monitor index"),
            Self::MonitorMotion { index, x, y } => {
                ensure!(*index < 64, "invalid monitor index");
                Self::Motion { x: *x, y: *y }.validate()?;
            }
            Self::MonitorFeedback {
                index,
                queue_ms,
                dropped_groups,
            } => {
                ensure!(*index < 64, "invalid monitor index");
                Self::Feedback {
                    queue_ms: *queue_ms,
                    dropped_groups: *dropped_groups,
                }
                .validate()?;
            }
            Self::ConfigureVideo {
                width,
                fps,
                bitrate,
            } => ensure!(
                (*width == 0 || (320..=7680).contains(width))
                    && (1..=MAX_FPS).contains(fps)
                    && (500..=100_000).contains(bitrate),
                "invalid video settings"
            ),
            Self::Motion { x, y } => ensure!(
                x.is_finite()
                    && y.is_finite()
                    && (0.0..=1.0).contains(x)
                    && (0.0..=1.0).contains(y),
                "invalid pointer coordinates"
            ),
            Self::Button { button, .. } => ensure!(
                matches!(*button, 1..=3 | BUTTON_BACK | BUTTON_FORWARD),
                "unsupported button"
            ),
            Self::Key { code, .. } => ensure!((1..=127).contains(code), "unsupported key"),
            Self::RelativeMotion { dx, dy } => ensure!(
                dx.unsigned_abs() <= MAX_RELATIVE_MOTION
                    && dy.unsigned_abs() <= MAX_RELATIVE_MOTION,
                "relative motion out of range"
            ),
            Self::Scroll { x, y } => ensure!(
                x.abs_diff(0) <= 20 && y.abs_diff(0) <= 20,
                "scroll out of range"
            ),
            Self::SelectMonitor { index }
            | Self::RequestKeyframe {
                monitor: Some(index),
            } => ensure!(*index < 64, "invalid monitor index"),
            Self::Clipboard { text } => ensure!(
                text.len() <= MAX_TEXT && !text.contains('\0'),
                "clipboard text exceeds limit or contains NUL"
            ),
            Self::Feedback {
                queue_ms,
                dropped_groups,
            } => ensure!(
                *queue_ms <= 60_000 && *dropped_groups <= 100_000,
                "invalid video feedback"
            ),
            Self::NetworkFeedback {
                delay_ms,
                received_kbps,
            } => ensure!(
                delay_ms.is_none_or(|delay| delay <= 60_000) && *received_kbps <= 10_000_000,
                "invalid network feedback"
            ),
            Self::GamepadConnected { pad, name } => ensure!(
                *pad < MAX_GAMEPADS
                    && name.len() <= MAX_GAMEPAD_NAME
                    && !name.chars().any(char::is_control),
                "invalid gamepad slot or name"
            ),
            Self::ClientDisplay(display) => display.validate()?,
            Self::GamepadDisconnected { pad } => {
                ensure!(*pad < MAX_GAMEPADS, "invalid gamepad slot")
            }
            Self::GamepadButton { pad, button, .. } => ensure!(
                *pad < MAX_GAMEPADS && *button < GAMEPAD_BUTTONS,
                "invalid gamepad button"
            ),
            Self::GamepadAxis { pad, axis, value } => ensure!(
                *pad < MAX_GAMEPADS
                    && *axis < GAMEPAD_AXES
                    && (*axis < GAMEPAD_FIRST_TRIGGER || *value >= 0),
                "invalid gamepad axis"
            ),
            _ => (),
        }
        Ok(())
    }
}

pub fn validate_video_size(width: u32, height: u32) -> Result<()> {
    ensure!(
        (2..=7680).contains(&width)
            && (2..=8192).contains(&height)
            && u64::from(width) * u64::from(height) <= 33_554_432,
        "video size exceeds supported limits"
    );
    Ok(())
}

/// Highest stream frame rate (240 Hz monitors).
pub const MAX_FPS: u32 = 240;
/// What hosts without `Desktop::max_fps` accept; they reject anything higher.
pub const LEGACY_MAX_FPS: u32 = 120;
/// Frame rate when neither the user, the client display nor the host monitor
/// gives one.
pub const DEFAULT_FPS: u32 = 60;
/// Automatic bitrate up to 60 FPS, in kbit/s: the launcher's long-standing
/// default. Higher rates scale it ([`high_rate_bitrate`]).
pub const AUTO_BITRATE: u32 = 20_000;
/// Bits per pixel per frame a stream above 60 FPS gets at least, before the
/// frame-rate factor: about 13 Mbps for 1440p and 30 Mbps for 4K.
const HIGH_RATE_BITS_PER_PIXEL: f64 = 0.06;

/// Whole hertz from millihertz (179 960 → 180, 59 940 → 60); zero stays
/// unknown.
pub fn refresh_hz(refresh_mhz: u32) -> u32 {
    refresh_mhz.saturating_add(500) / 1000
}

/// The bitrate for a stream that would get `kbps` at 60 FPS. At 60 FPS and
/// below `kbps` is returned unchanged. Above, it is raised to a per-pixel
/// floor for the size and scaled by sqrt(fps / 60): more frames per second
/// each carry less change, so bits per frame may fall (Moonlight uses the
/// same curve). Never below `kbps`, never above the protocol's 100 Mbps.
pub fn high_rate_bitrate(kbps: u32, width: u32, height: u32, fps: u32) -> u32 {
    if fps <= 60 {
        return kbps;
    }
    let floor = f64::from(width) * f64::from(height) * 60.0 * HIGH_RATE_BITS_PER_PIXEL / 1000.0;
    let scaled = f64::from(kbps).max(floor) * (f64::from(fps) / 60.0).sqrt();
    (scaled as u32).min(100_000).max(kbps)
}

/// Bitrate when the user leaves it automatic.
pub fn auto_bitrate(width: u32, height: u32, fps: u32) -> u32 {
    high_rate_bitrate(AUTO_BITRATE, width, height, fps)
}

/// A stream configuration chosen from a [`ClientDisplay`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StreamChoice {
    /// Stream width as `ConfigureVideo` takes it: 0 = the monitor's native.
    pub width: u32,
    pub fps: u32,
    pub bitrate: u32,
}

/// Pick the stream for `display` from a host monitor of `monitor` pixels
/// refreshing at `monitor_refresh_mhz` (0 = unknown), on a host accepting up
/// to `host_max_fps`.
///
/// Rate: the lowest of the user's cap, the client display's and the host
/// monitor's refresh rate, and the host's maximum; [`DEFAULT_FPS`] when none
/// of the first three is known. Size: an explicit width as given; otherwise
/// the largest size with the monitor's aspect ratio that fits the client's
/// video area, never above native (no upscaling), and native when the area
/// is unknown.
pub fn negotiate(
    display: &ClientDisplay,
    monitor: (u32, u32),
    monitor_refresh_mhz: u32,
    host_max_fps: u32,
) -> StreamChoice {
    let fps = [
        display.max_fps.unwrap_or(0),
        refresh_hz(display.refresh_mhz),
        refresh_hz(monitor_refresh_mhz),
    ]
    .into_iter()
    .filter(|&fps| fps > 0)
    .min()
    .unwrap_or(DEFAULT_FPS)
    .clamp(1, host_max_fps.clamp(1, MAX_FPS));
    let (monitor_width, monitor_height) = (monitor.0.max(2), monitor.1.max(2));
    let width = display.stream_width.unwrap_or_else(|| {
        if display.width == 0 || display.height == 0 || monitor_width <= 320 {
            return 0;
        }
        let scale = (f64::from(display.width) / f64::from(monitor_width))
            .min(f64::from(display.height) / f64::from(monitor_height));
        let width = (f64::from(monitor_width) * scale) as u32 / 2 * 2;
        if width >= monitor_width {
            0
        } else {
            width.max(320)
        }
    });
    let (stream_width, stream_height) = match width {
        0 => (monitor_width, monitor_height),
        width => (
            width,
            (u64::from(width) * u64::from(monitor_height) / u64::from(monitor_width)) as u32,
        ),
    };
    StreamChoice {
        width,
        fps,
        bitrate: display
            .bitrate
            .unwrap_or_else(|| auto_bitrate(stream_width, stream_height, fps)),
    }
}

/// Read untrusted MoQ frames with an application-level size limit.
pub async fn read_frame(
    group: &mut moq_net::group::Consumer,
    limit: usize,
) -> Result<Option<bytes::Bytes>> {
    Ok(read_timed_frame(group, limit)
        .await?
        .map(|(_, _, bytes)| bytes))
}

/// Whether an Annex B access unit is an IDR (H.264) or IRAP (H.265) picture,
/// where a decoder can start. Decided by its first slice NAL unit, so only
/// the parameter sets in front of it are scanned. Encoders with intra
/// refresh also flag refresh starts as key frames; those are not.
pub fn annexb_keyframe(codec: VideoCodec, data: &[u8]) -> bool {
    let mut index = 0;
    while index + 3 < data.len() {
        if data[index] != 0 || data[index + 1] != 0 || data[index + 2] != 1 {
            index += 1;
            continue;
        }
        let header = data[index + 3];
        match codec {
            VideoCodec::H264 => match header & 0x1f {
                5 => return true,
                1..=4 => return false,
                _ => (),
            },
            VideoCodec::H265 => match (header >> 1) & 0x3f {
                16..=21 => return true,
                0..=31 => return false,
                _ => (),
            },
        }
        index += 3;
    }
    false
}

/// [`read_frame`] with the frame's MoQ timestamp and when its first byte
/// arrived.
pub async fn read_timed_frame(
    group: &mut moq_net::group::Consumer,
    limit: usize,
) -> Result<Option<(moq_net::Timestamp, std::time::Instant, bytes::Bytes)>> {
    let Some(mut frame) = group.next_frame().await? else {
        return Ok(None);
    };
    let arrived = std::time::Instant::now();
    ensure!(frame.size <= limit as u64, "frame exceeds {limit} bytes");
    let timestamp = frame.timestamp;
    Ok(Some((timestamp, arrived, frame.read_all().await?)))
}

/// Test harnesses only (`TELEPORT_TEST_WALL_CLOCK=1` on both ends of one
/// machine): video frames carry their capture time on the Unix clock instead
/// of moq's jittered local clock, so the client can measure capture→ready.
/// Off by default; peers never compare clocks across machines.
pub fn test_wall_clock() -> bool {
    static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ENABLED.get_or_init(|| std::env::var_os("TELEPORT_TEST_WALL_CLOCK").is_some_and(|v| v == "1"))
}

/// The timestamp of a video frame captured at `captured`.
pub fn video_timestamp(captured: std::time::Instant) -> moq_net::Timestamp {
    if test_wall_clock()
        && let Ok(now) = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)
        && let Ok(timestamp) = moq_net::Timestamp::try_from(now.saturating_sub(captured.elapsed()))
    {
        return timestamp;
    }
    moq_net::Timestamp::now()
}

/// Capture→now of a frame stamped by [`video_timestamp`], in test mode only.
pub fn video_frame_age(timestamp: moq_net::Timestamp) -> Option<Duration> {
    if !test_wall_clock() {
        return None;
    }
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?;
    now.checked_sub(Duration::from(timestamp))
}

/// Control/update history must not be skipped when QUIC streams reorder.
pub async fn ordered_group(
    track: &mut moq_net::track::Subscriber,
    pending: &mut std::collections::BTreeMap<u64, moq_net::group::Consumer>,
    expected: u64,
) -> Result<moq_net::group::Consumer> {
    use anyhow::Context;
    if let Some(group) = pending.remove(&expected) {
        return Ok(group);
    }
    loop {
        let group = if pending.is_empty() {
            track.recv_group().await?
        } else {
            tokio::time::timeout(INPUT_TIMEOUT, track.recv_group())
                .await
                .context("control reorder gap expired")??
        }
        .context("ordered track closed")?;
        ensure!(
            group.sequence >= expected && group.sequence - expected <= 16,
            "control group outside reorder window"
        );
        if group.sequence == expected {
            return Ok(group);
        }
        ensure!(
            pending.insert(group.sequence, group).is_none(),
            "duplicate control group"
        );
    }
}

/// SDL scancodes use USB HID usage IDs; the portal uses Linux evdev codes.
pub fn evdev(scancode: sdl2::keyboard::Scancode) -> Option<u16> {
    let code = scancode as usize;
    let letters = [
        30, 48, 46, 32, 18, 33, 34, 35, 23, 36, 37, 38, 50, 49, 24, 25, 16, 19, 31, 20, 22, 47, 17,
        45, 21, 44,
    ];
    match code {
        4..=29 => Some(letters[code - 4]),
        30..=38 => Some((code - 28) as u16),
        39 => Some(11),
        40 => Some(28),
        41 => Some(1),
        42 => Some(14),
        43 => Some(15),
        44 => Some(57),
        45 => Some(12),
        46 => Some(13),
        47 => Some(26),
        48 => Some(27),
        49 => Some(43),
        51 => Some(39),
        52 => Some(40),
        53 => Some(41),
        54 => Some(51),
        55 => Some(52),
        56 => Some(53),
        57 => Some(58),
        58..=67 => Some((code + 1) as u16),
        68 => Some(87),
        69 => Some(88),
        70 => Some(99),
        71 => Some(70),
        72 => Some(119),
        73 => Some(110),
        74 => Some(102),
        75 => Some(104),
        76 => Some(111),
        77 => Some(107),
        78 => Some(109),
        79 => Some(106),
        80 => Some(105),
        81 => Some(108),
        82 => Some(103),
        83 => Some(69), // Num Lock
        84 => Some(98), // keypad /
        85 => Some(55), // keypad *
        86 => Some(74), // keypad -
        87 => Some(78), // keypad +
        88 => Some(96), // keypad Enter
        89..=97 => Some([79, 80, 81, 75, 76, 77, 71, 72, 73][code - 89]),
        98 => Some(82),                        // keypad 0
        99 => Some(83),                        // keypad decimal
        100 => Some(86),                       // ISO non-US backslash
        101 => Some(127),                      // application/menu
        103 => Some(117),                      // keypad equals
        104..=115 => Some((code + 79) as u16), // F13..F24
        224 => Some(29),
        225 => Some(42),
        226 => Some(56),
        227 => Some(125),
        228 => Some(97),
        229 => Some(54),
        230 => Some(100),
        231 => Some(126),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pairing_files_with_and_without_device_keys() {
        let token = "ab".repeat(32);
        let fingerprint = "cd".repeat(32);
        // A pairing file written before device keys.
        let old: Pairing = serde_json::from_value(serde_json::json!({
            "token": token, "fingerprint": fingerprint,
        }))
        .unwrap();
        old.validate().unwrap();
        assert!(old.device_key.is_none());
        // Hash-only credentials serialize exactly as before, so older
        // clients and hosts read what this version writes.
        assert_eq!(
            serde_json::to_value(&old).unwrap(),
            serde_json::json!({ "token": token, "fingerprint": fingerprint })
        );
        let keyed = Pairing {
            device_key: Some("0f".repeat(32)),
            ..old.clone()
        };
        keyed.validate().unwrap();
        let reparsed: Pairing =
            serde_json::from_slice(&serde_json::to_vec(&keyed).unwrap()).unwrap();
        assert_eq!(reparsed.device_key, keyed.device_key);
        for bad in ["", "0f", &"zz".repeat(32), &"0f".repeat(33)] {
            let pairing = Pairing {
                device_key: Some(bad.into()),
                ..old.clone()
            };
            assert!(pairing.validate().is_err(), "{bad}");
        }
    }

    #[test]
    fn monitor_controls_validate_indexes_and_payloads() {
        let legacy: Desktop = serde_json::from_value(serde_json::json!({
            "version": VERSION, "width": 1280, "height": 720, "fps": 60,
            "source": "test", "monitors": [], "active_monitor": 0,
            "audio": false, "clipboard": false
        }))
        .unwrap();
        assert!(
            !legacy.multimonitor,
            "older hosts must not advertise independent streams"
        );
        assert!(
            Event::MonitorStream {
                index: 63,
                enabled: true
            }
            .validate()
            .is_ok()
        );
        assert!(
            Event::MonitorStream {
                index: 64,
                enabled: true
            }
            .validate()
            .is_err()
        );
        assert!(
            Event::MonitorMotion {
                index: 1,
                x: 0.0,
                y: 1.0
            }
            .validate()
            .is_ok()
        );
        assert!(
            Event::MonitorMotion {
                index: 1,
                x: f64::INFINITY,
                y: 1.0
            }
            .validate()
            .is_err()
        );
        assert!(
            Event::MonitorFeedback {
                index: 1,
                queue_ms: 60_001,
                dropped_groups: 0
            }
            .validate()
            .is_err()
        );
        let event = Event::MonitorMotion {
            index: 1,
            x: 0.25,
            y: 0.75,
        };
        let wire = serde_json::to_vec(&event).unwrap();
        assert!(matches!(
            serde_json::from_slice::<Event>(&wire).unwrap(),
            Event::MonitorMotion { index: 1, .. }
        ));
    }
    #[test]
    fn only_superseded_or_harmless_events_are_lossy() {
        assert!(Event::Motion { x: 0.5, y: 0.5 }.is_lossy());
        assert!(Event::Scroll { x: 0, y: 1 }.is_lossy());
        assert!(Event::RequestKeyframe { monitor: None }.is_lossy());
        for strict in [
            Event::Key {
                code: 30,
                down: false,
            },
            Event::Button {
                button: 1,
                down: false,
            },
            Event::ReleaseAll,
            Event::ClipboardRequest,
            Event::SelectMonitor { index: 0 },
            Event::ClientFeatures {
                keyframe_requests: true,
                pointer_datagrams: true,
                local_cursor: true,
                rumble: true,
                cursor_positions: true,
            },
        ] {
            assert!(!strict.is_lossy(), "{strict:?} must never be dropped");
        }
    }
    #[test]
    fn relative_motion_is_bounded_reliable_and_negotiated() {
        let max = MAX_RELATIVE_MOTION as i32;
        assert!(
            Event::RelativeMotion { dx: max, dy: -max }
                .validate()
                .is_ok()
        );
        assert!(
            Event::RelativeMotion { dx: max + 1, dy: 0 }
                .validate()
                .is_err()
        );
        assert!(
            Event::RelativeMotion {
                dx: 0,
                dy: i32::MIN
            }
            .validate()
            .is_err()
        );
        // A dropped delta is lost movement: never shed under backpressure.
        assert!(!Event::RelativeMotion { dx: 1, dy: 1 }.is_lossy());
        let old: Desktop = serde_json::from_value(serde_json::json!({ "version": 2, "width": 1, "height": 1, "fps": 1,
            "source": "test", "monitors": [], "active_monitor": 0, "audio": false, "clipboard": false })).unwrap();
        assert!(!old.relative_pointer);
    }
    #[test]
    fn gamepad_events_are_bounded_negotiated_and_roundtrip() {
        let old: Desktop = serde_json::from_value(serde_json::json!({ "version": 2, "width": 1, "height": 1, "fps": 1,
            "source": "test", "monitors": [], "active_monitor": 0, "audio": false, "clipboard": false })).unwrap();
        assert!(!old.gamepads, "older hosts must not receive gamepad events");
        for event in [
            Event::GamepadConnected {
                pad: MAX_GAMEPADS - 1,
                name: "Xbox Series X Controller".into(),
            },
            Event::GamepadDisconnected { pad: 0 },
            Event::GamepadButton {
                pad: 1,
                button: GAMEPAD_BUTTONS - 1,
                down: true,
            },
            Event::GamepadAxis {
                pad: 2,
                axis: 1,
                value: i16::MIN,
            },
            Event::GamepadAxis {
                pad: 2,
                axis: GAMEPAD_AXES - 1,
                value: i16::MAX,
            },
        ] {
            assert!(event.validate().is_ok(), "{event:?}");
            let wire = serde_json::to_vec(&event).unwrap();
            let back = serde_json::from_slice::<Event>(&wire).unwrap();
            assert_eq!(format!("{back:?}"), format!("{event:?}"));
        }
        for invalid in [
            Event::GamepadConnected {
                pad: MAX_GAMEPADS,
                name: String::new(),
            },
            Event::GamepadConnected {
                pad: 0,
                name: "x".repeat(MAX_GAMEPAD_NAME + 1),
            },
            Event::GamepadConnected {
                pad: 0,
                name: "pad\n".into(),
            },
            Event::GamepadDisconnected { pad: MAX_GAMEPADS },
            Event::GamepadButton {
                pad: MAX_GAMEPADS,
                button: 0,
                down: true,
            },
            Event::GamepadButton {
                pad: 0,
                button: GAMEPAD_BUTTONS,
                down: true,
            },
            Event::GamepadAxis {
                pad: 0,
                axis: GAMEPAD_AXES,
                value: 0,
            },
            // SDL triggers never go negative.
            Event::GamepadAxis {
                pad: 0,
                axis: GAMEPAD_FIRST_TRIGGER,
                value: -1,
            },
        ] {
            assert!(invalid.validate().is_err(), "{invalid:?}");
        }
        // Values outside i16 / u8, or missing fields, fail to decode at all.
        for wire in [
            r#"{"type":"gamepad_axis","pad":0,"axis":0,"value":32768}"#,
            r#"{"type":"gamepad_button","pad":256,"button":0,"down":true}"#,
            r#"{"type":"gamepad_button","pad":0,"button":0}"#,
        ] {
            assert!(serde_json::from_str::<Event>(wire).is_err(), "{wire}");
        }
        // Buttons and attach/detach are never shed; an axis is superseded.
        for strict in [
            Event::GamepadButton {
                pad: 0,
                button: 0,
                down: false,
            },
            Event::GamepadDisconnected { pad: 0 },
            Event::GamepadConnected {
                pad: 0,
                name: String::new(),
            },
        ] {
            assert!(!strict.is_lossy(), "{strict:?} must never be dropped");
        }
        assert!(
            Event::GamepadAxis {
                pad: 0,
                axis: 0,
                value: 0
            }
            .is_lossy()
        );
    }
    #[test]
    fn keyframe_requests_roundtrip_and_bound_monitor_index() {
        for monitor in [None, Some(3)] {
            let wire = serde_json::to_vec(&Event::RequestKeyframe { monitor }).unwrap();
            let event = serde_json::from_slice::<Event>(&wire).unwrap();
            assert!(matches!(event, Event::RequestKeyframe { monitor: m } if m == monitor));
            assert!(event.validate().is_ok());
        }
        let features: Event =
            serde_json::from_str(r#"{"type":"client_features","keyframe_requests":true}"#).unwrap();
        assert!(matches!(
            features,
            Event::ClientFeatures {
                keyframe_requests: true,
                pointer_datagrams: false,
                local_cursor: false,
                rumble: false,
                cursor_positions: false,
            }
        ));
        let primary: Event = serde_json::from_str(r#"{"type":"request_keyframe"}"#).unwrap();
        assert!(matches!(primary, Event::RequestKeyframe { monitor: None }));
        assert!(
            Event::RequestKeyframe { monitor: Some(64) }
                .validate()
                .is_err()
        );
    }
    #[test]
    fn old_metadata_defaults_to_h264_without_new_capabilities() {
        let old = serde_json::json!({ "version": 2, "width": 1280, "height": 720, "fps": 60,
            "source": "test", "monitors": [], "active_monitor": 0, "audio": false, "clipboard": false });
        let desktop: Desktop = serde_json::from_value(old.clone()).unwrap();
        assert_eq!(desktop.codec, VideoCodec::H264);
        assert!(!desktop.configurable_video && !desktop.telemetry);
        assert!(!desktop.keyframe_requests);
        assert!(!desktop.local_cursor);
        assert!(!desktop.codec_selection && desktop.hardware_codecs.is_empty());
        assert_eq!(desktop.video_start_group, 0);
        let mut invalid = old;
        invalid["codec"] = serde_json::json!("surprise");
        assert!(serde_json::from_value::<Desktop>(invalid).is_err());
    }
    #[test]
    fn codec_selection_roundtrips_and_is_never_dropped() {
        let event: Event =
            serde_json::from_str(r#"{"type":"select_codec","codec":"h265"}"#).unwrap();
        assert!(matches!(
            event,
            Event::SelectCodec {
                codec: VideoCodec::H265,
                encoder_preset: None,
            }
        ));
        assert!(event.validate().is_ok() && !event.is_lossy());
        assert!(serde_json::from_str::<Event>(r#"{"type":"select_codec","codec":"av1"}"#).is_err());
        let desktop: Desktop = serde_json::from_value(serde_json::json!({ "version": 2,
            "width": 1, "height": 1, "fps": 1, "source": "test", "monitors": [],
            "active_monitor": 0, "audio": false, "clipboard": false,
            "codec_selection": true, "hardware_codecs": ["h264", "h265"] }))
        .unwrap();
        assert!(desktop.codec_selection);
        assert_eq!(
            desktop.hardware_codecs,
            [VideoCodec::H264, VideoCodec::H265]
        );
        // Encoder presets: optional on the wire, old hosts default to none.
        assert!(!desktop.encoder_presets);
        assert_eq!(desktop.encoder_preset, EncoderPreset::Latency);
        let chosen = Event::SelectCodec {
            codec: VideoCodec::H264,
            encoder_preset: Some(EncoderPreset::Quality),
        };
        let wire = serde_json::to_string(&chosen).unwrap();
        assert!(wire.contains(r#""encoder_preset":"quality""#), "{wire}");
        assert!(matches!(
            serde_json::from_str::<Event>(&wire).unwrap(),
            Event::SelectCodec {
                encoder_preset: Some(EncoderPreset::Quality),
                ..
            }
        ));
        let plain = serde_json::to_string(&Event::SelectCodec {
            codec: VideoCodec::H264,
            encoder_preset: None,
        })
        .unwrap();
        assert!(!plain.contains("encoder_preset"), "{plain}");
    }
    #[test]
    fn rumble_updates_are_bounded_and_declared_by_the_client() {
        let rumble = |pad, duration_ms| Update::Rumble {
            pad,
            low: u16::MAX,
            high: 0,
            duration_ms,
        };
        assert!(rumble(MAX_GAMEPADS - 1, MAX_RUMBLE_MS).validate().is_ok());
        assert!(rumble(MAX_GAMEPADS, 100).validate().is_err());
        assert!(rumble(0, MAX_RUMBLE_MS + 1).validate().is_err());
        let json = serde_json::to_string(&rumble(1, 250)).unwrap();
        assert!(json.contains(r#""type":"rumble""#), "{json}");
        assert!(matches!(
            serde_json::from_str::<Update>(&json).unwrap(),
            Update::Rumble {
                pad: 1,
                duration_ms: 250,
                ..
            }
        ));
        // Clients from before rumble never declare it; hosts ignore the
        // field if they predate it.
        let features: Event =
            serde_json::from_str(r#"{"type":"client_features","keyframe_requests":true}"#).unwrap();
        assert!(matches!(
            features,
            Event::ClientFeatures { rumble: false, .. }
        ));
    }
    #[test]
    fn side_mouse_buttons_are_valid_and_negotiated() {
        let button = |button| Event::Button { button, down: true }.validate();
        for valid in [1, 2, 3, BUTTON_BACK, BUTTON_FORWARD] {
            assert!(button(valid).is_ok(), "{valid}");
        }
        // 4-7 are wheel steps in X11 numbering; they travel as Scroll.
        for invalid in [0, 4, 5, 6, 7, 10, 255] {
            assert!(button(invalid).is_err(), "{invalid}");
        }
        let old: Desktop = serde_json::from_value(serde_json::json!({ "version": 2, "width": 1,
            "height": 1, "fps": 1, "source": "test", "monitors": [], "active_monitor": 0,
            "audio": false, "clipboard": false }))
        .unwrap();
        assert!(
            !old.extra_mouse_buttons,
            "older hosts reject buttons 8 and 9"
        );
    }
    fn cursor(width: u16, height: u16) -> CursorImage {
        CursorImage {
            width,
            height,
            hot_x: 0,
            hot_y: 0,
            rgba: vec![0x80; usize::from(width) * usize::from(height) * 4],
        }
    }

    #[test]
    fn cursor_updates_roundtrip_compactly_and_reject_bad_images() {
        for update in [
            Update::Cursor {
                cursor: Some(cursor(16, 24)),
            },
            Update::Cursor { cursor: None },
        ] {
            let wire = serde_json::to_vec(&update).unwrap();
            let decoded: Update = serde_json::from_slice(&wire).unwrap();
            decoded.validate().unwrap();
            match (update, decoded) {
                (Update::Cursor { cursor: a }, Update::Cursor { cursor: b }) => assert_eq!(a, b),
                _ => unreachable!(),
            }
        }
        // The largest cursor still fits a control frame.
        let max = Update::Cursor {
            cursor: Some(cursor(MAX_CURSOR_SIZE, MAX_CURSOR_SIZE)),
        };
        assert!(serde_json::to_vec(&max).unwrap().len() <= MAX_CONTROL);
        max.validate().unwrap();

        assert!(cursor(MAX_CURSOR_SIZE + 1, 1).validate().is_err());
        assert!(cursor(0, 16).validate().is_err());
        let mut short = cursor(16, 16);
        short.rgba.pop();
        assert!(short.validate().is_err());
        let mut hot = cursor(16, 16);
        hot.hot_x = 16;
        assert!(hot.validate().is_err());
        assert!(
            Update::Cursor {
                cursor: Some(hot.clone())
            }
            .validate()
            .is_err()
        );
        // Oversized payloads are refused before decoding.
        let huge = serde_json::json!({ "type": "cursor", "cursor": { "width": 1, "height": 1,
            "hot_x": 0, "hot_y": 0, "rgba": "A".repeat(400_000) } });
        assert!(serde_json::from_value::<Update>(huge).is_err());
        let invalid = serde_json::json!({ "type": "cursor", "cursor": { "width": 1, "height": 1,
            "hot_x": 0, "hot_y": 0, "rgba": "not base64!" } });
        assert!(serde_json::from_value::<Update>(invalid).is_err());
    }

    #[test]
    fn local_cursor_is_negotiated_by_both_ends() {
        let features: Event = serde_json::from_str(
            r#"{"type":"client_features","keyframe_requests":true,"local_cursor":true}"#,
        )
        .unwrap();
        assert!(matches!(
            features,
            Event::ClientFeatures {
                local_cursor: true,
                ..
            }
        ));
        assert!(!features.is_lossy());
        let desktop: Desktop = serde_json::from_value(serde_json::json!({ "version": 2,
            "width": 1, "height": 1, "fps": 1, "source": "X11", "monitors": [],
            "active_monitor": 0, "audio": false, "clipboard": false, "local_cursor": true }))
        .unwrap();
        assert!(desktop.local_cursor);
        // Positions only go to clients that ask for them, and are bounded.
        let features: Event = serde_json::from_str(
            r#"{"type":"client_features","local_cursor":true,"cursor_positions":true}"#,
        )
        .unwrap();
        assert!(matches!(
            features,
            Event::ClientFeatures {
                cursor_positions: true,
                ..
            }
        ));
        let position = Update::CursorPosition {
            monitor: 1,
            x: 0.25,
            y: 1.0,
        };
        let wire = serde_json::to_vec(&position).unwrap();
        assert!(
            serde_json::from_slice::<Update>(&wire)
                .unwrap()
                .validate()
                .is_ok()
        );
        for (monitor, x, y) in [(64, 0.5, 0.5), (0, -0.1, 0.5), (0, 0.5, f64::NAN)] {
            assert!(Update::CursorPosition { monitor, x, y }.validate().is_err());
        }
    }

    #[test]
    fn input_bounds() {
        assert!(
            Event::ConfigureVideo {
                width: 0,
                fps: 240,
                bitrate: 100_000
            }
            .validate()
            .is_ok()
        );
        for (width, fps, bitrate) in [
            (319, 60, 8000),
            (7681, 60, 8000),
            (1920, 0, 8000),
            (1920, 241, 8000),
            (1920, 60, 100_001),
        ] {
            assert!(
                Event::ConfigureVideo {
                    width,
                    fps,
                    bitrate
                }
                .validate()
                .is_err()
            );
        }
        assert!(validate_video_size(5120, 1440).is_ok());
        assert!(validate_video_size(2160, 3840).is_ok());
        assert!(validate_video_size(7680, 8192).is_err());
        assert!(
            Event::Motion {
                x: f64::NAN,
                y: 0.0
            }
            .validate()
            .is_err()
        );
        assert!(Event::Motion { x: 1.1, y: 0.0 }.validate().is_err());
        assert!(Event::Scroll { x: i32::MIN, y: 0 }.validate().is_err());
        assert!(
            Event::Key {
                code: 125,
                down: true
            }
            .validate()
            .is_ok()
        );
    }
    #[test]
    fn keyboard_mapping() {
        use sdl2::keyboard::Scancode::*;
        assert_eq!(evdev(A), Some(30));
        assert_eq!(evdev(Return), Some(28));
        assert_eq!(evdev(RGui), Some(126));
        assert_eq!(evdev(F12), Some(88));
        assert_eq!(evdev(KpEnter), Some(96));
        assert_eq!(evdev(Kp1), Some(79));
        assert_eq!(evdev(Kp9), Some(73));
        assert_eq!(evdev(Kp0), Some(82));
        assert_eq!(evdev(NumLockClear), Some(69));
        assert_eq!(evdev(NonUsBackslash), Some(86));
        assert_eq!(evdev(Application), Some(127));
        assert_eq!(evdev(F13), Some(183));
        assert_eq!(evdev(F24), Some(194));
    }

    #[test]
    fn high_frame_rates_are_capability_gated_both_ways() {
        // A new client reading an old host: no max_fps, no negotiation.
        let old: Desktop = serde_json::from_value(serde_json::json!({ "version": 2,
            "width": 1280, "height": 720, "fps": 60, "source": "X11",
            "monitors": [{ "id": 0, "name": "A", "width": 2560, "height": 1440 }],
            "active_monitor": 0, "audio": false, "clipboard": false,
            "configurable_video": true }))
        .unwrap();
        assert_eq!(old.accepted_max_fps(), LEGACY_MAX_FPS);
        assert!(!old.display_negotiation);
        assert_eq!(old.monitors[0].refresh_mhz, 0);
        // An old client reading a new host ignores the new fields (no
        // deny_unknown_fields on the wire types).
        let new = serde_json::to_value(Desktop {
            max_fps: MAX_FPS,
            display_negotiation: true,
            ..old.clone()
        })
        .unwrap();
        assert_eq!(new["max_fps"], 240);
        let mut capped = old.clone();
        capped.max_fps = 1000;
        assert_eq!(capped.accepted_max_fps(), MAX_FPS);
        // An old host keeps refusing above 120; new hosts accept up to 240.
        let configure = |fps| Event::ConfigureVideo {
            width: 0,
            fps,
            bitrate: 20_000,
        };
        assert!(configure(180).validate().is_ok());
        assert!(configure(241).validate().is_err());
        // The event round-trips; old hosts fail closed on unknown events,
        // which is why clients only send it to hosts advertising negotiation.
        let event = Event::ClientDisplay(ClientDisplay {
            width: 2560,
            height: 1440,
            refresh_mhz: 179_960,
            stream_width: None,
            max_fps: Some(240),
            bitrate: None,
        });
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains(r#""type":"client_display""#), "{json}");
        let back: Event = serde_json::from_str(&json).unwrap();
        back.validate().unwrap();
        assert!(!back.is_lossy());
        for invalid in [
            ClientDisplay {
                max_fps: Some(241),
                ..Default::default()
            },
            ClientDisplay {
                max_fps: Some(0),
                ..Default::default()
            },
            ClientDisplay {
                stream_width: Some(100),
                ..Default::default()
            },
            ClientDisplay {
                bitrate: Some(100_001),
                ..Default::default()
            },
            ClientDisplay {
                refresh_mhz: 2_000_000,
                ..Default::default()
            },
        ] {
            assert!(Event::ClientDisplay(invalid).validate().is_err());
        }
    }

    #[test]
    fn negotiation_takes_the_lowest_known_rate_and_fits_the_client() {
        let qhd = (2560, 1440);
        let display = |width, height, refresh_mhz, max_fps| ClientDisplay {
            width,
            height,
            refresh_mhz,
            stream_width: None,
            max_fps,
            bitrate: None,
        };
        // 179.96 Hz client, 179.96 Hz host monitor, no cap: 180 at native.
        let choice = negotiate(&display(2560, 1440, 179_960, None), qhd, 179_960, MAX_FPS);
        assert_eq!((choice.width, choice.fps), (0, 180));
        // A user cap, a slower client display, a slower host monitor and an
        // old host's limit each win when lowest.
        assert_eq!(
            negotiate(&display(0, 0, 179_960, Some(120)), qhd, 0, MAX_FPS).fps,
            120
        );
        assert_eq!(
            negotiate(&display(0, 0, 59_940, Some(240)), qhd, 179_960, MAX_FPS).fps,
            60
        );
        assert_eq!(
            negotiate(&display(0, 0, 240_000, None), qhd, 143_856, MAX_FPS).fps,
            144
        );
        assert_eq!(
            negotiate(&display(0, 0, 179_960, None), qhd, 0, LEGACY_MAX_FPS).fps,
            120
        );
        // Nothing known: the default rate.
        assert_eq!(negotiate(&display(0, 0, 0, None), qhd, 0, MAX_FPS).fps, 60);
        // Size: fit the client's area with the monitor's aspect ratio.
        let window = negotiate(&display(1280, 800, 60_000, None), qhd, 0, MAX_FPS);
        assert_eq!(window.width, 1280);
        let tall = negotiate(&display(1920, 1200, 60_000, None), qhd, 0, MAX_FPS);
        assert_eq!(tall.width, 1920);
        let wide = negotiate(&display(1600, 720, 60_000, None), qhd, 0, MAX_FPS);
        assert_eq!(wide.width, 1280);
        // Larger than the monitor, or unknown: native, never upscaled.
        assert_eq!(
            negotiate(&display(3840, 2160, 60_000, None), qhd, 0, MAX_FPS).width,
            0
        );
        assert_eq!(negotiate(&display(0, 0, 0, None), qhd, 0, MAX_FPS).width, 0);
        // Tiny windows stay at a usable minimum.
        assert_eq!(
            negotiate(&display(200, 100, 0, None), qhd, 0, MAX_FPS).width,
            320
        );
        // Explicit choices pass through.
        let explicit = ClientDisplay {
            stream_width: Some(1920),
            bitrate: Some(12_000),
            ..display(1280, 720, 60_000, None)
        };
        let choice = negotiate(&explicit, qhd, 0, MAX_FPS);
        assert_eq!((choice.width, choice.bitrate), (1920, 12_000));
        // Automatic bitrate follows the chosen size and rate.
        assert_eq!(
            negotiate(&display(2560, 1440, 179_960, None), qhd, 0, MAX_FPS).bitrate,
            auto_bitrate(2560, 1440, 180)
        );
    }

    #[test]
    fn automatic_bitrate_only_grows_above_60_fps() {
        for fps in [1, 30, 60] {
            assert_eq!(high_rate_bitrate(8000, 3840, 2160, fps), 8000);
            assert_eq!(auto_bitrate(2560, 1440, fps), AUTO_BITRATE);
        }
        // 1440p: max(20, 13.3) Mbps × sqrt(fps / 60).
        assert_eq!(auto_bitrate(2560, 1440, 120), 28_284);
        assert_eq!(auto_bitrate(2560, 1440, 180), 34_641);
        assert_eq!(auto_bitrate(2560, 1440, 240), 40_000);
        // The per-pixel floor lifts a low base at large sizes.
        assert_eq!(high_rate_bitrate(8000, 2560, 1440, 180), 22_986);
        assert_eq!(high_rate_bitrate(8000, 1280, 720, 180), 13_856);
        // Never above the protocol maximum, never below the base.
        assert_eq!(auto_bitrate(7680, 4320, 240), 100_000);
        assert_eq!(high_rate_bitrate(100_000, 320, 180, 240), 100_000);
        assert_eq!(refresh_hz(179_960), 180);
        assert_eq!(refresh_hz(59_940), 60);
        assert_eq!(refresh_hz(0), 0);
    }
}
