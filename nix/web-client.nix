# @teleport/web (web/client): the browser client as a static ESM bundle,
#   nix build .#web-client  ->  result/{teleport-web.js,teleport_e2e_wasm_bg.wasm}
# The end-to-end layer is src/e2e.rs itself, compiled to wasm32 by
# web/e2e-wasm; @moq/net comes from web/client/package-lock.json. The unit
# tests (vitest) and the type check run in the build.
{ pkgs }:
let
  src = pkgs.lib.fileset.toSource {
    root = ../.;
    fileset = pkgs.lib.fileset.unions [
      ../src/e2e.rs
      ../web/e2e-wasm/Cargo.toml
      ../web/e2e-wasm/Cargo.lock
      ../web/e2e-wasm/src
      ../web/e2e-wasm/fingerprint
      ../web/client/package.json
      ../web/client/package-lock.json
      ../web/client/tsconfig.json
      ../web/client/build.sh
      ../web/client/src
      ../web/client/test/unit.test.ts
    ];
  };
in
pkgs.stdenv.mkDerivation {
  pname = "teleport-web-client";
  version = (builtins.fromJSON (builtins.readFile ../web/client/package.json)).version;
  inherit src;
  cargoDeps = pkgs.rustPlatform.importCargoLock { lockFile = ../web/e2e-wasm/Cargo.lock; };
  cargoRoot = "web/e2e-wasm";
  npmDeps = pkgs.fetchNpmDeps {
    src = ../web/client;
    hash = "sha256-5DHD3EaeYLM1C0T7/FX6ZYAPZV7DW0MD0TNrJEym6QE=";
  };
  npmRoot = "web/client";
  nativeBuildInputs = [
    pkgs.rustPlatform.cargoSetupHook
    pkgs.cargo
    pkgs.rustc
    pkgs.lld
    pkgs.llvmPackages.clang-unwrapped
    pkgs.wasm-bindgen-cli
    pkgs.esbuild
    pkgs.nodejs
    pkgs.npmHooks.npmConfigHook
  ];
  buildPhase = ''
    runHook preBuild
    export CC_wasm32_unknown_unknown=clang
    export CARGO_TARGET_WASM32_UNKNOWN_UNKNOWN_LINKER=wasm-ld
    bash web/client/build.sh
    runHook postBuild
  '';
  doCheck = true;
  checkPhase = ''
    runHook preCheck
    (cd web/client && npx tsc -p tsconfig.json && npx vitest run)
    runHook postCheck
  '';
  installPhase = ''
    runHook preInstall
    mkdir -p $out
    cp web/client/dist/* $out/
    runHook postInstall
  '';
}
