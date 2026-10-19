{
  pkgs,
  bundle,
  format,
}:
# .deb or .rpm of the relocatable bundle (nix/bundle.nix), built with nfpm:
# the bundle in /opt/teleport, `teleport` on PATH, and the system setup that
# nix/module.nix does on NixOS (systemd user unit, uinput/uhid udev rules and
# module loading, UDP buffer sysctl, the portal's dev.teleport.Host entry,
# the unattended-start lock unit, the system-login PAM helper and policy),
# examples for what needs root (SDDM automatic login), and the documentation
# in /usr/share/doc/teleport. Installing enables nothing: users opt in per
# account (docs/distribution.md, docs/wake-and-unattended.md).
let
  system = ../packaging/linux/system;
  examples = ../packaging/linux/examples;
  # /etc/pam.d/teleport for the helper. Debian and Ubuntu: a conffile with
  # their common-auth stack (an administrator's changes are kept). The .rpm
  # serves Fedora (system-auth) and openSUSE (common-auth), so postinstall.sh
  # writes the matching policy on first install unless one exists; the
  # package owns it as a %ghost file.
  pamPolicy =
    if format == "deb" then
      [
        {
          src = "${system}/pam-teleport.common-auth";
          dst = "/etc/pam.d/teleport";
          type = "config|noreplace";
        }
      ]
    else
      [
        {
          dst = "/etc/pam.d/teleport";
          type = "ghost";
        }
      ];
  # The documentation the help text and system files cite (README.md and
  # docs/*.md, in the bundle's usr/share/doc/teleport).
  docs =
    map
      (name: {
        src = "${bundle}/usr/share/doc/teleport/${name}";
        dst = "/usr/share/doc/teleport/${name}";
      })
      (
        [ "README.md" ]
        ++ builtins.filter (pkgs.lib.hasSuffix ".md") (builtins.attrNames (builtins.readDir ../docs))
      );
  arch =
    {
      deb = {
        x86_64 = "amd64";
        aarch64 = "arm64";
      };
      rpm = {
        x86_64 = "x86_64";
        aarch64 = "aarch64";
      };
    }
    .${format}.${pkgs.stdenv.hostPlatform.parsed.cpu.name};
  config = pkgs.writeText "nfpm.json" (
    builtins.toJSON {
      name = "teleport";
      inherit arch;
      platform = "linux";
      version = bundle.version;
      release = "1";
      maintainer = "Teleport developers <noreply@teleport.invalid>";
      description = "Native remote desktop over Media over QUIC (host and client)";
      homepage = "https://github.com/ejb1123/teleport";
      license = "Unspecified";
      # 2026-10-17: fixed so the package does not depend on the build time.
      mtime = "2026-10-17T00:00:00Z";
      overrides = {
        deb.recommends = [
          "libgl1"
          "libegl1"
        ];
        rpm.recommends = [
          "libglvnd-glx"
          "libglvnd-egl"
        ];
      };
      contents = [
        {
          src = "${bundle}";
          dst = "/opt/teleport";
          type = "tree";
        }
        {
          src = "/opt/teleport/AppRun";
          dst = "/usr/bin/teleport";
          type = "symlink";
        }
      ]
      # Store files are read-only; installed configuration is 0644.
      ++ map (file: file // { file_info.mode = 420; }) (
        [
          {
            src = "${bundle}/usr/share/applications/dev.teleport.Teleport.desktop";
            dst = "/usr/share/applications/dev.teleport.Teleport.desktop";
          }
          {
            src = "${system}/dev.teleport.Host.desktop";
            dst = "/usr/share/applications/dev.teleport.Host.desktop";
          }
          {
            src = "${bundle}/usr/share/icons/hicolor/scalable/apps/dev.teleport.Teleport.svg";
            dst = "/usr/share/icons/hicolor/scalable/apps/dev.teleport.Teleport.svg";
          }
        ]
        ++
          map
            (size: {
              src = "${bundle}/usr/share/icons/hicolor/${size}x${size}/apps/dev.teleport.Teleport.png";
              dst = "/usr/share/icons/hicolor/${size}x${size}/apps/dev.teleport.Teleport.png";
            })
            (
              map toString [
                16
                24
                32
                48
                64
                128
                256
                512
              ]
            )
        ++ [
          {
            src = "${bundle}/usr/share/metainfo/dev.teleport.Teleport.metainfo.xml";
            dst = "/usr/share/metainfo/dev.teleport.Teleport.metainfo.xml";
          }
          {
            src = "${system}/teleport-desktop.service";
            dst = "/usr/lib/systemd/user/teleport-desktop.service";
          }
          {
            src = "${system}/70-teleport.rules";
            dst = "/usr/lib/udev/rules.d/70-teleport.rules";
          }
          {
            src = "${system}/teleport-modules.conf";
            dst = "/usr/lib/modules-load.d/teleport.conf";
          }
          {
            src = "${system}/90-teleport.conf";
            dst = "/usr/lib/sysctl.d/90-teleport.conf";
          }
          # Unattended start (`unattended.enable`): the lock after an
          # automatic login, enabled per user like teleport-desktop.service.
          {
            src = "${system}/teleport-lock-on-login.service";
            dst = "/usr/lib/systemd/user/teleport-lock-on-login.service";
          }
          # Opt-in examples: SDDM automatic login, KDE's lock on start.
          {
            src = "${examples}/sddm-autologin.conf";
            dst = "/usr/share/teleport/examples/sddm-autologin.conf";
          }
          {
            src = "${examples}/kscreenlockerrc";
            dst = "/usr/share/teleport/examples/kscreenlockerrc";
          }
          # Both PAM policies, for reference and for postinstall.sh (.rpm).
          {
            src = "${system}/pam-teleport.common-auth";
            dst = "/usr/share/teleport/pam/teleport.common-auth";
          }
          {
            src = "${system}/pam-teleport.system-auth";
            dst = "/usr/share/teleport/pam/teleport.system-auth";
          }
        ]
        ++ pamPolicy
        ++ docs
      )
      ++ [
        # The system-login helper (`systemLogin`; docs/system-login.md):
        # root-owned 0755, never setuid. The host refuses it otherwise.
        {
          src = "${bundle}/usr/libexec/teleport/teleport-pam";
          dst = "/usr/lib/teleport/teleport-pam";
          file_info.mode = 493;
        }
      ];
      scripts.postinstall = "${system}/postinstall.sh";
    }
  );
in
pkgs.runCommand "teleport-${format}-${bundle.version}" { nativeBuildInputs = [ pkgs.nfpm ]; } ''
  mkdir -p $out
  cp ${config} nfpm.json
  nfpm package --config nfpm.json --packager ${format} --target $out/
''
