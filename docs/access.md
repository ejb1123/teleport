# Host settings and persistent login

These features require the updated host and client. They do not change the Linux
login password, unlock a logged-out graphical session, or replace portal consent.
Use a trusted LAN/VPN; the authentication integration has not had an independent
security audit.

## Configure the host

Start a persistent host (`--identity-dir`) or use the existing NixOS user service.
On Linux, choose **Host settings** in the native launcher, or run:

```sh
teleport host-manager
```

The native settings window manages the local host's Teleport account, one-time
pairing window, and enrolled devices. Service start/stop controls target
`teleport-desktop.service`; they do not install that service or edit NixOS config.
The default identity directory is `$HOME/.local/state/teleport/host`.

From an already trusted SSH login to the host, use:

```sh
teleport host-admin status
teleport host-admin set-password work
```

Enter a new Teleport passphrase twice at the hidden prompts. Do not pass secrets
in command arguments, chat, or environment variables. The username is case
sensitive; a passphrase must have at least 12 characters. This is one Teleport
account per host, not integration with Linux accounts/PAM.
For a custom identity directory, put `--identity-dir PATH` after `host-admin`
and before its subcommand. Administration uses a private same-user Unix socket,
not a network management endpoint. The updated host must be running.

## Connect from Linux or macOS

Run the client locally on the computer whose keyboard and display you are using:

```sh
nix build
./result/bin/teleport
```

Enter `HOST:4443`, the Teleport username and passphrase, then **Log in & save**.
Choose **Connect to desktop**. The client saves a device credential and the
host's certificate fingerprint, not your passphrase. Subsequent connections use
that saved identity without another password prompt.

CLI enrollment also hides the password prompt:

```sh
teleport login HOST:4443 --username work --device-name work-linux
```

Allow TCP on the host's listening port for enrollment and UDP for streaming,
restricted to the intended network. With a password configured, enrollment TCP
stays available. Disabling password login closes it unless a code window is open.

To use another address for the same host (for example LAN versus ZeroTier), select
the saved host, edit its address and click **Use saved identity**. This explicitly
copies the pinned credential to the new address; it never trusts an IP address or
VPN membership. A different server certificate still fails verification.

## Pairing and revocation

One-time code pairing remains available without restarting the host:

```sh
teleport host-admin open-pairing
teleport host-admin close-pairing
teleport host-admin revoke-device DEVICE_ID
teleport host-admin revoke-all-devices
teleport host-admin disable-password
```

Treat an opened pairing code as secret. Codes expire after five minutes, have a
bounded attempt count, and enroll one device. New code and password enrollments
on persistent hosts get individual credentials. Revoking a managed device also
disconnects its active session, normally within one second.

Changing or disabling the password revokes **password-enrolled** devices, not
code-enrolled devices. **Revoke all devices** revokes all managed enrollments.
Neither operation revokes the old shared credential in `pairing.json` or its
copies. Legacy imported credentials remain accepted to preserve existing access.
To invalidate that shared credential, stop the host and rotate its entire
identity, then re-enroll clients; this changes the TLS fingerprint too. Do not
delete or rotate identity files while relying on the only remote connection.

## Security boundaries

Password enrollment uses OPAQUE (`opaque-ke` 4.0.1), Ristretto255/TripleDH/SHA-512
and fixed Argon2id parameters (64 MiB, three passes, one lane). The authenticated
exchange binds the host certificate fingerprint and enrollment metadata; the
device credential is returned with authenticated encryption. The host persists
an OPAQUE registration record, not a plaintext password. Managed bearer tokens
are stored as hashes on the host. The compatibility shared token remains in its
existing private identity file.

Messages and handshakes are bounded. A persistent host-wide attempt budget and
per-address throttling limit online guessing, but a reachable attacker can still
cause denial of service. A strong unique passphrase and network restrictions
remain necessary. Identity files are private to the host user; client credential
files grant desktop access and must also stay private.

Security-key login and peripheral forwarding are separate features. A password
login does not implicitly grant access to a client's SSH agent, smartcard, USB
devices, or FIDO authenticator.
