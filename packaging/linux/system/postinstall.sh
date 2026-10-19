#!/bin/sh
# .deb/.rpm post-install: the group, device and PAM setup nix/module.nix does.
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
# .rpm only (rpm passes a number, dpkg "configure"; the .deb ships the policy
# as a conffile): /etc/pam.d/teleport for the system-login helper
# (/usr/lib/teleport/teleport-pam), with the distribution's own stack, unless
# one exists. Fedora and RHEL have system-auth, openSUSE common-auth (in
# /usr/lib/pam.d or /usr/etc/pam.d on newer releases). It is unused until a
# host runs with --system-auth-helper; rpm owns it as a %ghost file.
case "${1-}" in
[0-9]*)
    if [ ! -e /etc/pam.d/teleport ] && [ ! -L /etc/pam.d/teleport ]; then
        policy=
        for directory in /etc/pam.d /usr/lib/pam.d /usr/etc/pam.d; do
            if [ -e "$directory/system-auth" ]; then
                policy=system-auth
                break
            fi
            if [ -e "$directory/common-auth" ] && [ -e "$directory/common-account" ]; then
                policy=common-auth
                break
            fi
        done
        if [ -n "$policy" ]; then
            install -o root -g root -m 0644 "/usr/share/teleport/pam/teleport.$policy" \
                /etc/pam.d/teleport 2>/dev/null || true
        fi
    fi
    ;;
esac
if command -v update-desktop-database >/dev/null 2>&1; then
    update-desktop-database -q /usr/share/applications 2>/dev/null || true
fi
exit 0
