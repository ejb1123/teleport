# Guests and shared sessions

Several clients can watch and play on one host at once, for example friends
joining a game with their own controllers (parsec-comparison G15 and G16).
Each session is an **owner** or a **guest**:

- **Owners** are the host's own devices: paired with a code, logged in with
  the Teleport password or the Linux account, or holding the host's
  `pairing.json`. They have full control and never wait for approval. A host
  with one owner connected behaves as before.
- **Guests** are devices enrolled through an **invite**. They get only what
  the owner granted, need a live approval each time they connect, and can be
  changed or revoked at any time.

There is no account and no "connect without approval" mode. The owner may
mark a single guest device **trusted**; a trusted guest then skips approval,
but only while an owner session is connected (unless the host keeps guests
without an owner, see [policy](#session-policy)).

## Invite a guest

On the host, somewhere guests cannot see the screen:

```sh
teleport host-admin create-invite --name Alice --uses 1 --minutes 60 \
  --permissions view,gamepad
```

Or **Invite guest** in `teleport host-manager` (one use, one hour, view and
gamepad). The response holds a link such as
`teleport://invite/<id>/<secret>` (plus the host's relay id when the host
uses a relay). Treat it like a pairing code: anyone holding it can enroll a
guest device until it is used up, expires or is revoked
(`teleport host-admin revoke-invite ID`, or **Revoke all managed**).

The guest enrolls with the link on stdin, never on the command line:

```sh
teleport pair HOST:4443                       # direct (TCP on the host's port)
teleport pair HOST:4443 --relay moqt://relay.example:4444 \
  --relay-fingerprint <relay fp> --relay-key <key>   # through the relay
```

and then connects like any client (`teleport client`, or the launcher). The
client logs "waiting for the host" until it is approved.

### What an invite exchange proves

The exchange (`TPINVT01`) is code pairing's SPAKE2 protocol
([pairing](pairing.md)) with the invite's 128-bit secret as the password.
The invite id goes first in the clear so the host can pick the secret; it is
bound into the transcript. As with `TPPAIR02`, the client sends its device
key after key confirmation and before anything is issued; the host spends one
use of the invite and stores the guest device with that key in one persisted
update, then seals the credential to the key
([device keys](relay.md#device-keys)). A guest device is therefore always
keyed: through a relay or peer to peer, its stored hash alone opens nothing.

- A wrong secret fails key confirmation and spends nothing.
- A dropped device key (a meddling relay) issues and spends nothing.
- Uses are counted down in `access.json` before the credential leaves, so an
  invite never enrolls more devices than it allows, across restarts too.
  An exchange that fails after that point removes the device again; the use
  stays spent (fails closed).
- Expired and revoked invites enroll nothing. Through a relay the client
  checks that the host it reached matches the link's relay id.
- With no invite, code or login open, the enrollment port is closed.

Guest devices are named after the invite (`Alice`, or `Alice #2` for
several uses), so prompts never show text a guest chose.

## Permissions

| Permission | What it allows |
| --- | --- |
| view | Watch video and hear audio (always) |
| gamepad | Its own virtual pads: a block of four host pad slots, separate from everyone else's |
| keyboard-mouse | Keyboard and mouse, while it holds control (one session at a time) |
| clipboard | Explicit clipboard transfer, on hosts started with `--clipboard` |

The default is least privilege: **view and gamepad**. Change a guest live:

```sh
teleport host-admin set-guest DEVICE_ID --permissions view,gamepad,keyboard-mouse
teleport host-admin set-guest DEVICE_ID --trusted true
```

or the **Gamepad / Keys+mouse / Clipboard / Trusted** buttons in the host
settings window. Changes apply to running sessions at once: a lost
permission releases what the guest held (keys, buttons, its pads) and its
client shows the change.

Every event is checked on the host before it is injected
(`src/guests.rs`, `Members::screen` in `src/host.rs`): permissions, then
keyboard/mouse control, then stream leadership. Clients only display what
they are told.

**Keyboard and mouse means acting as the host user.** A guest who may type
can open a terminal, run programs and open these settings. Grant it only to
people you would let sit at the computer. While any guest holds this
permission the host stops forwarding an owner's SSH agent, and the settings
window asks for confirmation before granting it.

Guests' PlayStation controllers are emulated as Xbox pads, without
touchpad: Linux desktops drive the pointer with a DualShock 4 or DualSense
touchpad, which would bypass the keyboard/mouse rules. Steam's desktop
configuration can likewise turn any pad into a mouse; close Steam or
disable it for guests' pads if that matters.

## Keyboard and mouse control

One session drives the keyboard and mouse at a time:

- An owner gets control at once on input: the owner can always take it back
  by moving the mouse. Ctrl+Alt+T in an owner's client locks control to it
  until pressed again.
- A guest with the permission gets control when nobody holds it, or when
  the holder has been idle for three seconds, unless control is locked.
- Someone at the host takes it with **Take control** (host settings) or
  `teleport host-admin take-control`; guests are refused until
  **Release control** / `release-control`. Physical input at the host is
  not detected (the desktop portal does not report it).

When control changes hands, keys and buttons the previous holder held are
released first, and its client is told who took control.

## Approval

A guest connection (unless trusted, see above) waits for an answer:

- in the host settings window (`teleport host-manager`): **Allow** / **Deny**;
- on the admin socket: `teleport host-admin sessions`, then
  `approve ID` or `deny ID`;
- in an owner's connected client: a prompt over the video, Ctrl+Alt+Y to
  allow, Ctrl+Alt+N to deny (the oldest request, named in the prompt).

Nobody answering denies it after `--guest-approval-timeout` (30 s). A denied
or timed-out device may ask again only after 30 seconds, a kicked one after
a minute; each device has at most one request waiting, and at most eight
guests wait at once. A guest that disconnects while waiting withdraws its
request. Revoking a device while it waits keeps it out.

## Session policy

| Host option | NixOS (`services.teleport-desktop.sessions.`) | Default |
| --- | --- | --- |
| `--max-sessions N` | `max` | 8 |
| `--keep-guests-without-owner` | `kickGuestsWithOwner = false` | guests leave with the last owner |
| `--lock-on-last-disconnect` | `lockOnLastDisconnect` | off |
| `--guest-idle-minutes N` | `guestIdleMinutes` | 0 (never) |
| `--bandwidth-cap KBPS` | `bandwidthCapKbps` | 0 (no cap) |
| `--guest-approval-timeout S` | `guestApprovalTimeout` | 30 |

- **Guests leave with the owner:** when the last owner session leaves,
  guests are disconnected, and while no owner is connected even trusted
  guests need approval (from someone at the host).
- **Lock on last disconnect:** `loginctl lock-session` (the caller's session,
  or the user's graphical session from logind for the user service).
- **Idle kick:** guests without keyboard, mouse or gamepad input for that
  long are disconnected. Owners never are.
- **Bandwidth cap:** one encoder serves every session, so the host sends
  `bitrate × sessions`. With a cap the encoder runs at most at
  `cap / sessions`.

## Sharing one stream

- One capture and one encoder serve every session; the first session's
  broadcast holds the stream and later sessions' broadcasts forward it
  (`forward_track`). Audio is forwarded the same way. A joining session
  starts at a fresh keyframe.
- The **stream leader** is the first owner session, or the first session
  when no owner is connected. Only the leader changes the stream (size,
  frame rate, bitrate, monitor, extra monitor streams, local cursor mode);
  others' requests are refused with a notice and their display is applied
  if they lead later. Privacy mode and virtual monitors need an owner as
  leader.
- The codec is chosen once: later clients get `Desktop::shared_stream` and
  take it (`--codec auto`); an explicit different `--codec` is refused with
  a message. HDR and SDR sessions cannot share a stream.
- Rate control is per session: the leader's feedback drives the encoder,
  every other session's own controller can pull the shared bitrate down to
  a quarter of the leader's (a worse link skips frames instead), and the
  cap's share bounds it. Guests' keyframe requests are limited to one a
  second.
- A new connection with the credential of a running session is that
  device reconnecting (the old connection may not have timed out yet): it
  replaces the old session, and when that was the only one the stream
  starts afresh for it. Devices that share one credential (copies of the
  host's `pairing.json`) therefore cannot be connected at the same time;
  give each device its own (pairing code or password login).
- Clients from before shared sessions can join as viewers but get no
  access updates; clients that cannot strip frame-timing headers are
  refused from a stream that has them.

## Clients

F8 shows the session's role, permissions, who holds keyboard and mouse and
how many other sessions are connected. A panel over the video explains
permission changes and lost control, and shows owners the guests waiting
for approval. `--smoke-*` options drive the same paths in headless tests.

## Not done

- Per-guest encoders for different codecs or sizes (one shared stream).
- Detecting physical keyboard/mouse use at the host.
- Restricting a guest to some applications (Parsec's "approved apps" is a
  weak boundary; see parsec-comparison section 6).
- Launcher UI for invites and approval prompts in the launcher itself (CLI,
  host settings window and the in-session prompt only).
- A guest's clipboard permission lets it write the host clipboard at any
  time, not only while it holds control.
