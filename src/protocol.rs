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
    /// Only to hosts advertising `Desktop::av1`: older peers reject the
    /// name, so it never appears in `Desktop::codecs` or `hardware_codecs`.
    Av1,
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

/// The controller a host pad emulates (docs/gamepad.md). `Xbox` is a uinput
/// Xbox 360 pad; the PlayStation types are `/dev/uhid` HID devices with
/// Sony's report descriptors, driven by the kernel's `hid-playstation`.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, Default, PartialEq, Eq, clap::ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum GamepadType {
    #[default]
    Xbox,
    /// DualShock 4 (`054c:05c4`): touchpad, motion, lightbar.
    Ds4,
    /// DualSense (`054c:0ce6`): touchpad, motion, lightbar, player LEDs,
    /// adaptive triggers.
    #[serde(rename = "dualsense")]
    #[value(name = "dualsense")]
    DualSense,
}

impl GamepadType {
    pub fn is_xbox(&self) -> bool {
        *self == Self::Xbox
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Xbox => "Xbox 360",
            Self::Ds4 => "DualShock 4",
            Self::DualSense => "DualSense",
        }
    }
}

/// Desktop audio encoding (parsec-comparison G21).
#[derive(Serialize, Deserialize, Debug, Clone, Copy, Default, PartialEq, Eq, clap::ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum AudioFormat {
    /// 10 ms Opus packets at the chosen bitrate.
    #[default]
    Opus,
    /// Uncompressed 48 kHz stereo S16LE, ~1.5 Mbit/s, for unconstrained links
    /// (Parsec's `network_raw_audio`): [`PCM_FRAME_BYTES`] per group.
    Pcm,
}

impl AudioFormat {
    /// The host track carrying this format.
    pub fn track(self) -> &'static str {
        match self {
            Self::Opus => "opus",
            Self::Pcm => "pcm",
        }
    }
}

/// Which pad types a host creates (`Desktop::gamepads` and
/// `Desktop::gamepad_types`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct GamepadOffer {
    pub xbox: bool,
    pub ds4: bool,
    pub dualsense: bool,
}

impl GamepadOffer {
    pub fn any(self) -> bool {
        self.xbox || self.ds4 || self.dualsense
    }

    pub fn offers(self, kind: GamepadType) -> bool {
        match kind {
            GamepadType::Xbox => self.xbox,
            GamepadType::Ds4 => self.ds4,
            GamepadType::DualSense => self.dualsense,
        }
    }

    /// What a pad that wants `kind` becomes: the Xbox pad when the host does
    /// not offer it.
    pub fn resolve(self, kind: GamepadType) -> GamepadType {
        if self.offers(kind) {
            kind
        } else {
            GamepadType::Xbox
        }
    }
}

/// Opus bitrates offered, in kbit/s (510 is Opus's maximum).
pub const AUDIO_BITRATES: [u32; 6] = [64, 96, 128, 192, 256, 510];
pub const DEFAULT_AUDIO_BITRATE: u32 = 128;
/// One 10 ms PCM frame: 480 samples × 2 channels × 2 bytes.
pub const PCM_FRAME_BYTES: usize = 480 * 2 * 2;
/// The PCM payload rate, kbit/s.
pub const PCM_KBPS: u32 = 48_000 * 2 * 16 / 1000;

/// `--audio-bitrate`: one of [`AUDIO_BITRATES`].
pub fn parse_audio_bitrate(value: &str) -> Result<u32, String> {
    value
        .parse()
        .ok()
        .filter(|kbps| AUDIO_BITRATES.contains(kbps))
        .ok_or_else(|| format!("expected one of {AUDIO_BITRATES:?} (kbit/s)"))
}

/// A session's desktop audio: Opus at `bitrate` kbit/s, or PCM.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub struct AudioSettings {
    pub format: AudioFormat,
    /// Opus bitrate in kbit/s, one of [`AUDIO_BITRATES`]; unused for PCM.
    pub bitrate: u32,
}

impl Default for AudioSettings {
    fn default() -> Self {
        Self {
            format: AudioFormat::Opus,
            bitrate: DEFAULT_AUDIO_BITRATE,
        }
    }
}

