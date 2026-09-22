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
| Receive to decoded | Latest complete access unit entering appsrc until decoded pixels are copied to the client frame slot; includes queue, decode, conversion and copy |
| Decoded frame wait | Time in the client frame slot until the render loop begins processing it, before texture upload |
| Decoder queue | Encoded bytes waiting in appsrc; not a measurement of every internal decoder queue |
| Skipped groups | MoQ GOPs superseded, missing, or incomplete; not a packet-loss percentage |
| Superseded frames | Decoded frames replaced in the latest-frame slot before the renderer consumed them |
| Unmatched timestamps | Frames whose exact local receive identity could not be recovered; their receive/decode timing and video generation are unavailable |

No cross-machine wall-clock subtraction is used. Capture latency, one-way network
latency, GPU presentation/scanout, and input-to-photon latency are **not measured**.
Do not add these partial numbers and label the sum end-to-end latency. A physical
high-speed-camera test remains the acceptance method for input-to-photon latency.
An unchanged desktop may produce fewer captured frames; requested FPS is a cap,
not a promise of unique frames. Values shown for the latest frame are samples,
not percentiles. HDR codec diagnostics are separate from performance telemetry.

Quality changes restart the video encoder for that authenticated session and
release held input. A MoQ video-group barrier accompanies the updated desktop
metadata; pre-change frames cannot unlock input or satisfy the configured-frame
check, including same-size display switches. Unknown frame timestamps after a
barrier fail closed rather than guessing a generation. A per-frame reference
timestamp preserves receive identity separately from decoder-adjusted presentation
timestamps; the decoder probe verifies that metadata survives conversion. Old clients remain on
their existing SDR path; old hosts do not support the new quality controls or
host telemetry. Re-pairing is not required when retaining host identity.
