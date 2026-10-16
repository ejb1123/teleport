# Gamepads

Game controllers plugged into the client appear on a Linux host as virtual
Xbox 360 pads. Nothing needs to be turned on in the client: once the host
supports gamepads, connected controllers are forwarded automatically. The
session notice shows "Gamepad N connected: …" when a controller is attached.

- The client reads controllers through SDL2's GameController API. Any
  controller SDL knows (Xbox, PlayStation, Switch Pro, 8BitDo, Steam
  Controller, …) is forwarded using the Xbox layout. SDL's mapping decides
  which physical button is "A".
- Up to 4 controllers per session, in the order they were attached. A fifth
  is ignored and the client shows a notice.
- The host creates one `/dev/uinput` device per controller, named
  `Teleport X-Box 360 pad N` with USB ID `045e:028e`. It uses the same evdev
  codes and ranges as the kernel `xpad` driver: sticks −32768..32767,
  triggers 0..255, d-pad as `ABS_HAT0X/Y`. SDL, Steam Input, Proton and evdev
  games treat it like a wired Xbox 360 pad.
- Rumble: a game's vibration on the virtual pad plays on the client's
  controller (`SDL_GameControllerRumble`), strong motor as SDL's low-frequency
  one. Controllers without motors ignore it.
- Controller input reaches the host only while a session window has focus.
  When the session loses focus, or Ctrl+Alt+K or a toolbar click releases
  input, every pad button is released and every axis centred. The pads stay
  attached, so a game does not see a controller unplug. Unplugging a
  controller removes its pad. Ending the session (disconnect, reconnect, host
  shutdown) removes every pad.

## Host setup

The host offers gamepads (`Desktop::gamepads`) only when the host user can
write `/dev/uinput`. Otherwise the client reports "Host cannot accept
gamepads" and keeps the controllers local. Check with:

```sh
ls -l /dev/uinput; getfacl /dev/uinput; id
```

**NixOS** (module option, which enables `hardware.uinput` and adds
`services.teleport-desktop.users` to the `uinput` group):

```nix
services.teleport-desktop.gamepads = true;
```

To set it up without the module option:

```nix
hardware.uinput.enable = true;
users.users.<name>.extraGroups = [ "uinput" ];
```

**Other distributions:** add a udev rule, e.g.
`/etc/udev/rules.d/60-teleport-uinput.rules`:

```
KERNEL=="uinput", SUBSYSTEM=="misc", GROUP="input", MODE="0660", OPTIONS+="static_node=uinput"
```

Then add the host user to the `input` group, run `modprobe uinput` (and load
it at boot), and `udevadm control --reload && udevadm trigger`. Some
distributions and Steam's udev rules already grant the active seat user
access through an ACL (`TAG+="uaccess"`). `getfacl /dev/uinput` then lists
the user, and nothing else is needed.

A new group membership takes effect only for new login sessions. The host
runs as a systemd user unit, so log out completely (or reboot) before
expecting `gamepads` to be advertised. Restarting the unit alone keeps the
old groups of the running user manager.

Write access to `/dev/uinput` lets a process create any input device,
including keyboards. Grant it only to accounts that already run the
Teleport host, which can already inject keyboard and pointer input into
that desktop.

## Wire protocol

- `GamepadConnected { pad, name }` / `GamepadDisconnected { pad }`: `pad` <
  4, `name` ≤ 128 bytes without control characters (used only in host logs).
- `GamepadButton { pad, button, down }`: SDL2 `SDL_GameControllerButton`
  numbers (< 21). Misc, paddles and touchpad click have no Xbox 360 equivalent
  and are ignored by the host. Never dropped.
- `GamepadAxis { pad, axis, value }`: SDL2 `SDL_GameControllerAxis` (< 6) with
  SDL's i16 values; triggers must be ≥ 0. The client coalesces motion to the
  latest value per axis once per loop iteration. Axis events are marked
  lossy (`Event::is_lossy`), so a full input queue drops them instead of
  ending the session. The client keeps an unsent axis dirty and retries it
  on the next iteration, so the final position (e.g. a stick back at
  centre) is delayed under backpressure but never lost.
- Older hosts reject unknown events, so the client sends none of these unless
  the host advertises `gamepads`. The `--source test` host advertises it and
  accepts the events as no-ops.
- Rumble: the client declares `ClientFeatures { rumble: true }` (older hosts
  ignore the field; older clients never declare it, and would reject the
  update). Pads the host creates after that offer `EV_FF`/`FF_RUMBLE` with 16
  effect slots, like `xpad`. A thread per pad polls the uinput fd, answers
  `UI_FF_UPLOAD`/`UI_FF_ERASE` (`UI_BEGIN_*`/`UI_END_*`; a game blocks in
  `EVIOCSFF` until then; non-rumble effects are refused) and tracks `EV_FF`
  play/stop. Playing effects add up (saturating); the host sends
  `Update::Rumble { pad, low, high, duration_ms }` on the updates track when
  the motors change, when an effect is (re)played and when effects end.
  `duration_ms` is the time left, at most 65535 ms (SDL's limit); effects
  without an end are re-sent every 30 s. Zeros stop the motors, also when a
  pad is removed. The `--source test` host answers each pad button press
  with a 100 ms rumble so the update path can be tested.

## Limits

- Linux hosts only. On macOS or Windows hosts the client never sends
  gamepad events, because only Linux hosts advertise `gamepads`.
- Rumble only (no trigger rumble, other force-feedback effects, LEDs,
  motion sensors, touchpad or paddles). Effect delays (`replay.delay`) and
  gain are ignored.
  Every controller appears as an Xbox 360 pad, so games show Xbox button
  prompts.
- No per-controller mapping beyond SDL's own database. SDL's
  `SDL_GAMECONTROLLERCONFIG` environment variable on the client can add
  mappings.
- Gamepad events share the reliable input track with keys, so heavy packet
  loss delays them the same way.

## Verification

- Unit tests: protocol bounds and serde (`protocol::tests::gamepad_*`), SDL →
  evdev mapping, hat/d-pad, neutralise, test-backend no-ops
  (`gamepad::tests`), client slotting, coalescing and retry after a full
  queue (`client::controllers::tests`).
- `cargo test --bin teleport -- --ignored uinput_pad_reaches_evdev
  sdl_reads_virtual_pad` needs `/dev/uinput` and `/dev/input/event*`
  access. It creates a real pad, reads the events back from evdev, checks
  that release-all and destroy work, and checks through SDL that every
  forwarded button and axis comes back as the same SDL button and axis.
- Rumble: `gamepad::tests::rumble_effects_add_up_end_and_stop` (effect
  bookkeeping), `uinput_pad_rumble_reaches_the_client` (ignored, needs
  `/dev/uinput`: a game's `EVIOCSFF` upload, `EV_FF` play and stop and
  `EVIOCRMFF` on the evdev node reach the session as `Rumble`, and pads made
  without rumble offer no force feedback) and
  `sdl_rumble_on_the_virtual_pad_reaches_the_client` (ignored: SDL sees
  rumble on the virtual pad and `SDL_GameControllerRumble` arrives with the
  same strengths). `client::controllers::tests` covers finding the client
  controller for a pad.
- `tests/smoke.rs` (`--smoke-input`) sends the gamepad events end to end over
  QUIC to a `--source test` host and checks that its rumble comes back.
- Not covered by tests: a physical controller on the client (its motors
  actually turning), or a game reading the pad on a host.
