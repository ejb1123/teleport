# Existing Linux account login (0.5.0)

Use the host account's current Linux password, e.g. `ej`, without registering a
separate Teleport password. This is opt-in and attaches to that user's **running
graphical session**, including its existing lock screen. It does not create a
desktop before local login, automatically unlock it, or bypass capture consent.
Do not restart the only host you can reach without a recovery connection.

## NixOS host

Update the Teleport package input, then enable the module option alongside your
existing settings:

```nix
services.teleport-desktop = {
  enable = true;
  package = inputs.teleport.packages.${pkgs.stdenv.hostPlatform.system}.default;
  users = [ "ej" ];
  systemLogin = true;
  listen = "0.0.0.0:4443"; # Restrict firewall access to your LAN/VPN.
};
```

The module installs a dedicated PAM policy and builds an unprivileged helper
against your NixOS system's PAM stack. Apply your configuration and restart that
user's host service during a safe maintenance window; no reboot is required.
This repository update alone does not change the running system configuration.

## .deb, .rpm and Arch package host

The packages install the helper and its policy; nothing uses them until the
host is started with `--system-auth-helper`:

| File | What |
|---|---|
| `/usr/lib/teleport/teleport-pam` | the helper, root-owned mode 0755, not setuid; runs as the desktop user |
| `/etc/pam.d/teleport` | the policy, root-owned 0644: the distribution's `common-auth` and `common-account` (Debian, Ubuntu, openSUSE) or `system-auth` (Fedora, Arch); no session |

The .deb installs the policy as a conffile and the Arch package lists it in
`backup=`, so changes you make are kept on upgrade. The .rpm serves Fedora
and openSUSE, which name their stacks differently, so its post-install script
writes the matching one (`/usr/share/teleport/pam/teleport.system-auth` or
`teleport.common-auth`) whenever `/etc/pam.d/teleport` is missing, and never
replaces an existing one. Review it if your distribution's PAM stack is
customised (extra factors are not supported, see below).

The helper is compiled when the package is built, but linked to run on the
system's own glibc and libpam: its interpreter is the system's dynamic loader
and it has no library path of its own, so it loads `/etc/pam.d/teleport` and
the system's PAM modules like any other program on the machine
(`nix/pam-helper.nix`). It needs glibc 2.34 or newer. The release smoke test
checks on each tested distribution that it accepts the right password and
refuses a wrong one for a test account.

Then, as the desktop user, in `~/.config/teleport/host.env` (distribution.md):

```sh
TELEPORT_HOST_ARGS=--system-auth-helper /usr/lib/teleport/teleport-pam
```

and `systemctl --user restart teleport-desktop` during a safe maintenance window.
The packaged unit already passes `--identity-dir`, which system login requires.
`--unlock-on-system-login` (unattended hosts, wake-and-unattended.md) goes on the
same line.

## Other Linux hosts (tarball, AppImage, source)

The tarball and the AppImage carry the same helper at
`usr/libexec/teleport/teleport-pam` and both policies in
`usr/share/teleport/system/` (`pam-teleport.system-auth`,
`pam-teleport.common-auth`) inside the bundle. The host refuses a helper that is
not root-owned or sits in a directory others can write, so install them as root
(from the unpacked tarball, or `--appimage-extract`):

```sh
sudo install -Dm755 usr/libexec/teleport/teleport-pam /usr/local/libexec/teleport-pam
# Only if /etc/pam.d/teleport does not exist; otherwise review and merge.
sudo install -m 0644 usr/share/teleport/system/pam-teleport.system-auth /etc/pam.d/teleport
```

Pick `pam-teleport.common-auth` where `/etc/pam.d/common-auth` exists
(Debian, Ubuntu, openSUSE). Alternatively compile the helper with the
distribution's compiler and PAM headers (`base-devel` and `pam` on Arch,
`libpam0g-dev` on Debian), outside `nix develop`, in a private build directory:

```sh
/usr/bin/cc -O2 -Wall -Wextra -Werror /opt/teleport/usr/share/teleport/teleport-pam.c -o teleport-pam -lpam
sudo install -Dm755 teleport-pam /usr/local/libexec/teleport-pam
```

