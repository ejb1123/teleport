//! Host virtual gamepads: one uinput Xbox 360-style pad per client controller.
//!
//! The client forwards SDL2 GameController events, which already normalise
//! every controller to the Xbox layout. Each pad is created as a
//! `045e:028e` "X-Box 360 pad" with the same evdev codes and ranges as the
//! kernel `xpad` driver, so SDL, Steam Input and games that read evdev all
//! recognise it without custom mappings. Force feedback is not forwarded.

use crate::protocol::{Event, GAMEPAD_AXES, MAX_GAMEPADS};
use anyhow::{Context, Result, ensure};
use std::{collections::BTreeSet, os::fd::AsRawFd};

const UINPUT_PATH: &str = "/dev/uinput";

const EV_SYN: u16 = 0x00;
const EV_KEY: u16 = 0x01;
const EV_ABS: u16 = 0x03;
const SYN_REPORT: u16 = 0;

const BTN_A: u16 = 0x130;
const BTN_B: u16 = 0x131;
const BTN_X: u16 = 0x133;
const BTN_Y: u16 = 0x134;
const BTN_TL: u16 = 0x136;
const BTN_TR: u16 = 0x137;
const BTN_SELECT: u16 = 0x13a;
const BTN_START: u16 = 0x13b;
const BTN_MODE: u16 = 0x13c;
const BTN_THUMBL: u16 = 0x13d;
const BTN_THUMBR: u16 = 0x13e;
const KEYS: [u16; 11] = [
    BTN_A, BTN_B, BTN_X, BTN_Y, BTN_TL, BTN_TR, BTN_SELECT, BTN_START, BTN_MODE, BTN_THUMBL,
    BTN_THUMBR,
];

const ABS_X: u16 = 0x00;
const ABS_Y: u16 = 0x01;
const ABS_Z: u16 = 0x02;
const ABS_RX: u16 = 0x03;
const ABS_RY: u16 = 0x04;
const ABS_RZ: u16 = 0x05;
const ABS_HAT0X: u16 = 0x10;
const ABS_HAT0Y: u16 = 0x11;
/// (code, minimum, maximum, fuzz, flat) exactly as `xpad` reports a 360 pad.
const AXES: [(u16, i32, i32, i32, i32); 8] = [
    (ABS_X, -32768, 32767, 16, 128),
    (ABS_Y, -32768, 32767, 16, 128),
    (ABS_RX, -32768, 32767, 16, 128),
    (ABS_RY, -32768, 32767, 16, 128),
    (ABS_Z, 0, 255, 0, 0),
    (ABS_RZ, 0, 255, 0, 0),
    (ABS_HAT0X, -1, 1, 0, 0),
    (ABS_HAT0Y, -1, 1, 0, 0),
];

/// One evdev event a pad emits.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RawEvent {
    pub kind: u16,
    pub code: u16,
    pub value: i32,
}

const fn raw(kind: u16, code: u16, value: i32) -> RawEvent {
    RawEvent { kind, code, value }
}

const SYN: RawEvent = raw(EV_SYN, SYN_REPORT, 0);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Target {
    Key(u16),
    /// Up, down, left, right: the 360 pad reports its d-pad as a hat.
    Dpad(usize),
}

/// SDL2 `SDL_GameControllerButton` → Xbox 360 evdev. Misc, paddles and the
/// touchpad click have no 360 equivalent and are ignored.
fn button_target(button: u8) -> Option<Target> {
    Some(match button {
        0 => Target::Key(BTN_A),
        1 => Target::Key(BTN_B),
        2 => Target::Key(BTN_X),
        3 => Target::Key(BTN_Y),
        4 => Target::Key(BTN_SELECT),
        5 => Target::Key(BTN_MODE),
        6 => Target::Key(BTN_START),
        7 => Target::Key(BTN_THUMBL),
        8 => Target::Key(BTN_THUMBR),
        9 => Target::Key(BTN_TL),
        10 => Target::Key(BTN_TR),
        11..=14 => Target::Dpad(usize::from(button - 11)),
        _ => return None,
    })
}

