#!/usr/bin/env bash
# Evaluate the NixOS module (nix/module.nix) in minimal SDDM/Plasma
# configurations and check what it generates: automatic login, the lock
# service, Wake-on-LAN, polkit and the host's flags (nix/module-eval.nix).
# Nothing is built or switched.
#
#   tools/check-module.sh            # check; prints the settings with -v
set -euo pipefail
cd "$(dirname "$0")/.."
system=$(nix eval --raw --impure --expr builtins.currentSystem)
if [ "${1:-}" = "-v" ]; then
  nix eval --json ".#checks.$system.module-eval.settings"
  echo
fi
result=$(nix eval ".#checks.$system.module-eval.passed")
[ "$result" = true ]
echo "module evaluation: all expectations hold"
