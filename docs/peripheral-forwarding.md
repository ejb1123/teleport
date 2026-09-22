# Peripheral forwarding: implementation paths and acceptance gates

Assessment date: 2026-09-22. This is a design and test plan, **not a claim that USB,
smart-card, or remote WebAuthn forwarding is implemented**. No kernel modules,
system services, reader drivers, browser extensions, or privileged helpers were
installed while preparing it.

Here, **endpoint** means the Mac/Linux computer running the Teleport client;
**desktop host** means the physical Linux computer being controlled. For USB/IP
these names are reversed: the endpoint exports a physical device and is the
USB/IP server; the desktop host imports it and is the USB/IP client. Linux's
virtual host controller works on a physical machine; a VM is not required.
[Linux USB/IP protocol](https://cdn.kernel.org/doc/html/latest/usb/usbip_protocol.html)

## Supported versus proposed

| Capability | Current boundary | Viable next implementation | First acceptance target |
| --- | --- | --- | --- |
| Text clipboard | Existing explicit send/fetch; host opt-in | Keep separate from device access | Existing desktop session |
| SSH-agent forwarding | Separate feature; does not forward a USB device or smart-card reader | Session-scoped agent protocol bridge | Local agent signs; private key stays at endpoint |
| Generic USB, Linux endpoint | Not implemented | Linux USB/IP exporter → authenticated tunnel → Linux `vhci_hcd` importer | A disposable, nonessential bulk-transfer device |
| Generic USB, Mac endpoint | Not implemented; cannot promise arbitrary devices | IOKit/libusb exporter with narrowly scoped acquisition helper where necessary | A driver-unclaimed development board |
| PC/SC smart card, Mac or Linux endpoint | Not implemented | Native endpoint PC/SC → bounded APDU channel → Linux pcsc-lite virtual reader | A test PIV card and a nonproduction certificate |
| Physical FIDO2 key in remote Linux browser | Not implemented | Endpoint native HID → CTAPHID channel → restricted Linux UHID device | One named key plus Firefox and Chromium |
| Mac Touch ID / platform passkeys in remote browser | Not implemented; not a USB device | Separate browser/OS credential broker, if supported APIs and entitlements permit | Dedicated test relying party, never arbitrary-site claims |
| FIDO2 login **to Teleport itself** | Separate authentication feature | Register and verify credentials for Teleport's own relying party | Login tests; does not establish forwarding support |
| Windows endpoint | Not a validated peripheral target | Native PC/SC/HID candidates; generic USB needs a separately evaluated exporter/driver | Windows hardware and signed packaging before support claims |

“SSH key backed by a security key,” “PIV smart card,” “FIDO2 security key,” and
“platform passkey” are different interfaces, even when one physical product
implements several of them. Enabling one must not silently expose the others.

## Shared transport and lifecycle

The current native session has authenticated MoQ transport and separate video,
metadata, updates, audio, and control tracks. Peripheral operations must not be
put into the drop-tolerant video path or allowed to stall desktop input.
The following are proposed protocol requirements, not existing wire contracts:

- Negotiate an individual capability and version before creating a forwarding
  channel. Old clients/hosts remain usable with the capability absent.
- Require host policy **and** endpoint consent, identifying the device and the
  destination host fingerprint. Default off; no automatic “share every device.”
- Use reliable ordered delivery per device, bounded frames, request IDs,
  cancellation, deadlines, and backpressure. Abort on sequence loss; never replay
  a previous session's APDU, signature operation, or USB write on reconnect.
- Bind channels to the authenticated desktop session and device credential.
  Revocation, disconnect, explicit stop, or endpoint unplug closes handles and
  removes host-side virtual devices. Reconnect requires fresh authorization.
- Put device I/O on bounded workers, not media/UI event loops. Keep metrics to
  counts and timings; no PINs, APDUs, authenticator payloads, or raw device data in
  logs, error messages, crash attachments, or stats overlays.

## Generic USB on the physical Linux host

Start Linux-to-Linux with the in-kernel USB/IP exporter and importer as the
compatibility reference. The importer needs `vhci_hcd`; the exporter commonly
uses `usbip_host`. Device binding and attach/detach need a privileged component
or explicitly delegated permissions. The USB/IP protocol carries transfer
requests and cancellations, not Teleport authentication or encryption, so do not
expose its raw listener as a Teleport network service.
[Linux USB/IP architecture and messages](https://cdn.kernel.org/doc/html/latest/usb/usbip_protocol.html)

Proposed helper boundary:

1. The unprivileged Teleport process owns authentication and the encrypted tunnel.
2. A separately installed, root-owned helper accepts same-UID/session-authorized
   requests over private local IPC. It permits only enumerated device IDs and
   attach/detach operations, never arbitrary commands, sysfs paths, or module names.
3. The helper checks actual device identity and policy immediately before binding,
   records ownership, and detaches on session death. A changed device at the same
   bus address must not inherit authorization.
4. Any TCP adapter required by stock USB/IP tooling stays isolated locally;
   loopback alone is not authentication against other local users. Prefer a
   private helper namespace or an FD-based adapter with a reviewed lifecycle.

Initially deny hubs, storage, keyboards, mice, network adapters, and security
tokens from generic auto-selection. Sharing may remove a device from endpoint
applications; storage additionally risks filesystem corruption on disconnect.
Never detach the user's only keyboard, remote-access network adapter, or disk.
Isochronous camera/audio, suspend/resume, and composite-device behavior are
separate compatibility milestones, not implied by successful enumeration.

### What makes the Mac exporter harder

macOS can export some devices through user-space USB APIs; it does not need a
virtual USB host controller for this direction. However, libusb documents that
capturing driver-owned devices can require authorization and affects the whole
composite device, not merely the selected interface. Its maintainers recommend
native HID access rather than libusb for macOS HID devices. An Apple Developer
account or ordinary application notarization does not by itself resolve device
capture entitlement restrictions.
[libusb platform and driver-claim limitations](https://github.com/libusb/libusb/wiki/FAQ)

Do not ship `pyusbip` as a turnkey solution: its own documentation describes
missing transfer types and cancellation handling, and warns of Linux USB-stack
failures from malformed/partial responses. It is evidence that Mac export is
possible, not a production compatibility guarantee.
[pyusbip's documented limitations](https://github.com/jwise/pyusbip#limitations)

The concrete next spike is a small audited exporter for one unclaimed device,
with correct descriptors, control/bulk transfers, URB cancellation, and unplug.
Driver-owned devices require a separately reviewed, signed privileged helper or
appropriate approved system extension. No development-mode reboot, security
policy weakening, or blanket root UI is part of this plan.

## Smart cards: prefer PC/SC, not whole-device USB

On the endpoint use the native PC/SC resource manager to select one reader,
observe insertion/removal, establish transactions, and transmit APDUs. On Linux,
present a virtual reader through pcsc-lite's IFD handler interface. This preserves
normal host applications such as OpenSC while leaving USB reader ownership on
the endpoint. Implement reconnect/reset, ATR, protocol negotiation, transactions,
and bounded response sizes—not just `SCardTransmit` in isolation.
[PC/SC API](https://pcsclite.apdu.fr/api/group__API.html),
[pcsc-lite IFD interface](https://pcsclite.apdu.fr/api/group__IFDHandler.html)

`vsmartcard` supplies a useful prototype: its `vpcd` reader plus `vicc --type=relay`
can bridge a real card. Its wire protocol covers power/reset, ATR and APDU
exchange. The documented TCP listener is not authenticated Teleport IPC; an
isolated prototype may tunnel it, but production should use a hardened private
broker/IFD boundary. Installing the host reader driver is an explicit system
configuration operation. Its GPLv3 licensing also needs consideration before
bundling rather than invoking a separately installed component.
[Virtual Smart Card](https://frankmorgner.github.io/vsmartcard/virtualsmartcard/README.html),
[vpcd protocol](https://frankmorgner.github.io/vsmartcard/virtualsmartcard/api.html)

For Mac-to-Linux this does **not** require installing a virtual reader on macOS:
the Mac reads its already-supported physical reader. CryptoTokenKit can expose
cryptographic assets but is not automatically an arbitrary APDU interface for
every token; tokens available only through that API need a separate adapter.
[Apple CryptoTokenKit](https://developer.apple.com/documentation/cryptotokenkit)

Sharing a card authorizes the host to issue commands, potentially including PIN
verification or signing. Private keys staying on the card is not sufficient
protection against a malicious host. Require explicit session consent; never
automatically retry incorrect PINs, cache PINs, reset cards, or expose vendor
control commands. PIN-pad reader support needs its own feature/control-command
policy and hardware tests.

## Remote WebAuthn is two distinct projects

### A physical FIDO2 key plugged into the endpoint

A focused implementation can use native endpoint HID APIs and create a Linux
UHID device with a fixed, reviewed FIDO-only descriptor. Do not forward arbitrary
remote report descriptors that could turn the virtual device into a keyboard.
UHID provides creation, reports, and destruction; closing its FD removes the
virtual device. Access to `/dev/uhid` needs narrowly scoped provisioning or a
helper, not world-writable permissions.
[Linux UHID](https://docs.kernel.org/hid/uhid.html),
[HIDAPI supported backends](https://github.com/libusb/hidapi)

The bridge must preserve CTAPHID channel framing, initialization, cancellation,
keepalives, and user-presence behavior. This is a HID/CTAP interface—not the
token's PIV/CCID interface. Initially reject reset, credential-management,
configuration, and vendor commands; explicitly document any compatibility lost
by policy. Test PIN/UV behavior without weakening authenticator checks.
[FIDO CTAP specifications](https://fidoalliance.org/specifications/download/)

The Linux browser remains responsible for WebAuthn origin/RP checks. A raw
authenticator relay cannot independently establish the real browser origin from
an opaque client-data hash; do not display a host-supplied site name as verified
security information. A compromised remote host can still ask an exposed token
to perform operations. A physical touch is not proof the requested site is safe.
[WebAuthn security model and client data](https://www.w3.org/TR/webauthn-3/)

### Touch ID and platform/synchronized passkeys

These are not USB peripherals. An application-level broker needs trustworthy
browser-origin information, credential request/response semantics, cancellation,
and OS authorization UI. Apple's ordinary passkey integration requires associated
domains; it is not blanket permission for Teleport to authenticate arbitrary
third-party sites. A browser extension or specialized browser integration needs
separate investigation and cannot simply fabricate a new origin.
[Apple passkey service integration](https://developer.apple.com/documentation/authenticationservices/connecting-to-a-service-with-passkeys)

FIDO2 authentication **to Teleport** uses Teleport's own credential registration
and challenge verification and should remain independent of all these bridges.
Native physical-key support is feasible with libraries such as libfido2 on Linux
and macOS, but login support must not be advertised as remote-browser passkey
support. [libfido2](https://github.com/Yubico/libfido2)

## Tests required before enabling a capability

1. **Protocol:** fake device backends; malformed lengths, unknown commands,
   timeouts, duplicates, cancellation, queue exhaustion and fuzzing. No device
   capability appears when its real backend/helper is unavailable.
2. **Authorization:** default-off, consent denial, another local UID, revoked
   client, replaced USB device, stale session, and reconnect. Confirm no raw
   USB/IP or card listener is exposed to the LAN or ZeroTier interface.
3. **Hardware isolation:** use a disposable Linux test system first for kernel
   USB import. No production desktop driver rebinding, module changes, reboot,
   or remote-only recovery assumptions.
4. **USB:** selected bulk device works in its real application; cancellation,
   unplug, malformed response and network failure detach cleanly. Expand one
   named device/class at a time; enumerate failures as unsupported.
5. **Smart card:** read a test certificate, then sign a disposable challenge with
   explicit consent; validate insertion/removal, exclusive transaction behavior,
   wrong-PIN handling without retries, timeout, and driver restart. Test the
   actual macOS version and reader, not only a software emulator.
6. **FIDO relay:** use a spare token and test RP. Registration and assertion in
   Firefox/Chromium must validate RP ID, challenge, signature, UP/UV flags, touch,
   PIN, cancellation, unplug, and rejected administrative commands. Test multiple
   simultaneous callers and confirm no replay after reconnect.
7. **Lifecycle/performance:** revoke and disconnect during an in-flight operation;
   verify helper/device cleanup and no desktop input starvation. Measure latency
   with loss and realistic RTT, rather than asserting universal USB compatibility.
8. **Distribution:** review helper installation, signed macOS packaging,
   NixOS opt-in modules, rollback, licenses, and uninstall. Publish tested hardware
   and platform versions with each enabled capability.

Recommended order: session-scoped SSH-agent work independently; PC/SC prototype;
physical FIDO HID prototype; Linux USB/IP integration; then carefully limited
Mac generic USB export. Platform-passkey brokerage remains its own research and
security-review track.
