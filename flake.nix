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
            gst_all_1.gst-plugins-bad
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
        pkgs.lib.optionalAttrs pkgs.stdenv.hostPlatform.isLinux {
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
            version = "0.4.0";
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
            postFixup = ''
              wrapProgram $out/bin/teleport \
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
          };
        }
      );
    };
}
