# Session tickets (issuer-signed access)

A **session ticket** lets a service you run decide who may open a session on
a host, instead of a person pairing each device. The host trusts the
service's Ed25519 public key. The service signs a short-lived ticket naming
the host, the person, what they may do and the key of the client they will
use. The host accepts each ticket once, for one session.

This is for fleets: a support desk that opens sessions on devices it
manages, or a web site whose users reach their own machines. Pairing codes,
passwords and Linux login are unchanged and work alongside tickets.

Status: development (2026-10). Covered by unit tests and a relay integration
test (`issuer_signed_ticket_opens_one_relayed_session` in `tests/relay.rs`).

## Host

```sh
teleport host --identity-dir /run/teleport/host \
  --ticket-issuer-key <64 hex digits> \
  --ticket-approval issuer \
  --relay moqt://relay.example:4444 --relay-fingerprint <pin> --relay-key <key>
```

- `--ticket-issuer-key` (repeatable) is the issuer's raw Ed25519 public key
  in hex. It needs `--identity-dir`.
- `--ticket-approval local` (the default) treats a ticket session like any
  guest: someone at the host approves it (host settings, `host-admin
  approve`, or an owner's client). `issuer` admits it at once, because the
  issuer already obtained consent, and keeps it when no owner is connected.
  Use `issuer` only when the issuer really does ask the person who owns the
  machine.
- Ticket sessions are always **guests**. They get exactly the ticket's
  permissions; an empty list is view only.

## Ticket format

```
tkt1.<payload>.<signature>
```

Both parts are base64url without padding. The signature is Ed25519 over the
ASCII bytes `teleport-ticket-v1.` followed by the payload part exactly as it
appears in the ticket, so issuers need no JSON canonicalisation. The payload
is a JSON object; unknown fields are refused.

| Field | Meaning |
|---|---|
| `v` | `1` |
| `iss` | Issuer label, 1–64 printable characters (logged). |
| `aud` | The host certificate's SHA-256 fingerprint, 64 lowercase hex digits. A ticket opens only that host. |
| `sub` | Who the session is for, 1–64 characters; the host shows it as the session's name. |
| `sid` | The issuer's session id, 1–64 of `A-Z a-z 0-9 - _` (logged, for the issuer's audit). |
| `perms` | Any of `keyboard_mouse`, `gamepad`, `clipboard`, `microphone`; empty is view only. |
| `cnf` | The client's Ed25519 public key, base64url (32 bytes). |
| `nbf`, `exp` | Unix seconds. At most 600 seconds apart; 30 seconds of clock difference are tolerated. |
| `jti` | Unique ticket id, 1–64 of `A-Z a-z 0-9 - _`. The host accepts it once. |
| `max_session_s` | The session ends this many seconds after it starts (1–86400). |

`src/ticket.rs` has a known-answer vector produced by an independent
implementation; check yours against it.

## Redemption

The client redeems a ticket on the host's enrollment channel, directly over
TCP or through the relay (`TPTKT001`):

1. TLS 1.3 pinned to the `aud` fingerprint, inside the relay stream when
   relayed, so the relay sees only ciphertext.
2. The client sends the ticket and an Ed25519 signature, made with the `cnf`
   key, over that TLS connection's exporter. This proves it holds the key,
   and the signature is useless on any other connection.
3. The host checks the issuer signature, audience, lifetime, the key proof
   and that the `jti` is new. It then returns a one-session credential that
   lives only in memory and is bound to the `cnf` key: a relayed session must
   also sign its handshake with that key. The credential lapses if no
   session starts within 60 seconds.

The client learns only "refused" when a ticket is not accepted; the host log
says why.

## Client and tools

```sh
teleport ticket issuer-keygen issuer.key    # prints the public key for hosts
teleport ticket client-keygen client.key    # prints the client key for `cnf`
teleport ticket sign --issuer-key-file issuer.key \
  --host-fingerprint <pin> --client-key <cnf> --name "Support (Ana)" > t.ticket
teleport login-ticket HOST:4443 --fingerprint <pin> \
  --ticket-file t.ticket --key-file client.key [--relay ...]
```

`login-ticket` redeems the ticket and starts the client with the resulting
credential. A real issuer is a service, not these commands; they exist for
small setups and for testing an issuer.

## What a ticket session can and cannot do

- It ends when the session ends (the credential is single use), when
  `max_session_s` passes (checked every second, like revocation), or when
  `host-admin revoke-all` runs.
- It is never saved as a device and never appears in the device list.
- A stolen ticket without the client key cannot be redeemed, and a stolen
  one-session credential cannot be used through a relay without the key.
- The issuer is trusted completely for the permissions it grants. Whoever
  holds the issuer's private key can open sessions on every host that trusts
  it, within those hosts' `--ticket-approval` policy. Keep that key in a
  service, not on a laptop.

## Links from a web page

An issuer's web site can open a session in the native client with a link:

```
teleport://redeem?url=<percent-encoded https URL>
```

The URL is single use and short-lived; it is the only secret in the link.
`teleport open LINK` (and the `x-scheme-handler/teleport` desktop entry the
Linux packages install, so a browser can launch it):

1. makes a fresh client key, which never leaves the process;
2. POSTs `{"client_key": "<base64url public key>"}` to the URL, with no
   cookies or other credentials;
3. expects `200` with
   `{"ticket", "name", "host_fingerprint", "relay": {"url", "fingerprint", "key"}}`
   (`fingerprint` and `key` may be empty), or an error status with
   `{"error": "<message>"}`, which it shows;
4. redeems the ticket through that relay and starts the client, relay only.

The server must spend the URL on the first successful POST and bind the
ticket's `cnf` to the posted key. Plain `http` URLs are accepted only on
loopback. macOS does not register the scheme yet: run `teleport open` with
the link there.

## Tickets in the handshake

A client can skip the redemption step and open a relayed session with the
ticket directly. A browser has no TLS stack to run inside a relay stream, so
this is how browser clients connect. Native clients do it with
`teleport client --ticket-file T --key-file K --relay ...`, which is relay
only.

1. The relayed hello is marked `ticket` and carries no credential. Its
   identifier is keyed with a public constant, so until step 3 the session
   keys rest only on the X25519 exchange. The host is authenticated by its
   transcript signature, as always.
2. The host answers any ticket hello if it trusts at least one issuer.
3. The client's key confirmation carries, sealed under the session keys:
   - the session id;
   - the ticket;
   - an Ed25519 signature over the transcript with the ticket's `cnf` key.

   The host verifies the ticket exactly as in redemption: issuer, audience,
   lifetime, a `jti` it has not seen, and the `cnf` signature. It then leases
   the session at once. Nothing is released before that succeeds.

**What the relay learns.** The relay never sees the ticket, so it cannot
learn who it is for (`sub`) or the permissions. It sees only that a ticket
hello took place.

**Refusals.** A ticket for another host, a replayed ticket, or a ticket
signed for another key gets no session. The client then times out waiting,
and the host log says why.
