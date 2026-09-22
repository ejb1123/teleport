# Experimental SSH-agent forwarding

This forwards requests to an agent on the **client** computer, so programs inside
the remote Linux desktop can authenticate with its SSH keys without copying the
private keys. It does not forward arbitrary USB devices, smartcards, GPG agents,
or website/passkey requests. Both ends need the updated implementation.

**Only enable this for a host you trust with SSH authentication.** A compromised
host, or another process running as your user on that host, can request signatures
while forwarding is active. Keeping private keys local does not prevent that
misuse. Use a dedicated, minimally loaded agent with local confirmation and key
lifetimes when appropriate. Teleport does not silently change your agent policy.
[OpenSSH agent restrictions](https://www.openssh.org/agent-restrict.html)

## Enable explicitly

Allow the capability in the Linux host's startup arguments:

```sh
teleport host --allow-ssh-agent ...
```

For the NixOS module, add `"--allow-ssh-agent"` to
`services.teleport-desktop.extraArgs`, then apply your configuration when ready
for a host-service restart. Building Teleport does not change the running service
or enable forwarding automatically.

On the client, run Teleport from an environment with your local `SSH_AUTH_SOCK`.
The launcher has an **SSH agent** toggle with a second-click confirmation. Consent
applies to the selected address and next session; it is not saved as a preference.
Alternatively:

```sh
teleport client HOST:4443 --pairing-file PRIVATE_PAIRING.json --forward-ssh-agent
```

The host advertises support and must explicitly allow the request. An older host
or a host with forwarding disabled rejects it instead of silently dropping the
requested feature. Automatic reconnect is disabled for agent-forwarding sessions.
The session title identifies agent sharing; **Disconnect** ends forwarding.

The host prints a session-specific socket path in its local service log. In a
terminal on the remote desktop, point only the intended command at that path:

```sh
SSH_AUTH_SOCK=/tmp/teleport-agent-SESSION_ID/agent.sock ssh user@destination
```

Use the actual path reported for this session, not the example. The application
does not overwrite the desktop's existing agent or change its global environment.
The private socket is removed when the session ends or access is revoked. Start
a new explicitly authorized session to obtain a new socket.

## Restrictions

- Agent messages travel on separate bounded, ordered tracks inside the pinned,
  authenticated desktop connection, not an unauthenticated TCP listener.
- Only identity listing and SSH public-key authentication signing are permitted.
  Add/remove identities, loading PKCS#11 providers, arbitrary `ssh-keygen -Y`
  signing, agent extensions and non-SSH signing are rejected on the client too.
- Requests, replies, concurrency and time spent waiting for the agent are bounded.
  A failed or desynchronized forwarding channel ends the session rather than
  replaying a signature request or associating a reply with a different request.
- OpenSSH destination-constrained keys are **not supported** by this initial
  integration: it does not implement the authenticated session-binding extension.
  They must fail closed; remove no constraints merely to make forwarding work.
- Local agent confirmation remains the agent's responsibility. Teleport cannot
  force a confirmation prompt for every key or guarantee hardware touch policy.

Protocol references: [SSH agent protocol](https://www.rfc-editor.org/rfc/rfc9987.html)
and [OpenSSH agent extensions](https://github.com/openssh/openssh-portable/blob/master/PROTOCOL.agent).
Real Linux/macOS agents and security-key-backed SSH keys need acceptance testing
in addition to the isolated protocol tests; this is not a blanket compatibility
claim for all agents or hardware.
