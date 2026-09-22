# Performance diagnostics

Open **Stats** in the session toolbar, press **F8**, or pass `client --stats`.
The overlay and its shortcut are local; F8 is not injected into the remote desktop.
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
| Decoder recoveries | Pipeline resets after compressed backlog exceeds the age/count/size budget; the entire remaining GOP is skipped and reception resumes at a new keyframe group |
| Stale frames | Output discarded because it was already more than 250 ms old before publication |

No cross-machine wall-clock subtraction is used. Capture latency, one-way network
latency, GPU presentation/scanout, and input-to-photon latency are **not measured**.

With the Linux SDR NV12 renderer path (0.5.4), `Convert / copy` measures the
post-decode raw queue plus any YUV normalization/download and CPU plane copy;
it does **not** include SDL's later YUV-to-RGB rendering. The raw queue retains
only the latest decoded frame; skipped raw frames may leave unmatched receive
ledger entries until their bounded eviction, but cannot corrupt codec references.
The normal NV12 path halves unpadded pixel bytes versus RGB24, while retaining
CPU mapping/copying and GPU upload. This is not a fully GPU-resident pipeline.
Use the renderer/device log and F8 to distinguish GPU-backed OpenGL from CPU
renderers such as llvmpipe. X11 versus Wayland alone does not determine that.
Do not add these partial numbers and label the sum end-to-end latency. A physical
high-speed-camera test remains the acceptance method for input-to-photon latency.
An unchanged desktop may produce fewer captured frames; requested FPS is a cap,
not a promise of unique frames. Values shown for the latest frame are samples,
not percentiles. HDR codec diagnostics are separate from performance telemetry.

Version 0.5.2 recovers when compressed input backlog exceeds 150 ms, 12 frames,
or 8 MiB. A recovery is also reported as pressure to the host's bitrate controller,
even though clearing the queue makes its next instantaneous measurement small.
This prevents stale-input accumulation; it does not guarantee that an overloaded
GPU/CPU can sustain the requested resolution or FPS. Repeated recoveries mean
reduce those settings and compare the same-frame timing stages. A hardware decoder
name does not imply that color conversion, downloads or texture upload are free.
Mac HEVC builds additionally patch VideoToolbox's output buffering to use SPS
reordering requirements. The native session title is deliberately stable; use F8
for changing diagnostics.

In 0.5.3, Linux VA/QSV startup and each restart get up to 750 ms from their first
input frame to initialize, with recovery thresholds of 60 queued frames and
8 MiB even during that interval. Size is checked before each push, so one
additional access unit can temporarily exceed the byte threshold. Existing stale
output filtering still applies. If two resets fail to stabilize the decoder,
the third recovery replaces VA/QSV with the matching software decoder in the
same session, preserving parser, HDR caps, authentication and input connection.
It resumes at a new keyframe, never at dependent frames from the abandoned GOP.
The software path retains the bounded recovery policy: persistent CPU overload
can still end the session with an error rather than accumulating unlimited delay.
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
