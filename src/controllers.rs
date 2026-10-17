//! Client game controllers: SDL2 GameController → `Gamepad*` events.
//!
//! SDL normalises every supported controller to the Xbox layout, so its
//! button/axis numbers go on the wire unchanged. Pads are only announced to
//! hosts advertising `Desktop::gamepads`; older hosts reject unknown events.
//! Buttons are sent at once and reliably. Axis motion is coalesced to the
//! latest value per axis and flushed once per loop iteration; an axis whose
//! value was dropped by a full queue stays dirty and is retried.
//!
//! A PlayStation controller is emulated as the same PlayStation pad when
//! the host offers it (`Desktop::gamepad_types`; `--gamepad-type` overrides
//! the choice). Its touchpad contacts and motion sensors then go to the
//! host the same coalesced, retried way, and the host's lightbar, player
//! LEDs and DualSense trigger effects play on it.

use super::send;
use crate::protocol::{
    Event, GAMEPAD_AXES, GamepadOffer, GamepadType, MAX_GAMEPAD_NAME, MAX_GAMEPADS, MAX_MOTION,
    TOUCHPAD_FINGERS, TRIGGER_EFFECT_LEN,
};
use anyhow::Result;
use sdl2::{
    GameControllerSubsystem, controller::GameController, event::Event as SdlEvent,
    sensor::SensorType,
};
use tokio::sync::mpsc;

const AXES: usize = GAMEPAD_AXES as usize;
const FINGERS: usize = TOUCHPAD_FINGERS as usize;

/// `--gamepad-type`: which pad the host emulates for each controller.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, clap::ValueEnum)]
pub enum GamepadChoice {
    /// A DualShock 4 for PlayStation 4 controllers, a DualSense for
    /// PlayStation 5 ones, an Xbox 360 pad for everything else.
    #[default]
    Auto,
    Xbox,
    Ds4,
    #[value(name = "dualsense")]
    DualSense,
}

/// SDL2 `SDL_GameControllerType` values the automatic choice uses.
const SDL_CONTROLLER_TYPE_PS4: i32 = 4;
const SDL_CONTROLLER_TYPE_PS5: i32 = 7;

impl GamepadChoice {
    /// The pad wanted for a controller SDL reports as `sdl_type`.
    pub fn wants(self, sdl_type: i32) -> GamepadType {
        match self {
            Self::Auto => match sdl_type {
                SDL_CONTROLLER_TYPE_PS4 => GamepadType::Ds4,
                SDL_CONTROLLER_TYPE_PS5 => GamepadType::DualSense,
                _ => GamepadType::Xbox,
            },
            Self::Xbox => GamepadType::Xbox,
            Self::Ds4 => GamepadType::Ds4,
            Self::DualSense => GamepadType::DualSense,
        }
    }
}

unsafe extern "C" {
    /// Declared here with an `int` result: SDL versions newer than the
    /// bindings return types the Rust enum does not know.
    #[link_name = "SDL_GameControllerTypeForIndex"]
    fn controller_type_for_index(joystick_index: libc::c_int) -> libc::c_int;
}

/// DualSense player-LED patterns (hid-playstation, SDL) → player index.
fn player_index(leds: u8) -> Option<u32> {
    [0b00100, 0b01010, 0b10101, 0b11011, 0b11111]
        .iter()
        .position(|pattern| *pattern == leds)
        .map(|index| index as u32)
}

struct Slot<C> {
    handle: C,
    instance: u32,
    name: String,
    announced: bool,
    /// The pad this controller should become, and what the host made of it
    /// once announced.
    wants: GamepadType,
    kind: GamepadType,
    /// Latest value from the controller and the last value queued to the host.
    current: [i16; AXES],
    sent: [i16; AXES],
    touch: [Option<(f32, f32)>; FINGERS],
    touch_sent: [Option<(f32, f32)>; FINGERS],
    /// Gyro samples since the last send, averaged so a coalesced reading
    /// keeps the rotation of every sample; the latest accelerometer reading.
    gyro_sum: [f32; 3],
    gyro_samples: u32,
    gyro: [f32; 3],
    accel: [f32; 3],
    motion_dirty: bool,
}

