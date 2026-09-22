# Security-key login and credential forwarding

Status: development implementation and design, September 2026. The offline
native FIDO2 adapter below is implemented; physical hardware acceptance remains
pending. It is **not** FIDO2 host login, passkey login or remote WebAuthn. This
document does not establish smartcard, USB or SSH-agent forwarding support.
The existing `src/access.rs` implements OPAQUE password enrollment and revocable
device tokens; streaming authenticates with a saved token and pinned TLS
certificate. Neither that code nor this proposed integration has had an
independent security audit.

## Available offline development commands

Run from the packaged Nix app or `nix develop`; the environment supplies an
absolute trusted `TELEPORT_LIBFIDO2` path. The commands run locally on the
computer holding the USB key, not through the remote desktop:

```sh
teleport security-key list
teleport security-key probe
teleport security-key enroll --fingerprint TRUSTED_HOST_SHA256_HEX --output key-test.json
teleport security-key verify --fingerprint TRUSTED_HOST_SHA256_HEX --credential key-test.json
```

Use `--device` with a path from `list` when multiple keys are present. `list` and
`probe` do not create credentials or request a PIN. `enroll` **does create a
non-discoverable credential on the selected authenticator**, requires presence
and user verification, and proves possession with a fresh assertion before
saving its public credential. It never overwrites an existing output file. A
failed operation can leave an empty output file and a newly created key-side
credential; no automatic credential deletion, reset or PIN change is attempted.

`verify` creates a new random, host-bound offline challenge and verifies the
result with libfido2. PIN entry is hidden/local and not automatically retried.
Opening a device is bounded to five seconds; touch/verification operations to
30 seconds. These are blocking CLI operations, not integrated SDL UI workers.
The native RP encoding currently uses the two 32-hex-digit halves of the host
fingerprint as labels under `teleport.invalid`. It is not a website origin.

Only ES256 credentials and UP+UV are accepted; U2F fallback is disabled. This
diagnostic does not establish manufacturer trust, durable counter monitoring,
production host enrollment, network login or forwarding. Physical Linux/macOS
key tests and standalone Mac dynamic-library bundling still require acceptance.
Do not supply private keys, PINs or passwords on the command line.

## Distinct features

| Feature | Authenticates to | Required integration |
| --- | --- | --- |
| Teleport security-key login | The selected Teleport host | Host credential registry, challenge verification, client authenticator UI |
| Remote WebAuthn | A website open on the remote desktop | Browser/OS integration preserving the website's origin and RP rules |
| Smartcard/PIV forwarding | A remote application using a card | PC/SC or application-specific proxy, PIN and APDU policy |
| USB forwarding | A remote driver/application | Platform USB transport and device-ownership management |
| SSH-agent forwarding | An SSH destination | Restricted agent protocol, destination policy and local consent |

