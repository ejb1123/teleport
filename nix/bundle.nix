{
  pkgs,
  teleport,
  pamHelper,
  media,
}:
# Relocatable Linux bundle (an AppDir): the Nix-built binary with every shared
# library it, its GStreamer plugins and its helpers load, started through the
# bundled glibc loader by AppRun. The AppImage, .deb and .rpm all package this
# directory. GPU driver userspace (libGL/libEGL/GLX dispatch, Mesa, libgbm,
# Vulkan drivers, NVIDIA/CUDA, VA-API drivers) is deliberately left out: it
# must match the running kernel driver, so AppRun searches the system's
# directories for it after the bundle (docs/distribution.md). Loaders that only
# find drivers (libva, the Vulkan loader, which FFmpeg links) are bundled.
let
  inherit (pkgs) lib;
  libc = pkgs.stdenv.cc.libc;
  loader = builtins.baseNameOf pkgs.stdenv.cc.bintools.dynamicLinker;
  triple =
    {
      x86_64 = "x86_64-linux-gnu";
      aarch64 = "aarch64-linux-gnu";
    }
    .${pkgs.stdenv.hostPlatform.parsed.cpu.name};
  pluginDirs = lib.concatMapStringsSep " " (p: "${lib.getLib p}/lib/gstreamer-1.0") media;
  # Mesa's drivers (libgallium, dri, the GLX/EGL vendor libraries).
  excludedStorePaths = toString pkgs.mesa;
  fonts = pkgs.writeText "teleport-bundle-fonts.conf" ''
    <?xml version="1.0"?>
    <!DOCTYPE fontconfig SYSTEM "urn:fontconfig:fonts.dtd">
    <fontconfig>
      <dir prefix="relative">../fonts</dir>
      <cachedir prefix="xdg">teleport/fontconfig</cachedir>
      <alias>
        <family>sans-serif</family>
        <prefer><family>DejaVu Sans</family></prefer>
      </alias>
      <alias>
        <family>monospace</family>
        <prefer><family>DejaVu Sans Mono</family></prefer>
      </alias>
    </fontconfig>
  '';
