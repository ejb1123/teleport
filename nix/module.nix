{
  config,
  lib,
  pkgs,
  ...
}:
let
  cfg = config.services.teleport-desktop;
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
      serviceConfig = {
        ExecStart = "${cfg.package}/bin/teleport host --listen ${lib.escapeShellArg cfg.listen} --identity-dir %h/.local/state/teleport/host --restore-token %h/.local/state/teleport/host/portal-restore.json ${lib.optionalString cfg.systemLogin "--system-auth-helper ${pamHelper}/libexec/teleport-pam"} ${lib.escapeShellArgs cfg.extraArgs}";
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
