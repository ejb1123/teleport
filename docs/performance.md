# Performance diagnostics

The default [direct FFmpeg backend](ffmpeg.md) reports `ffmpeg:` codec names.
Its parse/decode/download interval includes the direct codec call, hardware
download, pixel normalization and packed output copy. The later convert/copy
interval covers Rust-side frame publication; these are not pure GPU timings.
The raw-output-pad descriptions below apply to the legacy GStreamer backend.

Open **Stats** in the session toolbar, press **F8**, or pass `client --stats`.
The overlay is local. F8 (or Stats) steps through three pages, summary with
the graph, session and pipeline, then hides it; each page fits the window and
says when lines are cut. With keyboard capture enabled F8 goes remote, and the
Stats button still controls the overlay.
Measurements reset on reconnect. Rates warm up for one second before reporting.
The overlay also reports codec, actual encoder/decoder names, configured bitrate,
and stream dimensions. H.265 is available independently of the experimental HDR
mode; SDR is the default. Hardware decoder probes occur before connecting to avoid delaying control
heartbeats, and cache their selection for that client process.

| Measurement | What it actually measures |
| --- | --- |
| Control RTT | Local monotonic time from queuing an authenticated probe until its matching reply is handled; includes host/client scheduling and ordered control queues |
| Video Mbps | Encoded access-unit bytes received per interval, excluding QUIC/TLS/control/audio overhead |
| Received / decoded / displayed FPS | Counts at each corresponding client stage; displaying a frame is not proof it scanned out |
| Stream / local display | The host's current stream size and rate and how they were set (negotiated, requested from an older host, or host settings); this window's drawable size, its display's refresh rate as SDL reports it, and the presentation sync (adaptive vsync, immediate, or `--vsync`). A line appears when the display refreshes slower than the stream: the extra frames are decoded but never seen. Whether VRR is active is not visible to the client |
| Host encoder time | Latest matched encoder sink/output PTS interval, including buffering inside that encoder; unavailable if timestamps do not match |
| Receive to ready | Latest complete access unit entering appsrc until decoded pixels have been copied for the client frame slot; includes queue, decode, conversion and copy |
| Compressed queue | That same frame's appsrc enqueue-to-output interval, before parsing |
| Parse/decode/download | That same frame's appsrc output to decoder raw-output pad; includes parser/driver scheduling, not pure GPU execution |
| Convert / copy | That same frame's raw decoder output to copied display pixels; mapping may also perform deferred GPU downloads |
| Decoded frame wait | Time in the client frame slot until the render loop begins processing it, before texture upload |
| Decoder queue | Encoded bytes waiting in appsrc; not a measurement of every internal decoder queue |
| Skipped groups | MoQ GOPs superseded, missing, or incomplete; not a packet-loss percentage |
| Superseded frames | Decoded frames replaced in the latest-frame slot before the renderer consumed them |
| Unmatched timestamps | Frames whose exact local receive identity could not be recovered; their receive/decode timing and video generation are unavailable |
| Decoder recoveries | Pipeline resets after compressed backlog exceeds the age/count/size budget, or after a decode error; the entire remaining GOP is skipped and reception resumes at a new keyframe group |
| Decode errors | Access units the direct FFmpeg decoder rejected; each triggers one recovery rather than ending the session |
| Stale frames | Output discarded because it was already more than 250 ms old before publication |

The rows above are client-local. The end-to-end rows below are not: they need
the host's clock.

## End-to-end latency (capture → present)

