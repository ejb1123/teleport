# Linux system-account login (planned; not implemented)

Target: install the host, allow an existing account such as `ej`, and authenticate
from a native client using that account's current Linux password. No separate
Teleport password registration and no copying passwords into Teleport settings.

## First milestone: attach to an existing desktop

- Add an explicit **Linux account** login mode alongside existing Teleport login.
  Do not silently downgrade between authentication methods.
- Use a dedicated `teleport` PAM policy for authentication **and account checks**.
  Deny root, empty passwords, expired/disallowed accounts and users not explicitly
  allowed by the administrator. Unsupported PAM challenges must fail closed.
- Authenticate the server before sending any system password: verified TLS with
  a previously trusted fingerprint, administrator-provisioned trust, or explicit
  first-use fingerprint verification. A certificate supplied by an untrusted
  connection is not sufficient by itself. Never send the password through the
  existing bare TCP enrollment framing.
- Keep passwords transient and out of arguments, environment, config and logs.
  The existing OPAQUE record cannot validate an arbitrary system PAM password.
- Isolate PAM in a short-lived local worker with bounded input, a timeout, strict
  peer/UID checks and the minimum necessary privileges. Do not elevate the SDL,
  media decoder, capture pipeline or entire network-facing host to root.
- Bind the authenticated account's resolved UID to that user's host agent and
  existing graphical session. Reject other-user session attachment. Preserve
  compositor capture consent and the existing screen-lock boundary.
- Apply persistent throttling and a bounded worker pool before PAM. Respect the
  system's lockout policy; tests must use disposable accounts/PAM fixtures, never
  guess passwords against the real `ej` account.
- Default system-login access to a session-scoped credential. Persistent device
  trust, if offered, must be a separate explicit choice with revocation and
  documented behavior after password changes/account disablement.
- Provide NixOS options and an Arch installation path using the host distribution's
  PAM stack. Do not assume Nix-packaged PAM modules can load arbitrary Arch modules.
- Test correct/incorrect passwords, denied/expired users, other-user routing,
  untrusted/changed server certificates, worker failure, throttling, and disconnect
  cleanup before enabling this on the production desktop.

## Separate milestone: before local login

The current unit is a graphical-session user service. Boot-time network presence
and creating/attaching a graphical login session need a separate system broker,
session lifecycle and compositor/display-manager integration. PAM authentication
does not create a usable Wayland desktop or bypass portal consent. This milestone
must not be advertised as working until it is tested independently, including
logout, screen lock, multiple users and reconnects. No reboot is part of the
current renderer fix.

References: upstream Linux-PAM
[authentication](https://github.com/linux-pam/linux-pam/blob/master/doc/man/pam_authenticate.3.xml)
and [account checks](https://github.com/linux-pam/linux-pam/blob/master/doc/man/pam_acct_mgmt.3.xml).
