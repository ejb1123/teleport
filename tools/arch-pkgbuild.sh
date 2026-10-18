#!/usr/bin/env bash
# Writes the AUR package teleport-desktop-bin for one release: PKGBUILD with
# pkgver and the tarball's sha256, the .install file, and .SRCINFO when
# makepkg is available (on Arch, or in an archlinux container).
#
#   tools/arch-pkgbuild.sh VERSION TARBALL OUTDIR [--with-tarball]
#
# TARBALL is teleport-VERSION-x86_64-linux.tar.gz (`nix build .#tarball`, or
# the release asset). --with-tarball copies it into OUTDIR as well, so
# makepkg builds from it without downloading the release (CI, local tests).
# OUTDIR is what goes into the AUR repository (docs/distribution.md).
set -euo pipefail
[ $# -ge 3 ] || { sed -n '2,11p' "$0" >&2; exit 2; }
version=$1 tarball=$2 out=$3 copy=${4-}
here=$(cd "$(dirname "$0")/.." && pwd)
case "$version" in
  v*) echo "VERSION is without the v: ${version#v}" >&2; exit 2 ;;
esac
name=teleport-$version-x86_64-linux.tar.gz
[ "$(basename "$tarball")" = "$name" ] || { echo "expected a tarball named $name" >&2; exit 1; }
sum=$(sha256sum "$tarball" | cut -d' ' -f1)
mkdir -p "$out"
sed -e "s/^_version=.*/_version=$version/" \
  -e "s/^pkgver=.*/pkgver=${version//-/_}/" \
  -e "s/^pkgrel=.*/pkgrel=1/" \
  -e "s/^sha256sums_x86_64=.*/sha256sums_x86_64=('$sum')/" \
  "$here/packaging/arch/PKGBUILD" >"$out/PKGBUILD"
cp "$here/packaging/arch/teleport-desktop-bin.install" "$out/"
if [ "$copy" = --with-tarball ]; then
  cp "$tarball" "$out/$name"
fi
if command -v makepkg >/dev/null 2>&1; then
  (cd "$out" && makepkg --printsrcinfo >.SRCINFO)
else
  echo "makepkg not found: run 'makepkg --printsrcinfo > .SRCINFO' in $out on Arch" >&2
fi
echo "$out/PKGBUILD: teleport-desktop-bin $version, sha256 $sum"
