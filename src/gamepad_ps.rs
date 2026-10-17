//! DualShock 4 and DualSense reports for `/dev/uhid` pads (parsec-comparison
//! G13).
//!
//! The pads present Sony's USB report descriptors and IDs, so the kernel's
//! `hid-playstation` driver binds and creates the gamepad, touchpad and
//! motion-sensor evdev nodes, lightbar and player LEDs, exactly as for a
//! wired controller. The driver does not read the descriptor's fields: it
//! parses the vendor reports below byte by byte, so their layout follows
//! the driver (`drivers/hid/hid-playstation.c`) and SDL's HIDAPI drivers.
//! The descriptors are the genuine ones as captured in `hid-tools`
//! (`hidtools/device/sony_gamepad.py`, used by the kernel's HID selftests).

use crate::protocol::{GamepadType, TRIGGER_EFFECT_LEN};

pub const SONY: u16 = 0x054c;
/// DualShock 4 (first revision, CUH-ZCT1).
pub const DS4_PRODUCT: u16 = 0x05c4;
pub const DUALSENSE_PRODUCT: u16 = 0x0ce6;

/// USB HID report descriptor of a wired DualShock 4 (CUH-ZCT1).
pub const DS4_DESCRIPTOR: [u8; 507] = [
    0x05, 0x01, 0x09, 0x05, 0xa1, 0x01, 0x85, 0x01, 0x09, 0x30, 0x09, 0x31, 0x09, 0x32, 0x09, 0x35,
    0x15, 0x00, 0x26, 0xff, 0x00, 0x75, 0x08, 0x95, 0x04, 0x81, 0x02, 0x09, 0x39, 0x15, 0x00, 0x25,
    0x07, 0x35, 0x00, 0x46, 0x3b, 0x01, 0x65, 0x14, 0x75, 0x04, 0x95, 0x01, 0x81, 0x42, 0x65, 0x00,
    0x05, 0x09, 0x19, 0x01, 0x29, 0x0e, 0x15, 0x00, 0x25, 0x01, 0x75, 0x01, 0x95, 0x0e, 0x81, 0x02,
    0x06, 0x00, 0xff, 0x09, 0x20, 0x75, 0x06, 0x95, 0x01, 0x15, 0x00, 0x25, 0x7f, 0x81, 0x02, 0x05,
    0x01, 0x09, 0x33, 0x09, 0x34, 0x15, 0x00, 0x26, 0xff, 0x00, 0x75, 0x08, 0x95, 0x02, 0x81, 0x02,
    0x06, 0x00, 0xff, 0x09, 0x21, 0x95, 0x36, 0x81, 0x02, 0x85, 0x05, 0x09, 0x22, 0x95, 0x1f, 0x91,
    0x02, 0x85, 0x04, 0x09, 0x23, 0x95, 0x24, 0xb1, 0x02, 0x85, 0x02, 0x09, 0x24, 0x95, 0x24, 0xb1,
    0x02, 0x85, 0x08, 0x09, 0x25, 0x95, 0x03, 0xb1, 0x02, 0x85, 0x10, 0x09, 0x26, 0x95, 0x04, 0xb1,
    0x02, 0x85, 0x11, 0x09, 0x27, 0x95, 0x02, 0xb1, 0x02, 0x85, 0x12, 0x06, 0x02, 0xff, 0x09, 0x21,
    0x95, 0x0f, 0xb1, 0x02, 0x85, 0x13, 0x09, 0x22, 0x95, 0x16, 0xb1, 0x02, 0x85, 0x14, 0x06, 0x05,
    0xff, 0x09, 0x20, 0x95, 0x10, 0xb1, 0x02, 0x85, 0x15, 0x09, 0x21, 0x95, 0x2c, 0xb1, 0x02, 0x06,
    0x80, 0xff, 0x85, 0x80, 0x09, 0x20, 0x95, 0x06, 0xb1, 0x02, 0x85, 0x81, 0x09, 0x21, 0x95, 0x06,
    0xb1, 0x02, 0x85, 0x82, 0x09, 0x22, 0x95, 0x05, 0xb1, 0x02, 0x85, 0x83, 0x09, 0x23, 0x95, 0x01,
    0xb1, 0x02, 0x85, 0x84, 0x09, 0x24, 0x95, 0x04, 0xb1, 0x02, 0x85, 0x85, 0x09, 0x25, 0x95, 0x06,
    0xb1, 0x02, 0x85, 0x86, 0x09, 0x26, 0x95, 0x06, 0xb1, 0x02, 0x85, 0x87, 0x09, 0x27, 0x95, 0x23,
    0xb1, 0x02, 0x85, 0x88, 0x09, 0x28, 0x95, 0x3f, 0xb1, 0x02, 0x85, 0x89, 0x09, 0x29, 0x95, 0x02,
    0xb1, 0x02, 0x85, 0x90, 0x09, 0x30, 0x95, 0x05, 0xb1, 0x02, 0x85, 0x91, 0x09, 0x31, 0x95, 0x03,
    0xb1, 0x02, 0x85, 0x92, 0x09, 0x32, 0x95, 0x03, 0xb1, 0x02, 0x85, 0x93, 0x09, 0x33, 0x95, 0x0c,
    0xb1, 0x02, 0x85, 0x94, 0x09, 0x34, 0x95, 0x3f, 0xb1, 0x02, 0x85, 0xa0, 0x09, 0x40, 0x95, 0x06,
    0xb1, 0x02, 0x85, 0xa1, 0x09, 0x41, 0x95, 0x01, 0xb1, 0x02, 0x85, 0xa2, 0x09, 0x42, 0x95, 0x01,
    0xb1, 0x02, 0x85, 0xa3, 0x09, 0x43, 0x95, 0x30, 0xb1, 0x02, 0x85, 0xa4, 0x09, 0x44, 0x95, 0x0d,
    0xb1, 0x02, 0x85, 0xf0, 0x09, 0x47, 0x95, 0x3f, 0xb1, 0x02, 0x85, 0xf1, 0x09, 0x48, 0x95, 0x3f,
    0xb1, 0x02, 0x85, 0xf2, 0x09, 0x49, 0x95, 0x0f, 0xb1, 0x02, 0x85, 0xa7, 0x09, 0x4a, 0x95, 0x01,
    0xb1, 0x02, 0x85, 0xa8, 0x09, 0x4b, 0x95, 0x01, 0xb1, 0x02, 0x85, 0xa9, 0x09, 0x4c, 0x95, 0x08,
    0xb1, 0x02, 0x85, 0xaa, 0x09, 0x4e, 0x95, 0x01, 0xb1, 0x02, 0x85, 0xab, 0x09, 0x4f, 0x95, 0x39,
    0xb1, 0x02, 0x85, 0xac, 0x09, 0x50, 0x95, 0x39, 0xb1, 0x02, 0x85, 0xad, 0x09, 0x51, 0x95, 0x0b,
    0xb1, 0x02, 0x85, 0xae, 0x09, 0x52, 0x95, 0x01, 0xb1, 0x02, 0x85, 0xaf, 0x09, 0x53, 0x95, 0x02,
    0xb1, 0x02, 0x85, 0xb0, 0x09, 0x54, 0x95, 0x3f, 0xb1, 0x02, 0x85, 0xe0, 0x09, 0x57, 0x95, 0x02,
    0xb1, 0x02, 0x85, 0xb3, 0x09, 0x55, 0x95, 0x3f, 0xb1, 0x02, 0x85, 0xb4, 0x09, 0x55, 0x95, 0x3f,
    0xb1, 0x02, 0x85, 0xb5, 0x09, 0x56, 0x95, 0x3f, 0xb1, 0x02, 0x85, 0xd0, 0x09, 0x58, 0x95, 0x3f,
    0xb1, 0x02, 0x85, 0xd4, 0x09, 0x59, 0x95, 0x3f, 0xb1, 0x02, 0xc0,
];

