{ pkgs, bundle }:
# Generic Linux tarball of the relocatable bundle (nix/bundle.nix):
# teleport-VERSION-ARCH-linux/ with AppRun, the launcher, at its top. Unpack
# anywhere and run AppRun, or install it as packaging/arch/PKGBUILD does.
# Owners, order and timestamps are fixed, so the archive is reproducible.
let
  arch = pkgs.stdenv.hostPlatform.parsed.cpu.name;
  name = "teleport-${bundle.version}-${arch}-linux";
in
pkgs.runCommand "teleport-tarball-${bundle.version}"
  {
    nativeBuildInputs = [ pkgs.gnutar ];
    passthru.file = "${name}.tar.gz";
  }
  ''
    mkdir -p $out
    # Rename the top directory only, not symlink targets (--transform would).
    tar --create --sort=name --owner=0 --group=0 --numeric-owner \
      --mtime=@''${SOURCE_DATE_EPOCH:-1} --mode=u+w,go-w \
      --transform 'flags=rh;s|^\.|${name}|' -C ${bundle} . |
      gzip -9 -n >$out/${name}.tar.gz
  ''
