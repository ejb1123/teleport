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

The host address is not contacted in relay mode. `--relay` may also come from
`TELEPORT_RELAY_URL` (keeps a provider token out of the process list).

### Choosing the path: `--route`

| `--route` | Behaviour |
|---|---|
| `direct` | Only the host address (default without `--relay`). |
| `relay` | Only the relay (default with `--relay`). |
| `auto` | The host address for at most 2.5 s, then the relay. |

`auto` falls back only when the direct *transport* fails (no answer, refused, or
a different certificate at that address); once a host has answered on the direct
path, its errors are reported, not retried through the relay. Both paths
authenticate the host by the same pinned fingerprint, and the relay path is
always end-to-end encrypted, so neither direction of fallback downgrades
security. Every reconnect (`--reconnect`) chooses again. The chosen path is
logged (`connection path chosen: relay (end-to-end encrypted)`) and shown in the
Stats overlay (F8) as `Path`.

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

## What protects the session

The relay terminates QUIC/TLS, so TLS alone would let it read everything and
would authenticate the relay rather than the host. Relay mode therefore adds
its own layer (`src/e2e.rs`, `src/bridge.rs`):

- **Host authentication is unchanged:** the host signs the handshake transcript
  with the private key of the same TLS certificate the client already pins by
  SHA-256 fingerprint. A relay cannot complete the handshake as the host.
- **The credential is never sent.** The client proves it holds an accepted
  credential with `HMAC(SHA-256(token), ephemeral key)`. Hosts already store
  only `SHA-256(token)` for devices and tickets. A fresh ephemeral key per
  session keeps these identifiers unlinkable for the relay.
- **Session keys** come from an X25519 exchange mixed with the credential hash
  (forward secret; only a holder of an accepted credential can derive them).
- **Every object** — video, audio, input, pointer, clipboard, SSH-agent
  traffic, control updates — is sealed with ChaCha20-Poly1305 under a key
  specific to its direction and track. Objects cannot be moved between tracks,
  groups or directions; input and control tracks reject any replay or
  reordering; media tracks accept reordering but never a repeat.
- **Connection options** (codec, HDR, SSH-agent opt-in) are covered by the
  credential MAC and the host signature; a relay that alters them is not
  recognised by the host and is rejected by the client.
- **Nothing starts before key confirmation.** After verifying the host, the
  client sends a sealed confirmation. Only when it authenticates does the host
  lease the credential (claiming a single-use ticket) and capture the desktop.
  A replayed hello is answered at most once and never gets further.
- **Bounded handshake work:** at most 8 hellos are processed concurrently,
  each read within 2 s and confirmed within 5 s; a client ephemeral key is
  answered at most once.
- **Unknown or revoked credentials get no reply,** so the relay cannot probe
  which credentials exist. Revocation and single-use system-login tickets are
  enforced exactly as for direct sessions.
- **The relay itself is pinned** with `--relay-fingerprint` (self-signed relay)
  or verified against system roots (a provider with a public certificate).

## What the relay can still see

IP addresses of both ends, the per-host namespace (derived from, but not equal
to, the host fingerprint), track names, object sizes and timing. Input timing
in particular reveals typing cadence. It can also drop or delay traffic, which
ends or degrades the session but cannot inject content.

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
  bytes read off the relay do not contain the plaintext.
- `cargo test --test relay -- --ignored` — real relay, host and client
  processes: video and input end to end, forged credential ignored, unpinned
  relay refused; `--route auto` with an unreachable address connects through
  the relay (and directly when the address works); presence online, then
  offline after the host stops.
- `cargo test --bin teleport presence:: profiles:: launcher::` — forged,
  altered, foreign-host, expired and future presence rejected; relay settings
  saved with a private key file, old profiles loading, client arguments
  without secrets.
- `cargo test --test ui -- --ignored relay` (Xvfb) — editing relay settings in
  the launcher with mouse and keyboard; presence dot online, then offline.

## Known limitations

- **The host's access record now authenticates relayed sessions.** Hosts
  store `SHA-256(token)` for devices and tickets; relay mode accepts that hash
  as proof (the token itself is never sent). Anyone who obtains the host's
  `access.json` or identity directory could therefore open relayed sessions as
  those devices. On a direct connection the hash alone is not enough. Protect
  the identity directory as a secret. The planned fix is per-device key pairs
  registered at pairing (the host would store only public keys).
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
