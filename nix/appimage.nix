{ pkgs, bundle }:
# AppImage of the relocatable bundle (nix/bundle.nix): AppImage's static type 2
# runtime (FUSE built in, or --appimage-extract-and-run without FUSE) followed
# by a zstd SquashFS of the AppDir. Owners are fixed and timestamps come from
# SOURCE_DATE_EPOCH (set by Nix), so the image is reproducible from the flake.
let
  arch = pkgs.stdenv.hostPlatform.parsed.cpu.name;
  runtime = pkgs.fetchurl {
    url = "https://github.com/AppImage/type2-runtime/releases/download/20251108/runtime-${arch}";
    sha256 =
      {
        x86_64 = "0396xi3km1yd0z4329lbjxmv82ddc40gd0x8hca0ylcj7i28pjig";
        aarch64 = "0i3lv1j8d9yqssh2i97idmdg9xx11jgdairkdpzw1ikwj77xzjq0";
      }
      .${arch};
  };
  name = "Teleport-${bundle.version}-${arch}.AppImage";
in
pkgs.runCommand "teleport-appimage-${bundle.version}"
  {
    nativeBuildInputs = [ pkgs.squashfsTools ];
    passthru.file = name;
  }
  ''
    mksquashfs ${bundle} image.squashfs -root-owned -no-xattrs -noappend -quiet \
      -comp zstd -Xcompression-level 19 -b 1M \
      -processors "''${NIX_BUILD_CORES:-1}"
    mkdir -p $out
    cat ${runtime} image.squashfs >$out/${name}
    chmod 755 $out/${name}
  ''
