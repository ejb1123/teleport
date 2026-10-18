#!/usr/bin/env bash
# The release version lives in Cargo.toml; these files repeat it and must
# agree with it (the flake reads Cargo.toml itself):
#   Cargo.lock, packaging/Info.plist, the AppStream metainfo's newest
#   <release>, packaging/arch/PKGBUILD.
#
#   tools/release-version.sh set 0.6.0    # bump them all (metainfo dated today)
#   tools/release-version.sh check v0.6.0 # the release workflow, on the tag
#   tools/release-version.sh check        # just that the files agree
#
# docs/distribution.md, "Making a release", has the whole procedure.
set -euo pipefail
cd "$(dirname "$0")/.."
metainfo=packaging/linux/dev.teleport.Teleport.metainfo.xml

cargo_version() { sed -n 's/^version = "\(.*\)"$/\1/p' Cargo.toml | head -n 1; }

case "${1-}" in
set)
  version=${2:?usage: $0 set VERSION}
  version=${version#v}
  [[ $version =~ ^[0-9]+\.[0-9]+\.[0-9]+([-+.][0-9A-Za-z.]+)?$ ]] ||
    { echo "not a version: $version" >&2; exit 2; }
  old=$(cargo_version)
  sed -i "0,/^version = \".*\"$/s//version = \"$version\"/" Cargo.toml
  sed -i "/^name = \"teleport\"$/{n;s/^version = \".*\"$/version = \"$version\"/}" Cargo.lock
  sed -i "s|<key>CFBundleShortVersionString</key><string>[^<]*</string>|<key>CFBundleShortVersionString</key><string>$version</string>|" packaging/Info.plist
  if ! grep -q "<release version=\"$version\"" "$metainfo"; then
    sed -i "0,/^    <release /s||    <release version=\"$version\" date=\"$(date -u +%F)\"/>\n&|" "$metainfo"
  fi
  sed -i -e "s/^_version=.*/_version=$version/" -e "s/^pkgver=.*/pkgver=${version//-/_}/" \
    -e "s/^pkgrel=.*/pkgrel=1/" packaging/arch/PKGBUILD
  # .SRCINFO as makepkg --printsrcinfo would write it (the release workflow
  # regenerates it, with the real checksum, for the AUR).
  old_pkgver=$(sed -n 's/^\tpkgver = //p' packaging/arch/.SRCINFO)
  sed -i -e "s/^\tpkgver = .*/\tpkgver = ${version//-/_}/" -e "s/^\tpkgrel = .*/\tpkgrel = 1/" \
    -e "s|/v[^/]*/teleport-[^/]*-x86_64-linux.tar.gz|/v$version/teleport-$version-x86_64-linux.tar.gz|" \
    packaging/arch/.SRCINFO
  [ -n "$old_pkgver" ] || echo "packaging/arch/.SRCINFO has no pkgver" >&2
  echo "$old -> $version"
  "$0" check "$version"
  ;;
check)
  version=$(cargo_version)
  status=0
  fail() { echo "version mismatch: $*" >&2; status=1; }
  if [ -n "${2-}" ]; then
    tag=${2#refs/tags/}
    [ "${tag#v}" = "$version" ] || fail "tag $tag, Cargo.toml $version (tags are v<Cargo.toml version>)"
  fi
  lock=$(sed -n '/^name = "teleport"$/{n;s/^version = "\(.*\)"$/\1/p}' Cargo.lock)
  [ "$lock" = "$version" ] || fail "Cargo.lock $lock (run cargo update -p teleport --offline)"
  plist=$(sed -n 's|.*<key>CFBundleShortVersionString</key><string>\([^<]*\)</string>.*|\1|p' packaging/Info.plist)
  [ "$plist" = "$version" ] || fail "packaging/Info.plist $plist"
  newest=$(sed -n 's|^ *<release version="\([^"]*\)".*|\1|p' "$metainfo" | head -n 1)
  [ "$newest" = "$version" ] || fail "$metainfo newest release $newest"
  pkgver=$(sed -n 's/^_version=//p' packaging/arch/PKGBUILD)
  [ "$pkgver" = "$version" ] || fail "packaging/arch/PKGBUILD _version $pkgver"
  [ "$status" = 0 ] && echo "version $version consistent"
  exit "$status"
  ;;
*)
  sed -n '2,11p' "$0" >&2
  exit 2
  ;;
esac
