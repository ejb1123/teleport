# Wake-on-LAN, sleep, and unattended start (G23, G24)

Status (2026-10-17): implemented for Linux hosts (KDE Plasma 6 on NixOS
tested by evaluation and namespace tests only; see "Verified" below).
2026-10-19: unattended start for the .deb, .rpm and Arch packages
("Unattended start without NixOS"). The
client side (launcher, `teleport wake`, `teleport suspend`) works on Linux
and macOS.

## Waking a sleeping desktop

Each host tells its paired clients, inside the authenticated session, how to
wake it (`Desktop::wake`): the MAC address and IPv4 network of the interface
it is reached on (the one holding `--listen`'s address, or the default
route's), and whether Wake-on-LAN by magic packet is armed there (`ethtool`
"Wake-on: g", read without privilege). The client saves this in the
desktop's profile (`wake` in `profiles.json`), but only for the saved
desktop whose pin matches the identity the session authenticated.

The launcher then shows an offline desktop as **Asleep or off · Wake
available**, with **Wake** and **Sleep** buttons and the Wake-on-LAN state.
**Wake**:

1. sends the magic packet (102 bytes: six `0xff`, then the MAC sixteen
   times) to UDP port 9 at `255.255.255.255`, at each local interface's
   subnet broadcast, and at the desktop's saved subnet broadcast;
2. asks every saved **wake relay** on the desktop's subnet to send it there
   (below);
3. waits up to two minutes, resending every 10 s, for the desktop's signed
   presence through its relay or a direct authenticated answer (a `ping`
   request over QUIC with the pinned certificate).

From a terminal:

```sh
teleport wake home:4443 --wait 120          # a saved desktop
teleport wake --mac 00:1b:21:3c:4d:5e       # any machine on this LAN
teleport wake-listen                        # on the sleeping machine (while awake):
                                            # prints magic packets that arrive
```

A magic packet is neither secret nor authenticated: anybody on the LAN can
always wake a machine. Teleport only decides who can make *its hosts* send
one.

### Remote wake through a wake relay

From outside the LAN, a broadcast cannot reach the sleeping machine. If
another of your Teleport hosts on that LAN is always on (a small server, a
second PC), start it with `--wake-relay` (NixOS `wakeRelay = true`). Pair
this client with it as well and connect once, so its profile records the
role and its network. The launcher's Wake then also sends a `wake` request
to it, and it broadcasts the packet on its own networks.

- The request is an ordinary authenticated connection whose options carry
  `command=wake&mac=…`: over QUIC to the relay host's address, or through
  the **end-to-end encrypted relay**, where the options are bound into the
  handshake transcript. The relay host checks the credential exactly as for
  a desktop session (device keys, revocation, `--relay-require-device-key`)
  and only then acts. It never captures its desktop for a command.
- **The MoQ relay cannot trigger wakes**: it cannot complete the handshake
  for any credential, cannot alter the options, and a replayed hello is
  answered once and never confirmed.
- Owner-only: `access::SessionLease::owner` is the single check point and
  reads the role from the access record at request time. Devices paired with
  a code, password or Linux login, the host pairing file and system-login
  tickets are owners; invited guest devices (docs/guests.md) are refused
  `wake` and `suspend` whatever they were granted, directly and through the
  relay (`ping` stays open to them). Guests are also not told the host's
  hardware address, network or power options (`Desktop.wake`).
- Opt-in, bounded: hosts refuse `wake` without `--wake-relay`, wake only
  unicast addresses (optionally only `--wake-relay-allow MAC`, repeatable),
  and at most 6 per minute.
- `teleport wake --mac MAC --via ADDRESS` asks one wake relay explicitly (a
  saved desktop, or any address with `--via-pairing-file` and the relay
  options, as for `teleport client`).

Saved wake relays are chosen automatically when their saved IPv4 network
contains the target's (same address and prefix). If a relay host is on
several networks, it broadcasts on all of them.

### Sleep on request

`--allow-suspend` (NixOS `allowSuspend = true`) lets the owner's devices
suspend the host: the launcher's **Sleep** (two clicks within 5 s) or
`teleport suspend ADDRESS`. The host answers first, then runs
`systemctl suspend` 2 s later. The NixOS option also grants the allowed users
logind's suspend action through polkit, because a user service is not in a
login session and would otherwise be asked for a password. Suspend still
respects inhibitors (`systemd-inhibit`).

### Host setup and firmware

```nix
services.teleport-desktop = {
  wakeOnLan.interface = "enp5s0";   # networking.interfaces.enp5s0.wakeOnLan
  allowSuspend = true;
  # On an always-on machine instead:
  # wakeRelay = true;
};
```

Wake-on-LAN also needs the firmware and the card:

