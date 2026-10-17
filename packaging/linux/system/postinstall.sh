#!/bin/sh
# .deb/.rpm post-install: the group and device setup nix/module.nix does.
# Nobody is added to the group and no service is enabled.
set -e
if ! getent group uinput >/dev/null 2>&1; then
    groupadd --system uinput 2>/dev/null || addgroup --system uinput 2>/dev/null || true
fi
modprobe uinput 2>/dev/null || true
modprobe uhid 2>/dev/null || true
if command -v udevadm >/dev/null 2>&1; then
    udevadm control --reload 2>/dev/null || true
    udevadm trigger --subsystem-match=misc 2>/dev/null || true
fi
if command -v sysctl >/dev/null 2>&1; then
    sysctl -q -p /usr/lib/sysctl.d/90-teleport.conf 2>/dev/null || true
fi
if command -v update-desktop-database >/dev/null 2>&1; then
    update-desktop-database -q /usr/share/applications 2>/dev/null || true
fi
exit 0
