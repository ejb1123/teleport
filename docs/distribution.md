# Installing and distributing Teleport

| Platform | Release asset | Host | Client | Needs Nix | Built from |
|---|---|---|---|---|---|
| Any Linux (glibc, x86-64) | `Teleport-VERSION-x86_64.AppImage` | yes | yes | no | `nix build .#appimage` |
| Any Linux (glibc, x86-64) | `teleport-VERSION-x86_64-linux.tar.gz` | yes | yes | no | `nix build .#tarball` |
| Debian 12/13, Ubuntu 22.04/24.04 | `teleport_VERSION-1_amd64.deb` | yes, with systemd unit and udev rules | yes | no | `nix build .#deb` |
| Fedora, openSUSE Tumbleweed/Leap | `teleport-VERSION-1.x86_64.rpm` | yes, as above | yes | no | `nix build .#rpm` |
| Arch, CachyOS, Manjaro, EndeavourOS | `teleport-desktop-bin-VERSION-1-x86_64.pkg.tar.zst`, AUR `teleport-desktop-bin` | yes, as above | yes | no | `packaging/arch/PKGBUILD` |
| Linux, sandboxed | `Teleport-VERSION-x86_64.flatpak` (`dev.teleport.Teleport`) | **no** (see below) | yes | no | `packaging/flatpak/` |
| macOS 11+ | `Teleport-VERSION-{arm64,x86_64}.{dmg,zip}`, Homebrew cask `teleport-desktop` | no | yes | no | `tools/macos-app.sh` |
| NixOS / Nix | flake package and NixOS module | yes | yes | yes | `nix build` |

