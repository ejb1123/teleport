# Teleport

An experimental **native** remote desktop: Linux host, macOS/Linux client,
Rust + SDL + GStreamer, with Media over QUIC for video and input. No browser,
Electron, WebRTC, external relay, or account is required.

This is a first working prototype, not a Parsec performance replacement yet.
It shares an existing graphical session, one client at a time. Start on a LAN
or a VPN with direct UDP connectivity.

## Quick start: Linux → Mac

Clone this repository on both machines and install Nix with flakes enabled.
The locked flake covers x86_64/aarch64 Linux and Intel/Apple Silicon macOS.
The initial build downloads and compiles dependencies and can take several minutes.

On the **Linux machine**, from a terminal inside your graphical session:

```sh
nix develop
cargo build --release --locked
./target/release/teleport doctor
./target/release/teleport host --listen 0.0.0.0:4443 \
  --pairing-file /tmp/teleport-pairing.json
```

On Wayland, choose **one monitor** in the local permission dialog and grant
keyboard/mouse control. The host uses the RemoteDesktop/ScreenCast portals and
PipeWire directly; it does not capture through XWayland. `--source auto` selects
the portal for Wayland and X11 capture/XTest for an X11 session.

Allow **UDP 4443** from your Mac through the host firewall. Teleport does not
change firewall rules. On NixOS, for example, add this to your own configuration
and rebuild (restrict access to your trusted interface/network as appropriate):

```nix
networking.firewall.allowedUDPPorts = [ 4443 ];
```

On the **Mac**, securely copy the pairing file using your existing SSH access,
then run the native client:

```sh
scp YOUR_USER@LINUX_IP:/tmp/teleport-pairing.json ./pairing.json
chmod 600 pairing.json
nix develop
cargo run --release --locked -- client LINUX_IP:4443 --pairing-file pairing.json
```

Replace `YOUR_USER` and `LINUX_IP`. If SSH isn't configured, transfer the file
through another trusted channel. Do not commit or share the pairing file: it
grants desktop control. Its host certificate fingerprint is verified before
the secret is sent. Media and input travel over TLS-protected QUIC.

Click inside the native window to control the remote desktop. Keyboard events
use physical key positions; the host keyboard layout determines the characters.
Mac Command maps to Linux Super, Option maps to Alt. Some OS shortcuts remain
local. **Ctrl+Alt+Q** quits the client locally. Losing window focus releases all
remote keys/buttons; disconnects and expired heartbeats do the same on the host.

Stop the host with Ctrl+C. Credentials rotate on every host start, and the host
refuses to overwrite an existing pairing file. Use a new filename on restart
(and copy that new file to the Mac). A reconnect while the host stays running
uses the same pairing file and capture permission.

## Nix package

You can also build/run the wrapped package, which locates its media plugins
without entering a shell:

```sh
nix build
nix run . -- doctor
nix run . -- client LINUX_IP:4443 --pairing-file pairing.json
```

Host mode is Linux-only. macOS builds include the native client. Nixpkgs 26.05
is pinned to retain Intel Mac support; later upgrades must revisit that target.

## Synthetic test

This produces an animated test screen without desktop access or input injection:

```sh
./target/release/teleport host --source test --listen 0.0.0.0:4443 \
  --pairing-file /tmp/teleport-test-pairing.json
```

Copy that pairing file to the Mac and connect exactly as above. Test this first
to separate networking/decoding problems from portal/capture problems.

## Current scope and limitations

- Native SDL window, resizable with letterboxing and correctly mapped pointer
  coordinates; video, keyboard, three mouse buttons, and scrolling.
- Default 1280-pixel stream width, 60 fps cap, 8 Mbps H.264. Tune with `--width`,
  `--fps`, and `--bitrate` (kilobits/second). Aspect ratio is retained.
- CPU x264 encoding with no B-frames or lookahead; short keyframe groups. This
  baseline still copies frames through CPU memory. GPU encoding and zero-copy
  rendering are not implemented. FPS in the title is decoded/displayed frame
  rate, **not** end-to-end latency.
- macOS selects GStreamer's VideoToolbox hardware decoder when available;
  otherwise software H.264 decoding. Linux currently uses software decoding.
- KDE/GNOME require working RemoteDesktop and ScreenCast portal backends.
  Other Wayland compositors may lack remote input support. This prototype uses
  portal input notification methods, not libei yet.
- Local approval is required when starting a Wayland host. No login-screen,
  reboot recovery, saved portal permission, or unattended-access guarantee.
- No audio, clipboard/file transfer, gamepad, virtual display, HDR, IME,
  multi-monitor switching, adaptive bitrate, or automatic NAT traversal yet.
- Direct LAN/VPN connections only. A fixed bitrate above available bandwidth
  causes skipped video groups. Severe control loss/reordering disconnects the
  session instead of silently dropping keyboard/button events.
- Windows is a future client target, not an implemented or tested platform.

## Transport

Pinned `moq-net 0.2.20` and `moq-native 0.19.17`, Quinn backend, raw QUIC with
**moq-lite-05** negotiated explicitly. This is the MoQ project's simplified
wire protocol, not a claim of compliance with the latest IETF draft.

The host publishes a `desktop` broadcast with metadata and an `h264` track.
Each video group starts with an H.264 keyframe and inline SPS/PPS. The receiver
cancels an older group when a newer group arrives. Frames are Annex B access
units; this application's media framing is not Hang/CMAF interoperable.

The client publishes a separate high-priority `controls/input` track with
ordered JSON events, sequence numbers, and heartbeats. Control groups rotate
to bound retained data. A bounded reorder buffer restores control-group order.
Missing events or gaps beyond the timeout/window end the connection and
release input, rather than applying an incomplete key sequence. Pointer
motion is coalesced at the window loop. Only one authenticated controller is
served at a time. Private credentials are generated per host process.

## Development and checks

```sh
nix develop
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test --locked
cargo test --locked --test smoke -- --ignored --nocapture
nix build
TELEPORT_TEST_BINARY=./result/bin/teleport cargo test --locked --test smoke -- --ignored --nocapture
nix flake check --all-systems
```

The ignored smoke test needs local UDP sockets and installed media plugins;
it runs a synthetic host, decodes video through a native client without a
window, reconnects, and checks invalid-token/certificate rejection.
The packaged-binary variant strips GStreamer discovery environment variables
to check that the wrapper works outside the development shell. Ordinary
unit tests can run in the Nix build sandbox. Desktop permission dialogs and
real Mac graphics/input still need manual tests on those systems.

If connection fails, check the IP, UDP firewall, and matching current pairing
file. If video fails, run `teleport doctor` and try `--source test`. For capture
errors, run the host in the graphical user's session, not via sudo. Portal
cancellation and unavailable permissions are errors, not automatic fallbacks
to privileged capture. Use `RUST_LOG=teleport=debug` for application diagnostics.
On the client, `--software-decoder` bypasses VideoToolbox and
`--software-renderer` bypasses the GPU renderer for troubleshooting.
