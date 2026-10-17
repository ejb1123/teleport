#!/usr/bin/env bash
# Turn the macOS `nix build` result into a self-contained, signed and
# notarised Teleport.app (docs/distribution.md, "macOS application").
#
#   tools/macos-app.sh [options]
#
#   --result PATH        nix build output (default: ./result, built if missing)
#   --out DIR            where to write Teleport.app and archives (default: dist)
#   --identity NAME      codesign identity, e.g. "Developer ID Application: Jane
#                        Doe (TEAMID1234)"; default "-" (ad-hoc: runs on this
#                        Mac only, cannot be notarised)
#   --keychain PATH      keychain holding the identity (CI)
#   --notary-profile P   notarise with a `xcrun notarytool store-credentials`
#                        profile, or set the environment instead:
#                          NOTARY_KEY (path to an App Store Connect API .p8),
#                          NOTARY_KEY_ID, NOTARY_ISSUER; or
#                          NOTARY_APPLE_ID, NOTARY_TEAM_ID, NOTARY_PASSWORD
#                          (an app-specific password)
#   --dmg                also build, sign (and notarise) Teleport-VERSION-ARCH.dmg
#   --skip-notarize      sign but do not notarise even if credentials are set
#
# What it does:
#   1. copies the unwrapped Rust binary, GStreamer's plugin scanner, the
#      plugins, libfido2 and every /nix/store dylib they load into the bundle
#      (Contents/MacOS, PlugIns/gstreamer-1.0, Frameworks), rewriting install
#      names to @rpath so nothing refers to /nix/store any more;
#   2. writes Info.plist (bundle id dev.teleport.Teleport, version, microphone
#      usage string), Teleport.icns and the UI font;
#   3. signs every Mach-O inside-out with the hardened runtime
#      (`codesign --options runtime --timestamp`) and the entitlements in
#      packaging/macos/Teleport.entitlements, then the bundle;
#   4. with credentials: `notarytool submit --wait`, `stapler staple`,
#      `spctl --assess` and a zip of the stapled app.
#
# The bundled binary finds its plugins, font and libfido2 from its own
# location (app_bundle_environment in src/main.rs), not a wrapper script.
# Needs the Xcode command line tools (codesign, install_name_tool, otool,
# notarytool, stapler, iconutil, sips, PlistBuddy, ditto, hdiutil).
set -euo pipefail

root=$(cd "$(dirname "$0")/.." && pwd)
result=$root/result
out=$root/dist
identity=-
keychain=
notary_profile=
make_dmg=0
skip_notarize=0

die() {
    echo "macos-app: $*" >&2
    exit 1
}

while [ $# -gt 0 ]; do
    case "$1" in
    --result) result=$2; shift 2 ;;
    --out) out=$2; shift 2 ;;
    --identity) identity=$2; shift 2 ;;
    --keychain) keychain=$2; shift 2 ;;
    --notary-profile) notary_profile=$2; shift 2 ;;
    --dmg) make_dmg=1; shift ;;
    --skip-notarize) skip_notarize=1; shift ;;
    -h | --help) sed -n '2,/^set -euo/p' "$0" | sed '$d; s/^# \{0,1\}//'; exit 0 ;;
    *) die "unknown option $1 (see --help)" ;;
    esac
done

[ "$(uname -s)" = Darwin ] || die "runs on macOS only"
for tool in codesign install_name_tool otool iconutil sips ditto xcrun /usr/libexec/PlistBuddy; do
    command -v "$tool" >/dev/null || die "$tool not found (install the Xcode command line tools)"
done

if [ ! -e "$result" ]; then
    echo "Building the Nix package..."
    nix build "$root" -o "$result"
fi
wrapper=$result/bin/teleport
binary=$result/bin/.teleport-wrapped
[ -x "$binary" ] || die "$binary not found; is $result a macOS build of this flake?"

version=$(sed -n 's/^version = "\(.*\)"/\1/p' "$root/Cargo.toml" | head -n 1)
arch=$(lipo -archs "$binary" 2>/dev/null || uname -m)
[ -n "$version" ] || die "no version in Cargo.toml"

