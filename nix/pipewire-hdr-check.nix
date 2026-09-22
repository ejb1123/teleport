# Fast, daemon-free negotiation + RGB channel-order regression check.
{ pkgs }:
pkgs.stdenv.mkDerivation {
  pname = "teleport-pipewire-hdr-check";
  version = "1";
  src = pkgs.pipewire.src;
  patches = [ ./pipewire-hdr.patch ];
  nativeBuildInputs = [ pkgs.pkg-config ];
  buildInputs = [
    pkgs.pipewire
    pkgs.gst_all_1.gst-plugins-base
  ];
  dontConfigure = true;
  buildPhase = ''
    runHook preBuild
    cp ${pkgs.writeText "config.h" ""} config.h
    $CC -I. -Isrc/gst \
      $(pkg-config --cflags libspa-0.2 gstreamer-video-1.0 gstreamer-audio-1.0 gstreamer-allocators-1.0) \
      ${./pipewire-hdr-test.c} src/gst/gstpipewireformat.c \
      $(pkg-config --libs gstreamer-video-1.0 gstreamer-audio-1.0 gstreamer-allocators-1.0) \
      -o format-test
    runHook postBuild
  '';
  doCheck = true;
  checkPhase = ''
    runHook preCheck
    ./format-test
    runHook postCheck
  '';
  installPhase = ''
    mkdir -p $out/bin
    cp format-test $out/bin/teleport-pipewire-hdr-check
  '';
}
