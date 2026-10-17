# Installing and distributing Teleport

| Platform | Package | Host | Client | Needs Nix | Built from |
|---|---|---|---|---|---|
| Any Linux (glibc, x86-64/arm64) | `Teleport-VERSION-ARCH.AppImage` | yes | yes | no | `nix build .#appimage` |
| Debian/Ubuntu | `teleport_VERSION-1_ARCH.deb` | yes, with systemd unit and udev rules | yes | no | `nix build .#deb` |
| Fedora/openSUSE/RHEL | `teleport-VERSION-1.ARCH.rpm` | yes, as above | yes | no | `nix build .#rpm` |
| Linux, sandboxed | Flatpak `dev.teleport.Teleport` | **no** (see below) | yes | no | `packaging/flatpak/` |
| macOS 11+ | `Teleport.app` (zip/dmg), signed and notarised | no | yes | no | `tools/macos-app.sh` |
| NixOS / Nix | flake package and NixOS module | yes | yes | yes | `nix build` |

The AppImage, .deb and .rpm package one relocatable directory
(`nix/bundle.nix`): the Nix-built binary, glibc and its loader, GStreamer and
its plugins, FFmpeg, SDL, PipeWire's client library, libfido2 and a font, about
400 MB unpacked. GPU drivers are not in it; they come from the system (see
"GPU drivers in the relocatable bundle"). The macOS app bundles the same way
on its side. None of these packages is published yet: the release workflow
builds them (see "Release artifacts").

## Linux AppImage

```sh
chmod +x Teleport-0.5.4-x86_64.AppImage
./Teleport-0.5.4-x86_64.AppImage                 # launcher
./Teleport-0.5.4-x86_64.AppImage doctor
./Teleport-0.5.4-x86_64.AppImage host --listen 192.168.1.10:4443 --pair --identity-dir ~/.local/state/teleport/host
```

The AppImage runtime mounts the image with FUSE (built in; it needs
`fusermount3` or `fusermount`, from the `fuse3` package). Without FUSE (most
containers), set `APPIMAGE_EXTRACT_AND_RUN=1` or pass
`--appimage-extract-and-run` as the first argument, which unpacks it to a
temporary directory instead. The launcher's session window and the
privacy-mode guard are started through the AppImage file again
(`TELEPORT_SELF_EXE`), so they keep running after the launcher closes.

