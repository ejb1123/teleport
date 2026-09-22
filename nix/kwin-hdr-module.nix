# Import explicitly, then enable. Installing this does not hot-replace KWin;
# test at a planned next graphical login, never with `kwin_wayland --replace`.
{ config, lib, ... }:
{
  options.services.teleport-desktop.experimentalKwinHdr = lib.mkEnableOption "experimental KWin 6.7.5 output screencast HDR negotiation";

  config = lib.mkIf config.services.teleport-desktop.experimentalKwinHdr {
    nixpkgs.overlays = [
      (_final: prev: {
        kdePackages = prev.kdePackages.overrideScope (
          _kfinal: kprev: {
            kwin = import ./kwin-hdr.nix {
              pkgs = prev // {
                kdePackages = kprev;
              };
            };
          }
        );
      })
    ];
  };
}
