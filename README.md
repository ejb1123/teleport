# Teleport

An experimental **native** remote desktop: Linux host, macOS/Linux client,
Rust + SDL + GStreamer, with Media over QUIC for video and input. No browser,
Electron, WebRTC, external relay, or account is required.

This is an experimental implementation, not a production Parsec replacement yet.
It shares an existing graphical session, one client at a time. Start on a LAN
or a VPN with direct UDP connectivity.
See [implementation status](docs/status.md) for verified features and unfinished work.

## Quick start: Linux → Mac

Clone this repository on both machines and install Nix with flakes enabled.
The locked flake covers x86_64/aarch64 Linux and Intel/Apple Silicon macOS.
The initial build downloads and compiles dependencies and can take several minutes.

On the **Linux machine**, from a terminal inside your graphical session:

```sh
nix run . -- doctor
nix run . -- host --listen 0.0.0.0:4443 --pair \
  --identity-dir "$HOME/.local/state/teleport/host" \
  --restore-token "$HOME/.local/state/teleport/host/portal-restore.json"
```

On Wayland, choose the **monitors you want to share** in the local permission dialog and grant
keyboard/mouse control. The host uses the RemoteDesktop/ScreenCast portals and
PipeWire directly; it does not capture through XWayland. `--source auto` selects
the portal for Wayland and X11 capture/XTest for an X11 session.

Allow **UDP 4443** for streaming and **TCP 4443** for initial code pairing
from your Mac through the host firewall. Teleport does not
change firewall rules. On NixOS, for example, add this to your own configuration
and rebuild (restrict access to your trusted interface/network as appropriate):

```nix
networking.firewall.allowedUDPPorts = [ 4443 ];
networking.firewall.allowedTCPPorts = [ 4443 ]; # initial code pairing only
```

On the **Mac**, run the native client:

```sh
nix run .
```

Enter `LINUX_IP:4443` and the six-digit code shown in the Linux host terminal.
Click **Pair & save host**, then **Connect / reconnect**. Next time just select
the saved host and connect—no code or trust prompt. Keep the host's identity
directory unchanged so saved trust survives restarts. An identity mismatch is
never accepted automatically; explicitly pair again only after checking the host.

The code is valid for five minutes, one successful enrollment, and at most five
TCP connection attempts. Restart the host with `--pair` for a new code if it
expires or is consumed. The temporary TCP listener closes afterwards; subsequent
desktop connections need only UDP. Without `--pair`, no enrollment port opens.
Treat the displayed code as a secret and don't publish terminal/service logs.
Code pairing remains experimental LAN/VPN functionality; see
[pairing security and limitations](docs/pairing.md).

CLI pairing is also available; it asks for the code on stdin rather than putting
it in process arguments:

```sh
nix run . -- pair LINUX_IP:4443
```

Manual file import remains available through **Use file** or drag-and-drop.
To use that alternative, copy the file over an existing trusted SSH connection:

```sh
scp YOUR_USER@LINUX_IP:.local/state/teleport/host/pairing.json ./pairing.json
chmod 600 pairing.json
nix run . -- client LINUX_IP:4443 --pairing-file pairing.json --reconnect
```

Replace `YOUR_USER` and `LINUX_IP`. Do not commit or share the pairing file: it
grants desktop control. Its host certificate fingerprint is verified before
the secret is sent. Media and input travel over TLS-protected QUIC.

Click inside the native window to control the remote desktop. Keyboard events
use physical key positions; the host keyboard layout determines the characters.
Mac Command maps to Linux Super, Option maps to Alt. Some OS shortcuts remain
local. **Ctrl+Alt+Q** quits the client locally. Losing window focus releases all
remote keys/buttons; disconnects and expired heartbeats do the same on the host.

The session toolbar provides **Monitor >**, **Audio**, **Send text**, **Get text**,
**Fullscreen**, **Reconnect**, and **Disconnect**. The title shows monitor, size,
displayed FPS, and action status. Toolbar clicks are local and never forwarded
to the remote desktop. A connection window shows failures and has Retry/Cancel.

Stop the host with Ctrl+C. With `--identity-dir`, the certificate and pairing
token survive host restarts, so your saved client pairing keeps working. Keep
this directory private (0700); its credentials are 0600. Do not copy `key.pem`
to clients. To revoke paired access, stop the host, move the old identity
directory to a private backup location, then start with a new identity directory
and re-pair trusted clients. Everyone with the same pairing file has the same
access; per-client credentials/revocation are not yet implemented.

Without `--identity-dir`, use `--pairing-file NEW_FILENAME` for ephemeral
credentials. This mode rotates credentials each start and refuses overwrites.
`--restore-token` requests remembered Wayland permission, **not** a bypass:
your compositor may prompt again. Only already-authorized monitors can be
switched. Monitor hotplug needs a host restart. Unattended operation is limited
to an existing logged-in graphical session, not login-screen/reboot access.