The host from an AppImage needs the same setup as a .deb install: the
`uinput` group and udev rules for gamepads, the UDP buffer sysctl and, for
portal pre-authorisation on KDE, the `dev.teleport.Host` desktop entry. The
files are in `packaging/linux/system/`; the .deb/.rpm install them for you.
The system-login PAM helper is not prebuilt (it must link the system's PAM):
`cc -O2 /opt/teleport/usr/share/teleport/teleport-pam.c -o teleport-pam -lpam`
(the .deb/.rpm path; in an AppImage the source is in the same place inside
the image), then follow [system login](system-login.md).

Tested (2026-10-17) with `APPIMAGE_EXTRACT_AND_RUN=1` in stock Ubuntu 22.04,
Ubuntu 24.04 and Fedora 43 containers without Nix: `teleport doctor` passes
and a loopback session (`host --source test`, `client --headless-frames 90`)
prints `SMOKE PASS`. With the host's Intel GPU passed in and Ubuntu's
`intel-media-va-driver`, the client decodes with VA-API through the system
driver (`decoder=ffmpeg:h264:vaapi`) and GStreamer's `vah264dec` registers;
with Ubuntu's Mesa on Xvfb the windowed client opens an OpenGL renderer and
runs to `--exit-after-frames`. On the NixOS development machine the image
mounts through FUSE and finds the NVIDIA driver (`nvh264enc` and `nvh264dec`
available). Not tested: a physical desktop session on another distribution,
an NVIDIA GPU in a non-Nix root, AMD, arm64.

## Debian, Ubuntu, Fedora: .deb and .rpm

```sh
sudo apt install ./teleport_0.5.4-1_amd64.deb      # Debian, Ubuntu
sudo dnf install ./teleport-0.5.4-1.x86_64.rpm     # Fedora
teleport                                           # launcher, also in the menu
```

The package installs the relocatable bundle in `/opt/teleport` with
`/usr/bin/teleport` pointing at its launcher, plus what `nix/module.nix`
configures on NixOS:

| File | Purpose |
|---|---|
| `/usr/lib/systemd/user/teleport-desktop.service` | host in the graphical session (`services.teleport-desktop`) |
| `/usr/lib/udev/rules.d/70-teleport.rules` | `/dev/uinput` and `/dev/uhid` for group `uinput` (`gamepads`, `playstationGamepads`) |
| `/usr/lib/modules-load.d/teleport.conf` | loads `uinput` and `uhid` at boot |
| `/usr/lib/sysctl.d/90-teleport.conf` | 8 MiB UDP buffers (`udpBuffers`) |
| `/usr/share/applications/dev.teleport.Host.desktop` | lets the portal identify (and KDE pre-authorise) the host |

Installing creates the `uinput` system group, reloads udev and the sysctl, and
enables nothing. Per host user:

```sh
sudo usermod -aG uinput "$USER"     # only for virtual gamepads; log out and in
mkdir -p ~/.config/teleport
cat >~/.config/teleport/host.env <<'EOF'
TELEPORT_LISTEN=192.168.1.10:4443
# TELEPORT_VIRTUAL_DISPLAY=fallback   # never | fallback | always
# TELEPORT_PRIVACY_MODE=off           # off | request | always
# TELEPORT_HOST_ARGS=--clipboard      # further host arguments
EOF
systemctl --user enable --now teleport-desktop
journalctl --user -u teleport-desktop -f
```

Pair clients as in "Native launcher" (start once by hand with `--pair`, or
`teleport host-admin` over SSH). KDE pre-authorisation, which the NixOS
module's `preauthorizeRemoteDesktop` does at every start:
`flatpak permission-set kde-authorized remote-desktop dev.teleport.Host yes`
(see "Opt-in NixOS graphical-session service" for what it allows). Uninstalling
leaves the group and per-user state in `~/.local/state/teleport`.

Tested (2026-10-17): the .deb installs on Debian 13 and Ubuntu 24.04, the .rpm
on Fedora 43 (containers); the files above are in place with mode 0644,
`uinput` exists, `systemd-analyze verify` accepts the unit, `udevadm verify`
the rules, and `/usr/bin/teleport doctor` and the loopback session pass. The
unit has not run under a real graphical session on these distributions.

## Flatpak (client)

`packaging/flatpak/dev.teleport.Teleport.yml` builds the client from source on
the Freedesktop 26.08 runtime (GStreamer, FFmpeg, SDL 2, libva and libfido2
come from the runtime; SDL2_ttf and GStreamer's `ximagesrc`, which the
runtime leaves out and `teleport doctor` checks for, are built as modules;
crates are vendored
from `cargo-sources.json`, regenerated with flatpak-builder-tools'
`flatpak-cargo-generator.py Cargo.lock` whenever `Cargo.lock` changes):

```sh
flatpak-builder --user --install-deps-from=flathub --force-clean \
  --repo=repo build packaging/flatpak/dev.teleport.Teleport.yml
flatpak build-bundle repo Teleport.flatpak dev.teleport.Teleport
flatpak install --user Teleport.flatpak
flatpak run dev.teleport.Teleport
```

Permissions: `--share=network` (QUIC), `--socket=wayland` and
`--socket=fallback-x11`, `--socket=pulseaudio` (host audio, and the
microphone for microphone passthrough), `--share=ipc`, and `--device=all`:
the GPU for VA-API/NVIDIA decoding and presentation, and hidraw for game
controllers (SDL's HIDAPI driver) and FIDO2 security keys, which Flatpak has
no narrower permission for. Portals need no permission. Flatpak's GL
extensions (`org.freedesktop.Platform.GL.*`) supply the GPU driver, matched
to the host's NVIDIA driver by Flatpak.

**The host is not supported in the Flatpak.** It needs the RemoteDesktop and
ScreenCast portals under a stable application ID that KDE can pre-authorise
(the sandbox's ID is fine, but unattended restore and virtual displays were
built and tested against `dev.teleport.Host`), write access to `/dev/uinput`
and `/dev/uhid` for virtual gamepads (Flatpak cannot grant these; `--device=all`
does not include them), the systemd user unit, KDE's `kscreen-doctor` for
virtual displays and privacy mode, and the PAM helper for system login.
Install the host natively (AppImage, .deb/.rpm) or with Nix.

Tested (2026-10-17) with flatpak-builder 1.4.4 in a throwaway `HOME`: the
manifest builds (a 5.7 MB single-file bundle; the runtime supplies the rest),
and inside the sandbox `teleport doctor` passes and a loopback session (both
ends in the sandbox, test source) prints `SMOKE PASS`, decoding on the NVIDIA
GPU through Flatpak's NVIDIA GL extension (`decoder=ffmpeg:h264:cuda`). Not
tested: a windowed session against a real host, Flathub's linter.

## Native launcher

Run the Linux host with `--pair --identity-dir PATH` to show a short-lived code.
Run `nix run .` (no arguments) on the client to open the native connection screen.
Enter `host:port` and that code, click **Pair & save host**, then **Connect / reconnect**.
Saved hosts connect without another pairing prompt. Initial enrollment also needs
TCP on the host's port (normally 4443); normal streaming still uses UDP.
See [pairing security](pairing.md) for expiry and limits.

Alternatively, click **Use file** and paste a pairing file's absolute path, or
drop it onto the window. Click **Import & trust pairing** only for a file obtained
from your host over a trusted channel, such as SSH.
Pairing imports must be regular files readable only by your account; if necessary,
run `chmod 600 /path/to/pairing.json` first. Symlinked profile directories and
credential files are rejected.
Use **Next saved host** to cycle saved hosts. Clipboard sharing is opt-in.
Saved profiles and copied pairing credentials live in `~/.config/teleport` on
Linux or `~/Library/Application Support/Teleport` on macOS. The directory is
private (0700) and written files are private (0600). They are not encrypted at
rest: protect your OS account and disk. Importing updated credentials replaces
the selected host entry; older credential copies remain available for recovery.
Remove obsolete `pairing-*.json` files manually after confirming they are unused.

The session is a separate native process. Closing the launcher keeps it running;
**Disconnect** stops the session launched by that launcher. Connection failures
appear in the launcher after the child exits; detailed logs remain in its terminal.
This small SDL UI has keyboard text entry, paste and file drop, but is not yet a
fully accessible platform-native form with a file picker or keychain integration.

## Linux desktop installation with Nix

```sh
nix profile add .
```

The package installs a desktop menu entry. Depending on the desktop, log out/in
or ensure the profile's `share` directory is included in `XDG_DATA_DIRS`.

## macOS application

**Installing:** open `Teleport-VERSION-arm64.dmg` and drag Teleport to
Applications (or unzip the `.zip`). A notarised build opens without warnings.
The first time keyboard capture (**Bind keys**, Control+Option+K) grabs the
keyboard, macOS asks to allow Teleport under Privacy & Security →
Accessibility / Input Monitoring; there is no usage string for these, the
prompt names the app. Microphone passthrough asks for the microphone with the
`NSMicrophoneUsageDescription` text. The bundle ID is `dev.teleport.Teleport`.

**Building** (on a Mac with Nix and the Xcode command line tools):

```sh
nix build
tools/macos-app.sh                                   # ad-hoc signed, this Mac only
tools/macos-app.sh --dmg \
  --identity "Developer ID Application: Jane Doe (TEAMID1234)" \
  --notary-profile teleport-notary                   # signed and notarised
```

`--notary-profile` names credentials saved once with
`xcrun notarytool store-credentials teleport-notary --apple-id ... --team-id ...`;
CI uses an App Store Connect API key instead (`NOTARY_KEY`, `NOTARY_KEY_ID`,
`NOTARY_ISSUER`; or `NOTARY_APPLE_ID`, `NOTARY_TEAM_ID`, `NOTARY_PASSWORD`).
`--keychain` selects the keychain holding the identity; `--out` the output
directory (default `dist/`). The script:

1. copies the unwrapped binary, GStreamer's plugin scanner and plugins,
   libfido2 and every `/nix/store` dylib they load (recursively, by
   `otool -L`) into `Contents/MacOS`, `Contents/PlugIns/gstreamer-1.0` and
   `Contents/Frameworks`, rewrites their install names and rpaths to
   `@rpath`/`@loader_path`/`@executable_path` and fails if any `/nix/store`
   reference remains. The binary finds its plugins, font and libfido2 from
   its own location (`app_bundle_environment` in `src/main.rs`), so the
   executable is the Mach-O binary itself, not a wrapper script;
2. writes `Info.plist` from `packaging/Info.plist` (version from
   `Cargo.toml`, build number from the commit count, `LSMinimumSystemVersion`
   from the binary), `Teleport.icns` from `packaging/icons` and the UI font;
3. signs every Mach-O inside-out with `codesign --options runtime --timestamp`,
   the main executable and plugin scanner with
   `packaging/macos/Teleport.entitlements` (microphone; `allow-jit` for
   GStreamer's ORC), then the bundle, and verifies it. Ad-hoc signing
   (`--identity -`, the default) skips the hardened runtime, whose library
   validation needs a Team ID;
4. with an identity and notary credentials, `notarytool submit --wait` on a
   zip, `stapler staple` and `spctl --assess`; then the zip, and with `--dmg`
   a signed, notarised and stapled disk image.

The script is checked with `shellcheck` but **has not run on a Mac**: no
macOS machine was available. Expect to fix details on the first run
(install-name collisions between dylibs of the same name from different
store paths, plugins that need extra entitlements).

`nix build` alone still produces `result/Applications/Teleport.app`, a
**Nix-dependent launcher** (a wrapper script around the store binary) for
people who use Nix: keep it in a Nix profile so its closure stays installed.

## Opt-in NixOS graphical-session service

Add the flake's `nixosModules.default` to your configuration imports, then:

```nix
services.teleport-desktop = {
  enable = true;
  package = inputs.teleport.packages.${pkgs.stdenv.hostPlatform.system}.default;
  users = [ "YOUR_USERNAME" ];
  listen = "192.168.10.108:4443"; # use your actual LAN or VPN address
};
# Only if your network policy allows it:
# networking.firewall.allowedUDPPorts = [ 4443 ];
```

This is a user service tied to an existing graphical login; it neither enables
automatic login nor creates a login-screen desktop. It preserves host identity
under `~/.local/state/teleport/host`. Copy the generated pairing file securely to
the client once. The initial Wayland portal request requires local consent.
If first-time identity creation is interrupted, an incomplete identity is rejected
instead of silently replacing keys. Inspect/back up that private directory and
start with a fresh directory if necessary; re-pair clients after replacing it.
Restore tokens request that consent be remembered; the compositor/portal decides
whether this is allowed and may prompt again after logout or permission changes.
No service is enabled and no firewall is changed merely by building this project.

The module raises `net.core.rmem_max`/`wmem_max` to 8 MiB (`udpBuffers`,
default on, `lib.mkDefault` so your own value wins); with the kernel default
the host logs `UDP receive/send buffer is smaller than requested` and can drop
packets during keyframe bursts. Elsewhere, on host and client:

```sh
printf 'net.core.rmem_max = 8388608\nnet.core.wmem_max = 8388608\n' |
  sudo tee /etc/sysctl.d/90-teleport.conf && sudo sysctl --system
```

The host registers the application ID `dev.teleport.Host` with the portal. On KDE,
pre-authorize it once so lost or expired portal sessions are recreated
without a local dialog, even while the screen is locked:
`flatpak permission-set kde-authorized remote-desktop dev.teleport.Host yes`, or set
`services.teleport-desktop.preauthorizeRemoteDesktop = true` and the NixOS module
registers it every time the host starts. Virtual displays and privacy mode
(`virtualDisplay`, `privacyMode`) rely on it for unattended use; see
[virtual displays](virtual-display.md).
Inspect with `systemctl --user status teleport-desktop` and
`journalctl --user -u teleport-desktop`. Stop it with `systemctl --user stop teleport-desktop`.
Do not enable user lingering expecting desktop capture to work before login.

### Test the NixOS unit without rebooting or switching the system

After adding the input and module to your system flake, build just its generated
unit (replace `nixos` with your configuration name):

```sh
teleport_unit=$(nix build --no-link --print-out-paths \
  '/etc/nixos#nixosConfigurations.nixos.config.systemd.user.units."teleport-desktop.service".unit')
systemctl --user link --runtime "$teleport_unit/teleport-desktop.service"
systemctl --user daemon-reload
# Stop any manually launched Teleport host first, to release its port/capture.
systemctl --user start teleport-desktop
journalctl --user -u teleport-desktop -n 30 --no-pager
```

Approve any local portal prompt, then test `systemctl --user restart
teleport-desktop` and reconnect from an already paired client. A running process
alone is not proof of unattended access: check that capture actually resumes
without local approval. Keep the same identity directory to preserve client trust.
The service does not automatically open code enrollment on startup.

This runtime link is temporary and does **not** activate the whole NixOS
configuration or persist across reboot. A later reviewed NixOS rebuild installs
the declarative service for future graphical logins. No reboot, logout, auto-login,
or user lingering is needed for this current-session test. A local test input can
use `git+file:///absolute/path/to/teleport`; its lock pins a committed revision,
so edits are not deployed until committed and the input is updated. Such an input
also requires that checkout when updating and is not a portable configuration.

## GPU drivers in the relocatable bundle

The AppImage, .deb and .rpm run Nix-built binaries on a foreign system. The
binaries are not patched to the system's loader: `AppRun` starts them through
the bundled glibc loader, `ld-linux-*.so --library-path PATH`. Only that
process gets PATH; `LD_LIBRARY_PATH` is left alone, so programs Teleport
starts (`kscreen-doctor`, `busctl`) keep the system's libraries. Bundled
helpers (`wl-copy`, `wl-paste`, `xclip`, GStreamer's plugin scanner) start
through the same loader and path (`TELEPORT_LIBRARY_PATH`).

PATH is, in order:

1. the bundle's `usr/lib`;
2. `TELEPORT_DRIVER_PATH` (colon-separated, for unusual driver locations);
3. `/run/opengl-driver/lib` (NixOS);
4. the distribution's `/usr/lib/<multiarch>`, `/lib/<multiarch>`,
   `/usr/lib64`, `/lib64`, `/usr/lib`, `/lib`, the directories listed in
   `/etc/ld.so.conf.d/*.conf` (where NVIDIA and CUDA packages register
   themselves), `/usr/local/cuda/lib64`;
5. the bundle's `usr/lib/fallback`: libglvnd's `libGL`/`libEGL`/`libGLX`/
   `libGLdispatch` and `libgbm`, so Teleport starts where the system has none
   (servers, containers); a desktop's own copies come first;
6. libva: the bundle's `usr/lib/libva` goes directly after `usr/lib` when the
   system's `libva.so.2` is older than the bundled one (FFmpeg needs the
   newer symbols, and older drivers load into a newer libva), and last
   otherwise (a driver built for a newer libva does not load into an older
   one). Versions are read from the file names.

What the bundle never contains, and therefore always comes from the system:
Mesa's drivers (`libEGL_mesa`, `libGLX_mesa`, `libgallium`, DRI and VA
drivers), NVIDIA's userspace (`libEGL_nvidia`, `libGLX_nvidia`,
`libcuda.so.1`, `libnvcuvid.so.1`, `libnvidia-encode.so.1`) and Intel's
oneVPL/Media SDK runtimes (`libmfx-gen.so.1.2`). Their versions must match the
kernel driver, which only the system knows. The GL dispatch libraries read the
system's vendor files (`/usr/share/glvnd/egl_vendor.d`) when they are the
system's own. `LIBVA_DRIVERS_PATH` defaults to the existing ones of
`/run/opengl-driver/lib/dri`, `/usr/lib/<multiarch>/dri`, `/usr/lib64/dri` and
`/usr/lib/dri`; set it to override. Vulkan's loader is bundled (FFmpeg links
it) and finds the system's ICDs in `/usr/share/vulkan/icd.d`.