impl AudioSettings {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            AUDIO_BITRATES.contains(&self.bitrate),
            "unsupported audio bitrate"
        );
        Ok(())
    }

    pub fn label(&self) -> String {
        match self.format {
            AudioFormat::Opus => format!("Opus {} kbps", self.bitrate),
            AudioFormat::Pcm => format!(
                "PCM 48 kHz stereo ({:.1} Mbps)",
                f64::from(PCM_KBPS) / 1000.0
            ),
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
    pub const ALL: [Self; 3] = [Self::H264, Self::H265, Self::Av1];
    pub fn track(self) -> &'static str {
        match self {
            Self::H264 => "h264",
            Self::H265 => "h265",
            Self::Av1 => "av1",
        }
    }
    pub fn label(self) -> &'static str {
        match self {
            Self::H264 => "H.264",
            Self::H265 => "H.265",
            Self::Av1 => "AV1",
        }
    }
    /// `tp_codec_open`'s codec number.
    pub fn ffmpeg_id(self) -> i32 {
        match self {
            Self::H264 => 0,
            Self::H265 => 1,
            Self::Av1 => 2,
        }
    }
    /// Codecs that can carry 4:4:4 here (`SelectCodec::chroma_444`).
    pub fn supports_444(self) -> bool {
        self != Self::Av1
    }
    /// Codecs that can carry 10-bit SDR here (`SelectCodec::ten_bit`): no
    /// hardware encodes or decodes H.264 High 10.
    pub fn supports_ten_bit(self) -> bool {
        self != Self::H264
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
    /// Pad types `GamepadConnected::kind` may ask for, with `gamepads`.
    /// Hosts listing a PlayStation type also accept `GamepadTouch` and
    /// `GamepadMotion`. Empty for older hosts: Xbox pads only.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub gamepad_types: Vec<GamepadType>,
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
    /// Empty for older and software-only hosts. H.264 and H.265 only: older
    /// clients reject other names (see `hardware_av1`).
    #[serde(default)]
    pub hardware_codecs: Vec<VideoCodec>,
    /// Host encodes `VideoCodec::Av1` when `SelectCodec` asks (in hardware,
    /// or with SVT-AV1); older hosts reject the name.
    #[serde(default)]
    pub av1: bool,
    /// Host encodes AV1 in hardware.
    #[serde(default)]
    pub hardware_av1: bool,
    /// Host applies `SelectCodec::chroma_444` (H.264/H.265 4:4:4).
    #[serde(default)]
    pub chroma_444_selection: bool,
    /// Codecs the host encodes 4:4:4 in hardware.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub hardware_444: Vec<VideoCodec>,
    /// The session's video is 4:4:4 (full-resolution chroma).
    #[serde(default)]
    pub chroma_444: bool,
    /// Host applies `SelectCodec::ten_bit` (H.265/AV1 Main 10 SDR).
    #[serde(default)]
    pub ten_bit_selection: bool,
    /// Codecs the host encodes as Main 10 in hardware.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub hardware_ten_bit: Vec<VideoCodec>,
    /// The session's video is 10-bit SDR (Main 10, BT.709).
    #[serde(default)]
    pub ten_bit: bool,
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
    /// Host stamps video frames with [`FrameTiming`] and answers probes with
    /// its clock (`Update::Pong::host_us`) once the client declares
    /// `ClientFeatures::frame_timing`; older hosts do neither.
    #[serde(default)]
    pub frame_timing: bool,
    /// Host adds a virtual monitor sized to the client display when
    /// `ClientFeatures::virtual_monitor` asks (docs/virtual-display.md).
    #[serde(default)]
    pub virtual_monitor: bool,
    /// Host turns its physical monitors off for the session when
    /// `ClientFeatures::privacy` asks (or always, by host setting).
    #[serde(default)]
    pub privacy_mode: bool,
    /// Privacy mode is on: the host's physical monitors are off and the
    /// session streams a virtual output.
    #[serde(default)]
    pub privacy_active: bool,
    /// Host shares the desktop between several sessions (docs/guests.md):
    /// it sends `Update::Access` (and, to owners, `Update::GuestRequest`)
    /// once the client declares `ClientFeatures::guests`, and accepts
    /// `Event::TakeControl` and `Event::ApproveGuest`. Older hosts reject
    /// those events.
    #[serde(default)]
    pub guests: bool,
    /// This session's role and permissions, from hosts with `guests`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub access: Option<SessionAccess>,
    /// Another session already chose the stream: the client takes `codec`
    /// as it is (one encoder is shared), even when choosing automatically,
    /// and does not send `SelectCodec`.
    #[serde(default)]
    pub shared_stream: bool,
    /// Host plays the client's microphone into a virtual source ("Teleport
    /// Microphone") once the client declares `ClientFeatures::microphone`
    /// and publishes [`MICROPHONE_TRACK`] (host `--microphone`).
    #[serde(default)]
    pub microphone: bool,
    /// Host applies `SelectCodec::audio` (bitrate, PCM) and publishes both
    /// audio tracks; older hosts send Opus at 128 kbit/s on `opus` only.
    #[serde(default)]
    pub audio_options: bool,
    /// The host's desktop audio settings for clients that do not choose.
    #[serde(default)]
    pub audio_settings: AudioSettings,
    /// How to wake this host and which power requests it takes (G23); the
    /// client saves it in the desktop's profile. Absent from older hosts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wake: Option<WakeInfo>,
    /// The host maps `Event::SecureAttention` (the client's Ctrl+Alt+End)
    /// to locking its session, or to the unlock prompt when it is locked.
    #[serde(default)]
    pub secure_attention: bool,
    /// Host injects `Event::ScrollSmooth` (trackpad pixel scrolling with a
    /// gesture end); older hosts reject it, so clients keep whole-notch
    /// `Scroll` events for them.
    #[serde(default)]
    pub smooth_scroll: bool,
}

/// What a host tells its paired clients about waking it and its power
/// requests (`Desktop::wake`, G23), saved in the client's profile. Parsing
/// and checks are in `crate::wake`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WakeInfo {
    /// Hardware address of the interface the host is reached on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mac: Option<String>,
    /// Wake-on-LAN by magic packet is armed on that interface (`ethtool`
    /// "Wake-on: g"); `None` when the host could not tell.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wake_on_lan: Option<bool>,
    /// The interface's IPv4 address and prefix: its subnet broadcast, and
    /// which wake relays share its LAN.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ipv4: Option<String>,
    /// This host sends magic packets for its owner (`--wake-relay`).
    #[serde(default)]
    pub wake_relay: bool,
    /// This host suspends at its owner's request (`--allow-suspend`).
    #[serde(default)]
    pub suspend: bool,
}

/// What a guest may do besides watching (docs/guests.md). Owners may do
/// everything; a guest starts with [`Permissions::GUEST_DEFAULT`].
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Permissions {
    /// Its own virtual pads (a block of [`MAX_GAMEPADS`] host slots).
    #[serde(default)]
    pub gamepad: bool,
    /// Keyboard and mouse, while it holds control (one session at a time).
    #[serde(default)]
    pub keyboard_mouse: bool,
    /// Explicit clipboard transfer (only on hosts started with `--clipboard`).
    #[serde(default)]
    pub clipboard: bool,
    /// Its microphone into the host's virtual microphone (host `--microphone`;
    /// one session at a time). Off by default: host applications hear it as
    /// the host user.
    #[serde(default)]
    pub microphone: bool,
}

