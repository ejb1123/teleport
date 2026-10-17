//! Opt-in session keyboard capture; never confines the mouse.
use sdl2::{
    event::Event,
    keyboard::{Keycode, Mod},
    video::Window,
};

#[derive(Default)]
pub struct Capture {
    window: Option<Window>,
}

impl Capture {
    pub fn active(&self) -> bool {
        self.window.is_some()
    }
    pub fn release(&mut self) {
        if let Some(mut window) = self.window.take() {
            window.set_keyboard_grab(false);
        }
    }
    pub fn bind(&mut self, mut window: Window) -> bool {
        self.release();
        sdl2::hint::set_with_priority(
            "SDL_ALLOW_ALT_TAB_WHILE_GRABBED",
            "0",
            &sdl2::hint::Hint::Override,
        );
        window.set_keyboard_grab(true);
        if window.keyboard_grab() {
            self.window = Some(window);
            true
        } else {
            window.set_keyboard_grab(false);
            false
        }
    }
    pub fn follow(&mut self, focused: Option<Window>) {
        if !self.active() {
            return;
        }
        match focused {
            Some(window)
                if self
                    .window
                    .as_ref()
                    .is_some_and(|old| old.id() == window.id()) => {}
            Some(window) => {
                self.bind(window);
            }
            None => self.release(),
        }
    }
}
impl Drop for Capture {
    fn drop(&mut self) {
        self.release();
    }
}

#[derive(Debug, PartialEq)]
pub enum Action {
    Toggle,
    /// Ctrl+Alt+M: lock/unlock the pointer for games (relative motion).
    PointerLock,
    /// Ctrl+Alt+V: mute/unmute the microphone sent to the host.
    Microphone,
    /// Ctrl+Alt+End: secure attention (Teleport's Ctrl+Alt+Del): lock the
    /// host session, or show its unlock prompt.
    SecureAttention,
    Disconnect,
    Stats,
    Swallow,
}

