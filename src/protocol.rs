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
}

impl Pairing {
    pub fn read(path: &Path) -> Result<Self> {
        let pairing: Self = serde_json::from_slice(&std::fs::read(path)?)?;
        pairing.validate()?;
        Ok(pairing)
    }

    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.token.len() == 64 && self.token.bytes().all(|b| b.is_ascii_hexdigit()),
            "invalid pairing token"
        );
        moq_native::tls::parse_fingerprint(&self.fingerprint)?;
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
    /// Host accepts the `Gamepad*` events and backs them with virtual pads.
    /// Older hosts reject unknown events, so clients must check this first.
    #[serde(default)]
    pub gamepads: bool,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct Monitor {
    pub id: usize,
    pub name: String,
    pub width: u32,
    pub height: u32,
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
    },
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
                    && (1..=120).contains(fps)
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
            Self::Button { button, .. } => ensure!((1..=3).contains(button), "unsupported button"),
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
            Self::GamepadConnected { pad, name } => ensure!(
                *pad < MAX_GAMEPADS
                    && name.len() <= MAX_GAMEPAD_NAME
                    && !name.chars().any(char::is_control),
                "invalid gamepad slot or name"
            ),
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

/// Read untrusted MoQ frames with an application-level size limit.
pub async fn read_frame(
    group: &mut moq_net::group::Consumer,
    limit: usize,
) -> Result<Option<bytes::Bytes>> {
    let Some(mut frame) = group.next_frame().await? else {
        return Ok(None);
    };
    ensure!(frame.size <= limit as u64, "frame exceeds {limit} bytes");
    Ok(Some(frame.read_all().await?))
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
        assert_eq!(desktop.video_start_group, 0);
        let mut invalid = old;
        invalid["codec"] = serde_json::json!("surprise");
        assert!(serde_json::from_value::<Desktop>(invalid).is_err());
    }
    #[test]
    fn input_bounds() {
        assert!(
            Event::ConfigureVideo {
                width: 0,
                fps: 120,
                bitrate: 100_000
            }
            .validate()
            .is_ok()
        );
        for (width, fps, bitrate) in [
            (319, 60, 8000),
            (7681, 60, 8000),
            (1920, 0, 8000),
            (1920, 121, 8000),
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
}
