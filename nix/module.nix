{
  config,
  lib,
  pkgs,
  ...
}:
let
  cfg = config.services.teleport-desktop;
  plasma = config.services.desktopManager.plasma6.enable or false;
  unattendedUser =
    if cfg.unattended.user != null then
      cfg.unattended.user
    else if cfg.users != [ ] then
      lib.head cfg.users
    else
      null;
  pamHelper =
    pkgs.runCommand "teleport-system-pam"
      {
        nativeBuildInputs = [ pkgs.stdenv.cc ];
        buildInputs = [ pkgs.pam ];
      }
      ''
        mkdir -p $out/libexec
        cc -O2 -Wall -Wextra -Werror ${cfg.package}/share/teleport/teleport-pam.c -o $out/libexec/teleport-pam -lpam
      '';
in
{
  options.services.teleport-desktop = {
    enable = lib.mkEnableOption "Teleport host in each enabled user's graphical session";
    package = lib.mkOption {
      type = lib.types.package;
      description = "Teleport flake package for this machine.";
    };
    listen = lib.mkOption {
      type = lib.types.str;
      default = "127.0.0.1:4443";
      description = "QUIC listen address; use a LAN/VPN interface to allow remote connections.";
    };
    users = lib.mkOption {
      type = lib.types.listOf lib.types.str;
      default = [ ];
      description = "Users permitted to run the graphical-session host.";
    };
    extraArgs = lib.mkOption {
      type = lib.types.listOf lib.types.str;
      default = [ ];
      description = "Additional host arguments. Clipboard and audio are opt-in.";
    };
    systemLogin = lib.mkEnableOption "existing Linux password login for allowed users' running desktops";
    microphone = lib.mkEnableOption ''
      a virtual "Teleport Microphone" input that plays a client's
      microphone (`teleport client --mic`) while its session lasts (PipeWire)'';
    gamepads = lib.mkEnableOption ''
      virtual gamepads for clients' game controllers: enables
      `hardware.uinput` and adds the allowed users to the `uinput` group
      (takes effect at their next login)'';
    playstationGamepads = lib.mkOption {
      type = lib.types.bool;
      default = true;
      description = ''
        With `gamepads`: also let the allowed users create DualShock 4 and
        DualSense pads through `/dev/uhid` (group `uinput`, the `uhid`
        module loaded at boot), so PlayStation controllers on clients
        appear as PlayStation pads with touchpad, motion sensors and LEDs.
        Without it the host offers only the Xbox 360 pad. Like uinput, uhid
        lets its users create any input device. See docs/gamepad.md.
      '';
    };
    udpBuffers = lib.mkOption {
      type = lib.types.bool;
      default = true;
      description = ''
        Raise `net.core.rmem_max` and `net.core.wmem_max` to 8 MiB (with
        `lib.mkDefault`, so an explicit setting wins) so the QUIC socket gets
        the buffers it requests; smaller buffers drop packets during keyframe
        bursts on fast links.
      '';
    };
    virtualDisplay = lib.mkOption {
      type = lib.types.enum [
        "never"
        "fallback"
        "always"
      ];
      default = "fallback";
      description = ''
        Virtual displays through the desktop portal (KDE Plasma 6; GNOME
        untested), passed as `--virtual-display`: `fallback` streams one when
        no monitor is connected (headless) and lets clients add one as an
        extra monitor; `always` streams one sized to the client instead of
        the monitors; `never` turns both off. Unattended use needs
        `preauthorizeRemoteDesktop`. See docs/virtual-display.md.
      '';
    };
    privacyMode = lib.mkOption {
      type = lib.types.enum [
        "off"
        "request"
        "always"
      ];
      default = "off";
      description = ''
        Privacy mode (KDE Plasma 6), passed as `--privacy-mode`: while a
        session lasts the physical monitors are turned off and the session
        streams a virtual display. `request` when the client asks
        (`teleport client --privacy`, launcher "Host screen: private"),
        `always` for every session. The monitors come back on disconnect,
        on any failure, through a guard process if the host dies, and from
        this unit's `ExecStopPost`. See docs/virtual-display.md.
      '';
    };
    wakeOnLan = {
      interface = lib.mkOption {
        type = lib.types.nullOr lib.types.str;
        default = null;
        example = "enp5s0";
        description = ''
          Arm Wake-on-LAN by magic packet on this wired network card
          (`networking.interfaces.<name>.wakeOnLan`), so clients can wake
          the host with the launcher's Wake or `teleport wake`. The
          firmware must also allow waking from the card (often "Wake on
          LAN", "Power On By PCI-E" or "ErP" off) and the card needs
          standby power; Wi-Fi rarely works. See docs/wake-and-unattended.md.
        '';
      };
    };
    wakeRelay = lib.mkOption {
      type = lib.types.bool;
      default = false;
      description = ''
        Act as a wake relay (`--wake-relay`): when one of the owner's paired
        devices asks, send a Wake-on-LAN packet on this host's networks, to
        wake another machine on this LAN from anywhere through the
        end-to-end encrypted relay. Meant for an always-on machine.
      '';
    };
    allowSuspend = lib.mkOption {
      type = lib.types.bool;
      default = false;
      description = ''
        Suspend this machine when one of the owner's paired devices asks
        (`--allow-suspend`; launcher Sleep, `teleport suspend`). Grants the
        allowed users logind's suspend action through polkit, which a user
        service otherwise may not get without a password.
      '';
    };
    unattended = {
      enable = lib.mkEnableOption ''
        unattended start after a reboot: the display manager logs
        `unattended.user` in automatically and the session locks itself at
        once, so the host runs (and can be reached) without anyone at the
        machine. Unlock remotely by typing the password into the lock screen
        (Ctrl+Alt+End shows it), or with `unlockOnSystemLogin`. Read the
        security notes in docs/wake-and-unattended.md first: the desktop is
        briefly visible before it locks, and keyrings stay locked'';
      user = lib.mkOption {
        type = lib.types.nullOr lib.types.str;
        default = null;
        description = "The user to log in automatically (one of `users`); defaults to the first.";
      };
      lockEveryLogin = lib.mkOption {
        type = lib.types.bool;
        default = false;
        description = ''
          KDE Plasma: also set the screen locker's "lock on start"
          (`LockOnStart`), so the compositor locks before the desktop is
          ever shown. Then every login, not only the automatic one, starts
          locked (you type the password twice at the machine).
        '';
      };
      unlockOnSystemLogin = lib.mkOption {
        type = lib.types.bool;
        default = false;
        description = ''
          Unlock the session when a client signs in with the account's Linux
          password (`systemLogin`), right after PAM verified it
          (`--unlock-on-system-login`). The password is never stored.
        '';
      };
    };
    preauthorizeRemoteDesktop = lib.mkOption {
      type = lib.types.bool;
      default = false;
      description = ''
        Pre-authorize the host (application ID `dev.teleport.Host`) for remote
        desktop in the user's portal permission store each time it starts, so
        KDE creates or recreates its screen and input session without the local
        consent dialog -- including while the screen is locked, when that dialog
        cannot appear. Intended for unattended hosts: anyone the host admits can
        then control the desktop without local confirmation. Desktops other than
        KDE ignore the entry.
      '';
    };
  };
  config = lib.mkIf cfg.enable {
    assertions = [
      {
        assertion = cfg.users != [ ];
        message = "services.teleport-desktop.users must explicitly list the desktop users allowed to share their session.";
      }
      {
        assertion = !cfg.unattended.enable || cfg.preauthorizeRemoteDesktop;
        message = "services.teleport-desktop.unattended needs preauthorizeRemoteDesktop: nobody is at the locked machine to approve screen sharing.";
      }
      {
        assertion = !cfg.unattended.enable || lib.elem unattendedUser cfg.users;
        message = "services.teleport-desktop.unattended.user must be one of services.teleport-desktop.users.";
      }
      {
        assertion = !cfg.unattended.unlockOnSystemLogin || cfg.systemLogin;
        message = "services.teleport-desktop.unattended.unlockOnSystemLogin needs systemLogin (PAM password check).";
      }
    ];
    # Wake-on-LAN by magic packet on the host's card (G23).
    networking.interfaces = lib.optionalAttrs (cfg.wakeOnLan.interface != null) {
      ${cfg.wakeOnLan.interface}.wakeOnLan.enable = true;
    };
    # Unattended start (G24): log in automatically, then lock at once.
    services.displayManager = lib.mkIf cfg.unattended.enable {
      autoLogin = {
        enable = true;
        user = unattendedUser;
      };
      # SDDM's automatic login needs a session to start.
      defaultSession = lib.mkIf plasma (lib.mkDefault "plasma");
    };
    # KDE: the compositor locks before the desktop is first shown (every
    # login, not only the automatic one).
    environment.etc."xdg/kscreenlockerrc" =
      lib.mkIf (cfg.unattended.enable && cfg.unattended.lockEveryLogin && plasma)
        {
          text = ''
            [Daemon]
            LockOnStart[$i]=true
          '';
        };
    # The host user service suspends through logind; outside a login session
    # polkit would otherwise ask for a password nobody can type.
    security.polkit.extraConfig = lib.mkIf cfg.allowSuspend ''
      // Teleport allowSuspend: the allowed users' host service may suspend.
      polkit.addRule(function (action, subject) {
        if ((action.id == "org.freedesktop.login1.suspend" ||
             action.id == "org.freedesktop.login1.suspend-multiple-sessions") &&
            ${builtins.toJSON cfg.users}.indexOf(subject.user) >= 0) {
          return polkit.Result.YES;
        }
      });
    '';
    systemd.user.services.teleport-lock-on-login = lib.mkIf cfg.unattended.enable {
      description = "Lock the desktop session Teleport's unattended start logged in";
      wantedBy = [ "graphical-session.target" ];
      after = [ "graphical-session.target" ];
      partOf = [ "graphical-session.target" ];
      unitConfig.ConditionUser = unattendedUser;
      path = [ pkgs.systemd ];
      serviceConfig = {
        Type = "oneshot";
        # Only a display manager's automatic login (PAM service
        # *-autologin); a password login stays as it is.
        ExecStart = "${cfg.package}/bin/teleport lock-session --only-automatic-login --timeout 60";
      };
    };
    environment.systemPackages = [
      cfg.package
      # xdg-desktop-portal only accepts the unit-derived application ID when a
      # matching desktop entry exists; without it the host is anonymous and
      # cannot be pre-authorized.
      (pkgs.makeDesktopItem {
        name = "dev.teleport.Host";
        desktopName = "Teleport Host";
        comment = "Remote desktop host (identifies the Teleport host service to desktop portals)";
        exec = "teleport host";
        icon = "krfb";
        noDisplay = true;
      })
    ];
    security.pam.services.teleport = lib.mkIf cfg.systemLogin { };
    boot.kernel.sysctl = lib.mkIf cfg.udpBuffers {
      "net.core.rmem_max" = lib.mkDefault 8388608;
      "net.core.wmem_max" = lib.mkDefault 8388608;
    };
    # The host advertises gamepads only when it can write /dev/uinput.
    hardware.uinput.enable = lib.mkIf cfg.gamepads true;
    users.users = lib.mkIf cfg.gamepads (
      lib.genAttrs cfg.users (_: {
        extraGroups = [ "uinput" ];
      })
    );
    # PlayStation pads (docs/gamepad.md): /dev/uhid for the same group.
    boot.kernelModules = lib.mkIf (cfg.gamepads && cfg.playstationGamepads) [ "uhid" ];
    services.udev.extraRules = lib.mkIf (cfg.gamepads && cfg.playstationGamepads) ''
      KERNEL=="uhid", SUBSYSTEM=="misc", GROUP="uinput", MODE="0660", OPTIONS+="static_node=uhid"
    '';
    # The host registers the application ID `dev.teleport.Host` with the portal
    # itself (host portal Registry; the desktop entry above is required). KDE
    # can then pre-authorize it (see preauthorizeRemoteDesktop), so a lost or
    # expired portal session is recreated without a local dialog, even while
    # the screen is locked.
    systemd.user.services.teleport-desktop = {
      description = "Teleport remote desktop host (graphical session)";
      wantedBy = [ "graphical-session.target" ];
      after = [ "graphical-session.target" ];
      partOf = [ "graphical-session.target" ];
      unitConfig.ConditionUser = map (user: "|${user}") cfg.users;
      # ethtool reads the Wake-on-LAN setting, loginctl and systemctl lock,
      # unlock and suspend; kscreen-doctor sizes virtual displays and runs
      # privacy mode (KDE).
      path = [
        pkgs.ethtool
        pkgs.systemd
      ]
      ++ lib.optional plasma pkgs.kdePackages.libkscreen;
      serviceConfig = {
        ExecStart = "${cfg.package}/bin/teleport host --listen ${lib.escapeShellArg cfg.listen} --identity-dir %h/.local/state/teleport/host --restore-token %h/.local/state/teleport/host/portal-restore.json --virtual-display ${cfg.virtualDisplay} --privacy-mode ${cfg.privacyMode} ${lib.optionalString cfg.microphone "--microphone"} ${lib.optionalString cfg.systemLogin "--system-auth-helper ${pamHelper}/libexec/teleport-pam"} ${lib.optionalString cfg.unattended.unlockOnSystemLogin "--unlock-on-system-login"} ${lib.optionalString cfg.wakeRelay "--wake-relay"} ${lib.optionalString cfg.allowSuspend "--allow-suspend"} ${lib.escapeShellArgs cfg.extraArgs}";
        # However the host stopped (crash, kill, logout), turn monitors left
        # off by privacy mode back on. "-": nothing to do is not a failure.
        ExecStopPost =
          lib.mkIf (cfg.privacyMode != "off")
            "-${cfg.package}/bin/teleport privacy-restore --state %h/.local/state/teleport/host/privacy-restore.json";
        Restart = "on-failure";
        RestartSec = 10;
        UMask = "0077";
        # KDE's portal skips its consent dialog for apps marked "yes" here (the
        # mechanism KRdp uses). "-": a missing permission store must not stop
        # the host; it then falls back to the normal dialog.
        ExecStartPre = lib.mkIf cfg.preauthorizeRemoteDesktop "-${pkgs.systemd}/bin/busctl --user call org.freedesktop.impl.portal.PermissionStore /org/freedesktop/impl/portal/PermissionStore org.freedesktop.impl.portal.PermissionStore SetPermission sbssas kde-authorized true remote-desktop dev.teleport.Host 1 yes";
      };
    };
  };
}
