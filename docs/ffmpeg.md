# Direct FFmpeg video backend

The default video backend calls libavcodec, libavutil and libswscale directly
through an opaque C ownership boundary. H.264 and H.265 use send/receive APIs;
there are no GStreamer video encoder, decoder or parser elements in this path,
and no spawned FFmpeg process. GStreamer still supplies desktop capture,
host scaling/color conversion (unless DMA-BUF capture is active), audio, and the appsrc/appsink scheduling bridge.
MoQ framing, authentication, input and the SDL renderer remain unchanged.

Linux auto encoding probes NVIDIA NVENC first, then Intel Quick Sync, VA-API,
and software. Explicit encoder choices do not silently substitute another kind.
Linux decoding probes CUDA, VA-API, Quick Sync, then software; macOS probes
VideoToolbox before software. Hardware selection requires actual decoded output.
Backend names in logs and Stats start with `ffmpeg:`. Software uses libx264 /
libx265 encoding and FFmpeg H.264 / HEVC decoding. The Nix flake links these
libraries on Linux/macOS and enables oneVPL for Quick Sync on x86-64 Linux.

The client's default `--codec auto` uses these probes: the host advertises
the codecs its hardware encoders produce (probed once per process at 720p,
`Desktop.hardware_codecs`) and the client picks H.265 only when it also has a
hardware H.265 decoder. Zero-copy capture and presentation are codec-agnostic;
the offscreen CUDA→GL and VA-API→EGL presentation tests and the DMA-BUF →
NVENC/VA-API encode test cover H.265 as well as H.264. 10-bit output stays
HDR10-only, which is always H.265.

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

**Zero-copy on Linux with NVIDIA (2026-10-16):** when the session window uses
SDL's OpenGL renderer on an NVIDIA GL context and the stream is SDR, CUDA-decoded
frames stay on the GPU: CUDA copies the NV12 planes into GL textures and a
shader converts and draws them under SDL's overlays (`src/cuda_gl.c`). Stats
(F8) then shows `CUDA surface / OpenGL zero-copy`. `TELEPORT_ZERO_COPY=0`
disables it; a presentation error switches the process to the CPU path and
reconnects. Offscreen measurement on an RTX 3080: the GPU→CPU→GPU transfer
alone drops from 0.67 to 0.22 ms at 1080p and 2.78 to 0.27 ms at 4K, before
counting the removed conversion and copies.

**Zero-copy on Linux with VA-API (Intel/AMD, 2026-10-16):** when the session
window's GL context is EGL (not GLX) on a DRM device, the client opens the
VA-API decoder on that EGL display's own render node (`vaapi@/dev/dri/renderDN`),
exports each decoded surface as DMA-BUFs (`av_hwframe_map` to DRM PRIME, separate
R8 + GR88 layers), imports them as EGLImages with explicit format modifiers and
draws them with the same BT.709 shader (`src/vaapi_egl.c`, shared GL code in
`src/gl_nv12.c`). Stats shows `VA-API DMA-BUF / EGLImage zero-copy`. The same
safety rules apply: `TELEPORT_ZERO_COPY=0` opts out, a presentation error
switches the process to the CPU path and reconnects, and GL state is restored.
SDL contexts are EGL on native Wayland. On X11 (the default whenever `DISPLAY`
is set) SDL uses GLX, so this path stays off unless `TELEPORT_X11_EGL=1` makes
SDL use EGL there (`SDL_VIDEO_X11_FORCE_EGL`, mapped by sdl2-compat to SDL3's
`SDL_VIDEO_FORCE_EGL`); that is opt-in because it also changes the context type
for every other X11 user, including the NVIDIA path. If the VA-API decoder for
the window's GPU is unavailable, decoding falls back to the normal selection
with CPU NV12 presentation.

Measured offscreen on the Intel UHD 630 (EGL device platform, FBO readback,
machine under load): export + import cost ~0.03 + ~0.045 ms of CPU per frame;
import + draw + `glFinish` is 1.9–2.5 ms at 1080p and 3.6–9.5 ms at 4K, versus
6.5–13 ms (1080p) and 27–53 ms (4K) for the VA-API download alone that the CPU
path pays before its own conversion and upload. Windowed check: a weston
headless compositor on the Intel GPU (Mesa EGL) showed the host's solid
#3366cc desktop as (49, 102, 201) under SDL's toolbar.

