# UX audit (2026-10-17, tracker R25)

This audit covers the launcher, the session window and input feel. The
reference setup is a MacBook (Retina, 2560x1440 logical, 120 Hz) connected to
a KDE host. Items are grouped into three lists, in priority order:

- done in this change;
- should do next;
- later.

Scrolling has its own page: docs/scrolling.md.

## "Double overlapping text"

The cause could not be reproduced here (no Mac). This is what the code
review found.

**Client overlays (Mac, default SDL path).** No overlay text is drawn twice.

- Each label is rasterised at the drawable's pixel density and copied 1:1
  (`session_text`), and each panel is drawn once.
- The Metal presenter that composites read-back overlays over the video
  (`hdr_present.rs`) runs only for HDR or `TELEPORT_MAC_GPU_VIDEO=1`, not in
  the reported session.
- The overlays did *overlap*, though:
  - the guest/notice panel sat exactly on top of the F8 panel (both top
    right);
  - the warnings panel collided with F8 in windows narrower than about
    1140 px;
  - F8 was taller than a 720p window.

  All three are fixed below.

**Host zero-copy capture (the reported session).** When only the pointer
moves, KWin sends "cursor-only" PipeWire buffers, and `pw_cursor.rs` repeats
the last good frame instead.

- The repeat shared the DMA-BUF but not the PipeWire buffer that owns it. As
  soon as the next real frame replaced it, that buffer went back to KWin.
- KWin could then draw a newer frame into the memory NVENC was still reading
  for the repeat.
- The result is a frame mixing two images: ghosted, doubled text, mostly
  while the pointer moves over changing content.
- The repeat now holds its parent buffer (`GstParentBufferMeta`) until it is
  encoded.

There is one more, unconfirmed suspect: nothing waits on the compositor's
DMA-BUF fence before NVENC reads the buffer (`dmabuf_encode.c`). It is only
safe if KWin finishes rendering before it queues the buffer.

**Diagnostics added:**

- F8 page 2 shows "Window" (points), "Drawable" and renderer output pixels with
  the scale, plus the overlay layers drawn in the last frame.
- Ctrl+Alt+S saves two snapshots as BMP files in Downloads, or the temporary
  directory:
  - what the session window drew;
  - the next decoded video frame, unscaled. On the zero-copy GPU path the
    frame is not on the CPU, so only the window is saved.

  Doubled text in the video file means the host or encoder is at fault.
  Doubled text only in the window file means client presentation is at
  fault.

**What the user should send:**

1. A Mac screenshot (Cmd+Shift+4) of the doubled text, with F8 on page 2 (press F8 twice).
2. The two Ctrl+Alt+S files.
3. Whether the effect stays with `TELEPORT_CAPTURE_ZERO_COPY=0` on the host.
4. Whether it shows while the pointer is still.

## Done here

1. **Scrolling** (docs/scrolling.md):
   - smooth pixel scrolling with macOS momentum and gesture phases;
   - no double inversion of natural scrolling;
   - `--scroll-direction` and `--scroll-speed`;
   - saner trackpad speed on X11 hosts;
   - wheel in pointer lock no longer sends an absolute position.
2. **Torn repeated frames on zero-copy hosts** (above).
3. **F8 split into three pages.** F8 or Stats cycles through summary (with
   the graph), session, pipeline and hidden. Each page is cut to the window
   and says when lines are cut. Every page fits a 720p window.
4. **Overlay placement:**
   - the guest/notice panel no longer covers F8 (F8 moves below it);
   - warnings move to the bottom left when they would hit F8.
5. **Notices in fullscreen.** With the toolbar hidden, notices such as "Mouse
   locked … Ctrl+Alt+M releases" and "Host text copied" appeared nowhere.
   They now show as a toast at the top centre for five seconds.
6. **Window title.** "<desktop name> - Teleport - Remote Desktop" instead
   of the same title for every session, so Mission Control and Cmd+Tab tell
   sessions apart. The launcher passes the saved name.
