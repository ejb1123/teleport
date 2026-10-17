# Scrolling

How the client turns local wheel and trackpad input into host scrolling
(`src/scroll.rs`, `src/mac_scroll.rs`, host side in `src/capture.rs`).

## Devices and units

SDL reports every scroll as a `MouseWheel` event with precise deltas in
"notch" units:

| Client | Mouse wheel | Trackpad / Magic Mouse |
| --- | --- | --- |
| macOS (SDL3 Cocoa, also behind sdl2-compat) | whole notches (`deltaY` rounded away from zero) | fractions (`NSEvent.deltaY`, about 10 points per unit), momentum included |
| Wayland (SDL3) | whole notches (value120 / 120) | pixels / 10 |
| X11 (XInput2) | whole notches | smooth valuator, 1 unit per notch |

- **Wheels** stay whole notches everywhere (`Event::Scroll`). The host
  injects them as portal `NotifyPointerAxisDiscrete`, or XTest buttons 4-7.
- **Trackpads** go to hosts that advertise `Desktop::smooth_scroll` (portal
  hosts and test hosts) as `Event::ScrollSmooth { dx, dy, finish }`. These
  are host logical pixels: 10 per unit times `--scroll-speed` (0.1-10,
  default 1). The host injects them with portal `NotifyPointerAxis(dx, dy,
  finish)`.
- **X11 hosts and older hosts** only have discrete steps (XTest has no smooth
  scrolling). They get one notch per 50 pixels of trackpad movement, rather
  than one per unit as before, so a trackpad no longer scrolls about five
  times faster than the finger. An X11 host that receives `ScrollSmooth`
  anyway converts it to notches the same way.
- **Coalescing:** smooth deltas are coalesced to at most one event per
  8 ms (about 120 Hz). They are flushed before any pointer motion or click,
  so ordering with the pointer is kept. The host receives the pointer
  position before the first delta of a gesture, and in pointer lock it
  receives no absolute position at all.
- **Axes:** horizontal and diagonal pans carry both axes in one event.

## Which device, and when a gesture ends

**macOS.** An NSEvent local monitor (`src/mac_scroll.rs`) reads what SDL
drops:

- `hasPreciseScrollingDeltas`: wheel or trackpad;
- `phase`: fingers began, ended or cancelled;
- `momentumPhase`: coasting began or ended.

It queues them in order with SDL's wheel events.

**Elsewhere** there are no phases, so the client guesses:

- a gesture whose first delta is fractional is a trackpad;
- a gesture ends after 100 ms without deltas.

## Inertia (momentum)

macOS's own momentum deltas are forwarded, so coasting looks and stops as it
does in a Mac window. The danger is double inertia: `finish` (an axis stop)
lets some host toolkits start a kinetic scroll of their own. So `finish` is
sent only when one of these happens:

- the momentum ends (`momentumPhase` ended);
- the fingers lift and no momentum begins within 50 ms;
- the user touches the trackpad again, which stops coasting at once, as on a
  Mac;
- the system cancels the gesture;
- there is a 500 ms safety timeout, in case an end phase is lost.

By then the velocity is close to zero, so the host has nothing left to coast
with. Without phases (Linux clients), `finish` follows the 100 ms timeout, and
for touchpads, which have no momentum of their own, host toolkits may then
add their kinetic scroll.

KWin's fake-input path ignores `finish`. GNOME (mutter) turns it into an axis
stop.

## Direction

SDL's deltas are already in "content" terms. They follow the local natural
scrolling setting, and SDL marks such events `FLIPPED` so games can undo it.
`--scroll-direction`:

- `auto` (default): sends the values as they are. Host content moves exactly
  as in a native window on this computer, whatever the Mac setting.
- `traditional` / `natural`: forces one convention from the device's physical
  direction, whatever this computer does.

KDE's (libinput) natural scrolling is a per-device option of the host's own
touchpads and mice. The portal's virtual pointer and XTest are not libinput
devices, so the host never inverts a second time.

`direction_matrix_never_inverts_twice` tests this matrix: Mac natural on or
off, trackpad or wheel, and each mode. The host setting is constant because
it does not apply to virtual pointers.

The old client inverted `FLIPPED` values. With macOS natural scrolling on,
that made the host scroll the opposite way to every Mac window.

## Verification

**Done:**

- Unit tests:
  - scaling, coalescing and timeouts;
  - the direction matrix;
  - diagonal pans;
  - momentum forwarding with a single late `finish`;
  - touch-to-stop;
  - lift without momentum;
  - wheel during coasting;
  - X11 notch accumulation;
  - protocol round trip and bounds.
- `tests/smoke.rs`: a test host logs a wheel notch, twelve smooth deltas and
  the `finish`.
- `tools/check-macos.sh` type-checks the NSEvent monitor.

**Not verified (no Mac, no real KDE session here):**

- The NSEvent monitor at run time, including the assumption that SDL emits
  exactly one wheel event per non-zero-delta NSEvent, in the same order.
- The macOS momentum timing, and the 50 ms lift grace.
- The 10-points-per-unit scale on a real trackpad.
- How KDE applications feel with `NotifyPointerAxis` pixel values.

**Manual checks on the MacBook:**

1. Two-finger scroll in a long KDE document (Kate, Dolphin, Firefox). It
   should track the fingers with no steps, and diagonal pans should move both
   axes.
2. Flick and let go. The content should coast and stop like a Mac window,
   with no extra glide when the Mac's coasting ends.
3. Flick, then touch the trackpad during coasting. It should stop at once.
4. Toggle System Settings > Trackpad > Natural scrolling. The host should
   always move like Finder does. Try `--scroll-direction traditional` and
   `natural`.
5. Use a notched USB mouse wheel. Each notch should be one step. Move the
   pointer between two panes and scroll at once; the pane under the pointer
   should scroll.
6. Tune `--scroll-speed` (e.g. 0.7 or 1.5) if the distance feels off. F8
   shows the mode ("Scrolling" line).
7. Against an X11 host, the trackpad should scroll in steps of about three
   lines, at a sane rate.