# Paths the Nix wrapper script sets: plugins, font and libfido2.
store_paths() {
    grep -o "/nix/store/[^'\":\$ ]*$1" "$wrapper" | sort -u
}
plugin_dirs=$(store_paths '/lib/gstreamer-1.0')
font=$(store_paths 'DejaVuSans.ttf' | head -n 1)
libfido2=$(store_paths '/libfido2[.0-9]*dylib' | head -n 1)
[ -n "$plugin_dirs" ] || die "no GStreamer plugin directories in $wrapper"
[ -f "$font" ] || die "no UI font in $wrapper"
[ -f "$libfido2" ] || die "no libfido2 in $wrapper"
# GStreamer's plugin scanner lives next to the library the binary loads.
gst_lib=$(otool -L "$binary" | awk '/libgstreamer-1\.0\..*dylib/ { print $1; exit }')
[ -n "$gst_lib" ] || die "the binary does not link libgstreamer"
scanner=$(dirname "$(dirname "$gst_lib")")/libexec/gstreamer-1.0/gst-plugin-scanner
[ -x "$scanner" ] || die "gst-plugin-scanner not found at $scanner"

app=$out/Teleport.app
contents=$app/Contents
rm -rf "$app"
mkdir -p "$contents/MacOS" "$contents/Frameworks" "$contents/PlugIns/gstreamer-1.0" \
    "$contents/Resources/fonts"

# Bundled Mach-O files, for rewriting and signing.
machos=()

# install -m 755, writable so install_name_tool can edit the copy.
copy() {
    cp -L "$1" "$2"
    chmod 755 "$2"
    machos+=("$2")
}

# /nix/store dylibs a Mach-O file loads (system libraries are left alone).
nix_deps() {
    otool -L "$1" | tail -n +2 | awk '{ print $1 }' | grep '^/nix/store/' || true
}

# Copies each /nix/store dependency into Frameworks under the name it is
# referred to by, recursively. (No associative arrays: macOS ships bash 3.2.)
bundle_deps() {
    local dep name
    while IFS= read -r dep; do
        [ -n "$dep" ] || continue
        name=$(basename "$dep")
        [ ! -e "$contents/Frameworks/$name" ] || continue
        [ -f "$dep" ] || die "missing dependency $dep"
        copy "$dep" "$contents/Frameworks/$name"
        bundle_deps "$dep"
    done < <(nix_deps "$1")
}