/// Slot bookkeeping, independent of SDL so it can be unit tested.
pub struct Pads<C> {
    slots: [Option<Slot<C>>; MAX_GAMEPADS as usize],
}

impl<C> Default for Pads<C> {
    fn default() -> Self {
        Self {
            slots: std::array::from_fn(|_| None),
        }
    }
}

fn wire_name(name: &str) -> String {
    let mut clean = String::new();
    for c in name.chars().map(|c| if c.is_control() { ' ' } else { c }) {
        if clean.len() + c.len_utf8() > MAX_GAMEPAD_NAME {
            break;
        }
        clean.push(c);
    }
    clean
}

fn motion_value(value: f32) -> f32 {
    if value.is_finite() {
        value.clamp(-MAX_MOTION, MAX_MOTION)
    } else {
        0.0
    }
}

impl<C> Pads<C> {
    fn slot(&self, instance: u32) -> Option<usize> {
        self.slots
            .iter()
            .position(|slot| slot.as_ref().is_some_and(|slot| slot.instance == instance))
    }

    fn slot_mut(&mut self, instance: u32) -> Option<&mut Slot<C>> {
        let index = self.slot(instance)?;
        self.slots[index].as_mut()
    }

    /// Take the first free slot. `None` when the instance is already attached
    /// or every slot is in use.
    pub fn attach(
        &mut self,
        instance: u32,
        name: &str,
        axes: [i16; AXES],
        wants: GamepadType,
        handle: C,
    ) -> Option<u8> {
        if self.slot(instance).is_some() {
            return None;
        }
        let index = self.slots.iter().position(Option::is_none)?;
        self.slots[index] = Some(Slot {
            handle,
            instance,
            name: wire_name(name),
            announced: false,
            wants,
            kind: GamepadType::Xbox,
            current: axes,
            // A new host pad starts centred; any held deflection is re-sent.
            sent: [0; AXES],
            touch: [None; FINGERS],
            touch_sent: [None; FINGERS],
            gyro_sum: [0.0; 3],
            gyro_samples: 0,
            gyro: [0.0; 3],
            accel: [0.0; 3],
            motion_dirty: false,
        });
        Some(index as u8)
    }

    /// `GamepadDisconnected` when the host had been told about this pad.
    pub fn detach(&mut self, instance: u32) -> Option<Event> {
        let index = self.slot(instance)?;
        let slot = self.slots[index].take()?;
        slot.announced
            .then_some(Event::GamepadDisconnected { pad: index as u8 })
    }

    /// `GamepadConnected` for every pad the host has not been told about,
    /// as the type it wants if the host offers it.
    pub fn announce(&mut self, offer: GamepadOffer) -> Vec<Event> {
        let mut events = Vec::new();
        for (pad, slot) in self.slots.iter_mut().enumerate() {
            if let Some(slot) = slot.as_mut().filter(|slot| !slot.announced) {
                slot.announced = true;
                slot.kind = offer.resolve(slot.wants);
                tracing::info!(
                    pad,
                    controller = %slot.name,
                    emulated = %slot.kind.label(),
                    "gamepad announced"
                );
                events.push(Event::GamepadConnected {
                    pad: pad as u8,
                    name: slot.name.clone(),
                    kind: slot.kind,
                });
            }
        }
        events
    }

    pub fn axis(&mut self, instance: u32, axis: u8, value: i16) {
        if let Some(slot) = self.slot_mut(instance)
            && usize::from(axis) < AXES
        {
            // Triggers are 0..=32767 in SDL; keep that invariant on the wire.
            slot.current[usize::from(axis)] = if axis >= crate::protocol::GAMEPAD_FIRST_TRIGGER {
                value.max(0)
            } else {
                value
            };
        }
    }

