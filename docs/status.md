# Implementation status

## Version 0.4.1: Arch environment isolation and NEO enrollment

The Linux package now isolates its SDL libraries and Fontconfig configuration
from inherited system overrides. A packaged regression test deliberately supplies
a foreign SDL library and invalid font configuration; the old package fails it.
Security-key diagnostics no longer initialize GStreamer.

Explicit U2F/CTAP1 enrollment supports password-plus-key-touch for new device
credentials. Challenges follow OPAQUE password proof, bind the authenticated host
and exchange, and use authenticated encryption. Verification checks presence,
signature and a persistent increasing counter. Existing trusted-device and code
pairing paths remain unchanged: **this is not touch-per-session or global MFA**.
FIDO2 credentials retain their separate UV requirement without silent downgrade.
See [NEO setup and recovery](yubikey-neo.md).

Native verification and real TCP exchange tests use synthetic ES256 credentials;
physical NEO enrollment/login and the updated package on the user's Arch machine
remain acceptance tests. No authenticator reset or PIN change is required.

Local verification (2026-09-22): 83 distinct tests pass, including normally
ignored native crypto/media/network/UI tests and the packaged foreign-library
regression. The latter fails against 0.4.0 and passes against 0.4.1. The Nix release
build, strict Clippy, formatting checks and all 12 packaged integration tests pass.
This remains software/laboratory evidence, not a physical NEO or Arch acceptance
claim. The running production host was not restarted or reconfigured.

## Version 0.4 development checkpoint

Implemented: native Linux Host Settings, private same-user administration over
Unix sockets, OPAQUE password enrollment, unique managed-device credentials,
revocation of active sessions, and explicit reuse of a saved identity at another
address. Existing shared credentials are retained for compatibility, not silently
revoked by managed-device controls. See [access and migration](access.md).

Experimental SSH-agent forwarding is implemented with explicit host/client opt-in,
session-scoped sockets, client-side SSH-only request filtering and no automatic
forwarding on reconnect. Real authenticated network forwarding/cleanup and a real
OpenSSH Ed25519 signature through the relay pass locally. Destination-constrained
keys and arbitrary signing are not supported. See [SSH-agent limits](ssh-agent.md).

The native libfido2 adapter provides **offline diagnostics and proof-of-possession
tests only**, not security-key login to Teleport or remote websites. Physical key
acceptance remains pending. [Security-key design](security-key-design.md) separates
that foundation from the still-unimplemented host authentication integration.

Generic USB, PC/SC smartcard and remote WebAuthn/FIDO forwarding are **not yet
implemented**. Their platform helpers, device consent and hardware acceptance
requirements are recorded in [peripheral forwarding](peripheral-forwarding.md).
Do not infer support from offline key diagnostics or SSH-agent support. No new
privileged device helpers or forwarding kernel modules have been activated.

The currently running production host remains on the previous release while
these changes are tested. A package build does not deploy or restart it.

Local verification (2026-09-22): all **79 tests** pass with normally ignored
cases enabled (68 unit/media/authentication tests and 11 process/network/UI
tests). This includes native libfido2 signature verification with synthetic
ES256 fixtures, actual OpenSSH signing, active managed-device revocation, and
real mouse/keyboard interaction with Host Settings and the launcher. Physical
FIDO hardware was not available; no physical-key success is claimed. Linux
strict Clippy and Rust/Nix formatting checks pass. Mac verification of this
checkpoint remains pending CI and physical-device testing.

## Version 0.3

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

The v0.3.0 application at commit `35e5808` was subsequently deployed through the
same declarative NixOS service, preserving identity and portal restoration.
After activation, a real Wayland connection accepted native resolution, 60 FPS
and 20 Mbps settings and decoded 15 frames at 2560×1440 using NVIDIA H.264
encoding/decoding, with zero unmatched frame identities. This was a local host
smoke test, not a physical Mac or sustained latency benchmark. The compositor
package and running KDE session were unchanged; experimental KWin HDR was not
enabled. The `be7ad5e` follow-up only fixes Mac linting of a Linux-only function.

See [testing.md](testing.md) for real-device acceptance and
[distribution.md](distribution.md) for installation and service setup.