- UEFI/BIOS: enable "Wake on LAN", "Power On By PCI-E/PCI" or similar, and
  disable "ErP"/"EuP" (deep power saving cuts standby power to the card).
- The card must keep power and link while the machine sleeps; check with
  `ethtool enp5s0` ("Wake-on: g") after a reboot.
- Wired Ethernet. Wake on wireless LAN rarely works and needs other setup.
- From suspend (S3) it usually works; from power-off (S5) only if the
  firmware allows it.
- Switches forget the sleeping machine's port after a few minutes; the
  broadcast reaches it anyway, which is why Teleport never sends unicast.
- NetworkManager desktops can also set it per connection
  (`nmcli connection modify … 802-3-ethernet.wake-on-lan magic`).

The host logs `power and session control mac=… wake_on_lan=… network=…`
at start, and warns when Wake-on-LAN is off on the card.

## Unattended start after a reboot (G24)

Parsec's Windows service runs as SYSTEM, starts the app in the winlogon
session to see the login and UAC screens, and sends Ctrl+Alt+Del
(parsec-comparison 1.20). The Linux equivalent Teleport implements is
**automatic login plus an immediate lock** (option a):

```nix
services.teleport-desktop = {
  enable = true;
  users = [ "ej" ];
  systemLogin = true;                  # Linux-password login (docs/system-login.md)
  preauthorizeRemoteDesktop = true;    # required: nobody is there to approve
  unattended = {
    enable = true;                     # SDDM logs ej in at boot, then it locks
    # lockEveryLogin = true;           # KDE: lock before the desktop shows
    # unlockOnSystemLogin = true;      # unlock after a PAM-verified login
  };
};
```

- `services.displayManager.autoLogin` logs the user in at boot (SDDM:
  `[Autologin]` with the `plasma` session; only at boot, `Relogin=false`).
- The user service `teleport-lock-on-login` runs `teleport lock-session
  --only-automatic-login`: if logind's session was started by a display
  manager's automatic login (PAM service `*-autologin`), it locks it and
  waits until `LockedHint` is set, re-requesting every 2 s for up to 60 s.
  Password logins at the machine are left alone.
- `lockEveryLogin` (KDE) also sets `LockOnStart[$i]=true` in
  `/etc/xdg/kscreenlockerrc`, so KWin locks before the desktop is first
  drawn; then every login starts locked.
- The host starts with the session (as today) and, pre-authorized, captures
  the lock screen.

To get in: connect, press **Ctrl+Alt+End** (shows the password prompt) and
type the account password into the lock screen, exactly as at the machine.
With `unlockOnSystemLogin`, a **Linux account** sign-in (PAM-verified,
docs/system-login.md) unlocks the session through `loginctl unlock-session`
before the stream starts. The password is never stored; a saved-device
connection never unlocks anything by itself.
Only owners sign in with the Linux account, so only an owner's connection
unlocks, and only once that session has joined the desktop (not when its
connection fails or the host is full); guests already watching see the
desktop from then on, as they would after typing the password.

### Unattended start without NixOS (.deb, .rpm, Arch)

The .deb, .rpm and Arch (`teleport-desktop-bin`) packages install the same
pieces as files, all inactive until you turn them on (the tarball and the
AppImage carry them in `usr/share/teleport/` inside the bundle; install them
by hand at the paths below):