The bundled glibc (2.42) is newer than every supported distribution's, so
system drivers linked against an older glibc load into it; the reverse would
not work. Where the system's and the bundle's copy of a common library
(`libdrm`, `libstdc++`, `libz`) differ, the bundle's wins, which is safe while
it is the newer one. On NixOS, prefer the Nix package: its libva and VA
drivers match each other, while the AppImage's libva may be older than the
system's drivers (`teleport doctor` then shows the `va*` elements unavailable).

Diagnose with `TELEPORT_DRIVER_PATH`, `LIBVA_DRIVERS_PATH`, `teleport doctor`
and the client's F8 decoder line; `LD_DEBUG=libs teleport doctor` shows where
each library was found.

## Release artifacts

`.github/workflows/release.yml` (manually dispatched) builds:

- the Nix closure archives (`nix-store --export`, for Nix users, as before);
- `linux-packages`: the AppImage, .deb and .rpm with `nix build`, then runs
  the AppImage's doctor and a loopback session in `ubuntu:24.04`, and the
  .deb (`debian:13`) and .rpm (`fedora:43`) doctors, and uploads them with
  `SHA256SUMS`;
- `macos-app`: `tools/macos-app.sh --dmg` on `macos-14` (Apple Silicon), then
  runs the bundled binary's doctor with a minimal environment.