/// SDL2 `SDL_GameControllerAxis` value → Xbox 360 evdev. SDL and `xpad` agree
/// on stick directions (Y positive is down), so sticks pass through; SDL
/// triggers (0..=32767) scale to the 360's 8-bit range.
fn axis_target(axis: u8, value: i16) -> Option<(u16, i32)> {
    let value = i32::from(value);
    let trigger = (value.max(0) * 255 + 16383) / 32767;
    Some(match axis {
        0 => (ABS_X, value),
        1 => (ABS_Y, value),
        2 => (ABS_RX, value),
        3 => (ABS_RY, value),
        4 => (ABS_Z, trigger),
        5 => (ABS_RZ, trigger),
        _ => return None,
    })
}

/// What one pad holds, so it can always be returned to neutral.
#[derive(Default, Debug)]
pub struct PadState {
    keys: BTreeSet<u16>,
    dpad: [bool; 4],
}

impl PadState {
    fn hat(&self) -> [RawEvent; 2] {
        let [up, down, left, right] = self.dpad.map(i32::from);
        [
            raw(EV_ABS, ABS_HAT0X, right - left),
            raw(EV_ABS, ABS_HAT0Y, down - up),
        ]
    }

    /// Repeated presses and releases of unheld buttons emit nothing.
    pub fn button(&mut self, button: u8, down: bool) -> Vec<RawEvent> {
        match button_target(button) {
            Some(Target::Key(code)) => {
                let changed = if down {
                    self.keys.insert(code)
                } else {
                    self.keys.remove(&code)
                };
                if changed {
                    vec![raw(EV_KEY, code, i32::from(down)), SYN]
                } else {
                    Vec::new()
                }
            }
            Some(Target::Dpad(index)) if self.dpad[index] != down => {
                self.dpad[index] = down;
                let [x, y] = self.hat();
                vec![x, y, SYN]
            }
            _ => Vec::new(),
        }
    }

    pub fn axis(&mut self, axis: u8, value: i16) -> Vec<RawEvent> {
        axis_target(axis, value)
            .map(|(code, value)| vec![raw(EV_ABS, code, value), SYN])
            .unwrap_or_default()
    }

    /// Release every button and centre every axis (sticks, triggers, d-pad).
    pub fn neutral(&mut self) -> Vec<RawEvent> {
        let mut events: Vec<_> = std::mem::take(&mut self.keys)
            .into_iter()
            .map(|code| raw(EV_KEY, code, 0))
            .collect();
        self.dpad = [false; 4];
        events.extend(AXES.iter().map(|(code, ..)| raw(EV_ABS, *code, 0)));
        events.push(SYN);
        events
    }
}

/// Whether this process can create uinput devices.
pub fn uinput_writable() -> bool {
    let path = std::ffi::CString::new(UINPUT_PATH).expect("static path");
    unsafe { libc::access(path.as_ptr(), libc::W_OK) == 0 }
}

const fn ioc(direction: u64, number: u64, size: usize) -> u64 {
    (direction << 30) | ((size as u64) << 16) | ((b'U' as u64) << 8) | number
}
const IOC_WRITE: u64 = 1;
#[cfg(test)]
const IOC_READ: u64 = 2;
const UI_DEV_CREATE: u64 = ioc(0, 1, 0);
const UI_DEV_DESTROY: u64 = ioc(0, 2, 0);
const UI_DEV_SETUP: u64 = ioc(IOC_WRITE, 3, size_of::<libc::uinput_setup>());
const UI_ABS_SETUP: u64 = ioc(IOC_WRITE, 4, size_of::<libc::uinput_abs_setup>());
const UI_SET_EVBIT: u64 = ioc(IOC_WRITE, 100, size_of::<libc::c_int>());
const UI_SET_KEYBIT: u64 = ioc(IOC_WRITE, 101, size_of::<libc::c_int>());
const UI_SET_ABSBIT: u64 = ioc(IOC_WRITE, 103, size_of::<libc::c_int>());
#[cfg(test)]
const fn ui_get_sysname(len: usize) -> u64 {
    ioc(IOC_READ, 44, len)
}