**Zero-copy capture on Linux (host, 2026-10-16):** with an NVENC or VA-API
encoder and an SDR PipeWire (portal) source, the host negotiates
`memory:DMABuf` RGB frames (XR24/AR24/XB24/AB24) with every modifier the
encoder's GPU can import (queried with `eglQueryDmaBufModifiersEXT` on that
GPU's EGL device) and never maps them (`src/dmabuf_encode.c`):

- VA-API: the DMA-BUF becomes a VA surface (DRM PRIME 2 import) and the video
  processor scales and converts it to limited-range BT.709 NV12 straight into
  a surface of the encoder's pool. The encoder is opened on the first render
  node with a VA driver so formats are queried on the same GPU; Intel CCS
  modifiers are not offered.
- NVIDIA: NVENC takes CUDA frames. The DMA-BUF is imported as an EGLImage on
  the NVIDIA EGL device of the encoder's CUDA context, a GLES 3 shader samples
  it as an external texture (so linear, external-only layouts work too) and
  writes luma and half-size chroma textures, and CUDA copies those into the
  CUDA frame. NVENC's own RGB input was not used: it converts with a fixed
  matrix and cannot scale or crop.

PipeWire crop metadata is honoured. The compositor's buffer is released only
after the GPU has read it. Logs show `capture="DMA-BUF zero-copy"` and the
host encoder in Stats reads `ffmpeg:<encoder> DMA-BUF zero-copy`.
`TELEPORT_CAPTURE_ZERO_COPY=0` opts out. HDR (P010), Quick Sync, software
encoders and X11 keep CPU capture. If the encoder's GPU imports none of the
formats, negotiation fails or the first frame fails to import or encode
(checked for up to 2 s before the encoder is returned), the encoder is
rebuilt with CPU capture and the process stops trying DMA-BUFs; a failure
after that first frame ends the pipeline like any capture error (the host
recreates it, now on the CPU path).

Measured on this box (see docs/review-2026-10.md, M2): whole-process CPU per
frame drops from 8–31 ms to 0.8–1.6 ms at 1080p and 4K on both GPUs, and the
real KDE desktop (KWin on the RTX 3080, 2560x1440) went from 17.8 ms CPU and
59 ms capture→packet per frame to 1.4 ms and 9 ms.

Quick Sync, software decoding and macOS still download frames: everything
else below applies to them. Encoding without DMA-BUF capture likewise
receives CPU-addressable captured pixels and uploads when the hardware API
requires it.
Direct codecs provide explicit buffer ownership and timing; they do not prove
that the reported Intel UHD 630 black flashes are fixed.

The existing experimental macOS native-surface path
(`TELEPORT_MAC_GPU_VIDEO=1`) currently requires `TELEPORT_VIDEO_BACKEND=gstreamer`.
The direct backend rejects that combination rather than pretending to provide
native surfaces. FFmpeg-to-Metal presentation remains future work.

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
# Zero-copy presentation vs CPU decode (CUDA and VA-API; offscreen EGL):
nix develop --command env TELEPORT_TEST_HARDWARE=1 LD_LIBRARY_PATH=/run/opengl-driver/lib cargo test --bin teleport zero_copy -- --ignored --nocapture
# Host DMA-BUF capture: encode colours, pipeline + fallback, CPU vs zero-copy
# benchmark (release build for fair CPU-path numbers; one test at a time):
nix develop --command env TELEPORT_TEST_HARDWARE=1 LD_LIBRARY_PATH=/run/opengl-driver/lib cargo test --release --bin teleport zero_copy_capture -- --ignored --nocapture --test-threads=1
# The real desktop through the portal (opens its own session; run again with
# TELEPORT_CAPTURE_ZERO_COPY=0 for the CPU path):
nix develop --command env TELEPORT_TEST_PORTAL=1 LD_LIBRARY_PATH=/run/opengl-driver/lib cargo test --release --bin teleport portal_capture_benchmark -- --ignored --nocapture
```
