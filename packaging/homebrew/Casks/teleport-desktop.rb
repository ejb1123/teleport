# Template: tools/homebrew-tap.sh fills in the version and checksums for a
# release and writes the result for the tap ejb1123/homebrew-teleport
# (docs/distribution.md, "Homebrew"). Not "teleport": homebrew-core's
# `teleport` is Gravitational's, which also installs a `teleport` command.
cask "teleport-desktop" do
  arch arm: "arm64", intel: "x86_64"

  version "@VERSION@"
  sha256 arm:   "@SHA256_ARM64@",
         intel: "@SHA256_X86_64@"

  url "https://github.com/ejb1123/teleport/releases/download/v#{version}/Teleport-#{version}-#{arch}.dmg"
  name "Teleport"
  desc "Remote desktop client for Linux hosts over Media over QUIC"
  homepage "https://github.com/ejb1123/teleport"

  livecheck do
    url :url
    strategy :github_latest
  end

  depends_on macos: ">= :big_sur"

  app "Teleport.app"
  binary "#{appdir}/Teleport.app/Contents/MacOS/teleport", target: "teleport-desktop"

  zap trash: [
    "~/Library/Application Support/Teleport",
    "~/Library/Caches/Teleport",
    "~/Library/Preferences/dev.teleport.Teleport.plist",
    "~/Library/Saved Application State/dev.teleport.Teleport.savedState",
  ]

  caveats <<~EOS
    Unless this release was notarised, Teleport.app is only ad-hoc signed and
    macOS refuses to open it. Right-click Teleport in Applications and choose
    Open, or run:
      xattr -dr com.apple.quarantine #{appdir}/Teleport.app

    The command-line client is linked as `teleport-desktop`.
  EOS
end
