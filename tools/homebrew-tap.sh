#!/usr/bin/env bash
# Fills in the Homebrew cask and formula templates (packaging/homebrew) for a
# release, for the tap repository ejb1123/homebrew-teleport:
#
#   tools/homebrew-tap.sh VERSION DIST OUTDIR
#
# DIST holds the release assets: Teleport-VERSION-arm64.dmg and
# Teleport-VERSION-x86_64.dmg for the cask, teleport-VERSION-x86_64-linux.tar.gz
# for the formula. A cask or formula whose assets are missing is not written.
# Copy OUTDIR/Casks and OUTDIR/Formula into the tap (docs/distribution.md).
set -euo pipefail
[ $# -eq 3 ] || { sed -n '2,10p' "$0" >&2; exit 2; }
version=${1#v} dist=$2 out=$3
here=$(cd "$(dirname "$0")/.." && pwd)
sum() { sha256sum "$1" | cut -d' ' -f1; }
written=0

arm=$dist/Teleport-$version-arm64.dmg intel=$dist/Teleport-$version-x86_64.dmg
if [ -f "$arm" ] && [ -f "$intel" ]; then
  mkdir -p "$out/Casks"
  sed -e "s/@VERSION@/$version/g" -e "s/@SHA256_ARM64@/$(sum "$arm")/" \
    -e "s/@SHA256_X86_64@/$(sum "$intel")/" \
    -e '1,/^[a-z]/{/^#/d}' \
    "$here/packaging/homebrew/Casks/teleport-desktop.rb" >"$out/Casks/teleport-desktop.rb"
  echo "$out/Casks/teleport-desktop.rb"
  written=1
else
  echo "no cask: $arm or $intel is missing" >&2
fi

linux=$dist/teleport-$version-x86_64-linux.tar.gz
if [ -f "$linux" ]; then
  mkdir -p "$out/Formula"
  sed -e "s/@VERSION@/$version/g" -e "s/@SHA256_LINUX_X86_64@/$(sum "$linux")/" \
    -e '1,/^[a-z]/{/^#/d}' \
    "$here/packaging/homebrew/Formula/teleport-desktop.rb" >"$out/Formula/teleport-desktop.rb"
  echo "$out/Formula/teleport-desktop.rb"
  written=1
else
  echo "no formula: $linux is missing" >&2
fi
[ "$written" = 1 ]