/// A created `/dev/uinput` device; dropping it destroys the device.
pub struct UinputPad {
    file: std::fs::File,
}

impl UinputPad {
    pub fn create(slot: u8) -> Result<Self> {
        use std::os::unix::fs::OpenOptionsExt;
        let file = std::fs::OpenOptions::new()
            .write(true)
            .custom_flags(libc::O_NONBLOCK | libc::O_CLOEXEC)
            .open(UINPUT_PATH)
            .with_context(|| {
                format!("opening {UINPUT_PATH}; see docs/gamepad.md for the required permission")
            })?;
        let pad = Self { file };
        let fd = pad.file.as_raw_fd();
        let control = |request: u64, value: libc::c_ulong, what: &str| -> Result<()> {
            let result = unsafe { libc::ioctl(fd, request as _, value) };
            ensure!(
                result >= 0,
                "uinput {what}: {}",
                std::io::Error::last_os_error()
            );
            Ok(())
        };
        control(UI_SET_EVBIT, EV_KEY.into(), "enable keys")?;
        control(UI_SET_EVBIT, EV_ABS.into(), "enable axes")?;
        for code in KEYS {
            control(UI_SET_KEYBIT, code.into(), "enable button")?;
        }
        for (code, minimum, maximum, fuzz, flat) in AXES {
            control(UI_SET_ABSBIT, code.into(), "enable axis")?;
            let mut setup: libc::uinput_abs_setup = unsafe { std::mem::zeroed() };
            setup.code = code;
            setup.absinfo.minimum = minimum;
            setup.absinfo.maximum = maximum;
            setup.absinfo.fuzz = fuzz;
            setup.absinfo.flat = flat;
            let pointer = &setup as *const libc::uinput_abs_setup;
            control(UI_ABS_SETUP, pointer as libc::c_ulong, "axis setup")?;
        }
        let mut setup: libc::uinput_setup = unsafe { std::mem::zeroed() };
        setup.id.bustype = 0x03; // BUS_USB
        setup.id.vendor = 0x045e;
        setup.id.product = 0x028e;
        setup.id.version = 0x0110;
        let name = format!("Teleport X-Box 360 pad {}", slot + 1);
        for (target, byte) in setup.name.iter_mut().zip(name.bytes()) {
            *target = byte as libc::c_char;
        }
        let pointer = &setup as *const libc::uinput_setup;
        control(UI_DEV_SETUP, pointer as libc::c_ulong, "device setup")?;
        control(UI_DEV_CREATE, 0, "create device")?;
        Ok(pad)
    }

    pub fn write(&mut self, events: &[RawEvent]) -> Result<()> {
        use std::io::Write;
        if events.is_empty() {
            return Ok(());
        }
        let mut bytes = Vec::with_capacity(events.len() * size_of::<libc::input_event>());
        for event in events {
            let mut record: libc::input_event = unsafe { std::mem::zeroed() };
            record.type_ = event.kind;
            record.code = event.code;
            record.value = event.value;
            bytes.extend_from_slice(unsafe {
                std::slice::from_raw_parts(
                    (&record as *const libc::input_event).cast::<u8>(),
                    size_of::<libc::input_event>(),
                )
            });
        }
        self.file
            .write_all(&bytes)
            .context("writing virtual gamepad events")
    }

    /// Kernel name (`inputN`) of the created device, under /sys/devices/virtual/input.
    #[cfg(test)]
    pub fn sysname(&self) -> Result<String> {
        let mut buffer = [0u8; 64];
        let result = unsafe {
            libc::ioctl(
                self.file.as_raw_fd(),
                ui_get_sysname(buffer.len()) as _,
                buffer.as_mut_ptr(),
            )
        };
        ensure!(
            result >= 0,
            "uinput sysname: {}",
            std::io::Error::last_os_error()
        );
        let end = buffer.iter().position(|b| *b == 0).unwrap_or(buffer.len());
        Ok(String::from_utf8_lossy(&buffer[..end]).into_owned())
    }
}