(`packaging/pam/teleport-pam.c` in a checkout.) Never make the helper
setuid/setgid; never use the Nix-built `libexec/teleport-pam` of `nix build`
(linked to Nix's PAM) with the distribution's PAM modules.

Add `--system-auth-helper /usr/local/libexec/teleport-pam` to your Teleport user
service command. The host also requires `--identity-dir` and must run as the
desktop user, not root. For a manually launched host:

```sh
teleport host --listen 0.0.0.0:4443 \
  --identity-dir "$HOME/.local/state/teleport/host" \
  --system-auth-helper /usr/local/libexec/teleport-pam
```

Do not start a second host using an identity owned by a running service. Allow
TCP (login) and UDP (streaming) on the host port over your intended network only.

## Native client (Linux or macOS)

1. Obtain the certificate fingerprint from **Host settings**, or from
   `teleport host-admin status` over an already trusted SSH connection.
2. **Add desktop** with a name and `HOST:4443`, then choose **Connect** and
   **Linux account**. Enter the Linux username and password and click **Continue**.
3. On first contact, compare the full displayed SHA-256 fingerprint with your
   trusted source and explicitly approve it. The certificate-observation connection
   sends no credentials and deliberately aborts TLS; approval saves only the pin.
   Login then opens a new, strictly pinned TLS connection and launches the session.
   Later logins reuse the pin automatically, but still ask for the Linux password.
   Changed certificates are blocked, not silently replaced or re-approved.

This is Teleport's TLS certificate fingerprint, not the SSH server's host key.
SSH is only one trusted way to read and compare it on the host.

CLI equivalent (password prompted privately):

```sh
teleport login-system HOST:4443 --username ej --fingerprint VERIFIED_64_HEX_FINGERPRINT
```

The client sends the password only after pinned TLS verifies the server. There
is no automatic first-contact certificate acceptance on this path. The login
ticket authorizes one connection, expires unused after 60 seconds, and is
destroyed on disconnect or host restart. It lives only in a private temporary
client file, never saved profiles. Log in again after disconnect; automatic
reconnect is disabled for these tickets.

## Security boundaries and current limits

- Only the non-root account owning the host process can authenticate. Another
  user's password cannot attach to this desktop. Multiple simultaneous per-user
  hosts need different ports; there is no multi-user routing broker yet.
- PAM authentication and account checks must both succeed. Missing policy,
  wrong passwords, expired/disallowed accounts, helper failures and unsupported
  prompts fail closed. Only one standard password prompt is supported; additional
  PAM factors and password-change conversations are not implemented.
- Teleport's existing U2F enrollment requirement blocks PAM login instead of
  accepting password-only authentication around it. No automatic MFA downgrade.
  Explicitly saved pairing credentials keep their existing separate behavior.
- Password/account changes are checked at login, not continuously in an active
  stream. `teleport host-admin revoke-all-devices` invalidates current and pending
  system tickets as well as managed devices, but still preserves legacy trust.
- Passwords are not placed in arguments, environment, config or logs. The helper
  is root-owned but runs as the desktop user, with a timeout and bounded input.
  The media pipeline and network host are not elevated to root.
- Persistent per-address/global throttling precedes PAM; distribution lockout
  policy also applies. Do not repeatedly test bad passwords on a real account.
- This integration has not received an independent security audit. Synthetic PAM
  fixtures and TLS tests cover authentication failures and credential lifecycle;
  actual distribution/account policy still needs a supervised acceptance test.

## After a reboot: automatic login plus lock

NixOS `unattended.enable` logs the user in at boot and locks the session at
once, so the host runs before anyone is at the machine; a remote user then
types the password into the lock screen. With `unattended.unlockOnSystemLogin`,
a successful Linux account login (this PAM check) also unlocks the session
(`loginctl unlock-session`), without storing the password. Trade-offs and
setup: [wake and unattended start](wake-and-unattended.md).

## Before local login: not implemented

The current unit is a graphical-session user service. Boot-time network presence
and creating/attaching a graphical login session need a separate system broker,
session lifecycle and compositor/display-manager integration. PAM authentication
does not create a usable Wayland desktop or bypass portal consent. This milestone
must not be advertised as working until it is tested independently, including
logout, screen lock, multiple users and reconnects. No reboot or live PAM changes
are part of developing this feature.

References: upstream Linux-PAM
[authentication](https://github.com/linux-pam/linux-pam/blob/master/doc/man/pam_authenticate.3.xml)
and [account checks](https://github.com/linux-pam/linux-pam/blob/master/doc/man/pam_acct_mgmt.3.xml).
