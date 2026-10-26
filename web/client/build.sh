#!/usr/bin/env bash
# Build @teleport/web: the e2e WASM (src/e2e.rs through web/e2e-wasm), its
# wasm-bindgen glue, then one ESM bundle with @moq/net.
#   bash web/client/build.sh            (from a shell with cargo for
#   wasm32-unknown-unknown, clang, wasm-ld, wasm-bindgen, esbuild and
#   node_modules installed; the flake's .#web-client does all of it)
# Output: web/client/dist/{teleport-web.js,teleport_e2e_wasm_bg.wasm}.
set -euo pipefail
HERE=$(cd "$(dirname "$0")" && pwd)
WASM_CRATE=$HERE/../e2e-wasm
: "${CC_wasm32_unknown_unknown:=clang}"
export CC_wasm32_unknown_unknown
(cd "$WASM_CRATE" && cargo build --release --locked --target wasm32-unknown-unknown)
rm -rf "$HERE/wasm" "$HERE/dist"
wasm-bindgen --target web --out-dir "$HERE/wasm" \
  "${CARGO_TARGET_DIR:-$WASM_CRATE/target}/wasm32-unknown-unknown/release/teleport_e2e_wasm.wasm"
mkdir -p "$HERE/dist"
esbuild "$HERE/src/index.ts" --bundle --format=esm --target=es2022 \
  --outfile="$HERE/dist/teleport-web.js"
cp "$HERE/wasm/teleport_e2e_wasm_bg.wasm" "$HERE/dist/"
