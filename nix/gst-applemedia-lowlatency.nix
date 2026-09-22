# Application-local macOS VideoToolbox fix; no system GStreamer replacement.
{ pkgs }:
pkgs.gst_all_1.gst-plugins-bad.overrideAttrs (old: {
  patches = (old.patches or [ ]) ++ [ ./gst-applemedia-lowlatency.patch ];
  postPatch = (old.postPatch or "") + ''
    cp ${./gst-vtdec-hevc-reorder.h} sys/applemedia/gst-vtdec-hevc-reorder.h
  '';
})
