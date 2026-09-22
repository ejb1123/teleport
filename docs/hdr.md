# Experimental HDR

Teleport has an explicit, experimental **KWin output capture → HEVC Main 10 →
MoQ → P010 → macOS Metal/EDR** path. It is not yet a validated end-to-end HDR
desktop product. The running host's stock KWin still supplies 8-bit SDR capture;
building Teleport does not replace it or restart the graphical session.

## Implemented

- Opt-in [KWin 6.7.5 patch](kwin-hdr.md) renders output captures into packed
  10-bit, full-range BT.2020 RGB / PQ when explicitly negotiated. Ordinary SDR
  consumers keep their existing choices. HDR uses shared memory and CPU readback,
  not zero-copy. Window/region and X11 capture remain SDR.
- Application-local PipeWire plugin patch maps RGB10A2 and preserves color
  metadata in both negotiation directions. The system PipeWire daemon is unchanged.
- Host validates actual source precision and color caps, converts to 10-bit
  limited-range BT.2020/PQ YUV, and verifies Main 10 encoder output before
  advertising HDR. H.264/HDR and contradictory negotiation are rejected.
- Decoder probes actual codec output, checks 10-bit/PQ/BT.2020 before conversion,
  and retains P010 planes. Software and NVIDIA paths have local synthetic tests;
  macOS uses VideoToolbox when its real probe succeeds, otherwise software.
- [Mac native presenter](hdr-presentation.md) converts P010 to extended-linear
  BT.2020 in an RGBA16Float Metal layer with EDR enabled. UI chrome remains SDR.
  It checks display headroom; it never silently presents PQ as ordinary RGB8.
- Explicit `hdr10` protocol mode defines PQ/BT.2020, limited YUV, and 203-nit
  reference white. No invented mastering-display or content-light metadata is sent.

## Trying it

First build and arrange a **planned new graphical session** with the patched
compositor using [the KWin instructions](kwin-hdr.md). Do not hot-replace KWin
while relying on remote access. No reboot or logout was performed during this work.
Keep local unlock/recovery available for the first test.

Update both endpoints, then on an HDR-capable Mac choose **HDR · preview** in
the launcher (automatically selects H.265), or use an existing pairing file:

```sh
teleport client HOST:4443 --pairing-file /path/to/pairing.json \
  --codec h265 --dynamic-range hdr10 --width 0 --stats
```

An unpatched/unsupported source fails negotiation; it is not relabeled HDR.
Linux graphical clients currently request SDR. There is no client-side HDR-to-SDR
tone mapper; choose an SDR stream, which uses the compositor's SDR capture path.
HDR-enabled physical monitors alone do not establish HDR capture support.

## Evidence and outstanding acceptance

`teleport doctor --hdr` runs bounded synthetic codec diagnostics without opening
desktop capture or networking. A codec PASS is not a display/capture certification.

Locally tested synthetic RGB10 PQ → software x265 or NVIDIA NVENC → MoQ →
NVIDIA decode/P010 preserves black Y=64, white Y=940, and neutral chroma=512.
A gradient retained 660 (software) / 643 (NVIDIA) distinct decoded luma levels,
demonstrating more than eight-bit precision in that test. SDR-source rejection
is also tested. The PipeWire format regression checks color fields, SDR negotiation,
and actual red/green/blue packed-channel unpacking. These are not measurements of
a real desktop or panel.

Still required before calling full HDR supported:

1. Real patched-KWin portal capture: known patches, PQ values, reference white,
   wide-gamut colors and highlights; ordinary SDR capture must remain correct.
2. Physical Mac/Retina HDR display: native build/runtime, above-white output,
   color accuracy, display moves, brightness changes, sleep/wake and SDR rejection.
3. Mixed-monitor switching, lock/unlock and reconnect on the patched session.
4. Native-resolution latency, CPU/GPU load, bandwidth and power comparisons.

The baseline KWin limitation was checked against live PipeWire caps (BGRA/BGRx,
no HDR color fields) and [upstream output capture source](https://invent.kde.org/plasma/kwin/-/blob/Plasma/6.7/src/plugins/screencast/outputscreencastsource.cpp).
Portal authorization remains unchanged; see the [ScreenCast interface](https://flatpak.github.io/xdg-desktop-portal/docs/doc-org.freedesktop.portal.ScreenCast.html).
