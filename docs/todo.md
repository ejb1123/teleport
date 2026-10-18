# To do later

Open work as of 2026-10-17. Parsec gap numbers (G…) refer to
[parsec-comparison.md](parsec-comparison.md); tracker rows (R…) to
[review-2026-10.md](review-2026-10.md). Usability items are listed in full in
[ux-audit.md](ux-audit.md).

## Decisions

- **License.** None is chosen yet, so the public code is all rights reserved.
  MIT OR Apache-2.0 matches the Rust ecosystem and the MIT dependencies
  (moq, libmatoya). Add a LICENSE file and `license` in Cargo.toml.
- **Name.** It stays "Teleport". Gravitational, Inc. holds registered TELEPORT
  marks for remote-access software (USPTO 87405440, Class 9; 97503247,
  Class 42). Screened alternatives, should that change: Tachylink, Lanless,
  Couchlink, Lowlag. A rename should keep wire and crypto labels so existing
  pairings keep working.

## Needs real hardware or a person

- **macOS client (G2).**
  - Check the scrolling feel: work through the checklist in
    [scrolling.md](scrolling.md) (momentum, natural direction, wheel against
    trackpad).
  - Measure latency on the Metal path.
  - Check that `nix build` passes on a Mac, since the unit tests run there.
  - If the "double overlapping text" comes back, press Ctrl+Alt+S and look
    at both snapshots in ~/Downloads.
  - Find out whether hosts with `TELEPORT_CAPTURE_ZERO_COPY=0` show it.
- **Untested on real hardware:**
  - virtual displays and privacy mode on physical monitors;
  - DualShock 4 and DualSense on physical controllers and real games;
  - guests in real multi-user sessions;
  - unattended start on a real reboot;
  - Wake-on-LAN from real suspend;
  - hardware AV1 encode, which needs an RTX 40 or Intel Arc;
  - AMD and Quick Sync paths;
  - GNOME and wlroots hosts.
- **Glass-to-glass latency (G3).** Use the photodiode or camera method in
  [performance.md](performance.md) with `--source test --flash-on-input`.

## Unfinished features

- **Browser client (G26).** Planned as WebTransport + WebCodecs, reusing
  Teleport's Rust e2e/pairing code compiled to WebAssembly.
  - Certificate pinning has to work through WebTransport's
    `serverCertificateHashes`: a short-lived certificate vouched for by the
    pinned identity.
  - A first attempt was stopped early. It is on branch
    `worktree-agent-a8414446f5ec7151d`, with little done.
  - Native Android, iOS and tvOS apps are out of scope.
- **Windows (G18, G19).** Dropped for now; the README says Windows is not
  supported.
  - The client is on branch `worktree-agent-a1fef2a1193c2643d` (`9516eed`).
    It cross-compiles with MinGW and passes sessions under Wine. It still
    needs a merge with current main: the conflict is in `src/ffmpeg_media.rs`
    (its `open_decoder` split against G8's `output_format` and labels).
  - The host was never started. A Windows 11 VM run by the perpendicular
    project could test it; see the notes in that project's docs.
- **Release workflow.** `.github/workflows/release.yml` is on branch
  `worktree-agent-a47790ac5bc828443` (`8cf999c`), never merged or run.
  Pushing it needs SSH or a token with the `workflow` scope, plus Apple
  signing and notarisation secrets for the macOS job.
- **Relay on the NixOS module.** The production host has no relay options
  set. Add the relay URL and key (self-hosted or Cloudflare) to enable
  presence, peer-to-peer signalling and remote reach.
- **Cloudflare provisioned relays.** Only the public draft-14 relay was
  tested live. Verify a provisioned draft-16 relay with publish tokens, and
  send the moq-net patches upstream. The branch `teleport/moq-net-0.2.20` on
  `ejb1123/moq` can be opened as a PR against `moq-dev/moq`.

## Known bugs and limits

- **Zero-copy CUDA crash.** The presenter segfaults on exit in
  `cuGraphicsUnregisterResource` (`GlPresenter::drop`) on Wayland clients.
- **No DMA-BUF fences.** Zero-copy capture reads frames without waiting on
  KWin's DMA-BUF fences, which could tear.
- **Ping spends a ticket.** A `ping` host command sent with a fresh
  system-login ticket uses that ticket up.
- **Cloudflare request budget.** About 50 requests per connection; a session
  costs the host about 7, so busy hosts reconnect.
- **Flaky tests under load.** `relay::auto_route…`,
  `network::full_hd_clean_link_decode_path` and
  `guests::invites_are_limited_to_their_uses_and_lifetime` fail at load
  above about 10. Make them load-tolerant.
- **Intel GPUs.** VA-API text quality on the UHD 630 stays near 17.5 dB on
  dense text, at any preset and bitrate.
- **Audio.** Uncompressed PCM has no loss concealment; the microphone is mono
  only.

## Usability (from ux-audit.md)

1. Persist launcher settings per desktop.
2. Swap Ctrl and Cmd by default on macOS.
3. Show the real connection error instead of the sign-in page.
4. Reconnect in place, keeping fullscreen and the window position.
5. Fix the connecting window: HiDPI, wrapped errors, Enter to retry.
6. Fix blurry launcher text on Retina.
7. Add launcher keyboard navigation and focus.
8. Say "Ctrl+Option" on macOS.
9. Add a first-run hint.
10. Confirm before Cmd+Q.
11. Later: IME and dead keys, a scrollable desktop list, mouse-lock hints,
    and aligned F8 columns.
