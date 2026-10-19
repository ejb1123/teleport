//! `/dev/uhid` PlayStation pads: a HID device per pad that the kernel's
//! `hid-playstation` driver binds like a wired DualShock 4 or DualSense.
//!
//! The host writes complete input reports ([`super::ps::PsState`]). A
//! thread per pad answers the driver's feature-report reads (calibration,
//! address, firmware: its probe fails without them), turns output reports
//! from the driver or from games writing the hidraw node into rumble,
//! lightbar, player-LED and trigger-effect feedback for the client, and
//! repeats the current report every [`KEEPALIVE`] like a real pad's
//! continuous stream, so the driver has the pad's state as soon as it has
//! bound.

use super::{FeedbackTarget, Motors, PadFeedback, Rumble, ps};
use crate::protocol::{GamepadType, MAX_RUMBLE_MS, TRIGGER_EFFECT_LEN};
use anyhow::{Context, Result};
use std::{
    io::Write,
    os::fd::AsRawFd,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU32, Ordering},
    },
    time::{Duration, Instant},
};

pub const UHID_PATH: &str = "/dev/uhid";

// enum uhid_event_type
const UHID_DESTROY: u32 = 1;
const UHID_START: u32 = 2;
const UHID_OUTPUT: u32 = 6;
const UHID_GET_REPORT: u32 = 9;
const UHID_GET_REPORT_REPLY: u32 = 10;
const UHID_CREATE2: u32 = 11;
const UHID_INPUT2: u32 = 12;
const UHID_SET_REPORT: u32 = 13;
const UHID_SET_REPORT_REPLY: u32 = 14;
/// enum uhid_report_type
const UHID_FEATURE_REPORT: u8 = 0;
const UHID_DATA_MAX: usize = 4096;
/// `sizeof(struct uhid_event)` from `<linux/uhid.h>` (packed; the union is
/// a little larger than `struct uhid_create2_req`, the largest request).
const UHID_EVENT_SIZE: usize = 4380;
/// Where `struct uhid_create2_req::rd_data` starts.
const CREATE2_DESCRIPTOR: usize = 128 + 64 + 64 + 2 + 2 + 4 * 4;
const BUS_USB: u16 = 0x03;
/// How often the current input report is repeated without new input.
const KEEPALIVE: Duration = Duration::from_millis(100);
/// How often motors left running are re-sent (one update lasts at most
/// [`MAX_RUMBLE_MS`]).
const RUMBLE_REFRESH: Duration = Duration::from_secs(30);

/// Whether this process can create uhid devices.
pub fn uhid_writable() -> bool {
    let path = std::ffi::CString::new(UHID_PATH).expect("static path");
    unsafe { libc::access(path.as_ptr(), libc::R_OK | libc::W_OK) == 0 }
}

fn put_str(target: &mut [u8], text: &str) {
    // NUL-terminated within the field.
    let bytes = &text.as_bytes()[..text.len().min(target.len() - 1)];
    target[..bytes.len()].copy_from_slice(bytes);
}

/// `UHID_CREATE2` for a USB device with `descriptor`.
fn create2_event(name: &str, phys: &str, vendor: u16, product: u16, descriptor: &[u8]) -> Vec<u8> {
    let mut event = vec![0u8; UHID_EVENT_SIZE];
    event[..4].copy_from_slice(&UHID_CREATE2.to_ne_bytes());
    let request = &mut event[4..];
    put_str(&mut request[..128], name);
    put_str(&mut request[128..192], phys);
    // uniq (192..256) stays empty: hid-playstation sets it from the pad's
    // pairing report.
    request[256..258].copy_from_slice(&(descriptor.len() as u16).to_ne_bytes());
    request[258..260].copy_from_slice(&BUS_USB.to_ne_bytes());
    request[260..264].copy_from_slice(&u32::from(vendor).to_ne_bytes());
    request[264..268].copy_from_slice(&u32::from(product).to_ne_bytes());
    request[268..272].copy_from_slice(&0x0100u32.to_ne_bytes()); // version
    // country (272..276) 0
    request[CREATE2_DESCRIPTOR..CREATE2_DESCRIPTOR + descriptor.len()].copy_from_slice(descriptor);
    event
}

