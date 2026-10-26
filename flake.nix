{
  description = "Teleport remote desktop development environment";

  # 26.05 retains Intel macOS support, which newer nixpkgs has dropped.
  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-26.05";

  outputs =
    { nixpkgs, ... }:
    let
      systems = [
        "x86_64-linux"
        "aarch64-linux"
        "aarch64-darwin"
        "x86_64-darwin"
      ];
      forAllSystems = nixpkgs.lib.genAttrs systems;
      environment = pkgs: {
        vaDrivers = pkgs.buildEnv {
          name = "teleport-va-drivers";
          paths = [
            pkgs.mesa
          ]
          ++ pkgs.lib.optionals pkgs.stdenv.hostPlatform.isx86_64 [
            pkgs.intel-media-driver
            pkgs.intel-vaapi-driver
          ];
          pathsToLink = [ "/lib/dri" ];
        };
        tools = with pkgs; [
          pkg-config
          cmake
          ninja
        ];
        media =
          with pkgs;
          [
            gst_all_1.gstreamer
            gst_all_1.gst-plugins-base
            gst_all_1.gst-plugins-good
            (
              if pkgs.stdenv.hostPlatform.isDarwin then
                import ./nix/gst-applemedia-lowlatency.nix { inherit pkgs; }
              else
                gst_all_1.gst-plugins-bad
            )
            gst_all_1.gst-plugins-ugly
            gst_all_1.gst-libav
            (ffmpeg-headless.override {
              withVpl = stdenv.hostPlatform.isLinux && stdenv.hostPlatform.isx86_64;
            })
            SDL2
            SDL2_ttf
            libfido2
          ]
          ++ pkgs.lib.optionals pkgs.stdenv.hostPlatform.isLinux [
            (import ./nix/pipewire-hdr.nix { inherit pkgs; })
            pkgs.libei
            pkgs.wayland
            pkgs.libxkbcommon
            pkgs.libdrm
            pkgs.libva
            # Header-only: CUDA driver API declarations for zero-copy GL presentation
            # (libcuda itself is loaded at runtime only when present).
            pkgs.nv-codec-headers-12
            pkgs.libx11
            pkgs.libxext
            pkgs.libxdamage
            pkgs.libxtst
            pkgs.wl-clipboard
            pkgs.xclip
            pkgs.pam
          ];
      };
    in
    {
      nixosModules.default = import ./nix/module.nix;
      nixosModules.experimental-kwin-hdr = import ./nix/kwin-hdr-module.nix;
      # Explicit opt-in: building the app never replaces the running compositor.
      overlays.kwin-hdr = _final: prev: {
        kdePackages = prev.kdePackages.overrideScope (
          _kfinal: kprev: {
            kwin = import ./nix/kwin-hdr.nix {
              pkgs = prev // {
                kdePackages = kprev;
              };
            };
          }
        );
      };
      checks = forAllSystems (
        system:
        let
          pkgs = import nixpkgs { inherit system; };
        in
        {
          applemedia-lowlatency = import ./nix/gst-applemedia-lowlatency-check.nix { inherit pkgs; };
        }
        // pkgs.lib.optionalAttrs pkgs.stdenv.hostPlatform.isLinux {
          pipewire-hdr = import ./nix/pipewire-hdr-check.nix { inherit pkgs; };
          # NixOS module options (SDDM autologin, lock, WoL), evaluated only.
          module-eval = import ./nix/module-eval.nix { inherit nixpkgs system; };
        }
      );
      packages = forAllSystems (
        system:
        let
          pkgs = import nixpkgs { inherit system; };
          deps = environment pkgs;
          teleport = pkgs.rustPlatform.buildRustPackage {
            pname = "teleport";
            # Cargo.toml is the one place the version is set (docs/distribution.md).
            version = (builtins.fromTOML (builtins.readFile ./Cargo.toml)).package.version;
            src = pkgs.lib.fileset.toSource {
              root = ./.;
              fileset = pkgs.lib.fileset.unions [
                ./Cargo.toml
                ./Cargo.lock
                ./build.rs
                ./src
                ./tests
                # Not packaging/linux: the relocatable bundle's scripts must
                # not rebuild the application.
                ./packaging/pam
                ./packaging/icons
                ./packaging/teleport.desktop
                ./packaging/teleport-open.desktop
                ./packaging/Info.plist
                ./nix/mesa-runtime.sh
              ];
            };
            cargoLock = {
              lockFile = ./Cargo.lock;
              # moq-net from the fork in Cargo.toml's [patch.crates-io].
              outputHashes."moq-net-0.2.20" = "sha256-QZOxhfXR1qr7gxGUTmUtltm/Zntt/hDn6o2HsBu2s48=";
            };
            nativeBuildInputs = [
              pkgs.pkg-config
              pkgs.cmake
              pkgs.makeWrapper
            ];
            dontUseCmakeConfigure = true;
            buildInputs = deps.media;
            postBuild = pkgs.lib.optionalString pkgs.stdenv.hostPlatform.isLinux ''
              $CC -O2 -Wall -Wextra -Werror packaging/pam/teleport-pam.c -o teleport-pam -lpam
            '';
            preCheck = pkgs.lib.optionalString pkgs.stdenv.hostPlatform.isLinux ''
              $CC -O2 -Wall -Wextra -Werror packaging/pam/test-teleport-pam.c -o test-teleport-pam
              ./test-teleport-pam
            '';
            postInstall = pkgs.lib.optionalString pkgs.stdenv.hostPlatform.isLinux ''
              install -Dm755 teleport-pam $out/libexec/teleport-pam
              install -Dm644 packaging/pam/teleport-pam.c $out/share/teleport/teleport-pam.c
              install -Dm644 packaging/pam/teleport.arch $out/share/teleport/teleport.pam.arch
            '';
            postFixup = ''
              wrapProgram $out/bin/teleport \
                ${pkgs.lib.optionalString pkgs.stdenv.hostPlatform.isLinux ''
                  --unset LD_LIBRARY_PATH --unset LD_PRELOAD \
                  --run '. ${pkgs.replaceVars ./nix/mesa-runtime.sh { mesa = pkgs.mesa; }}' \
                  --unset SDL_DYNAMIC_API --unset SDL3_DYNAMIC_API \
                  --set FONTCONFIG_FILE "${import ./nix/fontconfig.nix { inherit pkgs; }}" \
                  --set FONTCONFIG_PATH "${pkgs.fontconfig.out}/etc/fonts" \
                  --set-default LIBVA_DRIVERS_PATH "${deps.vaDrivers}/lib/dri" \
                  ${pkgs.lib.optionalString pkgs.stdenv.hostPlatform.isx86_64 ''--set-default ONEVPL_PRIORITY_PATH "${pkgs.vpl-gpu-rt}/lib"''} \
                  --run 'export GST_REGISTRY="''${GST_REGISTRY:-''${XDG_CACHE_HOME:-$HOME/.cache}/teleport/${builtins.baseNameOf (toString deps.vaDrivers)}-registry.bin}"' \
                ''} \
                --prefix GST_PLUGIN_SYSTEM_PATH_1_0 : "${pkgs.lib.makeSearchPath "lib/gstreamer-1.0" (map pkgs.lib.getLib deps.media)}" \
                --set TELEPORT_FONT "${pkgs.dejavu_fonts}/share/fonts/truetype/DejaVuSans.ttf" \
                --set TELEPORT_LIBFIDO2 "${pkgs.lib.getLib pkgs.libfido2}/lib/libfido2${pkgs.stdenv.hostPlatform.extensions.sharedLibrary}" \
                ${pkgs.lib.optionalString pkgs.stdenv.hostPlatform.isLinux ''--prefix PATH : "${
                  pkgs.lib.makeBinPath [
                    pkgs.wl-clipboard
                    pkgs.xclip
                    # pw-cli: the host's virtual microphone (host --microphone)
                    pkgs.pipewire
                  ]
                }"''} \
                --set TELEPORT_PACKAGED 1
            ''
            + pkgs.lib.optionalString pkgs.stdenv.hostPlatform.isLinux ''
              install -Dm644 packaging/teleport.desktop $out/share/applications/teleport.desktop
              substituteInPlace $out/share/applications/teleport.desktop --replace-fail 'Exec=teleport' "Exec=$out/bin/teleport"
              install -Dm644 packaging/teleport-open.desktop $out/share/applications/teleport-open.desktop
              substituteInPlace $out/share/applications/teleport-open.desktop --replace-fail 'Exec=teleport' "Exec=$out/bin/teleport"
              install -Dm644 packaging/icons/dev.teleport.Teleport.svg $out/share/icons/hicolor/scalable/apps/dev.teleport.Teleport.svg
              for size in 16 24 32 48 64 128 256 512; do
                install -Dm644 packaging/icons/hicolor/''${size}x''${size}/dev.teleport.Teleport.png \
                  $out/share/icons/hicolor/''${size}x''${size}/apps/dev.teleport.Teleport.png
              done
            ''
            + pkgs.lib.optionalString pkgs.stdenv.hostPlatform.isDarwin ''
              mkdir -p $out/Applications/Teleport.app/Contents/MacOS
              cp packaging/Info.plist $out/Applications/Teleport.app/Contents/Info.plist
              makeWrapper $out/bin/teleport $out/Applications/Teleport.app/Contents/MacOS/teleport
            '';
            meta.mainProgram = "teleport";
          };
          # Relocatable Linux bundle and its packages (docs/distribution.md).
          # The system-login PAM helper, linked to run on the distribution's
          # glibc and libpam (.deb, .rpm, Arch package; nix/pam-helper.nix).
          pamHelper = import ./nix/pam-helper.nix { inherit pkgs; };
          bundle = import ./nix/bundle.nix {
            inherit pkgs teleport pamHelper;
            inherit (deps) media;
          };
        in
        {
          default = teleport;
        }
        // pkgs.lib.optionalAttrs pkgs.stdenv.hostPlatform.isLinux {
          inherit bundle;
          pam-helper = pamHelper;
          relay-image = import ./nix/relay-image.nix { inherit pkgs teleport; };
          appimage = import ./nix/appimage.nix { inherit pkgs bundle; };
          tarball = import ./nix/tarball.nix { inherit pkgs bundle; };
          deb = import ./nix/linux-packages.nix {
            inherit pkgs bundle;
            format = "deb";
          };
          rpm = import ./nix/linux-packages.nix {
            inherit pkgs bundle;
            format = "rpm";
          };
        }
      );
      formatter = forAllSystems (system: nixpkgs.legacyPackages.${system}.nixfmt);
      devShells = forAllSystems (
        system:
        let
          pkgs = import nixpkgs { inherit system; };
          deps = environment pkgs;
        in
        {
          default = pkgs.mkShell {
            packages =
              with pkgs;
              [
                cargo
                rustc
                rustfmt
                clippy
                rust-analyzer
                git
                openssh
                nixfmt
              ]
              ++ deps.tools
              ++ deps.media
              ++ pkgs.lib.optionals pkgs.stdenv.hostPlatform.isLinux [
                pkgs.xorg-server
                # tests/network.rs: netem link shaping in a user namespace
                pkgs.iproute2
                pkgs.util-linux
              ];

            RUST_SRC_PATH = "${pkgs.rustPlatform.rustLibSrc}";
            TELEPORT_FONT = "${pkgs.dejavu_fonts}/share/fonts/truetype/DejaVuSans.ttf";
            TELEPORT_LIBFIDO2 = "${pkgs.lib.getLib pkgs.libfido2}/lib/libfido2${pkgs.stdenv.hostPlatform.extensions.sharedLibrary}";
            shellHook = pkgs.lib.optionalString pkgs.stdenv.hostPlatform.isLinux ''
              export LIBVA_DRIVERS_PATH="''${LIBVA_DRIVERS_PATH:-${deps.vaDrivers}/lib/dri}"
              ${pkgs.lib.optionalString pkgs.stdenv.hostPlatform.isx86_64 ''export ONEVPL_PRIORITY_PATH="''${ONEVPL_PRIORITY_PATH:-${pkgs.vpl-gpu-rt}/lib}"''}
              export GST_REGISTRY="''${GST_REGISTRY:-''${XDG_CACHE_HOME:-$HOME/.cache}/teleport/${builtins.baseNameOf (toString deps.vaDrivers)}-registry.bin}"
            '';
          };
        }
      );
    };
}
