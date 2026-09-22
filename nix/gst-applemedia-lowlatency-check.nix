# Applies the patch to the pinned source and exercises its platform-neutral SPS
# helper with real Main/Main10 low-delay and reordered streams on Linux/macOS.
{ pkgs }:
pkgs.stdenv.mkDerivation {
  pname = "teleport-applemedia-lowlatency-check";
  version = "1";
  src = pkgs.gst_all_1.gst-plugins-bad.src;
  patches = [ ./gst-applemedia-lowlatency.patch ];
  nativeBuildInputs = [ pkgs.pkg-config ];
  buildInputs = with pkgs.gst_all_1; [
    gst-plugins-base
    gst-plugins-bad
    gst-plugins-ugly
  ];
  dontConfigure = true;
  buildPhase = ''
    cp ${./gst-vtdec-hevc-reorder.h} sys/applemedia/gst-vtdec-hevc-reorder.h
    $CC -Wall -Wextra -Werror -Isys/applemedia \
      $(pkg-config --cflags gstreamer-app-1.0 gstreamer-codecparsers-1.0) \
      ${./gst-applemedia-lowlatency-test.c} \
      $(pkg-config --libs gstreamer-app-1.0 gstreamer-codecparsers-1.0) -o reorder-test
  '';
  doCheck = true;
  checkPhase = ''
    export FONTCONFIG_FILE="${pkgs.makeFontsConf { fontDirectories = [ ]; }}"
    export XDG_CACHE_HOME="$TMPDIR/cache"
    export GST_PLUGIN_SYSTEM_PATH_1_0="${
      pkgs.lib.makeSearchPath "lib/gstreamer-1.0" (
        map pkgs.lib.getLib [
          pkgs.gst_all_1.gstreamer
          pkgs.gst_all_1.gst-plugins-base
          pkgs.gst_all_1.gst-plugins-bad
        ]
      )
    }"
    export GST_REGISTRY="$TMPDIR/teleport-reorder-registry.bin"
    ./reorder-test
  '';
  installPhase = ''
    mkdir -p $out/bin
    cp reorder-test $out/bin/teleport-applemedia-reorder-check
  '';
}
