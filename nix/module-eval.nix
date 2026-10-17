# Evaluates nix/module.nix in minimal NixOS configurations (SDDM, Plasma 6)
# and checks what it generates, without building or switching anything:
#
#   nix eval --json .#checks.x86_64-linux.module-eval.settings   # show
#   nix eval .#checks.x86_64-linux.module-eval.passed           # check
#
# `passed` is true, or evaluation fails naming the broken expectation.
{ nixpkgs, system }:
let
  inherit (nixpkgs) lib;
  pkgs = import nixpkgs { inherit system; };
  evaluate =
    settings:
    (lib.nixosSystem {
      inherit system;
      modules = [
        ./module.nix
        {
          system.stateVersion = "26.05";
          boot.loader.grub.enable = false;
          fileSystems."/" = {
            device = "none";
            fsType = "tmpfs";
          };
          users.users.gamer.isNormalUser = true;
          services.desktopManager.plasma6.enable = true;
          services.displayManager.sddm = {
            enable = true;
            wayland.enable = true;
          };
          services.teleport-desktop = {
            enable = true;
            # Only evaluated, never built.
            package = pkgs.hello;
            users = [ "gamer" ];
          }
          // settings;
        }
      ];
    }).config;
  failed = config: map (a: a.message) (lib.filter (a: !a.assertion) config.assertions);
  unattended = evaluate {
    preauthorizeRemoteDesktop = true;
    systemLogin = true;
    wakeOnLan.interface = "enp5s0";
    wakeRelay = true;
    allowSuspend = true;
    unattended = {
      enable = true;
      lockEveryLogin = true;
      unlockOnSystemLogin = true;
    };
  };
  plain = evaluate { };
  hostCommand = config: config.systemd.user.services.teleport-desktop.serviceConfig.ExecStart;
  settings = {
    sddmConf = unattended.environment.etc."sddm.conf.d/00-nixos.conf".source.text;
    autoLogin = unattended.services.displayManager.autoLogin;
    defaultSession = unattended.services.displayManager.defaultSession;
    lockService = {
      inherit (unattended.systemd.user.services.teleport-lock-on-login)
        wantedBy
        after
        ;
      inherit (unattended.systemd.user.services.teleport-lock-on-login.serviceConfig) Type ExecStart;
      condition = unattended.systemd.user.services.teleport-lock-on-login.unitConfig.ConditionUser;
    };
    kscreenlocker = unattended.environment.etc."xdg/kscreenlockerrc".text;
    wakeOnLan = unattended.networking.interfaces.enp5s0.wakeOnLan;
    polkit = unattended.security.polkit.extraConfig;
    host = hostCommand unattended;
    hostPath = map (p: p.name) unattended.systemd.user.services.teleport-desktop.path;
    plainHost = hostCommand plain;
    plainAutoLogin = plain.services.displayManager.autoLogin.enable;
    plainLockService = plain.systemd.user.services ? teleport-lock-on-login;
    failures = {
      unattended = failed unattended;
      plain = failed plain;
      withoutPreauthorization = failed (evaluate { unattended.enable = true; });
      unlockWithoutSystemLogin = failed (evaluate {
        preauthorizeRemoteDesktop = true;
        unattended = {
          enable = true;
          unlockOnSystemLogin = true;
        };
      });
      foreignUser = failed (evaluate {
        preauthorizeRemoteDesktop = true;
        unattended = {
          enable = true;
          user = "root";
        };
      });
    };
  };
  expect =
    name: condition:
    if condition then true else throw "nix/module-eval.nix: expectation failed: ${name}";
  has = lib.hasInfix;
  checks = with settings; [
    (expect "SDDM logs gamer in automatically into Plasma" (
      has "[Autologin]" sddmConf && has "User=gamer" sddmConf && has "Session=plasma.desktop" sddmConf
    ))
    (expect "the automatic login is the gamer" (autoLogin.enable && autoLogin.user == "gamer"))
    (expect "the lock service runs in the gamer's graphical session" (
      lockService.wantedBy == [ "graphical-session.target" ]
      && lockService.condition == "gamer"
      && lockService.Type == "oneshot"
      && has "lock-session --only-automatic-login" lockService.ExecStart
    ))
    (expect "KDE locks on start, immutably" (has "LockOnStart[$i]=true" kscreenlocker))
    (expect "Wake-on-LAN is armed on the card" wakeOnLan.enable)
    (expect "polkit lets the gamer suspend" (
      has "org.freedesktop.login1.suspend" polkit && has ''["gamer"]'' polkit
    ))
    (expect "the host gets the power and unlock flags" (
      has "--wake-relay" host && has "--allow-suspend" host && has "--unlock-on-system-login" host
    ))
    (expect "the host can run ethtool and loginctl" (
      lib.any (has "ethtool") hostPath && lib.any (has "systemd") hostPath
    ))
    (expect "nothing is on by default" (
      !plainAutoLogin
      && !plainLockService
      && !(has "--wake-relay" plainHost)
      && !(has "--allow-suspend" plainHost)
      && !(has "--unlock-on-system-login" plainHost)
    ))
    (expect "valid configurations pass their assertions" (
      failures.unattended == [ ] && failures.plain == [ ]
    ))
    (expect "unattended without pre-authorization is refused" (
      lib.any (has "preauthorizeRemoteDesktop") failures.withoutPreauthorization
    ))
    (expect "unlocking needs the PAM check" (
      lib.any (has "needs systemLogin") failures.unlockWithoutSystemLogin
    ))
    (expect "only an allowed user logs in automatically" (
      lib.any (has "unattended.user must be one of") failures.foreignUser
    ))
  ];
  passed = lib.all (check: check) checks;
in
# Building it runs the checks too.
pkgs.writeText "teleport-module-eval" (builtins.toJSON (assert passed; settings))
// {
  inherit settings passed;
}
