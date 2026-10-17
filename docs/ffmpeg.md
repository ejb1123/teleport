# Direct FFmpeg video backend

The default video backend calls libavcodec, libavutil and libswscale directly
through an opaque C ownership boundary. H.264, H.265 and AV1 use send/receive APIs;
there are no GStreamer video encoder, decoder or parser elements in this path,
and no spawned FFmpeg process. GStreamer still supplies desktop capture,
host scaling/color conversion (unless DMA-BUF capture is active), audio, and the appsrc/appsink scheduling bridge.
MoQ framing, authentication, input and the SDL renderer remain unchanged.

Linux auto encoding probes NVIDIA NVENC first, then Intel Quick Sync, VA-API,
and software. Explicit encoder choices do not silently substitute another kind.
Linux decoding probes CUDA, VA-API, Quick Sync, then software; macOS probes
VideoToolbox before software. Hardware selection requires actual decoded output.
Backend names in logs and Stats start with `ffmpeg:`. Software uses libx264 /
libx265 / SVT-AV1 encoding and FFmpeg H.264 / HEVC / dav1d decoding. The Nix
flake links these libraries on Linux/macOS and enables oneVPL for Quick Sync
on x86-64 Linux. Decoders are probed at 64x48 (H.264) and 160x160 (H.265,
AV1): NVDEC decodes no H.265 below 144x144, and before 2026-10-17 its probe
failed, so H.265 fell back to VA-API or software on NVIDIA.

The client's default `--codec auto` uses these probes: the host advertises
the codecs its hardware encoders produce (probed once per process at 720p,
`Desktop.hardware_codecs` and `hardware_av1`) and the client picks AV1, then
H.265, only when it also has a hardware decoder for it; otherwise H.264.
Zero-copy capture and presentation are codec-agnostic; the offscreen
CUDA→GL and VA-API→EGL presentation tests and the DMA-BUF → NVENC/VA-API
encode test cover H.265 (and CUDA AV1) as well as H.264. Quality options
for SDR sessions, each capability-gated and off by default: 4:4:4 chroma
and 10-bit SDR (below).

Encoders use closed, short GOPs without B-frames. NVENC also explicitly disables
its output delay. libx264 and h264_nvenc use periodic intra refresh (a one
second sweep) instead of periodic IDRs once the client requests keyframes
itself; IDRs then come only at the start and on request (`forced-idr`), and
only IDRs start a MoQ group (`protocol::annexb_keyframe`: these encoders
also flag refresh starts as key frames). `TELEPORT_INTRA_REFRESH=0` on the
host turns it off. H.265, VA-API and QSV keep periodic IDRs: FFmpeg's H.265
decoder cannot decode past a missing reference, and FFmpeg exposes no VA-API
or QSV intra refresh. Test: `intra_refresh_heals_a_lost_frame_without_an_idr`
(add `TELEPORT_TEST_HARDWARE=1` for NVENC). Bitrate changes transactionally reopen the encoder and start
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

## Encoder presets

`--encoder-preset latency|balanced|quality` (host default for its clients;
client and launcher **Encoder:** choose per session through `SelectCodec`,
gated on `Desktop.encoder_presets`) trades encode time for quality. All keep
B-frames, look-ahead and NVENC's output delay off, so none adds frame delay;
the cost is encode time per frame. `latency` is the historical setting and the
default. F8 shows the preset and the host encoder name carries it.

| Encoder | latency | balanced | quality |
| --- | --- | --- | --- |
| NVENC H.264 | p1, ull | p4, ull | p6, hq, two-pass (quarter res) |
| NVENC H.265 | p1, ull | p4, ull | p6, hq |
| VA-API | driver default | quality level 4 | quality level 1 |
| Quick Sync | veryfast | medium | slower |
| x264 / x265 | ultrafast | superfast | veryfast |
| SVT-AV1 | 10 | 9 | 8 |