/// USB HID report descriptor of a wired DualSense.
pub const DUALSENSE_DESCRIPTOR: [u8; 257] = [
    0x05, 0x01, 0x09, 0x05, 0xa1, 0x01, 0x85, 0x01, 0x09, 0x30, 0x09, 0x31, 0x09, 0x32, 0x09, 0x35,
    0x09, 0x33, 0x09, 0x34, 0x15, 0x00, 0x26, 0xff, 0x00, 0x75, 0x08, 0x95, 0x06, 0x81, 0x02, 0x06,
    0x00, 0xff, 0x09, 0x20, 0x95, 0x01, 0x81, 0x02, 0x05, 0x01, 0x09, 0x39, 0x15, 0x00, 0x25, 0x07,
    0x35, 0x00, 0x46, 0x3b, 0x01, 0x65, 0x14, 0x75, 0x04, 0x95, 0x01, 0x81, 0x42, 0x65, 0x00, 0x05,
    0x09, 0x19, 0x01, 0x29, 0x0f, 0x15, 0x00, 0x25, 0x01, 0x75, 0x01, 0x95, 0x0f, 0x81, 0x02, 0x06,
    0x00, 0xff, 0x09, 0x21, 0x95, 0x0d, 0x81, 0x02, 0x06, 0x00, 0xff, 0x09, 0x22, 0x15, 0x00, 0x26,
    0xff, 0x00, 0x75, 0x08, 0x95, 0x34, 0x81, 0x02, 0x85, 0x02, 0x09, 0x23, 0x95, 0x2f, 0x91, 0x02,
    0x85, 0x05, 0x09, 0x23, 0x95, 0x28, 0xb1, 0x02, 0x85, 0x08, 0x09, 0x24, 0x95, 0x2f, 0xb1, 0x02,
    0x85, 0x09, 0x09, 0x24, 0x95, 0x13, 0xb1, 0x02, 0x85, 0x0a, 0x09, 0x25, 0x95, 0x1a, 0xb1, 0x02,
    0x85, 0x20, 0x09, 0x26, 0x95, 0x3f, 0xb1, 0x02, 0x85, 0x21, 0x09, 0x27, 0x95, 0x04, 0xb1, 0x02,
    0x85, 0x22, 0x09, 0x40, 0x95, 0x3f, 0xb1, 0x02, 0x85, 0x80, 0x09, 0x28, 0x95, 0x3f, 0xb1, 0x02,
    0x85, 0x81, 0x09, 0x29, 0x95, 0x3f, 0xb1, 0x02, 0x85, 0x82, 0x09, 0x2a, 0x95, 0x09, 0xb1, 0x02,
    0x85, 0x83, 0x09, 0x2b, 0x95, 0x3f, 0xb1, 0x02, 0x85, 0x84, 0x09, 0x2c, 0x95, 0x3f, 0xb1, 0x02,
    0x85, 0x85, 0x09, 0x2d, 0x95, 0x02, 0xb1, 0x02, 0x85, 0xa0, 0x09, 0x2e, 0x95, 0x01, 0xb1, 0x02,
    0x85, 0xe0, 0x09, 0x2f, 0x95, 0x3f, 0xb1, 0x02, 0x85, 0xf0, 0x09, 0x30, 0x95, 0x3f, 0xb1, 0x02,
    0x85, 0xf1, 0x09, 0x31, 0x95, 0x3f, 0xb1, 0x02, 0x85, 0xf2, 0x09, 0x32, 0x95, 0x0f, 0xb1, 0x02,
    0xc0,
];