## Audio, clipboard and quality

Clipboard transfers are explicit text-only actions, limited to 64 KiB. Enable
`--clipboard` on the host and the client (or the launcher's clipboard toggle).
**Send text** copies the local clipboard to Linux; **Get text** copies Linux's
clipboard locally. No continuous clipboard scraping or automatic synchronization.
Linux clipboard helpers run in the graphical session and are included by Nix.

Audio is off by default. List output monitors with `pactl list short sources`,
then add `--audio-source YOUR_OUTPUT.monitor` to the host command. Never choose
a microphone: only explicit `.monitor` names are accepted. Audio uses low-delay
Opus and the client's audio output; use **Audio** or `--mute` to mute playback.
This is output audio only, with no microphone forwarding and no tight A/V sync
guarantee. `--audio-source test` generates a diagnostic tone.

The host defaults to `--encoder auto`: it probes VA-API and NVIDIA encoding,
then falls back to CPU x264 if neither can encode. Force `--encoder software`,
`--encoder nvidia`, or `--encoder vaapi` for diagnosis. This is hardware
**encoding**, not a zero-copy pipeline; capture/conversion still use CPU memory.
The installed plugins and system GPU drivers must support the selected backend.

`--bitrate 8000` is the starting bitrate and adaptive ceiling, in kbit/s.
Receiver queue pressure and skipped video groups reduce the bitrate, with slow
recovery when the connection clears. This conservative heuristic is not a
full bandwidth estimator. `--fixed-bitrate` disables it; encoders that cannot
change bitrate during playback also stay fixed, with a warning.

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
nix run . -- host --source test --listen 0.0.0.0:4443 \
  --pairing-file /tmp/teleport-test-pairing.json
```

Copy that pairing file to the Mac and connect exactly as above. Test this first
to separate networking/decoding problems from portal/capture problems.

## Current scope and limitations

- Native SDL window, resizable with letterboxing and correctly mapped pointer
  coordinates; video, keyboard, three mouse buttons, and scrolling.
- Default 1280-pixel stream width, 60 fps cap, 8 Mbps H.264. Tune with `--width`,
  `--fps`, and `--bitrate` (kilobits/second). Aspect ratio is retained.
- Auto hardware encoding with CPU x264 fallback, low-delay settings and short
  keyframe groups. Frames still copy through CPU memory; zero-copy rendering
  is not implemented. FPS in the title is decoded/displayed frame
  rate, **not** end-to-end latency.
- macOS selects GStreamer's VideoToolbox hardware decoder when available;
  otherwise software H.264 decoding. Linux currently uses software decoding.
- KDE/GNOME require working RemoteDesktop and ScreenCast portal backends.
  Other Wayland compositors may lack remote input support. This prototype uses
  portal input notification methods, not libei yet.
- Initial local approval is required for Wayland. Saved permission is opt-in
  and compositor-dependent. No login-screen access or unattended-access guarantee.
- No file transfer, gamepad, virtual display, HDR, IME, monitor hotplug,
  keychain-backed credentials or automatic NAT traversal yet.
- Direct LAN/VPN connections only. A bitrate above available bandwidth
  causes skipped video groups. Severe control loss/reordering disconnects the
  session instead of silently dropping keyboard/button events.
- Windows is a future client target, not an implemented or tested platform.
- A signed/notarized standalone Mac installer is not implemented. Nix provides
  a native `.app` launcher that still depends on its Nix store closure; see
  [installation and distribution](docs/distribution.md).

## Transport

Pinned `moq-net 0.2.20` and `moq-native 0.19.17`, Quinn backend, raw QUIC with
**moq-lite-05** negotiated explicitly. This is the MoQ project's simplified
wire protocol, not a claim of compliance with the latest IETF draft.

The host publishes a `desktop` broadcast with metadata and an `h264` track.
Each video group starts with an H.264 keyframe and inline SPS/PPS. The receiver
cancels an older group when a newer group arrives. Frames are Annex B access
units; this application's media framing is not Hang/CMAF interoperable.
Application protocol version 2 adds monitors, audio, feedback and explicit
clipboard controls. Update **both** host and client from the original prototype.
An ordered `updates` track carries monitor acknowledgements and clipboard
responses; an optional `opus` track carries independent audio packets.

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
cargo test --locked --test ui -- --ignored --nocapture
cargo test --locked --bin teleport -- --ignored --nocapture
nix build
TELEPORT_TEST_BINARY=./result/bin/teleport cargo test --locked --test smoke -- --ignored --nocapture
nix flake check --all-systems
```

Ignored tests need local UDP sockets, media plugins, and Xvfb. They exercise
video/audio, credential pinning, persistent identity across host restarts,
monitor switching, real X11 input/clipboard, stuck-key release after a suspended
client, and real native toolbar/launcher clicks on isolated displays.
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