impl Permissions {
    /// Least privilege for a new guest: view plus its own gamepads.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub const GUEST_DEFAULT: Self = Self {
        gamepad: true,
        keyboard_mouse: false,
        clipboard: false,
        microphone: false,
    };
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub const ALL: Self = Self {
        gamepad: true,
        keyboard_mouse: true,
        clipboard: true,
        microphone: true,
    };

    /// `view`, `gamepad`, `keyboard-mouse`, `clipboard`, `microphone`, comma
    /// separated (`view` alone grants nothing beyond watching).
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub fn parse(text: &str) -> Result<Self, String> {
        let mut permissions = Self::default();
        for item in text
            .split(',')
            .map(str::trim)
            .filter(|item| !item.is_empty())
        {
            match item {
                "view" => (),
                "gamepad" => permissions.gamepad = true,
                "keyboard-mouse" => permissions.keyboard_mouse = true,
                "clipboard" => permissions.clipboard = true,
                "microphone" => permissions.microphone = true,
                _ => {
                    return Err(format!(
                        "unknown permission {item:?}; use view, gamepad, keyboard-mouse, clipboard, microphone"
                    ));
                }
            }
        }
        Ok(permissions)
    }

    /// `view, gamepad, keyboard and mouse, clipboard, microphone`: what is
    /// granted.
    pub fn label(&self) -> String {
        let mut parts = vec!["view"];
        if self.gamepad {
            parts.push("gamepad");
        }
        if self.keyboard_mouse {
            parts.push("keyboard and mouse");
        }
        if self.clipboard {
            parts.push("clipboard");
        }
        if self.microphone {
            parts.push("microphone");
        }
        parts.join(", ")
    }
}

/// Who a session is to the host (docs/guests.md).
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    /// The host's own devices (pairing code, password, Linux login, the
    /// host pairing file): full control, no approval.
    Owner,
    /// A device enrolled through an invite: what it was granted.
    Guest,
}

impl Role {
    pub fn label(self) -> &'static str {
        match self {
            Self::Owner => "owner",
            Self::Guest => "guest",
        }
    }
}

/// Who drives the host's keyboard and mouse.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum ControlHolder {
    /// Nobody: the next permitted session to type or move takes it.
    #[default]
    Nobody,
    /// This session.
    You,
    /// An owner's session.
    Owner,
    /// Another guest.
    Guest,
    /// Someone at the host took it back (host settings or `host-admin`).
    Host,
}

impl ControlHolder {
    pub fn label(self) -> &'static str {
        match self {
            Self::Nobody => "nobody",
            Self::You => "you",
            Self::Owner => "the owner",
            Self::Guest => "another guest",
            Self::Host => "the host",
        }
    }
}

/// A session's role, effective permissions and the keyboard/mouse holder.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub struct SessionAccess {
    pub role: Role,
    pub permissions: Permissions,
    #[serde(default)]
    pub control: ControlHolder,
    /// Other sessions watching the same desktop.
    #[serde(default)]
    pub others: u32,
}

impl SessionAccess {
    /// One line for F8.
    pub fn label(&self) -> String {
        format!(
            "{}: {}   |   keys/mouse: {}   |   {} other session(s)",
            self.role.label(),
            self.permissions.label(),
            self.control.label(),
            self.others
        )
    }
}

