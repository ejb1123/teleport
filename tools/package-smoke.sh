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
    bundle=${teleport%/AppRun}
    for file in usr/share/doc/teleport/wake-and-unattended.md usr/libexec/teleport/teleport-pam \
        usr/share/teleport/system/teleport-lock-on-login.service \
        usr/share/teleport/examples/sddm-autologin.conf; do
        [ -e "$bundle/$file" ] || { echo "missing $file in the tarball" >&2; exit 1; }
    done
    echo "== tarball carries the documentation, PAM helper and unattended files"
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
    # Unattended start (docs/wake-and-unattended.md) and system login
    # (docs/system-login.md).
    for file in /usr/lib/systemd/user/teleport-lock-on-login.service \
        /usr/share/teleport/examples/sddm-autologin.conf \
        /usr/share/teleport/examples/kscreenlockerrc \
        /usr/lib/teleport/teleport-pam /etc/pam.d/teleport; do
        [ -e "$file" ] || { echo "missing $file" >&2; exit 1; }
    done
    echo "== unattended and system-login files installed"
    # The documentation help text and units cite, unless the image tells
    # the package manager to drop /usr/share/doc (Ubuntu's minimal images,
    # the Arch image's NoExtract).
    if grep -qs '^path-exclude=/usr/share/doc/\*' /etc/dpkg/dpkg.cfg.d/* ||
        grep -qs '^NoExtract.*usr/share/doc/\*' /etc/pacman.conf; then
        echo "== /usr/share/doc excluded by this image's package manager configuration"
        if [ "$kind" = arch ]; then
            pacman -Qlq teleport-desktop-bin | grep -qx /usr/share/doc/teleport/wake-and-unattended.md ||
                { echo "the package has no /usr/share/doc/teleport" >&2; exit 1; }
            echo "== documentation in the package"
        fi
    else
        for file in README.md distribution.md system-login.md wake-and-unattended.md guests.md; do
            [ -s "/usr/share/doc/teleport/$file" ] || { echo "missing /usr/share/doc/teleport/$file" >&2; exit 1; }
        done
        echo "== documentation installed"
    fi
    # What the host's validate_helper (src/system_pam.rs) and the helper
    # itself require: a root-owned 0755 helper (never setuid) in root-owned
    # directories, and a root-owned policy not writable by group or others.
    mode() { stat -c '%u %g %a' "$1"; }
    [ "$(mode /usr/lib/teleport/teleport-pam)" = "0 0 755" ] ||
        { echo "teleport-pam is $(mode /usr/lib/teleport/teleport-pam), not 0 0 755" >&2; exit 1; }
    for directory in /usr/lib/teleport /usr/lib /usr /; do
        [ "$(stat -c '%u' "$directory")" = 0 ] && [ $(($(stat -c '%#a' "$directory") & 022)) = 0 ] ||
            { echo "$directory is $(mode "$directory")" >&2; exit 1; }
    done
    [ "$(mode /etc/pam.d/teleport)" = "0 0 644" ] ||
        { echo "/etc/pam.d/teleport is $(mode /etc/pam.d/teleport), not 0 0 644" >&2; exit 1; }
    # The helper runs on this system's glibc and libpam (a loader failure
    # exits 127 with a message): as root it refuses at once, silently.
    status=0
    output=$(/usr/lib/teleport/teleport-pam </dev/null 2>&1) || status=$?
    [ "$status" = 1 ] && [ -z "$output" ] ||
        { echo "teleport-pam as root: status $status, output: $output" >&2; exit 1; }
    # And it checks a real password with this distribution's PAM stack, as
    # the host runs it: as the account itself, request on stdin (TPAM0001,
    # big-endian lengths, user, password), "OK" on stdout only on success.
    # Fedora's image has util-linux-core only, without su.
    if ! command -v su >/dev/null 2>&1 && command -v dnf >/dev/null 2>&1; then
        dnf install -y -q util-linux >/dev/null
    fi
    useradd -m tpsmoke
    echo 'tpsmoke:Smoke-pass-1' | chpasswd
    pam_check() {
        # $1: password, $2: its length as an octal escape.
        printf "TPAM0001\\000\\000\\000\\007\\000\\000\\000\\$2%s%s" tpsmoke "$1" |
            su -s /bin/sh -c /usr/lib/teleport/teleport-pam tpsmoke 2>&1
    }
    [ "$(pam_check Smoke-pass-1 014)" = OK ] ||
        { echo "teleport-pam refused the right password" >&2; cat /etc/pam.d/teleport >&2; exit 1; }
    [ -z "$(pam_check Wrong-pass-1 014 || true)" ] ||
        { echo "teleport-pam accepted a wrong password" >&2; exit 1; }
    echo "== teleport-pam: right password accepted, wrong one refused ($(sed -n 's/^auth[[:space:]]*include[[:space:]]*//p' /etc/pam.d/teleport))"
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
