# Experimental KWin HDR screencasting

This is an opt-in patch against **KWin 6.7.5**, using the Linux host's pinned
NixOS packages, not Teleport's independently pinned application dependencies.
It does not weaken portal permissions or change unattended-access policy.
Building the package does not change the running compositor.

The final patched KWin 6.7.5 package has built successfully with the host's
pinned NixOS dependencies. Its isolated NVIDIA virtual-output test also passed:
SDR BGRA center white was 255/255/255, HDR `RGB10A2_LE` center white was
594/594/594 (the expected 203-nit PQ value), and reconnecting in SDR returned
255/255/255. HDR negotiation verified full-range RGB, BT.2020 primaries, and
PQ transfer metadata. This demonstrates actual scene conversion and packed
10-bit capture; the running desktop, real portal session, and Mac HDR panel
have not been tested with the patched compositor.

The application-local PipeWire conversion regression test is separately
available as `import ./nix/pipewire-hdr-check.nix { inherit pkgs; }`. It checks
the HDR negotiation roundtrip, all four colorimetry fields, SDR negotiation,
and real GStreamer unpacking of packed primary-color pixels. This check has
passed in the Nix sandbox; it does not validate a running compositor or panel.

`nix/kwin-hdr-gl-test.c` is a separate, optional hardware preflight. It creates
an isolated EGL device/pbuffer context (no visible window), checks that the
framebuffer really has 10-bit channels, and verifies all 1024 red/green levels
survive packed readback. This passed on this machine's NVIDIA RTX 3080 and on
Mesa llvmpipe. It is not a test of KWin's scene conversion or the Mac display.
Compile with a C compiler and `pkg-config --cflags --libs egl gl`. On NixOS,
the NVIDIA-only run used `__EGL_VENDOR_LIBRARY_FILENAMES` pointing to
`/run/opengl-driver/share/glvnd/egl_vendor.d/10_nvidia.json` and
`LD_LIBRARY_PATH=/run/opengl-driver/lib` for that test process only.

## What changes

Output capture keeps the existing SDR DMA-BUF and shared-memory choices first.
A consumer explicitly accepting `RGB10A2_LE` with full-range RGB BT.2020 / PQ
can instead negotiate a 10-bit shared-memory frame. Window and region capture
remain SDR. HDR capture requires desktop OpenGL: KWin's OpenGL ES allocation
helper reduces these textures to 8-bit, so that backend never advertises HDR.
The HDR path intentionally uses CPU readback; zero-copy HDR and its
modifier negotiation are not implemented.

The compositor renders the scene into a `GL_RGB10_A2` target with a BT.2020/PQ
color description, SDR reference white of 203 cd/m², and 10000 cd/m² container
headroom. This is a defined capture target, **not** a claim about source-content
mastering luminance. No invented content-mastering or light-level metadata is
sent. KWin's existing per-surface color transformations convert scene colors
into this target. Merely labeling an SDR image as HDR is not sufficient.

The packed little-endian word has R in bits 0–9, G in 10–19, B in 20–29,
and alpha in 30–31. The matching formats are DRM `ABGR2101010`, SPA
`ABGR_210LE`, Qt `A2BGR30_Premultiplied`, GL `RGBA` with
`UNSIGNED_INT_2_10_10_10_REV`, and GStreamer `RGB10A2_LE`.
The PipeWire plugin patch adds this format mapping and carries colorimetry
in both negotiation directions. GStreamer source caps are
`video/x-raw,format=RGB10A2_LE,colorimetry=1:1:14:7`; the named `bt2100-pq`
instead denotes limited-range YUV and must not label the RGB source.

## Build without activation

From this checkout, using the current host configuration:

```sh
nix build --no-link --impure --expr '
  let host = builtins.getFlake "/etc/nixos";
  in import ./nix/kwin-hdr.nix { pkgs = host.nixosConfigurations.nixos.pkgs; }
'
```

For a future, deliberately scheduled deployment, explicitly import
`nix/kwin-hdr-module.nix` and set
`services.teleport-desktop.experimentalKwinHdr = true`. The version guard rejects
other KWin versions; rebase and retest the patch before upgrading. Removing the
option restores stock KWin on the next graphical login. Do not hot-replace the
compositor while relying on its remote session. This project has not restarted
the active compositor, logged out, or rebooted to test this patch.

With the Teleport flake input already named `teleport`, the NixOS module list
can include the exported module explicitly:

```nix
modules = [
  teleport.nixosModules.default
  teleport.nixosModules.experimental-kwin-hdr
  { services.teleport-desktop.experimentalKwinHdr = true; }
  # Your existing host configuration and Teleport service settings.
];
```

This is a future deployment example, not a change made to `/etc/nixos` during
development. Building the Teleport application alone does **not** install,
activate, or restart the patched compositor. A planned new graphical session
is needed to run a deployed compositor package.

## Required acceptance tests

The optional synthetic test does not attach to the live desktop or portal:

```sh
# Build the helper with the application's pinned dependencies.
helper=$(nix build --no-link --print-out-paths --impure --expr '
  let lock = builtins.fromJSON (builtins.readFile ./flake.lock);
      pkgs = import (builtins.fetchTree lock.nodes.nixpkgs.locked) {
        system = "x86_64-linux";
      };
  in import ./nix/kwin-hdr-nested-helper.nix { inherit pkgs; }
')
# Set this to the patched package built above, not the running system package.
patched_kwin=/nix/store/REPLACE-WITH-PATCHED-KWIN
nix develop --command bash nix/kwin-hdr-nested-test.sh \
  "$patched_kwin" "$helper/bin/nested-capture"
```

The harness creates private runtime/configuration directories, a private D-Bus
and PipeWire daemon, and a 128×128 virtual KWin output with a white test surface.
It declares the helper's restricted interface in that private application
directory using KWin's normal authorization mechanism; it does not disable
permission checks. It starts no hardware-monitoring session manager and links
only its private capture ports. It checks actual center-pixel values, not just
caps: SDR white must be 255 and 203-nit PQ white approximately 594/1023 in each
channel (with a small quantization tolerance), then SDR must work again. Logs
are retained in the printed temporary directory. This is a synthetic scene
conversion test, not a substitute for real portal or HDR display acceptance.

A successful package build is not an HDR image-quality test. On a deliberately
scheduled patched session, verify portal permission behavior and SDR capture
first, then negotiate the HDR caps above and inspect actual negotiated format.
Use known color patches and luminance ramps to verify channel order, PQ values,
203-nit SDR reference, gradients, highlights, and client HDR presentation.
Test repeated SDR/HDR connections and mixed SDR/HDR monitors. Confirm a plain
SDR consumer still gets the normal SDR format. Measure CPU use and capture
latency at native resolution; the shared-memory path is an experimental
correctness baseline, not a promise of zero-copy performance.
