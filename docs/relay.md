# Relay mode (end-to-end encrypted)

When the host's UDP port cannot be reached directly, both the host and the
client can connect *out* to a MoQ relay instead. The relay forwards objects it
cannot read or forge: every session is authenticated and encrypted end to end
between the client and the host.

Status (development, 2026-10): works with the self-hosted `teleport relay`.
Provider relays such as Cloudflare's: the IETF MoQ Transport path is implemented and
tested locally, but not yet verified against a provider (see below).

## Quick start (self-hosted relay)

On any machine both ends can reach over UDP (a small VPS works):

```sh
teleport relay --listen 0.0.0.0:4444 --identity-dir ~/.local/state/teleport/relay \
  --key "$(openssl rand -hex 16)"
# prints: RELAY FINGERPRINT <64 hex digits>
```

`--identity-dir` keeps the relay's certificate across restarts so the pinned
fingerprint stays valid. Allow UDP 4444 to the relay. `--key` (or
`TELEPORT_RELAY_KEY`) admits only hosts and clients that present the same key
with `--relay-key`; without it, anyone who can reach the relay could join a
host's namespace and observe traffic sizes and timing (never content) or
disrupt sessions. Use a key on any relay reachable from the internet.

Host (needs a persistent identity, as for saved trust):

```sh
teleport host --identity-dir ~/.local/state/teleport/host \
  --relay moqt://relay.example:4444 --relay-fingerprint <relay fingerprint> \
  --relay-key <relay key>
```

The host keeps accepting direct connections as well; relay sessions are
additional. It reconnects to the relay if the relay restarts.

Client (any existing pairing file or saved device credential works unchanged):

```sh
teleport client host.example:4443 --pairing-file <credential> \
  --relay moqt://relay.example:4444 --relay-fingerprint <relay fingerprint> \
  --relay-key <relay key>
```

The host address is not contacted in relay mode. The launcher does not expose
relay settings yet.