copy "$binary" "$contents/MacOS/teleport"
bundle_deps "$binary"
copy "$scanner" "$contents/MacOS/gst-plugin-scanner"
bundle_deps "$scanner"
for dir in $plugin_dirs; do
    for plugin in "$dir"/*.dylib "$dir"/*.so; do
        [ -e "$plugin" ] || continue
        name=$(basename "$plugin")
        [ ! -e "$contents/PlugIns/gstreamer-1.0/$name" ] || continue
        copy "$plugin" "$contents/PlugIns/gstreamer-1.0/$name"
        bundle_deps "$plugin"
    done
done
# libfido2 is opened by path (TELEPORT_LIBFIDO2), so it keeps a fixed name.
fido_name=$(basename "$(otool -D "$libfido2" | tail -n 1)")
if [ ! -e "$contents/Frameworks/$fido_name" ]; then
    copy "$libfido2" "$contents/Frameworks/$fido_name"
    bundle_deps "$libfido2"
fi
[ "$fido_name" = libfido2.dylib ] || ln -sf "$fido_name" "$contents/Frameworks/libfido2.dylib"

# Rewrite every /nix/store reference to @rpath and drop Nix's rpaths.
for file in "${machos[@]}"; do
    args=()
    while IFS= read -r dep; do
        [ -n "$dep" ] || continue
        args+=(-change "$dep" "@rpath/$(basename "$dep")")
    done < <(nix_deps "$file")
    case "$file" in
    */Frameworks/*)
        args+=(-id "@rpath/$(basename "$file")")
        rpath=@loader_path
        ;;
    */PlugIns/*) rpath=@loader_path/../../Frameworks ;;
    *) rpath=@executable_path/../Frameworks ;;
    esac
    while IFS= read -r old; do
        args+=(-delete_rpath "$old")
    done < <(otool -l "$file" | awk '/cmd LC_RPATH/ { getline; getline; print $2 }' | sort -u)
    args+=(-add_rpath "$rpath")
    install_name_tool "${args[@]}" "$file"
done
leftover=$(for file in "${machos[@]}"; do otool -L "$file" | tail -n +2; done | grep -c /nix/store || true)
[ "$leftover" -eq 0 ] || die "$leftover references to /nix/store remain"

# Resources: font, icon, Info.plist.
cp -L "$font" "$contents/Resources/fonts/DejaVuSans.ttf"
iconset=$(mktemp -d)/Teleport.iconset
mkdir -p "$iconset"
for size in 16 32 128 256 512; do
    sips -z "$size" "$size" "$root/packaging/icons/dev.teleport.Teleport.png" \
        --out "$iconset/icon_${size}x${size}.png" >/dev/null
    double=$((size * 2))
    sips -z "$double" "$double" "$root/packaging/icons/dev.teleport.Teleport.png" \
        --out "$iconset/icon_${size}x${size}@2x.png" >/dev/null
done
iconutil -c icns "$iconset" -o "$contents/Resources/Teleport.icns"
rm -rf "$(dirname "$iconset")"

plist=$contents/Info.plist
cp "$root/packaging/Info.plist" "$plist"
chmod 644 "$plist"
build=$(cd "$root" && git rev-list --count HEAD 2>/dev/null || echo 1)
/usr/libexec/PlistBuddy -c "Set :CFBundleShortVersionString $version" "$plist"
/usr/libexec/PlistBuddy -c "Set :CFBundleVersion $build" "$plist"
minos=$(otool -l "$binary" | awk '/cmd LC_BUILD_VERSION/ { f = 1 } f && $1 == "minos" { print $2; exit }')
if [ -n "$minos" ]; then
    /usr/libexec/PlistBuddy -c "Add :LSMinimumSystemVersion string $minos" "$plist"
fi
plutil -lint "$plist" >/dev/null

# Sign inside-out: libraries and plugins, helpers, then the main executable
# with its entitlements, then the bundle. --timestamp needs a real identity.
# Ad-hoc signatures carry no Team ID, which the hardened runtime's library
# validation would reject, so they go without it.
if [ "$identity" = - ]; then
    sign=(codesign --force --sign -)
else
    sign=(codesign --force --options runtime --sign "$identity" --timestamp)
fi
if [ -n "$keychain" ]; then
    sign+=(--keychain "$keychain")
fi
entitlements=$root/packaging/macos/Teleport.entitlements
for file in "${machos[@]}"; do
    case "$file" in
    "$contents"/MacOS/*) ;;
    *) "${sign[@]}" "$file" ;;
    esac
done
# The plugin scanner runs plugin code too (ORC), so it gets the same set.
"${sign[@]}" --entitlements "$entitlements" "$contents/MacOS/gst-plugin-scanner"
"${sign[@]}" --entitlements "$entitlements" "$contents/MacOS/teleport"
"${sign[@]}" --entitlements "$entitlements" "$app"
codesign --verify --strict --deep --verbose=2 "$app"
"$contents/MacOS/teleport" --version

zip=$out/Teleport-$version-$arch.zip
notarize() {
    if [ -n "$notary_profile" ]; then
        xcrun notarytool submit "$1" --keychain-profile "$notary_profile" --wait
    elif [ -n "${NOTARY_KEY-}" ]; then
        xcrun notarytool submit "$1" --key "$NOTARY_KEY" --key-id "${NOTARY_KEY_ID:?}" \
            --issuer "${NOTARY_ISSUER:?}" --wait
    else
        xcrun notarytool submit "$1" --apple-id "${NOTARY_APPLE_ID:?}" \
            --team-id "${NOTARY_TEAM_ID:?}" --password "${NOTARY_PASSWORD:?}" --wait
    fi
}
can_notarize=0
if [ "$skip_notarize" = 0 ] && [ "$identity" != - ] &&
    { [ -n "$notary_profile" ] || [ -n "${NOTARY_KEY-}" ] || [ -n "${NOTARY_APPLE_ID-}" ]; }; then
    can_notarize=1
fi
if [ "$can_notarize" = 1 ]; then
    # notarytool takes a zip; the ticket is stapled to the app itself.
    ditto -c -k --keepParent "$app" "$zip"
    notarize "$zip"
    xcrun stapler staple "$app"
    xcrun stapler validate "$app"
    spctl --assess --type execute --verbose=2 "$app"
else
    echo "Not notarised (needs a Developer ID identity and notary credentials)." >&2
fi
rm -f "$zip"
ditto -c -k --keepParent "$app" "$zip"

if [ "$make_dmg" = 1 ]; then
    dmg=$out/Teleport-$version-$arch.dmg
    staging=$(mktemp -d)
    cp -R "$app" "$staging/"
    ln -s /Applications "$staging/Applications"
    rm -f "$dmg"
    hdiutil create -volname Teleport -srcfolder "$staging" -fs HFS+ -format UDZO "$dmg" >/dev/null
    rm -rf "$staging"
    if [ "$identity" != - ]; then
        sign_dmg=(codesign --force --sign "$identity" --timestamp)
        if [ -n "$keychain" ]; then
            sign_dmg+=(--keychain "$keychain")
        fi
        "${sign_dmg[@]}" "$dmg"
    fi
    if [ "$can_notarize" = 1 ]; then
        notarize "$dmg"
        xcrun stapler staple "$dmg"
    fi
    echo "$dmg"
fi
echo "$app"
echo "$zip"
