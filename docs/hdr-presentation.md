# Native HDR presentation (macOS implementation, hardware verification pending)

## Experimental GPU-resident SDR video

On macOS, build this revision with `nix build`, then launch:

```sh
TELEPORT_MAC_GPU_VIDEO=1 ./result/bin/teleport
```

Choose SDR and hardware decoding (H.264 or H.265). This opt-in path passes the
VideoToolbox `CVPixelBuffer` retained by GStreamer's `GstCoreVideoMeta` directly
to a CoreVideo Metal texture cache. The application does not map, CPU-convert,
copy, or upload the decoded video planes. The application-local applemedia patch
requests IOSurface-backed, Metal-compatible output when this variable is `1`.
The private metadata layout is checked against the pinned GStreamer version;
unknown layouts, missing surfaces, unsupported color metadata, and failed Metal
imports are errors, not a silent CPU fallback labeled as GPU video.

The Metal shader handles limited-range BT.709 NV12 and supported chroma siting.
At most three submitted frames retain their decoder sample and imported CoreVideo
textures until their GPU commands complete. The normal one-frame latest-image
slot, generation barriers, input mapping, and native toolbar remain in use.

This is **GPU-resident video**, not an entirely CPU-free application: networking,
input, and UI work still use the CPU, and the toolbar/stats panels are read back
and uploaded separately. HDR still uses the high-precision CPU-upload path below.
Software decoding/rendering and headless operation also keep their existing paths.
Linux is unchanged. Unset `TELEPORT_MAC_GPU_VIDEO` to return to normal rendering.

Mac hardware acceptance is required before making this default: test both codecs,
60 fps at 720p/1080p/native resolution, Retina resizing/fullscreen, monitor changes,
minimize/restore, sleep/wake, toolbar/stats, repeated reconnects and stream switches.
Compare F8 receive-to-ready/presented FPS and Activity Monitor CPU against the
normal renderer at identical settings. Check grayscale, saturated colors and
fine colored text. A successful Linux test or Darwin type-check does not verify
Apple framework linkage, Metal shader compilation, visual correctness, or latency.

The Mac-only surface lifetime test can be run separately:

```sh
nix develop --command env TELEPORT_MAC_GPU_VIDEO=1 cargo test --test mac_surface -- --ignored
```

It checks both codecs, IOSurface backing, and retained pixel-buffer ownership
after decoder destruction. It does not create a Metal drawable or validate colors.

`src/hdr_present.rs` is a native SDL2/Metal presenter. It does not make the existing
SDL RGB24 renderer HDR, and must only be selected for a genuinely HDR frame whose
color metadata has been checked upstream. H.265 or a ten-bit format alone is not
evidence of HDR. Linux native HDR presentation is not implemented by this module;
request an SDR stream instead. A client-side SDR tone mapper is not implemented.

## Integration contract

Create `HdrPresenter::new(canvas.window())` on the macOS main thread **after** the
SDL renderer has been created. It attaches a second SDL Metal view to the same
native window. That view occupies the full window; it presents the video and the
UI together. No separate window or SDL3 event loop is introduced.

`present(frame, desktop_rect, overlays, window_size)` accepts:

- `P010Frame`: even width/height, separate borrowed Y and interleaved UV byte
  planes, and byte strides. Ten-bit samples are left-aligned in little-endian
  16-bit words. Input must be **limited-range BT.2020 non-constant-luminance YUV,
  ST 2084/PQ with centered (`chroma-site=jpeg`) chroma**. The media decoder
  normalizes and validates that chroma siting. Input is not valid for HLG,
  full-range YUV, left-sited chroma, or untagged SDR.
- `desktop_rect` and `window_size`: the same logical SDL coordinates used for
  letterboxing and remote pointer mapping. The Metal drawable has its own Retina
  pixel dimensions; normalized quad coordinates preserve logical input mapping.
- `Overlay`: small straight-alpha RGBA8/sRGB UI surfaces, in painting order.
  Each has a logical destination rectangle and an independently specified actual
  pixel width/height and byte stride. Opaque toolbar and stats panels are the
  simplest correct sources. If the SDL render target contains premultiplied
  alpha, unpremultiply before submission or capture opaque panel regions.

Continue drawing the existing toolbar/stats with SDL, but skip RGB24 video upload
and video drawing in HDR mode. Read back only the relevant UI panel rectangles,
using physical SDL render-output coordinates on Retina displays; pass their
logical destinations separately. Call the Metal presenter after SDL composition.
Do not call `canvas.present()` afterward, create another SDL renderer afterward,
or show an uncomposited UI panel underneath the covering Metal view. Cached UI
readbacks can be reused until their contents change.

The SDL Cocoa Metal view deliberately returns `nil` from `hitTest:`, allowing the
existing SDL window to receive pointer input. Key handling, input-release behavior,
toolbar hit testing, monitor switching and video-generation barriers remain the
caller's responsibility. The presenter keeps a cloned SDL window handle alive.

## Color and GPU path

The CPU uploads P010 planes into `R16Unorm` and `RG16Unorm` Metal textures. A shader
performs limited-range expansion, BT.2020 non-constant-luminance YUV-to-RGB and the
ST 2084 inverse transfer function. **There is no eight-bit RGB intermediate.**

