# An OCI image running only `teleport relay` (docs/relay.md#container-image),
# for self-hosting the relay on Kubernetes or any container runtime.
#
#   nix build .#relay-image && ./result | docker load     # or skopeo/crane
#
# The relay forwards end-to-end encrypted objects it cannot read, so the
# image holds no secrets: pass the access key through TELEPORT_RELAY_KEY
# and mount a volume at /var/lib/teleport-relay to keep the relay's
# certificate (and so its pinned fingerprint) across restarts.
{ pkgs, teleport }:
let
  # The unwrapped binary alone: the relay needs no media plugins, drivers,
  # fonts or desktop helpers, which the wrapper's environment references
  # (about 2 GB of image). The binary keeps only its linked libraries.
  relay = pkgs.runCommand "teleport-relay-bin-${teleport.version}" { } ''
    install -Dm755 ${teleport}/bin/.teleport-wrapped $out/bin/teleport
  '';
in
pkgs.dockerTools.streamLayeredImage {
  name = "teleport-relay";
  tag = teleport.version;
  contents = [ pkgs.cacert ];
  extraCommands = ''
    mkdir -p var/lib/teleport-relay tmp
    chmod 1777 tmp
  '';
  fakeRootCommands = ''
    chown 65534:65534 var/lib/teleport-relay
  '';
  enableFakechroot = false;
  config = {
    User = "65534:65534";
    Entrypoint = [
      "${relay}/bin/teleport"
      "relay"
    ];
    Cmd = [
      "--listen"
      "0.0.0.0:4444"
      "--identity-dir"
      "/var/lib/teleport-relay"
    ];
    Env = [
      "HOME=/var/lib/teleport-relay"
      "RUST_LOG=teleport=info"
    ];
    ExposedPorts = {
      "4444/udp" = { };
      "3478/udp" = { };
    };
    Volumes = {
      "/var/lib/teleport-relay" = { };
    };
  };
}
