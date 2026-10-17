//! Client key options found in Parsec's client: hotkeys kept on this
//! computer (never sent to the host, e.g. a push-to-talk key), and swapping
//! Ctrl and Command (Super) between macOS and other systems. Both apply to
//! forwarded keys only; Teleport's own chords (Ctrl+Alt+K/M/Q/V/End) are handled
//! before them.
use sdl2::{
    event::Event,
    keyboard::{Keycode, Mod, Scancode},
};

const CTRL: Mod = Mod::LCTRLMOD.union(Mod::RCTRLMOD);
const ALT: Mod = Mod::LALTMOD.union(Mod::RALTMOD);
const SHIFT: Mod = Mod::LSHIFTMOD.union(Mod::RSHIFTMOD);
const GUI: Mod = Mod::LGUIMOD.union(Mod::RGUIMOD);

/// A key with exactly these modifiers held (lock keys aside).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Hotkey {
    key: Scancode,
    modifiers: Mod,
}

impl Hotkey {
    /// `F13`, `ScrollLock`, `Ctrl+Shift+M`, `Cmd+F1`: SDL key names, with
    /// Ctrl/Control, Alt/Option, Shift and Super/Cmd/Command/Win/Meta.
    pub fn parse(text: &str) -> Result<Self, String> {
        let mut modifiers = Mod::NOMOD;
        let mut parts: Vec<&str> = text.split('+').map(str::trim).collect();
        let name = parts.pop().filter(|name| !name.is_empty());
        for part in parts {
            modifiers |= match part.to_ascii_lowercase().as_str() {
                "ctrl" | "control" => CTRL,
                "alt" | "option" => ALT,
                "shift" => SHIFT,
                "super" | "cmd" | "command" | "win" | "meta" | "gui" => GUI,
                _ => return Err(format!("unknown modifier {part:?} in {text:?}")),
            };
        }
        // Short forms of SDL's "Right Ctrl" and similar.
        let name = name.map(|name| match name.to_ascii_lowercase().as_str() {
            "lctrl" => "Left Ctrl",
            "rctrl" => "Right Ctrl",
            "lalt" => "Left Alt",
            "ralt" => "Right Alt",
            "lshift" => "Left Shift",
            "rshift" => "Right Shift",
            "lsuper" | "lcmd" | "lgui" => "Left GUI",
            "rsuper" | "rcmd" | "rgui" => "Right GUI",
            _ => name,
        });
        let key = name.and_then(Scancode::from_name).ok_or_else(|| {
            format!("unknown key in {text:?} (use SDL key names such as F13 or ScrollLock)")
        })?;
        Ok(Self { key, modifiers })
    }

    fn matches(&self, key: Scancode, held: Mod) -> bool {
        // A modifier used as the key (a right Ctrl push-to-talk) is held
        // by its own press.
        let held = held & !own_modifier(key);
        key == self.key
            && [CTRL, ALT, SHIFT, GUI]
                .iter()
                .all(|group| held.intersects(*group) == self.modifiers.intersects(*group))
    }

    pub fn label(&self) -> String {
        let mut label = String::new();
        for (group, name) in [
            (CTRL, "Ctrl+"),
            (ALT, "Alt+"),
            (SHIFT, "Shift+"),
            (GUI, "Super+"),
        ] {
            if self.modifiers.intersects(group) {
                label += name;
            }
        }
        label + self.key.name()
    }
}

fn own_modifier(key: Scancode) -> Mod {
    match key {
        Scancode::LCtrl | Scancode::RCtrl => CTRL,
        Scancode::LAlt | Scancode::RAlt => ALT,
        Scancode::LShift | Scancode::RShift => SHIFT,
        Scancode::LGui | Scancode::RGui => GUI,
        _ => Mod::NOMOD,
    }
}

/// The key options of one session.
#[derive(Default)]
pub struct KeyOptions {
    swap_ctrl_cmd: bool,
    local: Vec<Hotkey>,
    /// Kept keys whose release must stay here too.
    held: Vec<Scancode>,
}

impl KeyOptions {
    pub fn new(swap_ctrl_cmd: bool, local: Vec<Hotkey>) -> Self {
        Self {
            swap_ctrl_cmd,
            local,
            held: Vec::new(),
        }
    }

    pub fn swap_ctrl_cmd(&self) -> bool {
        self.swap_ctrl_cmd
    }

    pub fn local(&self) -> &[Hotkey] {
        &self.local
    }

    /// The key event to forward to the host (Ctrl and Command swapped if
    /// asked), or `None` to keep it on this computer. Other events pass.
    pub fn filter(&mut self, event: Event) -> Option<Event> {
        match event {
            Event::KeyDown {
                timestamp,
                window_id,
                scancode: Some(code),
                keymod,
                repeat,
                ..
            } => {
                if self.held.contains(&code) {
                    return None;
                }
                if self.local.iter().any(|hotkey| hotkey.matches(code, keymod)) {
                    self.held.push(code);
                    return None;
                }
                let code = self.swap(code);
                Some(Event::KeyDown {
                    timestamp,
                    window_id,
                    keycode: Keycode::from_scancode(code),
                    scancode: Some(code),
                    keymod,
                    repeat,
                })
            }
            Event::KeyUp {
                timestamp,
                window_id,
                scancode: Some(code),
                keymod,
                repeat,
                ..
            } => {
                if let Some(index) = self.held.iter().position(|held| *held == code) {
                    self.held.remove(index);
                    return None;
                }
                let code = self.swap(code);
                Some(Event::KeyUp {
                    timestamp,
                    window_id,
                    keycode: Keycode::from_scancode(code),
                    scancode: Some(code),
                    keymod,
                    repeat,
                })
            }
            other => Some(other),
        }
    }