The presentation target is `RGBA16Float`, tagged with extended-linear BT.2020 and
configured with `wantsExtendedDynamicRangeContent = true`. Video values are
absolute PQ luminance divided by **203 cd/m² reference white**: SDR white becomes
1.0, a 1,000-nit highlight approximately 4.926, and a 10,000-nit highlight
approximately 49.261. macOS then maps this EDR-relative representation according
to the display's current brightness/headroom. This is a fixed reference-white
policy, not a claim that the screen emits exactly 203 cd/m² for value 1.0.

UI sRGB values are linearized and converted from BT.709 primaries to BT.2020
primaries before alpha composition. UI white is 1.0, so ordinary UI is not made
artificially HDR-bright.

The current implementation does **not** install `CAEDRMetadata` tone-mapping
metadata. It preserves values above 1.0, but highlights above the display's current
headroom may clip. It must not be described as mastering-metadata-aware or as
providing complete HDR10 display mapping. Any future tone mapper needs accurate
stream metadata and independent reference-image validation.

Three reusable Y/UV texture sets bound in-flight storage. A texture is not written
again until its previous GPU command completes. UI textures are currently
allocated per presentation; caching and end-to-end latency benchmarking remain
follow-up work. This initial implementation is CPU-uploaded P010, not a zero-copy
VideoToolbox/IOSurface renderer.

## Capability and failure handling

Creation and each presentation query the window's current
`NSScreen.maximumExtendedDynamicRangeColorComponentValue`. Headroom must exceed
1.0 before output is reported as HDR. A bounded three-second startup grace period
allows initial drawables to activate EDR; during this period headroom of 1.0
returns `DrawableUnavailable`, even if a warmup drawable was submitted. Persistent
SDR headroom then returns a clear error. A screen move or brightness change can
remove headroom after HDR starts; that returns an error immediately. The caller
can offer a reconnection requesting an SDR stream. A genuinely unavailable Metal
drawable also returns `DrawableUnavailable`. Neither case counts as a presented
HDR frame. Do not downgrade TLS trust or change portal permissions on any failure.

The code needs macOS-target dependencies `objc2 = 0.6.3` and `objc2-foundation`,
`objc2-metal`, `objc2-quartz-core`, `objc2-core-graphics` at `0.3.2`. This avoids the
deprecated `metal`/`objc` binding family. An isolated presenter crate passed
`cargo check --target aarch64-apple-darwin` and target Clippy with `-D warnings`
on Linux using the official Rust 1.95 compiler and matching Apple target standard
library. This checks the Apple Rust
binding types without linking or running them. Linux tests cover buffer bounds
and PQ reference arithmetic, **not** Apple API linkage, Metal shader compilation
or real HDR output.

## Required Mac acceptance checks

1. Build/check on Apple Silicon macOS, including strict clippy and Metal shader
   compilation at runtime. Verify the native window/toolbar/input still work.
2. Use an HDR-capable display with current EDR headroom above 1.0. Record display
   model, settings, OS version, selected decoder and measured headroom.
3. Present a tagged BT.2020/PQ reference pattern with 203-, 400- and 1,000-nit
   patches. Verify values above SDR white survive decoding and presentation;
   compare against a known-good HDR reference renderer or measurement instrument.
   An SDR screenshot alone cannot prove HDR output.
4. Check neutral grayscale, saturated BT.2020 colors, PQ black and limited-range
   code values. Check sRGB toolbar color/brightness against the SDR client.
5. Test Retina resizing, full screen, moving between SDR/HDR monitors, minimized
   windows, sleep/wake, stats overlay, monitor switching and reconnect. Confirm
   that unsupported headroom never leaves a falsely labeled HDR session.
6. Measure CPU/GPU cost and frame latency before claiming a low-latency HDR path.

No reboot, logout, display-profile mutation, compositor replacement or local
service restart is needed to inspect or build this presenter.

## Primary references

- [SDL2 Metal view creation](https://wiki.libsdl.org/SDL2/SDL_Metal_CreateView) and
  [layer access](https://wiki.libsdl.org/SDL2/SDL_Metal_GetLayer).
- [SDL Cocoa Metal view implementation](https://github.com/libsdl-org/SDL/blob/SDL2/src/video/cocoa/SDL_cocoametalview.m), including pass-through hit testing.
- [Apple: performing your own tone mapping](https://developer.apple.com/documentation/metal/performing-your-own-tone-mapping), covering floating-point EDR targets and linear color spaces.
- [Apple: EDR content](https://developer.apple.com/documentation/quartzcore/cametallayer/wantsextendeddynamicrangecontent) and [EDR metadata](https://developer.apple.com/documentation/quartzcore/cametallayer/edrmetadata), including clipping when tone-mapping metadata is absent.
- [Apple: Explore HDR rendering with EDR](https://developer.apple.com/videos/play/wwdc2021/10161/).
- [Maintained QuartzCore bindings](https://docs.rs/objc2-quartz-core/0.3.2/objc2_quartz_core/struct.CAMetalLayer.html) and [Metal bindings](https://docs.rs/objc2-metal/0.3.2/objc2_metal/).
