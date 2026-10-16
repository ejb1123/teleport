//! Client game controllers: SDL2 GameController → `Gamepad*` events.
//!
//! SDL normalises every supported controller to the Xbox layout, so its
//! button/axis numbers go on the wire unchanged. Pads are only announced to
//! hosts advertising `Desktop::gamepads`; older hosts reject unknown events.
//! Buttons are sent at once and reliably. Axis motion is coalesced to the
//! latest value per axis and flushed once per loop iteration; an axis whose
//! value was dropped by a full queue stays dirty and is retried.

use super::send;
use crate::protocol::{Event, GAMEPAD_AXES, MAX_GAMEPAD_NAME, MAX_GAMEPADS};
use anyhow::Result;
use sdl2::{GameControllerSubsystem, controller::GameController, event::Event as SdlEvent};
use tokio::sync::mpsc;

const AXES: usize = GAMEPAD_AXES as usize;

struct Slot<C> {
    _handle: C,
    instance: u32,
    name: String,
    announced: bool,
    /// Latest value from the controller and the last value queued to the host.
    current: [i16; AXES],
    sent: [i16; AXES],
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

impl<C> Pads<C> {
    fn slot(&self, instance: u32) -> Option<usize> {
        self.slots
            .iter()
            .position(|slot| slot.as_ref().is_some_and(|slot| slot.instance == instance))
    }

    /// Take the first free slot. `None` when the instance is already attached
    /// or every slot is in use.
    pub fn attach(
        &mut self,
        instance: u32,
        name: &str,
        axes: [i16; AXES],
        handle: C,
    ) -> Option<u8> {
        if self.slot(instance).is_some() {
            return None;
        }
        let index = self.slots.iter().position(Option::is_none)?;
        self.slots[index] = Some(Slot {
            _handle: handle,
            instance,
            name: wire_name(name),
            announced: false,
            current: axes,
            // A new host pad starts centred; any held deflection is re-sent.
            sent: [0; AXES],
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

    /// `GamepadConnected` for every pad the host has not been told about.
    pub fn announce(&mut self) -> Vec<Event> {
        let mut events = Vec::new();
        for (pad, slot) in self.slots.iter_mut().enumerate() {
            if let Some(slot) = slot.as_mut().filter(|slot| !slot.announced) {
                slot.announced = true;
                events.push(Event::GamepadConnected {
                    pad: pad as u8,
                    name: slot.name.clone(),
                });
            }
        }
        events
    }

    pub fn axis(&mut self, instance: u32, axis: u8, value: i16) {
        if let Some(index) = self.slot(instance)
            && let Some(slot) = &mut self.slots[index]
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

    /// Pending axis values for announced pads, oldest pad first. Pass `Some`
    /// to restrict to one controller (before one of its buttons).
    pub fn dirty(&self, only: Option<u32>) -> Vec<Event> {
        let mut events = Vec::new();
        for (pad, slot) in self.slots.iter().enumerate() {
            let Some(slot) = slot else { continue };
            if !slot.announced || only.is_some_and(|instance| instance != slot.instance) {
                continue;
            }
            for axis in 0..AXES {
                if slot.current[axis] != slot.sent[axis] {
                    events.push(Event::GamepadAxis {
                        pad: pad as u8,
                        axis: axis as u8,
                        value: slot.current[axis],
                    });
                }
            }
        }
        events
    }

    pub fn mark_sent(&mut self, event: &Event) {
        if let Event::GamepadAxis { pad, axis, value } = *event
            && let Some(Some(slot)) = self.slots.get_mut(usize::from(pad))
            && usize::from(axis) < AXES
        {
            slot.sent[usize::from(axis)] = value;
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
}

/// Queue a lossy axis value; `false` when the queue was full and it must be
/// retried.
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
    notice: Option<String>,
    unsupported_noticed: bool,
}

impl Controllers {
    pub fn new(sdl: &sdl2::Sdl) -> Self {
        let subsystem = sdl
            .game_controller()
            .inspect_err(|error| tracing::warn!(%error, "game controllers unavailable"))
            .ok();
        let mut controllers = Self {
            subsystem,
            pads: Pads::default(),
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
        let axes = [
            sdl2::controller::Axis::LeftX,
            sdl2::controller::Axis::LeftY,
            sdl2::controller::Axis::RightX,
            sdl2::controller::Axis::RightY,
            sdl2::controller::Axis::TriggerLeft,
            sdl2::controller::Axis::TriggerRight,
        ]
        .map(|axis| controller.axis(axis));
        match self.pads.attach(instance, &name, axes, controller) {
            Some(pad) => {
                tracing::info!(pad, controller = %name, "game controller attached");
                self.notice = Some(format!("Gamepad {} connected: {name}", pad + 1));
            }
            None => {
                self.notice = Some(format!(
                    "Only {MAX_GAMEPADS} gamepads are forwarded; ignoring {name}"
                ));
            }
        }
    }

    /// Handle a controller event. Returns `true` when the event was one.
    pub fn event(
        &mut self,
        event: &SdlEvent,
        events: &mpsc::Sender<Event>,
        host_gamepads: bool,
    ) -> Result<bool> {
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
                    for event in self.pads.announce() {
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
            SdlEvent::ControllerDeviceRemapped { .. }
            | SdlEvent::ControllerTouchpadDown { .. }
            | SdlEvent::ControllerTouchpadMotion { .. }
            | SdlEvent::ControllerTouchpadUp { .. } => (),
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

    /// Once per loop iteration: announce new pads, then send coalesced axes.
    pub fn flush(&mut self, events: &mpsc::Sender<Event>, host_gamepads: bool) -> Result<()> {
        if self.pads.len() == 0 {
            return Ok(());
        }
        if !host_gamepads {
            if !self.unsupported_noticed {
                self.unsupported_noticed = true;
                self.notice = Some(
                    "Host cannot accept gamepads (update it, or give it /dev/uinput access)".into(),
                );
            }
            return Ok(());
        }
        for event in self.pads.announce() {
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

    #[test]
    fn slots_announce_coalesce_and_retry_axes() {
        let mut pads = Pads::<()>::default();
        assert_eq!(
            pads.attach(7, "Pad\tOne", [0, 100, 0, 0, 0, 0], ()),
            Some(0)
        );
        assert_eq!(pads.attach(7, "dup", [0; AXES], ()), None);
        assert_eq!(pads.attach(9, "Two", [0; AXES], ()), Some(1));
        // Nothing reaches the host before it has been told about the pad.
        assert!(pads.button(7, 0, true).is_none());
        assert!(pads.dirty(None).is_empty());
        let announced = pads.announce();
        assert!(matches!(
            &announced[..],
            [
                Event::GamepadConnected { pad: 0, name },
                Event::GamepadConnected { pad: 1, .. },
            ] if name == "Pad One"
        ));
        assert!(pads.announce().is_empty());
        // A deflection held at attach time is sent; motion coalesces.
        pads.axis(9, 0, 10);
        pads.axis(9, 0, 20);
        pads.axis(9, 4, -5); // trigger clamps to SDL's 0..=32767
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
        assert_eq!(pads.attach(11, "Three", [0; AXES], ()), Some(0));
        assert!(pads.detach(11).is_none());
        for instance in 20..23 {
            pads.attach(instance, "", [0; AXES], ());
        }
        assert_eq!(pads.len(), MAX_GAMEPADS as usize);
        assert_eq!(pads.attach(30, "fifth", [0; AXES], ()), None);
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
                name: wire_name("a\nb\0c")
            }
            .validate()
            .is_ok()
        );
    }
}
