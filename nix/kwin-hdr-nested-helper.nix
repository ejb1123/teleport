{ pkgs }:
pkgs.stdenv.mkDerivation {
  pname = "teleport-kwin-hdr-nested-capture";
  version = "1";
  dontUnpack = true;
  nativeBuildInputs = [
    pkgs.pkg-config
    pkgs.wayland-scanner
  ];
  buildInputs = [
    pkgs.wayland
    pkgs.gst_all_1.gst-plugins-base
  ];
  buildPhase = ''
    protocol=${pkgs.kdePackages.plasma-wayland-protocols}/share/plasma-wayland-protocols/zkde-screencast-unstable-v1.xml
    wayland-scanner client-header "$protocol" screencast-client.h
    wayland-scanner private-code "$protocol" screencast-client.c
    $CC -I. ${./kwin-hdr-nested-capture.c} screencast-client.c \
      $(pkg-config --cflags --libs wayland-client gstreamer-app-1.0 gstreamer-video-1.0) \
      -lm -o nested-capture
  '';
  installPhase = ''
    mkdir -p $out/bin
    cp nested-capture $out/bin/
  '';
}
