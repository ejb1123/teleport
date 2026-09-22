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
          ]
          ++ pkgs.lib.optionals pkgs.stdenv.hostPlatform.isLinux [
            pkgs.pipewire
            pkgs.libei
            pkgs.wayland
            pkgs.libxkbcommon
            pkgs.libdrm
            pkgs.libva
            pkgs.libx11
            pkgs.libxext
            pkgs.libxdamage
            pkgs.libxtst
          ];
      };
    in
    {
      packages = forAllSystems (
        system:
        let
          pkgs = import nixpkgs { inherit system; };
          deps = environment pkgs;
        in
        {
          default = pkgs.rustPlatform.buildRustPackage {
            pname = "teleport";
            version = "0.1.0";
            src = pkgs.lib.fileset.toSource {
              root = ./.;
              fileset = pkgs.lib.fileset.unions [
                ./Cargo.toml
                ./Cargo.lock
                ./src
                ./tests
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
                --prefix GST_PLUGIN_SYSTEM_PATH_1_0 : "${pkgs.lib.makeSearchPath "lib/gstreamer-1.0" (map pkgs.lib.getLib deps.media)}"
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
                nixfmt
              ]
              ++ deps.tools
              ++ deps.media
              ++ pkgs.lib.optionals pkgs.stdenv.hostPlatform.isLinux [ pkgs.xorg-server ];

            RUST_SRC_PATH = "${pkgs.rustPlatform.rustLibSrc}";
          };
        }
      );
    };
}