Measured (`ffmpeg::quality::encoder_presets_time_and_quality`, synthetic
desktop text scrolling 3 rows per frame, 2560x1440 at 60 FPS and 20 Mbps CBR,
RTX 3080 and Intel UHD 630, debug test build, load average 9-11):

| Encoder | Preset | Encode mean / p95 (ms) | Luma PSNR (dB) | SSIM | Mbps |
| --- | --- | --- | --- | --- | --- |
| h264_nvenc | latency | 4.9 / 6.7 | 42.95 | 0.9828 | 19.6 |
| h264_nvenc | balanced | 7.4 / 8.8 | 43.26 | 0.9850 | 19.6 |
| h264_nvenc | quality | 10.4 / 11.9 | 46.50 | 0.9932 | 19.8 |
| hevc_nvenc | latency | 4.7 / 5.9 | 48.63 | 0.9869 | 16.4 |
| hevc_nvenc | balanced | 8.4 / 9.7 | 47.42 | 0.9892 | 7.8 |
| hevc_nvenc | quality | 15.1 / 16.5 | 48.04 | 0.9922 | 7.4 |
| h264_vaapi | latency / balanced / quality | 12.6 / 12.4 / 13.2 | 17.50 / 17.50 / 17.63 | 0.738-0.749 | 19.9 |
| hevc_vaapi | latency / balanced / quality | 21.0 / 21.1 / 27.7 | 17.75 | 0.793 | 19.5 |
| libx264 | latency | 8.4 / 15.4 | 37.30 | 0.9538 | 14.2 |
| libx264 | balanced | 12.9 / 23.4 | 42.09 | 0.9753 | 13.1 |
| libx264 | quality | 16.4 / 26.1 | 41.81 | 0.9796 | 10.6 |

- NVENC: `quality` costs ~5 ms (H.264) to ~10 ms (H.265) per 1440p frame,
  fine at 60 FPS, too slow for 120 FPS and above. H.265 at `balanced` and
  `quality` reaches higher SSIM with under half the bits (its rate control
  does not fill CBR on this content). Two-pass on H.265 was rejected: the
  encoder undershot to 3 Mbps and lost 14 dB.
- VA-API (Intel UHD 630): the quality level changes encode time but not
  measurable quality here, because this content stays at ~17.5 dB on every
  preset: every frame uses its whole CBR budget (~41 kB) yet the text stays
  coarse, even when the screen is static. Cause not yet known (filler data
  or the 2-frame VBV suspected); see docs/review-2026-10.md.
- x264: `balanced` is the sweet spot (+4.8 dB for +4.5 ms).
- Not measured: Quick Sync (no working runtime here), AMD, real games.

## Static-scene refinement

Capture is damage-driven, so when the screen stops the last frame stays at
motion quality; only the 250 ms capture keepalive repeats it, and each
repeat (a P-frame at the frame budget) sharpens it a little. Once the screen
has been still for 200 ms (the first keepalive repeat), the encoder thread
encodes the still frame 8 more times back to back, then waits for the next
change. The burst stops as soon as a new frame is captured (the capture
queue is checked before each repeat), so a change waits at most one encode.
Repeats are ordinary P-frames at the frame budget: the burst spends what the
idle link would have carried (~133 ms of the configured bitrate).