| NixOS option | Package file | Turned on by |
|---|---|---|
| host service | `/usr/lib/systemd/user/teleport-desktop.service` | `systemctl --user enable teleport-desktop` (distribution.md) |
| `unattended.enable`: lock after the automatic login | `/usr/lib/systemd/user/teleport-lock-on-login.service` | `systemctl --user enable teleport-lock-on-login` |
| `unattended.enable`: the automatic login (SDDM) | `/usr/share/teleport/examples/sddm-autologin.conf` | copying it to `/etc/sddm.conf.d/` |
| `unattended.lockEveryLogin` (KDE) | `/usr/share/teleport/examples/kscreenlockerrc` | copying it to `/etc/xdg/kscreenlockerrc` |
| `preauthorizeRemoteDesktop` | (KDE's permission store) | `flatpak permission-set` once, below |
| `systemLogin` | `/usr/lib/teleport/teleport-pam`, `/etc/pam.d/teleport` | `--system-auth-helper` (system-login.md) |
| `unattended.unlockOnSystemLogin` | | `--unlock-on-system-login` |
| `sessions.lockOnLastDisconnect` | | `--lock-on-last-disconnect` |

For the account `ej` on KDE Plasma 6 with SDDM, after the host service
works (distribution.md, "Debian, Ubuntu, Fedora: .deb and .rpm"):

```sh
# 1. As ej: pre-authorise the host with KDE's portal, once. Without it the
#    portal asks for consent on a screen nobody can see, and the host waits.
flatpak permission-set kde-authorized remote-desktop dev.teleport.Host yes
#    (without the flatpak command, the same through D-Bus:)
# busctl --user call org.freedesktop.impl.portal.PermissionStore \
#   /org/freedesktop/impl/portal/PermissionStore \
#   org.freedesktop.impl.portal.PermissionStore SetPermission \
#   sbssas kde-authorized true remote-desktop dev.teleport.Host 1 yes

# 2. As ej: lock the session the display manager logs in automatically.
systemctl --user enable teleport-lock-on-login

# 3. As root: SDDM logs ej in at boot (only at boot: Relogin=false).
sudo install -d /etc/sddm.conf.d
sudo install -m 0644 /usr/share/teleport/examples/sddm-autologin.conf \
  /etc/sddm.conf.d/teleport-autologin.conf
sudo sed -i 's/^User=USER$/User=ej/' /etc/sddm.conf.d/teleport-autologin.conf

# 4. Optional, as root (lockEveryLogin): KWin locks before the desktop is
#    first drawn, at every login. Merge by hand if the file exists.
sudo install -m 0644 /usr/share/teleport/examples/kscreenlockerrc /etc/xdg/kscreenlockerrc

# 5. Optional, as ej: Linux-password login that also unlocks, and locking
#    when the last session leaves. In ~/.config/teleport/host.env:
#    TELEPORT_HOST_ARGS=--system-auth-helper /usr/lib/teleport/teleport-pam --unlock-on-system-login --lock-on-last-disconnect
systemctl --user restart teleport-desktop
```

Check the session name in step 3: it is the file name in
`/usr/share/wayland-sessions/` (`plasma.desktop` for Plasma 6 on Wayland).
SDDM reads the files in `/etc/sddm.conf.d/` in name order and then
`/etc/sddm.conf`, later settings winning: if another of them already sets
`[Autologin]`, edit that one instead. Another display manager needs its own automatic
login (GDM: `AutomaticLoginEnable=True` and `AutomaticLogin=ej` in
`/etc/gdm/custom.conf`, `/etc/gdm3/daemon.conf` on Debian); the lock unit
works with any of them, since it only looks at logind's PAM service name
(`*-autologin`).

`teleport-lock-on-login` does nothing after a password login at the
machine. The host warns at start (`journalctl --user -u teleport-desktop`)
when it runs on KDE in a locked or automatically logged-in session that is
not pre-authorised. Read the security trade-offs below before enabling
step 3. To undo: remove `/etc/sddm.conf.d/teleport-autologin.conf` (and
`/etc/xdg/kscreenlockerrc`), `systemctl --user disable
teleport-lock-on-login`, and
`flatpak permission-remove kde-authorized remote-desktop dev.teleport.Host`.

### Secure attention: Ctrl+Alt+End

Ctrl+Alt+Del cannot be captured reliably on every client, so the client's
**Ctrl+Alt+End** (as in Windows Remote Desktop) sends `SecureAttention` to
hosts that advertise it (`Desktop::secure_attention`, a logind session was
found). Unlocked: the host locks the session (logind `lock-session`; the
Windows Ctrl+Alt+Del menu's first entry). Locked: it taps Shift, which wakes
the screen and shows the password field. Either way the client shows a
notice. End alone, and Ctrl+Alt+Del, are still forwarded unchanged.

Secure attention is the owner's: on a shared desktop (docs/guests.md) a
guest's request is refused before anything happens, whatever it was
granted, and guests are not offered it. Locking ends what every session
sees; typing the password into the lock screen needs keyboard and mouse
control like any other typing, so a guest with that permission could type
there too, but only with the password.

### Surviving lock, unlock and screen-off

Monitors waking from power saving hotplug their outputs, and KDE then closes
the RemoteDesktop portal session. During a session the host already noticed
this on the next failed injection or capture error and recreated it. Now it
also watches the portal's `Closed` signal: during a session it recreates the
portal session within 200 ms, and between sessions within 15 s, instead of
leaving a dead session for the next client. Pre-authorization lets KDE
recreate it without a dialog while locked. Whether locking or unlocking
alone ends KDE's portal session is not verified; if it does, the same
recovery applies. Hosts without `preauthorizeRemoteDesktop` would need the
local consent dialog, which cannot appear on a locked screen, hence the
NixOS assertion for `unattended`, and on other distributions the host's
warning at start (a KDE session that is locked or was logged in
automatically, without the `kde-authorized` permission).

### Security trade-offs

- **The desktop is open for a moment at boot.** Between the automatic login
  and the lock (typically a second or two, at most until the lock service
  runs), the physical screen shows the unlocked desktop. `lockEveryLogin`
  closes this gap on KDE at the cost of locking every login. Do not use
  unattended start where people you do not trust have physical access.
- **The lock screen is the only barrier at the machine.** Anyone who can
  restart it reaches a locked session, not a login prompt; the lock screen's
  password check (PAM, throttled by faillock) is the same as the greeter's.
- **Keyrings stay locked.** No password is typed at login, so KWallet/GNOME
  Keyring are not unlocked by PAM; apps ask when they need them.
- **Disk encryption still needs its passphrase at boot** unless it is
  unlocked by a TPM, so an encrypted machine cannot restart unattended
  anyway.
- **Remote access needs the same credentials as before.** Teleport only
  admits paired devices or a PAM-verified Linux account login; the session
  stays locked until someone types the password (or `unlockOnSystemLogin`
  is on and the system login succeeded). Anyone the host admits can control
  the lock screen and, with the password, the desktop.
- `allowSuspend` lets every owner device suspend the host at any time.
- `wakeRelay` lets the owner's devices make that host broadcast magic
  packets (rate-limited). Wake-on-LAN itself is unauthenticated by design.

### Option b: capturing SDDM's greeter (not built)

NixOS's SDDM runs its Wayland greeter as the `sddm` user in a separate
`kwin_wayland --no-global-shortcuts --no-kactivities --no-lockscreen`.
Reaching it would need one of:

- **A host as the `sddm` user against the greeter's KWin.** The greeter
  session has no xdg-desktop-portal and no PipeWire, and KWin offers screen
  casting (`zkde_screencast_unstable_v1`) and fake input only to clients it
  trusts (desktop-file allow list, or `KWIN_WAYLAND_NO_PERMISSION_CHECKS`
  for the whole greeter). It means changing SDDM's compositor command,
  starting PipeWire for `sddm`, and a second capture/input backend, all
  tied to KWin and SDDM internals that change between Plasma releases.
- **A root service capturing the GPU's scanout (KMS) with uinput input**, as
  Sunshine does. It sees greeter, lock screen and TTYs alike, but needs
  `CAP_SYS_ADMIN`, is driver-dependent (NVIDIA needs `nvidia-drm.modeset`
  and cannot always export the framebuffer), cannot draw the cursor plane,
  and would put the network-facing media pipeline in a privileged process,
  which Teleport deliberately avoids (docs/system-login.md).
- After the greeter, the user's own session would still need its own host,
  so the stream would switch hosts at login.

Neither is clean or robust enough to build now. GNOME's remote desktop
system/headless mode (gnome-remote-desktop with GDM integration) is the
closest upstream solution; KDE has no equivalent yet. Automatic login plus
lock gives the same practical result for a single-user gaming PC.

## Verified

- Unit tests: magic packet construction and parsing (SecureOn suffix,
  corrupted copies, lengths), MAC parsing, subnet broadcasts, target lists,
  default-route and `ethtool` parsing, interface choice, wake info
  validation and wake-relay selection, profile saving only for the pinned
  desktop, command encoding (fits a relayed hello) and refusal of malformed
  ones, opt-in/owner/allow-list/rate-limit decisions, logind session id
  checks, the lock-after-login retry logic, automatic-login detection,
  secure-attention decisions, Ctrl+Alt+End handling, system-login lease
  flag.
- `tests/wake.rs` (unprivileged user+network+mount namespace with a dummy
  LAN interface; packets never leave it): the client's broadcasts reach a
  listener; through a real relay, a `--wake-relay` host sends the packet for
  the client; owner suspend through the relay and directly runs the
  (substituted) suspend command; an unknown credential is refused; a host
  without the options refuses both and sends nothing.
- `tools/check-module.sh`: `nix eval` of minimal SDDM + Plasma 6 NixOS
  configurations: the generated `sddm.conf.d/00-nixos.conf` `[Autologin]`,
  the lock service, `LockOnStart`, `wakeOnLan`, the polkit rule, the host's
  flags and path, defaults off, and the assertions.
- `tools/package-smoke.sh` (2026-10-19, stock containers): the .deb, .rpm
  and Arch package install `teleport-lock-on-login.service`, the SDDM and
  `kscreenlockerrc` examples and the PAM helper and policy (see
  system-login.md); unit test of the pre-authorisation check's parsing.

Not verified: the lock unit, the SDDM example and the pre-authorisation
warning on a real .deb/.rpm/Arch desktop (no such machine was booted), a
real wake from suspend or power-off (no second machine was
woken; magic packets were never sent on the real LAN), a real boot with
automatic login and lock in a VM, KDE's portal `Closed` signal on monitor
wake and the recreation under a locked session on hardware, typing into the
KDE lock screen through the portal, polkit's decision for the user service's
`systemctl suspend`, Ctrl+Alt+End from a macOS keyboard.
