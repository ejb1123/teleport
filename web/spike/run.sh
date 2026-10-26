#!/usr/bin/env bash
# Browser client risk spike (docs/design/self-hosted-remote-support.md §7):
# builds what the checks need and runs risks 1-4 in headless Chromium.
#   bash web/spike/run.sh [1 2 3 4]      (default: all)
# Needs nix. Builds a debug teleport, @moq/net 0.3.5 + esbuild, the e2e
# WASM crate and Playwright's browsers from nixpkgs, under .spike/.
set -euo pipefail
ROOT=$(cd "$(dirname "$0")/../.." && pwd)
SPIKE=$ROOT/.spike
mkdir -p "$SPIKE"
cd "$ROOT"
nix develop "$ROOT" --command cargo build --locked
cp target/debug/teleport "$SPIKE/teleport"
cat > "$SPIKE/env.nix" <<'NIX'
let pkgs = (builtins.getFlake "nixpkgs").legacyPackages.x86_64-linux;
in {
  py = pkgs.buildEnv { name = "web-spike-py"; paths = [ (pkgs.python3.withPackages (p: [ p.playwright ])) ]; };
  browsers = pkgs.playwright-driver.browsers;
  wasm = pkgs.buildEnv { name = "web-spike-wasm"; paths = [ pkgs.wasm-bindgen-cli pkgs.lld pkgs.llvmPackages.clang-unwrapped ]; };
}
NIX
nix build --impure -f "$SPIKE/env.nix" py --out-link "$SPIKE/pyenv"
nix build --impure -f "$SPIKE/env.nix" browsers --out-link "$SPIKE/browsers"
nix build --impure -f "$SPIKE/env.nix" wasm --out-link "$SPIKE/wasmenv"
# risk 2: @moq/net 0.3.5 speaks moq-lite-05, the version teleport pins.
mkdir -p "$SPIKE/js"
[ -f "$SPIKE/js/package.json" ] || echo '{"name":"spike","private":true,"type":"module"}' > "$SPIKE/js/package.json"
nix shell nixpkgs#nodejs --command npm --prefix "$SPIKE/js" install --no-audit --no-fund @moq/net@0.3.5
NODE_PATH=$SPIKE/js/node_modules nix shell nixpkgs#esbuild --command \
  esbuild web/spike/moq-spike.js --bundle --format=iife --target=es2022 --outfile=web/spike/moq-spike.bundle.js
# risk 3: src/e2e.rs for wasm32.
W=$SPIKE/wasmenv/bin
nix develop "$ROOT" --command bash -c "cd web/e2e-wasm && PATH=\$PATH:$W CC_wasm32_unknown_unknown=$W/clang \
  CARGO_TARGET_WASM32_UNKNOWN_UNKNOWN_LINKER=$W/wasm-ld cargo build --release --target wasm32-unknown-unknown && \
  wasm-bindgen --target web --out-dir ../spike/pkg target/wasm32-unknown-unknown/release/teleport_e2e_wasm.wasm"
# risk 4: two seconds of a session's H.264 access units.
nix develop "$ROOT" --command bash -c "TELEPORT_DUMP_H264=$ROOT/web/spike/h264.bin cargo test --locked --bin teleport dump_h264 -- --ignored"
cd web/spike
for risk in ${@:-1 2 3 4}; do
  nix develop "$ROOT" --command env TELEPORT_BIN="$SPIKE/teleport" BROWSERS="$SPIKE/browsers" \
    "$SPIKE/pyenv/bin/python3" "risk$risk.py"
done
