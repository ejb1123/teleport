# Remote desktop product audit — 2026-09-23

Source review and current test coverage, not an independent security audit or
a claim of physical acceptance on every OS. This review does not authorize
enabling device forwarding, changing system security settings, or rebooting.

## Addressed in this change

- Opt-in session keyboard binding, explicit local release/disconnect shortcuts,
  focus-following between sibling windows, and cleanup on focus loss/exit.
  F8 goes remote while bound. See [keyboard capture](keyboard-capture.md).
- Missing keypad, ISO backslash, menu and extended function-key mappings in
  `protocol::evdev`; existing transport does not need a protocol change.
- Session toolbar/statistics/auxiliary-window text uses drawable DPI, integer
  pixel placement and 1:1 glyph copies, replacing fixed 2x downsampling. This
  does not fix text already blurred inside the encoded desktop stream.

## Prioritized gaps

| Priority | Area / evidence | Next acceptance target |
| --- | --- | --- |
| P0 | Capture uses SDL; OS-reserved shortcuts and compositor policy remain outside our control (`keyboard_capture`, `windowing`) | Real X11, KDE/GNOME Wayland, XWayland and macOS shortcut matrix; capture failure indication; never trap users |
| P0 | Global physical scancode mapping; no remote text/IME protocol or lock-state synchronization (`protocol::evdev`, client input handlers) | AltGr, dead keys, non-US layouts, IME composition, Caps/Num Lock, Mac Command/Super policy; configurable local escape chord |
| P0 | Running-desktop PAM login is not a login-screen/session broker (`system_login`, `capture`) | Explicitly distinguish locked desktop, logged-out user, cold boot, portal revocation and compositor crash; supervised cold-boot test with local recovery |
| P0 | Managed-device revocation exists, but independent security audit is pending (`access`, `pairing`) | Review legacy shared credentials, per-session authorization and MFA policy; scoped view/input/clipboard/device permissions; host-visible active-session controls |
| P1 | Primary stats exclude auxiliary streams (`stats`, `multi_display`); Linux NV12 still maps/copies/uploads | Per-monitor FPS/timing/queue metrics, p50/p95/p99 and sustainable-mode guidance; controlled GPU-resident prototype rather than assuming SDL is the bottleneck |
| P1 | Text fix covers session chrome, not a complete UI replacement (`client`, `launcher`, `host_ui`) | DPI-aware launcher/host forms, font fallback and complex-script shaping, keyboard navigation, accessibility, mixed-DPI physical screenshots |
| P1 | Existing bounded recovery/reconnect and key release are not long-duration WAN acceptance (`client`, `host`) | Sleep/wake, network changes, packet loss, suspend mid-drag, revoked credential during reconnect, clear retry/cancel UI; don't replay input |
| P1 | Independent monitors work; mappings are session-local; hotplug closes auxiliary windows (`multi_display`) | Persist mapping by stable monitor IDs, per-monitor settings, mixed DPI/refresh, hotplug recovery; drag across differing remote/local arrangements |
| P1 | HDR and macOS GPU surfaces have explicit experimental limits (`hdr_present`, docs/hdr*) | Real patched-KWin capture and display validation, metadata/color correctness, SDR/HDR mixed monitors, no silent HDR downgrade |
| P1 | Nix distribution works; standalone clean-machine delivery remains separate (`docs/distribution.md`) | Signed/notarized Mac bundle, dependency closure, upgrade/rollback, credential migration and uninstall tests |
| P2 | Explicit text clipboard exists, not file transfer (`clipboard`, protocol) | Opt-in clipboard direction and size policy; separate file-transfer protocol with consent, progress, cancellation and safe destination paths |
| P2 | Output audio exists; no microphone forwarding or precise A/V sync (`audio`) | Device-change recovery, latency measurement, independent per-session microphone permission |
| P2 | SSH agent is opt-in; USB, PC/SC and remote WebAuthn are not implemented | Keep these separate audited capabilities; device consent, revocation, reconnect/unplug isolation; see peripheral-forwarding.md |
| P2 | Absolute pointer input with small button mapping (`client::button`) | Back/forward mouse buttons, optional relative-pointer mode, cursor-shape transport; never sacrifice cross-monitor escape for pointer lock |

Recommended next implementation batch: finish input/layout acceptance and
per-monitor diagnostics, then replace/improve the launcher UI with accessibility
and DPI as explicit requirements. Avoid treating a toolkit rewrite as a fix for
decode/download throughput or platform shortcut restrictions.
