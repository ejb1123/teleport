# Virtual displays and privacy mode (Linux)

Teleport's answer to Parsec's virtual display driver (parsec-comparison G17,
section 1.20): virtual monitors for headless hosts and as extra screens, and a
privacy mode that turns the host's monitors off during a session. On Linux the
compositor provides virtual outputs through the same desktop portal the host
already uses, so no driver is installed.

## Options

| Where | Option | Effect |
| --- | --- | --- |
| Host | `--virtual-display fallback` (default) | Stream the monitors; when the portal has none to share (headless, or every monitor disconnected while asleep), stream a virtual display sized to the client. Clients may add a virtual monitor. |
| Host | `--virtual-display always` | Always stream a virtual display sized to the client (a headless server, or exact resolution matching). |
| Host | `--virtual-display never` | No virtual outputs (the behaviour before this change). |
| Host | `--privacy-mode off` (default) / `request` / `always` | Privacy mode for sessions that ask for it, or for every session. |
| Client | `--privacy` | Ask for privacy mode (hosts with `--privacy-mode request`). |
| Client | `--virtual-monitor` | Ask for an extra virtual monitor sized to this display. |
| Launcher | **Host screen: as is / private / + virtual monitor** | The two client options, per connection. |
| NixOS | `services.teleport-desktop.virtualDisplay`, `.privacyMode` | The host options; `privacyMode != "off"` also adds `ExecStopPost = teleport privacy-restore`. |
| Anyone | `teleport privacy-restore [--state FILE] [--all]` | Turn monitors left off by privacy mode back on (see below). |

Unattended use needs the portal pre-authorization
(`services.teleport-desktop.preauthorizeRemoteDesktop`, docs/distribution.md):
every virtual output is its own RemoteDesktop session, created without a local
dialog only when `dev.teleport.Host` is pre-authorized.

## How it works

- **Source.** The host asks the ScreenCast portal for source type VIRTUAL
  (`AvailableSourceTypes` must include it) inside a RemoteDesktop session.
  xdg-desktop-portal-kde 6.7 then has KWin create the output
  `Virtual-virtual-xdp-kde-dev.teleport.Host` at 1920x1080, scale 1
  (`zkde_screencast_unstable_v1.stream_virtual_output`); it exists as long as
  the portal session. Closing the session, the host exiting, or its D-Bus
  connection dropping (crash, `SIGKILL`) removes it.
- **Size.** KWin's virtual outputs take custom modes, so the host gives it the
  client's display (`ClientDisplay`: physical pixels, refresh rate, clamped to
  7680x4320 and 30–240 Hz) with `kscreen-doctor
  output.N.addCustomMode.W.H.mHz.full` and `output.N.mode.ID`, before and
  during the session. With `always` this is exact resolution matching: the
  negotiated stream is the output's native size. At most four custom modes
  are kept per output.
- **Input.** Absolute pointer motion goes to the virtual output's PipeWire
  node through the same RemoteDesktop session (KDE offsets it by the output's
  geometry); keys and buttons are global.
- **Fallback.** A monitor session that the portal fails (KDE answers "Other"
  when it has no screen) or that shares no monitor falls back to a virtual
  output; a dismissed dialog does not. Virtual outputs of other apps are not
  counted as monitors. During a session the host checks every 5 s
  (`kscreen-doctor`) whether a monitor is back and then streams it instead
  (video restarts once; at most one try a minute while the portal still
  refuses). Between sessions the fallback output is released, so a returning
  user never finds an invisible screen beside the real ones.
- **Extra monitor.** `ClientFeatures.virtual_monitor` adds a portal session
  of its own whose output becomes the last entry of `Desktop.monitors`
  (`Monitor.virtual_display`), streamed like any other monitor
  (docs/multi-monitor.md) and kept at the client's display size. It is
  removed when the session ends or its portal session is recreated (and then
  added again).
- **Protocol.** Capability-gated as usual: `Desktop.virtual_monitor`,
  `Desktop.privacy_mode`, `Desktop.privacy_active`, `Monitor.virtual_display`,
  `ClientFeatures.privacy` and `.virtual_monitor` (older peers ignore them).

### KDE limitations found