First-time pairing works through the relay too (`--pair` prints a relay
pairing link; `teleport pair ADDRESS --relay ...` reads it), so the host never
needs a reachable port. See [enrollment through the relay](#enrollment-through-the-relay).

## Enrollment through the relay

All three ways to obtain a credential run through the relay, unchanged, as the
same byte streams they use over TCP (`src/relay_enroll.rs`):

| Enrollment | Relay namespace | Client command | End-to-end protection |
| --- | --- | --- | --- |
| One-time code (`TPPAIR01`, SPAKE2) | rendezvous from the pairing link | `teleport pair ADDRESS --relay ...` (link on stdin) | PAKE: the code never leaves either end |
| Password (`TPAUTH01`, OPAQUE) | host namespace (`--relay-host`) | `teleport login ADDRESS --username U --relay-host ID --relay ...` | OPAQUE: authenticates the host fingerprint; the password never leaves the client |
| Linux account (`TPSYS001`, PAM) | host namespace, from `--fingerprint` | `teleport login-system ADDRESS --username U --fingerprint F --relay ...` | TLS 1.3 pinned to the host certificate, inside the relay stream |

`ID` is the host's relay id (its namespace; not secret), shown as
`relay_host` by `teleport host-admin status` and logged as `namespace=` when
the host connects. The client checks that the host OPAQUE authenticated has
that id. For Linux account login, the password is sent only after the TLS
handshake inside the relay stream has verified the pinned fingerprint, so the
relay sees only TLS ciphertext; a wrong pin sends nothing (tested, including
that the relayed bytes never contain the password). The resulting one-session
ticket then opens a relayed session (`login-system` passes the relay options
to the client it starts).

Layout per attempt, `<attempt>` being 128 random bits chosen by the client:

* `<attempt>/request`  client → host
* `<attempt>/response` host → client

Each is one ordered track; every chunk written is one group with one frame
(at most 4 KiB). The receiver restores order within a window of 16 groups,
rejects gaps and duplicates, and treats the end of the track as end of stream.
The framing adds no security of its own and needs none: every protocol it
carries authenticates its own transcript, so reordering, truncation or
injection only makes the exchange fail.

Host-side bounds per relay namespace: at most 4 attempts in flight (others
ignored), each at most 100 s and 64 KiB per direction, an attempt id served
once (replay cache of 1024). Password and system-login attempts through the
relay share one rate-limit source (the relay hides client addresses), in
addition to the persistent global limit, so a relay user can make relayed
logins temporarily unavailable — failing closed, like attempts over TCP.

## What protects the session

The relay terminates QUIC/TLS, so TLS alone would let it read everything and
would authenticate the relay rather than the host. Relay mode therefore adds
its own layer (`src/e2e.rs`, `src/bridge.rs`):

- **Host authentication is unchanged:** the host signs the handshake transcript
  with the private key of the same TLS certificate the client already pins by
  SHA-256 fingerprint. A relay cannot complete the handshake as the host.
- **The credential is never sent.** The client names its credential with
  `HMAC(SHA-256(token), ephemeral key ‖ request)`. Hosts store only
  `SHA-256(token)` for devices and tickets. A fresh ephemeral key per session
  keeps these identifiers unlinkable for the relay.
- **Device keys.** Devices enrolled by password login or code pairing also
  hold an Ed25519 key; the host stores only its public half. Such a device
  signs the handshake transcript (both ephemeral keys, the credential
  identifier, the request and the host certificate), and the host requires
  that signature before it accepts anything from the client. See
  [Device keys](#device-keys).
- **Session keys** come from an X25519 exchange mixed with the credential hash
  (forward secret; only an end of the handshake can derive them).
- **Every object** — video, audio, input, pointer, clipboard, SSH-agent
  traffic, control updates — is sealed with ChaCha20-Poly1305 under a key
  specific to its direction and track. Objects cannot be moved between tracks,
  groups or directions; input and control tracks reject any replay or
  reordering; media tracks accept reordering but never a repeat.
- **Connection options** (codec, HDR, SSH-agent opt-in) are covered by the
  credential MAC and the host signature; a relay that alters them is not
  recognised by the host and is rejected by the client.
- **Nothing starts before key confirmation.** After verifying the host, the
  client sends a sealed confirmation (and, for a keyed device, its sealed
  transcript signature). Only when it authenticates does the host lease the
  credential (claiming a single-use ticket) and capture the desktop.
  A replayed hello is answered at most once and never gets further.
- **Bounded handshake work:** at most 8 hellos are processed concurrently,
  each read within 2 s and confirmed within 5 s; a client ephemeral key is
  answered at most once.
- **Unknown or revoked credentials get no reply,** so the relay cannot probe
  which credentials exist. Revocation and single-use system-login tickets are
  enforced exactly as for direct sessions.
- **The relay itself is pinned** with `--relay-fingerprint` (self-signed relay)
  or verified against system roots (a provider with a public certificate).

## Device keys

Relayed hellos are recognised by `SHA-256(token)`, which is also what the host
stores in `access.json`. On its own, the stored hash would let anyone who read
the host's access record compute a valid hello, derive the session keys of a
session it starts itself, and confirm them. Device keys close that gap:

- **Enrollment.** After a password login (OPAQUE) or a code pairing (SPAKE2)
  delivers the device token, the client generates an Ed25519 key, sends its
  public half to the host sealed under a key derived from that enrollment's
  shared secret, and keeps the private seed in its pairing file (0600, next to
  the token, field `device_key`) only after the host's authenticated
  acknowledgement. The host stores the public key with the device record
  (`public_key`). A key, once registered, is never replaced.
- **Relayed sessions.** The client's confirmation carries a signature over the
  transcript hash (both ephemeral keys, credential identifier, request, host
  certificate), sealed so the relay cannot link it to a device. The host hands
  out session keys only after the signature verifies against the stored
  public key, and leases a keyed device only for that verified key. A party
  holding only the stored hash gets the host's (non-secret) reply and nothing
  else; its own key, a signature from another session, or a tampered
  signature are rejected.
- **Compatibility.** Old pairing files (no `device_key`) and old
  `access.json` records (no `public_key`) load unchanged and stay
  hash-authenticated. Old hosts ignore the extra confirmation object; old
  clients close the connection after the token, which leaves a hash-only
  device. Direct connections are unchanged: they present the token itself.
- **`--relay-require-device-key`** (host): relayed hellos for credentials the
  host's records alone would authenticate (the host pairing file's token and
  devices without a key) get no reply. System-login tickets stay allowed: they
  live only in memory, last 60 s and are single-use. Re-enroll older devices
  (password login or a pairing code) before enabling it.

## What the relay can still see

IP addresses of both ends, the per-host namespace (derived from, but not equal
to, the host fingerprint), track names, object sizes and timing. Input timing
in particular reveals typing cadence. It can also drop or delay traffic, which
ends or degrades the session but cannot inject content. During enrollment it
additionally sees the pairing rendezvous namespace (derived from the random
rendezvous, never from the code) and the sizes and timing of PAKE or TLS
messages it cannot read.

## Provider relays (Cloudflare)

Teleport offers `moq-lite-05` and IETF MoQ Transport drafts 16 and 14 to a
relay. Provider relays such as Cloudflare's speak the IETF drafts only; that
path is tested locally against `teleport relay --version moq-transport-16`
(and `-14`), including the fallback of pointer motion to the ordered input
track, because IETF sessions carry no datagrams.

To use a provider relay, pass its WebTransport URL and token, e.g.
`--relay "https://<relay host>/?jwt=<token>"` on both ends (no
`--relay-fingerprint`: its public certificate is verified against system
roots). Teleport replaces the URL path with the host's namespace, which the
host logs when it connects (`namespace=`); mint the provider token for that
path. Not yet verified against Cloudflare itself (no account was available
for testing); report what you find.

## Tests

- `cargo test --bin teleport e2e:: bridge:: relay::` — handshake, tamper,
  splice, replay, wrong host/credential, request alteration, and a check that
  bytes read off the relay do not contain the plaintext; device keys: a
  stored-hash-only client, a foreign key, tampered, missing, swapped and
  transplanted signatures are all rejected, legacy devices still work.
- `cargo test --bin teleport access:: protocol:: host_admin::` — key
  registration in code pairing (and an old client that skips it), leases that
  require the proven key, old `access.json` and pairing files loading.
- `cargo test --test relay -- --ignored` — real relay, host and client
  processes: video and input end to end, forged credential ignored, unpinned
  relay refused, and with `--relay-require-device-key` a code-paired device
  streams while its stripped credential and the host token are refused;
  and first pairing purely through the relay (wrong code, single use, then a
  relayed session decoding video).
- `cargo test --bin teleport relay_enroll:: system_login::` — relay stream
  framing (order, gaps, byte limit), wrong code and replayed pairing
  transcripts, Linux login keeping the password inside pinned TLS.
- `cargo test --bin teleport host_admin:: -- --ignored` — password and code
  enrollment through an in-process relay.

## Known limitations

- **Hash-only credentials remain for compatibility.** Devices enrolled before
  device keys (or by older clients) and the host's own pairing token are still
  authenticated through the relay by `SHA-256(token)`, so anyone who obtains
  the host's identity directory can open relayed sessions as them (the host
  pairing file holds that token in clear anyway). Re-enroll such devices and
  run the host with `--relay-require-device-key`; protect the identity
  directory as a secret regardless.
- **The host's records still identify devices.** A party holding
  `access.json` can recognise which device a relayed hello names and get the
  host's reply, but cannot confirm a session for a keyed device. Someone who
  can *write* the identity directory can replace public keys and is out of
  scope (as with any host compromise).
- **A lost acknowledgement leaves a device unusable through the relay** if the
  host stored the key but the client never saw the acknowledgement (the
  connection dropped in between): the client then keeps no key. Re-enroll it;
  direct connections are unaffected.
- Objects within one media group can be reordered by the relay (each is still
  authenticated); the decoder treats it like loss.
- One relayed session at a time, as for direct sessions.

The design and these limits were reviewed adversarially (2026-10-15): no
confidentiality or integrity break was found; replay, intake and group
injection findings were fixed before release.
