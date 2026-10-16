//! Host virtual gamepads: one uinput Xbox 360-style pad per client controller.
//!
//! The client forwards SDL2 GameController events, which already normalise
//! every controller to the Xbox layout. Each pad is created as a
//! `045e:028e` "X-Box 360 pad" with the same evdev codes and ranges as the
//! kernel `xpad` driver, so SDL, Steam Input and games that read evdev all
//! recognise it without custom mappings.
//!
//! When the client plays rumble (`ClientFeatures::rumble`), pads also offer
//! `FF_RUMBLE` like `xpad`. A thread per pad answers the kernel's effect
//! uploads and erases on the uinput fd (a game blocks in `EVIOCSFF` until
//! then) and turns played effects into [`Rumble`] updates for the client.

use crate::protocol::{Event, GAMEPAD_AXES, MAX_GAMEPADS, MAX_RUMBLE_MS};
use anyhow::{Context, Result, ensure};
use std::{
    collections::{BTreeMap, BTreeSet},
    os::fd::AsRawFd,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

const UINPUT_PATH: &str = "/dev/uinput";

const EV_SYN: u16 = 0x00;
const EV_KEY: u16 = 0x01;
const EV_ABS: u16 = 0x03;
const EV_FF: u16 = 0x15;
/// Kernel requests read from the uinput fd: force-feedback upload and erase.
const EV_UINPUT: u16 = 0x0101;
const UI_FF_UPLOAD: u16 = 1;
const UI_FF_ERASE: u16 = 2;
const FF_RUMBLE: u16 = 0x50;
/// Effects a game may keep uploaded at once.
const FF_EFFECTS: u32 = 16;
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

/// What a pad's motors should do now, from the effects a game plays.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Motors {
    /// Strong (low-frequency) motor: `ff_rumble_effect.strong_magnitude`,
    /// SDL's `low_frequency_rumble`.
    pub low: u16,
    /// Weak (high-frequency) motor.
    pub high: u16,
    /// How long to play unless updated; 0 when stopped.
    pub duration_ms: u32,
}

/// Rumble for the client's pad `pad` (`Update::Rumble`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rumble {
    pub pad: u8,
    pub motors: Motors,
}

pub type RumbleSender = tokio::sync::mpsc::UnboundedSender<Rumble>;

/// How often an effect without an end is re-sent: one update lasts at most
/// [`MAX_RUMBLE_MS`].
const RUMBLE_REFRESH: Duration = Duration::from_secs(30);

#[derive(Clone, Copy, Debug)]
struct Effect {
    strong: u16,
    weak: u16,
    /// `ff_replay.length`; 0 plays until stopped.
    length_ms: u16,
}

/// One pad's uploaded rumble effects and which of them play. Playing
/// effects add up, saturating, as in the kernel's memless devices.
#[derive(Default, Debug)]
pub struct Effects {
    uploaded: BTreeMap<i16, Effect>,
    /// Playing effects and when they end (`None`: until stopped).
    playing: BTreeMap<i16, Option<Instant>>,
}

impl Effects {
    /// A new or replaced effect; a replaced one keeps playing with its new
    /// strength.
    pub fn upload(&mut self, id: i16, strong: u16, weak: u16, length_ms: u16) {
        self.uploaded.insert(
            id,
            Effect {
                strong,
                weak,
                length_ms,
            },
        );
    }

    pub fn erase(&mut self, id: i16) {
        self.uploaded.remove(&id);
        self.playing.remove(&id);
    }

    /// `EV_FF` for effect `id`: play it `count` times, or stop it on 0.
    pub fn play(&mut self, id: i16, count: i32, now: Instant) {
        match self.uploaded.get(&id) {
            Some(effect) if count > 0 => {
                let end = (effect.length_ms > 0).then(|| {
                    let repeats = count.min(1000) as u64;
                    now + Duration::from_millis(u64::from(effect.length_ms) * repeats)
                });
                self.playing.insert(id, end);
            }
            _ => {
                self.playing.remove(&id);
            }
        }
    }

