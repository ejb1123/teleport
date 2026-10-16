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
fingerprint stays valid. Allow UDP 4444 to the relay, and UDP 3478 for its
STUN responder (used for [peer-to-peer](#peer-to-peer-through-nats);
`--stun-listen ADDRESS|off` moves or disables it). `--key` (or
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

The host address is not contacted in relay mode. `--relay` may also come from
`TELEPORT_RELAY_URL` (keeps a provider token out of the process list).

### Choosing the path: `--route`

| `--route` | Behaviour |
|---|---|
| `direct` | Only the host address (default without `--relay`). |
| `relay` | Only the relay (default with `--relay`). |
| `auto` | The host address for at most 2.5 s and, at the same time, a [peer-to-peer](#peer-to-peer-through-nats) path signalled through the relay; then the relay. |

`auto` falls back only when the direct *transport* fails (no answer, refused, or
a different certificate at that address); once a host has answered on the direct
or peer-to-peer path, its errors are reported, not retried through the relay.
Every path authenticates the host by the same pinned fingerprint, and the relay
path is always end-to-end encrypted, so no direction of fallback downgrades
security. Every reconnect (`--reconnect`) chooses again. The chosen path is
logged (`connection path chosen: relay (end-to-end encrypted)`, with
`elapsed_ms`) and shown in the Stats overlay (F8) as `Path`: `direct`,
`p2p, server-reflexive candidate` (or `host`/`port-mapped candidate`), or
`relay`.

## Launcher

Select a saved desktop and click **Relay** (next to **Sign in again**):

- **Relay URL** — `moqt://relay.example:4444` or a provider's `https://` URL.
  Clear it and save to remove the relay (and its saved key).
- **Relay fingerprint** — the `RELAY FINGERPRINT` printed by `teleport relay`;
  leave empty for a provider relay with a public certificate.
- **Relay key** — the relay's `--key`. Stored in its own file
  (`relay-key-<random>` beside `profiles.json`, mode 0600, like pairing files);
  `profiles.json` holds only its path. Leave the field empty to keep the saved
  key; **Forget saved key** removes it.
- **Connect** — *Direct, then relay* (`auto`, default), *Direct only*, *Relay only*.

Tab moves between fields and Enter saves. Connect passes these settings to the
client: the URL and key through the client's environment (never its command
line), the fingerprint and route as arguments. Signing in again keeps the relay
settings. The status line says when a session went through the relay.

Older `profiles.json` files load unchanged; the `relay` entry is optional:

```json
{ "name": "Home", "address": "home.lan:4443", "pairing_file": "…", "fingerprint": "…",
  "relay": { "url": "moqt://relay.example:4444", "fingerprint": "…",
             "key_file": "…/relay-key-0123456789abcdef", "route": "auto" } }
```

Pairing can save relay settings for a desktop with `profiles::set_relay`.

## Presence (online / offline)

A host connected to a relay publishes a `presence` broadcast in its namespace:
every 30 s one ~1 KB statement with its certificate, the namespace, a wall-clock
timestamp and a sequence number, signed with the host identity key (the key of
the certificate clients pin; its own signature label, distinct from the
handshake's). The launcher follows each desktop that has a relay (and is not set
to *Direct only*) in a background thread, one relay connection per desktop, and
shows a dot in the desktop list and a line under the desktop name:

- **Online** — a statement that verifies against the pinned fingerprint and is
  at most 75 s old (and newer than the last one accepted).
- **Offline** — the relay is reachable but no valid statement arrives within
  5 s, the host leaves the relay, or the last statement expires.
- **Unknown** (outline) — the relay is unreachable or not checked yet.

The relay therefore cannot report a host online that is not: it cannot sign
statements, and a replayed one expires after 75 s. It can hide a host (show it
offline) by dropping statements. Freshness assumes host and client clocks agree
within 30 s.

From a terminal: `teleport presence <saved address>` or
`teleport presence --relay <url> --relay-fingerprint <pin> --relay-key <key>
--host-fingerprint <host pin>` prints `ONLINE`, `OFFLINE` or `UNKNOWN`.

First-time pairing works through the relay too (`--pair` prints a relay
pairing link; `teleport pair ADDRESS --relay ...` reads it), so the host never
needs a reachable port. See [enrollment through the relay](#enrollment-through-the-relay).

## Enrollment through the relay

All three ways to obtain a credential run through the relay, unchanged, as the
same byte streams they use over TCP (`src/relay_enroll.rs`):

| Enrollment | Relay namespace | Client command | End-to-end protection |
| --- | --- | --- | --- |
| One-time code (`TPPAIR02`, SPAKE2) | rendezvous from the pairing link | `teleport pair ADDRESS --relay ...` (link on stdin) | PAKE: the code never leaves either end |
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

- **Enrollment.** The key is part of issuing the credential. The client
  announces the capability inside the authenticated transcript (`device_key`
  in the OPAQUE hello, which is in the OPAQUE context; the `TPPAIR02` magic,
  which is in the SPAKE2 transcript), so a relay can neither strip nor add it.
  Right after proving the password or code, and before anything is issued, it
  generates an Ed25519 key and sends the public half sealed under a key
  derived from that enrollment's shared secret and transcript. The host
  writes the device record with `public_key` in the same update as its token
  hash, then sends the credential sealed together with that public key. The
  client saves the pairing (seed in field `device_key`, 0600, next to the
  token) only if that answer names its own key. Any failure of these steps
  fails the enrollment: no saved pairing, and no hash-only record on the host
  (it never creates one for a capable client, and removes the device again if
  it cannot send the credential). A missing or late key is never read as
  "old client".
  Message order: password — magic, hello, OPAQUE request, *response*, OPAQUE
  finalization, sealed key, [*U2F challenge*, assertion], *sealed credential
  with key*; code — `TPPAIR02`, SPAKE2 share, *share*, confirmation, sealed
  key, *sealed credential with key* (host messages in italics). The code is
  consumed only after the key arrived.
- **Relayed sessions.** The client's confirmation carries a signature over the
  transcript hash (both ephemeral keys, credential identifier, request, host
  certificate), sealed so the relay cannot link it to a device. The host hands
  out session keys only after the signature verifies against the stored
  public key, and leases a keyed device only for that verified key. A party
  holding only the stored hash gets the host's (non-secret) reply and nothing
  else; its own key, a signature from another session, or a tampered
  signature are rejected. A relayed session stays leased only while the
  device's stored key is still the one it proved (checked every second with
  revocation), so no session outlives a key change.
- **Compatibility.** Old pairing files (no `device_key`) and old
  `access.json` records (no `public_key`) load unchanged and stay
  hash-authenticated. Old hosts ignore the extra confirmation object. Old
  clients (no flag, `TPPAIR01`) still enroll, hash-only. A new client and a
  host from before device keys do not enroll: the old host rejects
  `TPPAIR02`, and answers a password login with its old credential format,
  which the client refuses ("update Teleport on the host"; revoke the device
  that host just listed). A relay cannot cause this against a current host,
  whose answer to a flagged transcript is always bound to the key. Pairing
  with a host that serves only its own pairing file (`--pair` without managed
  devices) yields that shared token unkeyed, which the host states inside the
  authenticated answer. Direct connections are unchanged: they present the
  token itself.
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
messages it cannot read. For peer-to-peer signalling it sees that an exchange
took place, but not the addresses exchanged; as the default STUN server it
sees the public address of each end's peer-to-peer socket (see
[threat model](#threat-model-and-privacy)).

## Peer-to-peer through NATs

With a relay configured and `--route auto` (the launcher's default), the
client also tries to reach the host *directly* when neither has a port
forward: ICE-lite style hole punching, with the relay used only to exchange
addresses (`src/p2p.rs`, `src/stun.rs`). A direct path avoids the relay
detour (in the namespace test, 23 ms instead of 86 ms input round trip; see
[measurements](#measurements)).

### Flow

1. **One socket per end.** The host opens a peer-to-peer QUIC socket
   (`--p2p-listen`, default `[::]:0`, beside its usual `--listen`); the client
   opens one per attempt. A custom quinn `AsyncUdpSocket` wraps each: datagrams
   that carry the STUN magic cookie (and so lack QUIC's fixed bit) go to a
   small STUN agent, everything else to quinn. STUN and punches therefore
   leave from the very socket, and NAT mapping, that QUIC uses. (moq-native
   owns the main listener's socket and does not expose it, so the host's
   peer-to-peer socket is a second quinn endpoint with the same TLS identity
   and transport settings.)
2. **Candidates.** Each end gathers: `host` — its interface addresses with
   that socket's port (loopback, link-local and container/VM bridges left
   out); `srflx` — its public address as a STUN server saw it; `mapped` — with
   `--upnp`, a NAT-PMP mapping from the default gateway (Linux; deleted on
   exit; UPnP-IGD and PCP are not implemented). The STUN server defaults to the
   self-hosted relay's responder (`moqt://` relay host, UDP 3478). Nothing is
   sent to a public STUN server unless named with `--stun HOST:PORT`
   (repeatable); `--no-stun` sends none. Provider (`https://`) relays get no
   STUN traffic.
3. **Signalling** is an end-to-end handshake of its own through the relay, at
   `<attempt>/p2p-hello`, `p2p-reply`, `p2p-confirm`, `p2p-offer`,
   `p2p-answer`: the same X25519 exchange, host transcript signature,
   key confirmation and device-key signature as a session, with the same
   host-side bounds (8 pending, timeouts, replay cache, unknown credentials
   unanswered, `--relay-require-device-key`). It is domain-separated from
   sessions: the credential identifier and keys use
   `HMAC(SHA-256(token), "teleport-p2p-signal-v1")` instead of `SHA-256(token)`,
   and the hello's (signed) request is `teleport-p2p-v1`. A relay can
   therefore neither turn a signalling exchange into a relayed session (or
   the reverse) nor get an older host to answer it. After the host has
   verified the confirmation, the client sends its candidates and the host
   answers with its own, each sealed (ChaCha20-Poly1305) under keys derived
   from that handshake's transcript and specific to direction and track.
   Signalling never leases the credential or captures anything.
4. **Punching.** On a valid offer the host *arms* its peer-to-peer socket for
   30 s, sends STUN Binding indications from it to each client candidate
   (11 rounds over 3 s), then answers. Its NAT now expects the client. The
   first six rounds (400 ms) carry an IP TTL of 2, set per datagram: they
   cross the host's own router but expire before the client's NAT. A
   conntrack NAT without an inbound firewall would otherwise record the
   unsolicited punch and remap the client's port when the client's own
   packets to the host go out. Later rounds use the normal TTL, for a host
   NAT further than one hop away.
5. **Connectivity.** The client dials every host candidate from its own
   socket (host candidates first, 30 ms apart) and keeps the first QUIC
   handshake whose certificate has the pinned SHA-256 fingerprint; the others
   are dropped before any credential is sent. The MoQ session on top is an
   ordinary direct session: the credential travels in the SETUP path inside
   that pinned TLS connection, and the host authorizes, leases and revokes it
   exactly as on `--listen`. Hole punching changes reachability, never
   authentication.
6. **Budget.** Signalling (relay connection, STUN, handshake, answer) must
   finish within 2.5 s and a handshake within 1.5 s more. All of it runs
   alongside the 2.5 s direct attempt; if both fail, the relay is used as
   before.

### Compatibility

Peer-to-peer happens only when both ends support and allow it:

- A new client asks only through the relay. An older host (or one with
  `--no-p2p`) cannot recognise the domain-separated hello and stays silent;
  the client waits at most 2.5 s, during the direct attempt it makes anyway,
  then uses the relay. Measured: 3.43–3.46 s to connect, as before.
- An older client never sends a signalling hello; a new host behaves as
  before for it. `teleport client --no-p2p` restores the old behaviour.
- `--route direct` and `--route relay` are unchanged.

### Threat model and privacy

- **Authentication is unchanged.** The host is authenticated by its pinned
  certificate in the peer-to-peer TLS handshake, as on a LAN; the client by
  its credential, checked by the same code as `--listen`. A forged candidate
  can only point the client at a server that fails the pin, before the
  credential is sent.
- **The relay** sees that a signalling exchange happens (path names, sizes,
  timing) but not the candidates, which are sealed; it cannot inject, alter,
  replay or move them between sessions (AEAD keyed by the transcript,
  strict counters). It already knows both ends' public IP addresses from its
  own connections. It can block signalling, which only means the relay is
  used.
- **The STUN server** (by default the relay itself) learns the public
  address and port of each end's peer-to-peer socket, as it would for any
  STUN client. Naming a public server with `--stun` discloses that to its
  operator.
- **The other end** learns your addresses: the host gets the client's
  interface and public addresses, and the client the host's, but only after
  completing the authenticated handshake (including its device-key signature
  where the host stored one) — an unauthorized relay client learns nothing.
  Over the relay neither end sees the other's address; with peer-to-peer
  they do, as with a direct connection. Use `--no-p2p` (either end) or
  `--route relay` to keep addresses private from the peer.
- **Exposure.** The host's peer-to-peer socket listens on all interfaces by
  default but ignores QUIC Initials unless an authenticated signalling
  exchange armed it in the last 30 s, and then admits only what `--listen`
  would. Punches go only to candidates of an authenticated client (at most
  88 datagrams of 20 bytes per exchange).
- **The relay's STUN responder** answers Binding requests only (no
  attributes echoed), at most 2000 per second, so its use as a reflection
  amplifier is small (a 20-byte request gets a 32- or 44-byte answer).

### Measurements

`tests/p2p.rs`, debug build, two Linux MASQUERADE NATs with 5 ms per access
link and the relay 15 ms further away, so ~20 ms RTT between the peers and
~80 ms through the relay; input probe round trips while the pointer streams
at 500 Hz. Two serial runs of every scenario (2026-10-16, load average
5.4–8.4):

| Scenario | Path | Connect (s) | Probe p50 / p95 (ms) |
| --- | --- | --- | --- |
| Cone NATs with router firewall | p2p, server-reflexive | 1.33–1.36 | 23.2–24.0 / 24.1–27.2 |
| Cone NATs without inbound firewall | p2p, server-reflexive | 1.32–1.35 | 23.2–24.0 / 25.2–28.6 |
| Client NAT symmetric (`--random-fully`) | relay | 3.57–3.64 | 84.2–87.5 / 86.2–94.3 |
| Peer UDP blocked | relay | 3.60–3.63 | 87.4–87.5 / 94.5–95.6 |
| Host `--no-p2p` (as an older host) | relay | 3.42–3.45 | 84.1–87.3 / 86.4–93.6 |
| Client `--no-p2p` (previous behaviour) | relay | 3.43–3.47 | 85.1–87.2 / 88.3–92.9 |

The direct address is unreachable in every scenario, so the previous
behaviour spends the full 2.5 s direct attempt before the relay. Peer-to-peer
connects in about 1.3 s (about 1.15 s of it relay signalling, since each
relay message costs an announce and subscribe round trip; the QUIC handshake
itself takes ~60 ms) and removes the relay detour from every round trip:
about 24 ms instead of 86 ms here. When punching fails after successful
signalling, connecting takes 0.1–0.2 s longer than before. Before the
low-TTL first punches, the run without an inbound firewall fell back to the
relay.

### Limitations

- **Symmetric NATs** (a new public port per destination, as with
  `--random-fully` or many carrier-grade NATs) defeat this scheme: the client
  falls back to the relay, which is the only TURN-like path. Port prediction
  and birthday-style probing are not implemented.
- Relays without the STUN responder (provider relays, or `--stun-listen off`)
  leave only host candidates unless `--stun` names a server: peer-to-peer then
  works on a LAN or with `--upnp`, not across two NATs.
- `--upnp` speaks NAT-PMP only, and finds the gateway on Linux only.
- A session does not migrate between paths: one that started on the relay
  stays there until it reconnects.
- The host's peer-to-peer socket listens on a random port each start (fix it
  with `--p2p-listen` for firewall rules).
- The low-TTL punches assume the host's NAT is its first hop. Behind a
  second NAT (carrier-grade, or a VM behind its host) the outer NAT opens only
  with the later full-TTL punches, so the client's first QUIC Initial may be
  lost and the handshake waits for its retransmission (about 1 s, within the
  1.5 s budget); if that outer NAT also has no inbound firewall, those
  punches can arrive first and spoil the client's mapping.

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
- `cargo test --bin teleport access:: pairing:: protocol:: host_admin::
  relay_enroll::` — keyed enrollment through a meddling relay (device key
  dropped, delayed or cut, credential undeliverable or dropped, capability
  flag stripped or added): never a hash-only record for a capable client,
  nothing saved on failure, the record removed when undeliverable, the code
  kept when no key arrived; old clients still enroll hash-only; leases that
  require the proven key and end when it changes; old `access.json` and
  pairing files loading.
- `cargo test --test relay -- --ignored` — real relay, host and client
  processes: video and input end to end, forged credential ignored, unpinned
  relay refused, and with `--relay-require-device-key` a code-paired device
  streams while its stripped credential and the host token are refused;
  and first pairing purely through the relay (wrong code, single use, then a
  relayed session decoding video); `--route auto` with an unreachable address
  connects through the relay after a peer-to-peer exchange that offers no
  usable (loopback) candidate, and directly when the address works; presence
  online, then offline after the host stops.
- `cargo test --bin teleport stun:: p2p:: natpmp::` — STUN encoding and
  decoding (RFC 5769 vectors, malformed and foreign messages, the relay's
  responder); candidate validation (unspecified, loopback, multicast,
  broadcast, reserved, link-local, port 0, too many); candidates sealed to
  their session, track and direction (another session, tampering, an
  unsealed list from the relay rejected); the domain-separated signalling
  credential; STUN on the QUIC socket (spoofed answers ignored), a punch and
  a pinned QUIC handshake on the same socket (wrong pin refused), per-datagram
  TTL; NAT-PMP against a fake gateway.
- `cargo test --test p2p -- --ignored` — unprivileged namespaces with two
  NATs and a distant relay (see [measurements](#measurements)): hole punching
  with and without router firewalls (relay used only for signalling, host sees
  the client's NAT address), and the relay fallback for a symmetric client
  NAT, blocked peer UDP, a host with `--no-p2p` and a client with `--no-p2p`.
- `cargo test --bin teleport relay_enroll:: system_login::` — relay stream
  framing (order, gaps, byte limit), wrong code and replayed pairing
  transcripts, Linux login keeping the password inside pinned TLS.
- `cargo test --bin teleport host_admin:: -- --ignored` — password and code
  enrollment through an in-process relay.
- `cargo test --bin teleport presence:: profiles:: launcher::` — forged,
  altered, foreign-host, expired and future presence rejected; relay settings
  saved with a private key file, old profiles loading, client arguments
  without secrets.
- `cargo test --test ui -- --ignored relay` (Xvfb) — editing relay settings in
  the launcher with mouse and keyboard; presence dot online, then offline.

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
- **A lost final message leaves an unused device record.** If the credential
  reaches the host's side of the connection but never the client, the client
  saves nothing and the host keeps a keyed record nobody holds the key for
  (useless with `access.json` alone). Enroll again (a new pairing code, or
  log in again) and revoke the stale entry in the device list.
- Objects within one media group can be reordered by the relay (each is still
  authenticated); the decoder treats it like loss.
- One relayed session at a time, as for direct sessions.
- Presence: anyone admitted to the relay namespace (relay key holders) can
  publish their own `presence` broadcast and make the host appear offline; they
  cannot make it appear online. A relay can replay the host's last statement
  for up to 75 s after it left.
- `auto` with a single-use Linux-login ticket: if the direct attempt times out
  just as the host accepts it, the ticket may be spent and the relay attempt
  rejected; sign in again.
- The launcher watches presence with one relay connection per desktop
  (keep-alive every 3 s); presence is not shown for *Direct only* desktops.

The design and these limits were reviewed adversarially (2026-10-15): no
confidentiality or integrity break was found; replay, intake and group
injection findings were fixed before release.
