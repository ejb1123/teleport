#!/bin/sh
# Installs one release artefact in a stock distribution container and checks
# it: installed system files (packages), `teleport doctor`, and a headless
# loopback session (test-source host, headless software-decoding client) that
# must print SMOKE PASS. Runs as root inside the container:
#
#   docker run --rm -v "$PWD/dist:/dist:ro" -v "$PWD/tools:/tools:ro" IMAGE \
#     sh /tools/package-smoke.sh KIND [/dist]
#
# KIND: deb (Debian, Ubuntu), rpm (Fedora, openSUSE), arch (.pkg.tar.zst on
# Arch, CachyOS, Manjaro), appimage, tarball. The release workflow runs it
# for every image (.github/workflows/release.yml); docs/distribution.md lists
# the results.
set -eu
kind=${1:?usage: package-smoke.sh deb|rpm|arch|appimage|tarball [DIST]}
dist=${2:-/dist}
port=4543

one() {
    # The single file matching a glob in $dist.
    # shellcheck disable=SC2086 # the glob must expand
    set -- $dist/$1
    [ $# -eq 1 ] && [ -e "$1" ] || { echo "expected one $dist/$1" >&2; exit 1; }
    echo "$1"
}

# shellcheck disable=SC1091 # the container's own file
echo "== $(. /etc/os-release && echo "$PRETTY_NAME"), $kind"
case $kind in
deb)
    export DEBIAN_FRONTEND=noninteractive
    apt-get update -qq >/dev/null
    apt-get install -y -qq "$(one 'teleport_*.deb')" >/dev/null
    teleport=teleport
    ;;
rpm)
    if command -v dnf >/dev/null 2>&1; then
        dnf install -y -q "$(one 'teleport-*.rpm')" >/dev/null
    elif command -v zypper >/dev/null 2>&1; then
        # Unsigned: the release's packages carry no repository signature.
        zypper --non-interactive --quiet install --allow-unsigned-rpm "$(one 'teleport-*.rpm')"
    else
        echo "no dnf or zypper" >&2
        exit 1
    fi
    teleport=teleport
    ;;
arch)
    # CachyOS's pacman runs hooks (sysusers, udev) in a network sandbox,
    # which an unprivileged container cannot create; not needed on a host.
    if grep -q '^ID=cachyos' /etc/os-release; then
        sed -i 's/^\[options\]$/[options]\nDisableSandboxNetwork/' /etc/pacman.conf
    fi
    pacman -Sy --noconfirm --needed --quiet archlinux-keyring >/dev/null 2>&1 || true
    pacman -U --noconfirm "$(one 'teleport-desktop-bin-*.pkg.tar.zst')"
    teleport=teleport
    ;;
appimage)
    # No FUSE in a container: unpack to a temporary directory instead.
    export APPIMAGE_EXTRACT_AND_RUN=1
    # Artifact downloads drop the executable bit, and /dist is read-only.
    cp "$(one 'Teleport-*.AppImage')" /tmp/Teleport.AppImage
    chmod +x /tmp/Teleport.AppImage
    teleport=/tmp/Teleport.AppImage
    ;;
tarball)
    mkdir -p /opt/teleport-tarball
    tar -xzf "$(one 'teleport-*-linux.tar.gz')" -C /opt/teleport-tarball
    set -- /opt/teleport-tarball/teleport-*/AppRun
    teleport=$1
    ;;
*)
    echo "unknown kind $kind" >&2
    exit 2
    ;;
esac

case $kind in
deb | rpm | arch)
    for file in /usr/bin/teleport /usr/lib/systemd/user/teleport-desktop.service \
        /usr/lib/udev/rules.d/70-teleport.rules /usr/lib/modules-load.d/teleport.conf \
        /usr/lib/sysctl.d/90-teleport.conf /usr/share/applications/dev.teleport.Teleport.desktop \
        /usr/share/applications/dev.teleport.Host.desktop \
        /usr/share/icons/hicolor/scalable/apps/dev.teleport.Teleport.svg \
        /usr/share/icons/hicolor/256x256/apps/dev.teleport.Teleport.png; do
        [ -e "$file" ] || { echo "missing $file" >&2; exit 1; }
    done
    echo "== system files installed"
    # The .deb/.rpm create it in post-install, the Arch package through
    # pacman's systemd-sysusers hook.
    getent group uinput >/dev/null || { echo "no uinput group" >&2; exit 1; }
    echo "== uinput group exists"
    ;;
esac

export HOME=/tmp/teleport-smoke
mkdir -p "$HOME"
cd "$HOME"
"$teleport" --version
"$teleport" doctor
"$teleport" host --source test --encoder software --listen "127.0.0.1:$port" \
    --pairing-file pairing.json >host.log 2>&1 &
host=$!
i=0
while [ ! -s pairing.json ] && [ $i -lt 300 ]; do
    sleep 0.2
    i=$((i + 1))
done
[ -s pairing.json ] || { cat host.log; exit 1; }
status=0
"$teleport" client "127.0.0.1:$port" --pairing-file pairing.json --headless-frames 60 \
    --software-decoder >client.log 2>&1 || status=$?
kill "$host" 2>/dev/null || true
wait "$host" 2>/dev/null || true
if [ "$status" != 0 ] || ! grep -q "SMOKE PASS" client.log; then
    cat client.log host.log
    exit 1
fi
grep -o "SMOKE PASS.*frames=[0-9]*" client.log | cut -c1-60
grep -o "decoder=[a-z0-9:]*" client.log | tail -n 1
echo "== $kind OK"
