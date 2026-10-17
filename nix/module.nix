{
  config,
  lib,
  pkgs,
  ...
}:
let
  cfg = config.services.teleport-desktop;
  plasma = config.services.desktopManager.plasma6.enable or false;
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
    gamepads = lib.mkEnableOption ''
      virtual gamepads for clients' game controllers: enables
      `hardware.uinput` and adds the allowed users to the `uinput` group
      (takes effect at their next login)'';
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
    ];
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
      # kscreen-doctor sizes virtual displays and runs privacy mode (KDE).
      path = lib.optional plasma pkgs.kdePackages.libkscreen;
      serviceConfig = {
        ExecStart = "${cfg.package}/bin/teleport host --listen ${lib.escapeShellArg cfg.listen} --identity-dir %h/.local/state/teleport/host --restore-token %h/.local/state/teleport/host/portal-restore.json --virtual-display ${cfg.virtualDisplay} --privacy-mode ${cfg.privacyMode} ${lib.optionalString cfg.systemLogin "--system-auth-helper ${pamHelper}/libexec/teleport-pam"} ${lib.escapeShellArgs cfg.extraArgs}";
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
