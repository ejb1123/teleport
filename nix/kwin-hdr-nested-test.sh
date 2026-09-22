#!/usr/bin/env bash
# Synthetic compositor test only. Never attaches to the real portal/desktop.
set -euo pipefail

if [[ $# != 2 ]]; then
  echo "usage: $0 /nix/store/...-kwin /absolute/path/to/nested-capture" >&2
  exit 2
fi
nested_kwin=$1
nested_helper=$2
[[ -x "$nested_kwin/bin/kwin_wayland" && -x "$nested_helper" ]]

if [[ ${TELEPORT_HDR_PRIVATE_SESSION:-} != 1 ]]; then
  nested_root=$(mktemp -d /tmp/teleport-hdr-nested.XXXXXX)
  mkdir -m 700 "$nested_root/runtime" "$nested_root/config" "$nested_root/data" "$nested_root/cache" "$nested_root/state"
  export XDG_RUNTIME_DIR="$nested_root/runtime"
  export XDG_CONFIG_HOME="$nested_root/config" XDG_DATA_HOME="$nested_root/data"
  export XDG_CACHE_HOME="$nested_root/cache" XDG_STATE_HOME="$nested_root/state"
  export TELEPORT_HDR_TEST_ROOT="$nested_root" TELEPORT_HDR_PRIVATE_SESSION=1
  unset DISPLAY WAYLAND_DISPLAY DBUS_SESSION_BUS_ADDRESS PIPEWIRE_REMOTE PIPEWIRE_RUNTIME_DIR
  unset QT_PLUGIN_PATH QML2_IMPORT_PATH QT_QPA_PLATFORM_PLUGIN_PATH
  exec dbus-run-session -- bash "$0" "$nested_kwin" "$nested_helper"
fi

# Refuse unsafe reuse of the private branch against the user's session.
[[ $XDG_RUNTIME_DIR == /tmp/teleport-hdr-nested.*/runtime ]]
[[ $XDG_DATA_HOME == "$TELEPORT_HDR_TEST_ROOT/data" ]]
export WAYLAND_DISPLAY=teleport-hdr-test PIPEWIRE_RUNTIME_DIR="$XDG_RUNTIME_DIR"
export QT_QPA_PLATFORM=offscreen KWIN_COMPOSE=O2
mkdir -p "$XDG_DATA_HOME/applications"
# A local trusted-helper declaration only in this synthetic session's data dir;
# do not disable KWin's permission checks, or modify the user's applications.
sed "s|@HELPER@|$nested_helper|g" "$(dirname "$0")/kwin-hdr-test.desktop.in" > "$XDG_DATA_HOME/applications/teleport-hdr-test.desktop"
nested_pipewire_pid= nested_kwin_pid= nested_surface_pid=
cleanup() {
  for nested_pid in "$nested_surface_pid" "$nested_kwin_pid" "$nested_pipewire_pid"; do
    if [[ -n $nested_pid ]]; then kill "$nested_pid" 2>/dev/null || true; wait "$nested_pid" 2>/dev/null || true; fi
  done
  echo "Synthetic-session logs retained at $TELEPORT_HDR_TEST_ROOT"
}
trap cleanup EXIT
pipewire > "$TELEPORT_HDR_TEST_ROOT/pipewire.log" 2>&1 &
nested_pipewire_pid=$!
for ((i=0;i<100;i++)); do [[ -S $XDG_RUNTIME_DIR/pipewire-0 ]] && break; sleep 0.1; done
[[ -S $XDG_RUNTIME_DIR/pipewire-0 ]]
"$nested_kwin/bin/kwin_wayland" --virtual --socket "$WAYLAND_DISPLAY" --width 128 --height 128 --no-lockscreen --no-global-shortcuts --no-kactivities > "$TELEPORT_HDR_TEST_ROOT/kwin.log" 2>&1 &
nested_kwin_pid=$!
for ((i=0;i<200;i++)); do [[ -S $XDG_RUNTIME_DIR/$WAYLAND_DISPLAY ]] && break; sleep 0.1; done
[[ -S $XDG_RUNTIME_DIR/$WAYLAND_DISPLAY ]]
gst-launch-1.0 -q videotestsrc pattern=white is-live=true ! video/x-raw,width=128,height=128 ! waylandsink fullscreen=true > "$TELEPORT_HDR_TEST_ROOT/surface.log" 2>&1 &
nested_surface_pid=$!
sleep 2
timeout 20 "$nested_helper" sdr
timeout 20 "$nested_helper" hdr
timeout 20 "$nested_helper" sdr