in
pkgs.runCommand "teleport-bundle-${teleport.version}"
  {
    nativeBuildInputs = [ pkgs.patchelf ];
    inherit (teleport) version;
    passthru = { inherit loader triple; };
  }
  ''
    root=$out
    mkdir -p $root/usr/bin $root/usr/lib/fallback $root/usr/lib/libva $root/usr/libexec/teleport/bin $root/usr/libexec/teleport/elf \
      $root/usr/share/teleport $root/usr/share/fonts/truetype $root/usr/share/applications

    is_elf() { [ "$(head -c 4 "$1" 2>/dev/null | od -An -c | tr -d ' ')" = '177ELF' ]; }
    excluded_store() {
      for e in ${excludedStorePaths}; do
        case "$1" in "$e"/*) return 0 ;; esac
      done
      return 1
    }
    # Driver libraries, whatever package provides them: never bundled.
    excluded_name() {
      case "$1" in
        libGLX_* | libEGL_* | libglapi.so* | libgallium* | libcuda.so* | libnvidia-* | \
        libnvcuvid.so* | libmfx-gen.so* | libmfxhw64.so*) return 0 ;;
      esac
      return 1
    }
    # GL dispatch (libglvnd) and libgbm (Nix's SDL3 links it): in
    # usr/lib/fallback, which AppRun searches after the system's directories,
    # so the system's copies, matching its drivers, win; these only let
    # Teleport start where there are none (minimal containers, servers).
    # libva goes to usr/lib/libva, which AppRun puts before the system's
    # directories or after them, whichever libva is newer: a VA driver built
    # for a newer libva does not load into an older one, and FFmpeg needs at
    # least the bundled version.
    target_dir() {
      case "$1" in
        libGL.so* | libEGL.so* | libGLX.so* | libGLdispatch.so* | libOpenGL.so* | \
        libGLESv1_CM.so* | libGLESv2.so* | libgbm.so*) echo "$root/usr/lib/fallback" ;;
        libva.so* | libva-*.so*) echo "$root/usr/lib/libva" ;;
        *) echo "$root/usr/lib" ;;
      esac
    }

    declare -A seen_dir source_dir
    # Copies the libraries of a store directory into usr/lib (first copy of a
    # name wins) and follows their RUNPATHs. Whole directories rather than
    # NEEDED entries, so libraries loaded with dlopen (SDL's Wayland/X11/audio
    # backends, FFmpeg's VA-API) come along.
    add_dir() {
      local dir
      dir=$(readlink -f "$1") || return 0
      case "$dir" in /nix/store/*) ;; *) return 0 ;; esac
      [ -d "$dir" ] || return 0
      [ -z "''${seen_dir[$dir]-}" ] || return 0
      seen_dir[$dir]=1
      excluded_store "$dir" && return 0
      local file name target
      for file in "$dir"/*.so "$dir"/*.so.*; do
        [ -e "$file" ] || continue
        name=$(basename "$file")
        excluded_name "$name" && continue
        is_elf "$file" || continue
        target=$(target_dir "$name")
        if [ -e "$target/$name" ]; then
          continue
        fi
        source_dir[$name]=$dir
        if [ -L "$file" ] && [ "$(readlink "$file")" = "$(basename "$(readlink "$file")")" ]; then
          cp -P "$file" "$target/$name"
        else
          cp -L "$file" "$target/$name"
          chmod u+w "$target/$name"
        fi
        follow "$file"
      done
    }
    follow() {
      local rpath entry origin
      local -a entries
      rpath=$(patchelf --print-rpath "$1" 2>/dev/null) || return 0
      origin=$(dirname "$(readlink -f "$1")")
      IFS=: read -ra entries <<<"$rpath"
      for entry in "''${entries[@]}"; do
        add_dir "''${entry//\$ORIGIN/$origin}"
      done
      # A few libraries name a dependency by its absolute store path.
      while IFS= read -r entry; do
        case "$entry" in /nix/store/*) add_dir "$(dirname "$entry")" ;; esac
      done < <(patchelf --print-needed "$1" 2>/dev/null)
    }
    # Copies an executable or plugin and follows its libraries.
    add_file() {
      install -Dm755 "$1" "$2"
      chmod u+w "$2"
      follow "$1"
    }

    add_dir ${libc}/lib
    add_file ${teleport}/bin/.teleport-wrapped $root/usr/bin/teleport
    add_dir ${lib.getLib pkgs.libfido2}/lib

    for plugins in ${pluginDirs}; do
      [ -d "$plugins" ] || continue
      for plugin in "$plugins"/*.so; do
        name=$(basename "$plugin")
        [ -e "$root/usr/lib/gstreamer-1.0/$name" ] && continue
        add_file "$plugin" "$root/usr/lib/gstreamer-1.0/$name"
      done
    done

    # The PipeWire client library looks for its SPA plugins and modules next
    # to itself; AppRun points it at these copies.
    pipewire=$(dirname "''${source_dir[libpipewire-0.3.so.0]}")
    for sub in spa-0.2 pipewire-0.3; do
      while IFS= read -r -d "" file; do
        rel=''${file#"$pipewire/lib/"}
        add_file "$file" "$root/usr/lib/$rel"
      done < <(find "$pipewire/lib/$sub" -name '*.so' -print0)
    done
    cp -rL "$pipewire/share/pipewire" $root/usr/share/pipewire
    chmod -R u+w $root/usr/share/pipewire

    helper() {
      add_file "$1" "$root/usr/libexec/teleport/elf/$2"
      substitute ${../packaging/linux/helper} "$3" --subst-var-by loader ${loader} --subst-var-by name "$2"
      chmod 755 "$3"
    }
    helper ${pkgs.gst_all_1.gstreamer.out}/libexec/gstreamer-1.0/gst-plugin-scanner gst-plugin-scanner \
      $root/usr/libexec/teleport/gst-plugin-scanner
    for tool in wl-copy wl-paste; do
      helper ${pkgs.wl-clipboard}/bin/$tool $tool $root/usr/libexec/teleport/bin/$tool
    done
    helper ${pkgs.xclip}/bin/xclip xclip $root/usr/libexec/teleport/bin/xclip

    # Each copy keeps only what the bundled loader needs: no Nix RUNPATH, so a
    # library missing from the bundle is looked up on the system, not in a
    # /nix/store that may exist with other versions.
    while IFS= read -r -d "" file; do
      if [ ! -L "$file" ] && is_elf "$file"; then
        patchelf --remove-rpath "$file" 2>/dev/null || true
        while IFS= read -r needed; do
          case "$needed" in
            /nix/store/*) patchelf --replace-needed "$needed" "$(basename "$needed")" "$file" ;;
          esac
        done < <(patchelf --print-needed "$file" 2>/dev/null)
      fi
    done < <(find $root/usr -type f -print0)

    # Every library any bundled file links must be in the bundle, unless it is
    # a driver library the system provides.
    missing=0
    while IFS= read -r -d "" file; do
      [ ! -L "$file" ] && is_elf "$file" || continue
      while IFS= read -r needed; do
        [ -e "$(target_dir "$needed")/$needed" ] ||
          excluded_name "$needed" || [ "$needed" = "${loader}" ] || {
          echo "''${file#"$root/"} needs $needed, which is not bundled" >&2
          missing=1
        }
      done < <(patchelf --print-needed "$file" 2>/dev/null)
    done < <(find $root/usr -type f -print0)
    [ "$missing" = 0 ] || exit 1
    [ -e $root/usr/lib/${loader} ] || { echo "loader ${loader} missing" >&2; exit 1; }
    [ -e $root/usr/lib/libfido2.so.1 ] || { echo "libfido2 missing" >&2; exit 1; }

    cp ${pkgs.dejavu_fonts}/share/fonts/truetype/DejaVuSans.ttf \
      ${pkgs.dejavu_fonts}/share/fonts/truetype/DejaVuSansMono.ttf $root/usr/share/fonts/truetype/
    cp ${fonts} $root/usr/share/teleport/fonts.conf
    cp ${teleport}/share/teleport/teleport-pam.c ${teleport}/share/teleport/teleport.pam.arch $root/usr/share/teleport/

    substitute ${../packaging/linux/AppRun} $root/AppRun \
      --subst-var-by loader ${loader} \
      --subst-var-by triple ${triple} \
      --subst-var-by id "$(basename $out | cut -c1-12)"
    chmod 755 $root/AppRun
    install -Dm644 ${../packaging/icons/dev.teleport.Teleport.svg} \
      $root/usr/share/icons/hicolor/scalable/apps/dev.teleport.Teleport.svg
    install -Dm644 ${../packaging/icons/dev.teleport.Teleport.png} $root/dev.teleport.Teleport.png
    ln -s dev.teleport.Teleport.png $root/.DirIcon
    install -Dm644 ${../packaging/linux/dev.teleport.Teleport.desktop} $root/usr/share/applications/dev.teleport.Teleport.desktop
    ln -s usr/share/applications/dev.teleport.Teleport.desktop $root/dev.teleport.Teleport.desktop
    install -Dm644 ${../packaging/linux/dev.teleport.Open.desktop} $root/usr/share/applications/dev.teleport.Open.desktop
    install -Dm644 ${../packaging/linux/dev.teleport.Teleport.metainfo.xml} \
      $root/usr/share/metainfo/dev.teleport.Teleport.metainfo.xml
    for size in 16 24 32 48 64 128 256 512; do
      install -Dm644 ${../packaging/icons/hicolor}/''${size}x''${size}/dev.teleport.Teleport.png \
        $root/usr/share/icons/hicolor/''${size}x''${size}/apps/dev.teleport.Teleport.png
    done
    # The host's system setup (systemd user unit, udev rules, module loading,
    # sysctl, portal desktop entry), for installs from the tarball, the
    # AppImage or the Arch package (docs/distribution.md).
    mkdir -p $root/usr/share/teleport/system $root/usr/share/teleport/examples
    cp ${../packaging/linux/system}/* $root/usr/share/teleport/system/
    # Opt-in examples for unattended start (SDDM automatic login, KDE lock on
    # start; docs/wake-and-unattended.md).
    cp ${../packaging/linux/examples}/* $root/usr/share/teleport/examples/
    # The system-login PAM helper, linked against the system's glibc and
    # libpam (nix/pam-helper.nix): after the checks above, which require every
    # library to be bundled. The .deb, .rpm and Arch package install it
    # root-owned as /usr/lib/teleport/teleport-pam (docs/system-login.md).
    install -Dm755 ${pamHelper}/libexec/teleport-pam $root/usr/libexec/teleport/teleport-pam
    # The documentation that help text, messages and the system files cite;
    # the packages install it in /usr/share/doc/teleport.
    mkdir -p $root/usr/share/doc/teleport
    cp ${../README.md} $root/usr/share/doc/teleport/README.md
    cp ${../docs}/*.md $root/usr/share/doc/teleport/
    # Development notes and audits are not user documentation.
    rm -f $root/usr/share/doc/teleport/{todo,status,review-2026-10,parsec-comparison,ux-audit,remote-desktop-audit}.md
  ''