Everything in the first seven rows is attached to each GitHub release with
`SHA256SUMS` (see "Making a release"). The AppImage, tarball, .deb, .rpm and
Arch package all contain one relocatable directory (`nix/bundle.nix`): the
Nix-built binary, glibc and its loader, GStreamer and its plugins, FFmpeg,
SDL, PipeWire's client library, libfido2 and a font, about 400 MB unpacked.
GPU drivers are not in it; they come from the system (see "GPU drivers in the
relocatable bundle"). The macOS app bundles the same way on its side.

Not Gravitational's Teleport: the .deb and .rpm are called `teleport` (a name
Gravitational's own repositories also use), the Arch package and the
Homebrew cask `teleport-desktop`, because Arch's AUR and homebrew-core
already have Gravitational's `teleport`. Both install a `teleport` command,
so the packages conflict and cannot be installed together (Homebrew links
ours as `teleport-desktop`).

## Install by distribution

Download the files from the
[release page](https://github.com/ejb1123/teleport/releases) and check them
with `sha256sum -c SHA256SUMS --ignore-missing`. The packages are not signed
yet (see "Release artifacts").

| System | Install | Tested 2026-10-18 (stock containers, `tools/package-smoke.sh`) |
|---|---|---|
| Debian 12, 13; Ubuntu 22.04, 24.04 | `sudo apt install ./teleport_VERSION-1_amd64.deb` | install, files, doctor, loopback session |
| Fedora 44 | `sudo dnf install ./teleport-VERSION-1.x86_64.rpm` | as above |
| openSUSE Tumbleweed, Leap 15.6, 16.0 | `sudo zypper install --allow-unsigned-rpm ./teleport-VERSION-1.x86_64.rpm` | as above |
| Arch, CachyOS (Manjaro, EndeavourOS) | `sudo pacman -U ./teleport-desktop-bin-VERSION-1-x86_64.pkg.tar.zst`, or an AUR helper: `yay -S teleport-desktop-bin` | as above, the package built with makepkg in `archlinux:latest` |
| Any glibc Linux | AppImage (below), or `tar -xzf teleport-VERSION-x86_64-linux.tar.gz` and run `teleport-VERSION-x86_64-linux/AppRun` | AppImage: Ubuntu 22.04, Fedora 44; tarball: Debian 12, Arch: doctor, loopback session |
| Any Linux with Flatpak (client) | `flatpak install --user ./Teleport-VERSION-x86_64.flatpak` | 2026-10-17 locally (doctor, loopback in the sandbox); the release job is not run yet |
| macOS 11+ (client) | open the .dmg and drag Teleport to Applications, or `brew install --cask ejb1123/teleport/teleport-desktop` | not tested: no Mac; the release job runs doctor; `brew audit` of the cask passes |
| NixOS | the flake (see "Opt-in NixOS graphical-session service") | CI (`nix build`) |

Then start `teleport` (the launcher, also in the desktop menu) on the client,
and on the host follow "Debian, Ubuntu, Fedora: .deb and .rpm" below for the
systemd user unit, which every native package installs the same way.
Manjaro and EndeavourOS use Arch's repositories and pacman and are covered by
the Arch test; CachyOS is tested with its own image (`cachyos/cachyos`).

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
files are in `usr/share/teleport/system/` inside the image
(`--appimage-extract`; `packaging/linux/system/` in a checkout); the
.deb/.rpm install them for you. The system-login PAM helper, linked against
the system's glibc and libpam, is `usr/libexec/teleport/teleport-pam` inside
the image; install it root-owned as [system login](system-login.md) ("Other
Linux hosts") describes. The documentation is in `usr/share/doc/teleport/`;
`teleport --help` names the pages and points at this version's on GitHub.

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
sudo zypper install --allow-unsigned-rpm ./teleport-0.5.4-1.x86_64.rpm   # openSUSE
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
| `/usr/lib/systemd/user/teleport-lock-on-login.service` | locks the session after an automatic login (`unattended.enable`) |
| `/usr/share/teleport/examples/sddm-autologin.conf`, `kscreenlockerrc` | opt-in examples: SDDM automatic login, KDE lock on start (`unattended`) |
| `/usr/lib/teleport/teleport-pam` | system-login PAM helper, root-owned 0755 (`systemLogin`) |
| `/etc/pam.d/teleport` | its PAM policy (`common-auth` or `system-auth`); kept on upgrade |
| `/usr/share/doc/teleport/` | `README.md` and `docs/*.md`, which `--help`, messages and the units cite |

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

Linux-password login: add `--system-auth-helper /usr/lib/teleport/teleport-pam`
to `TELEPORT_HOST_ARGS` ([system login](system-login.md)). Unattended start
after a reboot (automatic login, then an immediate lock; what the NixOS
module's `unattended` does): [wake and unattended
start](wake-and-unattended.md), "Unattended start without NixOS". Ubuntu's
minimal images tell dpkg to skip `/usr/share/doc`; there `--help` points at
the documentation on GitHub instead.

Tested (2026-10-17): the .deb installs on Debian 13 and Ubuntu 24.04, the .rpm
on Fedora 43 (containers); the files above are in place with mode 0644,
`uinput` exists, `systemd-analyze verify` accepts the unit, `udevadm verify`
the rules, and `/usr/bin/teleport doctor` and the loopback session pass. The
unit has not run under a real graphical session on these distributions.
Retested 2026-10-18 with `tools/package-smoke.sh` (install, files, `uinput`,
doctor, loopback session): the .deb on Ubuntu 22.04 and 24.04 and Debian 12
and 13, the .rpm on Fedora 44 and openSUSE (below). Retested 2026-10-19
locally with the unattended-start files, the documentation and the PAM
helper: on all ten package images (.deb, .rpm, Arch, CachyOS) the helper is
root-owned 0755, the policy root-owned 0644 with the distribution's stack
(`common-auth` on Debian, Ubuntu and openSUSE, `system-auth` on Fedora, Arch
and CachyOS), and the helper accepts a test account's password and refuses a
wrong one. The documentation is not extracted on Ubuntu's and Arch's
container images, which exclude `/usr/share/doc`.

## openSUSE

The .rpm installs unchanged on Tumbleweed and Leap: the bundle carries its
own libraries, so its only requirements are `/bin/sh` and the coreutils
AppRun uses, and its recommendations (`libglvnd-glx`, `libglvnd-egl`, Fedora
names) are weak dependencies zypper skips when a name does not exist
(openSUSE has `libglvnd`). zypper refuses unsigned packages unless told:

```sh
sudo zypper install --allow-unsigned-rpm ./teleport-0.5.4-1.x86_64.rpm
```

The post-install script creates the `uinput` group with `groupadd` (in
`shadow`, present on every openSUSE installation; minimal containers too).
On Leap 15.6 (glibc 2.38) and 16.0 (2.40) the bundle's glibc 2.42 is newer; that
does not matter, because the bundle uses its own glibc and loader for
everything it starts (see "GPU drivers in the relocatable bundle").

## Arch Linux, CachyOS, Manjaro, EndeavourOS

The AUR package is `teleport-desktop-bin` (`packaging/arch/PKGBUILD`):
`teleport` and `teleport-bin` in the AUR are Gravitational's. It installs the
release tarball's bundle in `/opt/teleport` and the same system files as the
.deb/.rpm (table above), plus `/usr/lib/sysusers.d/teleport.conf` for the
`uinput` group; pacman's own hooks then create the group, reload udev and
refresh the desktop and icon caches, and its install script loads `uinput`
and `uhid` and applies the sysctl. It conflicts with Gravitational's
packages, which also install `/usr/bin/teleport`.

```sh
# From the AUR, with a helper:
yay -S teleport-desktop-bin            # or paru -S teleport-desktop-bin
# By hand:
git clone https://aur.archlinux.org/teleport-desktop-bin.git
cd teleport-desktop-bin && makepkg -si
# Without the AUR: the package attached to the release
sudo pacman -U ./teleport-desktop-bin-0.5.4-1-x86_64.pkg.tar.zst
```

The release workflow builds that `.pkg.tar.zst` with makepkg from the same
PKGBUILD in `archlinux:latest`, and installs and runs it in `archlinux:latest`
and `cachyos/cachyos` (CachyOS's x86-64-v3 repositories do not matter: the
bundle brings its own libraries). Manjaro and EndeavourOS use Arch's packages
and pacman, so the Arch test covers them.

There is no source package (`teleport-desktop`, built with cargo against
Arch's FFmpeg, GStreamer, SDL 2 and PipeWire) yet: Teleport's build expects
the paths the Nix wrapper sets (`TELEPORT_FONT`, `TELEPORT_LIBFIDO2`,
GStreamer plugin paths) and a patched PipeWire for HDR capture, and nobody
has tried it against Arch's libraries.

### Publishing to the AUR

Done by hand by the maintainer, once per release, from the
`aur-teleport-desktop-bin` artifact of the release workflow run (PKGBUILD
with the release's version and sha256, `.SRCINFO`, the install file):

1. Once: create an account on <https://aur.archlinux.org>, add an SSH public
   key under "My Account", and in `~/.ssh/config`:

   ```
   Host aur.archlinux.org
     IdentityFile ~/.ssh/aur
     User aur
   ```

2. Once: `git clone ssh://aur@aur.archlinux.org/teleport-desktop-bin.git`
   (an empty repository; the first push creates the package).
3. Each release: copy `PKGBUILD`, `.SRCINFO` and `teleport-desktop-bin.install`
   from the artifact into that clone, check with `namcap PKGBUILD` and a
   `makepkg -si`, then
   `git add PKGBUILD .SRCINFO teleport-desktop-bin.install &&
   git commit -m "Update to 0.6.0" && git push`.

Without the artifact, `tools/arch-pkgbuild.sh 0.6.0
teleport-0.6.0-x86_64-linux.tar.gz OUTDIR` writes the same files (on Arch, or
run `makepkg --printsrcinfo > .SRCINFO` there afterwards).

## Generic Linux tarball

`teleport-VERSION-x86_64-linux.tar.gz` is the bundle itself, for systems
without a package format above or for installing somewhere else:

```sh
tar -xzf teleport-0.5.4-x86_64-linux.tar.gz -C ~/.local/opt
~/.local/opt/teleport-0.5.4-x86_64-linux/AppRun doctor
ln -s ~/.local/opt/teleport-0.5.4-x86_64-linux/AppRun ~/.local/bin/teleport
```

`AppRun` finds the bundle from its own path, so the directory can live
anywhere and be symlinked. The host's system files (systemd user units, udev
rules, modules-load and sysctl files, portal desktop entry, PAM policies) are
in `usr/share/teleport/system/`, the unattended-start examples in
`usr/share/teleport/examples/`, the PAM helper in
`usr/libexec/teleport/teleport-pam` and the documentation in
`usr/share/doc/teleport/` (which `--help` then cites); install them as the
.deb does (table above), changing the units' `ExecStart` paths if `teleport`
is not `/usr/bin/teleport`.

## Homebrew (macOS, and Linux)

The tap is `ejb1123/homebrew-teleport` (`packaging/homebrew/`): a cask for
the macOS app and a formula for the Linux tarball, both `teleport-desktop`
because homebrew-core's `teleport` is Gravitational's.

```sh
brew install --cask ejb1123/teleport/teleport-desktop   # macOS: Teleport.app
brew install ejb1123/teleport/teleport-desktop          # Linux (Linuxbrew)
```

The cask installs `Teleport.app` from the release .dmg (Apple silicon or
Intel), links the command-line client as `teleport-desktop`, and `brew
uninstall --zap` also removes `~/Library/Application Support/Teleport` (saved
desktops and pairing credentials), `~/Library/Caches/Teleport` and the
app's preferences and saved state. Until releases are notarised, macOS
blocks the ad-hoc signed app: right-click → Open, or
`xattr -dr com.apple.quarantine /Applications/Teleport.app` (the cask's
caveats say so).

The formula installs the tarball's bundle in the keg and a `teleport-desktop`
wrapper around `AppRun`. Homebrew's relocation does not touch it: the
binaries run through the bundle's own loader. It does not install the host's
system files (udev rules, systemd unit); Linuxbrew users who run the host
should use their distribution's package instead.

Checked 2026-10-18 in the `homebrew/brew` container (Homebrew 4.6.20, from
generated files): `brew style` passes for both; `brew audit --strict` passes
for the formula, and for the cask with `HOMEBREW_SIMULATE_MACOS_ON_LINUX=1`
for both architectures (offline; the release URLs do not exist yet); the
formula installs from the release tarball and `brew test` (its `--version`
and `doctor`) passes. The cask has not been installed: there is no Mac here.

**Setting up the tap** (the maintainer, once):

```sh
gh repo create ejb1123/homebrew-teleport --public \
  --description "Homebrew tap for Teleport (remote desktop)"
git clone https://github.com/ejb1123/homebrew-teleport && cd homebrew-teleport
mkdir -p Casks Formula
```

**Each release:** download the `homebrew-tap-update` artifact of the release
workflow run (the cask and formula with the release's version and sha256,
from `tools/homebrew-tap.sh`), copy `Casks/` and `Formula/` into the tap,
`brew style` and `brew audit --cask --online teleport-desktop` there, commit
and push. The templates in this repository keep `@VERSION@`/`@SHA256_…@`
placeholders and are not installable themselves.

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
   `otool -L`) into `Contents/MacOS`, `Contents/PlugIns` (no subdirectory: `codesign`
   takes any directory there for a bundle) and `Contents/Frameworks`, rewrites their install names and rpaths to
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

## Making a release

The version is set once, in `Cargo.toml`; the flake reads it from there,
and `tools/release-version.sh` keeps the files that repeat it (Cargo.lock,
`packaging/Info.plist`, the AppStream metainfo's release list, the Arch
PKGBUILD) in step. A release tag is `v` plus that version; the release
workflow refuses a tag that does not match.

1. Check "Release validation" below, and that CI is green on `main`.
2. Bump the version and commit:

   ```sh
   tools/release-version.sh set 0.6.0     # Cargo.toml, Cargo.lock, Info.plist, metainfo, PKGBUILD
   git diff                               # optionally add a "## 0.6.0" section to CHANGELOG.md
   git commit -am "Release 0.6.0"
   ```

3. Tag and push (pushing a tag needs nothing beyond push access; changes to
   `.github/workflows/` need a token with the `workflow` scope):

   ```sh
   git tag -a v0.6.0 -m "Teleport 0.6.0"
   git push origin main v0.6.0
   ```

4. The **Release** workflow (`.github/workflows/release.yml`) then builds,
   tests and publishes the GitHub Release `v0.6.0` with every artefact and
   `SHA256SUMS`. Its notes are the `## 0.6.0` section of `CHANGELOG.md` if
   there is one, otherwise the commit subjects since the previous `v*` tag
   (`tools/release-notes.sh`), followed by install pointers. A version with a
   hyphen (`0.6.0-rc.1`) is published as a pre-release (its Arch `pkgver`
   is `0.6.0_rc.1`, since pacman versions cannot contain `-`; nfpm writes
   the .deb/.rpm version its own way, and no pre-release has been built
   yet).
5. Publish the packages that live elsewhere, from the run's artifacts: the
   AUR (`aur-teleport-desktop-bin`, see "Publishing to the AUR") and the
   Homebrew tap (`homebrew-tap-update`, see "Homebrew").

Running the workflow by hand (Actions → Release → Run workflow) with an
empty tag builds and tests the branch without publishing anything; with an
existing tag it rebuilds that tag and replaces the release's assets.

## Release artifacts

The release workflow's jobs:

| Job | Runs on | Produces / checks |
|---|---|---|
| `version` | ubuntu-24.04 | the tag matches `Cargo.toml` and the files that repeat it |
| `linux` | ubuntu-24.04 | AppImage, .deb, .rpm and tarball (`nix build .#appimage .#deb .#rpm .#tarball`) |
| `arch` | `archlinux:latest` container | `teleport-desktop-bin-VERSION-1-x86_64.pkg.tar.zst` with makepkg from the tarball, and the AUR files |
| `linux-test` | stock images, no Nix | `tools/package-smoke.sh`: install, system files (also the unattended-start unit and examples, the documentation, and the PAM helper's and policy's owner and mode), a password check through the PAM helper with a test account, `teleport doctor`, loopback session (`host --source test` and `client --headless-frames 60 --software-decoder`, which must print `SMOKE PASS`) |
| `flatpak` | `flatpak-github-actions:freedesktop-26.08` | `Teleport-VERSION-x86_64.flatpak`; doctor and loopback session inside the sandbox |
| `macos` | macos-14 (arm64), macos-15-intel (x86_64) | `Teleport-VERSION-ARCH.dmg` and `.zip` (`tools/macos-app.sh --dmg`); the bundled binary's `--version` and doctor without Nix on PATH |
| `release` | ubuntu-24.04 | `SHA256SUMS`, notes, the GitHub Release; the `homebrew-tap-update` artifact |

`linux-test` covers: .deb on Ubuntu 22.04 and 24.04 and Debian 12 and 13;
.rpm on Fedora (latest), openSUSE Tumbleweed, Leap 15.6 and Leap 16.0; the
Arch package on Arch and CachyOS; the AppImage on Ubuntu 22.04 and Fedora;
the tarball on Debian 12 and Arch. `tools/package-smoke.sh KIND` runs the
same check locally:

```sh
mkdir -p dist && cp result*/* dist/   # from nix build .#deb etc.
docker run --rm -v "$PWD/dist:/dist:ro" -v "$PWD/tools:/tools:ro" \
  opensuse/tumbleweed sh /tools/package-smoke.sh rpm
```

**macOS signing.** When the repository has the signing secrets, the app is
signed with the Developer ID and notarised; otherwise it is ad-hoc signed and
the job warns. Secrets: `MACOS_CERTIFICATE_P12` (base64 of the Developer ID
Application certificate and key, exported as .p12),
`MACOS_CERTIFICATE_PASSWORD`, `MACOS_SIGNING_IDENTITY` (e.g. `Developer ID
Application: Jane Doe (TEAMID1234)`), `NOTARY_KEY_P8` (base64 of an App Store
Connect API key), `NOTARY_KEY_ID` and `NOTARY_ISSUER`. The certificate goes
into a temporary keychain that the job deletes. An ad-hoc signed app is
rejected by Gatekeeper on other Macs (right-click → Open, or
`xattr -dr com.apple.quarantine Teleport.app`, to run it anyway).

Linux packages are not signed: no repository signing key exists yet. The
AppImage can carry a GPG signature (`appimagetool --sign`) and the .deb/.rpm
are normally signed by the repository that serves them; both need a key and a
place to publish (below). Checksums do not establish trust on their own;
obtain artifacts only from the release page or a trusted repository. Only
x86-64 Linux packages are built; arm64 would need an `ubuntu-24.04-arm` job.

## Future package repositories

Not set up; each would let users get updates with their package manager
instead of downloading every release:

- **openSUSE Build Service** (build.opensuse.org): one project can build and
  sign .rpm (openSUSE, Fedora) and .deb (Debian, Ubuntu) repositories. It
  builds from source in its own sandbox without network access, so it needs
  an RPM spec / Debian source package with vendored crates (`cargo vendor`)
  rather than the Nix bundle; or a `_service` that repackages the release
  tarball, which OBS allows for binary packages in home projects.
- **Ubuntu PPA** (Launchpad): source packages only, built on Launchpad
  without network access, so it needs a Debian source package with vendored
  crates, signed with the maintainer's GPG key and uploaded with `dput`; one
  upload per Ubuntu series.
- **Flathub**: submit `packaging/flatpak/dev.teleport.Teleport.yml` (client)
  as a pull request to `flathub/flathub`; it needs a tagged git source
  instead of `type: dir`, a license, screenshots in the metainfo, and
  reviewers' acceptance of `--device=all`.
- **A self-hosted apt/rpm repository** signed with a project key (e.g. on
  GitHub Pages, built by the release workflow) is the smallest step that
  gives signed packages and updates.

## Release validation

Before tagging a release, test Mac-to-Linux desktop streaming, reconnect,
clipboard opt-in, monitor switching, and portal restoration on real devices,
and for the packages: the signed `.app` on a clean Mac without Nix (Gatekeeper,
microphone and keyboard-capture prompts, hardware decoding), the AppImage and
a .deb/.rpm host on a real KDE and GNOME desktop of another distribution
(portal consent and restore, gamepads through the installed udev rules, the
systemd user unit), and NVIDIA and AMD clients.
