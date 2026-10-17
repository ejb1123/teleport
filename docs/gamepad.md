# Gamepads

Game controllers plugged into the client appear on a Linux host as virtual
pads: PlayStation 4 and 5 controllers as a DualShock 4 or DualSense (when
the host can create them), everything else as an Xbox 360 pad. Nothing
needs to be turned on in the client: once the host supports gamepads,
connected controllers are forwarded automatically. The session notice shows
"Gamepad N connected: …" when a controller is attached.

- The client reads controllers through SDL2's GameController API. Any
  controller SDL knows (Xbox, PlayStation, Switch Pro, 8BitDo, Steam
  Controller, …) is forwarded using the Xbox layout. SDL's mapping decides
  which physical button is "A".
- Up to 4 controllers per session, in the order they were attached. A fifth
  is ignored and the client shows a notice.
- Xbox pads: the host creates one `/dev/uinput` device per controller, named
  `Teleport X-Box 360 pad N` with USB ID `045e:028e`. It uses the same evdev
  codes and ranges as the kernel `xpad` driver: sticks −32768..32767,
  triggers 0..255, d-pad as `ABS_HAT0X/Y`. SDL, Steam Input, Proton and evdev
  games treat it like a wired Xbox 360 pad.
- PlayStation pads: the host creates a `/dev/uhid` HID device with the
  genuine USB report descriptor and ID of a wired DualShock 4 (`054c:05c4`)
  or DualSense (`054c:0ce6`). The kernel's `hid-playstation` driver binds
  it as it would a real controller: gamepad, touchpad and motion-sensor
  evdev nodes, lightbar and player LEDs, force feedback and a hidraw node.
  Games see a PlayStation controller with PlayStation button prompts, and
  Steam Input or SDL can use the touchpad and gyro.
- Which pad: `--gamepad-type auto|xbox|ds4|dualsense` on the client
  (launcher: **Gamepad:** in Connection settings). `auto` (the default)
  picks the DualShock 4 for controllers SDL reports as PS4 and the DualSense
  for PS5 ones, the Xbox pad otherwise. A host without uhid access, or an
  older host, creates Xbox pads.
- Rumble: a game's vibration on the virtual pad plays on the client's
  controller (`SDL_GameControllerRumble`), strong motor as SDL's low-frequency
  one. Controllers without motors ignore it.