    /// The motors at `now`; finished effects are dropped.
    pub fn motors(&mut self, now: Instant) -> Motors {
        self.playing
            .retain(|_, end| end.is_none_or(|end| end > now));
        let mut motors = Motors::default();
        for (id, end) in &self.playing {
            let Some(effect) = self.uploaded.get(id) else {
                continue;
            };
            motors.low = motors.low.saturating_add(effect.strong);
            motors.high = motors.high.saturating_add(effect.weak);
            let left = end.map_or(MAX_RUMBLE_MS, |end| {
                (end.duration_since(now).as_millis() as u32).clamp(1, MAX_RUMBLE_MS)
            });
            motors.duration_ms = motors.duration_ms.max(left);
        }
        if motors.low == 0 && motors.high == 0 {
            motors.duration_ms = 0;
        }
        motors
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
const IOC_READ: u64 = 2;
const UI_DEV_CREATE: u64 = ioc(0, 1, 0);
const UI_DEV_DESTROY: u64 = ioc(0, 2, 0);
const UI_DEV_SETUP: u64 = ioc(IOC_WRITE, 3, size_of::<libc::uinput_setup>());
const UI_ABS_SETUP: u64 = ioc(IOC_WRITE, 4, size_of::<libc::uinput_abs_setup>());
const UI_SET_EVBIT: u64 = ioc(IOC_WRITE, 100, size_of::<libc::c_int>());
const UI_SET_KEYBIT: u64 = ioc(IOC_WRITE, 101, size_of::<libc::c_int>());
const UI_SET_ABSBIT: u64 = ioc(IOC_WRITE, 103, size_of::<libc::c_int>());
const UI_SET_FFBIT: u64 = ioc(IOC_WRITE, 107, size_of::<libc::c_int>());
const UI_BEGIN_FF_UPLOAD: u64 = ioc(
    IOC_READ | IOC_WRITE,
    200,
    size_of::<libc::uinput_ff_upload>(),
);
const UI_END_FF_UPLOAD: u64 = ioc(IOC_WRITE, 201, size_of::<libc::uinput_ff_upload>());
const UI_BEGIN_FF_ERASE: u64 = ioc(
    IOC_READ | IOC_WRITE,
    202,
    size_of::<libc::uinput_ff_erase>(),
);
const UI_END_FF_ERASE: u64 = ioc(IOC_WRITE, 203, size_of::<libc::uinput_ff_erase>());
#[cfg(test)]
const fn ui_get_sysname(len: usize) -> u64 {
    ioc(IOC_READ, 44, len)
}

/// A created `/dev/uinput` device; dropping it destroys the device.
pub struct UinputPad {
    file: std::fs::File,
}

impl UinputPad {
    /// `force_feedback` offers `FF_RUMBLE`; the owner must then serve the
    /// kernel's requests ([`serve_force_feedback`]).
    pub fn create(slot: u8, force_feedback: bool) -> Result<Self> {
        use std::os::unix::fs::OpenOptionsExt;
        let file = std::fs::OpenOptions::new()
            .read(true)
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
        if force_feedback {
            control(UI_SET_EVBIT, EV_FF.into(), "enable force feedback")?;
            control(UI_SET_FFBIT, FF_RUMBLE.into(), "enable rumble")?;
            setup.ff_effects_max = FF_EFFECTS;
        }
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

/// Answer one `UI_FF_UPLOAD` request: keep rumble effects, refuse others
/// (the kernel already refuses types the pad does not offer).
fn answer_upload(fd: libc::c_int, request_id: u32, effects: &mut Effects) -> Result<()> {
    let mut upload: libc::uinput_ff_upload = unsafe { std::mem::zeroed() };
    upload.request_id = request_id;
    let begin = unsafe { libc::ioctl(fd, UI_BEGIN_FF_UPLOAD as _, &mut upload) };
    ensure!(
        begin >= 0,
        "uinput begin upload: {}",
        std::io::Error::last_os_error()
    );
    let effect = upload.effect;
    upload.retval = if effect.type_ == FF_RUMBLE {
        let rumble: libc::ff_rumble_effect =
            unsafe { std::ptr::read_unaligned(effect.u.as_ptr().cast()) };
        effects.upload(
            effect.id,
            rumble.strong_magnitude,
            rumble.weak_magnitude,
            effect.replay.length,
        );
        0
    } else {
        -libc::EINVAL
    };
    let end = unsafe { libc::ioctl(fd, UI_END_FF_UPLOAD as _, &upload) };
    ensure!(
        end >= 0,
        "uinput end upload: {}",
        std::io::Error::last_os_error()
    );
    Ok(())
}

fn answer_erase(fd: libc::c_int, request_id: u32, effects: &mut Effects) -> Result<()> {
    let mut erase: libc::uinput_ff_erase = unsafe { std::mem::zeroed() };
    erase.request_id = request_id;
    let begin = unsafe { libc::ioctl(fd, UI_BEGIN_FF_ERASE as _, &mut erase) };
    ensure!(
        begin >= 0,
        "uinput begin erase: {}",
        std::io::Error::last_os_error()
    );
    effects.erase(erase.effect_id as i16);
    erase.retval = 0;
    let end = unsafe { libc::ioctl(fd, UI_END_FF_ERASE as _, &erase) };
    ensure!(
        end >= 0,
        "uinput end erase: {}",
        std::io::Error::last_os_error()
    );
    Ok(())
}

/// Serve force feedback for pad `pad` on its uinput fd until `stop` is set,
/// the device goes away or the session's receiver is gone. Reports the
/// motors whenever a game uploads, erases or plays an effect, when effects
/// end, and every [`RUMBLE_REFRESH`] for effects without an end.
fn serve_force_feedback(file: std::fs::File, pad: u8, sender: RumbleSender, stop: Arc<AtomicBool>) {
    let fd = file.as_raw_fd();
    let mut effects = Effects::default();
    let mut sent = Motors::default();
    let mut sent_at = Instant::now();
    let size = size_of::<libc::input_event>();
    let mut buffer = vec![0u8; size * 32];
    while !stop.load(Ordering::Relaxed) {
        let mut poll = libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        };
        // Also wakes every 50 ms to end effects and to notice `stop`.
        let ready = unsafe { libc::poll(&mut poll, 1, 50) };
        let mut requested = false;
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
                tracing::warn!(pad, %error, "virtual gamepad force feedback stopped");
                break;
            }
            for chunk in buffer[..read as usize].chunks_exact(size) {
                let event: libc::input_event =
                    unsafe { std::ptr::read_unaligned(chunk.as_ptr().cast()) };
                let answered = match (event.type_, event.code) {
                    (EV_UINPUT, UI_FF_UPLOAD) => {
                        answer_upload(fd, event.value as u32, &mut effects)
                    }
                    (EV_UINPUT, UI_FF_ERASE) => answer_erase(fd, event.value as u32, &mut effects),
                    (EV_FF, code) => {
                        effects.play(code as i16, event.value, Instant::now());
                        Ok(())
                    }
                    _ => continue,
                };
                requested = true;
                if let Err(error) = answered {
                    tracing::warn!(
                        pad,
                        error = format!("{error:#}"),
                        "force feedback request failed"
                    );
                }
            }
        } else if ready < 0
            && std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted
        {
            break;
        }
        let now = Instant::now();
        let motors = effects.motors(now);
        let changed = (motors.low, motors.high) != (sent.low, sent.high);
        let refresh = motors.duration_ms == MAX_RUMBLE_MS && now - sent_at >= RUMBLE_REFRESH;
        if changed || refresh || (requested && motors.duration_ms > 0) {
            if sender.send(Rumble { pad, motors }).is_err() {
                return;
            }
            (sent, sent_at) = (motors, now);
        }
    }
    if sent.duration_ms > 0 {
        let _ = sender.send(Rumble {
            pad,
            motors: Motors::default(),
        });
    }
}

struct Pad {
    state: PadState,
    /// `None` for the `--source test` backend: events are accepted and dropped.
    device: Option<UinputPad>,
    /// Stops the pad's force-feedback thread, if it has one.
    force_feedback: Option<Arc<AtomicBool>>,
}

impl Drop for Pad {
    fn drop(&mut self) {
        // The thread notices within 50 ms; the device itself is destroyed now.
        if let Some(stop) = &self.force_feedback {
            stop.store(true, Ordering::Relaxed);
        }
    }
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
    /// Set for a client that plays rumble: new pads offer force feedback.
    rumble: Option<RumbleSender>,
}

impl Gamepads {
    pub fn new(virtual_devices: bool) -> Self {
        Self {
            pads: Default::default(),
            virtual_devices,
            rumble: None,
        }
    }

