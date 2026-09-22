# One-time code pairing

Start the host from your graphical-session terminal with `--pair` and a persistent
`--identity-dir`. After portal approval it prints a random six-digit code. Enter
the host's address under **Add desktop** in the native launcher. **Connect** opens
sign-in; select **Pairing code**, enter the code, and **Continue**. Verify and
approve the first-contact host fingerprint to save private credentials and open
the desktop automatically. A changed saved fingerprint is blocked.
No manual file transfer or subsequent trust prompt is needed. The existing file
import workflow remains available. This does not change Wayland consent rules.

## Transport and authentication

- Enrollment uses TCP on the same numeric address/port as the UDP desktop host.
  Allow TCP 4443 temporarily for enrollment if using the default port. There is
  no automatic discovery, public rendezvous service or NAT traversal.
- RustCrypto SPAKE2 with distinct client/host roles derives an ephemeral shared
  secret from the code. The code is never transmitted as plaintext or a hash.
- HKDF-SHA256 binds the exchange transcript and separates client-confirmation
  and credential-encryption keys. The host checks HMAC-SHA256 key confirmation
  before releasing a ChaCha20-Poly1305 authenticated/encrypted credential packet.
- The client authenticates/decrypts the host packet and validates the credential
  before saving it in the existing 0700 profile directory and 0600 files.
- Subsequent MoQ connections still strictly pin the saved TLS certificate.
  Changed identities fail; there is no trust-all certificate verifier or automatic
  fallback. A fresh host code can renew credentials for the same identity, but
  cannot replace a saved fingerprint. Investigate unexpected identity changes
  before explicitly removing old trust.

## Limits and recovery

The listener exists only with explicit `--pair`, for five minutes and five total
TCP connection attempts, closing after one authenticated enrollment. Successful
authentication consumes the code before credentials are transmitted; if delivery
or local saving fails, restart enrollment rather than reuse the code. Codes are
not stored in profiles or accepted on the CLI command line. The host's displayed
code is sensitive, so don't expose its terminal output or service logs.

Any connection, including malformed or abandoned connections, consumes an attempt.
An attacker on the network can exhaust the window and deny enrollment; this fails
closed rather than granting access. Each handshake is bounded to ten seconds and
messages are length-limited. No firewall/service settings change automatically.

To recover, stop/restart the host with `--pair` and the **same identity directory**.
Previously trusted clients keep working. Starting without persistent identity
instead invalidates old client credentials on the next host restart. Paired
clients share the existing host token; this is not per-device revocation.

## Security maturity

This is experimental LAN/VPN enrollment, not an independently audited security
protocol implementation. In particular, the selected SPAKE2 crate documents that
it has not had an independent audit and warns about timing behavior; see its
[upstream security notes](https://docs.rs/spake2/0.4.0/spake2/#security).
Use trusted SSH/manual pairing-file transfer where that maturity is insufficient.

Tests cover incorrect codes, tampered ciphertext, code reuse, expiry, exhausted
attempts, private credential saving, and actual desktop connections using the
saved result. No test is a substitute for a cryptographic security audit.
