# Direct FFmpeg video backend

The default video backend calls libavcodec, libavutil and libswscale directly
through an opaque C ownership boundary. H.264 and H.265 use send/receive APIs;
there are no GStreamer video encoder, decoder or parser elements in this path,
and no spawned FFmpeg process. GStreamer still supplies desktop capture,
host scaling/color conversion, audio, and the appsrc/appsink scheduling bridge.
MoQ framing, authentication, input and the SDL renderer remain unchanged.

Linux auto encoding probes NVIDIA NVENC first, then Intel Quick Sync, VA-API,
and software. Explicit encoder choices do not silently substitute another kind.
Linux decoding probes CUDA, VA-API, Quick Sync, then software; macOS probes
VideoToolbox before software. Hardware selection requires actual decoded output.
Backend names in logs and Stats start with `ffmpeg:`. Software uses libx264 /
libx265 encoding and FFmpeg H.264 / HEVC decoding. The Nix flake links these
libraries on Linux/macOS and enables oneVPL for Quick Sync on x86-64 Linux.

Encoders use closed, short GOPs without B-frames. NVENC also explicitly disables
its output delay. Bitrate changes transactionally reopen the encoder and start
a new keyframe group; accepted bitrate is the configured target, not measured
wire throughput. Decoder recovery flushes references and resumes at a new GOP.
Ten-bit HDR requires HEVC and verified BT.2020/PQ decoded output; SDR cannot be
relabeled as HDR. This does not establish end-to-end HDR capture/display support.

## Trying it

```sh
nix build
./result/bin/teleport
```

Rebuild both ends to use direct encoding and decoding together. Either end can
also interoperate with the legacy backend. For comparison or rollback:

```sh
TELEPORT_VIDEO_BACKEND=gstreamer ./result/bin/teleport
```

Set that variable independently on the host/client being compared. Building does
not update an installed host service. The old GStreamer VideoToolbox patch applies
only to the legacy backend.

## Important limits

This is **not zero-copy**. Hardware-decoded frames are downloaded, normalized
and copied into the existing SDL presentation path. Encoding likewise receives
CPU-addressable captured pixels and uploads when the hardware API requires it.
Direct codecs provide explicit buffer ownership and timing; they do not prove
that the reported Intel UHD 630 black flashes are fixed.

The existing experimental macOS native-surface path
(`TELEPORT_MAC_GPU_VIDEO=1`) currently requires `TELEPORT_VIDEO_BACKEND=gstreamer`.
The direct backend rejects that combination rather than pretending to provide
native surfaces. FFmpeg-to-Metal and Linux DMA-BUF presentation remain future work.

Verified here: software H.264/H.265, Main10 round-trip and SDR rejection,
NVENC and Intel VA-API H.264/H.265 feeding CUDA and Intel VA-API decoders, bitrate reopen/reset,
frame-generation tracking, and legacy/direct interoperability in both directions.
Quick Sync is compiled in, but this host's runtime rejects MFX session creation;
it is not hardware-verified here. Physical macOS, AMD and the affected remote
client still require acceptance tests.
Hardware-specific tests require `TELEPORT_TEST_HARDWARE=1`; they are not simulated
by software fallback.

```sh
nix develop --command cargo test --bin teleport ffmpeg -- --include-ignored
nix develop --command env TELEPORT_TEST_HARDWARE=1 cargo test --bin teleport direct_hardware -- --ignored --nocapture
nix develop --command env TELEPORT_TEST_HARDWARE=1 TELEPORT_TEST_ENCODER=vaapi cargo test --bin teleport direct_hardware -- --ignored --nocapture
nix develop --command cargo test --test ffmpeg_interop -- --ignored --nocapture
```