    /// Forget kept keys (the session lost focus; their releases go elsewhere).
    pub fn reset(&mut self) {
        self.held.clear();
    }

    fn swap(&self, code: Scancode) -> Scancode {
        if !self.swap_ctrl_cmd {
            return code;
        }
        match code {
            Scancode::LCtrl => Scancode::LGui,
            Scancode::RCtrl => Scancode::RGui,
            Scancode::LGui => Scancode::LCtrl,
            Scancode::RGui => Scancode::RCtrl,
            other => other,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: Scancode, down: bool, keymod: Mod, repeat: bool) -> Event {
        if down {
            Event::KeyDown {
                timestamp: 0,
                window_id: 1,
                keycode: Keycode::from_scancode(code),
                scancode: Some(code),
                keymod,
                repeat,
            }
        } else {
            Event::KeyUp {
                timestamp: 0,
                window_id: 1,
                keycode: Keycode::from_scancode(code),
                scancode: Some(code),
                keymod,
                repeat,
            }
        }
    }

    fn scancode(event: Option<Event>) -> Option<Scancode> {
        match event? {
            Event::KeyDown { scancode, .. } | Event::KeyUp { scancode, .. } => scancode,
            _ => None,
        }
    }

    #[test]
    fn parses_hotkeys_with_modifiers() {
        let hotkey = Hotkey::parse("Ctrl+Shift+F13").unwrap();
        assert_eq!(hotkey.label(), "Ctrl+Shift+F13");
        assert_eq!(Hotkey::parse("cmd + m").unwrap().label(), "Super+M");
        assert_eq!(Hotkey::parse("ScrollLock").unwrap().label(), "ScrollLock");
        for bad in ["", "Ctrl+", "Hyper+F1", "NoSuchKey"] {
            assert!(Hotkey::parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn kept_hotkeys_stay_local_with_their_repeats_and_release() {
        let mut keys = KeyOptions::new(false, vec![Hotkey::parse("Ctrl+F13").unwrap()]);
        // Without Ctrl, F13 goes to the host.
        assert_eq!(
            scancode(keys.filter(key(Scancode::F13, true, Mod::NOMOD, false))),
            Some(Scancode::F13)
        );
        assert!(
            keys.filter(key(Scancode::F13, false, Mod::NOMOD, false))
                .is_some()
        );
        // With Ctrl: kept here, repeats and release too (even after Ctrl is
        // let go first).
        assert!(
            keys.filter(key(Scancode::F13, true, Mod::RCTRLMOD, false))
                .is_none()
        );
        assert!(
            keys.filter(key(Scancode::F13, true, Mod::RCTRLMOD, true))
                .is_none()
        );
        assert!(
            keys.filter(key(Scancode::F13, false, Mod::NOMOD, false))
                .is_none()
        );
        // Extra modifiers do not match.
        assert!(
            keys.filter(key(
                Scancode::F13,
                true,
                Mod::LCTRLMOD | Mod::LSHIFTMOD,
                false
            ))
            .is_some()
        );
        // A modifier as the hotkey (right Ctrl push-to-talk).
        let mut keys = KeyOptions::new(false, vec![Hotkey::parse("RCtrl").unwrap()]);
        assert_eq!(keys.local()[0].label(), "Right Ctrl");
        assert_eq!(Hotkey::parse("Right Ctrl").unwrap(), keys.local()[0]);
        assert!(
            keys.filter(key(Scancode::RCtrl, true, Mod::RCTRLMOD, false))
                .is_none()
        );
        assert!(
            keys.filter(key(Scancode::RCtrl, false, Mod::NOMOD, false))
                .is_none()
        );
        assert!(
            keys.filter(key(Scancode::LCtrl, true, Mod::LCTRLMOD, false))
                .is_some()
        );
    }

    #[test]
    fn swaps_ctrl_and_command_both_ways() {
        let mut keys = KeyOptions::new(true, Vec::new());
        for (local, remote) in [
            (Scancode::LGui, Scancode::LCtrl),
            (Scancode::RGui, Scancode::RCtrl),
            (Scancode::LCtrl, Scancode::LGui),
            (Scancode::RCtrl, Scancode::RGui),
            (Scancode::C, Scancode::C),
        ] {
            for down in [true, false] {
                assert_eq!(
                    scancode(keys.filter(key(local, down, Mod::NOMOD, false))),
                    Some(remote)
                );
            }
        }
        // Off: unchanged.
        let mut keys = KeyOptions::default();
        assert_eq!(
            scancode(keys.filter(key(Scancode::LGui, true, Mod::NOMOD, false))),
            Some(Scancode::LGui)
        );
    }
}