    /// A touchpad finger moved (`Some`) or lifted (`None`).
    pub fn touch(&mut self, instance: u32, finger: u32, position: Option<(f32, f32)>) {
        if let Some(slot) = self.slot_mut(instance)
            && let Some(touch) = slot.touch.get_mut(finger as usize)
        {
            let clamp = |value: f32| {
                if value.is_finite() {
                    value.clamp(0.0, 1.0)
                } else {
                    0.0
                }
            };
            *touch = position.map(|(x, y)| (clamp(x), clamp(y)));
        }
    }

    pub fn gyro(&mut self, instance: u32, data: [f32; 3]) {
        if let Some(slot) = self.slot_mut(instance) {
            let data = data.map(motion_value);
            for (sum, value) in slot.gyro_sum.iter_mut().zip(data) {
                *sum += value;
            }
            slot.gyro_samples += 1;
            slot.gyro = data;
            slot.motion_dirty = true;
        }
    }

    pub fn accel(&mut self, instance: u32, data: [f32; 3]) {
        if let Some(slot) = self.slot_mut(instance) {
            slot.accel = data.map(motion_value);
            slot.motion_dirty = true;
        }
    }

    /// Pending axis values, touch contacts and motion for announced pads,
    /// oldest pad first. Pass `Some` to restrict to one controller (before
    /// one of its buttons).
    pub fn dirty(&self, only: Option<u32>) -> Vec<Event> {
        let mut events = Vec::new();
        for (pad, slot) in self.slots.iter().enumerate() {
            let Some(slot) = slot else { continue };
            if !slot.announced || only.is_some_and(|instance| instance != slot.instance) {
                continue;
            }
            let pad = pad as u8;
            for axis in 0..AXES {
                if slot.current[axis] != slot.sent[axis] {
                    events.push(Event::GamepadAxis {
                        pad,
                        axis: axis as u8,
                        value: slot.current[axis],
                    });
                }
            }
            if slot.kind.is_xbox() {
                continue;
            }
            for finger in 0..FINGERS {
                if slot.touch[finger] != slot.touch_sent[finger] {
                    let (x, y) = slot.touch[finger]
                        .or(slot.touch_sent[finger])
                        .unwrap_or_default();
                    events.push(Event::GamepadTouch {
                        pad,
                        finger: finger as u8,
                        down: slot.touch[finger].is_some(),
                        x,
                        y,
                    });
                }
            }
            if slot.motion_dirty {
                let gyro = if slot.gyro_samples > 0 {
                    slot.gyro_sum.map(|sum| sum / slot.gyro_samples as f32)
                } else {
                    slot.gyro
                };
                events.push(Event::GamepadMotion {
                    pad,
                    gyro,
                    accel: slot.accel,
                });
            }
        }
        events
    }

    pub fn mark_sent(&mut self, event: &Event) {
        match *event {
            Event::GamepadAxis { pad, axis, value } => {
                if let Some(Some(slot)) = self.slots.get_mut(usize::from(pad))
                    && usize::from(axis) < AXES
                {
                    slot.sent[usize::from(axis)] = value;
                }
            }
            Event::GamepadTouch {
                pad,
                finger,
                down,
                x,
                y,
            } => {
                if let Some(Some(slot)) = self.slots.get_mut(usize::from(pad))
                    && let Some(sent) = slot.touch_sent.get_mut(usize::from(finger))
                {
                    *sent = down.then_some((x, y));
                }
            }
            Event::GamepadMotion { pad, .. } => {
                if let Some(Some(slot)) = self.slots.get_mut(usize::from(pad)) {
                    slot.gyro_sum = [0.0; 3];
                    slot.gyro_samples = 0;
                    slot.motion_dirty = false;
                }
            }
            _ => (),
        }
    }

    /// The wire event for a button, only once the host knows the pad.
    pub fn button(&self, instance: u32, button: u8, down: bool) -> Option<Event> {
        let index = self.slot(instance)?;
        self.slots[index]
            .as_ref()
            .filter(|slot| slot.announced)
            .map(|_| Event::GamepadButton {
                pad: index as u8,
                button,
                down,
            })
    }

    pub fn len(&self) -> usize {
        self.slots.iter().flatten().count()
    }