fn input2_event(report: &[u8]) -> Vec<u8> {
    let mut event = Vec::with_capacity(6 + report.len());
    event.extend_from_slice(&UHID_INPUT2.to_ne_bytes());
    event.extend_from_slice(&(report.len() as u16).to_ne_bytes());
    event.extend_from_slice(report);
    event
}

fn get_report_reply(id: u32, error: u16, data: &[u8]) -> Vec<u8> {
    let mut event = Vec::with_capacity(12 + data.len());
    event.extend_from_slice(&UHID_GET_REPORT_REPLY.to_ne_bytes());
    event.extend_from_slice(&id.to_ne_bytes());
    event.extend_from_slice(&error.to_ne_bytes());
    event.extend_from_slice(&(data.len() as u16).to_ne_bytes());
    event.extend_from_slice(data);
    event
}

fn set_report_reply(id: u32, error: u16) -> Vec<u8> {
    let mut event = Vec::with_capacity(10);
    event.extend_from_slice(&UHID_SET_REPORT_REPLY.to_ne_bytes());
    event.extend_from_slice(&id.to_ne_bytes());
    event.extend_from_slice(&error.to_ne_bytes());
    event
}

/// A uhid event read from the device, as far as the pad cares.
#[derive(Debug, PartialEq, Eq)]
enum Request {
    Start,
    Output(Vec<u8>),
    GetReport { id: u32, number: u8, kind: u8 },
    SetReport { id: u32 },
    Other,
}

fn parse_event(event: &[u8]) -> Option<Request> {
    let u32_at = |at: usize| Some(u32::from_ne_bytes(event.get(at..at + 4)?.try_into().ok()?));
    let u16_at = |at: usize| Some(u16::from_ne_bytes(event.get(at..at + 2)?.try_into().ok()?));
    Some(match u32_at(0)? {
        UHID_START => Request::Start,
        UHID_OUTPUT => {
            // struct uhid_output_req { data[4096]; size; rtype; }
            let size = usize::from(u16_at(4 + UHID_DATA_MAX)?).min(UHID_DATA_MAX);
            Request::Output(event.get(4..4 + size)?.to_vec())
        }
        UHID_GET_REPORT => Request::GetReport {
            id: u32_at(4)?,
            number: *event.get(8)?,
            kind: *event.get(9)?,
        },
        UHID_SET_REPORT => Request::SetReport { id: u32_at(4)? },
        _ => Request::Other,
    })
}

/// A unique Bluetooth-style address per pad: the kernel refuses a second
/// PlayStation pad with an address it already knows.
fn pad_address(pad: u8) -> [u8; 6] {
    static CREATED: AtomicU32 = AtomicU32::new(0);
    let count = CREATED.fetch_add(1, Ordering::Relaxed);
    let pid = std::process::id();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |time| time.subsec_nanos());
    let mix = pid.wrapping_mul(0x9e37_79b9) ^ nanos ^ count.rotate_left(16);
    let [a, b, c, d] = mix.to_le_bytes();
    // Least significant byte first; the most significant byte is a locally
    // administered unicast prefix.
    [a, b, c, d, pad, 0x7e]
}

/// What the device's thread and the session share: the latest input
/// report, written under the lock so a keepalive never overtakes newer
/// input.
struct Shared {
    file: std::fs::File,
    report: Vec<u8>,
    written: Instant,
}

impl Shared {
    fn write_report(&mut self) -> std::io::Result<()> {
        self.written = Instant::now();
        match self.file.write_all(&input2_event(&self.report)) {
            // Before the device runs (and after it is destroyed) the kernel
            // refuses input; the keepalive sends the state again.
            Err(error) if error.raw_os_error() == Some(libc::EINVAL) => Ok(()),
            other => other,
        }
    }
}

/// A created uhid PlayStation pad; dropping it destroys the device.
pub struct UhidPad {
    shared: Arc<Mutex<Shared>>,
    stop: Arc<AtomicBool>,
    pub phys: String,
}

