use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use std::{path::Path, time::Duration};

pub const VERSION: u32 = 2;
pub const WIRE_VERSION: &str = "moq-lite-05";
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
}

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    Motion { x: f64, y: f64 },
    Button { button: u8, down: bool },
    Key { code: u16, down: bool },
    Scroll { x: i32, y: i32 },
    ReleaseAll,
    Ping,
    Probe { id: u64 },
    ConfigureVideo { width: u32, fps: u32, bitrate: u32 },
    SelectMonitor { index: usize },
    Feedback { queue_ms: u32, dropped_groups: u32 },
    Clipboard { text: String },
    ClipboardRequest,
}

impl Event {
    pub fn validate(&self) -> Result<()> {
        match self {
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
            Self::Scroll { x, y } => ensure!(
                x.abs_diff(0) <= 20 && y.abs_diff(0) <= 20,
                "scroll out of range"
            ),
            Self::SelectMonitor { index } => ensure!(*index < 64, "invalid monitor index"),
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
    fn old_metadata_defaults_to_h264_without_new_capabilities() {
        let old = serde_json::json!({ "version": 2, "width": 1280, "height": 720, "fps": 60,
            "source": "test", "monitors": [], "active_monitor": 0, "audio": false, "clipboard": false });
        let desktop: Desktop = serde_json::from_value(old.clone()).unwrap();
        assert_eq!(desktop.codec, VideoCodec::H264);
        assert!(!desktop.configurable_video && !desktop.telemetry);
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
    }
}
