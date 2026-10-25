# Self-hosted remote support for TRaX boxes

**Status:** Accepted. EJ decided D1 to D9 on 2026-10-25 (see
[decisions](#11-decisions-ej-2026-10-25)). Nothing is built yet.
**Date:** 2026-10-25.
**Asked for by:** EJ, 2026-10-25: "look into getting the teleport app working
... so we can use this as a self hosted deployed logmein for our own embedded
devices for customers to use through our site and for remote support", then
"design and shit needs to be worked on first".
**Written against:** this repository at `fd6d0d5`, and the TRaX monorepo
(`tprgroup/trax-monorepo`) at `origin/main` `1c5d8b182`. The TRaX side of this
design is ADR-011 (`infra/box/docs/decisions/ADR-011-platform-integration.md`),
decision D11, "owner-consented, time-boxed remote session over the stream".
This document proposes Teleport as that session.

---

## 0. Summary

Teleport already has most of what a LogMeIn-style product needs: an end-to-end
encrypted relay that sees no content, NAT hole punching, signed presence,
per-device keys, guests with per-guest permissions and live approval, and
revocation that ends a running session within a second. What it lacks for
this use is everything that makes it *managed*: it trusts pairing codes and
passwords typed by a person, it keeps its host key in a file, it captures only
through a desktop portal or X11, and its only client is a native app for Linux
and macOS.

The TRaX box is a QtWayland kiosk compositor on NixOS with a TPM-resident
device key and an outbound mTLS management stream to `encoder-service`. It has
no desktop portal, no inbound ports, and must never join Nebula (ADR-011 §2.1).

The design in five lines:

1. **Nothing listens on the box.** The account or admin site asks for a
   session; `encoder-service` sends a directive down the box's existing
   management stream; `trax-box-ctl` starts a time-boxed Teleport host that
   dials *out* to a TRaX-run Teleport relay.
2. **The relay is a separate service, `teleport relay`, run by TRaX** beside
   the PoPs. Not mediamtx, not TRT, not Nebula. It is end-to-end encrypted, so
   it is not in the trust base for content; it is admitted by short-lived
   platform tokens instead of a shared key.
3. **Identity is the box's TPM device key.** The Teleport host key is
   generated per session in RAM and certified by a signature from the
   TPM-resident device key; the cloud checks it against the key pinned at
   claim time and hands the fingerprint to the client. No new long-lived secret.
4. **Access is a platform-signed grant, not a pairing.** studio-api mints a
   one-use, minutes-long grant naming the box, the user, the role (owner or
   support), the permissions (view or control) and the session id. The box
   asks the person at its screen to approve, unless the owner pre-approved a
   support window, and shows a banner for the whole session.
5. **Native client first, browser second.** Support staff open a session from
   the admin site with a `teleport://` deep link. Customers need a browser
   client (WebTransport + WebCodecs + the E2E layer in WASM), because Teleport
   has no Windows client.

---

## 1. Goals and non-goals

### Goals

- **G1 Remote support.** A TRaX support agent views, and with permission
  controls, a customer's box from the admin site, without the customer
  forwarding a port, installing anything or reading out a code.
- **G2 Customer remote access.** A customer opens their own box from the
  account site, from anywhere, as they would with LogMeIn.
- **G3 Consent the customer controls.** The customer owns the device. Support
  access needs either approval at the box at the time, or an explicit,
  time-limited support grant the owner made in the account site. The owner can
  end any session.
- **G4 Audit.** Every session leaves a record the owner can read: who, when,
  which role, view or control, how it ended. Optional recording.
- **G5 Reuse.** Box identity, claim, ownership and the management stream are
  already designed (ADR-011) and partly built (#519, #1046). This design adds
  nothing that duplicates them.

### Non-goals

- **Not a general remote shell.** ADR-011 D11 also lists a restricted shell or
  a `TraxBoxControl` console. That is a separate feature with its own review.
  This design is screen, keyboard and mouse only.
- **Not headless encoders.** Cloud encoders and BYO private encoders have no
  display. Their support path stays logs on demand.
- **Not arbitrary customer PCs.** Teleport hosts on customers' own desktops,
  outside a TRaX box, are out of scope. The grant model in §6 would work for
  them later, but enrollment of a non-TRaX device is a different problem.
- **No file transfer, clipboard or peripheral forwarding in v1.** Teleport has
  clipboard and SSH-agent forwarding; both stay off for platform sessions.
- **Not a replacement for Teleport's own pairing.** The standalone product
  keeps working exactly as today. The managed mode is additive.

---

## 2. What Teleport is today

From the README, `docs/status.md`, `docs/relay.md`, `docs/access.md` and
`docs/guests.md` at `fd6d0d5`:

| Area | Today | Matters here because |
|---|---|---|
| Host | Linux only. Capture through the xdg-desktop-portal RemoteDesktop/ScreenCast portals with PipeWire (`--source portal`), X11 (`x11`), or a test pattern (`test`). Daily use on KDE Plasma 6 + NVIDIA; Intel UHD 630 tested. | The box has neither a portal nor X11 (§5.1). |
| Encode | Directly linked FFmpeg: NVENC, VA-API, QSV, software libx264/libx265. | The box is Intel Gen9 (HD 530) on VA-API. libx264/libx265 make a distributed binary GPL (§11, D1). |
| Transport | moq-lite-05 over raw QUIC (`moq-native` 0.19.17, Quinn). Direct, `teleport relay`, Cloudflare's MoQ relay. P2P hole punching with the relay as STUN (UDP 3478) and signalling channel. | Relay and NAT traversal already exist and are end-to-end encrypted. |
| E2E | Host signs the transcript with its pinned certificate key; X25519 session keys; ChaCha20-Poly1305 per direction and track; no work before key confirmation; unknown credentials get no reply. | The relay can be untrusted for content. |
| Enrollment | One-time code (SPAKE2), password (OPAQUE), Linux account (PAM), each with an Ed25519 device key. Works through the relay. | All assume a person on each end typing a secret. |
| Sessions | Owners and guests; per-guest permissions (keyboard/mouse, gamepad, clipboard); live approval with a 30 s timeout; revocation checked every second; `host-admin` socket. | Maps directly onto "support needs approval" and "view only". |
| Presence | Signed statement every 30 s through the relay. | Useful, but the management stream already reports box online state. |
| Clients | Native SDL app, Linux and macOS. **No Windows, no browser.** | Most customers cannot run the client (§7). |
| Packaging | NixOS module `services.teleport-desktop`, .deb, .rpm, Arch, AppImage, Flatpak (client), macOS dmg. | The box consumes the flake directly. |
| License | **None.** No `LICENSE` file; the GitHub metadata says `license: null`; the README says all rights reserved. | TRaX cannot ship it in a product until this is settled (§11, D1). |

Open issues that touch this design: **#6** (`hevc_qsv` "Invalid FrameType:0"
forcing an encoder reopen 11 s into a session) is on Intel, the box's GPU
vendor, though the box would use VA-API; **#5** (moq_net "Producer dropped
without finish()" warnings) is noise in the logs the support audit would
collect.

---

## 3. Architecture

```
 account site / admin site                         TRaX cloud
 ┌───────────────────────┐   POST /remote-sessions  ┌──────────────────────────┐
 │ "View this box"       │ ───────────────────────► │ studio-api               │
 │  → teleport:// link   │ ◄── grant + relay URL ── │  owns grants, consent,   │
 │  or browser client    │      + host fingerprint  │  audit (encoders, claims)│
 └──────────┬────────────┘                          └────────────┬─────────────┘
            │                                                    │ directive
            │ QUIC / WebTransport (E2E sealed)                   ▼
            │                                       ┌──────────────────────────┐
            ▼                                       │ encoder-service :50060   │
   ┌──────────────────┐                             │ box management stream    │
   │ teleport relay   │                             └────────────┬─────────────┘
   │ (TRaX-run, per   │                                          │ ControlCall
   │  metro, beside   │      outbound QUIC (E2E sealed)          ▼ (outbound mTLS)
   │  the PoPs)       │ ◄──────────────────────────  ┌──────────────────────────┐
   └──────────────────┘   or P2P after hole punch    │ trax-box                 │
                                                     │  trax-box-ctl ─ starts ─►│
                                                     │  teleport host (transient│
                                                     │  unit, RAM-only key)     │
                                                     │  QtWayland compositor    │
                                                     │  (PipeWire node, uinput) │
                                                     └──────────────────────────┘
```

### 3.1 Decision A1: where the relay runs

| Option | Good | Bad |
|---|---|---|
| **A. `teleport relay` as its own service, run by TRaX, one per metro beside the PoPs, GeoDNS `remote.<metro>.traxstreaming.live` (recommended)** | Already built and E2E-safe. Separate process, so a remote-support bug cannot touch the live media plane. Scales with sessions, which are few. P2P takes most traffic off it. | One more service to deploy, monitor and patch. Needs a token admission mode (§9.2) before it faces the internet. |
| B. Reuse PoP mediamtx (MoQ on 8892) | No new process. | mediamtx's MoQ is not moq-lite-05 and was never exercised end to end (ADR-011 §1.4). Puts support traffic in the prod media path. |
| C. Reuse the TRT gateway | Ours, QUIC, MOQT-based, bonded uplinks. | TRT is a media-contribution gateway on one canary PoP, speaking a different MoQ dialect; making it a general relay is a TRT project. |
| D. Nebula | Already reaches every platform node. | ADR-011 §2.1 and D11: a customer box never joins Nebula. Rejected. |
| E. Cloudflare's MoQ relay | Zero operations; works today. | "Self-hosted" is the point of the request; a third party sees IPs, sizes and timing of every support session. Keep it only as a dev convenience. |

**First demo:** a single relay instance on one PoP host or one small VM.
Per-metro comes later; nothing in the protocol cares how many there are.

### 3.2 NAT traversal

Unchanged from Teleport: try P2P (relay STUN + E2E-signalled candidates +
simultaneous open), fall back to the relay. Boxes sit on venue networks that
often block UDP entirely; there, P2P fails and so does the relay, because
Teleport is QUIC-only. ADR-011 already notes the same for TRT. A TCP or
WebSocket fallback is **not** in v1. Measure first: the box's management
stream is TCP gRPC, so a box that is online but cannot reach the relay is
detectable and should be reported as such in the admin UI, not as "offline".

### 3.3 Rendezvous: who starts the session

The Teleport host on the box does **not** run all the time. It starts on a
directive and exits when the session ends or its time box expires:

1. The user clicks **View** (or **Support session**) on a box.
2. studio-api checks ownership or support role, records the request, mints a
   grant (§6.2) and a relay admission token, and sends `StartRemoteSession`
   to encoder-service for that box.
3. encoder-service forwards it on the box's management stream as a
   `ControlCall` (ADR-011 D4's generic tunnel), method on the allowlist.
4. `trax-box-ctl` asks for consent on screen if the grant needs it (§6.3),
   then starts the Teleport host with the relay URL, its own namespace, the
   platform issuer key and the session limits.
5. The host generates its key, ctl gets it certified (§5.4), and the
   certified fingerprint goes back up the stream to studio-api.
6. studio-api returns the relay URL, host fingerprint and grant to the
   browser, which opens the native client by deep link or starts the browser
   client.

Why on demand: no idle relay connection per box, no presence traffic, no
capture pipeline running on a box that is producing a show, and the attack
surface exists only while a session is open.

---

## 4. Device side

### 4.1 Decision A2: capture on a kiosk compositor

The box's UI is a QtWayland `WaylandCompositor` QML app on `eglfs`/KMS
(`infra/box/docs/architecture.md` §2). It is the only compositor. There is no
xdg-desktop-portal and no X server, so neither of Teleport's real sources
works.

| Option | Good | Bad |
|---|---|---|
| **A. The compositor publishes its output as a PipeWire video node; Teleport gets a `--source pipewire-node:<name>` that connects to it directly, no portal (recommended)** | Teleport's capture is already PipeWire; this only skips the portal handshake. DMA-BUF capable, so zero-copy into VA-API stays possible. The compositor decides what is shared (the whole output, or the output minus an operator-private overlay). | Compositor work: render the output into a PipeWire stream (Qt Quick's `QQuickRenderTarget` into a DMA-BUF pool). |
| B. `kmsgrab` (FFmpeg KMS plane capture) | Works with any KMS compositor; no compositor change. | Needs `CAP_SYS_ADMIN`; captures the scanout plane only, misses overlays on other planes; a privileged capture process on a customer device. |
| C. Implement the ScreenCast portal in the compositor | Teleport unchanged. | A portal backend for a single-app kiosk is a lot of D-Bus surface for one client. |
| D. Screenshot loop over a ctl RPC | Trivial. | Low frame rate, CPU copies on Gen9; not a remote desktop. |

### 4.2 Input

| Option | Good | Bad |
|---|---|---|
| **A. `/dev/uinput` virtual keyboard and pointer (recommended)** | Compositor-agnostic: libinput in the compositor sees a normal device. Teleport already uses uinput for gamepads, so the host has the code paths and permissions model. | The compositor must accept absolute pointer events from a uinput tablet device; a test is needed. |
| B. A compositor input socket fed by ctl | Compositor can tag remote input (for example, refuse it on a "factory reset" screen). | New protocol in the compositor; more box-side code. |

Recommendation A for v1, with the compositor told by ctl that a remote
session is active so it can draw the banner and keep its screen-only
controls local (ADR-011 §4.3 already makes some RPCs screen-only; the same
list should be unreachable by remote input, which uinput cannot enforce, so
those screens need a local confirmation the remote pointer cannot give, such
as a physical button or a PIN).

### 4.3 Packaging and supervision

- **Nix.** The box flake (`infra/box/nix`) takes this repository as a flake
  input, pinned by rev, and builds a host-only package (§10, T1): no SDL
  client, no launcher, no macOS code, FFmpeg built without libx264/libx265
  (the box encodes on VA-API; this also keeps the box image free of GPL
  encoder code, D1).
- **Unit.** `trax-box-remote@.service`, a template instance per session,
  started only by ctl over its D-Bus/systemd API. Not `WantedBy` anything.
  `RuntimeMaxSec` equal to the grant's session limit (default 60 min). Its
  own system user, member of the group that owns the compositor's PipeWire
  socket and `/dev/uinput`, nothing else. `ProtectSystem=strict`,
  `PrivateTmp`, `NoNewPrivileges`, no writable paths except `/run/teleport`.
  No `StateDirectory`: nothing survives the session.
- **Control.** ctl talks to the host's `host-admin` socket to approve,
  deny, kick, and read session state, and stops the unit to end everything.
- **OTA.** Teleport ships inside the A/B image (ADR-001), so it updates with
  the box. No separate updater.

### 4.4 Identity on the box

Teleport today generates a self-signed certificate and stores `key.pem` in
`--identity-dir` (`src/identity.rs`). On the box that would be a new
long-lived secret on disk, which ADR-004 and ADR-011 D3 forbid.

| Option | Good | Bad |
|---|---|---|
| **A. Per-session key in RAM, certified by the TPM device key (recommended)** | The host makes an Ed25519/P-256 key in `/run`, ctl signs `{box_id, session_id, host_spki_sha256, not_after}` with the TPM-resident device key (`trax-box-device-v1`), studio-api verifies the signature against `device_key_spki` pinned at claim (`box_claims`, migration 0124) and hands the fingerprint to the client. A copied disk holds nothing; the key dies with the session. | Clients cannot pin a host across sessions; they pin whatever the cloud vouches for each time. That moves trust to the cloud, which is already where the grant comes from. |
| B. Teleport host key in the TPM | Strongest key custody. | Teleport would need a TPM signer (tss-esapi) for TLS and transcript signatures; heavy for a key that lives an hour. |
| C. Reuse the box's 1 h mTLS certificate | Already exists. | That key is the box's platform identity; giving it to a second process widens what a Teleport bug can impersonate. |

---

## 5. Identity and trust summary

| Party | Proves itself with | Long-lived secret added by this design |
|---|---|---|
| Box | TPM device key (existing), signing a per-session Teleport host key | none |
| Teleport host | per-session key, certified as above | none |
| User (customer or support) | Zitadel session in the account or admin site | none |
| Client app | a one-use grant, minutes long, bound to an ephemeral client key | none |
| Relay | public TLS certificate (or a pinned fingerprint shipped in the box image) | relay TLS key, server side only |
| Grant issuer | an Ed25519 key in Vault transit, used only by studio-api; its public half shipped in the box image and rotatable over the stream | issuer key, server side only |

---

## 6. Access control and consent

### 6.1 Roles

| Role | Who | Default permissions |
|---|---|---|
| `owner` | the box's `owner_sub` | view and control |
| `support` | a TRaX staff account with a support role in trax-auth (today the only role is `admin`, ADR-011 §1.4; D4) | view only; control needs a second, explicit customer approval |

### 6.2 The grant

studio-api mints, and the box verifies:

```
{ iss, box_id, session_id, sub, role, perms: ["view"] | ["view","control"],
  client_key_sha256, consent: "at_box" | "preapproved:<grant_id>",
  max_session_s, nbf, exp (≤ 5 min), jti }
```

signed by the issuer key. The client proves possession of `client_key` in
Teleport's existing transcript signature (the device-key path in
`docs/relay.md` "Device keys"), so a leaked grant is useless without the key
that never left the client. The host accepts each `jti` once.

This is a new enrollment kind in Teleport, alongside code, password and
PAM: **issuer-signed tickets**. It maps onto Teleport's existing guest model:
an `owner` grant is a trusted device for one session, a `support` grant is a
guest with the grant's permissions.

### 6.3 Consent

| Situation | What happens |
|---|---|
| Owner opens their own box | No prompt. Banner on the box screen for the whole session. |
| Support, box attended | The box screen shows who is asking (staff name, ticket reference) with **Allow view / Allow control / Deny**. 60 s timeout denies. |
| Support, box unattended | Refused, unless the owner made a **support grant** in the account site: a window (for example, the next 24 h), a permission level, optionally a named staff member. The session still shows the banner. |
| Support wants control mid-session | A second approval, at the box or by the owner in the account site. |
| Owner wants it to stop | **End session** in the account site, or the banner's stop button on the box. Either path ends it within a second. |

### 6.4 Audit, recording, revocation

- **Audit.** ctl writes session start, approval, permission changes, end and
  reason to the box's support log; studio-api writes the same events to a
  `remote_sessions` table keyed by `session_id`. The account site shows the
  owner every session on their boxes. The admin site shows staff their own.
- **Recording (optional, off by default).** Content is end-to-end encrypted,
  so the relay cannot record. If enabled, the box records the encoded stream
  it is already producing to its local disk and uploads it to the owner's
  storage (ADR-011 §11 question 15, recording upload target), never to TRaX storage by
  default. The banner says "recorded".
- **Revocation.** Grants expire in minutes; sessions have a hard
  `RuntimeMaxSec`; ending a session is a directive down the stream that ctl
  turns into a `host-admin` kick and a unit stop; revoking a box (ADR-011
  §3.4) closes the stream, and ctl stops any session when the stream drops
  for longer than a short grace period.

---

## 7. Web or client

### 7.1 Decision A3

| Option | Good | Bad |
|---|---|---|
| A. Native client only, launched by `teleport://session?...` deep link | Exists today (Linux, macOS), lowest latency, full features. A deep-link scheme already exists for relay pairing links. | No Windows client at all. A customer has to install software. |
| B. Browser client only | Works for everyone with a current browser. Nothing to install. Fits "through our site". | Large new work: WebTransport to the relay, WebCodecs decode, the E2E layer (`src/e2e.rs`, `src/bridge.rs`) compiled to WASM, input capture in a browser. Relay-only: browsers cannot hole-punch raw QUIC. |
| **C. A for the first demo and for support staff; B for customers (recommended)** | Support staff use Linux or macOS and can install the client. Customers get the browser. | Two clients to keep in step with the protocol. |

Browser client notes: moq-lite already has a WebTransport path in the MoQ
project, so the relay needs WebTransport listening (HTTP/3) alongside raw
QUIC; check browser WebTransport and WebCodecs H.264 support at the time
(Chromium and Firefox have both; confirm Safari). Use H.264 for browser
sessions. The browser client would live in this repository (it is the
protocol owner) and be published as a static bundle the account and admin
sites load.

---

## 8. Security threats and mitigations

| Threat | Mitigation | Residual |
|---|---|---|
| Relay compromise | E2E: relay sees IPs, sizes, timing, never content; cannot forge host or client. | Can drop or delay; typing cadence visible from input timing. |
| Stolen grant | One use, ≤ 5 min, bound to a client key the grant names. | A stolen client device with its key in memory during the 5 minutes. |
| Compromised cloud or insider | Cloud can mint grants, so it can request sessions. Consent is enforced on the box: a support grant without a valid owner pre-approval prompts at the screen; banner always shown; every session in the owner's audit log, written by the box as well as the cloud. | A compromised studio-api can forge an `owner` grant. Mitigation for later: owner grants co-signed by a key held by the owner's browser (passkey), checked by the box. |
| Support staff abuse | Staff role in trax-auth, staff identity shown on the box prompt, view-only default, audit visible to the customer. | Within an approved session, a staff member can do what the permissions allow. |
| Box as pivot | Teleport host runs as its own user with access only to the PipeWire node and uinput; no network listener; outbound to the relay only; no Nebula, no North APIs (ADR-011 §9). | A Teleport memory-safety bug in the FFmpeg C code is still a box compromise. |
| Phishing ("let me support you") | Support sessions are only started from the admin site by a staff account; the box prompt names the staff member; customers are told TRaX never asks for a code. | Social engineering of the owner into a pre-approval. |
| Cloned box | Device key is TPM-resident; a copy cannot certify a host key. | Software-key boxes (ADR-011 weaker tier) are cloneable; refuse remote sessions on them, or label them. |
| Operator privacy at the box | Banner; capture is the compositor's output, which the compositor can mask; mic and audio not captured in v1. | Whatever is on screen is visible. |
| Relay abuse as a free relay | Token admission: relay accepts only short-lived tokens minted by studio-api for one namespace. | DoS against the relay; rate limit per box and per user. |

The ADR-011 rule applies: every control above is verified from the side it
is supposed to stop (a grant for another box is refused; an expired grant is
refused; a support grant with no consent prompts; the relay refuses a client
with no token), not only from the side it allows.

---

## 9. Work split

### 9.1 In this repository (github.com/ejb1123/teleport), as PRs

| # | Work | Notes |
|---|---|---|
| T0 | **License.** A `LICENSE` file, or a written grant to TRaX (D1). | Blocks shipping, not design. |
| T1 | Host-only build target (`.#teleport-host`): no SDL client, no launcher; FFmpeg without GPL encoders as a build option. | For appliances. |
| T2 | `--source pipewire-node:<name>`: connect to a named PipeWire video node without the portal; DMA-BUF into VA-API. | Generic: any kiosk or headless compositor. |
| T3 | Absolute-pointer and keyboard injection through uinput, as an alternative to portal input. | Reuses the gamepad uinput code. |
| T4 | Issuer-signed tickets as an enrollment kind: `--ticket-issuer-key`, ticket verification, `jti` replay cache, ticket permissions mapped to guest permissions, possession proof via the device-key transcript signature. | The core of managed mode. |
| T5 | External host-key certification: `--host-key-ephemeral` plus a hook that hands the public key to a local signer and the client side that accepts a cloud-supplied fingerprint for this session. | Generic: any fleet with a device identity. |
| T6 | Relay token admission: `teleport relay --token-issuer-key`, tokens scoped to one namespace and expiry, replacing `--key` for internet-facing relays. WebTransport listener (for T8). | |
| T7 | Event stream on the `host-admin` socket: session start, approval, permission change, end, as JSON lines, for ctl to forward. | |
| T8 | Browser client: WebTransport, WebCodecs, E2E layer in WASM, input. | Later milestone. |
| T9 | Fix #5 and #6 before the box depends on it. | |

Everything here is generic. None of it names TRaX, studio-api or the box,
so Teleport stays a standalone product with a managed mode any fleet can use.

### 9.2 In the TRaX monorepo

| # | Work | Where |
|---|---|---|
| M1 | Compositor publishes its output as a PipeWire node; remote-session banner; screen-only controls refuse remote input. | `infra/box` (QML app) |
| M2 | ctl: `StartRemoteSession`/`EndRemoteSession` on the tunnel allowlist; consent prompt; host-key certification with the TPM device key; transient unit; event forwarding to the support log. | `infra/box/ctl` |
| M3 | NixOS: teleport flake input, `trax-box-remote@.service`, users and groups, issuer public key in the image. | `infra/box/nix` |
| M4 | studio-api: `remote_sessions` and `support_grants` tables, grant minting through Vault transit, relay tokens, ownership and role checks, audit API. | `services/studio-api` |
| M5 | encoder-service: carry the directive and the certified fingerprint for `device_kind=box`. | `services/encoder-service` |
| M6 | account site: **Open** on a box, **Support access** (grant a window), session history, **End session**. | `apps/account`, account-api |
| M7 | admin site: **Request support session**, staff session history. trax-auth `support` role. | `apps/admin`, admin-api, trax-auth |
| M8 | Relay deployment: ArgoCD app or PoP systemd unit, GeoDNS name, metrics (sessions, bytes, P2P vs relay ratio, refused tokens) and alerts. | `deploy/argocd` or `infra/edge` |

Dependencies inside TRaX: ADR-011 phase B1 (claim and hello over the
management stream) must be live first; this is its D11, later than B5
(remote control).

---

## 10. Milestones

| # | Milestone | Done when |
|---|---|---|
| **R0** | **Lab demo: a support agent views one lab box from the admin site with customer consent.** | On the HD 530 pilot box: admin clicks **Request support session**; the box screen asks; the operator allows view; the native client opens by deep link and shows the box's output through a TRaX-run relay; the box shows the banner; **End session** in the account site stops it within a second; both audit logs have the session. Shortcuts allowed for R0 only: one relay VM; issuer key in a file on the lab studio-api; kmsgrab instead of the PipeWire node if M1 is not ready; no browser client. |
| R1 | Owner access | Customer opens their own box from the account site with the native client; support grants with time windows. |
| R2 | Control | Support control with second approval; uinput input; screen-only controls refuse remote input, proven from the remote side. |
| R3 | Hardened | Issuer key in Vault transit; relay token admission; per-metro relays; metrics and alerts; security review per ADR-011 D11; license settled. |
| R4 | Browser client | Customer opens a session from the account site with no install. |

---

## 11. Decisions (EJ, 2026-10-25)

All nine are decided. D1: EJ chose MIT ("licence MIT"), so the repository is
MIT, and the box package is built without the GPL encoders (libx264/libx265) so
the shipped binary stays MIT. D2 to D9: EJ accepted every recommendation below.

| # | Decision | Options | Decided |
|---|---|---|---|
| D1 | **License.** The repository has none, so TRaX has no right to ship it. Also: Teleport links FFmpeg with libx264/libx265, which makes a distributed binary GPL. | (a) a permissive license (Apache-2.0/MIT) on the repo (b) keep it proprietary and give TPR Group a written license (c) a copyleft license (MPL-2.0 or GPL) | **(a) MIT.** The box package is built without GPL encoders. |
| D2 | Relay placement | (a) separate `teleport relay` beside the PoPs (b) PoP mediamtx MoQ (c) TRT gateway (d) Cloudflare | **(a)** |
| D3 | Client | (a) native only (b) browser only (c) native first, browser for customers | **(c)** |
| D4 | Support staff role | (a) any trax-auth `admin` (b) a new `support` role | **(b)**: support should not need full admin |
| D5 | Unattended support | (a) never; someone must be at the box (b) owner pre-approved windows (c) always allowed for support | **(b)** |
| D6 | Recording | (a) never (b) owner opt-in, stored on the box and uploaded to the owner's storage (c) always, stored by TRaX | **(b)** |
| D7 | Capture on the box | (a) compositor PipeWire node (b) kmsgrab (c) portal backend | **(a)**, with (b) allowed for R0 |
| D8 | Where managed-mode code lives | (a) all generic pieces upstream in this repo (b) a TRaX fork | **(a)**: one codebase; nothing in §9.1 is TRaX-specific |
| D9 | Does this supersede ADR-011 D11's "restricted shell / TraxBoxControl console"? | (a) Teleport replaces it (b) both, screen first (c) console first | **(b)**: screen sharing answers "what does the customer see"; a console is a separate review |