    /// The controller behind the host's pad `pad`, once the host knows it.
    pub fn handle_mut(&mut self, pad: u8) -> Option<&mut C> {
        self.slots
            .get_mut(usize::from(pad))?
            .as_mut()
            .filter(|slot| slot.announced)
            .map(|slot| &mut slot.handle)
    }

    /// The controller behind the host's PlayStation pad `pad`.
    pub fn playstation_mut(&mut self, pad: u8) -> Option<&mut C> {
        self.slots
            .get_mut(usize::from(pad))?
            .as_mut()
            .filter(|slot| slot.announced && !slot.kind.is_xbox())
            .map(|slot| &mut slot.handle)
    }
}

/// Queue a lossy event (axis, touch, motion); `false` when the queue was
/// full and it must be retried.
fn try_send_axis(events: &mpsc::Sender<Event>, event: Event) -> Result<bool> {
    event.validate()?;
    match events.try_send(event) {
        Ok(()) => Ok(true),
        Err(mpsc::error::TrySendError::Full(_)) => Ok(false),
        Err(error) => Err(anyhow::anyhow!("input queue disconnected: {error}")),
    }
}

/// SDL controllers opened for one session.
pub struct Controllers {
    subsystem: Option<GameControllerSubsystem>,
    pads: Pads<GameController>,
    choice: GamepadChoice,
    notice: Option<String>,
    unsupported_noticed: bool,
}

impl Controllers {
    pub fn new(sdl: &sdl2::Sdl, choice: GamepadChoice) -> Self {
        let subsystem = sdl
            .game_controller()
            .inspect_err(|error| tracing::warn!(%error, "game controllers unavailable"))
            .ok();
        let mut controllers = Self {
            subsystem,
            pads: Pads::default(),
            choice,
            notice: None,
            unsupported_noticed: false,
        };
        let count = controllers
            .subsystem
            .as_ref()
            .and_then(|subsystem| subsystem.num_joysticks().ok())
            .unwrap_or(0);
        for index in 0..count {
            controllers.open(index);
        }
        controllers
    }

    fn open(&mut self, index: u32) {
        let Some(subsystem) = &self.subsystem else {
            return;
        };
        if !subsystem.is_game_controller(index) {
            return;
        }
        let sdl_type = unsafe { controller_type_for_index(index as libc::c_int) };
        let controller = match subsystem.open(index) {
            Ok(controller) => controller,
            Err(error) => {
                tracing::warn!(%error, index, "could not open game controller");
                return;
            }
        };
        let instance = controller.instance_id();
        if self.pads.slot(instance).is_some() {
            return; // already open (enumerated and then announced by SDL)
        }
        let name = controller.name();
        let wants = self.choice.wants(sdl_type);
        if !wants.is_xbox() {
            // Sensor events only flow once enabled; forwarded only if the
            // host makes this a PlayStation pad.
            for sensor in [SensorType::Gyroscope, SensorType::Accelerometer] {
                if controller.has_sensor(sensor)
                    && let Err(error) = controller.sensor_set_enabled(sensor, true)
                {
                    tracing::debug!(%error, ?sensor, "controller sensor unavailable");
                }
            }
        }
        let axes = [
            sdl2::controller::Axis::LeftX,
            sdl2::controller::Axis::LeftY,
            sdl2::controller::Axis::RightX,
            sdl2::controller::Axis::RightY,
            sdl2::controller::Axis::TriggerLeft,
            sdl2::controller::Axis::TriggerRight,
        ]
        .map(|axis| controller.axis(axis));
        match self.pads.attach(instance, &name, axes, wants, controller) {
            Some(pad) => {
                tracing::info!(
                    pad,
                    controller = %name,
                    sdl_type,
                    wants = %wants.label(),
                    "game controller attached"
                );
                self.notice = Some(format!("Gamepad {} connected: {name}", pad + 1));
            }
            None => {
                self.notice = Some(format!(
                    "Only {MAX_GAMEPADS} gamepads are forwarded; ignoring {name}"
                ));
            }
        }
    }

