# Supervising a host

Some hosts are not started by the desktop's user but by a service: a
kiosk's controller that starts a host only for a support session, or a
fleet agent. Two host options make that service's job simple without
giving it the admin socket, which only the host's own user may open.

Status: development (2026-10). Covered by `tests/supervision.rs`.

## `--events-file PATH`

The host appends one JSON object per line to `PATH` and flushes each line
as it happens. The file is created with mode 0640, so a supervisor in the
host user's group can read it. Every line has `at` (unix milliseconds) and
`event`:

| `event` | Other fields | When |
|---|---|---|
| `ready` | `fingerprint`, `address` | The host is listening. `fingerprint` is the certificate clients pin, and the audience of a [session ticket](tickets.md). |
| `relay_connected` | `relay`, `namespace` | Each time the host (re)connects to its relay. |
| `approval_requested` | `session`, `name` | A guest waits for approval. |
| `session_joined` | `session`, `name`, `role`, `permissions`, `transport` | A session is admitted and streaming. `transport` is `direct`, `relay` or `p2p`. |
| `session_refused` | `session`, `name`, `reason` | A session was refused or not approved. |
| `session_left` | `session`, `name`, `reason` | A session ended, for any reason. |
| `exiting` | `reason` | The host is stopping. |

Writing is best effort: a full disk logs one warning and the session goes
on. Names come from the credential (a device name, a ticket's `sub`), so
treat them as text to display, not as identifiers.

## `--exit-after-session`

The host exits with status 0 when its session engine ends: the last session
left, or the first one failed. Start one host per session and let the
supervisor's process manager (for example a systemd unit with
`RuntimeMaxSec=`) bound how long it may wait for a client. Stopping the host
(SIGTERM) ends every session at once.

## Example: one session, then exit

```sh
teleport host --source kms --identity-dir /run/teleport/host \
  --ticket-issuer-key <hex> --ticket-approval issuer \
  --relay moqt://relay.example:4444 --relay-fingerprint <pin> \
  --events-file /run/teleport/events.jsonl --exit-after-session
```
