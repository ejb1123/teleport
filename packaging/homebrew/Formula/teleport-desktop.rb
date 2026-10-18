# Template: tools/homebrew-tap.sh fills in the version and checksum for a
# release and writes the result for the tap ejb1123/homebrew-teleport
# (docs/distribution.md, "Homebrew"). Linux only: on macOS, install the cask.
# Installs the relocatable bundle (nix/bundle.nix) as it is: its binaries run
# through the bundled glibc loader started by AppRun, so Homebrew's own glibc
# and ELF relocation are not involved.
class TeleportDesktop < Formula
  desc "Remote desktop host and client over Media over QUIC"
  homepage "https://github.com/ejb1123/teleport"
  url "https://github.com/ejb1123/teleport/releases/download/v@VERSION@/teleport-@VERSION@-x86_64-linux.tar.gz"
  sha256 "@SHA256_LINUX_X86_64@"
  license :cannot_represent

  livecheck do
    url :stable
    strategy :github_latest
  end

  depends_on arch: :x86_64
  depends_on :linux

  def install
    libexec.install Dir["*"]
    (bin/"teleport-desktop").write <<~SH
      #!/bin/sh
      exec "#{libexec}/AppRun" "$@"
    SH
    share.install_symlink libexec/"usr/share/applications", libexec/"usr/share/icons"
    pkgshare.install_symlink libexec/"usr/share/teleport/system"
  end

  def caveats
    <<~EOS
      The command is `teleport-desktop` (homebrew-core's `teleport` is
      Gravitational's). The client and launcher need nothing else.

      Running the host needs system setup Homebrew does not do: the files in
        #{opt_pkgshare}/system
      (systemd user unit, udev rules for virtual gamepads, UDP buffer sysctl).
      The .deb, .rpm and Arch packages install them; see
        https://github.com/ejb1123/teleport/blob/main/docs/distribution.md
    EOS
  end

  test do
    assert_match version.to_s, shell_output("#{bin}/teleport-desktop --version")
    system bin/"teleport-desktop", "doctor"
  end
end