impl Drop for UinputPad {
    fn drop(&mut self) {
        // Closing the fd also destroys it; destroy explicitly to be prompt.
        unsafe { libc::ioctl(self.file.as_raw_fd(), UI_DEV_DESTROY as _) };
    }
}

struct Pad {
    state: PadState,
    /// `None` for the `--source test` backend: events are accepted and dropped.
    device: Option<UinputPad>,
}

impl Pad {
    fn emit(&mut self, events: &[RawEvent]) -> Result<()> {
        match &mut self.device {
            Some(device) => device.write(events),
            None => Ok(()),
        }
    }
}

/// The session's pads, indexed by the client's slot number.
pub struct Gamepads {
    pads: [Option<Pad>; MAX_GAMEPADS as usize],
    /// Create real uinput devices (false for `--source test`).
    virtual_devices: bool,
}

impl Gamepads {
    pub fn new(virtual_devices: bool) -> Self {
        Self {
            pads: Default::default(),
            virtual_devices,
        }
    }

    /// Advertise `Desktop::gamepads` only if pads can actually be created.
    pub fn available(&self) -> bool {
        !self.virtual_devices || uinput_writable()
    }

    pub fn connected(&self) -> usize {
        self.pads.iter().flatten().count()
    }

    /// Apply one validated `Gamepad*` event. Events for a slot that was never
    /// connected (or failed to connect) are ignored rather than failing the
    /// session.
    pub fn input(&mut self, event: &Event) -> Result<()> {
        event.validate()?;
        match *event {
            Event::GamepadConnected { pad, ref name } => {
                if let Some(existing) = &mut self.pads[usize::from(pad)] {
                    let events = existing.state.neutral();
                    return existing.emit(&events);
                }
                let device = if self.virtual_devices {
                    Some(UinputPad::create(pad)?)
                } else {
                    None
                };
                tracing::info!(pad, controller = %name, virtual_device = device.is_some(), "gamepad connected");
                self.pads[usize::from(pad)] = Some(Pad {
                    state: PadState::default(),
                    device,
                });
            }
            Event::GamepadDisconnected { pad } => {
                let removed = self.pads[usize::from(pad)].take();
                tracing::info!(pad, existed = removed.is_some(), "gamepad disconnected");
            }
            Event::GamepadButton { pad, button, down } => {
                if let Some(pad) = &mut self.pads[usize::from(pad)] {
                    let events = pad.state.button(button, down);
                    pad.emit(&events)?;
                }
            }
            Event::GamepadAxis { pad, axis, value } => {
                debug_assert!(axis < GAMEPAD_AXES);
                if let Some(pad) = &mut self.pads[usize::from(pad)] {
                    let events = pad.state.axis(axis, value);
                    pad.emit(&events)?;
                }
            }
            _ => (),
        }
        Ok(())
    }

    /// `ReleaseAll`: release every button and centre every axis, keeping the
    /// devices so a game does not see its controller unplugged on focus loss.
    pub fn neutralise(&mut self) {
        for pad in self.pads.iter_mut().flatten() {
            let events = pad.state.neutral();
            if let Err(error) = pad.emit(&events) {
                tracing::warn!(%error, "gamepad release failed");
            }
        }
    }