/// What a uhid device presents for one PlayStation type.
pub struct Model {
    pub product: u16,
    /// Manufacturer and product strings as the kernel joins them for a
    /// wired controller.
    pub name: &'static str,
    pub descriptor: &'static [u8],
    /// Touchpad resolution (`hid-playstation`'s ABS_MT ranges).
    pub touch_width: u16,
    pub touch_height: u16,
}

pub fn model(kind: GamepadType) -> Option<&'static Model> {
    const DS4: Model = Model {
        product: DS4_PRODUCT,
        name: "Sony Computer Entertainment Wireless Controller",
        descriptor: &DS4_DESCRIPTOR,
        touch_width: 1920,
        touch_height: 942,
    };
    const DUALSENSE: Model = Model {
        product: DUALSENSE_PRODUCT,
        name: "Sony Interactive Entertainment DualSense Wireless Controller",
        descriptor: &DUALSENSE_DESCRIPTOR,
        touch_width: 1920,
        touch_height: 1080,
    };
    match kind {
        GamepadType::Xbox => None,
        GamepadType::Ds4 => Some(&DS4),
        GamepadType::DualSense => Some(&DUALSENSE),
    }
}

/// Both input reports are 64 bytes with report ID 1.
pub const INPUT_REPORT_LEN: usize = 64;

/// Raw sensor units this pad reports, matching the calibration it answers
/// with: 1/16 °/s for the gyro (≈2000 °/s full scale, a real pad's range)
/// and 1/8192 g for the accelerometer. They are also SDL's and the kernel's
/// defaults for uncalibrated Sony pads.
pub const GYRO_PER_DEGREE_S: f32 = 16.0;
pub const ACCEL_PER_G: f32 = 8192.0;
const STANDARD_GRAVITY: f32 = 9.80665;
/// `gyro_speed_plus`/`minus` in the calibration report (°/s) and the raw
/// reading at that speed.
const CALIBRATION_SPEED: i16 = 540;
const CALIBRATION_GYRO: i16 = CALIBRATION_SPEED * GYRO_PER_DEGREE_S as i16;
const CALIBRATION_ACCEL: i16 = ACCEL_PER_G as i16;

fn to_i16(value: f32) -> i16 {
    value
        .round()
        .clamp(f32::from(i16::MIN), f32::from(i16::MAX)) as i16
}

/// SDL gyro (rad/s) → raw reading.
pub fn raw_gyro(radians_per_second: f32) -> i16 {
    to_i16(radians_per_second.to_degrees() * GYRO_PER_DEGREE_S)
}

