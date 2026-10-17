#!/usr/bin/env bash
# Runs a command inside a private, headless KDE desktop: a virtual-framebuffer
# KWin, its own D-Bus session bus, PipeWire, permission store and desktop
# portals (xdg-desktop-portal + xdg-desktop-portal-kde), with private HOME,
# XDG and runtime directories. Nothing here talks to the caller's session:
# the user's bus, compositor, PipeWire and display configuration are never
# reached, and everything is torn down when the command exits.
#
#   tests/support/nested-kwin.sh [--outputs N] -- command...
#
# The command sees WAYLAND_DISPLAY, DBUS_SESSION_BUS_ADDRESS and
# XDG_RUNTIME_DIR of the nested desktop, with `dev.teleport.Host`
# pre-authorized for remote desktop (as the NixOS module does). KWin needs
# OpenGL (a render node or llvmpipe) for screencasting.
set -euo pipefail

outputs=1
while [ $# -gt 0 ]; do
    case "$1" in
        --outputs) outputs=$2; shift 2 ;;
        --) shift; break ;;
        *) break ;;
    esac
done

if [ "${NESTED_KWIN_INNER:-}" != 1 ]; then
    root=$(mktemp -d "${TMPDIR:-/tmp}/teleport-kwin.XXXXXX")
    chmod 700 "$root"
    mkdir -p "$root"/{home,config,data,state,cache,run,logs}
    chmod 700 "$root/run"
    status=0
    env -u DBUS_SESSION_BUS_ADDRESS -u WAYLAND_DISPLAY -u DISPLAY -u PIPEWIRE_REMOTE \
        -u PIPEWIRE_RUNTIME_DIR -u XDG_SESSION_ID -u XDG_SEAT -u XDG_VTNR \
        -u QT_PLUGIN_PATH -u QML2_IMPORT_PATH -u QT_QPA_PLATFORM_PLUGIN_PATH \
        NESTED_KWIN_INNER=1 NESTED_KWIN_ROOT="$root" HOME="$root/home" \
        XDG_CONFIG_HOME="$root/config" XDG_DATA_HOME="$root/data" \
        XDG_STATE_HOME="$root/state" XDG_CACHE_HOME="$root/cache" \
        XDG_RUNTIME_DIR="$root/run" \
        dbus-run-session -- "$0" --outputs "$outputs" -- "$@" || status=$?
    if [ "$status" -ne 0 ] || [ -n "${NESTED_KWIN_KEEP_LOGS:-}" ]; then
        for log in "$root"/logs/*.log; do
            echo "=== $(basename "$log")" >&2
            tail -n 40 "$log" >&2
        done
    fi
    # The portal starts the document portal, whose FUSE mount under run/doc
    # goes away a moment after the bus does.
    for _ in $(seq 50); do
        rm -rf "$root" 2>/dev/null && break
        fusermount3 -uz "$root/run/doc" 2>/dev/null || fusermount -uz "$root/run/doc" 2>/dev/null || true
        sleep 0.1
    done
    exit "$status"
fi

# Refuse to run against anything but the private directories and bus.
[[ $XDG_RUNTIME_DIR == "$NESTED_KWIN_ROOT/run" && $HOME == "$NESTED_KWIN_ROOT/home" ]]
[[ $XDG_CONFIG_HOME == "$NESTED_KWIN_ROOT/config" && -z ${WAYLAND_DISPLAY:-} ]]
[[ ${DBUS_SESSION_BUS_ADDRESS:-} == unix:* ]]
logs=$NESTED_KWIN_ROOT/logs
find_tool() {
    # Store paths behind the system profile's wrappers, so KWin runs without
    # the setcap wrapper and the portals without a user unit.
    local name=$1
    shift
    local candidate
    for candidate in "$@"; do
        if [ -x "$candidate" ]; then
            echo "$candidate"
            return
        fi
    done
    command -v "$name" || { echo "nested-kwin: $name not found" >&2; exit 77; }
}
for tool in pipewire wireplumber kscreen-doctor busctl; do
    command -v "$tool" >/dev/null || { echo "nested-kwin: $tool not found" >&2; exit 77; }
done
sw=/run/current-system/sw
kwin_wrapper=$(readlink -f "$sw/bin/kwin_wayland_wrapper" 2>/dev/null || true)
kwin=$(find_tool kwin_wayland "${kwin_wrapper%_wrapper}")
portal=$(find_tool xdg-desktop-portal "$(readlink -f "$sw/libexec/xdg-desktop-portal" 2>/dev/null || true)")
portal_kde=$(find_tool xdg-desktop-portal-kde "$(readlink -f "$sw/libexec/xdg-desktop-portal-kde" 2>/dev/null || true)")
permission_store=$(find_tool xdg-permission-store "$(dirname "$portal")/xdg-permission-store")

pids=()
cleanup() {
    for ((i = ${#pids[@]} - 1; i >= 0; i--)); do
        kill "${pids[$i]}" 2>/dev/null || true
    done
    wait 2>/dev/null || true
}
trap cleanup EXIT

pipewire >"$logs/pipewire.log" 2>&1 &
pids+=($!)
# WirePlumber exits at once ("Failed to connect to PipeWire") if it starts
# before PipeWire listens; screencasts would then never be linked and every
# pipewiresrc would wait for a format until it times out.
for _ in $(seq 100); do
    [ -S "$XDG_RUNTIME_DIR/pipewire-0" ] && [ -S "$XDG_RUNTIME_DIR/pipewire-0-manager" ] && break
    sleep 0.1
done
[ -S "$XDG_RUNTIME_DIR/pipewire-0-manager" ] || { echo "nested-kwin: PipeWire did not start" >&2; exit 1; }
# Links a consumer (pipewiresrc) to the screencast node. The `policy`
# profile has no hardware monitors: no ALSA, Bluetooth or cameras of the
# real machine are touched.
wireplumber --profile policy >"$logs/wireplumber.log" 2>&1 &
wireplumber=$!
pids+=($!)
"$kwin" --virtual --socket teleport-test --no-lockscreen --width 1280 --height 720 \
    --output-count "$outputs" >"$logs/kwin.log" 2>&1 &
pids+=($!)
for _ in $(seq 100); do
    [ -S "$XDG_RUNTIME_DIR/teleport-test" ] && break
    sleep 0.1
done
[ -S "$XDG_RUNTIME_DIR/teleport-test" ] || { echo "nested-kwin: KWin did not start" >&2; exit 1; }

export WAYLAND_DISPLAY=teleport-test XDG_CURRENT_DESKTOP=KDE XDG_SESSION_TYPE=wayland \
    QT_QPA_PLATFORM=wayland KDE_FULL_SESSION=true XDG_SESSION_DESKTOP=KDE
dbus-update-activation-environment WAYLAND_DISPLAY XDG_CURRENT_DESKTOP XDG_SESSION_TYPE \
    QT_QPA_PLATFORM KDE_FULL_SESSION XDG_SESSION_DESKTOP HOME XDG_CONFIG_HOME \
    XDG_DATA_HOME XDG_STATE_HOME XDG_CACHE_HOME XDG_RUNTIME_DIR

"$permission_store" >"$logs/permission-store.log" 2>&1 &
pids+=($!)
for _ in $(seq 50); do
    # busctl honours DBUS_SESSION_BUS_ADDRESS: the private bus.
    busctl --user call org.freedesktop.impl.portal.PermissionStore \
        /org/freedesktop/impl/portal/PermissionStore \
        org.freedesktop.impl.portal.PermissionStore SetPermission sbssas \
        kde-authorized true remote-desktop dev.teleport.Host 1 yes \
        >/dev/null 2>&1 && break
    sleep 0.1
done
QT_LOGGING_RULES="${NESTED_KWIN_PORTAL_LOGGING:-*.debug=false}" "$portal_kde" \
    >"$logs/portal-kde.log" 2>&1 &
pids+=($!)
for _ in $(seq 50); do
    busctl --user status org.freedesktop.impl.portal.desktop.kde >/dev/null 2>&1 && break
    sleep 0.1
done
"$portal" >"$logs/portal.log" 2>&1 &
pids+=($!)
for _ in $(seq 50); do
    busctl --user get-property org.freedesktop.portal.Desktop /org/freedesktop/portal/desktop \
        org.freedesktop.portal.ScreenCast AvailableSourceTypes >/dev/null 2>&1 && break
    sleep 0.1
done
kill -0 "$wireplumber" 2>/dev/null || {
    echo "nested-kwin: WirePlumber exited: $(cat "$logs/wireplumber.log")" >&2
    exit 1
}

"$@"
