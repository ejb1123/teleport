{ pkgs }:
# The system-login PAM helper (packaging/pam/teleport-pam.c) for the .deb,
# .rpm and Arch packages: compiled here, but linked to run on the
# distribution's own glibc and libpam. The interpreter is the system's
# standard dynamic loader and there is no RUNPATH, so at run time it loads
# the system's libc.so.6 and libpam.so.0, which read /etc/pam.d/teleport and
# load the system's PAM modules (never Nix's; system_pam.rs). Only
# libpam.so.0 (LIBPAM_1.0, the ABI every Linux-PAM since 2002 provides) and
# libc.so.6 are linked, and the build fails if any glibc symbol version is
# newer than 2.34, so it runs on every distribution the release tests
# (Ubuntu 22.04 has 2.35). tools/package-smoke.sh runs it on each of them.
let
  systemLoader =
    {
      x86_64 = "/lib64/ld-linux-x86-64.so.2";
      aarch64 = "/lib/ld-linux-aarch64.so.1";
    }
    .${pkgs.stdenv.hostPlatform.parsed.cpu.name};
  maxGlibc = "2.34";
in
pkgs.runCommand "teleport-pam-system"
  {
    nativeBuildInputs = [
      pkgs.stdenv.cc
      pkgs.patchelf
      pkgs.binutils
    ];
    buildInputs = [ pkgs.pam ];
    # No RUNPATH: neither the linker wrapper's (its variable is suffixed
    # with the target) nor stdenv's own $out/lib.
    "NIX_DONT_SET_RPATH_${pkgs.stdenv.cc.suffixSalt}" = "1";
    NIX_NO_SELF_RPATH = "1";
    passthru = { inherit systemLoader; };
  }
  ''
    # No Nix RUNPATH and the system's loader, set when linking so that no
    # store path is left in the binary.
    $CC -O2 -Wall -Wextra -Werror -std=gnu17 \
      -Wl,--dynamic-linker=${systemLoader} \
      ${../packaging/pam/teleport-pam.c} -o teleport-pam -lpam
    [ "$(patchelf --print-interpreter teleport-pam)" = ${systemLoader} ] ||
      { echo "interpreter is not ${systemLoader}" >&2; exit 1; }

    needed=$(patchelf --print-needed teleport-pam | sort | tr '\n' ' ')
    [ "$needed" = "libc.so.6 libpam.so.0 " ] || { echo "unexpected libraries: $needed" >&2; exit 1; }
    rpath=$(patchelf --print-rpath teleport-pam)
    [ -z "$rpath" ] || { echo "RUNPATH left: $rpath" >&2; exit 1; }
    # Every versioned glibc symbol must exist in the oldest supported glibc.
    newest=$(objdump -T teleport-pam | grep -o 'GLIBC_[0-9.]*' | sed 's/GLIBC_//' | sort -V | tail -n 1)
    [ "$(printf '%s\n%s\n' "$newest" ${maxGlibc} | sort -V | tail -n 1)" = ${maxGlibc} ] ||
      { echo "needs glibc $newest, newer than ${maxGlibc}" >&2; objdump -T teleport-pam >&2; exit 1; }
    # PAM symbols only from the stable LIBPAM_1.0 version (or unversioned).
    if objdump -T teleport-pam | grep -o 'LIBPAM_[A-Z_0-9.]*' | grep -qv '^LIBPAM_1.0$'; then
      echo "needs a LIBPAM version newer than 1.0" >&2; exit 1
    fi
    if strings teleport-pam | grep /nix/store >&2; then
      echo "references /nix/store" >&2; exit 1
    fi
    install -Dm755 teleport-pam $out/libexec/teleport-pam
  ''