/// Retain consumed key-downs until their releases, including modifier-first release.
#[derive(Default)]
pub struct Shortcuts {
    toggle_down: bool,
    lock_down: bool,
    microphone_down: bool,
    stats_down: bool,
    attention_down: bool,
}
impl Shortcuts {
    pub fn event(&mut self, event: &Event, captured: bool) -> Option<Action> {
        let (key, modifiers, down, repeat) = match *event {
            Event::KeyDown {
                keycode: Some(key),
                keymod,
                repeat,
                ..
            } => (key, keymod, true, repeat),
            Event::KeyUp {
                keycode: Some(key),
                keymod,
                ..
            } => (key, keymod, false, false),
            _ => return None,
        };
        let chord = modifiers.intersects(Mod::LCTRLMOD | Mod::RCTRLMOD)
            && modifiers.intersects(Mod::LALTMOD | Mod::RALTMOD);
        if key == Keycode::Q && down && chord {
            return Some(Action::Disconnect);
        }
        if key == Keycode::K && (self.toggle_down || (down && chord)) {
            let first = down && !repeat && !self.toggle_down;
            self.toggle_down = down;
            return Some(if first {
                Action::Toggle
            } else {
                Action::Swallow
            });
        }
        if key == Keycode::M && (self.lock_down || (down && chord)) {
            let first = down && !repeat && !self.lock_down;
            self.lock_down = down;
            return Some(if first {
                Action::PointerLock
            } else {
                Action::Swallow
            });
        }
        if key == Keycode::V && (self.microphone_down || (down && chord)) {
            let first = down && !repeat && !self.microphone_down;
            self.microphone_down = down;
            return Some(if first {
                Action::Microphone
            } else {
                Action::Swallow
            });
        }
        if key == Keycode::End && (self.attention_down || (down && chord)) {
            let first = down && !repeat && !self.attention_down;
            self.attention_down = down;
            return Some(if first {
                Action::SecureAttention
            } else {
                Action::Swallow
            });
        }
        if key == Keycode::F8 && (self.stats_down || (!captured && down)) {
            let first = down && !repeat && !self.stats_down;
            self.stats_down = down;
            return Some(if first {
                Action::Stats
            } else {
                Action::Swallow
            });
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn key(key: Keycode, down: bool, modifiers: Mod, repeat: bool) -> Event {
        if down {
            Event::KeyDown {
                timestamp: 0,
                window_id: 1,
                keycode: Some(key),
                scancode: None,
                keymod: modifiers,
                repeat,
            }
        } else {
            Event::KeyUp {
                timestamp: 0,
                window_id: 1,
                keycode: Some(key),
                scancode: None,
                keymod: modifiers,
                repeat,
            }
        }
    }
    #[test]
    fn toggle_consumes_repeat_and_release_in_both_modes() {
        for captured in [false, true] {
            let mut shortcuts = Shortcuts::default();
            let chord = Mod::LCTRLMOD | Mod::LALTMOD;
            assert_eq!(
                shortcuts.event(&key(Keycode::K, true, chord, false), captured),
                Some(Action::Toggle)
            );
            assert_eq!(
                shortcuts.event(&key(Keycode::K, true, chord, true), !captured),
                Some(Action::Swallow)
            );
            assert_eq!(
                shortcuts.event(&key(Keycode::K, false, Mod::NOMOD, false), !captured),
                Some(Action::Swallow)
            );
            assert_eq!(
                shortcuts.event(&key(Keycode::K, true, Mod::NOMOD, false), captured),
                None
            );
        }
    }
    #[test]
    fn pointer_lock_chord_toggles_once_and_swallows_its_release() {
        for captured in [false, true] {
            let mut shortcuts = Shortcuts::default();
            let chord = Mod::LCTRLMOD | Mod::LALTMOD;
            assert_eq!(
                shortcuts.event(&key(Keycode::M, true, chord, false), captured),
                Some(Action::PointerLock)
            );
            assert_eq!(
                shortcuts.event(&key(Keycode::M, true, chord, true), captured),
                Some(Action::Swallow)
            );
            assert_eq!(
                shortcuts.event(&key(Keycode::M, false, Mod::NOMOD, false), captured),
                Some(Action::Swallow)
            );
            // Plain M is typed into the remote desktop.
            assert_eq!(
                shortcuts.event(&key(Keycode::M, true, Mod::NOMOD, false), captured),
                None
            );
        }
    }
    #[test]
    fn microphone_chord_toggles_once_and_swallows_its_release() {
        for captured in [false, true] {
            let mut shortcuts = Shortcuts::default();
            let chord = Mod::RCTRLMOD | Mod::LALTMOD;
            assert_eq!(
                shortcuts.event(&key(Keycode::V, true, chord, false), captured),
                Some(Action::Microphone)
            );
            assert_eq!(
                shortcuts.event(&key(Keycode::V, true, chord, true), captured),
                Some(Action::Swallow)
            );
            assert_eq!(
                shortcuts.event(&key(Keycode::V, false, Mod::NOMOD, false), captured),
                Some(Action::Swallow)
            );
            // Ctrl+V (paste) still goes to the remote desktop.
            assert_eq!(
                shortcuts.event(&key(Keycode::V, true, Mod::LCTRLMOD, false), captured),
                None
            );
        }
    }

    #[test]
    fn ctrl_alt_end_is_secure_attention_and_end_alone_goes_remote() {
        for captured in [false, true] {
            let mut shortcuts = Shortcuts::default();
            let chord = Mod::RCTRLMOD | Mod::LALTMOD;
            assert_eq!(
                shortcuts.event(&key(Keycode::End, true, chord, false), captured),
                Some(Action::SecureAttention)
            );
            assert_eq!(
                shortcuts.event(&key(Keycode::End, true, chord, true), captured),
                Some(Action::Swallow)
            );
            // Released after the modifiers: still consumed.
            assert_eq!(
                shortcuts.event(&key(Keycode::End, false, Mod::NOMOD, false), captured),
                Some(Action::Swallow)
            );
            assert_eq!(
                shortcuts.event(&key(Keycode::End, true, Mod::NOMOD, false), captured),
                None
            );
            assert_eq!(
                shortcuts.event(&key(Keycode::End, true, Mod::LCTRLMOD, false), captured),
                None
            );
        }
    }

    #[test]
    fn captured_shortcuts_go_remote_except_emergency_controls() {
        let mut shortcuts = Shortcuts::default();
        for code in [Keycode::Tab, Keycode::F8, Keycode::LGUI] {
            assert_eq!(
                shortcuts.event(&key(code, true, Mod::LALTMOD, false), true),
                None
            );
            assert_eq!(
                shortcuts.event(&key(code, false, Mod::NOMOD, false), true),
                None
            );
        }
        assert_eq!(
            shortcuts.event(
                &key(Keycode::Q, true, Mod::RCTRLMOD | Mod::RALTMOD, false),
                true
            ),
            Some(Action::Disconnect)
        );
    }
    #[test]
    fn local_stats_release_stays_local_after_capture_changes() {
        let mut shortcuts = Shortcuts::default();
        assert_eq!(
            shortcuts.event(&key(Keycode::F8, true, Mod::NOMOD, false), false),
            Some(Action::Stats)
        );
        assert_eq!(
            shortcuts.event(&key(Keycode::F8, false, Mod::NOMOD, false), true),
            Some(Action::Swallow)
        );
    }
}
