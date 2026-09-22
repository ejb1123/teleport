# Teleport

An experimental **native** remote desktop: Linux host, macOS/Linux client,
Rust + SDL + GStreamer, with Media over QUIC for video and input. No browser,
Electron, WebRTC, external relay, or account is required.

This is an experimental implementation, not a production Parsec replacement yet.
It shares an existing graphical session, one client at a time. Start on a LAN
or a VPN with direct UDP connectivity.
See [implementation status](docs/status.md) for verified features and unfinished work.

The current development build adds native Linux **Host settings**, persistent
Teleport username/passphrase login and individually revocable device credentials.
See [host setup and login](docs/access.md). These are Teleport accounts, not Linux
login accounts. Security-key login and peripheral forwarding have separate
implementation and hardware-verification requirements; see
[security-key design](docs/security-key-design.md).

Version 0.5.0 adds opt-in **existing Linux account login** through PAM, with pinned
TLS and one-session credentials. See [system login setup](docs/system-login.md).
It attaches to a running desktop; boot/login-screen session creation is not included.

Version 0.5.2 makes the launcher saved-desktop-first: **Add desktop** saves a name
and `host:port` without contacting the host or asking for credentials. Select it
and **Connect** to sign in. First contact presents the full SHA-256 host fingerprint
for explicit approval; compare it with the host through trusted SSH or locally.
Approved fingerprints are remembered, and changed identities are blocked rather
than silently replaced. Linux passwords are never saved; paired-device credentials
remain reusable and revocable. Connection quality settings are collapsed by default.

Version 0.4.1 added an explicit **password + U2F touch** enrollment path for the
YubiKey NEO and isolates the Linux package from conflicting system SDL/font
configuration (including Arch). See [NEO setup and limitations](docs/yubikey-neo.md).
Saved devices still connect using their credentials; this is not a touch-per-session
policy, and physical NEO acceptance remains pending.

Version 0.3 adds a redesigned native launcher, selectable stream resolution/FPS/
bitrate, and a **Stats for nerds** overlay (toolbar or **F8**). Launcher connections
default to native monitor resolution, 60 FPS and 20 Mbps; preferences apply on
connection. Both endpoints need the new build for quality negotiation and host
telemetry. The F8 overlay reports actual streamed pixels, not the client window size;
the native title stays plain and stable.
**SDR H.264 and H.265/HEVC are the tested baseline.** An experimental patched-KWin
HDR path and Mac Metal/EDR renderer are available; see [HDR status](docs/hdr.md).
`nix run . -- doctor --hdr` tests synthetic codecs. A passing codec probe is not HDR
desktop support.

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

Allow **UDP 4443** for streaming and **TCP 4443** for password or code enrollment
from your Mac through the host firewall. Teleport does not
change firewall rules. On NixOS, for example, add this to your own configuration
and rebuild (restrict access to your trusted interface/network as appropriate):

```nix
networking.firewall.allowedUDPPorts = [ 4443 ];
networking.firewall.allowedTCPPorts = [ 4443 ]; # password / code enrollment
```

On the **Mac**, run the native client:

```sh
nix run .
```

Choose **Add desktop**, enter a name and `LINUX_IP:4443`, then save it.
Choose **Connect**, select **Pairing code**, and enter the six-digit code shown
in the Linux host terminal. After **Continue**, verify and approve the host
fingerprint; the native session opens automatically. Next time select the saved
desktop and connect without another code or trust prompt. Keep the host's identity
directory unchanged so saved trust survives restarts. An identity mismatch is
blocked and cannot be overwritten by signing in or pairing again.

The code is valid for five minutes, one successful enrollment, and at most five
TCP connection attempts. On updated persistent hosts, use **Host settings** or
`teleport host-admin open-pairing` for a new code without restarting. The TCP
listener closes afterwards unless persistent password enrollment is enabled;
subsequent desktop connections need only UDP.
Treat the displayed code as a secret and don't publish terminal/service logs.
Code pairing remains experimental LAN/VPN functionality; see
[pairing security and limitations](docs/pairing.md).

CLI pairing is also available; it asks for the code on stdin rather than putting
it in process arguments:

