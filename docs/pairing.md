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

- Direct enrollment uses TCP on the same numeric address/port as the UDP
  desktop host. Allow TCP 4443 temporarily for enrollment if using the default
  port. There is no automatic discovery or NAT traversal; a host behind NAT can
  instead pair [through a relay](#pairing-through-a-relay-no-reachable-host-port).
- RustCrypto SPAKE2 with distinct client/host roles derives an ephemeral shared
  secret from the code. The code is never transmitted as plaintext or a hash.
- HKDF-SHA256 binds the exchange transcript and separates client-confirmation
  and credential-encryption keys. The host checks HMAC-SHA256 key confirmation
  before releasing a ChaCha20-Poly1305 authenticated/encrypted credential packet.
- On a host with managed devices, the client's relay device key is part of
  the issued credential: after key confirmation the client sends the public
  half of a fresh Ed25519 key (sealed under the SPAKE2-derived keys), and the
  host stores it in the same record as the new token before sealing the token
  together with that key. Without the key nothing is issued and the code is
  not consumed. The `TPPAIR02` magic that announces this is part of the
  transcript; clients from before device keys send `TPPAIR01` and pair
  hash-only. See [device keys](relay.md#device-keys).
- The client authenticates/decrypts the host packet and validates the credential
  (and, for `TPPAIR02`, that the host bound its key) before saving it in the
  existing 0700 profile directory and 0600 files.
- Subsequent MoQ connections still strictly pin the saved TLS certificate.
  Changed identities fail; there is no trust-all certificate verifier or automatic
  fallback. A fresh host code can renew credentials for the same identity, but
  cannot replace a saved fingerprint. Investigate unexpected identity changes
  before explicitly removing old trust.

## Pairing through a relay (no reachable host port)

A host that uses a [relay](relay.md) can be paired through it, so the host
needs no reachable port at all, not even for setup:

```sh
teleport host --identity-dir ~/.local/state/teleport/host --pair \
  --relay moqt://relay.example:4444 --relay-fingerprint <relay fp> --relay-key <key>
# Pairing code: 123-456 (one use, expires in 5 minutes)
# Relay pairing link: teleport://relay/<32 hex digits>/123-456 (same code; ...)

teleport pair desktop.example:4443 \
  --relay moqt://relay.example:4444 --relay-fingerprint <relay fp> --relay-key <key>
# Relay pairing link from the host: <paste the link>
```

`teleport host-admin open-pairing` and **Open pairing** in the host manager
show the link too (`relay_pairing`). The client reads the link from stdin,
never from its command line. The address only names the saved profile; it is
not contacted. The result is the normal per-device credential (token and host
fingerprint), saved like any other. Later sessions use the relay namespace
derived from that fingerprint, exactly as in [relay mode](relay.md).

Why a link and not only the code: before pairing, the client does not know the
host fingerprint the host's relay namespace comes from. Opening pairing makes
the host pick a fresh random 128-bit *rendezvous* and listen on the relay
namespace `SHA-256("teleport-relay-pairing-v1" || rendezvous)[..16]` until the
code is used, closed, replaced or expires. The link carries the rendezvous and
the code; treat it exactly like the code.

The exchange is the same `TPPAIR02` SPAKE2 protocol as over TCP, byte for byte, over
an ordered stream made of two relay tracks per attempt (`<attempt>/request`,
`<attempt>/response`; see [relay enrollment](relay.md#enrollment-through-the-relay)).
Limits are shared with TCP pairing: one code, five attempts in total across
TCP and relay, five minutes, consumed before the credential is sent.

### What the relay learns, and why it cannot pair or intercept

- **Not the code.** The relay sees only the derived namespace, which depends on
  the random rendezvous, not on the code. SPAKE2 messages reveal nothing a
  passive observer can test guesses against: there is no offline dictionary
  attack on the six digits.
- **One guess per attempt at most.** An active relay (or anyone holding the
  relay key) can pretend to be the host or the client, but each such attempt is
  a single online guess (one in a million) and consumes one of the five
  attempts. Key confirmation (HMAC over the transcript) fails on any wrong
  guess, the host releases nothing, and the client gets nothing it can use.
- **No tampering with the result.** The credential is encrypted and
  authenticated under a key derived from the SPAKE2 secret and transcript, so
  the relay can neither read it nor substitute another host's fingerprint.
- **No replays.** Replaying a client's recorded messages to the host fails key
  confirmation, because the host's SPAKE2 share is fresh per attempt (tested);
  the host also serves each attempt id once.
- **What it can do:** see sizes and timing, drop traffic, or spend the five
  attempts (denial of service). That fails closed; open pairing again.

Host work per relay namespace is bounded: at most four relayed attempts at
once (others ignored), each at most 100 s and 64 KiB per direction.

Password enrollment (OPAQUE) and Linux system login also work through the
relay; see [relay enrollment](relay.md#enrollment-through-the-relay).

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