With hosts that advertise `Desktop.frame_timing`, the client declares
`ClientFeatures.frame_timing`. The host then puts a 28-byte header in front
of every video frame: its capture timestamp (the source's buffer time),
when the encoder returned the frame, and when it was handed to MoQ, all on
the host's monotonic clock. It also adds its clock to every `Pong`. Older
peers see neither.

The client estimates host clock − client clock the NTP way. It assumes each
`Probe`/`Pong` round trip is symmetric and keeps the sample with the lowest
round trip of the last 16 (probes go out every 200 ms until 8 are in, then
once a second). Arrival is stamped by the update reader, not the render
loop. The error is at most half that round trip, and less on a symmetric
path. F8 shows that bound as `clock ±`. No new port or trust is involved:
probes already ran over the authenticated control tracks.

| F8 line | Interval | Clock |
| --- | --- | --- |
| Capture to present | host capture → the client's present call returns | both (estimate) |
| capture→encode | source timestamp → encoder output, including colour conversion and the capture queue | host |
| encode→send | encoder output → handed to MoQ (QUIC's send buffer counts as network) | host |
| network | handed to MoQ → whole frame received | both (estimate) |
| receive→decode | whole frame received → decoded picture ready (Receive to ready) | client |
| decode→present | ready → present returned (render-loop pickup, upload, draw, swap) | client |

Each line is p50 / p95 over the last 300 presented frames. The stages add up
to the total. Network and the total carry the clock error, so on loopback
they can read slightly negative. Present is the buffer swap returning, not
scanout: the display's own processing and scanout are not in it.
Refinement re-encodes of a still frame (R12) carry no stamps. The whole-run
p50/p95 go to the log when the window closes (`end-to-end latency summary`)
and into headless smoke summaries (`e2e_*_us`).

`tests/network.rs` checks the estimate against the Unix clock both
processes share on one machine (`TELEPORT_TEST_WALL_CLOCK=1`):
`clock_offset_estimate_matches_the_wall_clock`. Every profile prints an
`E2E` line.

### Glass to glass and input to photon

The software number stops at present. To include the display, and to time
a click, use light:

1. Start a test host that flashes on input:
   `teleport host --source test --flash-on-input --encoder <kind> ...`.
   It streams black and switches to white, then back, on every key or
   mouse-button press it receives.
2. Connect the client full screen on the display under test, with the
   settings you want to measure (VSync, pacing, codec). Turn the host's own
   monitor off or point nothing at it: only the client's screen matters.
3. **Photodiode** (best): a photodiode or phototransistor taped to the
   client screen, into a microcontroller or a scope. Wire the same
   microcontroller as a USB HID mouse (or tap the mouse button with a
   second channel). Each click starts a timer and the light edge stops it:
   click → photon. Repeat 100+ times; report p50/p95 and the display
   model, refresh rate and mode.
4. **High-speed camera** (240–1000 fps phone slow motion works): film the
   mouse button and the screen in one shot. Count frames from the button
   bottoming out to the screen changing. At 240 fps each frame is 4.2 ms,
   so take 30+ clicks.
5. Glass to glass without input: point the camera at a physical stopwatch
   (or a second screen with a running ms counter) shown on the host, and at
   the client's screen in the same shot. This needs a real capture source
   (`--source portal`/`x11`), not the test pattern.

To separate the display, also measure local click → photon with a local app
that flashes on click (same screen and mode), and subtract. The software
part of the path is measured by `flash_on_input_follows_clicks`
(`--smoke-flash` clicks every 400 ms and times until a decoded frame flips)
and by F8.

With the Linux SDR NV12 renderer path (0.5.4), `Convert / copy` measures the
post-decode raw queue plus any YUV normalization/download and CPU plane copy;
it does **not** include SDL's later YUV-to-RGB rendering. The raw queue retains
only the latest decoded frame; skipped raw frames may leave unmatched receive
ledger entries until their bounded eviction, but cannot corrupt codec references.
The normal NV12 path halves unpadded pixel bytes versus RGB24, while retaining
CPU mapping/copying and GPU upload. This is not a fully GPU-resident pipeline.
Use the renderer/device log and F8 to distinguish GPU-backed OpenGL from CPU
renderers such as llvmpipe. X11 versus Wayland alone does not determine that.
Do not add these partial client-side numbers and call the sum end-to-end
latency; use the Capture to present line, which stamps each frame. The
glass-to-glass rig above remains the acceptance method for input-to-photon
latency.
An unchanged desktop may produce fewer captured frames; requested FPS is a cap,
not a promise of unique frames. Values shown for the latest frame are samples,
not percentiles. HDR codec diagnostics are separate from performance telemetry.

Version 0.5.2 recovers when compressed input backlog exceeds 150 ms, 12 frames
(200 ms of frames above 60 FPS), or 8 MiB. A recovery is also reported as pressure to the host's bitrate controller,
even though clearing the queue makes its next instantaneous measurement small.
This prevents stale-input accumulation; it does not guarantee that an overloaded
GPU/CPU can sustain the requested resolution or FPS. Repeated recoveries mean
reduce those settings and compare the same-frame timing stages. A hardware decoder
name does not imply that color conversion, downloads or texture upload are free.
Mac HEVC builds additionally patch VideoToolbox's output buffering to use SPS
reordering requirements. The native session title names the desktop and is
otherwise stable; use F8 for changing diagnostics.

In 0.5.3, Linux VA/QSV startup and each restart get up to 750 ms from their first
input frame to initialize, with recovery thresholds of 60 queued frames and
8 MiB even during that interval. Size is checked before each push, so one
additional access unit can temporarily exceed the byte threshold. Existing stale
output filtering still applies. If two resets fail to stabilize the decoder,
the third recovery replaces VA/QSV with the matching software decoder in the
same session, preserving parser, HDR caps, authentication and input connection.
It resumes at a new keyframe, never at dependent frames from the abandoned GOP.
Every recovery discards the backlog, so delay never accumulates. Since the
2026-10 review, frequent recoveries that still produce fresh frames (a burst
after a network stall, or a marginally overloaded decoder) stutter and lower
the host bitrate instead of ending the session. Five consecutive recoveries
with no fresh decoded frame still end it with an error. A direct FFmpeg decode
error (corrupt or truncated access unit) is counted as **Decode errors** in F8
and handled as one recovery, and the client asks a supporting host for an
immediate keyframe instead of waiting for the next scheduled group.
F8 and the warning log identify the replacement. For a controlled comparison,
choose **Decoder: software** before connecting. This bypasses hardware selection;
the default remains hardware. Mac/NVIDIA selection is unaffected.

Quality changes restart the video encoder for that authenticated session and
release held input. A MoQ video-group barrier accompanies the updated desktop
metadata; pre-change frames cannot unlock input or satisfy the configured-frame
check, including same-size display switches. Unknown frame timestamps after a
barrier fail closed rather than guessing a generation. A per-frame reference
timestamp preserves receive identity separately from decoder-adjusted presentation
timestamps; the decoder probe verifies that metadata survives conversion. Old clients remain on
their existing SDR path; old hosts do not support the new quality controls or
host telemetry. Re-pairing is not required when retaining host identity.

## Warnings and the graph

The client puts a short warning over the top-left of the video when
something degrades, like Parsec's overlay warnings. It decides once a second
from the same data as F8:

| Warning | Raised when |
| --- | --- |
| Network: video lost, expect stutter | MoQ groups skipped (one in two seconds running, or two or more in one), or a decoder reset within a second of lost groups |
| Network: video stalled for N ms | no frame for 500 ms or more, and the frame that ended the gap was held up on the way, came after lost groups, or the host sends no timing |
| Network: delay rising (N ms queued) | the queueing delay the client reports to the host is 50 ms or more for two seconds (100 ms or more at once) |
| Network: stream lowered (link ~N Mbps) | the host steps the resolution or frame rate down |
| Decoder fell behind and skipped ahead | a decoder reset (backlog or decode error) with no network trouble around it |
| Decoding takes N ms per frame, over the F fps budget | receive→ready above the frame interval for two seconds |
| Hardware decoder failed: decoding in software | the mid-session software fallback |
| Host encoder overloaded | the host's encode time above 90% of the frame interval for two seconds |
| Host is encoding in software: higher latency | the host's encoder is x264/x265 |
| Host capture stalled for N ms | no frame for 500 ms or more, but the next one came through as fast as usual and nothing was lost |

A warning stays for 6 s, at most two show at once, a new one appears at
most every 2 s, and the same one repeats at most every 30 s. Every raised
warning is also logged (`session warning`), including in headless runs, so
`tests/network.rs` prints them per profile (`WARN` lines). `--no-warnings`
(launcher: **Warnings off**) hides them; they are still logged.

F8 starts with a rolling graph of the last 240 presented frames: frame time
(present to present), capture→present latency (hosts with frame timing) and
video bitrate in quarter seconds. The labels show the window's mean.

## Frame pacing

`--pacing latency` (the default; launcher **Pacing: lowest latency**) shows
each frame the moment it is decoded. On a jittery link frames then reach
the screen unevenly, even though the host captured them evenly.

`--pacing smooth` (launcher **Pacing: smooth**) puts a small jitter buffer
between the decoder and the screen. For every frame it knows the host's
capture time (frame timing, see above) and when the frame was decoded here;
the difference minus the fastest recent one is that frame's lateness. The
buffer is the 95th percentile of that lateness over the last 120 frames,
capped at three frame intervals and 50 ms. Each frame is shown at its
capture time plus the fastest transit plus the buffer, so frames come out
at the cadence they were captured, whatever the stream's rate. A frame that
arrives after its slot is shown at once and counted as late. The render
loop sleeps until the next slot (sub-millisecond, not SDL's 1 ms wait).
Smooth pacing needs a host with frame timing; with older hosts it falls
back to lowest latency and F8 says so. It applies to the main window, not
to extra monitor windows, and it works with or without `--vsync`.

F8 shows the mode, the current buffer and the standard deviation of frame
times (present to present). Window close logs a `frame pacing summary`;
headless runs report `frame_time_*_us` and `paced_held_*_us`, which
`tests/network.rs` prints as `PACE` lines (`wifi_like_link` against
`wifi_like_link_with_smooth_pacing`).
