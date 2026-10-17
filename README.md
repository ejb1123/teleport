# Teleport

An experimental **native** remote desktop: Linux host, macOS/Linux client,
Rust + SDL + direct FFmpeg video codecs, with Media over QUIC for video and input. No browser,
Electron, WebRTC, external relay, or account is required (an optional relay can
reach hosts behind NAT; see [relay mode](docs/relay.md)).

This is an experimental implementation, not a production Parsec replacement yet.
It shares an existing graphical session, one client at a time. Start on a LAN
or a VPN with direct UDP connectivity.
See [implementation status](docs/status.md) for verified features and unfinished work.

Video encoding and decoding now default to directly linked FFmpeg libraries
(no FFmpeg subprocess). GStreamer remains for capture, audio and scheduling.
See [codec backend and testing](docs/ffmpeg.md), including hardware selection,
the legacy-backend comparison switch, and current GPU-copy limitations.

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

A host that uses a [relay](docs/relay.md) also prints a relay pairing link;
`pair ... --relay URL` reads it instead, so pairing needs no reachable host
port ([details](docs/pairing.md#pairing-through-a-relay-no-reachable-host-port)).

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

The launcher defaults to **Match client** size, **Match display** frame rate and
**Auto bitrate**: the client tells the host its window size (physical pixels)
and its display's refresh rate, and the host streams at the lowest of that
rate, its own monitor's refresh rate (X11, or what the KWin screencast states)
and the user's cap, up to 240 FPS, at the largest size of the host monitor's
aspect ratio that fits the window (never upscaled). Moving the window to another
display, a mode change, fullscreen or a resize renegotiates once it has settled
for a second, and only if the stream would change (a resize by 10% or more).
Explicit choices remain: native, 1080p-, 1440p-, 720p- or 4K-width; 60, 120,
144, 165, 180 or 240 FPS (a cap); 8/20/40/80 Mbps; H.264/H.265/AV1. Auto bitrate is
20 Mbps up to 60 FPS and scales above (34.6 Mbps at 1440p180). Hosts from
before 240 FPS support stream at most 120 FPS; the client says so and clamps.
Scaling preserves the selected monitor's aspect ratio; it does not change the
Linux monitor's mode (the desktop portal cannot). Native uses the size reported
by capture, which may be logical pixels on scaled Wayland desktops. Settings are
session-local and switching displays retains the selected quality policy.

Frames are presented as they arrive: adaptive vsync where the GL driver offers
it (GLX), otherwise no vsync wait, so a variable-refresh (FreeSync/G-Sync)
display follows the stream when the compositor enables VRR for the fullscreen
window. `--vsync` (launcher: **VSync on**) waits for every refresh instead, for
fixed-refresh displays that tear.
For explicit CLI settings:

```sh
nix run . -- client LINUX_IP:4443 --pairing-file pairing.json \
  --width 0 --fps 60 --bitrate 20000 --codec h265 --stats
```

`--codec auto` (the default; launcher **Auto codec**) streams AV1, else H.265,
when the host encodes it in hardware and the client decodes it in hardware, and
H.264 otherwise or with hosts from before automatic choice. An explicit
`--codec h264|h265|av1` always wins (AV1 needs a host that offers it; a software
host encodes it with SVT-AV1, which is slow). Two quality options, off by
default: `--chroma-444 auto|on` (launcher **4:4:4 colour**) keeps full-resolution
colour for sharp coloured text (H.265 4:4:4 on NVIDIA in hardware), and
`--ten-bit auto|on` (launcher **10-bit**) encodes Main 10 against banding in
dithered gradients. Stats (F8) shows the codec, 4:2:0/4:4:4, 10-bit and why
([AV1, 4:4:4 and 10-bit](docs/ffmpeg.md#av1)).
`--encoder-preset latency|balanced|quality` (host default, or per session from
the client; launcher **Encoder:**) trades host encode time for quality without
adding frame delay; `latency` is the default. See
[encoder presets](docs/ffmpeg.md#encoder-presets) for what each maps to and
measurements. On low-bitrate links the host sharpens a still screen with a
short burst of repeated frames 200 ms after it stops changing
([static-scene refinement](docs/ffmpeg.md#static-scene-refinement)).
`--width match --fps match --bitrate auto` negotiate as above. `--width 0` means native; a positive width requests scaling, with a supported
ceiling of 7680 pixels wide, 8192 high and 33,554,432 total pixels. Actual encoder
hardware may have lower limits. Host CLI defaults remain 1280 wide for older
clients; a new client's quality request overrides this for its own session.

Fullscreen can now map independent remote monitors to multiple local displays
within one authenticated session (requires an updated host). The toolbar overlays
the desktop and collapses to the top edge; **Use one/all displays** controls the
layout, and each window's display selector chooses its remote monitor. See
[fullscreen mapping, limits and testing](docs/multi-monitor.md).

On KDE Plasma 6 hosts the portal provides virtual displays. By default
(`--virtual-display fallback`) a host with no monitor to share streams a
virtual one sized to the client, and `teleport client --virtual-monitor`
(launcher **Host: + virtual monitor**) adds one beside the real monitors for
the session; `--virtual-display always` always streams one (exact resolution
matching). Privacy mode turns the host's monitors off while you are connected:
start the host with `--privacy-mode request` and connect with `--privacy`
(launcher **Host screen: private**). The monitors come back on disconnect, and
a guard process, `ExecStopPost` and `teleport privacy-restore` cover a host
that dies. See [virtual displays and privacy mode](docs/virtual-display.md).

Stats distinguish requested/accepted per-stream bitrate caps, the encoder's
current target, control round-trip time, actual main-stream encoded-video payload bitrate,
received/decoded/displayed FPS, skipped groups, superseded decoded frames,
decoder input queue, receive-to-decoded time, decoded-frame wait, and host encoder
time where timestamps survive. RTT includes scheduling; it is **not** one-way
network latency. With updated hosts F8 also shows **capture to present**
p50/p95, split into capture→encode, encode→send, network, receive→decode
and decode→present: hosts stamp each frame and the client maps their clock
onto its own from the control round trips (NTP-style). Display scanout is
not in it; `teleport host --source test --flash-on-input` and a photodiode
or high-speed camera time input to photon. F8 opens with a rolling graph of
frame time, latency and bitrate, and short warnings appear over the video
when the network, decoder or host degrades (`--no-warnings` or the
launcher's **Warnings off** hides them). `--pacing smooth` (launcher
**Pacing: smooth**) trades a little latency (a buffer of at most 3 frames
or 50 ms) for frames shown at the even cadence they were captured; the
default shows each frame as soon as it is decoded. See
[performance diagnostics](docs/performance.md).

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

Keyboard shortcuts: **Bind keys** or **Ctrl+Alt+K** requests keyboard capture
for Alt+Tab/Super and other shortcuts supported by the local OS. The same chord
releases it; **Ctrl+Alt+Q** always disconnects locally. While bound, F8 goes
remote; use the Stats button instead. See [keyboard capture and platform limits](docs/keyboard-capture.md)
and the [remote-desktop feature audit](docs/remote-desktop-audit.md).

The host defaults to `--encoder auto`: it probes NVIDIA, Intel Quick Sync (QSV),
then AMD/Intel VA-API encoding before falling back to CPU x264 (H.264)
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
with software decoding. The default Mac path still downloads RGB for SDL upload;
hardware decoding alone is not a zero-copy render path. Physical Mac HEVC
acceptance remains required. Intel VA-API has been hardware-tested for H.264,
HEVC SDR, and HEVC HDR10 decoding; AMD and QSV still require physical-hardware
acceptance on supported GPUs.

Version 0.5.4 uses NV12 textures for Linux SDR when the actual accelerated SDL
renderer advertises NV12 and passes a texture-upload check. This removes the CPU
YUV-to-RGB conversion from the usual VA path; SDL performs it during rendering.
A one-frame leaky queue **after decoding** prevents a slow downloader/converter
from building an unlimited raw-frame backlog. Compressed dependent frames are
never dropped individually. RGB remains the fallback for unsupported/software
renderers; Mac and HDR pixel paths are unchanged.

An experimental macOS SDR path is available with
`TELEPORT_MAC_GPU_VIDEO=1 ./result/bin/teleport` after `nix build`.
It imports VideoToolbox's IOSurface-backed NV12 planes directly into Metal for
H.264/H.265, avoiding application CPU video mapping, conversion and upload.
F8 reports the Metal device and pixel path. This is opt-in pending Mac hardware
acceptance; unset the variable to use normal rendering. HDR retains its existing
P010 upload path, and UI panels still use CPU readback/upload. See
[GPU video testing and limitations](docs/hdr-presentation.md#experimental-gpu-resident-sdr-video).

This is **not zero-copy**: decoded NV12 planes still pass through CPU memory and
are uploaded to SDL. Non-BT.709 sources may also require YUV color conversion.
The log and F8 report the renderer, pixel path and OpenGL device where exposed;
an SDL "accelerated" flag alone does not prove physical GPU rendering (llvmpipe
is a CPU implementation). On non-NixOS Intel/AMD systems, the wrapper now supplies
matching packaged Mesa GLX/EGL libraries as well as VA drivers. NixOS graphics
stacks and systems with a loaded proprietary NVIDIA module are left unchanged.
Explicit DRI/EGL overrides are respected. Physical Arch acceptance is still needed.

Version 0.5.3 gives Linux VA/QSV decoders a bounded startup allowance (750 ms,
with recovery thresholds of 60 compressed frames or 8 MiB). After repeated stalls, the session switches
to the matching software decoder at a fresh keyframe without reconnecting or
re-authenticating. F8 reports the decoder actually in use. You can also choose
**Decoder: software** in the launcher's **Connection settings**. Software decoding
uses more CPU and is not a guarantee of sufficient throughput at every resolution.
The underlying Arch VA stall has not been reproduced locally; this is a recovery
and fallback fix, not a claim that the driver problem is solved. Mac decoder
selection and recovery are unchanged.

Version 0.5.2 bounds steady-state decoder input backlog by age (150 ms), count (12 frames),
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
- Host CLI defaults to 1280-pixel stream width, 60 fps (up to 240 with `--fps`),
  8 Mbps H.264 (scaled above 60 fps unless `--bitrate` is given); the launcher
  matches the client window and display. Tune with `--width`, `--fps`, and
  `--bitrate` (kilobits/second). Aspect ratio is retained.
- Auto hardware encoding with CPU x264/x265 fallback, low-delay settings and short
  keyframe groups. Frames still copy through CPU memory; zero-copy rendering
  is not implemented. FPS in the stats overlay is decoded/displayed frame
  rate, **not** end-to-end latency (that is the Capture to present line).
- macOS probes VideoToolbox; Linux probes NVIDIA/VA/QSV decoding, with software
  fallback for H.264 and H.265. Actual NVIDIA output has been tested locally.
- KDE/GNOME require working RemoteDesktop and ScreenCast portal backends.
  Other Wayland compositors may lack remote input support. This prototype uses
  portal input notification methods, not libei yet.
- Initial local approval is required for Wayland. Saved permission is opt-in
  and compositor-dependent. No login-screen access or unattended-access guarantee.
- HDR requires the experimental patched-KWin/Mac path; see [its limitations](docs/hdr.md).
- The client draws the remote pointer locally (no round trip) with X11 hosts
  and Wayland hosts whose portal sends cursor metadata (KDE Plasma 6);
  `--embedded-cursor` on either end keeps it in the video. See
  [local cursor](docs/cursor.md).
- Games: the client locks the mouse (relative mode) while the host hides
  its pointer; `--relative-mouse always|never` (launcher **Mouse** button)
  changes that, Ctrl+Alt+M toggles the lock by hand.
- Gamepads (with rumble) need a Linux host with `/dev/uinput` access.
  PlayStation controllers become a DualShock 4 or DualSense (touchpad,
  gyro, lightbar, adaptive triggers) when the host can also use
  `/dev/uhid`; `--gamepad-type` (launcher **Gamepad:**) overrides the
  choice. See [gamepads](docs/gamepad.md).
- Virtual displays and privacy mode need KDE Plasma 6 (GNOME untested);
  see [virtual displays](docs/virtual-display.md).
- No file transfer, IME, monitor hotplug or
  keychain-backed credentials yet. NAT traversal needs a relay for
  signalling; behind a symmetric NAT the session uses the relay itself
  (there is no other TURN-like fallback).
- Without a relay: direct LAN/VPN connections only. A bitrate above available bandwidth
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
client requires matching metadata before subscribing. Hosts advertising
`codec_selection` send the metadata (with their hardware codecs) before video and
let the client's first input event, `select_codec`, override that parameter. Each video group starts
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

**Relay mode** (development): when the host's UDP port is not reachable, host and
client can both connect out to a MoQ relay; every relayed session is end-to-end
encrypted and authenticated against the pinned host identity. In the launcher,
select a desktop and click **Relay** to save a relay URL, its fingerprint, key and a
route (**Direct, then relay** by default). Desktops with a relay show signed
online/offline presence. See [relay mode](docs/relay.md).

**Peer-to-peer through NATs** (development): with a relay and the default
route, the client also tries a direct path between two NATed networks. Both
ends learn their public address from the relay's built-in STUN responder
(UDP 3478), exchange addresses end-to-end encrypted through the relay, and
punch holes from their QUIC sockets; the session then runs directly, pinned to
the host certificate as on a LAN. Stats (F8) shows `Path p2p, ...`; behind a
symmetric NAT or with UDP blocked the relay is used as before. `--no-p2p`
turns it off on either end; `--upnp` opts in to a NAT-PMP port mapping. See
[peer-to-peer](docs/relay.md#peer-to-peer-through-nats).

## Development and checks

```sh
nix develop
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test --locked
cargo test --locked --test smoke -- --ignored --nocapture
cargo test --locked --test ui -- --ignored --nocapture
cargo test --locked --bin teleport -- --ignored --nocapture
# KDE hosts: a private headless KWin with its own portal (never your session)
cargo test --locked --test virtual_display -- --ignored --test-threads=1
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
