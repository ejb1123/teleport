# Installing and distributing Teleport

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

## Linux desktop installation

```sh
nix profile add .
```

The package installs a desktop menu entry. Depending on the desktop, log out/in
or ensure the profile's `share` directory is included in `XDG_DATA_DIRS`.

## macOS application launcher

```sh
nix build
open result/Applications/Teleport.app
```

Install the package into a Nix profile to retain its store closure, then make an
alias to its `Applications/Teleport.app` in Finder. The `.app` is a **Nix-dependent
launcher**, not a portable standalone application. Copying just the `.app` to
another machine will not copy GStreamer, SDL, fonts, Rust executable or libraries.
It is not Developer-ID signed or notarized. A true distributable `.dmg` needs a
separate dependency-bundling pipeline, Apple Developer credentials, signing of
every bundled executable/library, notarization and clean-machine tests. This
repository does not pretend those requirements are complete or publish an
unsigned bundle as a signed release.

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

## Release validation

The manually dispatched release workflow builds Linux and macOS packages and
uploads Nix closure archives plus checksums. These are explicitly **Nix artifacts**,
not standalone installers; importing them is for trusted users with Nix installed.
These archives have checksums, not project cryptographic signatures; obtain them
only from a trusted repository/workflow. A checksum alone does not establish trust.
No GitHub release is automatically published and no signing secrets are required.
Before tagging a release, test Mac-to-Linux desktop streaming, reconnect,
clipboard opt-in, monitor switching, and portal restoration on real devices.