- PlayStation pads also forward the touchpad (two contacts, and its click),
  the gyro and the accelerometer, and play back the lightbar colour, the
  DualSense player LEDs (as SDL's player index) and DualSense
  adaptive-trigger effects (`SDL_GameControllerSendEffect`, on a DualSense
  only).
- Controller input reaches the host only while a session window has focus.
  When the session loses focus, or Ctrl+Alt+K or a toolbar click releases
  input, every pad button is released, every axis centred and every
  touchpad finger lifted. The pads stay attached, so a game does not see a
  controller unplug. Unplugging a controller removes its pad. Ending the
  session (disconnect, reconnect, host shutdown) removes every pad.

## Host setup

The host offers gamepads (`Desktop::gamepads`) only when the host user can
write `/dev/uinput`. Otherwise the client reports "Host cannot accept
gamepads" and keeps the controllers local. It offers PlayStation pads
(`Desktop::gamepad_types`) only when the user can also read and write
`/dev/uhid`. Check with:

```sh
ls -l /dev/uinput /dev/uhid; getfacl /dev/uinput /dev/uhid; id
```

**NixOS** (module option, which enables `hardware.uinput`, adds
`services.teleport-desktop.users` to the `uinput` group, loads `uhid` and
gives that group `/dev/uhid`):

```nix
services.teleport-desktop.gamepads = true;
# Xbox pads only, without /dev/uhid access:
# services.teleport-desktop.playstationGamepads = false;
```

To set it up without the module option:

```nix
hardware.uinput.enable = true;
users.users.<name>.extraGroups = [ "uinput" ];
boot.kernelModules = [ "uhid" ];
services.udev.extraRules = ''
  KERNEL=="uhid", SUBSYSTEM=="misc", GROUP="uinput", MODE="0660", OPTIONS+="static_node=uhid"
'';
```

**Other distributions:** add udev rules, e.g.
`/etc/udev/rules.d/60-teleport-uinput.rules`:

```
KERNEL=="uinput", SUBSYSTEM=="misc", GROUP="input", MODE="0660", OPTIONS+="static_node=uinput"
KERNEL=="uhid", SUBSYSTEM=="misc", GROUP="input", MODE="0660", OPTIONS+="static_node=uhid"
```

Then add the host user to the `input` group, run `modprobe uinput uhid`
(and load them at boot), and `udevadm control --reload && udevadm trigger`.
Some distributions and Steam's udev rules already grant the active seat user
access to `/dev/uinput` through an ACL (`TAG+="uaccess"`). `getfacl` then
lists the user, and nothing else is needed for Xbox pads.

The PlayStation pads also need the `hid-playstation` module (udev loads it
when a pad appears). It drives the DualShock 4 since Linux 6.2; older
kernels bind the DualShock 4 with `hid-sony`, which is untested. Games reading
the pads through hidraw (Steam Input, SDL's HIDAPI driver) need access to
the created `/dev/hidraw*` node, which Steam's udev rules
(`hardware.steam-hardware.enable` on NixOS) grant for Sony pads; through
evdev they work like any controller.

A new group membership takes effect only for new login sessions. The host
runs as a systemd user unit, so log out completely (or reboot) before
expecting `gamepads` to be advertised. Restarting the unit alone keeps the
old groups of the running user manager.

Write access to `/dev/uinput` or `/dev/uhid` lets a process create any
input device, including keyboards. Grant it only to accounts that already
run the Teleport host, which can already inject keyboard and pointer input
into that desktop.

## Wire protocol

- `GamepadConnected { pad, name, kind }` / `GamepadDisconnected { pad }`:
  `pad` < 4, `name` ≤ 128 bytes without control characters (used only in
  host logs). `kind` is `xbox` (left out on the wire), `ds4` or
  `dualsense`, and only a type in `Desktop::gamepad_types`; older hosts
  ignore it and create an Xbox pad.
- `GamepadButton { pad, button, down }`: SDL2 `SDL_GameControllerButton`
  numbers (< 21). Misc, paddles and touchpad click have no Xbox 360 equivalent
  and are ignored by the host for Xbox pads; PlayStation pads map touchpad
  click and (DualSense) MISC1 to the microphone button. Never dropped.
- `GamepadAxis { pad, axis, value }`: SDL2 `SDL_GameControllerAxis` (< 6) with
  SDL's i16 values; triggers must be ≥ 0. The client coalesces motion to the
  latest value per axis once per loop iteration. Axis events are marked
  lossy (`Event::is_lossy`), so a full input queue drops them instead of
  ending the session. The client keeps an unsent axis dirty and retries it
  on the next iteration, so the final position (e.g. a stick back at
  centre) is delayed under backpressure but never lost.
- `GamepadTouch { pad, finger, down, x, y }` (finger < 2, SDL's normalised
  0..=1 position) and `GamepadMotion { pad, gyro, accel }` (SDL units:
  rad/s and m/s², |value| ≤ 1000): only for PlayStation pads. Lossy and
  coalesced like axes, and retried the same way, so a lifted finger is
  never lost. The gyro is averaged over the samples since the last send,
  which keeps the total rotation when samples are coalesced.
- Older hosts reject unknown events, so the client sends none of these unless
  the host advertises `gamepads`, and touch and motion only for pads it
  connected with a PlayStation `kind`. The `--source test` host advertises
  every type and accepts the events as no-ops.
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
- PlayStation pads: the client declares `ClientFeatures { gamepad_lights:
  true }` when the host offers a PlayStation type. A thread per uhid pad
  answers the driver's feature-report reads (calibration, address,
  firmware; its probe fails without them), repeats the current input report
  every 100 ms like a real pad's stream, and parses output reports from the
  driver or from games writing hidraw (USB and Bluetooth layouts): motor
  bytes become `Update::Rumble` (until changed, re-sent every 30 s), the
  lightbar and player LEDs `Update::GamepadLights { pad, red, green, blue,
  player }`, DualSense trigger blocks `Update::GamepadTriggers { pad, right,
  left }` (the 11-byte effects as in the output report). The `--source
  test` host answers a PlayStation connect with player-1 lights and a
  DualSense press with a trigger effect.
- Report format (`src/gamepad_ps.rs`): 64-byte USB input report 1 as
  `hid-playstation` parses it. Motion uses 1/16 °/s and 1/8192 g, and the
  calibration report the pad answers makes the driver's (and SDL's)
  calibration exact on those units; the driver then reports 1/1024 °/s
  and 1/8192 g on evdev. Touch positions are 1920×942 (DS4) or 1920×1080
  (DualSense), and each new contact gets a new tracking ID. Each pad has
  its own random-looking address: the driver refuses two pads with the
  same one.

