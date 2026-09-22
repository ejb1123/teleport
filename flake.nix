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
        }
      );
      packages = forAllSystems (
        system:
        let
          pkgs = import nixpkgs { inherit system; };
          deps = environment pkgs;
        in
        {
          default = pkgs.rustPlatform.buildRustPackage {
            pname = "teleport";
            version = "0.5.3";
            src = pkgs.lib.fileset.toSource {
              root = ./.;
              fileset = pkgs.lib.fileset.unions [
                ./Cargo.toml
                ./Cargo.lock
                ./src
                ./tests
                ./packaging
              ];
            };
            cargoLock.lockFile = ./Cargo.lock;
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
                  ]
                }"''} \
                --set TELEPORT_PACKAGED 1
            ''
            + pkgs.lib.optionalString pkgs.stdenv.hostPlatform.isLinux ''
              install -Dm644 packaging/teleport.desktop $out/share/applications/teleport.desktop
              substituteInPlace $out/share/applications/teleport.desktop --replace-fail 'Exec=teleport' "Exec=$out/bin/teleport"
            ''
            + pkgs.lib.optionalString pkgs.stdenv.hostPlatform.isDarwin ''
              mkdir -p $out/Applications/Teleport.app/Contents/MacOS
              cp packaging/Info.plist $out/Applications/Teleport.app/Contents/Info.plist
              makeWrapper $out/bin/teleport $out/Applications/Teleport.app/Contents/MacOS/teleport
            '';
            meta.mainProgram = "teleport";
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
              ++ pkgs.lib.optionals pkgs.stdenv.hostPlatform.isLinux [ pkgs.xorg-server ];

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