    /// Session end: neutralise and destroy every pad.
    pub fn destroy_all(&mut self) {
        self.neutralise();
        let count = self.connected();
        self.pads = Default::default();
        if count > 0 {
            tracing::info!(count, "virtual gamepads removed");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sdl_buttons_map_to_xbox_360_codes_and_hat() {
        let mut pad = PadState::default();
        assert_eq!(pad.button(0, true), vec![raw(EV_KEY, BTN_A, 1), SYN], "A");
        assert!(pad.button(0, true).is_empty(), "repeat press is idempotent");
        assert_eq!(pad.button(9, true)[0], raw(EV_KEY, BTN_TL, 1));
        assert_eq!(pad.button(5, true)[0], raw(EV_KEY, BTN_MODE, 1));
        assert_eq!(pad.button(4, true)[0], raw(EV_KEY, BTN_SELECT, 1));
        assert_eq!(pad.button(6, true)[0], raw(EV_KEY, BTN_START, 1));
        // D-pad up then right is a diagonal on the hat.
        assert_eq!(
            pad.button(11, true),
            vec![raw(EV_ABS, ABS_HAT0X, 0), raw(EV_ABS, ABS_HAT0Y, -1), SYN]
        );
        assert_eq!(
            pad.button(14, true),
            vec![raw(EV_ABS, ABS_HAT0X, 1), raw(EV_ABS, ABS_HAT0Y, -1), SYN]
        );
        // Paddles, misc and touchpad have no 360 equivalent.
        for button in 15..21 {
            assert!(pad.button(button, true).is_empty());
        }
        assert!(pad.button(1, false).is_empty(), "unheld release is a no-op");
        let neutral = pad.neutral();
        for code in [BTN_A, BTN_TL, BTN_MODE, BTN_SELECT, BTN_START] {
            assert!(neutral.contains(&raw(EV_KEY, code, 0)), "{code:#x}");
        }
        for (code, ..) in AXES {
            assert!(neutral.contains(&raw(EV_ABS, code, 0)), "{code:#x}");
        }
        assert_eq!(neutral.last(), Some(&SYN));
        assert!(pad.button(0, false).is_empty(), "neutral released A");
        assert_eq!(pad.button(11, false), Vec::new(), "neutral released up");
    }

    #[test]
    fn sdl_axes_map_to_xbox_360_ranges() {
        let mut pad = PadState::default();
        assert_eq!(pad.axis(0, i16::MIN)[0], raw(EV_ABS, ABS_X, -32768));
        assert_eq!(pad.axis(1, 1234)[0], raw(EV_ABS, ABS_Y, 1234));
        assert_eq!(pad.axis(2, -5)[0], raw(EV_ABS, ABS_RX, -5));
        assert_eq!(pad.axis(3, i16::MAX)[0], raw(EV_ABS, ABS_RY, 32767));
        assert_eq!(pad.axis(4, 0)[0], raw(EV_ABS, ABS_Z, 0));
        assert_eq!(pad.axis(4, i16::MAX)[0], raw(EV_ABS, ABS_Z, 255));
        assert_eq!(pad.axis(5, 16384)[0], raw(EV_ABS, ABS_RZ, 128));
        assert!(pad.axis(6, 0).is_empty());
    }

    #[test]
    fn test_backend_accepts_pad_events_as_noops() {
        let mut pads = Gamepads::new(false);
        assert!(pads.available(), "test backend always advertises gamepads");
        // Events for an unconnected slot are ignored, not session errors.
        pads.input(&Event::GamepadButton {
            pad: 0,
            button: 0,
            down: true,
        })
        .unwrap();
        pads.input(&Event::GamepadConnected {
            pad: 3,
            name: "Pad".into(),
        })
        .unwrap();
        pads.input(&Event::GamepadButton {
            pad: 3,
            button: 0,
            down: true,
        })
        .unwrap();
        pads.input(&Event::GamepadAxis {
            pad: 3,
            axis: 4,
            value: 100,
        })
        .unwrap();
        assert!(
            pads.input(&Event::GamepadAxis {
                pad: 3,
                axis: 4,
                value: -1,
            })
            .is_err(),
            "events are validated again on the host"
        );
        assert_eq!(pads.connected(), 1);
        pads.neutralise();
        assert_eq!(pads.connected(), 1, "ReleaseAll keeps the device");
        assert!(pads.pads[3].as_ref().unwrap().state.keys.is_empty());
        pads.input(&Event::GamepadDisconnected { pad: 3 }).unwrap();
        assert_eq!(pads.connected(), 0);
        pads.input(&Event::GamepadConnected {
            pad: 0,
            name: String::new(),
        })
        .unwrap();
        pads.destroy_all();
        assert_eq!(pads.connected(), 0);
    }

    #[test]
    fn ioctl_numbers_match_linux_headers() {
        assert_eq!(UI_DEV_CREATE, 0x5501);
        assert_eq!(UI_DEV_DESTROY, 0x5502);
        assert_eq!(UI_DEV_SETUP, 0x405c_5503);
        assert_eq!(UI_ABS_SETUP, 0x401c_5504);
        assert_eq!(UI_SET_EVBIT, 0x4004_5564);
        assert_eq!(UI_SET_KEYBIT, 0x4004_5565);
        assert_eq!(UI_SET_ABSBIT, 0x4004_5567);
        assert_eq!(ui_get_sysname(64), 0x8040_552c);
    }

    /// Creates a real device, finds its evdev node and reads back a button,
    /// a stick, a trigger, the d-pad and the ReleaseAll/destroy behaviour.
    #[test]
    #[ignore = "needs write access to /dev/uinput (input/uinput group or a udev rule) and read access to the created /dev/input/event* node"]
    fn uinput_pad_reaches_evdev() {
        use std::io::Read;
        let mut pads = Gamepads::new(true);
        assert!(pads.available(), "/dev/uinput is not writable");
        pads.input(&Event::GamepadConnected {
            pad: 1,
            name: "test".into(),
        })
        .unwrap();
        let sysname = pads.pads[1]
            .as_ref()
            .unwrap()
            .device
            .as_ref()
            .unwrap()
            .sysname()
            .unwrap();
        let sys = std::path::Path::new("/sys/devices/virtual/input").join(&sysname);
        assert_eq!(
            std::fs::read_to_string(sys.join("name")).unwrap().trim(),
            "Teleport X-Box 360 pad 2"
        );
        assert_eq!(
            std::fs::read_to_string(sys.join("id/vendor"))
                .unwrap()
                .trim(),
            "045e"
        );
        assert_eq!(
            std::fs::read_to_string(sys.join("id/product"))
                .unwrap()
                .trim(),
            "028e"
        );
        let node = std::fs::read_dir(&sys)
            .unwrap()
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .find(|name| name.starts_with("event"))
            .expect("evdev node for the virtual pad");
        let path = std::path::Path::new("/dev/input").join(&node);
        // udev applies permissions (uaccess) shortly after creation.
        let mut device = None;
        for _ in 0..50 {
            if let Ok(file) = std::fs::File::open(&path) {
                device = Some(file);
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        let mut device = device.unwrap_or_else(|| panic!("cannot read {}", path.display()));
        for event in [
            Event::GamepadButton {
                pad: 1,
                button: 0,
                down: true,
            },
            Event::GamepadAxis {
                pad: 1,
                axis: 0,
                value: -20000,
            },
            Event::GamepadAxis {
                pad: 1,
                axis: 5,
                value: i16::MAX,
            },
            Event::GamepadButton {
                pad: 1,
                button: 12,
                down: true,
            },
        ] {
            pads.input(&event).unwrap();
        }
        pads.neutralise();
        let size = size_of::<libc::input_event>();
        let mut seen = Vec::new();
        let mut buffer = vec![0u8; size * 64];
        while !seen.contains(&raw(EV_ABS, ABS_HAT0Y, 0)) {
            let read = device.read(&mut buffer).unwrap();
            for chunk in buffer[..read].chunks_exact(size) {
                let event: libc::input_event =
                    unsafe { std::ptr::read_unaligned(chunk.as_ptr().cast()) };
                if event.type_ != EV_SYN {
                    seen.push(raw(event.type_, event.code, event.value));
                }
            }
        }
        assert_eq!(
            seen,
            vec![
                raw(EV_KEY, BTN_A, 1),
                raw(EV_ABS, ABS_X, -20000),
                raw(EV_ABS, ABS_RZ, 255),
                raw(EV_ABS, ABS_HAT0Y, 1),
                // ReleaseAll: the kernel drops unchanged axes.
                raw(EV_KEY, BTN_A, 0),
                raw(EV_ABS, ABS_X, 0),
                raw(EV_ABS, ABS_RZ, 0),
                raw(EV_ABS, ABS_HAT0Y, 0),
            ]
        );
        pads.destroy_all();
        for _ in 0..50 {
            if !sys.exists() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert!(!sys.exists(), "device destroyed at session end");
    }

    /// Round trip through the client's own mapping: SDL on this machine must
    /// see the virtual pad as a GameController and report every forwarded SDL
    /// button/axis exactly as the client sent it.
    #[test]
    #[ignore = "needs write access to /dev/uinput, read access to /dev/input/event* and SDL joystick hotplug (udev)"]
    fn sdl_reads_virtual_pad_back_as_the_same_controller() {
        use sdl2::controller::{Axis, Button};
        let sdl = sdl2::init().unwrap();
        let subsystem = sdl.game_controller().unwrap();
        let mut pump = sdl.event_pump().unwrap();
        for _ in pump.poll_iter() {}
        let mut pads = Gamepads::new(true);
        pads.input(&Event::GamepadConnected {
            pad: 0,
            name: "test".into(),
        })
        .unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let mut controller = None;
        while controller.is_none() && std::time::Instant::now() < deadline {
            for event in pump.poll_iter() {
                if let sdl2::event::Event::ControllerDeviceAdded { which, .. } = event {
                    let candidate = subsystem.open(which).unwrap();
                    if (candidate.vendor_id(), candidate.product_id())
                        == (Some(0x045e), Some(0x028e))
                        && candidate.name().contains("360")
                    {
                        controller = Some(candidate);
                    }
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        let controller = controller.expect("SDL did not report the virtual pad");
        let settle = |pump: &mut sdl2::EventPump| {
            std::thread::sleep(std::time::Duration::from_millis(30));
            for _ in pump.poll_iter() {}
        };
        for (number, button) in [
            (0, Button::A),
            (1, Button::B),
            (2, Button::X),
            (3, Button::Y),
            (4, Button::Back),
            (5, Button::Guide),
            (6, Button::Start),
            (7, Button::LeftStick),
            (8, Button::RightStick),
            (9, Button::LeftShoulder),
            (10, Button::RightShoulder),
            (11, Button::DPadUp),
            (12, Button::DPadDown),
            (13, Button::DPadLeft),
            (14, Button::DPadRight),
        ] {
            assert_eq!(button as i32, number);
            pads.input(&Event::GamepadButton {
                pad: 0,
                button: number as u8,
                down: true,
            })
            .unwrap();
            settle(&mut pump);
            assert!(controller.button(button), "{button:?} pressed");
            pads.input(&Event::GamepadButton {
                pad: 0,
                button: number as u8,
                down: false,
            })
            .unwrap();
            settle(&mut pump);
            assert!(!controller.button(button), "{button:?} released");
        }
        for (number, axis, value) in [
            (0, Axis::LeftX, -20000),
            (1, Axis::LeftY, 15000),
            (2, Axis::RightX, 32767),
            (3, Axis::RightY, -32768),
            (4, Axis::TriggerLeft, 32767),
            (5, Axis::TriggerRight, 16384),
        ] {
            assert_eq!(axis as i32, number);
            pads.input(&Event::GamepadAxis {
                pad: 0,
                axis: number as u8,
                value,
            })
            .unwrap();
            settle(&mut pump);
            let seen = i32::from(controller.axis(axis));
            // Triggers go through the 360's 8-bit range.
            let tolerance = if number >= 4 { 200 } else { 1 };
            assert!(
                (seen - i32::from(value)).abs() <= tolerance,
                "{axis:?}: sent {value}, SDL read {seen}"
            );
        }
        pads.neutralise();
        settle(&mut pump);
        for axis in [Axis::LeftX, Axis::RightY, Axis::TriggerLeft] {
            assert!(controller.axis(axis).abs() <= 1, "{axis:?} centred");
        }
    }
}