/// SDL accelerometer (m/s²) → raw reading.
pub fn raw_accel(meters_per_second_squared: f32) -> i16 {
    to_i16(meters_per_second_squared / STANDARD_GRAVITY * ACCEL_PER_G)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Contact {
    id: u8,
    x: u16,
    y: u16,
}

/// SDL2 `SDL_GameControllerButton` numbers the PlayStation pads use.
const SDL_A: u8 = 0;
const SDL_B: u8 = 1;
const SDL_X: u8 = 2;
const SDL_Y: u8 = 3;
const SDL_BACK: u8 = 4;
const SDL_GUIDE: u8 = 5;
const SDL_START: u8 = 6;
const SDL_LEFT_STICK: u8 = 7;
const SDL_RIGHT_STICK: u8 = 8;
const SDL_LEFT_SHOULDER: u8 = 9;
const SDL_RIGHT_SHOULDER: u8 = 10;
const SDL_DPAD_UP: u8 = 11;
const SDL_DPAD_RIGHT: u8 = 14;
/// SDL maps the DualSense microphone button to MISC1.
const SDL_MISC1: u8 = 15;
const SDL_TOUCHPAD: u8 = 20;

/// Where a button sits in the three button bytes (face/hat, shoulders,
/// system) of both pads.
fn button_bit(kind: GamepadType, button: u8) -> Option<(usize, u8)> {
    Some(match button {
        SDL_X => (0, 4), // square
        SDL_A => (0, 5), // cross
        SDL_B => (0, 6), // circle
        SDL_Y => (0, 7), // triangle
        SDL_LEFT_SHOULDER => (1, 0),
        SDL_RIGHT_SHOULDER => (1, 1),
        SDL_BACK => (1, 4),  // share / create
        SDL_START => (1, 5), // options
        SDL_LEFT_STICK => (1, 6),
        SDL_RIGHT_STICK => (1, 7),
        SDL_GUIDE => (2, 0),                                   // PS
        SDL_TOUCHPAD => (2, 1),                                // touchpad click
        SDL_MISC1 if kind == GamepadType::DualSense => (2, 2), // mute
        _ => return None,
    })
}

/// One PlayStation pad's input state, kept in SDL's terms and packed into a
/// complete input report on every change.
#[derive(Debug, Clone)]
pub struct PsState {
    kind: GamepadType,
    /// The three button bytes without the hat and the digital triggers.
    buttons: [u8; 3],
    /// Up, down, left, right.
    dpad: [bool; 4],
    /// SDL left X/Y, right X/Y.
    sticks: [i16; 4],
    /// SDL 0..=32767.
    triggers: [i16; 2],
    touches: [Option<Contact>; 2],
    next_touch_id: u8,
    gyro: [i16; 3],
    accel: [i16; 3],
    /// Report counter (DS4: bits 2..7 of the third button byte; DualSense:
    /// its own byte).
    counter: u8,
}

impl PsState {
    pub fn new(kind: GamepadType) -> Self {
        Self {
            kind,
            buttons: [0; 3],
            dpad: [false; 4],
            sticks: [0; 4],
            triggers: [0; 2],
            touches: [None; 2],
            next_touch_id: 0,
            gyro: [0; 3],
            // At rest a pad feels gravity (SDL: +Y up, so +1 g on Y).
            accel: [0, CALIBRATION_ACCEL, 0],
            counter: 0,
        }
    }

    /// Whether the button changed the report.
    pub fn button(&mut self, button: u8, down: bool) -> bool {
        if (SDL_DPAD_UP..=SDL_DPAD_RIGHT).contains(&button) {
            let index = usize::from(button - SDL_DPAD_UP);
            let changed = self.dpad[index] != down;
            self.dpad[index] = down;
            return changed;
        }
        let Some((byte, bit)) = button_bit(self.kind, button) else {
            return false;
        };
        let before = self.buttons[byte];
        if down {
            self.buttons[byte] |= 1 << bit;
        } else {
            self.buttons[byte] &= !(1 << bit);
        }
        before != self.buttons[byte]
    }

    pub fn axis(&mut self, axis: u8, value: i16) -> bool {
        let slot = match axis {
            0..=3 => &mut self.sticks[usize::from(axis)],
            4 | 5 => &mut self.triggers[usize::from(axis - 4)],
            _ => return false,
        };
        let value = if axis >= 4 { value.max(0) } else { value };
        let changed = *slot != value;
        *slot = value;
        changed
    }

    /// A finger on (`down`) or off the touchpad at SDL's normalised
    /// position. A new contact gets a new tracking ID, as on a real pad.
    pub fn touch(&mut self, finger: u8, down: bool, x: f32, y: f32) -> bool {
        let Some(slot) = self.touches.get_mut(usize::from(finger)) else {
            return false;
        };
        let Some(model) = model(self.kind) else {
            return false;
        };
        let scale =
            |value: f32, size: u16| (value.clamp(0.0, 1.0) * f32::from(size - 1)).round() as u16;
        let before = *slot;
        *slot = down.then(|| {
            let id = match before {
                Some(contact) => contact.id,
                None => {
                    let id = self.next_touch_id;
                    self.next_touch_id = (id + 1) & 0x7f;
                    id
                }
            };
            Contact {
                id,
                x: scale(x, model.touch_width),
                y: scale(y, model.touch_height),
            }
        });
        before != *slot
    }

    /// SDL gyro (rad/s) and accelerometer (m/s²) readings.
    pub fn motion(&mut self, gyro: [f32; 3], accel: [f32; 3]) -> bool {
        let before = (self.gyro, self.accel);
        self.gyro = gyro.map(raw_gyro);
        self.accel = accel.map(raw_accel);
        before != (self.gyro, self.accel)
    }

    /// Release every button, centre sticks and triggers, lift every finger
    /// and stop rotating; the accelerometer keeps its last reading.
    pub fn neutral(&mut self) {
        self.buttons = [0; 3];
        self.dpad = [false; 4];
        self.sticks = [0; 4];
        self.triggers = [0; 2];
        self.touches = [None; 2];
        self.gyro = [0; 3];
    }

    /// Hat value: 0 north, clockwise to 7 north-west, 8 released.
    fn hat(&self) -> u8 {
        let [up, down, left, right] = self.dpad.map(i8::from);
        match (right - left, down - up) {
            (0, -1) => 0,
            (1, -1) => 1,
            (1, 0) => 2,
            (1, 1) => 3,
            (0, 1) => 4,
            (-1, 1) => 5,
            (-1, 0) => 6,
            (-1, -1) => 7,
            _ => 8,
        }
    }

    /// SDL stick (−32768..=32767, +Y down) → HID byte (0..=255, 0x80
    /// centre, +Y down).
    fn stick(value: i16) -> u8 {
        ((i32::from(value) + 32768) >> 8) as u8
    }

    fn trigger(value: i16) -> u8 {
        ((i32::from(value.max(0)) * 255 + 16383) / 32767) as u8
    }

    fn button_bytes(&self) -> [u8; 3] {
        let mut bytes = self.buttons;
        bytes[0] |= self.hat();
        // L2/R2 also have digital bits, set while the trigger is pressed.
        if self.triggers[0] > 0 {
            bytes[1] |= 1 << 2;
        }
        if self.triggers[1] > 0 {
            bytes[1] |= 1 << 3;
        }
        bytes
    }

    fn put_contacts(&self, report: &mut [u8]) {
        for (index, touch) in self.touches.iter().enumerate() {
            let point = &mut report[index * 4..index * 4 + 4];
            match touch {
                Some(contact) => {
                    point[0] = contact.id & 0x7f;
                    point[1] = contact.x as u8;
                    point[2] = ((contact.x >> 8) & 0x0f) as u8 | ((contact.y & 0x0f) << 4) as u8;
                    point[3] = (contact.y >> 4) as u8;
                }
                // Bit 7: no contact. The last ID stays, like a real pad.
                None => point[0] = 0x80,
            }
        }
    }

    fn put_motion(&self, report: &mut [u8]) {
        for (index, value) in self.gyro.iter().chain(&self.accel).enumerate() {
            report[index * 2..index * 2 + 2].copy_from_slice(&value.to_le_bytes());
        }
    }

    /// The complete USB input report (ID 1). `elapsed_us` is the time since
    /// the pad was created, for the sensor timestamp.
    pub fn report(&mut self, elapsed_us: u64) -> [u8; INPUT_REPORT_LEN] {
        let mut report = [0u8; INPUT_REPORT_LEN];
        report[0] = 0x01;
        let [lx, ly, rx, ry] = self.sticks.map(Self::stick);
        let [l2, r2] = self.triggers.map(Self::trigger);
        let buttons = self.button_bytes();
        self.counter = self.counter.wrapping_add(1);
        match self.kind {
            GamepadType::Ds4 => {
                // struct dualshock4_input_report_usb: common at 1.
                report[1..5].copy_from_slice(&[lx, ly, rx, ry]);
                report[5] = buttons[0];
                report[6] = buttons[1];
                report[7] = buttons[2] | (self.counter & 0x3f) << 2;
                report[8] = l2;
                report[9] = r2;
                // 16-bit timestamp in units of 16/3 µs.
                let timestamp = (elapsed_us * 3 / 16) as u16;
                report[10..12].copy_from_slice(&timestamp.to_le_bytes());
                self.put_motion(&mut report[13..25]);
                // Battery 11 (full) with the cable attached.
                report[30] = 0x10 | 11;
                report[33] = 1; // one touch report
                report[34] = self.counter;
                self.put_contacts(&mut report[35..43]);
            }
            GamepadType::DualSense => {
                // struct dualsense_input_report after the report ID.
                report[1..7].copy_from_slice(&[lx, ly, rx, ry, l2, r2]);
                report[7] = self.counter;
                report[8..11].copy_from_slice(&buttons);
                self.put_motion(&mut report[16..28]);
                // 32-bit timestamp in units of 1/3 µs.
                let timestamp = (elapsed_us * 3) as u32;
                report[28..32].copy_from_slice(&timestamp.to_le_bytes());
                self.put_contacts(&mut report[33..41]);
                // Battery 100 %, charging complete.
                report[53] = 0x20 | 10;
            }
            GamepadType::Xbox => (),
        }
        report
    }
}

/// Motion calibration report, the same layout for both pads over USB:
/// biases, ± readings at `speed` for pitch, yaw and roll, the speeds, then
/// ±1 g readings for X, Y and Z. The values make the drivers' calibration
/// the identity on [`GYRO_PER_DEGREE_S`] and [`ACCEL_PER_G`] units.
fn calibration(id: u8, len: usize) -> Vec<u8> {
    let mut report = vec![0u8; len];
    report[0] = id;
    let mut values = Vec::new();
    values.extend([0i16; 3]);
    for _ in 0..3 {
        values.extend([CALIBRATION_GYRO, -CALIBRATION_GYRO]);
    }
    values.extend([CALIBRATION_SPEED, CALIBRATION_SPEED]);
    for _ in 0..3 {
        values.extend([CALIBRATION_ACCEL, -CALIBRATION_ACCEL]);
    }
    for (index, value) in values.iter().enumerate() {
        report[1 + index * 2..3 + index * 2].copy_from_slice(&value.to_le_bytes());
    }
    report
}

/// Pairing report: the controller's Bluetooth address, least significant
/// byte first, after the report ID. The kernel uses it as the device's
/// unique ID and refuses a second pad with the same address.
fn pairing(id: u8, len: usize, mac: [u8; 6]) -> Vec<u8> {
    let mut report = vec![0u8; len];
    report[0] = id;
    report[1..7].copy_from_slice(&mac);
    report
}

/// Firmware reports as `hid-tools` recorded them from real pads.
const DS4_FIRMWARE: [u8; 49] = [
    0xa3, 0x41, 0x70, 0x72, 0x20, 0x20, 0x38, 0x20, 0x32, 0x30, 0x31, 0x34, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x30, 0x39, 0x3a, 0x34, 0x36, 0x3a, 0x30, 0x36, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x01, 0x00, 0x43, 0x03, 0x00, 0x00, 0x00, 0x51, 0x00, 0x05, 0x00, 0x00, 0x80, 0x03,
    0x00,
];
const DUALSENSE_FIRMWARE: [u8; 64] = [
    0x20, 0x41, 0x75, 0x67, 0x20, 0x31, 0x38, 0x20, 0x32, 0x30, 0x32, 0x30, 0x30, 0x36, 0x3a, 0x32,
    0x30, 0x3a, 0x32, 0x39, 0x03, 0x00, 0x04, 0x00, 0x13, 0x03, 0x00, 0x00, 0x1e, 0x00, 0x00, 0x01,
    0x41, 0x0a, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x04, 0x02, 0x00, 0x00,
    0x2a, 0x00, 0x01, 0x00, 0x06, 0x00, 0x01, 0x00, 0x06, 0x00, 0x00, 0x00, 0x98, 0xd8, 0xb3, 0xb7,
];

/// The answer to a feature-report read (`GET_REPORT`), sized exactly as
/// the descriptor declares; `None` for reports the pad does not serve.
pub fn feature_report(kind: GamepadType, id: u8, mac: [u8; 6]) -> Option<Vec<u8>> {
    match (kind, id) {
        (GamepadType::Ds4, 0x02) => Some(calibration(id, 37)),
        (GamepadType::Ds4, 0x12) => Some(pairing(id, 16, mac)),
        (GamepadType::Ds4, 0x81) => Some(pairing(id, 7, mac)),
        (GamepadType::Ds4, 0xa3) => Some(DS4_FIRMWARE.to_vec()),
        (GamepadType::DualSense, 0x05) => Some(calibration(id, 41)),
        (GamepadType::DualSense, 0x09) => Some(pairing(id, 20, mac)),
        (GamepadType::DualSense, 0x20) => Some(DUALSENSE_FIRMWARE.to_vec()),
        _ => None,
    }
}

/// What one output report (from the kernel driver or a game writing the
/// hidraw node) asks of the pad. Fields are `None` when the report's valid
/// flags leave them unchanged.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Output {
    /// Strong (left, low-frequency) and weak (right) motor, 0..=255.
    pub motors: Option<(u8, u8)>,
    pub lightbar: Option<[u8; 3]>,
    /// DualSense player LEDs, bits 0..=4.
    pub player: Option<u8>,
    pub right_trigger: Option<[u8; TRIGGER_EFFECT_LEN]>,
    pub left_trigger: Option<[u8; TRIGGER_EFFECT_LEN]>,
}

