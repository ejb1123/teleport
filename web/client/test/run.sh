#!/usr/bin/env bash
# @teleport/web end to end (test/e2e.py) in headless Chromium against a real
# relay and host, using the bundle exactly as `nix build .#web-client` ships
# it. Needs nix and KVM-free local UDP.
set -euo pipefail
ROOT=$(cd "$(dirname "$0")/../../.." && pwd)
TOOLS=$ROOT/.web-test
mkdir -p "$TOOLS"
nix develop "$ROOT" --command cargo build --locked
rm -rf "$ROOT/web/client/dist"
nix build "$ROOT#web-client" --out-link "$ROOT/web/client/dist"
nix build --impure --expr '(builtins.getFlake "nixpkgs").legacyPackages.x86_64-linux.playwright-driver.browsers' \
  --out-link "$TOOLS/browsers"
nix build --impure --expr 'let p = (builtins.getFlake "nixpkgs").legacyPackages.x86_64-linux; in p.python3.withPackages (ps: [ ps.playwright ])' \
  --out-link "$TOOLS/python"
# The debug binary finds its media plugins through the devShell.
nix develop "$ROOT" --command env TELEPORT_BIN="$ROOT/target/debug/teleport" BROWSERS="$TOOLS/browsers" \
  "$TOOLS/python/bin/python3" "$ROOT/web/client/test/e2e.py"