impl UhidPad {
    pub fn create(
        pad: u8,
        kind: GamepadType,
        initial_report: &[u8],
        target: Arc<Mutex<FeedbackTarget>>,
    ) -> Result<Self> {
        use std::os::unix::fs::OpenOptionsExt;
        let model = ps::model(kind).context("not a PlayStation pad type")?;
        let mut file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_NONBLOCK | libc::O_CLOEXEC)
            .open(UHID_PATH)
            .with_context(|| {
                format!(
                    "opening {UHID_PATH}; see {} for the required permission",
                    crate::docs::page("gamepad.md")
                )
            })?;
        static CREATED: AtomicU32 = AtomicU32::new(0);
        let phys = format!(
            "teleport/{}/pad{}/{}",
            std::process::id(),
            pad + 1,
            CREATED.fetch_add(1, Ordering::Relaxed)
        );
        file.write_all(&create2_event(
            model.name,
            &phys,
            ps::SONY,
            model.product,
            model.descriptor,
        ))
        .context("creating the uhid device")?;
        let reader = file.try_clone()?;
        let shared = Arc::new(Mutex::new(Shared {
            file,
            report: initial_report.to_vec(),
            written: Instant::now(),
        }));
        let stop = Arc::new(AtomicBool::new(false));
        let device = Device {
            reader,
            kind,
            pad,
            address: pad_address(pad),
            shared: shared.clone(),
            target,
            stop: stop.clone(),
        };
        std::thread::Builder::new()
            .name(format!("pad{pad}-uhid"))
            .spawn(move || device.serve())?;
        Ok(Self { shared, stop, phys })
    }

    /// Send a new complete input report.
    pub fn write(&mut self, report: &[u8]) -> Result<()> {
        let mut shared = self.shared.lock().expect("uhid pad lock");
        shared.report = report.to_vec();
        shared
            .write_report()
            .context("writing a PlayStation pad report")
    }
}

impl Drop for UhidPad {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Ok(mut shared) = self.shared.lock() {
            let _ = shared.file.write_all(&UHID_DESTROY.to_ne_bytes());
        }
    }
}

/// The pad's output state as last reported to the client.
#[derive(Default)]
struct Sent {
    motors: Motors,
    motors_at: Option<Instant>,
    lights: Option<([u8; 3], u8)>,
    triggers: Option<([u8; TRIGGER_EFFECT_LEN], [u8; TRIGGER_EFFECT_LEN])>,
}

struct Device {
    reader: std::fs::File,
    kind: GamepadType,
    pad: u8,
    address: [u8; 6],
    shared: Arc<Mutex<Shared>>,
    target: Arc<Mutex<FeedbackTarget>>,
    stop: Arc<AtomicBool>,
}

impl Device {
    fn reply(&self, event: &[u8]) {
        let mut shared = self.shared.lock().expect("uhid pad lock");
        if let Err(error) = shared.file.write_all(event) {
            tracing::warn!(pad = self.pad, %error, "uhid reply failed");
        }
    }

    fn send(&self, feedback: PadFeedback, lights: bool) {
        let target = self.target.lock().expect("feedback lock");
        let wanted = if lights { target.lights } else { target.rumble };
        if wanted && let Some(sender) = &target.sender {
            let _ = sender.send(feedback);
        }
    }

    fn output(&self, report: &[u8], state: &mut State) {
        let Some(output) = ps::parse_output(self.kind, report) else {
            tracing::debug!(pad = self.pad, id = report.first(), "ignored output report");
            return;
        };
        if let Some((strong, weak)) = output.motors {
            state.motors = (strong, weak);
        }
        if let Some(rgb) = output.lightbar {
            state.lightbar = rgb;
            state.lights_set = true;
        }
        if let Some(player) = output.player {
            state.player = player;
            state.lights_set = true;
        }
        if let Some(right) = output.right_trigger {
            state.triggers.0 = right;
            state.triggers_set = true;
        }
        if let Some(left) = output.left_trigger {
            state.triggers.1 = left;
            state.triggers_set = true;
        }
        tracing::debug!(pad = self.pad, ?output, "pad output report");
    }

