# The docs/*.md pages installed in /usr/share/doc/teleport (nix/bundle.nix,
# nix/linux-packages.nix): every page except development notes and audits.
{ lib }:
let
  internal = [
    "todo.md"
    "status.md"
    "review-2026-10.md"
    "parsec-comparison.md"
    "ux-audit.md"
    "remote-desktop-audit.md"
  ];
in
builtins.filter (name: lib.hasSuffix ".md" name && !builtins.elem name internal) (
  builtins.attrNames (builtins.readDir ../docs)
)
