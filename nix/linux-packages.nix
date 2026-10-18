{
  pkgs,
  bundle,
  format,
}:
# .deb or .rpm of the relocatable bundle (nix/bundle.nix), built with nfpm:
# the bundle in /opt/teleport, `teleport` on PATH, and the system setup that
# nix/module.nix does on NixOS (systemd user unit, uinput/uhid udev rules and
# module loading, UDP buffer sysctl, the portal's dev.teleport.Host entry).
# Installing enables nothing: users opt in per account (docs/distribution.md).
let
  system = ../packaging/linux/system;
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
        ]
      );
      scripts.postinstall = "${system}/postinstall.sh";
    }
  );
in
pkgs.runCommand "teleport-${format}-${bundle.version}" { nativeBuildInputs = [ pkgs.nfpm ]; } ''
  mkdir -p $out
  cp ${config} nfpm.json
  nfpm package --config nfpm.json --packager ${format} --target $out/
''