    /// Report what changed since the last update.
    fn report(&self, state: &State, sent: &mut Sent, now: Instant) {
        let (strong, weak) = state.motors;
        let motors = Motors {
            low: u16::from(strong) * 257,
            high: u16::from(weak) * 257,
            // The pad keeps its motors running until told otherwise.
            duration_ms: if (strong, weak) == (0, 0) {
                0
            } else {
                MAX_RUMBLE_MS
            },
        };
        let refresh =
            motors.duration_ms > 0 && sent.motors_at.is_some_and(|at| now - at >= RUMBLE_REFRESH);
        if motors != sent.motors || refresh {
            self.send(
                PadFeedback::Rumble(Rumble {
                    pad: self.pad,
                    motors,
                }),
                false,
            );
            sent.motors = motors;
            sent.motors_at = Some(now);
        }
        let lights = (state.lightbar, state.player);
        if sent.lights != Some(lights) && state.lights_set {
            let [red, green, blue] = state.lightbar;
            self.send(
                PadFeedback::Lights {
                    pad: self.pad,
                    red,
                    green,
                    blue,
                    player: state.player,
                },
                true,
            );
            sent.lights = Some(lights);
        }
        if sent.triggers != Some(state.triggers) && state.triggers_set {
            self.send(
                PadFeedback::Triggers {
                    pad: self.pad,
                    right: state.triggers.0,
                    left: state.triggers.1,
                },
                true,
            );
            sent.triggers = Some(state.triggers);
        }
    }

    fn serve(self) {
        let fd = self.reader.as_raw_fd();
        let mut buffer = vec![0u8; UHID_EVENT_SIZE];
        let mut state = State::default();
        let mut sent = Sent::default();
        while !self.stop.load(Ordering::Relaxed) {
            let mut poll = libc::pollfd {
                fd,
                events: libc::POLLIN,
                revents: 0,
            };
            let ready = unsafe { libc::poll(&mut poll, 1, KEEPALIVE.as_millis() as i32 / 2) };
            if ready > 0 {
                if poll.revents & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL) != 0 {
                    break;
                }
                let read = unsafe { libc::read(fd, buffer.as_mut_ptr().cast(), buffer.len()) };
                if read < 0 {
                    let error = std::io::Error::last_os_error();
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted
                    ) {
                        continue;
                    }
                    tracing::warn!(pad = self.pad, %error, "uhid pad stopped");
                    break;
                }
                match parse_event(&buffer[..read as usize]) {
                    Some(Request::Start) => {
                        tracing::debug!(pad = self.pad, "uhid pad started");
                    }
                    Some(Request::GetReport { id, number, kind }) => {
                        let report = (kind == UHID_FEATURE_REPORT)
                            .then(|| ps::feature_report(self.kind, number, self.address))
                            .flatten();
                        match report {
                            Some(data) => self.reply(&get_report_reply(id, 0, &data)),
                            None => {
                                tracing::debug!(
                                    pad = self.pad,
                                    number,
                                    kind,
                                    "unknown report read"
                                );
                                self.reply(&get_report_reply(id, libc::EIO as u16, &[]))
                            }
                        }
                    }
                    // Feature writes (e.g. a game's settings) are accepted.
                    Some(Request::SetReport { id }) => self.reply(&set_report_reply(id, 0)),
                    Some(Request::Output(report)) => self.output(&report, &mut state),
                    Some(Request::Other) | None => (),
                }
            } else if ready < 0
                && std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted
            {
                break;
            }
            let now = Instant::now();
            self.report(&state, &mut sent, now);
            if let Ok(mut shared) = self.shared.lock()
                && now - shared.written >= KEEPALIVE
                && let Err(error) = shared.write_report()
            {
                tracing::warn!(pad = self.pad, %error, "uhid keepalive failed");
            }
        }
        if sent.motors.duration_ms > 0 {
            self.send(
                PadFeedback::Rumble(Rumble {
                    pad: self.pad,
                    motors: Motors::default(),
                }),
                false,
            );
        }
    }
}

