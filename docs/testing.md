# Manual acceptance checks

Use a LAN or VPN with UDP 4443 reachable. Start the host from its graphical
session and securely copy the newly generated pairing file. The README has
the full commands. Keep host and client on the same revision.

## macOS client

1. Run `nix develop`, `cargo build --release --locked`, and `teleport doctor`
   (use `./target/release/teleport` when running a Cargo build).
2. Connect to a `--source test` host. Expect an animated ball in a native SDL
   window. Resize it; the image must retain its aspect ratio.
3. Connect to the real host. Open a blank editor on Linux. Check typing,
   Shift, Ctrl, Option/Alt, Command/Super, arrows, Backspace, and Enter.
4. Check pointer alignment at all four corners, especially with a scaled
   HiDPI host monitor and letterboxing in the client window.
5. Check left/middle/right click, dragging and two-finger scrolling.
6. Hold a modifier, switch focus away from the client, then return. The host
   must not have a stuck modifier. Repeat by closing the client while holding
   a key, and by interrupting connectivity. Heartbeat timeout is three seconds.
7. Reconnect without restarting the host. Expect the same pairing file to work.
8. Check Ctrl+Alt+Q exits locally. Some system-reserved Mac shortcuts are not
   forwarded; keyboard layout and IME parity are not implemented.

## Linux sessions

- KDE Wayland: approve the desired monitors and input in the portal. Confirm video,
  pointer alignment, keys, focus loss and reconnect.
- GNOME Wayland: repeat independently; KDE success does not certify GNOME.
- X11: start from an actual X11 session with `--source x11`. Do not force X11
  capture against XWayland: it cannot capture the complete Wayland desktop.
- Cancel the portal dialog: host should exit with a readable error.
- Stop desktop sharing through the compositor: capture failure should end
  the connection; the client must not continue injecting input silently.

## Performance

Start at `--width 1280 --fps 60 --bitrate 8000`. Record CPU/GPU, host desktop
size/scaling, connection type and observed client FPS. FPS is not latency.
For input-to-display latency, film a local input action and the remote display
with a high-speed camera or use a purpose-built timestamp harness. Do not
compare unsynchronized wall clocks across hosts as one-way latency.

Compare `--encoder software` against `--encoder auto`/`nvidia`/`vaapi`.
Record the selected encoder from the host log rather than assuming a GPU was
used. The client still uploads decoded RGB to SDL. Compare `--fixed-bitrate`
against adaptive mode under controlled congestion; record quality, skipped
groups and input responsiveness, not just displayed FPS.

## Automated checks

`cargo test --test smoke -- --ignored --nocapture` runs:

- Synthetic H.264 over real authenticated MoQ/QUIC and native decoding.
- Reconnection and rejection of incorrect credentials/certificate pins.
- An isolated Xvfb host, real X11 capture, key press/release, pointer motion,
  and release of a deliberately held modifier after disconnect.
- Native SDL window rendering on the isolated display.

These tests do not replace testing on physical macOS graphics/input hardware.

## Local validation recorded 2026-09-22

The original prototype was successfully used Mac-to-Linux by the user, who
reported responsive streaming. That does not certify the new features on macOS.
New local automated checks cover persistent credentials across host restart,
portrait/landscape switching, network Opus PCM decoding, explicit Unicode
clipboard roundtrip, heartbeat release after a SIGSTOPped client, and toolbar
monitor/reconnect/disconnect clicks. NVIDIA and software encode/decode plus
live bitrate changes passed; VA-API is unavailable on this machine.

Final v0.2 local verification: formatting and Clippy (warnings denied), 13 unit
tests, two real media pipeline tests, three network/input integrations, and one
native UI integration all passed. Graceful host SIGTERM also releases held keys.
The optimized Linux Nix package built successfully; its `doctor` and all four
network/native UI integrations passed with development GStreamer discovery paths
removed. Flake evaluation succeeded for Linux/macOS on x86_64 and aarch64; that
is not a cross-platform build or runtime test. No live host/service/firewall was
changed by these tests.

### Still-required real-device acceptance

1. Build the same revision on both machines. Open the new Mac launcher and
   import pairing; check all toolbar buttons, resize and fullscreen on Retina.
2. Leave a static Wayland desktop connected for 30 minutes: damage-only capture
   must not be treated as a stalled connection. Type, drag, scroll and switch focus.
3. Sleep/wake the Mac, interrupt Wi-Fi, restore it, and confirm reconnect plus
   release of held keys/buttons. No synthetic input should occur while disconnected.
4. Select multiple real monitors in the Wayland dialog; switch both ways and
   check pointer alignment, including different resolutions/scales. Repeat X11
   with a physical multi-monitor layout. Hotplug requires host restart.
5. Enable an explicit output audio monitor; check playback, mute, and device
   changes. Keep audio at a comfortable level; no microphone should be captured.
6. Enable clipboard on both ends. Explicitly send/get Unicode multiline text,
   then disable sharing and confirm no clipboard transfer occurs.
7. Test `--identity-dir` plus `--restore-token`: approve locally once, stop and
   restart the host, confirm stored pairing remains valid, and record whether
   the compositor restores permission or prompts. Repeat after logout/login;
   do not infer login-screen capture or reboot recovery from a successful restart.

### Original prototype evidence (before these changes)

- Rust formatting, Clippy with warnings denied, and four unit tests passed,
  including recovery of out-of-order control groups.
- Both integration tests passed, including 600 control heartbeats across
  multiple group rotations before the key/pointer checks.
- On the development machine's real KDE Wayland session, portal capture
  streamed 60 frames through MoQ and the native decoder successfully.
- A native accelerated SDL window on that KDE desktop displayed 120 frames
  from the synthetic host and exited normally.
- The Nix Linux package built and its wrapped `doctor` found all plugins.
- Both integration tests also passed against the wrapped release package with
  development-shell GStreamer plugin/scanner variables removed. This includes
  video, reconnect, authentication rejection, X11 input and native rendering.
- Flake package/shell evaluation passed for all four advertised systems.
- The original physical Mac test was subsequently reported successful by the
  user. New Mac code requires another acceptance pass; CI configuration alone
  is not a passing macOS build result.
- Wayland input is implemented through the portal but its real key/button
  behavior still needs manual acceptance testing. Automated input validation
  used an isolated X11 session to avoid typing into the user's real desktop.