/// Parse a USB (DS4 0x05, DualSense 0x02) or Bluetooth (0x11, 0x31) output
/// report. `None` for other reports and short ones.
pub fn parse_output(kind: GamepadType, data: &[u8]) -> Option<Output> {
    let mut output = Output::default();
    match (kind, *data.first()?) {
        (GamepadType::Ds4, id @ (0x05 | 0x11)) => {
            // struct dualshock4_output_report_common
            let common = data.get(if id == 0x05 { 1 } else { 3 }..)?.get(..8)?;
            if common[0] & 0x01 != 0 {
                output.motors = Some((common[4], common[3]));
            }
            if common[0] & 0x02 != 0 {
                output.lightbar = Some([common[5], common[6], common[7]]);
            }
        }
        (GamepadType::DualSense, id @ (0x02 | 0x31)) => {
            // struct dualsense_output_report_common
            let common = data.get(if id == 0x02 { 1 } else { 3 }..)?.get(..47)?;
            let effect = |start: usize| {
                let mut bytes = [0u8; TRIGGER_EFFECT_LEN];
                bytes.copy_from_slice(&common[start..start + TRIGGER_EFFECT_LEN]);
                bytes
            };
            // Compatible vibration or haptics select: either carries the
            // motor bytes.
            if common[0] & 0x03 != 0 {
                output.motors = Some((common[3], common[2]));
            }
            if common[0] & 0x04 != 0 {
                output.right_trigger = Some(effect(10));
            }
            if common[0] & 0x08 != 0 {
                output.left_trigger = Some(effect(21));
            }
            if common[1] & 0x04 != 0 {
                output.lightbar = Some([common[44], common[45], common[46]]);
            }
            if common[1] & 0x10 != 0 {
                output.player = Some(common[43] & 0x1f);
            }
        }
        _ => return None,
    }
    Some(output)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn le16(report: &[u8], at: usize) -> i16 {
        i16::from_le_bytes([report[at], report[at + 1]])
    }

    /// Decode a touch point the way `hid-playstation` does.
    fn point(bytes: &[u8]) -> (bool, u8, u16, u16) {
        let active = bytes[0] & 0x80 == 0;
        let x = u16::from(bytes[1]) | (u16::from(bytes[2] & 0x0f) << 8);
        let y = u16::from(bytes[2] >> 4) | (u16::from(bytes[3]) << 4);
        (active, bytes[0] & 0x7f, x, y)
    }

    #[test]
    fn descriptors_are_sonys() {
        // Report ID 1 input; DS4 output 5, feature 2/0x12/0x81/0xa3;
        // DualSense output 2, feature 5/9/0x20.
        assert_eq!(
            &DS4_DESCRIPTOR[..8],
            &[0x05, 0x01, 0x09, 0x05, 0xa1, 0x01, 0x85, 0x01]
        );
        assert_eq!(DS4_DESCRIPTOR.last(), Some(&0xc0));
        assert_eq!(
            &DUALSENSE_DESCRIPTOR[..8],
            &[0x05, 0x01, 0x09, 0x05, 0xa1, 0x01, 0x85, 0x01]
        );
        assert_eq!(DUALSENSE_DESCRIPTOR.last(), Some(&0xc0));
        let declares = |descriptor: &[u8], id: u8, kind: u8, count: u8| {
            descriptor.windows(8).any(|w| {
                w[0] == 0x85 && w[1] == id && w[4] == 0x95 && w[5] == count && w[6] == kind
            })
        };
        // Output and feature sizes the parser and feature_report rely on
        // (Report Count is the size without the ID byte).
        assert!(declares(&DS4_DESCRIPTOR, 0x05, 0x91, 31));
        assert!(declares(&DS4_DESCRIPTOR, 0x02, 0xb1, 36));
        assert!(declares(&DS4_DESCRIPTOR, 0x81, 0xb1, 6));
        assert!(declares(&DS4_DESCRIPTOR, 0xa3, 0xb1, 48));
        assert!(declares(&DUALSENSE_DESCRIPTOR, 0x02, 0x91, 47));
        assert!(declares(&DUALSENSE_DESCRIPTOR, 0x05, 0xb1, 40));
        assert!(declares(&DUALSENSE_DESCRIPTOR, 0x09, 0xb1, 19));
        assert!(declares(&DUALSENSE_DESCRIPTOR, 0x20, 0xb1, 63));
        assert_eq!(model(GamepadType::Ds4).unwrap().product, 0x05c4);
        assert_eq!(model(GamepadType::DualSense).unwrap().product, 0x0ce6);
        assert!(model(GamepadType::Xbox).is_none());
    }

    #[test]
    fn neutral_reports_match_a_resting_pad() {
        for kind in [GamepadType::Ds4, GamepadType::DualSense] {
            let mut state = PsState::new(kind);
            let report = state.report(0);
            assert_eq!(report.len(), 64);
            assert_eq!(report[0], 1);
            let (sticks, buttons, touch) = match kind {
                GamepadType::Ds4 => (1..5, 5, 35),
                _ => (1..5, 8, 33),
            };
            assert!(
                report[sticks].iter().all(|b| *b == 0x80),
                "{kind:?} centred"
            );
            assert_eq!(report[buttons], 8, "{kind:?} hat released, no buttons");
            assert_eq!(report[buttons + 1], 0);
            assert_eq!(report[touch] & 0x80, 0x80, "{kind:?} no finger");
            assert_eq!(report[touch + 4] & 0x80, 0x80);
        }
    }

    #[test]
    fn buttons_hat_sticks_and_triggers_pack_like_the_driver_reads_them() {
        let mut ds4 = PsState::new(GamepadType::Ds4);
        assert!(ds4.button(SDL_A, true));
        assert!(!ds4.button(SDL_A, true), "repeat press changes nothing");
        ds4.button(SDL_Y, true);
        ds4.button(SDL_LEFT_SHOULDER, true);
        ds4.button(SDL_START, true);
        ds4.button(SDL_GUIDE, true);
        ds4.button(SDL_TOUCHPAD, true);
        assert!(!ds4.button(SDL_MISC1, true), "no mute button on a DS4");
        assert!(!ds4.button(16, true), "paddles are ignored");
        ds4.button(SDL_DPAD_UP, true);
        ds4.button(SDL_DPAD_RIGHT, true);
        ds4.axis(0, i16::MIN);
        ds4.axis(1, i16::MAX);
        ds4.axis(3, 0);
        ds4.axis(4, i16::MAX);
        ds4.axis(5, -5);
        let report = ds4.report(0);
        assert_eq!(&report[1..5], &[0x00, 0xff, 0x80, 0x80]);
        // Cross 0x20 | triangle 0x80 | hat north-east 1.
        assert_eq!(report[5], 0xa1);
        // L1 0x01 | L2 digital 0x04 | options 0x20.
        assert_eq!(report[6], 0x25);
        // PS | touchpad click, counter above.
        assert_eq!(report[7] & 0x03, 0x03);
        assert_eq!((report[8], report[9]), (255, 0));
        let mut dualsense = PsState::new(GamepadType::DualSense);
        dualsense.button(SDL_B, true);
        dualsense.button(SDL_X, true);
        dualsense.button(SDL_MISC1, true);
        dualsense.button(SDL_RIGHT_STICK, true);
        dualsense.button(13, true); // left
        dualsense.button(12, true); // down
        dualsense.axis(2, 16384);
        dualsense.axis(5, 16384);
        let report = dualsense.report(0);
        assert_eq!(&report[1..7], &[0x80, 0x80, 0xc0, 0x80, 0, 128]);
        // Circle 0x40 | square 0x10 | hat south-west 5.
        assert_eq!(report[8], 0x55);
        // R3 0x80 | R2 digital 0x08.
        assert_eq!(report[9], 0x88);
        assert_eq!(report[10], 0x04, "mute");
        let counter = report[7];
        assert_eq!(dualsense.report(0)[7], counter.wrapping_add(1));
        dualsense.neutral();
        let report = dualsense.report(0);
        assert_eq!((report[8], report[9], report[10]), (8, 0, 0));
        assert_eq!(report[3], 0x80);
    }

    #[test]
    fn touch_contacts_pack_twelve_bit_positions_and_ids() {
        for (kind, offset, height) in [
            (GamepadType::Ds4, 35, 942),
            (GamepadType::DualSense, 33, 1080),
        ] {
            let mut state = PsState::new(kind);
            assert!(state.touch(0, true, 0.0, 0.0));
            assert!(state.touch(1, true, 1.0, 1.0));
            assert!(!state.touch(2, true, 0.5, 0.5), "two contacts only");
            let report = state.report(0);
            assert_eq!(point(&report[offset..]), (true, 0, 0, 0));
            assert_eq!(point(&report[offset + 4..]), (true, 1, 1919, height - 1));
            // Moving keeps the ID; lifting marks the point inactive; a new
            // contact gets a new ID.
            state.touch(0, true, 0.5, 0.25);
            let report = state.report(0);
            let (active, id, x, y) = point(&report[offset..]);
            assert!(active && id == 0);
            assert_eq!(x, 960);
            assert_eq!(y, ((height - 1) as f32 * 0.25).round() as u16);
            assert!(state.touch(0, false, 0.0, 0.0));
            assert!(!point(&state.report(0)[offset..]).0);
            state.touch(0, true, 0.1, 0.1);
            assert_eq!(point(&state.report(0)[offset..]).1, 2);
            state.neutral();
            let report = state.report(0);
            assert!(!point(&report[offset..]).0 && !point(&report[offset + 4..]).0);
        }
    }

    #[test]
    fn motion_is_reported_in_calibrated_raw_units() {
        let gravity = 9.80665;
        let speed = 90f32.to_radians();
        for (kind, gyro_at, accel_at) in
            [(GamepadType::Ds4, 13, 19), (GamepadType::DualSense, 16, 22)]
        {
            let mut state = PsState::new(kind);
            assert!(state.motion([speed, -speed, 0.0], [0.0, gravity, -2.0 * gravity]));
            let report = state.report(0);
            assert_eq!(le16(&report, gyro_at), 1440, "{kind:?} 90 °/s at 1/16 °/s");
            assert_eq!(le16(&report, gyro_at + 2), -1440);
            assert_eq!(le16(&report, gyro_at + 4), 0);
            assert_eq!(le16(&report, accel_at), 0);
            assert_eq!(le16(&report, accel_at + 2), 8192, "1 g");
            assert_eq!(le16(&report, accel_at + 4), -16384);
            // Saturates instead of wrapping.
            state.motion([100.0, 0.0, 0.0], [0.0; 3]);
            assert_eq!(le16(&state.report(0), gyro_at), i16::MAX);
        }
        // Timestamps: DS4 16-bit in 16/3 µs, DualSense 32-bit in 1/3 µs.
        let mut ds4 = PsState::new(GamepadType::Ds4);
        assert_eq!(le16(&ds4.report(16_000), 10), 3000);
        let mut dualsense = PsState::new(GamepadType::DualSense);
        let report = dualsense.report(1000);
        assert_eq!(u32::from_le_bytes(report[28..32].try_into().unwrap()), 3000);
    }

    /// The kernel's calibration arithmetic (hid-playstation
    /// `dualsense_get_calibration_data`/`dualshock4_get_calibration_data`)
    /// applied to our report must turn raw readings into its evdev units:
    /// 1/1024 °/s and 1/8192 g.
    #[test]
    fn calibration_makes_the_kernel_scale_exact() {
        for (kind, id, len) in [
            (GamepadType::Ds4, 0x02, 37),
            (GamepadType::DualSense, 0x05, 41),
        ] {
            let report = feature_report(kind, id, [0; 6]).unwrap();
            assert_eq!(report.len(), len);
            assert_eq!(report[0], id);
            let value = |at: usize| i32::from(le16(&report, at));
            let speed_2x = value(19) + value(21);
            for axis in 0..3 {
                let bias = value(1 + axis * 2);
                let plus = value(7 + axis * 4);
                let minus = value(9 + axis * 4);
                let numerator = speed_2x * 1024;
                let denominator = (plus - bias).abs() + (minus - bias).abs();
                // 90 °/s raw → evdev.
                assert_eq!(1440 * numerator / denominator, 90 * 1024);
                let accel_plus = value(23 + axis * 4);
                let accel_minus = value(25 + axis * 4);
                let range_2g = accel_plus - accel_minus;
                let accel_bias = accel_plus - range_2g / 2;
                assert_eq!((8192 - accel_bias) * 2 * 8192 / range_2g, 8192);
            }
        }
        let mac = [1, 2, 3, 4, 5, 6];
        let pairing = feature_report(GamepadType::DualSense, 0x09, mac).unwrap();
        assert_eq!((pairing.len(), &pairing[1..7]), (20, &mac[..]));
        assert_eq!(
            feature_report(GamepadType::Ds4, 0x12, mac).unwrap().len(),
            16
        );
        assert_eq!(
            feature_report(GamepadType::Ds4, 0x81, mac).unwrap()[1..7],
            mac
        );
        assert_eq!(
            feature_report(GamepadType::Ds4, 0xa3, mac).unwrap().len(),
            49
        );
        assert_eq!(
            feature_report(GamepadType::DualSense, 0x20, mac)
                .unwrap()
                .len(),
            64
        );
        assert!(feature_report(GamepadType::DualSense, 0x02, mac).is_none());
        assert!(feature_report(GamepadType::Xbox, 0x05, mac).is_none());
    }

    #[test]
    fn output_reports_from_the_driver_and_games_parse() {
        // hid-playstation's DS4 USB report: motors and lightbar valid.
        let mut ds4 = [0u8; 32];
        ds4[0] = 0x05;
        ds4[1] = 0x01 | 0x02 | 0x04;
        ds4[4] = 0x40; // right, weak
        ds4[5] = 0xc0; // left, strong
        ds4[6..9].copy_from_slice(&[0x00, 0x00, 0x40]);
        assert_eq!(
            parse_output(GamepadType::Ds4, &ds4),
            Some(Output {
                motors: Some((0xc0, 0x40)),
                lightbar: Some([0, 0, 0x40]),
                ..Output::default()
            })
        );
        // Lightbar only: the motors are left alone.
        ds4[1] = 0x02;
        assert_eq!(parse_output(GamepadType::Ds4, &ds4).unwrap().motors, None);
        // Bluetooth form: common part after two header bytes.
        let mut bluetooth = [0u8; 78];
        bluetooth[0] = 0x11;
        bluetooth[3] = 0x01;
        bluetooth[6] = 0x11;
        bluetooth[7] = 0x22;
        assert_eq!(
            parse_output(GamepadType::Ds4, &bluetooth).unwrap().motors,
            Some((0x22, 0x11))
        );
        assert!(parse_output(GamepadType::Ds4, &ds4[..5]).is_none(), "short");
        assert!(
            parse_output(GamepadType::Ds4, &[0x02; 48]).is_none(),
            "not a DS4 report"
        );
        // DualSense USB: rumble, both triggers, lightbar and player LEDs.
        let mut dualsense = [0u8; 63];
        dualsense[0] = 0x02;
        dualsense[1] = 0x01 | 0x02 | 0x04 | 0x08;
        dualsense[2] = 0x04 | 0x10;
        dualsense[3] = 0x10; // right, weak
        dualsense[4] = 0xf0; // left, strong
        dualsense[11] = 0x21; // right trigger: feedback mode
        dualsense[12] = 0x7f;
        dualsense[22] = 0x26; // left trigger: vibration mode
        dualsense[32] = 0x09;
        dualsense[44] = 0b10101 | 0x20;
        dualsense[45..48].copy_from_slice(&[0xff, 0x80, 0x01]);
        let output = parse_output(GamepadType::DualSense, &dualsense).unwrap();
        assert_eq!(output.motors, Some((0xf0, 0x10)));
        assert_eq!(output.lightbar, Some([0xff, 0x80, 0x01]));
        assert_eq!(output.player, Some(0b10101));
        let right = output.right_trigger.unwrap();
        assert_eq!((right[0], right[1]), (0x21, 0x7f));
        let left = output.left_trigger.unwrap();
        assert_eq!((left[0], left[10]), (0x26, 0x09));
        // The kernel's player-LED-only update leaves the rest alone.
        let mut player = [0u8; 63];
        player[0] = 0x02;
        player[2] = 0x10;
        player[44] = 0b00100;
        assert_eq!(
            parse_output(GamepadType::DualSense, &player),
            Some(Output {
                player: Some(0b00100),
                ..Output::default()
            })
        );
        let mut bluetooth = [0u8; 78];
        bluetooth[0] = 0x31;
        bluetooth[3] = 0x02;
        bluetooth[5] = 0x33;
        bluetooth[6] = 0x44;
        assert_eq!(
            parse_output(GamepadType::DualSense, &bluetooth)
                .unwrap()
                .motors,
            Some((0x44, 0x33))
        );
        assert!(parse_output(GamepadType::DualSense, &dualsense[..40]).is_none());
        assert!(parse_output(GamepadType::Xbox, &dualsense).is_none());
        // The client replays the trigger blocks through SDL.
        let effect = crate::protocol::dualsense_trigger_effect(&right, &left);
        assert_eq!(effect[0], 0x0c);
        let replayed =
            parse_output(GamepadType::DualSense, &[&[0x02][..], &effect].concat()).unwrap();
        assert_eq!(replayed.right_trigger, Some(right));
        assert_eq!(replayed.left_trigger, Some(left));
        assert_eq!((replayed.motors, replayed.lightbar), (None, None));
    }
}
