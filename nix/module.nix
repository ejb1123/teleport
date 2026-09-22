{
  config,
  lib,
  pkgs,
  ...
}:
let
  cfg = config.services.teleport-desktop;
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
  };
  config = lib.mkIf cfg.enable {
    assertions = [
      {
        assertion = cfg.users != [ ];
        message = "services.teleport-desktop.users must explicitly list the desktop users allowed to share their session.";
      }
    ];
    environment.systemPackages = [ cfg.package ];
    systemd.user.services.teleport-desktop = {
      description = "Teleport remote desktop host (graphical session)";
      wantedBy = [ "graphical-session.target" ];
      after = [ "graphical-session.target" ];
      partOf = [ "graphical-session.target" ];
      unitConfig.ConditionUser = map (user: "|${user}") cfg.users;
      serviceConfig = {
        ExecStart = "${cfg.package}/bin/teleport host --listen ${lib.escapeShellArg cfg.listen} --identity-dir %h/.local/state/teleport/host --restore-token %h/.local/state/teleport/host/portal-restore.json ${lib.escapeShellArgs cfg.extraArgs}";
        Restart = "on-failure";
        RestartSec = 10;
        UMask = "0077";
      };
    };
  };
}
