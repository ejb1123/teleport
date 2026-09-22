# Application-local PipeWire GStreamer plugin; does not replace the system daemon.
{ pkgs }:
pkgs.pipewire.overrideAttrs (old: {
  patches = (old.patches or [ ]) ++ [ ./pipewire-hdr.patch ];
})
