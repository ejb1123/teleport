# Experimental, opt-in compositor package. This does not activate or restart KWin.
# Import with the NixOS host's pkgs (NOT Teleport's separately pinned nixpkgs).
{ pkgs }:
assert pkgs.lib.assertMsg (
  pkgs.kdePackages.kwin.version == "6.7.5"
) "Teleport HDR screencast patch is audited only for KWin 6.7.5";
pkgs.kdePackages.kwin.overrideAttrs (old: {
  patches = (old.patches or [ ]) ++ [ ./kwin-hdr.patch ];
})
