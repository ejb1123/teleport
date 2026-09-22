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

## Arch host (not needed on a client-only laptop)

Build Teleport with Nix, but compile the helper with Arch's compiler/PAM outside
`nix develop`. Install `base-devel` and `pam` if missing. In a private build
directory (not a shared predictable temporary filename):

```sh
/usr/bin/cc -O2 -Wall -Wextra -Werror /path/to/teleport/packaging/pam/teleport-pam.c -o teleport-pam -lpam
sudo install -Dm755 teleport-pam /usr/local/libexec/teleport-pam
```

Review `packaging/pam/teleport.arch`, then install it root-owned mode 0644 at
`/etc/pam.d/teleport` only if no existing Teleport PAM policy exists. If one
exists, merge/review it rather than overwriting administrator policy. It includes
Arch's `system-auth` authentication and account checks. Never make the helper
setuid/setgid; never use the Nix-built helper with Arch PAM modules.

Add `--system-auth-helper /usr/local/libexec/teleport-pam` to your existing
Teleport user service command. The host also requires `--identity-dir` and must
run as the desktop user, not root. For a manually launched host:

```sh
./result/bin/teleport host --listen 0.0.0.0:4443 \
  --identity-dir "$HOME/.local/state/teleport/host" \
  --system-auth-helper /usr/local/libexec/teleport-pam
```

Do not start a second host using an identity owned by a running service. Allow
TCP (login) and UDP (streaming) on the host port over your intended network only.

## Native client (Linux or macOS)

1. Obtain the certificate fingerprint from **Host settings**, or from
   `teleport host-admin status` over an already trusted SSH connection.
2. Choose **Use Linux account** in the launcher. Enter the address, Linux username
   and password, and verified 64-hex fingerprint. For an exactly matching saved
   host, a blank fingerprint uses the already-pinned identity.
3. Click **Log in once**, then **Connect** within 60 seconds.

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