With the `sign_macos` input set, it signs and notarises with these repository
secrets: `MACOS_CERTIFICATE_P12` (base64 of the Developer ID Application
certificate and key, exported as .p12), `MACOS_CERTIFICATE_PASSWORD`,
`MACOS_SIGNING_IDENTITY` (e.g. `Developer ID Application: Jane Doe
(TEAMID1234)`), `NOTARY_KEY_P8` (base64 of an App Store Connect API key),
`NOTARY_KEY_ID` and `NOTARY_ISSUER`. The certificate goes into a temporary
keychain that the job deletes. Without the input the app is ad-hoc signed,
which Gatekeeper rejects on other Macs (right-click → Open, or
`xattr -dr com.apple.quarantine Teleport.app`, to run it anyway).

Linux packages are not signed: no repository signing key exists yet. The
AppImage can carry a GPG signature (`appimagetool --sign`) and the .deb/.rpm
are normally signed by the repository that serves them; both need a key and a
place to publish, which are outside this repository. No GitHub release is
published automatically. Checksums do not establish trust on their own;
obtain artifacts only from a trusted repository or workflow.

## Release validation

Before tagging a release, test Mac-to-Linux desktop streaming, reconnect,
clipboard opt-in, monitor switching, and portal restoration on real devices,
and for the packages: the signed `.app` on a clean Mac without Nix (Gatekeeper,
microphone and keyboard-capture prompts, hardware decoding), the AppImage and
a .deb/.rpm host on a real KDE and GNOME desktop of another distribution
(portal consent and restore, gamepads through the installed udev rules, the
systemd user unit), and NVIDIA and AMD clients.