- A frame counts as unchanged when it is a keepalive repeat: zero-copy
  capture compares the memory address and the compositor's frame sequence
  (no reference is held; holding the compositor's buffer starved capture),
  CPU capture a hash of every fourth row.
- Only below 0.05 bits per pixel and frame (e.g. under ~11 Mbps at 1440p60):
  above, the still frame is already sharp. Not on VA-API or Quick Sync:
  repeats did not change the picture on the UHD 630.

Measured (`ffmpeg::quality::static_refinement_burst`, desktop text at
2560x1440 60 FPS, NVENC on the RTX 3080, load average 4-6). PSNR of the still
frame after the first keepalive, after the 8-frame burst (~130 ms later), and
two seconds later:

| Encoder, bitrate | Keepalive only | Burst | +2 s without burst | +2 s with burst | Burst cost |
| --- | --- | --- | --- | --- | --- |
| h264_nvenc 5 Mbps | 25.64 dB | 27.24 dB | 27.24 dB | 29.04 dB | 95 kB |
| hevc_nvenc 5 Mbps | 28.63 dB | 32.51 dB | 32.51 dB | 36.27 dB | 95 kB |
| libx264 5 Mbps | 14.05 dB | 14.91 dB | 14.97 dB | 15.85 dB | 45 kB |
| h264_nvenc 20 Mbps (not refined) | 50.05 dB | 50.25 dB | 50.25 dB | 50.25 dB | 375 kB |

The burst reaches in ~130 ms what keepalives alone reach in two seconds, and
the picture keeps improving from there. Rejected (docs/review-2026-10.md):
a per-frame quality boost instead of repeats (VA-API region-of-interest QP
offsets: +0.2 dB on H.264, a worse frame at -18 QP; libx264 bitrate and VBV
boost: under +1 dB; NVENC bitrate boost: FFmpeg turns it into an IDR, which
lost 8-12 dB).

## AV1

Encoders (probed like the others, `--encoder` kinds map to them): `av1_nvenc`
(RTX 40 and later), `av1_qsv` and `av1_vaapi` (Arc, Meteor Lake and later,
AMD RDNA3), and software SVT-AV1 (`libsvtav1`, low-delay prediction, CBR,
no look-ahead or scene detection, two tile columns, every core). Decoders:
FFmpeg's AV1 decoder on NVDEC, VA-API or VideoToolbox (it has no software
path), `av1_qsv`, and dav1d in software (frame delay 1). MoQ groups start
at a temporal unit with a sequence header and a shown key frame
(`protocol::keyframe`, which reads the OBU headers). NVDEC returns the same
NV12 CUDA surfaces as for H.264/H.265, so the CUDA/GL zero-copy presenter
draws AV1 unchanged (offscreen test); VA-API surfaces would go the same way
(no AV1 VA-API decoder here).

Hosts advertise `Desktop.av1` (they encode it when `SelectCodec` asks: in
hardware, or with SVT-AV1 when `--encoder` is `auto` or `software`) and
`hardware_av1`. AV1 never appears in `codecs` or `hardware_codecs`, which
older clients parse strictly, and the connection still asks for H.264, so
older hosts never see the name; `--codec av1` with such a host stops with
"host cannot stream AV1". `--codec auto` takes AV1 only with hardware on
both ends: SVT-AV1 is too slow for 60 FPS (below).

Intra refresh stays off for AV1: `av1_nvenc` has the option, but there is no
AV1 hardware encoder here to verify that it heals and still honours forced
key frames. Both AV1 decoders here decode past a lost frame
(`ffmpeg::formats::av1_decoding_past_a_lost_frame`: dav1d rejects the next
unit and continues, NVDEC continues), which intra refresh would need.

Measured (`ffmpeg::formats::av1_encode_decode_time_and_quality`, grey desktop
text scrolling 2 rows per frame, 60 FPS CBR, release build, i7-9700K (8
cores) at load average 12-16; SVT-AV1 presets 10/9/8 for
latency/balanced/quality; low delay maps presets above 10 to 10). Encode and
decode are mean / p95 per frame; NVDEC "GPU" is the frame zero-copy
presentation takes, "NV12" includes the download:

| Encoder | Size, bitrate | Encode (ms) | Luma PSNR / SSIM | Mbps | Decode software | NVDEC GPU / NV12 |
| --- | --- | --- | --- | --- | --- | --- |
| SVT-AV1 latency | 720p, 5 Mbps | 21.5 / 54.1 | 50.25 / 0.9987 | 3.3 | dav1d 4.6 / 13.1 | 1.7 / 6.8 |
| SVT-AV1 balanced | 720p, 5 Mbps | 38.4 / 98.8 | 57.78 / 0.9996 | 2.0 | 5.1 / 13.7 | 2.3 / 6.1 |
| SVT-AV1 quality | 720p, 5 Mbps | 49.2 / 130.5 | 59.09 / 0.9995 | 1.3 | 3.0 / 7.6 | 1.1 / 3.5 |
| x264 latency | 720p, 5 Mbps | 4.5 / 10.1 | 36.27 / 0.9539 | 3.6 | 1.7 / 3.1 | 1.9 / 3.9 |
| h264_nvenc | 720p, 5 Mbps | 2.4 / 4.8 | 42.76 / 0.9788 | 4.9 | 2.1 / 3.5 | 1.7 / 4.7 |
| hevc_nvenc | 720p, 5 Mbps | 3.4 / 10.5 | 47.20 / 0.9781 | 4.1 | 3.8 / 5.9 | 1.2 / 3.3 |
| SVT-AV1 latency | 1080p, 10 Mbps | 39.7 / 101.3 | 52.21 / 0.9991 | 6.0 | dav1d 5.6 / 10.0 | 1.5 / 6.2 |
| SVT-AV1 balanced | 1080p, 10 Mbps | 42.7 / 154.0 | 57.62 / 0.9996 | 4.0 | 5.3 / 13.3 | 1.5 / 7.2 |
| SVT-AV1 quality | 1080p, 10 Mbps | 47.0 / 115.2 | 58.11 / 0.9994 | 2.7 | 4.4 / 9.1 | 1.6 / 4.3 |
| x264 latency | 1080p, 10 Mbps | 7.0 / 10.4 | 33.07 / 0.9405 | 7.4 | 3.7 / 6.1 | 2.0 / 7.0 |
| h264_nvenc | 1080p, 10 Mbps | 3.1 / 5.1 | 41.39 / 0.9741 | 9.8 | 3.4 / 5.8 | 2.0 / 4.4 |
| hevc_nvenc | 1080p, 10 Mbps | 2.8 / 3.9 | 46.79 / 0.9767 | 8.3 | 7.4 / 9.1 | 1.3 / 4.3 |

- SVT-AV1's screen-content tools win on text by 5-17 dB at a fraction of the
  bits (it undershoots CBR here), but even its fastest low-delay preset took
  15-17 ms per 720p frame and 15-40 ms per 1080p frame across three runs at
  load 9-20 (p95 30-100 ms): not 60 FPS on this CPU. Hence auto never picks
  software AV1; `--codec av1` with a software host is for slow links.
- NVDEC decodes AV1 as fast as H.264/H.265 (1-2 ms to a GPU frame at 1080p);
  dav1d needs 4-6 ms.
- Not measured: hardware AV1 encoders (none here), VA-API AV1 decode (the
  UHD 630 has none), VideoToolbox AV1 (M3 and later; probed, not run).

## 4:4:4 chroma

`--chroma-444 off|auto|on` (launcher **4:4:4 colour**), sent in `SelectCodec`
to hosts advertising `Desktop.chroma_444_selection`, H.264/H.265 SDR only.
`auto` uses H.265 4:4:4 when the host encodes it in hardware
(`Desktop.hardware_444`) and this client decodes it in hardware, and then
prefers it to AV1; `on` uses 4:4:4 whenever the host can, in software if
need be (H.265 unless `--codec h264`). F8 shows 4:2:0 or 4:4:4 after the
codec.

- Encoders: NVENC H.264 and H.265 (YUV444P), x264/x265, VA-API/QSV with
  packed 4:4:4 surfaces (Ice Lake and later; the UHD 630 refuses it).
- Capture: zero-copy stays on for NVENC: the DMA-BUF shader writes three
  full-size planes instead of NV12's two. VA-API 4:4:4 uses CPU capture
  (its video processor writes NV12 here).