/// The pad's outputs as games last set them.
#[derive(Debug, Default)]
struct State {
    /// Strong, weak.
    motors: (u8, u8),
    lightbar: [u8; 3],
    player: u8,
    lights_set: bool,
    triggers: ([u8; TRIGGER_EFFECT_LEN], [u8; TRIGGER_EFFECT_LEN]),
    triggers_set: bool,
}

/// The HID device sysfs directory of the pad created with `phys`.
#[cfg(test)]
pub fn hid_device(phys: &str) -> Option<std::path::PathBuf> {
    std::fs::read_dir("/sys/bus/hid/devices")
        .ok()?
        .flatten()
        .map(|entry| entry.path())
        .find(|path| {
            std::fs::read_to_string(path.join("uevent")).is_ok_and(|uevent| {
                uevent
                    .lines()
                    .any(|line| line == format!("HID_PHYS={phys}"))
            })
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uhid_events_use_the_kernel_layout() {
        // include/uapi/linux/uhid.h: struct uhid_event is 4380 bytes on
        // every architecture (packed).
        assert_eq!(UHID_EVENT_SIZE, 4380);
        let event = create2_event("Pad", "teleport/1", 0x054c, 0x0ce6, &[0x05, 0x01, 0xc0]);
        assert_eq!(event.len(), 4380);
        assert_eq!(&event[..4], &11u32.to_ne_bytes());
        assert_eq!(&event[4..8], b"Pad\0");
        assert_eq!(&event[132..143], b"teleport/1\0");
        assert_eq!(&event[260..262], &3u16.to_ne_bytes(), "rd_size");
        assert_eq!(&event[262..264], &3u16.to_ne_bytes(), "BUS_USB");
        assert_eq!(&event[264..268], &0x054cu32.to_ne_bytes());
        assert_eq!(&event[268..272], &0x0ce6u32.to_ne_bytes());
        assert_eq!(&event[280..283], &[0x05, 0x01, 0xc0]);
        let long = "x".repeat(300);
        assert_eq!(
            create2_event(&long, "", 0, 0, &[])[4 + 127],
            0,
            "terminated"
        );
        assert_eq!(
            input2_event(&[1, 2]),
            [&12u32.to_ne_bytes()[..], &2u16.to_ne_bytes(), &[1, 2]].concat()
        );
        assert_eq!(
            get_report_reply(7, 0, &[5, 6]),
            [
                &10u32.to_ne_bytes()[..],
                &7u32.to_ne_bytes(),
                &0u16.to_ne_bytes(),
                &2u16.to_ne_bytes(),
                &[5, 6]
            ]
            .concat()
        );
        assert_eq!(set_report_reply(9, 0).len(), 10);
        let mut read = vec![0u8; UHID_EVENT_SIZE];
        read[..4].copy_from_slice(&9u32.to_ne_bytes());
        read[4..8].copy_from_slice(&42u32.to_ne_bytes());
        read[8] = 0x05;
        read[9] = UHID_FEATURE_REPORT;
        assert_eq!(
            parse_event(&read),
            Some(Request::GetReport {
                id: 42,
                number: 5,
                kind: 0
            })
        );
        read[..4].copy_from_slice(&6u32.to_ne_bytes());
        read[4..7].copy_from_slice(&[2, 3, 4]);
        read[4 + UHID_DATA_MAX..4 + UHID_DATA_MAX + 2].copy_from_slice(&3u16.to_ne_bytes());
        assert_eq!(parse_event(&read), Some(Request::Output(vec![2, 3, 4])));
        read[..4].copy_from_slice(&2u32.to_ne_bytes());
        assert_eq!(parse_event(&read), Some(Request::Start));
        assert_eq!(parse_event(&[1, 0]), None, "short read");
        let first = pad_address(0);
        assert_ne!(first, pad_address(0), "every pad gets its own address");
        assert_eq!((first[4], first[5]), (0, 0x7e));
    }
}
