# KMS capture (`--source kms`)

For hosts that have **no desktop portal and no X server**: a kiosk or
appliance whose own compositor drives the display through KMS (for example a
Qt `eglfs` or QtWayland compositor, Weston's DRM backend, or cage). The host
reads the framebuffer each display controller (CRTC) is showing, the same way
FFmpeg's `kmsgrab` device does, and streams it.

```sh
sudo setpriv --inh-caps +sys_admin --ambient-caps +sys_admin ... \
  teleport host --source kms --identity-dir /run/teleport/host ...
# or pick the device:
teleport host --source kms --kms-device /dev/dri/card1 ...
```

On NixOS or under systemd, grant the capability to the service instead of
running it as root:

```ini
[Service]
AmbientCapabilities=CAP_SYS_ADMIN
CapabilityBoundingSet=CAP_SYS_ADMIN
SupplementaryGroups=video render
```

## How it works

Every frame, the host asks the kernel which framebuffer the CRTC is scanning
out (it changes on every page flip), exports it as DMA-BUFs and hands them to
the encoder (`src/kms.rs`, `src/kms_grab.c`):

- **GPU path (zero-copy).** A VA-API or NVENC encoder that can import the
  framebuffer's format and modifier reads it directly on the GPU, as with
  portal DMA-BUF capture ([ffmpeg](ffmpeg.md)). This is the normal path on
  Intel, AMD and NVIDIA hosts.
- **CPU path.** When the encoder cannot import it (software encoding,
  `TELEPORT_CAPTURE_ZERO_COPY=0`, or an import failure), a **linear**
  framebuffer is mapped and read by the CPU. A **tiled** framebuffer (most
  GPUs scan out tiled buffers) cannot be read this way; the host logs
  `the display's framebuffer is not CPU-readable (tiled)` and sends no
  frames. Use a hardware encoder.

The framebuffer is sampled at the display's refresh rate (at most 240 Hz);
the encoder then streams at the rate the client asked for.

Each active CRTC is one monitor, named after its connector (`HDMI-A-1`).
A display that is switched off shows nothing and is left out.

## Requirements and limits

- **`CAP_SYS_ADMIN` (or DRM master).** The kernel only gives another
  process's framebuffer handles to a privileged caller; without it the host
  fails at start with `KMS capture needs CAP_SYS_ADMIN`. Membership in the
  `video` group is needed to open `/dev/dri/card*`.
- **View-only.** There is no input injection: keyboard, mouse and gamepad
  events from clients are ignored. Give guests view-only permissions.
- **No cursor.** A hardware cursor lives on its own plane and is not in the
  captured picture. Neither are other overlay planes: only the primary
  plane is captured. A compositor that composites everything into its
  primary plane (most kiosks) is captured completely.
- **SDR, 32-bit RGB.** XRGB/ARGB/XBGR/ABGR 8888 framebuffers; no HDR, no
  10-bit or YUV scanout formats.
- **Tearing.** The compositor may draw into a buffer while it is being
  encoded (as with `kmsgrab`). Double-buffered compositors rarely show it.
- **No virtual displays or privacy mode** (those need a desktop portal).

## Privacy

KMS capture shows **whatever the display shows**, including lock screens,
other users' sessions and any compositor-private overlay. There is no
per-window or per-application selection and no consent dialog on the host:
the capability grant is the consent. Run it only where streaming the whole
display is intended, and tell the people in front of the screen (a kiosk
can draw its own "remote session active" banner, which is then also in the
stream).

## Testing

Unit tests cover the source descriptions, the DMA-BUF variant the encoder
derives and the caps for linear and tiled framebuffers:

```sh
cargo test --locked --bin teleport kms::
```

The end-to-end test reads a real framebuffer. **Never point it at a desktop
someone is using**; use `vkms` (a virtual KMS device) in a VM:

```sh
# inside a VM with `boot.kernelModules = [ "vkms" ]`, as root:
modetest -M vkms -s <connector>@<crtc>:1024x768 &    # shows a test pattern
TELEPORT_TEST_KMS=/dev/dri/card0 cargo test --locked --bin teleport \
  kms::tests::captures_a_real_framebuffer -- --ignored --nocapture
```

It captures ten frames and checks that they carry the test pattern (several
distinct colours, not a blank buffer).