7. **Wayland HiDPI clients.** sdl2-compat draws SDL2 programs at 1x on
   Wayland and lets the compositor upscale them. That made text blurry and
   requested a half-size stream. Teleport now asks for full density
   (checked at scale 2 in a nested KWin).
8. **Launcher settings grid.**
   - 20 buttons in a grid made for 18. The last two (Ctrl/Cmd swap, Keep
     local) were drawn under the status panel but still toggled by clicks
     on it.
   - Now four tabs (Display, Input, Audio & sharing, Advanced), at most six
     buttons each.
   - Added scroll direction and speed.

## Should do

1. **Persist launcher settings per desktop.** Every toggle resets on each
   launch.
2. **Default Ctrl/Cmd swap on macOS.** Without it, Cmd+C reaches KDE as
   Super+C.
3. **Keep the actual failure reason.** A failed connection always lands on
   the sign-in page with "Connection failed. Check the network, or sign in
   again", even for timeouts.
   - Show the client's last error line.
   - Stay on Home unless authentication failed.
   - Map common errors (timeout, refused, fingerprint changed, bad code) to
     what to do next.
4. **Reconnect in place.** Each reconnect destroys the session window: the
   connecting window appears, then a new centred 1280x720 window. Fullscreen
   and position are lost on every Wi-Fi blip. Keep the window and draw
   "Reconnecting… (attempt n)" over the last frame.
5. **Connecting window.**
   - Errors are cut at 85 characters.
   - It lacks `allow_highdpi` (blurry on Retina).
   - Enter should mean Retry.
6. **Launcher HiDPI.** The software renderer with
   `set_logical_size(1040, 832)` and fixed 16/30/13 px fonts is scaled 2x on
   Retina, so it is blurry. Rasterise text at the output scale, as
   `session_text` does.
7. **Launcher keyboard navigation.**
   - Tab only moves between text fields.
   - Buttons have no focus ring.
   - Escape never closes the window.
   - On the Code sign-in page, Tab focuses a hidden username field.
8. **Mac wording.** Hints say Ctrl+Alt+…; macOS users look for
   Ctrl+Option+…
9. **First-run hint.** Say where the address and pairing code come from (the
   host settings window or `teleport host`, port 4443).
10. **Cmd+Q in a session.** It quits at once, without confirmation, also
    while keys are bound or swapped.
11. **Crash when a zero-copy session ends.** A CUDA zero-copy session on a
    Wayland client crashes as it ends: `cuGraphicsUnregisterResource` in
    `GlPresenter::drop`, seen in the nested-KWin runs here. Release the CUDA
    registration while its GL context is still current.
12. **DMA-BUF fences.** Wait on the compositor's DMA-BUF fence before NVENC
    reads a captured buffer (`dmabuf_encode.c`). See the second suspect
    above.

## Later

1. **Text input.** Scancodes only, so the host's layout decides characters.
   IME, dead keys and Mac ISO keyboards (§/` and <>) need a "type as text"
   path (portal keysyms).
2. **Toolbar labels.** "Send", "Get", "Retry" and "Full" rely on hover hints,
   which trackpad users rarely see. Use longer labels where the width allows.
3. **Launcher desktop list.** It shows seven desktops, scrolls only with the
   wheel, and has no scrollbar or keyboard selection.
4. **Mouse.** Show a first-use hint for "lock by hotkey", and a persistent
   small "pointer locked" badge in fullscreen.
5. **Key repeat.** It is host-side (KWin repeats held portal keys). That is
   correct, but a network stall while a key is held repeats until the
   release or the input timeout.
6. **Settings buttons.** They cycle with no way back and give no sign that
   they cycle. Use "Name: value" labels and step backwards with right click
   or Shift.
7. **F8 columns.** F8 pads its labels with spaces for a monospace font, but
   the UI font is proportional, so values do not line up. Draw labels and
   values as two columns.
