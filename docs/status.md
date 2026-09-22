# Version 0.2 implementation status

This is a tested development milestone, not completion of every production
requirement. Both endpoints must run application protocol version 2.

| Area | Implemented and locally checked | Still outstanding |
| --- | --- | --- |
| Reliability | Authenticated reconnect; fixed identities across host restart; release held keys after client suspension/disconnect and graceful host SIGTERM; media reordering regression | Real Mac sleep/wake, prolonged sessions, lossy/congested network acceptance |
| Native UI | Saved hosts, explicit pairing import, connection/error window, Retry/Cancel; session monitor/audio/clipboard/fullscreen/reconnect/disconnect buttons | Physical Mac/Retina pass; accessible native form controls and file picker; keychain integration |
| GPU encoding | Auto probe/fallback; actual NVIDIA and software encode/decode with live bitrate changes | VA-API hardware validation; zero-copy capture/rendering |
| Adaptive quality | Bounded bitrate decrease/recovery based on receive backlog/skipped groups; fixed-bitrate override | Controlled WAN/congestion measurement, resolution/FPS adaptation, full bandwidth estimation |
| Audio and clipboard | Opt-in monitor-source Opus, mute, headless network PCM proof; explicit Unicode text roundtrip using isolated X11 clipboard | Real Mac output/device-change and Wayland clipboard acceptance; precise A/V synchronization; file transfer |
| Distribution | Reproducible Nix package, Linux desktop entry, Nix-dependent Mac `.app`, CI and manual closure artifacts | Portable Mac dependency bundle, Developer-ID signing/notarization, clean-machine installer tests |
| Multi-monitor | Authorized portal streams, X11 RandR regions, switching/correct coordinate origins; synthetic landscape/portrait and toolbar tests | Physical mixed-scale monitors; hotplug (currently restart host) |
| Unattended sessions | Stable private host identity, opt-in portal restore tokens, opt-in NixOS graphical-session user service | Compositor-specific permission-restoration acceptance; login-screen/pre-login access is not implemented |

The signed standalone Mac distribution needs Apple Developer signing/notarization
setup, but credentials alone are not the whole task: the portable dependency
bundle and clean-machine testing still need implementation. Never commit signing
keys, pairing credentials or portal tokens.

Wayland permission persistence is a request to the compositor, not a guarantee.
The combined capture/control session uses RemoteDesktop's restoration mechanism,
as required by the [portal API](https://flatpak.github.io/xdg-desktop-portal/docs/doc-org.freedesktop.portal.RemoteDesktop.html).
Initial consent remains local. No service, firewall, login or running host is
changed just by building this repository.

See [testing.md](testing.md) for real-device acceptance and
[distribution.md](distribution.md) for installation and service setup.
