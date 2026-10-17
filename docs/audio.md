# Audio

## Desktop audio

Off by default. Start the host with `--audio-source YOUR_OUTPUT.monitor`
(`pactl list short sources` lists them; only `.monitor` names are accepted,
never a microphone). The host sends 10 ms Opus packets, one per MoQ group;
the client times them by sequence number with an adaptive playout delay and
conceals missing ones (docs/review-2026-10.md, M8). **Audio** in the session
toolbar or `--mute` mutes playback.

## Microphone (G20)

The client can send its microphone to the host, where applications such as
Discord or a game record it from an input named **Teleport Microphone**.

- Host: `teleport host --microphone` (NixOS: `services.teleport-desktop.microphone`).
  It needs PipeWire (`pw-cli`, included in the Nix package). The host offers
  it to every paired client; there are no guest roles yet, so every paired
  client is an owner.
- Client: `teleport client --mic`, or **Microphone on** in the launcher's
  connection settings. `--mic-muted` starts muted; **Ctrl+Alt+V**
  (Control+Option+V on macOS) mutes and unmutes; F8 shows the state. On
  Linux `--mic-device NAME` picks a PulseAudio/PipeWire source (`pactl list
  short sources`); otherwise the default input is used (`osxaudiosrc` on
  macOS).

The client sends 10 ms mono Opus (voice mode, 48 kbit/s, in-band FEC) on the
`microphone` track; nothing is sent while muted. The host plays it with the
same sequence-timed playout and packet-loss concealment as desktop audio.

### The virtual microphone

While a client's microphone is on, the host creates two PipeWire objects:

- a null sink, "Teleport microphone feed" (`teleport_microphone_sink.PID.N`),
  which the client's audio is played into, never anywhere else (the stream
  may not fall back or move to another device);
- a loopback whose output is an `Audio/Source`, "Teleport Microphone"
  (`teleport_microphone.PID.N`), the device applications record.

Both have the lowest session priority, so PipeWire never makes them the
default while a real device exists. A `pw-cli` child of the host owns them;
they are removed at session end, when the host exits, and when it crashes
(the child gets a parent-death signal). A host started with `--microphone`
also removes any left by a host process that no longer runs.

### Echo cancellation

When the host also streams its audio, the client's speakers can feed its own
microphone, and the host would hear itself. With `--mic` the client cancels
that echo with WebRTC's canceller (GStreamer `webrtcdsp`, fed by a
`webrtcechoprobe` in the desktop-audio player); `--no-echo-cancel` or the
launcher's **Mic on, no echo cancel** turns it off. It is used only where
the plugins exist (in the Nix package on Linux; on macOS when the GStreamer
build includes them). Noise suppression and gain control are left to the
application on the host.

Measured (`microphone_echo_cancellation` in tests/audio_latency.rs): with
the client's playback (a continuous 440 Hz tone) looped digitally into its
microphone, the echo drops from −23 dBFS to −65…−90 dBFS (42–67 dB, four
runs). Double talk is weak: while that tone plays, only 0–6 of 12 short
near-end ticks got through (WebRTC's suppressor; delay-agnostic mode and
higher suppression levels were worse). With the far end silent all 12 get
through. Turn it off (`--no-echo-cancel`) with headphones.

### Measurements

`microphone_end_to_end_latency` (tests/audio_latency.rs) plays a tone with a
tick every 500 ms into the client's microphone (a null sink), and records it
and the host's virtual microphone. It also checks that a stale virtual
microphone is removed at host start, that a session end and a killed host
remove theirs, and that no default device changes. Results are in
docs/review-2026-10.md (R17).

### Limits

- Mono only; the virtual microphone is stereo with equal channels.
- The host side needs PipeWire (not PulseAudio alone).
- A real macOS microphone and Discord on the host were not tested.
