# Keyboard capture

Click **Bind keys** in the main session toolbar, or press **Ctrl+Alt+K** in
any session window. On macOS this chord uses Control+Option, not Command.
Capture is off by default and is not persisted across reconnects.

- **Ctrl+Alt+K** toggles capture; **Ctrl+Alt+Q** always disconnects locally.
- While bound, ordinary keys including Alt+Tab, Super and F8 are forwarded
  when the local OS delivers them. Use the **Stats** button for local statistics.
- Keyboard capture follows focus between the session's monitor windows; it
  never confines the mouse. Leaving the session disarms capture. The toolbar
  remains clickable; keyboard grabs are also released on error/exit.
- Toggling releases remote held keys/buttons to avoid leaving modifiers down.
  Release physical modifiers before starting the next shortcut.
- Keypad, Num Lock, ISO backslash, application/menu and F13–F24 have physical
  key mappings. This does not synchronize lock-key LEDs or keyboard layouts.

This is a best-effort OS keyboard grab, **not a guarantee of every system key**.
SDL's grab-state getter is not proof that a compositor forwards every shortcut.
OS-reserved secure shortcuts cannot always be intercepted. Wayland needs
compositor shortcut-inhibition support; XWayland may be restricted by compositor
policy. No desktop security settings are changed automatically. macOS shortcut
acceptance requires testing on a physical Mac.

References: [SDL keyboard grab](https://wiki.libsdl.org/SDL2/SDL_SetWindowKeyboardGrab),
[fullscreen Alt+Tab policy](https://wiki.libsdl.org/SDL2/SDL_HINT_ALLOW_ALT_TAB_WHILE_GRABBED),
[XWayland restrictions](https://wiki.libsdl.org/SDL3/README-wayland).

Verification: shortcut unit tests cover repeat/release suppression, modifier-first
release, emergency disconnect, and F8 forwarding. The native Xvfb test checks an
actual X11 keyboard grab and release via a competing X client. These are not
physical Wayland/macOS global-shortcut acceptance tests. Test Alt+Tab, Super,
Ctrl+Alt+K, both-monitor transitions, focus loss, and disconnect before relying
on capture for unattended work.
