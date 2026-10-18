#!/usr/bin/env bash
# Lint the macOS (aarch64-apple-darwin) build from Linux, without a Mac or the
# macOS SDK, with the same command CI runs on macos-14:
# `cargo clippy --locked --all-targets -- -D warnings`. Catches
# name-resolution and type errors in Rust code that only compiles on macOS,
# and code that only the Linux host uses (dead on macOS). It does not link,
# and C code is not compiled (the C compiler and archiver are stand-ins that
# produce empty outputs).
#
#   tools/check-macos.sh            # from the repository root (all targets,
#                                   # including unit tests, which nix build runs)
#   tools/check-macos.sh -p NAME    # extra arguments go to cargo, before `--`
#
# Builds std for the target from the flake-pinned Rust sources (-Zbuild-std),
# because prebuilt rust-std does not match Nix's rustc build.
set -euo pipefail
cd "$(dirname "$0")/.."
work=${TELEPORT_MACOS_CHECK_DIR:-${XDG_CACHE_HOME:-$HOME/.cache}/teleport-macos-check}
mkdir -p "$work"

cat >"$work/cc" <<'EOF'
#!/bin/sh
out=""; prev=""
for a in "$@"; do
  case "$prev" in -o) out="$a" ;; esac
  case "$a" in -o?*) out="${a#-o}" ;; esac
  prev="$a"
done
[ -n "$out" ] && : >"$out"
exit 0
EOF
cat >"$work/ar" <<'EOF'
#!/bin/sh
for a in "$@"; do case "$a" in *.a) printf '!<arch>\n' >"$a"; exit 0 ;; esac; done
exit 0
EOF
chmod +x "$work/cc" "$work/ar"

library=$(nix build --no-link --print-out-paths --inputs-from . nixpkgs#rustPlatform.rustLibSrc)
# sccache rejects the stand-in C compiler, so build without it.
unset RUSTC_WRAPPER
RUSTC_BOOTSTRAP=1 \
  __CARGO_TESTS_ONLY_SRC_ROOT="$library" \
  CC_aarch64_apple_darwin="$work/cc" CXX_aarch64_apple_darwin="$work/cc" \
  AR_aarch64_apple_darwin="$work/ar" AWS_LC_SYS_CMAKE_BUILDER=0 \
  PKG_CONFIG_ALLOW_CROSS=1 CARGO_INCREMENTAL=0 \
  nix develop --command nice -n 19 ionice -c3 cargo clippy \
  -Zbuild-std=std,panic_abort --locked -j 2 \
  --target aarch64-apple-darwin --target-dir "$work/target" --all-targets "$@" \
  -- -D warnings