    /// The client plays rumble (`ClientFeatures::rumble`); pads created
    /// from now on in this session offer `FF_RUMBLE` and report to `sender`.
    pub fn enable_rumble(&mut self, sender: RumbleSender) {
        self.rumble = Some(sender);
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
                    Some(UinputPad::create(pad, self.rumble.is_some())?)
                } else {
                    None
                };
                let force_feedback = match (&device, &self.rumble) {
                    (Some(device), Some(sender)) => {
                        let stop = Arc::new(AtomicBool::new(false));
                        let (file, sender, flag) =
                            (device.file.try_clone()?, sender.clone(), stop.clone());
                        std::thread::Builder::new()
                            .name(format!("pad{pad}-rumble"))
                            .spawn(move || serve_force_feedback(file, pad, sender, flag))?;
                        Some(stop)
                    }
                    _ => None,
                };
                tracing::info!(
                    pad,
                    controller = %name,
                    virtual_device = device.is_some(),
                    rumble = self.rumble.is_some(),
                    "gamepad connected"
                );
                self.pads[usize::from(pad)] = Some(Pad {
                    state: PadState::default(),
                    device,
                    force_feedback,
                });
            }
            Event::GamepadDisconnected { pad } => {
                let removed = self.pads[usize::from(pad)].take();
                tracing::info!(pad, existed = removed.is_some(), "gamepad disconnected");
            }
            Event::GamepadButton { pad, button, down } => {
                if let Some(state) = &mut self.pads[usize::from(pad)] {
                    let events = state.state.button(button, down);
                    state.emit(&events)?;
                    // The test backend has no games: it answers each press
                    // with a short rumble so the update path can be tested.
                    if down
                        && state.device.is_none()
                        && let Some(sender) = &self.rumble
                    {
                        let _ = sender.send(Rumble {
                            pad,
                            motors: Motors {
                                low: 0x4000,
                                high: 0x8000,
                                duration_ms: 100,
                            },
                        });
                    }
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
        self.rumble = None;
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
        assert_eq!(UI_SET_FFBIT, 0x4004_556b);
        assert_eq!(UI_BEGIN_FF_UPLOAD, 0xc068_55c8);
        assert_eq!(UI_END_FF_UPLOAD, 0x4068_55c9);
        assert_eq!(UI_BEGIN_FF_ERASE, 0xc00c_55ca);
        assert_eq!(UI_END_FF_ERASE, 0x400c_55cb);
    }

    #[test]
    fn rumble_effects_add_up_end_and_stop() {
        let start = Instant::now();
        let at = |ms| start + Duration::from_millis(ms);
        let mut effects = Effects::default();
        assert_eq!(effects.motors(start), Motors::default());
        effects.upload(0, 0x8000, 0x1000, 300);
        effects.upload(1, 0x9000, 0x2000, 0);
        // Uploading alone plays nothing; playing an unknown id is ignored.
        effects.play(7, 1, start);
        assert_eq!(effects.motors(start), Motors::default());
        effects.play(0, 1, start);
        let motors = effects.motors(at(100));
        assert_eq!(
            (motors.low, motors.high, motors.duration_ms),
            (0x8000, 0x1000, 200)
        );
        // A second effect without an end adds up (saturating) and lasts.
        effects.play(1, 1, at(100));
        let motors = effects.motors(at(100));
        assert_eq!((motors.low, motors.high), (u16::MAX, 0x3000));
        assert_eq!(motors.duration_ms, MAX_RUMBLE_MS);
        // The timed one ends by itself; the endless one needs a stop.
        assert_eq!(effects.motors(at(400)).low, 0x9000);
        effects.play(1, 0, at(500));
        assert_eq!(effects.motors(at(500)), Motors::default());
        // Repeats multiply the length; erasing stops a playing effect.
        effects.play(0, 3, at(1000));
        assert_eq!(effects.motors(at(1000)).duration_ms, 900);
        effects.erase(0);
        assert_eq!(effects.motors(at(1000)), Motors::default());
        effects.play(0, 1, at(1000));
        assert_eq!(effects.motors(at(1000)), Motors::default(), "erased");
    }

    #[test]
    fn test_backend_answers_presses_with_rumble_once_enabled() {
        let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel();
        let mut pads = Gamepads::new(false);
        let press = Event::GamepadButton {
            pad: 2,
            button: 0,
            down: true,
        };
        pads.input(&Event::GamepadConnected {
            pad: 2,
            name: "Pad".into(),
        })
        .unwrap();
        pads.input(&press).unwrap();
        assert!(
            receiver.try_recv().is_err(),
            "no rumble before the client asks"
        );
        pads.enable_rumble(sender);
        pads.input(&press).unwrap();
        let rumble = receiver.try_recv().unwrap();
        assert_eq!(rumble.pad, 2);
        assert!(rumble.motors.duration_ms > 0);
        pads.destroy_all();
        pads.input(&Event::GamepadConnected {
            pad: 2,
            name: "Pad".into(),
        })
        .unwrap();
        pads.input(&press).unwrap();
        assert!(
            receiver.try_recv().is_err(),
            "a new session starts without rumble"
        );
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

    /// The created pad's sysfs directory and its evdev node opened the way a
    /// game opens it for force feedback (read and write).
    fn open_evdev(pads: &Gamepads, pad: u8) -> (std::path::PathBuf, std::fs::File) {
        let sysname = pads.pads[usize::from(pad)]
            .as_ref()
            .and_then(|pad| pad.device.as_ref())
            .expect("a virtual pad")
            .sysname()
            .unwrap();
        let sys = std::path::Path::new("/sys/devices/virtual/input").join(&sysname);
        let node = std::fs::read_dir(&sys)
            .unwrap()
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .find(|name| name.starts_with("event"))
            .expect("evdev node for the virtual pad");
        let path = std::path::Path::new("/dev/input").join(&node);
        for _ in 0..50 {
            if let Ok(file) = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(&path)
            {
                return (sys, file);
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        panic!("cannot open {} for reading and writing", path.display());
    }

    fn next_rumble(receiver: &mut tokio::sync::mpsc::UnboundedReceiver<Rumble>) -> Rumble {
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            if let Ok(rumble) = receiver.try_recv() {
                return rumble;
            }
            assert!(Instant::now() < deadline, "no rumble reported");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// A game's view: upload a rumble effect with `EVIOCSFF` (which blocks
    /// until the pad's thread answers the kernel), play it and stop it with
    /// `EV_FF` writes, then erase it. Each step must reach the client side.
    #[test]
    #[ignore = "needs write access to /dev/uinput and read/write access to the created /dev/input/event* node"]
    fn uinput_pad_rumble_reaches_the_client() {
        use std::io::Write;
        const EVIOCSFF: u64 =
            (1 << 30) | ((size_of::<libc::ff_effect>() as u64) << 16) | ((b'E' as u64) << 8) | 0x80;
        const EVIOCRMFF: u64 =
            (1 << 30) | ((size_of::<libc::c_int>() as u64) << 16) | ((b'E' as u64) << 8) | 0x81;
        let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel();
        let mut pads = Gamepads::new(true);
        assert!(pads.available(), "/dev/uinput is not writable");
        pads.enable_rumble(sender);
        pads.input(&Event::GamepadConnected {
            pad: 3,
            name: "test".into(),
        })
        .unwrap();
        let (sys, mut device) = open_evdev(&pads, 3);
        // The capability bitmap is hex words, most significant first.
        let ff = std::fs::read_to_string(sys.join("capabilities/ff")).unwrap();
        let words: Vec<u64> = ff
            .split_whitespace()
            .rev()
            .map(|word| u64::from_str_radix(word, 16).unwrap())
            .collect();
        let bit = usize::from(FF_RUMBLE);
        assert!(
            words
                .get(bit / 64)
                .is_some_and(|word| word >> (bit % 64) & 1 == 1),
            "FF_RUMBLE missing: {ff}"
        );
        let mut effect: libc::ff_effect = unsafe { std::mem::zeroed() };
        effect.type_ = FF_RUMBLE;
        effect.id = -1;
        effect.replay.length = 400;
        unsafe {
            std::ptr::write_unaligned(
                effect.u.as_mut_ptr().cast(),
                libc::ff_rumble_effect {
                    strong_magnitude: 0x8000,
                    weak_magnitude: 0x4000,
                },
            )
        };
        let fd = device.as_raw_fd();
        let result = unsafe { libc::ioctl(fd, EVIOCSFF as _, &mut effect) };
        assert!(result >= 0, "EVIOCSFF: {}", std::io::Error::last_os_error());
        assert!(effect.id >= 0, "kernel assigned an effect id");
        let play = |device: &mut std::fs::File, value: i32| {
            let mut record: libc::input_event = unsafe { std::mem::zeroed() };
            record.type_ = EV_FF;
            record.code = effect.id as u16;
            record.value = value;
            device
                .write_all(unsafe {
                    std::slice::from_raw_parts(
                        (&record as *const libc::input_event).cast::<u8>(),
                        size_of::<libc::input_event>(),
                    )
                })
                .unwrap();
        };
        play(&mut device, 1);
        let rumble = next_rumble(&mut receiver);
        assert_eq!(rumble.pad, 3);
        assert_eq!((rumble.motors.low, rumble.motors.high), (0x8000, 0x4000));
        assert!(
            (300..=400).contains(&rumble.motors.duration_ms),
            "{rumble:?}"
        );
        play(&mut device, 0);
        assert_eq!(next_rumble(&mut receiver).motors, Motors::default());
        let result = unsafe { libc::ioctl(fd, EVIOCRMFF as _, effect.id as libc::c_int) };
        assert!(
            result >= 0,
            "EVIOCRMFF: {}",
            std::io::Error::last_os_error()
        );
        // A pad created without rumble offers no force feedback.
        pads.destroy_all();
        pads.input(&Event::GamepadConnected {
            pad: 0,
            name: "test".into(),
        })
        .unwrap();
        let (sys, _) = open_evdev(&pads, 0);
        let ff = std::fs::read_to_string(sys.join("capabilities/ff")).unwrap();
        assert!(ff.trim().chars().all(|c| c == '0' || c == ' '), "{ff}");
        pads.destroy_all();
    }

    /// What a game using SDL does on the host, and what the client then does
    /// with the physical pad: `SDL_GameControllerRumble` on the virtual pad
    /// must come out as the same motor strengths.
    #[test]
    #[ignore = "needs write access to /dev/uinput, read/write access to /dev/input/event* and SDL joystick hotplug (udev)"]
    fn sdl_rumble_on_the_virtual_pad_reaches_the_client() {
        let sdl = sdl2::init().unwrap();
        let subsystem = sdl.game_controller().unwrap();
        let mut pump = sdl.event_pump().unwrap();
        for _ in pump.poll_iter() {}
        let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel();
        let mut pads = Gamepads::new(true);
        pads.enable_rumble(sender);
        pads.input(&Event::GamepadConnected {
            pad: 0,
            name: "test".into(),
        })
        .unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut controller = None;
        while controller.is_none() && Instant::now() < deadline {
            for event in pump.poll_iter() {
                if let sdl2::event::Event::ControllerDeviceAdded { which, .. } = event {
                    let candidate = subsystem.open(which).unwrap();
                    if candidate.name().contains("Teleport") || candidate.name().contains("360") {
                        controller = Some(candidate);
                    }
                }
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let mut controller = controller.expect("SDL did not report the virtual pad");
        assert!(controller.has_rumble(), "SDL sees no rumble on the pad");
        controller.set_rumble(0x1234, 0xabcd, 500).unwrap();
        let rumble = next_rumble(&mut receiver);
        assert_eq!((rumble.motors.low, rumble.motors.high), (0x1234, 0xabcd));
        assert!(rumble.motors.duration_ms > 0);
        controller.set_rumble(0, 0, 0).unwrap();
        // SDL stops by playing a zero effect or stopping it; either way the
        // motors go quiet.
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            let rumble = next_rumble(&mut receiver);
            if (rumble.motors.low, rumble.motors.high) == (0, 0) {
                break;
            }
            assert!(Instant::now() < deadline, "rumble never stopped");
        }
        drop(controller);
        pads.destroy_all();
    }
}