- Decoders: NVDEC decodes H.265 4:4:4 (not H.264: "Hardware is lacking
  required capabilities"), everything else decodes in software. The
  CUDA/GL presenter draws NVDEC's YUV444P surfaces from three planes; other
  decoders convert to RGB on the CPU (NV12 would halve the chroma again).

Measured (`ffmpeg::formats::chroma_444_text_sharpness`): coloured desktop
text (five ink colours, light and dark panels) at 1920x1080 60 FPS, two
seconds scrolling 2 rows per frame then half a second still, scored in RGB
against the source (PSNR and SSIM averaged over R, G, B), release build,
load average 10-13:

| Encoder | Bitrate | 4:2:0 moving / still (dB) | 4:4:4 moving / still (dB) | 4:2:0 / 4:4:4 SSIM still | Encode 4:2:0 / 4:4:4 (ms) |
| --- | --- | --- | --- | --- | --- |
| hevc_nvenc | 8 Mbps | 26.95 / 28.48 | 31.77 / 45.31 | 0.951 / 0.997 | 3.0 / 6.6 |
| hevc_nvenc | 20 Mbps | 28.05 / 28.55 | 41.36 / 50.48 | 0.952 / 0.999 | 2.9 / 5.4 |
| h264_nvenc | 8 Mbps | 26.03 / 28.48 | 22.82 / 34.88 | 0.952 / 0.974 | 5.1 / 7.9 |
| h264_nvenc | 20 Mbps | 27.64 / 28.51 | 33.39 / 46.19 | 0.952 / 0.998 | 3.4 / 6.4 |
| x265 | 20 Mbps | 26.13 / 28.14 | 27.77 / 34.16 | 0.927 / 0.929 | 44.7 / 57.4 |
| x264 | 20 Mbps | 27.50 / 28.56 | 15.57 / 25.85 | 0.952 / 0.914 | 5.6 / 8.0 |

- 4:2:0 cannot get past ~28.5 dB on coloured text: that is the chroma it
  throws away. H.265 4:4:4 on NVENC removes the ceiling at the same bitrate
  (+17 dB still at 8 Mbps, +13 dB while scrolling at 20 Mbps).
- At a low bitrate with motion H.264 4:4:4 is worse than 4:2:0 (-3 dB at
  8 Mbps); x264's ultrafast 4:4:4 is poor on this content (15 dB scrolling;
  on FFmpeg's testsrc2 it matches 4:2:0). Hence auto only uses H.265.
- Capture cost (`zero_copy_capture_benchmark` with
  `TELEPORT_BENCH_CODEC=h265 TELEPORT_BENCH_CHROMA444=1`, static quadrants,
  load 9-10): NVENC zero-copy 4:4:4 takes 0.99 ms CPU and 5.1 ms
  capture→packet per 1080p frame (4:2:0: 0.91 and 4.1 ms), 1.02 and 11.2 ms
  at 4K (4:2:0: 0.93 and 6.4 ms). Through CPU capture 4:4:4 would cost
  9.4 ms CPU and 21.9 ms at 1080p, 44.9 and 74.4 ms at 4K.
- Client: decoding H.265 4:4:4 to RGB on the CPU (NVDEC download + swscale)
  took 18.5 ms per 1080p frame against 12.5-13.5 ms for 4:2:0 at load ~12;
  the CUDA/GL presenter avoids both (not timed).

## 10-bit SDR

Encoding the 8-bit desktop as Main 10 (P010, BT.709, no HDR metadata) was
measured before it was built (`ffmpeg::formats::ten_bit_sdr_banding`,
1920x1080, a dark luma ramp 16..48 across the screen, clean or dithered to
8 bits from 10-bit precision as renderers do, still then panning; release
build, load average 9-12). Banding is measured as the RMS error of the
16x16 box-filtered luma (code values; contours show, dither noise does not)
and as the mean run of one code value along a row (the dithered source has
3 px):

| Encoder, bitrate | Main: error / runs | Main 10 rounded | Main 10 dithered |
| --- | --- | --- | --- |
| hevc_nvenc, 2 Mbps | 0.236 / 29 px | 0.191 / 11 px | 0.073 / 2.5 px |
| hevc_nvenc, 6 Mbps | 0.193 / 9 px | 0.112 / 4.5 px | 0.044 / 2.5 px |
| x265, 2 Mbps | 0.196 / 10 px | 0.212 / 12 px | 0.047 / 2.1 px |
| x265, 6 Mbps | 0.102 / 4.2 px | 0.116 / 3.4 px | 0.029 / 2.1 px |

(Still frame; panning is similar.) NVENC Main turns the dither into 29 px
bands at 2 Mbps; Main 10 keeps it, and shown through a 2x2 ordered dither
the result is 3-5x closer to the source for both encoders. A clean 8-bit
ramp gains nothing (Main is already 69-73 dB) and the dither adds a pattern
to its flat steps (-4 dB PSNR, invisible at one code value). NVENC Main 10
costs ~1.6 ms more per 1080p frame (2.2 → 3.8 ms).

So it is an option, off by default: `--ten-bit off|auto|on` (launcher
**10-bit**), sent in `SelectCodec` to hosts advertising
`Desktop.ten_bit_selection`, H.265 and AV1 only, not with 4:4:4; `auto`
needs Main 10 encode (`Desktop.hardware_ten_bit`) and decode in hardware,
`on` also moves an automatic H.264 choice to H.265. The host captures P010:
CPU capture converts to it, NVENC zero-copy renders R16/RG16 textures
(`GL_EXT_texture_norm16`), and the VA-API video processor writes P010
directly. Clients show it at 8 bits through a 2x2 ordered dither: on the
CPU (P010 → NV12; swscale only truncates) and in the CUDA/GL presenter,
which takes P010 surfaces into 16-bit textures. The VA-API presenter keeps
importing 8-bit NV12, so 10-bit VA-API decodes go through the CPU. F8 shows
"10-bit".

Verified: x265, SVT-AV1, hevc_nvenc and hevc_vaapi (UHD 630) Main 10 keep
values between 8-bit steps into NVDEC, VA-API and software decoders
(`direct_hardware_ten_bit`; VA-API rounds about a third of them); NVENC and
VA-API zero-copy capture colours; CUDA/GL presentation of H.265 and AV1
P010. Not verified: real dithered game content, a VA-API presenter for
P010, hardware AV1 Main 10.

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
Since 2026-10-17: SVT-AV1 into dav1d and NVDEC (also zero-copy), 4:4:4 and
10-bit SDR as above, and end-to-end sessions from a test-source host
(`tests/smoke.rs` `automatic_codec_choice_and_clients_without_selection`:
AV1, 4:4:4 and 10-bit sessions, and AV1 refused by an older host). The
legacy GStreamer backend rejects AV1, 4:4:4 and 10-bit SDR.
Quick Sync is compiled in, but this host's runtime rejects MFX session creation;
it is not hardware-verified here. Physical macOS, AMD and the affected remote
client still require acceptance tests.
Hardware-specific tests require `TELEPORT_TEST_HARDWARE=1`; they are not simulated
by software fallback.

```sh
nix develop --command cargo test --bin teleport ffmpeg -- --include-ignored
nix develop --command env TELEPORT_TEST_HARDWARE=1 cargo test --bin teleport direct_hardware -- --ignored --nocapture
nix develop --command env TELEPORT_TEST_HARDWARE=1 TELEPORT_TEST_ENCODER=vaapi cargo test --bin teleport direct_hardware -- --ignored --nocapture
# AV1, 4:4:4 and 10-bit measurements (release; TELEPORT_TEST_HARDWARE adds
# NVENC/NVDEC; TELEPORT_TEST_ENCODERS limits the encoders):
nix develop --command env TELEPORT_TEST_HARDWARE=1 LD_LIBRARY_PATH=/run/opengl-driver/lib cargo test --release --bin teleport ffmpeg::formats -- --ignored --nocapture --test-threads=1
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