## Limits

- Linux hosts only. On macOS or Windows hosts the client never sends
  gamepad events, because only Linux hosts advertise `gamepads`.
- Xbox pads: rumble only (no trigger rumble, other force-feedback effects,
  LEDs, motion sensors, touchpad or paddles). Effect delays
  (`replay.delay`) and gain are ignored.
- PlayStation pads: no DualSense haptics (audio-driven vibration), speaker,
  microphone or headset jack; no DualSense Edge paddles. Adaptive-trigger
  effects play only on a DualSense on the client (SDL has no trigger API
  for other controllers; Xbox impulse triggers are not driven). Player
  LEDs play only as SDL's player index, so only the five standard
  patterns. The host's battery is always reported full and charging.
  Bluetooth reports from games are parsed, but the pad itself is a USB
  pad.
- An Xbox or Switch controller forced to `ds4`/`dualsense` gets PlayStation
  prompts and the motion sensors SDL reports for it (none for most Xbox
  controllers), but no touchpad.
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
- PlayStation pads: `gamepad::ps::tests` (descriptors and report sizes;
  button, hat, stick, trigger, touch and motion packing; calibration
  checked against the driver's arithmetic; output-report parsing for USB
  and Bluetooth, driver and game reports), `gamepad::uhid::tests` (uhid
  event layout), `client::controllers::tests` (type choice, fallback to
  Xbox, touch and motion coalescing and retry), and two ignored tests that
  need `/dev/uhid`. `tools/uhid-vm-test.sh <test binary>` runs them as root
  in a throwaway KVM guest booting the host's NixOS kernel (verified on
  Linux 7.2):
  - `uhid_playstation_pads_bind_hid_playstation`: for each type,
    `hid-playstation` binds and creates the gamepad, touchpad and motion
    nodes; a button, stick, trigger, touch, touchpad click, gyro and
    accelerometer reading come back from evdev with the driver's values;
    the driver's lightbar/player-LED setup, a game's `EVIOCSFF` rumble and
    a game's hidraw output report (lightbar, player LEDs, trigger effects)
    reach the client side; the device goes away with the session.
  - `sdl_reads_uhid_pads_as_playstation_controllers`: SDL (sdl2-compat on
    SDL3, through HIDAPI) sees "DualSense Wireless Controller" / "PS4
    Controller" with gyro and accelerometer, reads the forwarded button,
    touch position and gyro (rad/s within 0.01), and its `SetLED`,
    rumble and DualSense trigger effect arrive at the client side.
  Run on a host without `/dev/uhid` access they print "SKIPPED" and pass.
- `tests/smoke.rs` (`--smoke-input`) sends the gamepad events end to end over
  QUIC to a `--source test` host, including a DualSense with touch and
  motion, and checks that its rumble, lights and trigger effects come back.
- Not covered by tests: a physical controller on the client (its motors,
  lightbar and triggers actually responding, its touchpad and gyro feeding
  the session), or a game or Steam Input reading the pads on a real host.
