# YubiKey NEO: password plus U2F touch

The NEO's FIDO interface is U2F/CTAP1, not FIDO2. The explicit `--u2f` path
requires physical presence, not a FIDO2 PIN or built-in user verification. It is
only used as a second factor after a successful Teleport password exchange.
FIDO2 enrollment still requires user verification; no automatic downgrade occurs.
Native CTAP1 operations use [libfido2's explicit U2F mode](https://developers.yubico.com/libfido2/Manuals/fido_dev_force_u2f.html),
not a custom implementation of the USB protocol.

This feature protects **new password-based device enrollments**, not every
desktop connection. Existing saved device tokens, code pairing and legacy shared
credentials retain their previous behavior. This is not a global key-only policy.
Do not use it as a claim that every path to the desktop requires a key touch.

## Setup (updated host and client required)

1. On the host, configure a Teleport account using `teleport host-admin
   set-password YOUR_USERNAME`. Read `teleport host-admin status` over an already
   trusted SSH connection and copy its certificate fingerprint. Do not take the
   fingerprint from an unauthenticated network advertisement.
2. On the computer with your NEO plugged in, run:

   ```sh
   ./result/bin/teleport security-key enroll --u2f \
     --fingerprint TRUSTED_HOST_SHA256_HEX --output neo-public.json
   ```

   This explicitly creates a non-discoverable credential. Touch the key when it
   flashes; enrollment also proves possession, so another touch may be needed.
   No PIN reset, smartcard PIN change or authenticator reset is performed.
   The output contains the credential's public information, not a private key.
   Keep it private and transfer it to the host over your trusted SSH connection.
3. On the host, approve that file locally:

   ```sh
   teleport host-admin require-u2f /path/to/neo-public.json
   teleport host-admin status
   ```

   A configured Teleport password is required. The public credential must match
   this host's fingerprint. Only one U2F credential is supported at a time in this
   initial integration; importing another explicitly replaces the requirement.
4. Log in through the native launcher, or `teleport login HOST:4443 --username
   YOUR_USERNAME`. After the password succeeds, touch the enrolled key when it
   flashes. The launcher stays responsive while the key operation runs.
   The client saves an individual device credential only after verification.

Old clients cannot perform U2F enrollment and are rejected when it is required.
Keep a trusted local/SSH recovery path. To remove the requirement explicitly:

```sh
teleport host-admin disable-u2f
```

Changing the password preserves the key requirement. Managed-device revocation
remains available independently; see [access and migration](access.md).

## Arch Linux packaging

Use the wrapped executable `./result/bin/teleport`, not `.teleport-wrapped` or a
bare copied binary. The Linux wrapper removes inherited `LD_LIBRARY_PATH`,
`LD_PRELOAD`, `SDL_DYNAMIC_API` and `SDL3_DYNAMIC_API` **for this application only**.
It supplies matching Nix Fontconfig configuration instead of reading an
incompatible system configuration. No Arch packages or shell settings are changed.
External preload-based tooling/graphics wrappers are consequently not inherited;
they require separate, explicit compatibility work rather than leaking arbitrary
system libraries into the Nix process.

`security-key list` and `probe` remain read-only and no longer initialize
GStreamer. The executable still links SDL, so isolation must happen in the wrapper
before process startup, not after parsing the key command.

## Verification limits

Software tests exercise native libfido2 signature verification, password/key
requirements and rejection paths. Physical NEO enrollment and a complete touch
login must still be tested on the user's device. Smartcard/PIV and remote website
forwarding are separate features; this change does not implement them.
