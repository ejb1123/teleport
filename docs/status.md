# Version 0.3 implementation status

Version 0.3 adds a redesigned native launcher and toolbar, per-connection
native/scaled resolution, FPS and bitrate selection, H.264/H.265 streaming, and
measured performance diagnostics (Stats/F8). Linux NVIDIA and VA plus macOS
VideoToolbox decoding use real synthetic output probes with software fallback.
Linux NVIDIA decoding has passed both codecs locally; physical Mac verification
of these new paths remains pending. Stream restarts carry a video generation
barrier so stale frames cannot resume input after configuration changes.

HDR has an **experimental opt-in implementation**, including a KWin patch,
strict 10-bit/PQ negotiation, Main 10 transport and Mac Metal/EDR presentation.
Synthetic precision tests pass; real patched-compositor capture and physical Mac
HDR display acceptance remain outstanding. The active compositor is unchanged.
See [HDR evidence](hdr.md) and [measurement definitions](performance.md).

Local pre-release verification (2026-09-22): all 47 unit/media tests, five
authenticated network smoke tests and two native UI interaction tests pass,
including the normally ignored hardware/integration cases on this machine.
Strict Linux Clippy passes. The isolated Mac HDR presenter passes Apple Silicon
Rust type-checking and Clippy, not Apple framework linking or display acceptance.
The PipeWire patch passes its 52 upstream tests and the additional negotiation /
packed-channel regression. None of these results substitute for the real HDR
desktop/display acceptance gates linked above.

Version 0.2.1 adds one-time code pairing with saved trust. Start the host with
`--pair` and a persistent identity directory; the client verifies the code exchange
and saves credentials automatically. No manual file transfer is required. See
[pairing.md](pairing.md) for the TCP firewall requirement, security maturity,
five-minute/five-attempt limits, and recovery.

This is a tested development milestone, not completion of every production
requirement. Both endpoints must run application protocol version 2.

| Area | Implemented and locally checked | Still outstanding |
| --- | --- | --- |
| Reliability | Authenticated reconnect; fixed identities across host restart; release held keys after client suspension/disconnect and graceful host SIGTERM; media reordering regression | Real Mac sleep/wake, prolonged sessions, lossy/congested network acceptance |
| Native UI | Redesigned saved-host sidebar, pairing and quality cards, keyboard editing; session toolbar and Stats/F8 overlay | Physical Mac/Retina pass; accessible native form controls and file picker; keychain integration |
| Video codecs / GPU | H.264/H.265 selection; actual NVIDIA and software encode/decode; live bitrate changes; verified frame generations | VA hardware validation; physical Mac VideoToolbox acceptance; zero-copy capture/rendering |
| Adaptive quality | Bounded bitrate decrease/recovery based on receive backlog/skipped groups; fixed-bitrate override | Controlled WAN/congestion measurement, resolution/FPS adaptation, full bandwidth estimation |
| Audio and clipboard | Opt-in monitor-source Opus, mute, headless network PCM proof; explicit Unicode text roundtrip using isolated X11 clipboard | Real Mac output/device-change and Wayland clipboard acceptance; precise A/V synchronization; file transfer |
| Distribution | Reproducible Nix package, Linux desktop entry, Nix-dependent Mac `.app`, CI and manual closure artifacts | Portable Mac dependency bundle, Developer-ID signing/notarization, clean-machine installer tests |
| Multi-monitor | Authorized portal streams, X11 RandR regions, switching/correct coordinate origins; synthetic landscape/portrait and toolbar tests | Physical mixed-scale monitors; hotplug (currently restart host) |
| Unattended sessions | Stable private host identity, portal restore tokens, activated NixOS graphical-session user service; KDE Wayland restoration across service restart; user-confirmed remote unlock and reconnect while locked | Other compositors and logout/reboot acceptance; login-screen/pre-login access is not implemented |

The signed standalone Mac distribution needs Apple Developer signing/notarization
setup, but credentials alone are not the whole task: the portable dependency
bundle and clean-machine testing still need implementation. Never commit signing
keys, pairing credentials or portal tokens.

Wayland permission persistence is a request to the compositor, not a guarantee.
The combined capture/control session uses RemoteDesktop's restoration mechanism,
as required by the [portal API](https://flatpak.github.io/xdg-desktop-portal/docs/doc-org.freedesktop.portal.RemoteDesktop.html).
Initial consent remains local. No service, firewall, login or running host is
changed just by building this repository.

On 2026-09-22, the v0.2.1 NixOS-generated user unit was built and runtime-linked
on the development machine's existing KDE Wayland session. It reused the existing
identity and portal token, restored capture without another approval, and passed
authenticated headless video decoding (15 frames). After an explicit service
restart, a second connection through the machine's LAN address decoded 30 frames
using NVIDIA encoding and software decoding. Both clients ran on the host; this
does not establish Mac connectivity or firewall reachability. No input events
were injected, and no logout, reboot, screen-lock test or full NixOS switch was
performed. The runtime link alone does not install persistent startup.

Later that day, the full NixOS configuration was built, its activation preview
reviewed, and switched successfully without reboot or logout. The declarative
unit is installed under `/etc/systemd/user`, wanted by `graphical-session.target`,
and remained running after its activation restart; the temporary runtime link
was removed. The user also confirmed unlocking this KDE Wayland desktop remotely
from the Mac. This is user-reported acceptance, not an automated password test;
no lock policy was weakened. The user subsequently confirmed reconnecting while
already locked also worked. Fresh login and reboot remain untested. Startup is still after graphical
login, not access to the login screen.

See [testing.md](testing.md) for real-device acceptance and
[distribution.md](distribution.md) for installation and service setup.