    /// Play the host's `Update::Rumble` for pad `pad` on its controller.
    /// Controllers without motors ignore it.
    pub fn rumble(&mut self, pad: u8, low: u16, high: u16, duration_ms: u32) {
        if let Some(controller) = self.pads.handle_mut(pad)
            && let Err(error) = controller.set_rumble(low, high, duration_ms)
        {
            tracing::debug!(pad, %error, "controller cannot rumble");
        }
    }

    /// `Update::GamepadLights`: the lightbar colour, and the DualSense player
    /// LEDs as SDL's player index (SDL shows the same patterns).
    pub fn lights(&mut self, pad: u8, rgb: [u8; 3], player: u8) {
        let Some(controller) = self.pads.playstation_mut(pad) else {
            return;
        };
        if let Some(index) = player_index(player)
            && let Err(error) = controller.set_player_index(Some(index))
        {
            tracing::debug!(pad, %error, "controller has no player LEDs");
        }
        // After the player index: SDL may colour the lightbar by player.
        let [red, green, blue] = rgb;
        if controller.has_led()
            && let Err(error) = controller.set_led(red, green, blue)
        {
            tracing::debug!(pad, %error, "controller has no lightbar");
        }
    }

    /// `Update::GamepadTriggers`: DualSense adaptive-trigger effects, sent
    /// raw through SDL to a PS5 controller (others refuse it).
    pub fn triggers(
        &mut self,
        pad: u8,
        right: &[u8; TRIGGER_EFFECT_LEN],
        left: &[u8; TRIGGER_EFFECT_LEN],
    ) {
        if let Some(controller) = self.pads.playstation_mut(pad)
            && let Err(error) =
                controller.send_effect(&crate::protocol::dualsense_trigger_effect(right, left))
        {
            tracing::debug!(pad, %error, "controller cannot play trigger effects");
        }
    }

    /// Handle a controller event. Returns `true` when the event was one.
    pub fn event(
        &mut self,
        event: &SdlEvent,
        events: &mpsc::Sender<Event>,
        offer: GamepadOffer,
    ) -> Result<bool> {
        let host_gamepads = offer.any();
        match *event {
            SdlEvent::ControllerDeviceAdded { which, .. } => self.open(which),
            SdlEvent::ControllerDeviceRemoved { which, .. } => {
                let had = self.pads.slot(which).is_some();
                if let Some(event) = self.pads.detach(which) {
                    send(events, event)?;
                }
                if had {
                    self.notice = Some("Gamepad disconnected".into());
                }
            }
            SdlEvent::ControllerButtonDown { which, button, .. }
            | SdlEvent::ControllerButtonUp { which, button, .. } => {
                let down = matches!(event, SdlEvent::ControllerButtonDown { .. });
                if host_gamepads {
                    // A press in the same iteration as the attach is not lost.
                    for event in self.pads.announce(offer) {
                        send(events, event)?;
                    }
                }
                if host_gamepads
                    && let Some(button) = self.pads.button(which, button as i32 as u8, down)
                {
                    // Keep order: the stick position at the press goes first.
                    self.flush_axes(events, Some(which))?;
                    send(events, button)?;
                }
            }
            SdlEvent::ControllerAxisMotion {
                which, axis, value, ..
            } => self.pads.axis(which, axis as i32 as u8, value),
            SdlEvent::ControllerTouchpadDown {
                which,
                touchpad: 0,
                finger,
                x,
                y,
                ..
            }
            | SdlEvent::ControllerTouchpadMotion {
                which,
                touchpad: 0,
                finger,
                x,
                y,
                ..
            } => self.pads.touch(which, finger, Some((x, y))),
            SdlEvent::ControllerTouchpadUp {
                which,
                touchpad: 0,
                finger,
                ..
            } => self.pads.touch(which, finger, None),
            SdlEvent::ControllerSensorUpdated {
                which,
                sensor: SensorType::Gyroscope,
                data,
                ..
            } => self.pads.gyro(which, data),
            SdlEvent::ControllerSensorUpdated {
                which,
                sensor: SensorType::Accelerometer,
                data,
                ..
            } => self.pads.accel(which, data),
            SdlEvent::ControllerDeviceRemapped { .. }
            | SdlEvent::ControllerTouchpadDown { .. }
            | SdlEvent::ControllerTouchpadMotion { .. }
            | SdlEvent::ControllerTouchpadUp { .. }
            | SdlEvent::ControllerSensorUpdated { .. } => (),
            _ => return Ok(false),
        }
        Ok(true)
    }