```sh
nix run . -- pair LINUX_IP:4443
```

Manual file import remains available through **Pairing file** on the sign-in screen
or drag-and-drop onto that screen.
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

The session toolbar provides **Display**, **Audio**, **Send text**, **Get text**,
**Stats**, **Full screen**, **Reconnect**, and **Disconnect**. The title stays plain;
changing diagnostics are in F8, with action status in the toolbar. Toolbar clicks are local and never forwarded
to the remote desktop. A connection window shows failures and has Retry/Cancel.

Stop the host with Ctrl+C. With `--identity-dir`, the certificate and pairing
token survive host restarts, so your saved client pairing keeps working. Keep
this directory private (0700); its credentials are 0600. Do not copy `key.pem`
to clients. Updated persistent hosts issue separate credentials for new password
and code enrollments; revoke these in **Host settings**. Existing copies of the
legacy host `pairing.json` still share access and are not covered by managed-device
revocation. Invalidating those requires rotating the entire host identity and
re-enrolling trusted clients. See [revocation details](docs/access.md#pairing-and-revocation).

Without `--identity-dir`, use `--pairing-file NEW_FILENAME` for ephemeral
credentials. This mode rotates credentials each start and refuses overwrites.
`--restore-token` requests remembered Wayland permission, **not** a bypass:
your compositor may prompt again. Only already-authorized monitors can be
switched. Monitor hotplug needs a host restart. Unattended operation is limited
to an existing logged-in graphical session, not login-screen/reboot access.

## Audio, clipboard and quality

Choose native, 1080p-width, 1440p-width, 720p-width or 4K-width in the launcher,
along with 30/60/120 FPS, 8/20/40 Mbps and H.264/H.265. Scaling preserves the selected monitor's aspect
ratio; it does not change the Linux monitor's mode. Native uses the size reported
by capture, which may be logical pixels on scaled Wayland desktops. Settings are
session-local and switching displays retains the selected quality policy.
For explicit CLI settings:

```sh
nix run . -- client LINUX_IP:4443 --pairing-file pairing.json \
  --width 0 --fps 60 --bitrate 20000 --codec h265 --stats
```

`--width 0` means native; a positive width requests scaling, with a supported
ceiling of 7680 pixels wide, 8192 high and 33,554,432 total pixels. Actual encoder
hardware may have lower limits. Host CLI defaults remain 1280 wide for older
clients; a new client's quality request overrides this for its own session.

Stats distinguish control round-trip time, encoded-video payload bitrate,
received/decoded/displayed FPS, skipped groups, superseded decoded frames,
decoder input queue, receive-to-decoded time, decoded-frame wait, and host encoder
time where timestamps survive. RTT includes scheduling; it is **not** one-way
network latency or input-to-photon latency. Capture and display scanout timing
are not measured. See [performance diagnostics](docs/performance.md).

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

The host defaults to `--encoder auto`: it probes Intel Quick Sync (QSV),
AMD/Intel VA-API, then NVIDIA encoding before falling back to CPU x264 (H.264)
or x265 (H.265). Force `--encoder qsv` (alias `quick-sync`), `--encoder vaapi`,
`--encoder nvidia`, or `--encoder software` for diagnosis. Both VA-API and QSV
support H.264, HEVC SDR, and HEVC Main10 HDR when the GPU supports them. This is hardware
**encoding**, not a zero-copy pipeline; capture/conversion still use CPU memory.
The installed plugins and GPU drivers must support the selected backend.
Linux Nix builds bundle matching Mesa/Intel VA drivers, and x86-64 builds include
Intel's oneVPL GPU runtime for QSV. The host still needs a working kernel GPU
driver and permission to access `/dev/dri/renderD*` (usually the `render` group).
Older Intel GPUs may work through VA-API but not the modern QSV runtime.
Explicit `LIBVA_DRIVERS_PATH`, `LIBVA_DRIVER_NAME`, and `ONEVPL_PRIORITY_PATH`
overrides are respected. Driver-specific GStreamer caches avoid reusing stale
"no hardware found" results from previous package versions. Run `teleport doctor`
to see available encoder/decoder factories; availability is not a hardware test.

Clients probe actual hardware decoding before connecting: NVIDIA, VA-API, then QSV on Linux,
and VideoToolbox on macOS. Failed probes fall back to the codec's software decoder.
The stats overlay shows the selected decoder by name. `--software-decoder` forces
CPU decoding. The probe checks a small synthetic stream; a device can still fail
at larger resolutions or after a driver/device change. If that happens, reconnect
with software decoding. Hardware decode currently downloads RGB for SDL upload;
this is accelerated decoding, not a zero-copy render path. Physical Mac HEVC
acceptance remains required. Intel VA-API has been hardware-tested for H.264,
HEVC SDR, and HEVC HDR10 decoding; AMD and QSV still require physical-hardware
acceptance on supported GPUs.

Version 0.5.2 bounds decoder input backlog by age (150 ms), count (12 frames),
and compressed size (8 MiB). Overload clears the decoder pipeline and abandons
the current GOP, resuming at a fresh keyframe group rather than dropping arbitrary
dependent frames. Output already older than 250 ms is discarded before presentation.
These are recovery thresholds, not an end-to-end latency guarantee: a slow driver
can still cause pauses or repeated recovery. The stats overlay separates compressed
queue time, parse/decode/download, and conversion/copy, and counts recoveries and
stale output. These client-local stages are not pure GPU execution measurements.

macOS Nix builds include an app-local GStreamer HEVC fix: its VideoToolbox output
queue follows the SPS reordering requirement instead of unconditionally retaining
up to 15 frames. Zero-reorder streams can be delivered immediately; streams with
B-frame reordering retain their required queue, and unsupported/malformed SPS keeps
the conservative fallback. The exact parsing/queue logic is regression-tested on
Linux; the patched decoder still requires physical Mac build/runtime acceptance.
No system GStreamer or running host service is replaced by building the app.

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
- Host CLI defaults to 1280-pixel stream width, 60 fps cap, 8 Mbps H.264;
  the launcher requests native/60fps/20Mbps. Tune with `--width`,
  `--fps`, and `--bitrate` (kilobits/second). Aspect ratio is retained.
- Auto hardware encoding with CPU x264/x265 fallback, low-delay settings and short
  keyframe groups. Frames still copy through CPU memory; zero-copy rendering
  is not implemented. FPS in the stats overlay is decoded/displayed frame
  rate, **not** end-to-end latency.
- macOS probes VideoToolbox; Linux probes NVIDIA/VA/QSV decoding, with software
  fallback for H.264 and H.265. Actual NVIDIA output has been tested locally.
- KDE/GNOME require working RemoteDesktop and ScreenCast portal backends.
  Other Wayland compositors may lack remote input support. This prototype uses
  portal input notification methods, not libei yet.
- Initial local approval is required for Wayland. Saved permission is opt-in
  and compositor-dependent. No login-screen access or unattended-access guarantee.
- HDR requires the experimental patched-KWin/Mac path; see [its limitations](docs/hdr.md).
- No file transfer, gamepad, virtual display, IME, monitor hotplug,
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

The host publishes a `desktop` broadcast with metadata and a selected `h264` or
`h265` track. Codec selection is an authenticated connection parameter, and the
client requires matching metadata before subscribing. Each video group starts
with a keyframe and inline parameter sets (including VPS for HEVC). The receiver
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
On the client, `--software-decoder` bypasses hardware decoding and
`--software-renderer` bypasses the GPU renderer for troubleshooting.

Linux local windows prefer X11/XWayland when `DISPLAY` is available; launcher,
host settings and connection progress use software presentation without requiring
the host GPU libraries. Streaming still tries an accelerated renderer first and
retries software presentation if renderer creation returns an error. Hardware
decoding and native Wayland host capture are independent of this window policy.
Explicit `SDL_VIDEODRIVER`/`SDL_VIDEO_DRIVER` selections are respected; use
`SDL_VIDEODRIVER=wayland` to opt into native Wayland windows (which still need a
working graphics presentation stack). macOS window selection is unchanged.