A YubiKey may implement several of these interfaces. Supporting one does not
support the others. In particular, forwarding CTAP messages is not WebAuthn
origin validation: the authenticator receives an RP identifier and a client-data
hash, not an independently verified browser-origin authorization. WebAuthn's
client/RP rules must still be enforced by the responsible browser or native
platform integration. [WebAuthn specification](https://www.w3.org/TR/webauthn/)

## Recommended first implementation: native FIDO2 login

Use `libfido2` for physical USB security keys on Linux/macOS. It supplies
authenticator communication and signature verification and supports both target
platforms. Bind a small reviewed Rust wrapper to its C API; keep raw pointers,
allocation/free ownership and cancellation within that wrapper. Do not implement
CTAP framing or signature verification from scratch. Package the library through
the Nix flake and Mac app bundle. Linux access needs appropriate device rules;
do not grant unrestricted access to every `hidraw` device.
[libfido2 overview](https://developers.yubico.com/libfido2/)

Call this **native FIDO2 security-key login**, not browser WebAuthn or iCloud
passkeys. Use ES256 initially, require user presence and user verification, and
reject authenticators that cannot satisfy the chosen policy. PIN entry is local
to the client/authenticator operation: never send or retain the PIN on the host.
Unsupported algorithms/attestation formats must fail explicitly, not downgrade.

### Host and relying-party binding

This is a proposed application protocol, not an already implemented standard
profile:

- Require an independently trusted host TLS fingerprint before security-key
  enrollment or use. Establish that trust through existing OPAQUE enrollment,
  one-time pairing, or an authenticated SSH import. Never accept an arbitrary
  certificate merely because it requests a key touch.
- Derive a per-host CTAP RP namespace from the pinned identity. The offline
  adapter uses `f-<first32hex>.<last32hex>.teleport.invalid`, retaining all 256
  fingerprint bits within DNS label limits. This is an application namespace, **not** proof of domain
  ownership and not an origin usable with browser/Apple passkeys. Pinning and
  the application policy provide the host binding. Confirm authenticator
  compatibility before freezing the exact encoding.
- Bind a versioned, length-delimited challenge envelope to the host fingerprint,
  RP identifier, account identifier, requested operation, unpredictable challenge
  and current connection. Hash the exact agreed bytes as client data. Use
  distinct registration and login purposes. The host reconstructs expected data
  rather than trusting client-supplied challenge or RP fields.
- Changing the pinned identity must require explicit recovery/re-enrollment.
  LAN/ZeroTier addresses can change without changing identity. ZeroTier network
  membership alone never establishes this trust.

The client requests an assertion only after showing the selected host and
operation. Host verification must supply the expected RP, data hash, presence
and verification policy to `fido_assert_verify`, alongside the enrolled key.
That API verifies the signature and associated authenticator attributes; the
application still owns challenge freshness, account mapping and transport
authorization. [Assertion verification](https://developers.yubico.com/libfido2/Manuals/fido_assert_verify.html)

### Enrollment, storage and connection authorization

Enrollment is explicitly authorized in Host Manager through its existing local
administration channel. A remote key can answer the registration challenge over
an already pinned connection, but a normal desktop token must not silently gain
permission to install new login credentials. Validate the returned credential
and prove possession before committing its public key. Treat verified
attestation signatures and trusted manufacturer provenance as separate policies;
do not claim hardware certification from signature verification alone.
[Credential verification](https://developers.yubico.com/libfido2/Manuals/fido_cred_verify.html)

Store credential ID, validated public key/algorithm, account, RP/host binding,
label, required UV policy, counter state and revocation generation. Private keys
stay on the authenticator. Extend versioned account storage without reusing a
password-record field. Support two independently enrolled recovery keys and
trusted local/SSH recovery before allowing password access to be disabled.

For login, introduce a bounded pre-desktop authentication exchange over pinned
TLS/QUIC. Issue one short-lived random challenge per connection; consume it
atomically and authorize only that connection after successful assertion. Never
publish desktop, clipboard or input capabilities before completion. A new
connection/reconnect requires a fresh assertion unless an explicit, bounded
resumption policy is designed separately. A persistent bearer token issued after
one key touch is **not** security-key-required authentication on every connection.

Preserve current clients until the administrator explicitly changes access
policy. Existing legacy/shared tokens and password/code-issued tokens bypass a
new key-only login path if still accepted. The UI cannot claim globally enforced
two-factor/key-only access until that migration disables those alternatives.
Revoking a key must invalidate in-flight challenges and end any session whose
authorization depends on it. Treat authenticator counters as clone-risk signals,
with a documented policy for zero/non-increasing counters; do not use counters
as the sole replay defense.

## Platform alternatives

Apple AuthenticationServices provides native passkey/security-key UI, but
platform passkeys are a separate product track: establish an owned RP domain,
associated-domain entitlement and website association, signed app identity and
the corresponding server verification rules. An Apple Developer account alone
does not establish these bindings. Do not silently present the private CTAP
namespace above as an Apple-associated website.
[Apple passkey setup](https://developer.apple.com/documentation/authenticationservices/connecting-to-a-service-with-passkeys?changes=l_2)

For future Windows clients, the native WebAuthn assertion API accepts an RP and
client data and owns a user-facing authenticator interaction. It needs its own
platform adapter and validation of the selected native RP/origin model; a Linux
implementation is not evidence of Windows support.
[Windows assertion API](https://learn.microsoft.com/en-us/windows/win32/api/webauthn/nf-webauthn-webauthnauthenticatorgetassertion)

## Safe, testable next slice

The current slice provides local device enumeration, a PIN/UV capability probe,
and explicitly requested offline registration/assertion round trips. It does
not reset keys, change PINs, delete credentials, forward devices or change
production-auth policy. Software-generated assertion tests exercise the actual
native verifier, including signature/RP/hash tampering and missing UP/UV.

Next, validate physical devices and packaged Mac loading, then add host-side
credential storage, authenticated registration approval and connection-bound
challenges. An SDL integration needs bounded workers and explicit cancellation
in addition to the CLI's operation timeouts. Expand capability reporting to
algorithm/extension discovery before presenting enrollment choices in the UI.

Required acceptance tests:

- Valid signature; wrong key, RP, account, client-data hash, host identity or
  operation; missing UP/UV; malformed/truncated/oversized CBOR and signatures.
- Challenge expiry, replay, cross-connection reuse, concurrent consumption,
  revocation during authentication and active-session revocation.
- No media before authentication; no fallback to existing bearer/password
  credentials when key-required policy is selected.
- Fake-adapter cancellation/no-device/permission-denied/PIN-blocked paths;
  constrained parsers and fuzz tests without connected hardware.
- Physical USB keys on Linux and macOS, including PIN/touch, removal mid-request,
  multiple keys, wrong key, zero-counter behavior and app packaging permissions.
  Mock signatures or successful compilation do not establish hardware support.

Remote browser WebAuthn, smartcard and USB forwarding require separate threat
models and platform prototypes. Keep each capability opt-in, scoped to the
specific host/session/device and revocable. Generic CTAP or USB forwarding must
not expose authenticator reset, PIN-management or credential-management commands
under a feature labelled merely “use my key to log in.”