    fn flush_axes(&mut self, events: &mpsc::Sender<Event>, only: Option<u32>) -> Result<()> {
        for event in self.pads.dirty(only) {
            if !try_send_axis(events, event.clone())? {
                tracing::debug!("input queue full; gamepad axes retried next iteration");
                break;
            }
            self.pads.mark_sent(&event);
        }
        Ok(())
    }

    /// Once per loop iteration: announce new pads, then send coalesced axes,
    /// touches and motion.
    pub fn flush(&mut self, events: &mpsc::Sender<Event>, offer: GamepadOffer) -> Result<()> {
        if self.pads.len() == 0 {
            return Ok(());
        }
        if !offer.any() {
            if !self.unsupported_noticed {
                self.unsupported_noticed = true;
                self.notice = Some(
                    "Host cannot accept gamepads (update it, or give it /dev/uinput access)".into(),
                );
            }
            return Ok(());
        }
        for event in self.pads.announce(offer) {
            send(events, event)?;
        }
        self.flush_axes(events, None)
    }

    pub fn take_notice(&mut self) -> Option<String> {
        self.notice.take()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const XBOX_ONLY: GamepadOffer = GamepadOffer {
        xbox: true,
        ds4: false,
        dualsense: false,
    };
    const EVERY: GamepadOffer = GamepadOffer {
        xbox: true,
        ds4: true,
        dualsense: true,
    };

    #[test]
    fn rumble_finds_the_controller_of_an_announced_pad() {
        let mut pads = Pads::<&str>::default();
        assert_eq!(
            pads.attach(7, "One", [0; AXES], GamepadType::Xbox, "first"),
            Some(0)
        );
        assert_eq!(
            pads.attach(9, "Two", [0; AXES], GamepadType::Xbox, "second"),
            Some(1)
        );
        assert!(
            pads.handle_mut(0).is_none(),
            "the host does not know it yet"
        );
        pads.announce(XBOX_ONLY);
        assert_eq!(pads.handle_mut(1).copied(), Some("second"));
        assert!(pads.handle_mut(2).is_none() && pads.handle_mut(200).is_none());
        assert!(
            pads.playstation_mut(1).is_none(),
            "an Xbox pad has no lights"
        );
        pads.detach(7);
        assert!(pads.handle_mut(0).is_none(), "unplugged");
    }

    #[test]
    fn slots_announce_coalesce_and_retry_axes() {
        let mut pads = Pads::<()>::default();
        let xbox = GamepadType::Xbox;
        assert_eq!(
            pads.attach(7, "Pad\tOne", [0, 100, 0, 0, 0, 0], xbox, ()),
            Some(0)
        );
        assert_eq!(pads.attach(7, "dup", [0; AXES], xbox, ()), None);
        assert_eq!(pads.attach(9, "Two", [0; AXES], xbox, ()), Some(1));
        // Nothing reaches the host before it has been told about the pad.
        assert!(pads.button(7, 0, true).is_none());
        assert!(pads.dirty(None).is_empty());
        let announced = pads.announce(XBOX_ONLY);
        assert!(matches!(
            &announced[..],
            [
                Event::GamepadConnected { pad: 0, name, kind: GamepadType::Xbox },
                Event::GamepadConnected { pad: 1, .. },
            ] if name == "Pad One"
        ));
        assert!(pads.announce(XBOX_ONLY).is_empty());
        // A deflection held at attach time is sent; motion coalesces.
        pads.axis(9, 0, 10);
        pads.axis(9, 0, 20);
        pads.axis(9, 4, -5); // trigger clamps to SDL's 0..=32767
        // Touch and motion of an Xbox pad stay local.
        pads.touch(9, 0, Some((0.5, 0.5)));
        pads.gyro(9, [1.0; 3]);
        let dirty = pads.dirty(None);
        assert_eq!(
            format!("{dirty:?}"),
            format!(
                "{:?}",
                [
                    Event::GamepadAxis {
                        pad: 0,
                        axis: 1,
                        value: 100
                    },
                    Event::GamepadAxis {
                        pad: 1,
                        axis: 0,
                        value: 20
                    },
                ]
            )
        );
        assert_eq!(pads.dirty(Some(9)).len(), 1);
        // Only delivered values are marked; the rest stay dirty for a retry.
        pads.mark_sent(&dirty[0]);
        assert_eq!(pads.dirty(None).len(), 1);
        pads.mark_sent(&dirty[1]);
        assert!(pads.dirty(None).is_empty());
        pads.axis(9, 0, 0);
        assert_eq!(pads.dirty(None).len(), 1, "return to centre is never lost");
        assert!(matches!(
            pads.button(9, 3, true),
            Some(Event::GamepadButton {
                pad: 1,
                button: 3,
                down: true
            })
        ));
        assert!(matches!(
            pads.detach(7),
            Some(Event::GamepadDisconnected { pad: 0 })
        ));
        assert!(pads.detach(7).is_none());
        // The freed slot is reused; a never-announced pad detaches silently.
        assert_eq!(pads.attach(11, "Three", [0; AXES], xbox, ()), Some(0));
        assert!(pads.detach(11).is_none());
        for instance in 20..23 {
            pads.attach(instance, "", [0; AXES], xbox, ());
        }
        assert_eq!(pads.len(), MAX_GAMEPADS as usize);
        assert_eq!(pads.attach(30, "fifth", [0; AXES], xbox, ()), None);
    }

    #[test]
    fn playstation_controllers_choose_their_pad_and_fall_back_to_xbox() {
        let choice = GamepadChoice::Auto;
        assert_eq!(choice.wants(SDL_CONTROLLER_TYPE_PS4), GamepadType::Ds4);
        assert_eq!(
            choice.wants(SDL_CONTROLLER_TYPE_PS5),
            GamepadType::DualSense
        );
        assert_eq!(choice.wants(1), GamepadType::Xbox, "Xbox 360");
        assert_eq!(choice.wants(5), GamepadType::Xbox, "Switch Pro");
        assert_eq!(choice.wants(99), GamepadType::Xbox, "unknown type");
        assert_eq!(
            GamepadChoice::Xbox.wants(SDL_CONTROLLER_TYPE_PS5),
            GamepadType::Xbox
        );
        assert_eq!(GamepadChoice::DualSense.wants(1), GamepadType::DualSense);
        assert_eq!(
            GamepadChoice::Ds4.wants(SDL_CONTROLLER_TYPE_PS5),
            GamepadType::Ds4
        );
        let mut pads = Pads::<()>::default();
        pads.attach(1, "DualSense", [0; AXES], GamepadType::DualSense, ());
        pads.attach(2, "DS4", [0; AXES], GamepadType::Ds4, ());
        // A host offering only some types (an older one: none) gets Xbox
        // pads for the rest; the kind is then left out of the wire event.
        let only_dualsense = GamepadOffer {
            dualsense: true,
            ..XBOX_ONLY
        };
        let announced = pads.announce(only_dualsense);
        assert!(matches!(
            &announced[..],
            [
                Event::GamepadConnected {
                    pad: 0,
                    kind: GamepadType::DualSense,
                    ..
                },
                Event::GamepadConnected {
                    pad: 1,
                    kind: GamepadType::Xbox,
                    ..
                },
            ]
        ));
        let json = serde_json::to_string(&announced[1]).unwrap();
        assert!(!json.contains("kind"), "{json}");
        let json = serde_json::to_string(&announced[0]).unwrap();
        assert!(json.contains(r#""kind":"dualsense""#), "{json}");
        assert!(pads.playstation_mut(0).is_some());
        assert!(pads.playstation_mut(1).is_none());
        assert_eq!(player_index(0b00100), Some(0));
        assert_eq!(player_index(0b11011), Some(3));
        assert_eq!(player_index(0b00001), None);
    }

    #[test]
    fn touch_and_motion_coalesce_and_are_retried() {
        let mut pads = Pads::<()>::default();
        pads.attach(5, "DualSense", [0; AXES], GamepadType::DualSense, ());
        pads.touch(5, 0, Some((0.25, 0.5)));
        pads.gyro(5, [1.0, 0.0, 0.0]);
        assert!(pads.dirty(None).is_empty(), "not announced yet");
        pads.announce(EVERY);
        // Two fingers; motion averages the gyro samples, latest accel wins.
        pads.touch(5, 1, Some((2.0, -1.0)));
        pads.gyro(5, [3.0, 2.0, f32::NAN]);
        pads.accel(5, [0.0, 9.8, 0.0]);
        pads.accel(5, [0.0, 9.0, 1.0]);
        pads.touch(5, 7, Some((0.5, 0.5))); // no third finger
        let dirty = pads.dirty(None);
        assert_eq!(dirty.len(), 3, "{dirty:?}");
        assert!(matches!(
            dirty[0],
            Event::GamepadTouch {
                pad: 0,
                finger: 0,
                down: true,
                x: 0.25,
                y: 0.5
            }
        ));
        assert!(matches!(
            dirty[1],
            Event::GamepadTouch {
                finger: 1,
                down: true,
                x: 1.0,
                y: 0.0,
                ..
            }
        ));
        assert!(matches!(
            dirty[2],
            Event::GamepadMotion {
                gyro: [2.0, 1.0, 0.0],
                accel: [0.0, 9.0, 1.0],
                ..
            }
        ));
        for event in &dirty {
            assert!(event.validate().is_ok() && event.is_lossy(), "{event:?}");
        }
        // A dropped lift stays pending until it is delivered.
        pads.mark_sent(&dirty[0]);
        pads.mark_sent(&dirty[2]);
        pads.touch(5, 0, None);
        let dirty = pads.dirty(None);
        assert_eq!(dirty.len(), 2, "{dirty:?}");
        assert!(matches!(
            dirty[0],
            Event::GamepadTouch {
                finger: 0,
                down: false,
                x: 0.25,
                ..
            }
        ));
        assert!(matches!(dirty[1], Event::GamepadTouch { finger: 1, .. }));
        assert_eq!(pads.dirty(None).len(), 2, "still pending");
        for event in &dirty {
            pads.mark_sent(event);
        }
        assert!(pads.dirty(None).is_empty());
        // The next motion carries only the new sample.
        pads.gyro(5, [0.5, 0.5, 0.5]);
        assert!(matches!(
            pads.dirty(None)[..],
            [Event::GamepadMotion {
                gyro: [0.5, 0.5, 0.5],
                ..
            }]
        ));
    }

    #[test]
    fn axis_retry_survives_a_full_queue() {
        let (sender, mut receiver) = mpsc::channel(1);
        sender.try_send(Event::Ping).unwrap();
        assert!(
            !try_send_axis(
                &sender,
                Event::GamepadAxis {
                    pad: 0,
                    axis: 0,
                    value: 1
                }
            )
            .unwrap()
        );
        receiver.try_recv().unwrap();
        assert!(
            try_send_axis(
                &sender,
                Event::GamepadAxis {
                    pad: 0,
                    axis: 0,
                    value: 1
                }
            )
            .unwrap()
        );
        drop(receiver);
        assert!(
            try_send_axis(
                &sender,
                Event::GamepadAxis {
                    pad: 0,
                    axis: 0,
                    value: 1
                }
            )
            .is_err()
        );
    }

    #[test]
    fn names_are_bounded_and_printable() {
        let long = "é".repeat(MAX_GAMEPAD_NAME);
        let name = wire_name(&long);
        assert!(name.len() <= MAX_GAMEPAD_NAME);
        assert!(
            Event::GamepadConnected {
                pad: 0,
                name: wire_name("a\nb\0c"),
                kind: GamepadType::Xbox,
            }
            .validate()
            .is_ok()
        );
    }
}