- xdg-desktop-portal-kde names every virtual output of an app the same
  (it checks for a collision without KWin's `Virtual-` prefix), and KWin
  derives the same UUID from that name. Two of them cannot be told apart, so
  the host never creates a second one: an extra virtual monitor is offered
  only next to physical monitors, not on top of a virtual primary display.
  If another host on the machine already has one, the new one keeps its
  1920x1080.
- KWin stores a layout per set of outputs, and all Teleport virtual outputs
  are the "same" output to it. After a privacy session that ended without
  restoring (host killed), the stored layout for "monitors + Teleport virtual
  output" has the monitors off, and the next virtual output would apply it.
  The host therefore turns back on any monitor that was on before it created
  a non-privacy virtual output (seen and fixed in the nested test).
- With every monitor disconnected, KDE's portal refuses to start a session
  when Qt reports no screen at all (`RemoteDesktopPortal::Start`). Whether a
  headless KWin on DRM (no connector) still offers a Qt placeholder screen,
  and so a virtual output, is **untested**; a KWin with at least one output
  works.
- KWin turns DPMS back on when an output is added; adding a virtual output
  can wake sleeping monitors (on hosts whose monitors stay connected while
  asleep).

### GNOME

GNOME's portal (Mutter `RecordVirtual`) offers the same VIRTUAL source type,
so `fallback` and `always` take the same code path. **Untested.** There is no
`kscreen-doctor`, so the output keeps the size Mutter chooses (it follows the
consumer's format negotiation, which the host does not drive), the monitor
switch-back check does not run, and privacy mode is not offered.

## Privacy mode

While a session lasts the physical monitors are **off (disabled outputs)**
and the session streams a virtual output; someone at the desk sees nothing.
Only an owner session that leads the stream may ask for it (or for a
virtual monitor); guests cannot ([guests](guests.md)). The host must opt in
(`--privacy-mode`), and with `request` the client must ask. Privacy mode
lasts while sessions run: if its owner leaves while guests stay
(`--keep-guests-without-owner`), the monitors stay off until the last one
leaves.

Order of operations (`host::enter_privacy`):

1. Save the enabled physical outputs (name, position, mode, scale, priority)
   to a private state file (`privacy-restore.json` in the identity directory,
   else `$XDG_RUNTIME_DIR/teleport-privacy-<port>.json`) and start the guard.
2. Switch capture to a virtual output sized to the client.
3. Disable the saved outputs in one `kscreen-doctor` call and check that none
   is still on.

Any failure restores the monitors and returns to them, with a notice to the
client. Every 2 s the host turns off monitors that came back (plugged in,
woken by a hotplug) and adds them to the state file.

### Why not DPMS

DPMS-off (`kscreen-doctor --dpms off`, KWin's DPMS protocol) is the obvious
first idea and was rejected:

- KWin wakes DPMS on any input, including the remote user's injected input,
  so the screens would turn on at the first mouse move;
- on this machine waking from DPMS is a DRM hotplug that destroys portal
  sessions (the host recreates them, but privacy would flap with it);
- nothing would stop the local user from waking them either.

Disabled outputs take no input wake-up and survive the hotplug, and with a
virtual output in the session the desktop stays usable remotely. The
trade-off is that windows move to the virtual output while private, and the
local monitors show "no signal" (some monitors then sleep and drop off the
bus; they come back with the stored layout when they reconnect).

### Restore, in layers

The monitors come back on:

1. **Disconnect, error or cancelled session.** The session's `Privacy` value
   restores when dropped, however `run_session` ends, while the virtual output
   still exists (so KWin stores the layout with the monitors on), then the
   host closes the virtual output and streams the monitors again.
2. **Host exits, crashes or is killed.** The guard (`teleport privacy-guard`,
   a child process fed one heartbeat a second over a pipe) restores on EOF,
   on SIGTERM/SIGINT/SIGHUP, and retries for 10 s.
3. **Host hangs.** The guard restores after 45 s without a heartbeat
   (longer than the slowest portal recovery).
4. **Unit stopped.** `ExecStopPost = teleport privacy-restore` (NixOS
   module) after systemd has killed everything, and the host also restores
   from a leftover state file when it starts.
5. **KWin itself.** When the virtual output disappears KWin applies its
   stored layout for the remaining outputs, which has the monitors on.
6. **By hand.** `teleport privacy-restore` (from SSH or a text console) uses
   the state file; `--all` turns on every connected monitor that is off.

The state file is written atomically with mode 0600, refused if it is a
symlink or not the user's, and deleted only after a restore that left no
saved monitor off.

## Tests

- Unit (`cargo test`): source planning and fallback decisions
  (`virtual_display::tests`), which outputs privacy turns off and how they
  are restored, mode selection and custom-mode bounds, size clamping, and
  every privacy exit path against a fake `kscreen-doctor` (normal end,
  dropped session, crashed host with `privacy-restore`, guard on EOF, guard
  on missing heartbeats, guard idle while heartbeats arrive).
- Integration (`cargo test --test virtual_display -- --ignored
  --test-threads=1`): each test starts its own headless KDE desktop with
  `tests/support/nested-kwin.sh` (KWin `--virtual`, private D-Bus, PipeWire,
  WirePlumber without hardware monitors, permission store, both portals,
  private HOME/XDG dirs; never the user's session) and checks, through a real
  QUIC client:
  - `--virtual-display always`: the output is resized to the client
    (1600x900@75), video flows, and absolute motion moves KWin's pointer to
    the expected place on it (read back from the screencast cursor metadata);
    killing the host removes the output;
  - default `fallback` beside a monitor: an extra virtual monitor at the
    client's size, streamed, with input, removed on disconnect;
  - privacy: two "physical" outputs off and back on after disconnect; a
    stopped (hung) host's guard turns them on; a killed host leaves nothing
    off; a later virtual display keeps them on despite KWin's stored layout.
  Skipped when KWin, the KDE portal, PipeWire, WirePlumber or
  `kscreen-doctor` are missing.

Not verified: a real headless machine (no connector), GNOME, NVIDIA/VA-API
zero-copy from a DRM virtual output (the nested tests use the software
encoder), privacy on physical monitors with the hotplug behaviour described
above, and HDR on a virtual output.
