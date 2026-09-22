{ pkgs }:
# makeFontsConf inherits the Nix fontconfig default's absolute /etc/fonts/conf.d
# include. On another distribution those rules can require a newer fontconfig
# parser. Keep Teleport's small, native UI and media plugins on one matching font
# configuration instead of applying the host distribution's font rules.
pkgs.writeText "teleport-fonts.conf" ''
  <?xml version="1.0"?>
  <!DOCTYPE fontconfig SYSTEM "urn:fontconfig:fonts.dtd">
  <fontconfig>
    <dir>${pkgs.dejavu_fonts}/share/fonts</dir>
    <cachedir prefix="xdg">teleport/fontconfig</cachedir>
    <alias>
      <family>sans-serif</family>
      <prefer><family>DejaVu Sans</family></prefer>
    </alias>
    <alias>
      <family>serif</family>
      <prefer><family>DejaVu Serif</family></prefer>
    </alias>
    <alias>
      <family>monospace</family>
      <prefer><family>DejaVu Sans Mono</family></prefer>
    </alias>
    <alias>
      <family>system-ui</family>
      <prefer><family>DejaVu Sans</family></prefer>
    </alias>
  </fontconfig>
''
