#!/usr/bin/env bash
# Release notes for TAG on stdout: the "## VERSION" section of CHANGELOG.md
# when there is one, otherwise the commit subjects since the previous v* tag
# (merges left out), then the install pointers.
#
#   tools/release-notes.sh v0.6.0 > notes.md
set -euo pipefail
tag=${1:?usage: $0 TAG}
version=${tag#v}
cd "$(dirname "$0")/.."

section=
if [ -f CHANGELOG.md ]; then
  # From "## 0.6.0" or "## [0.6.0]" (anything may follow) to the next "## ".
  section=$(awk -v v="$version" '
    /^## / { if (found) exit; h = $2; gsub(/[][]/, "", h); if (h == v) { found = 1; next } }
    found { print }' CHANGELOG.md)
fi
if [ -n "$(printf '%s' "$section" | tr -d '[:space:]')" ]; then
  printf '%s\n' "$section"
else
  previous=$(git describe --tags --abbrev=0 --match 'v*' "$tag^" 2>/dev/null || true)
  if [ -n "$previous" ]; then
    echo "Changes since $previous:"
    range=$previous..$tag
  else
    echo "Changes:"
    range=$tag
  fi
  echo
  # A first release would list the whole history: the newest 300 only.
  git log --no-merges --max-count=300 --format='- %s (%h)' "$range"
fi
cat <<EOF

## Install

Installation for every distribution, and what each package contains:
[docs/distribution.md](https://github.com/ejb1123/teleport/blob/$tag/docs/distribution.md).
Verify downloads against \`SHA256SUMS\`.

| System | Download |
|---|---|
| Debian, Ubuntu | \`teleport_${version}-1_amd64.deb\` |
| Fedora, openSUSE | \`teleport-${version}-1.x86_64.rpm\` |
| Arch, CachyOS, Manjaro, EndeavourOS | \`teleport-desktop-bin-${version//-/_}-1-x86_64.pkg.tar.zst\` (or the AUR) |
| Any Linux | \`Teleport-${version}-x86_64.AppImage\`, \`teleport-${version}-x86_64-linux.tar.gz\` |
| Linux client, sandboxed | \`Teleport-${version}-x86_64.flatpak\` |
| macOS (Apple silicon / Intel) | \`Teleport-${version}-arm64.dmg\` / \`Teleport-${version}-x86_64.dmg\` (or Homebrew) |
EOF