/// Longest device name shown in an approval prompt or session list.
pub const MAX_DEVICE_NAME: usize = 128;

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

    /// The pad types this host creates; nothing without `gamepads`.
    pub fn gamepad_offer(&self) -> GamepadOffer {
        if !self.gamepads {
            return GamepadOffer::default();
        }
        GamepadOffer {
            xbox: true,
            ds4: self.gamepad_types.contains(&GamepadType::Ds4),
            dualsense: self.gamepad_types.contains(&GamepadType::DualSense),
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
    /// A virtual output the compositor created for this host (headless
    /// fallback, privacy mode or an extra monitor); it exists only while the
    /// host streams it. Older hosts omit it.
    #[serde(default)]
    pub virtual_display: bool,
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
        /// The host's [`clock_us`] when it answered, only to clients that
        /// declared `ClientFeatures::frame_timing` (clock-offset estimate).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        host_us: Option<u64>,
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
    /// The lightbar colour and player LEDs a game (or the host kernel) set
    /// on PlayStation pad `pad`. `player` is the DualSense's five player
    /// LEDs, left to right in bits 0..=4 (0 for a DualShock 4). Only sent
    /// after the client declared `ClientFeatures::gamepad_lights`.
    GamepadLights {
        pad: u8,
        red: u8,
        green: u8,
        blue: u8,
        #[serde(default)]
        player: u8,
    },
    /// DualSense adaptive-trigger effects for pad `pad`: the 11-byte effect
    /// blocks of the right and left trigger exactly as in the controller's
    /// output report (mode byte first). Only sent after the client declared
    /// `ClientFeatures::gamepad_lights`.
    GamepadTriggers {
        pad: u8,
        right: [u8; TRIGGER_EFFECT_LEN],
        left: [u8; TRIGGER_EFFECT_LEN],
    },
    /// This session's role, permissions or the keyboard/mouse holder
    /// changed. Only sent after the client declared `ClientFeatures::guests`
    /// (a `Notice` explains the change to every client).
    Access {
        access: SessionAccess,
    },
    /// A guest device asks to join; an owner's client may answer with
    /// `Event::ApproveGuest` within `timeout_s` (the host denies after it).
    /// Only to owners that declared `ClientFeatures::guests`.
    GuestRequest {
        id: u64,
        name: String,
        timeout_s: u32,
    },
    /// Request `id` was answered (here or elsewhere) or timed out.
    GuestRequestClosed {
        id: u64,
    },
}

/// Bytes of one DualSense adaptive-trigger effect.
pub const TRIGGER_EFFECT_LEN: usize = 11;

/// The DualSense effects block (`DS5EffectsState_t` in SDL, the output
/// report's common part) that plays `right` and `left` trigger effects and
/// nothing else; `SDL_GameControllerSendEffect` takes it for a PS5 pad.
pub fn dualsense_trigger_effect(
    right: &[u8; TRIGGER_EFFECT_LEN],
    left: &[u8; TRIGGER_EFFECT_LEN],
) -> [u8; 47] {
    let mut effect = [0u8; 47];
    // Right and left trigger effects valid.
    effect[0] = 0x04 | 0x08;
    effect[10..21].copy_from_slice(right);
    effect[21..32].copy_from_slice(left);
    effect
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
            Self::GamepadLights { pad, player, .. } => ensure!(
                *pad < MAX_GAMEPADS && *player < 0x20,
                "invalid gamepad lights"
            ),
            Self::GamepadTriggers { pad, .. } => {
                ensure!(*pad < MAX_GAMEPADS, "invalid gamepad triggers")
            }
            Self::GuestRequest {
                name, timeout_s, ..
            } => ensure!(
                name.len() <= MAX_DEVICE_NAME
                    && !name.chars().any(char::is_control)
                    && *timeout_s <= 3600,
                "invalid guest request"
            ),
            Self::Access { access } => ensure!(access.others < 1024, "invalid access update"),
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

/// Client→host track carrying the client's microphone: one 10 ms mono Opus
/// packet per group, like desktop audio.
pub const MICROPHONE_TRACK: &str = "microphone";

/// Largest per-event pointer delta; clients split larger movements.
pub const MAX_RELATIVE_MOTION: u32 = 4096;

/// Largest per-axis `ScrollSmooth` delta in host pixels; clients split
/// larger bursts.
pub const MAX_SMOOTH_SCROLL: f64 = 4000.0;

/// Side mouse buttons in X11 numbering (SDL X1/X2, evdev BTN_SIDE/BTN_EXTRA).
pub const BUTTON_BACK: u8 = 8;
pub const BUTTON_FORWARD: u8 = 9;

/// Lowest video bitrate (kbit/s) the host accepts or targets.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub const MIN_BITRATE: u32 = 500;

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
/// Simultaneous touchpad contacts a PlayStation pad reports.
pub const TOUCHPAD_FINGERS: u8 = 2;
/// Bound for `GamepadMotion` values (SDL units: rad/s and m/s²); real pads
/// saturate far below it (about 35 rad/s and 39 m/s²).
pub const MAX_MOTION: f32 = 1000.0;

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
    /// Whole wheel notches, SDL signs: +y scrolls up, +x right.
    Scroll {
        x: i32,
        y: i32,
    },
    /// Trackpad scrolling in host logical pixels, with `Scroll`'s signs.
    /// `finish` ends the gesture (host applications may then continue with
    /// kinetic scrolling). Only to hosts advertising `Desktop::smooth_scroll`.
    ScrollSmooth {
        dx: f64,
        dy: f64,
        #[serde(default)]
        finish: bool,
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
        /// The pad to emulate; only a type in `Desktop::gamepad_types` (older
        /// hosts ignore the field and create an Xbox pad).
        #[serde(default, skip_serializing_if = "GamepadType::is_xbox")]
        kind: GamepadType,
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
    /// Finger `finger` (< [`TOUCHPAD_FINGERS`]) on a PlayStation pad's
    /// touchpad, normalised like SDL (0..=1, origin top left); `down: false`
    /// lifts it. Coalesced and retried like `GamepadAxis`. Only for pads
    /// connected with a PlayStation `kind`.
    GamepadTouch {
        pad: u8,
        finger: u8,
        down: bool,
        x: f32,
        y: f32,
    },
    /// A PlayStation pad's motion sensors in SDL's units and axes: `gyro`
    /// in rad/s (pitch, yaw, roll), `accel` in m/s². Latest wins (the client
    /// averages the gyro since its last send). Only for pads connected with
    /// a PlayStation `kind`.
    GamepadMotion {
        pad: u8,
        gyro: [f32; 3],
        accel: [f32; 3],
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
        /// The client strips a [`FrameTiming`] header from video frames and
        /// reads `Update::Pong::host_us`. Only sent to hosts advertising
        /// `Desktop::frame_timing`; older hosts ignore the field.
        #[serde(default)]
        frame_timing: bool,
        /// Turn the host's physical monitors off while this session lasts
        /// (privacy mode). Only sent when `Desktop::privacy_mode`; older hosts
        /// ignore the field.
        #[serde(default)]
        privacy: bool,
        /// Add a virtual monitor sized to this client's display, streamed as
        /// an extra monitor. Only sent when `Desktop::virtual_monitor`.
        #[serde(default)]
        virtual_monitor: bool,
        /// The client plays `Update::GamepadLights` and
        /// `Update::GamepadTriggers` for PlayStation pads. Older hosts ignore
        /// the field; older clients never declare it.
        #[serde(default)]
        gamepad_lights: bool,
        /// The client sends its microphone on [`MICROPHONE_TRACK`]. Only
        /// sent when `Desktop::microphone`; older hosts ignore the field.
        #[serde(default)]
        microphone: bool,
        /// The client reads `Update::Access`, `Update::GuestRequest` and
        /// `Update::GuestRequestClosed` (docs/guests.md). Only sent to hosts
        /// advertising `Desktop::guests`; older hosts ignore the field.
        #[serde(default)]
        guests: bool,
        /// The client reads `Desktop::chroma_444` and `Desktop::ten_bit` and
        /// decodes AV1, 4:4:4 and 10-bit SDR (or leaves when it cannot), so
        /// it may join a shared stream in those formats; hosts remove older
        /// clients from one (docs/guests.md). Older hosts ignore the field.
        #[serde(default)]
        video_formats: bool,
    },
    /// "Secure attention" (the client's Ctrl+Alt+End, Teleport's
    /// Ctrl+Alt+Del): lock the host session, or show the lock screen's
    /// password prompt when it is locked. Only sent when
    /// `Desktop::secure_attention`.
    SecureAttention,
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
        /// Desktop audio for this session, only to hosts advertising
        /// `Desktop::audio_options`; `None` (older clients) is Opus at the
        /// host's bitrate.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        audio: Option<AudioSettings>,
        /// Full-resolution chroma (4:4:4, H.264/H.265 SDR), only to hosts
        /// advertising `Desktop::chroma_444_selection`.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        chroma_444: bool,
        /// Main 10 from 8-bit capture (H.265/AV1 SDR, not with 4:4:4), only
        /// to hosts advertising `Desktop::ten_bit_selection`.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        ten_bit: bool,
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
    /// An owner takes keyboard and mouse control and keeps it from guests
    /// (`take: true`), or lets guests take it again (`take: false`). Only
    /// to hosts advertising `Desktop::guests`; refused from guests.
    TakeControl {
        take: bool,
    },
    /// An owner answers `Update::GuestRequest` `id`. Only to hosts
    /// advertising `Desktop::guests`; refused from guests.
    ApproveGuest {
        id: u64,
        allow: bool,
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
                | Self::ScrollSmooth { .. }
                | Self::Feedback { .. }
                | Self::NetworkFeedback { .. }
                | Self::MonitorFeedback { .. }
                | Self::Probe { .. }
                | Self::Ping
                | Self::RequestKeyframe { .. }
                | Self::GamepadAxis { .. }
                | Self::GamepadTouch { .. }
                | Self::GamepadMotion { .. }
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
            Self::ScrollSmooth { dx, dy, .. } => ensure!(
                dx.is_finite()
                    && dy.is_finite()
                    && dx.abs() <= MAX_SMOOTH_SCROLL
                    && dy.abs() <= MAX_SMOOTH_SCROLL,
                "smooth scroll out of range"
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
            Self::GamepadTouch {
                pad, finger, x, y, ..
            } => ensure!(
                *pad < MAX_GAMEPADS
                    && *finger < TOUCHPAD_FINGERS
                    && (0.0..=1.0).contains(x)
                    && (0.0..=1.0).contains(y),
                "invalid gamepad touch"
            ),
            Self::GamepadMotion { pad, gyro, accel } => ensure!(
                *pad < MAX_GAMEPADS
                    && gyro
                        .iter()
                        .chain(accel)
                        .all(|value| value.is_finite() && value.abs() <= MAX_MOTION),
                "invalid gamepad motion"
            ),
            Self::GamepadConnected { pad, name, .. } => ensure!(
                *pad < MAX_GAMEPADS
                    && name.len() <= MAX_GAMEPAD_NAME
                    && !name.chars().any(char::is_control),
                "invalid gamepad slot or name"
            ),
            Self::ClientDisplay(display) => display.validate()?,
            Self::SelectCodec {
                codec,
                audio,
                chroma_444,
                ten_bit,
                ..
            } => {
                ensure!(
                    !chroma_444 || codec.supports_444(),
                    "4:4:4 needs H.264 or H.265"
                );
                ensure!(
                    !ten_bit || (codec.supports_ten_bit() && !chroma_444),
                    "10-bit SDR needs H.265 or AV1 without 4:4:4"
                );
                if let Some(audio) = audio {
                    audio.validate()?;
                }
            }
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

/// Whether an encoded frame is one where a decoder can start: an IDR (H.264)
/// or IRAP (H.265) picture in an Annex B access unit, decided by its first
/// slice NAL unit (only the parameter sets in front of it are scanned), or
/// an AV1 temporal unit with a sequence header and a shown key frame.
/// Encoders with intra refresh also flag refresh starts as key frames; those
/// are not.
pub fn keyframe(codec: VideoCodec, data: &[u8]) -> bool {
    if codec == VideoCodec::Av1 {
        return av1_keyframe(data);
    }
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
            VideoCodec::Av1 => unreachable!(),
        }
        index += 3;
    }
    false
}

/// An AV1 temporal unit in the low-overhead OBU format FFmpeg's encoders
/// emit (AV1 spec 5.3): a sequence header, then a frame (header) OBU with
/// `show_existing_frame` 0, `frame_type` KEY_FRAME and `show_frame` 1. Every
/// encoder here repeats the sequence header in front of key frames; a unit
/// without one cannot start a decoder. Frame headers are read without
/// `reduced_still_picture_header`, which video streams never set.
fn av1_keyframe(data: &[u8]) -> bool {
    let mut index = 0;
    let mut sequence_header = false;
    while index < data.len() {
        let header = data[index];
        if header & 0x80 != 0 {
            return false;
        }
        let kind = (header >> 3) & 0x0f;
        index += 1 + usize::from(header & 0x04 != 0);
        let size = if header & 0x02 != 0 {
            // leb128 size field
            let mut size = 0_usize;
            let mut shift = 0;
            loop {
                let Some(&byte) = data.get(index) else {
                    return false;
                };
                index += 1;
                size |= usize::from(byte & 0x7f) << shift;
                if byte & 0x80 == 0 {
                    break;
                }
                shift += 7;
                if shift > 28 {
                    return false;
                }
            }
            size
        } else {
            data.len().saturating_sub(index)
        };
        let Some(payload) = index.checked_add(size).and_then(|end| data.get(index..end)) else {
            return false;
        };
        match kind {
            1 => sequence_header = true,
            // OBU_FRAME_HEADER, OBU_FRAME
            3 | 6 => {
                let Some(&first) = payload.first() else {
                    return false;
                };
                return sequence_header
                    && first & 0x80 == 0
                    && (first >> 5) & 0x03 == 0
                    && first & 0x10 != 0;
            }
            _ => (),
        }
        index += size;
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

/// Microseconds on this process's monotonic clock from its first use, which
/// `main` makes the process start (earlier instants would all read 0).
/// Hosts stamp [`FrameTiming`] and `Update::Pong::host_us` with it; clients
/// map those onto their own clock with an NTP-style offset estimate
/// (`crate::stats::ClockSync`). Never a wall clock.
pub fn clock_us(at: std::time::Instant) -> u64 {
    static ORIGIN: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
    let origin = *ORIGIN.get_or_init(std::time::Instant::now);
    at.saturating_duration_since(origin).as_micros() as u64
}

/// Host-side times of one video frame on the host's [`clock_us`]: captured
/// (the source's timestamp), out of the encoder, and handed to MoQ.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameTiming {
    pub capture_us: u64,
    pub encoded_us: u64,
    pub sent_us: u64,
}

/// Starts a [`FrameTiming`] header. Annex B access units always start with
/// a zero byte, so a frame starting with this is never video.
const FRAME_TIMING_MAGIC: [u8; 4] = *b"TPT1";
const FRAME_TIMING_LEN: usize = 4 + 3 * 8;

impl FrameTiming {
    /// `payload` with this header in front (`ClientFeatures::frame_timing`).
    pub fn prepend(&self, payload: &[u8]) -> bytes::Bytes {
        let mut out = Vec::with_capacity(FRAME_TIMING_LEN + payload.len());
        out.extend_from_slice(&FRAME_TIMING_MAGIC);
        for value in [self.capture_us, self.encoded_us, self.sent_us] {
            out.extend_from_slice(&value.to_le_bytes());
        }
        out.extend_from_slice(payload);
        out.into()
    }

    /// Splits the header off a received frame; frames without one come back
    /// unchanged. Inconsistent times (not capture ≤ encoded ≤ sent) are
    /// dropped with the header.
    pub fn split(frame: bytes::Bytes) -> (Option<Self>, bytes::Bytes) {
        if frame.len() < FRAME_TIMING_LEN || frame[..4] != FRAME_TIMING_MAGIC {
            return (None, frame);
        }
        let value = |index: usize| {
            let start = 4 + index * 8;
            u64::from_le_bytes(frame[start..start + 8].try_into().unwrap())
        };
        let timing = Self {
            capture_us: value(0),
            encoded_us: value(1),
            sent_us: value(2),
        };
        let valid = timing.capture_us <= timing.encoded_us && timing.encoded_us <= timing.sent_us;
        (valid.then_some(timing), frame.slice(FRAME_TIMING_LEN..))
    }
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
                frame_timing: true,
                privacy: true,
                virtual_monitor: true,
                gamepad_lights: true,
                microphone: true,
                guests: true,
                video_formats: true,
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
                kind: crate::protocol::GamepadType::Xbox,
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
                kind: crate::protocol::GamepadType::Xbox,
            },
            Event::GamepadConnected {
                pad: 0,
                name: "x".repeat(MAX_GAMEPAD_NAME + 1),
                kind: crate::protocol::GamepadType::Xbox,
            },
            Event::GamepadConnected {
                pad: 0,
                name: "pad\n".into(),
                kind: crate::protocol::GamepadType::Xbox,
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
                kind: crate::protocol::GamepadType::Xbox,
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
                frame_timing: false,
                privacy: false,
                virtual_monitor: false,
                gamepad_lights: false,
                microphone: false,
                guests: false,
                video_formats: false,
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
                audio: None,
                chroma_444: false,
                ten_bit: false,
            }
        ));
        assert!(event.validate().is_ok() && !event.is_lossy());
        assert!(serde_json::from_str::<Event>(r#"{"type":"select_codec","codec":"vp9"}"#).is_err());
        assert!(matches!(
            serde_json::from_str::<Event>(
                r#"{"type":"select_codec","codec":"av1","chroma_444":true}"#
            )
            .unwrap(),
            Event::SelectCodec {
                codec: VideoCodec::Av1,
                chroma_444: true,
                ..
            }
        ));
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
        // Neither AV1 nor 4:4:4 from hosts that predate them.
        assert!(!desktop.av1 && !desktop.hardware_av1);
        assert!(!desktop.chroma_444_selection && !desktop.chroma_444);
        assert!(desktop.hardware_444.is_empty());
        assert!(!desktop.ten_bit_selection && !desktop.ten_bit);
        assert!(desktop.hardware_ten_bit.is_empty());
        assert_eq!(desktop.encoder_preset, EncoderPreset::Latency);
        let chosen = Event::SelectCodec {
            codec: VideoCodec::H265,
            encoder_preset: Some(EncoderPreset::Quality),
            audio: None,
            chroma_444: false,
            ten_bit: true,
        };
        let wire = serde_json::to_string(&chosen).unwrap();
        assert!(wire.contains(r#""encoder_preset":"quality""#), "{wire}");
        assert!(wire.contains(r#""ten_bit":true"#), "{wire}");
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
            audio: None,
            chroma_444: false,
            ten_bit: false,
        })
        .unwrap();
        assert!(!plain.contains("encoder_preset"), "{plain}");
        assert!(!plain.contains("audio"), "{plain}");
        assert!(
            !plain.contains("chroma_444") && !plain.contains("ten_bit"),
            "{plain}"
        );
    }

    #[test]
    fn audio_settings_are_chosen_in_codec_selection_and_bounded() {
        // Older hosts: Opus 128 on the "opus" track, no options.
        let old: Desktop = serde_json::from_value(serde_json::json!({ "version": 2,
            "width": 1, "height": 1, "fps": 1, "source": "test", "monitors": [],
            "active_monitor": 0, "audio": true, "clipboard": false }))
        .unwrap();
        assert!(!old.audio_options);
        assert_eq!(old.audio_settings, AudioSettings::default());
        assert_eq!(old.audio_settings.format.track(), "opus");
        let pcm = AudioSettings {
            format: AudioFormat::Pcm,
            bitrate: DEFAULT_AUDIO_BITRATE,
        };
        let chosen = Event::SelectCodec {
            codec: VideoCodec::H264,
            encoder_preset: None,
            audio: Some(pcm),
            chroma_444: false,
            ten_bit: false,
        };
        let wire = serde_json::to_string(&chosen).unwrap();
        assert!(
            wire.contains(r#""audio":{"format":"pcm","bitrate":128}"#),
            "{wire}"
        );
        let parsed: Event = serde_json::from_str(&wire).unwrap();
        assert!(matches!(parsed, Event::SelectCodec { audio: Some(audio), .. } if audio == pcm));
        assert!(parsed.validate().is_ok());
        assert_eq!(pcm.format.track(), "pcm");
        for bitrate in AUDIO_BITRATES {
            assert_eq!(parse_audio_bitrate(&bitrate.to_string()), Ok(bitrate));
        }
        for bad in ["0", "100", "511", "x"] {
            assert!(parse_audio_bitrate(bad).is_err());
        }
        let odd = Event::SelectCodec {
            codec: VideoCodec::H264,
            encoder_preset: None,
            audio: Some(AudioSettings {
                format: AudioFormat::Opus,
                bitrate: 1000,
            }),
            chroma_444: false,
            ten_bit: false,
        };
        assert!(odd.validate().is_err());
        assert!(
            serde_json::from_str::<AudioSettings>(r#"{"format":"flac","bitrate":128}"#).is_err()
        );
        // 10 ms of 48 kHz stereo S16, ~1.5 Mbit/s.
        assert_eq!(PCM_FRAME_BYTES * 100 * 8 / 1000, PCM_KBPS as usize);
        assert_eq!(pcm.label(), "PCM 48 kHz stereo (1.5 Mbps)");
        assert_eq!(AudioSettings::default().label(), "Opus 128 kbps");
    }

    /// Every codec, preset, audio, 4:4:4 and 10-bit combination: absent
    /// fields stay off the wire (older hosts read the rest), everything
    /// round-trips, and only formats the codec can carry validate.
    #[test]
    fn codec_selection_combinations_have_minimal_wire_forms_and_validate() {
        let audio = AudioSettings {
            format: AudioFormat::Pcm,
            bitrate: DEFAULT_AUDIO_BITRATE,
        };
        for codec in VideoCodec::ALL {
            for encoder_preset in [None, Some(EncoderPreset::Balanced)] {
                for audio in [None, Some(audio)] {
                    for chroma_444 in [false, true] {
                        for ten_bit in [false, true] {
                            let event = Event::SelectCodec {
                                codec,
                                encoder_preset,
                                audio,
                                chroma_444,
                                ten_bit,
                            };
                            let wire = serde_json::to_string(&event).unwrap();
                            assert!(wire.contains(&format!(r#""codec":"{}""#, codec.track())));
                            assert_eq!(wire.contains("encoder_preset"), encoder_preset.is_some());
                            assert_eq!(wire.contains(r#""audio""#), audio.is_some());
                            assert_eq!(wire.contains(r#""chroma_444":true"#), chroma_444);
                            assert_eq!(wire.contains(r#""ten_bit":true"#), ten_bit);
                            assert!(!wire.contains("false"), "{wire}");
                            let parsed: Event = serde_json::from_str(&wire).unwrap();
                            assert!(matches!(
                                parsed,
                                Event::SelectCodec { codec: c, encoder_preset: p, audio: a, chroma_444: x, ten_bit: t }
                                    if c == codec && p == encoder_preset && a == audio && x == chroma_444 && t == ten_bit
                            ));
                            let valid = (!chroma_444 || codec.supports_444())
                                && (!ten_bit || (codec.supports_ten_bit() && !chroma_444));
                            assert_eq!(parsed.validate().is_ok(), valid, "{wire}");
                            assert!(!parsed.is_lossy());
                        }
                    }
                }
            }
        }
        // AV1 4:4:4, H.264 10-bit and 4:4:4 10-bit are refused.
        for wire in [
            r#"{"type":"select_codec","codec":"av1","chroma_444":true}"#,
            r#"{"type":"select_codec","codec":"h264","ten_bit":true}"#,
            r#"{"type":"select_codec","codec":"h265","chroma_444":true,"ten_bit":true}"#,
        ] {
            assert!(
                serde_json::from_str::<Event>(wire)
                    .unwrap()
                    .validate()
                    .is_err()
            );
        }
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
    #[test]
    fn smooth_scroll_roundtrips_bounded_lossy_and_negotiated() {
        let event = Event::ScrollSmooth {
            dx: -1.5,
            dy: 12.25,
            finish: true,
        };
        let text = serde_json::to_string(&event).unwrap();
        assert!(text.contains(r#""type":"scroll_smooth""#), "{text}");
        let back: Event = serde_json::from_str(&text).unwrap();
        assert!(matches!(
            back,
            Event::ScrollSmooth { dx, dy, finish: true } if dx == -1.5 && dy == 12.25
        ));
        assert!(back.validate().is_ok() && back.is_lossy());
        // `finish` may be left out mid-gesture.
        let back: Event =
            serde_json::from_str(r#"{"type":"scroll_smooth","dx":0.0,"dy":3.0}"#).unwrap();
        assert!(matches!(back, Event::ScrollSmooth { finish: false, .. }));
        for (dx, dy) in [
            (f64::NAN, 0.0),
            (0.0, f64::INFINITY),
            (MAX_SMOOTH_SCROLL + 1.0, 0.0),
            (0.0, -MAX_SMOOTH_SCROLL - 1.0),
        ] {
            assert!(
                Event::ScrollSmooth {
                    dx,
                    dy,
                    finish: false
                }
                .validate()
                .is_err()
            );
        }
        let old: Desktop = serde_json::from_value(serde_json::json!({ "version": 2, "width": 1,
            "height": 1, "fps": 1, "source": "test", "monitors": [], "active_monitor": 0,
            "audio": false, "clipboard": false }))
        .unwrap();
        assert!(!old.smooth_scroll, "older hosts reject ScrollSmooth");
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
    fn microphone_is_negotiated_by_both_ends() {
        // Older hosts and clients omit the fields: no microphone.
        let old: Desktop = serde_json::from_value(serde_json::json!({ "version": 2,
            "width": 1, "height": 1, "fps": 1, "source": "test", "monitors": [],
            "active_monitor": 0, "audio": false, "clipboard": false }))
        .unwrap();
        assert!(!old.microphone);
        assert!(matches!(
            serde_json::from_str::<Event>(r#"{"type":"client_features","keyframe_requests":true}"#)
                .unwrap(),
            Event::ClientFeatures {
                microphone: false,
                ..
            }
        ));
        let desktop = Desktop {
            microphone: true,
            ..old
        };
        let wire = serde_json::to_value(&desktop).unwrap();
        assert_eq!(wire["microphone"], true);
        let features: Event = serde_json::from_str(
            r#"{"type":"client_features","keyframe_requests":true,"microphone":true}"#,
        )
        .unwrap();
        assert!(matches!(
            features,
            Event::ClientFeatures {
                microphone: true,
                ..
            }
        ));
        assert!(features.validate().is_ok() && !features.is_lossy());
        assert_ne!(MICROPHONE_TRACK, POINTER_TRACK);
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

    #[test]
    fn frame_timing_header_roundtrips_and_is_negotiated() {
        let timing = FrameTiming {
            capture_us: 1_000,
            encoded_us: 4_500,
            sent_us: 4_600,
        };
        let video = bytes::Bytes::from_static(&[0, 0, 0, 1, 0x65, 0x88]);
        let (parsed, payload) = FrameTiming::split(timing.prepend(&video));
        assert_eq!(parsed, Some(timing));
        assert_eq!(payload, video);
        // Plain Annex B (older hosts, or before the client declared) is untouched.
        assert_eq!(FrameTiming::split(video.clone()), (None, video.clone()));
        // Times out of order are discarded; the payload is still stripped.
        let backwards = FrameTiming {
            capture_us: 5_000,
            ..timing
        };
        assert_eq!(
            FrameTiming::split(backwards.prepend(&video)),
            (None, video.clone())
        );
        // Off unless the host says so; a Pong without host_us still parses
        // and host_us is only on the wire when set.
        let desktop: Desktop = serde_json::from_value(serde_json::json!({ "version": 2,
            "width": 1, "height": 1, "fps": 1, "source": "test", "monitors": [],
            "active_monitor": 0, "audio": false, "clipboard": false }))
        .unwrap();
        assert!(!desktop.frame_timing);
        assert!(matches!(
            serde_json::from_str::<Update>(r#"{"type":"pong","id":3}"#).unwrap(),
            Update::Pong {
                id: 3,
                host_us: None
            }
        ));
        let wire = serde_json::to_string(&Update::Pong {
            id: 4,
            host_us: Some(77),
        })
        .unwrap();
        assert!(matches!(
            serde_json::from_str::<Update>(&wire).unwrap(),
            Update::Pong {
                id: 4,
                host_us: Some(77)
            }
        ));
        let plain = serde_json::to_string(&Update::Pong {
            id: 4,
            host_us: None,
        })
        .unwrap();
        assert!(!plain.contains("host_us"));
    }

    #[test]
    fn guest_access_is_negotiated_and_bounded() {
        assert_eq!(Permissions::parse("view"), Ok(Permissions::default()));
        assert_eq!(
            Permissions::parse("view,gamepad"),
            Ok(Permissions::GUEST_DEFAULT)
        );
        assert_eq!(
            Permissions::parse("gamepad, keyboard-mouse,clipboard,microphone"),
            Ok(Permissions::ALL)
        );
        assert!(Permissions::parse("admin").is_err());
        assert_eq!(
            Permissions::ALL.label(),
            "view, gamepad, keyboard and mouse, clipboard, microphone"
        );
        // Older hosts' metadata has no access; an older client ignores it.
        let old: Desktop = serde_json::from_value(serde_json::json!({ "version": 2,
            "width": 1, "height": 1, "fps": 1, "source": "test", "monitors": [],
            "active_monitor": 0, "audio": false, "clipboard": false }))
        .unwrap();
        assert!(!old.guests && old.access.is_none() && !old.shared_stream);
        // Grants stored before the microphone permission: no microphone.
        let stored: Permissions =
            serde_json::from_str(r#"{"gamepad":true,"keyboard_mouse":true,"clipboard":true}"#)
                .unwrap();
        assert!(!stored.microphone);
        let access = SessionAccess {
            role: Role::Guest,
            permissions: Permissions::GUEST_DEFAULT,
            control: ControlHolder::Owner,
            others: 1,
        };
        let wire = serde_json::to_string(&Update::Access { access }).unwrap();
        assert!(wire.contains(r#""role":"guest""#) && wire.contains(r#""control":"owner""#));
        let back: Update = serde_json::from_str(&wire).unwrap();
        back.validate().unwrap();
        // Prompts carry a bounded, printable name.
        let request = |name: &str| Update::GuestRequest {
            id: 1,
            name: name.into(),
            timeout_s: 30,
        };
        assert!(request("Alice").validate().is_ok());
        assert!(request("Ali\nce").validate().is_err());
        assert!(
            request(&"x".repeat(MAX_DEVICE_NAME + 1))
                .validate()
                .is_err()
        );
        // New events round-trip; older hosts fail closed on them, which is
        // why clients send them only to hosts advertising `guests`.
        for event in [
            Event::TakeControl { take: true },
            Event::ApproveGuest {
                id: 7,
                allow: false,
            },
        ] {
            let json = serde_json::to_string(&event).unwrap();
            let back: Event = serde_json::from_str(&json).unwrap();
            back.validate().unwrap();
            assert!(!back.is_lossy());
        }
        let features: Event =
            serde_json::from_str(r#"{"type":"client_features","guests":true}"#).unwrap();
        assert!(matches!(
            features,
            Event::ClientFeatures { guests: true, .. }
        ));
    }
}
