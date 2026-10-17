# Local cursor

The client draws the remote pointer itself instead of showing the one encoded
into the video. The local window system moves it, so the pointer follows the
mouse with no network, encode or decode latency; only its *shape* comes from
the host. This works with X11 hosts (`teleport host --source x11`) and with
Wayland hosts whose ScreenCast portal offers cursor metadata (KDE Plasma 6;
GNOME's portal offers it too, untested).

## How it works

- The host advertises `Desktop::local_cursor`. The client opts in with
  `ClientFeatures { local_cursor: true }`. Neither side sends anything new to a
  peer that did not advertise or request it, so old hosts and old clients keep
  the cursor in the video.
- Shapes go to the client as `Update::Cursor` on the reliable updates track:
  straight RGBA, at most 256x256, latest wins. A fully transparent or empty
  cursor (common in games) is sent as `cursor: None` (hidden). Cursors over
  256x256 are not sent; the client keeps the previous shape.
- The client validates every shape (size 1-256, hot spot inside, exact pixel
  length, base64 bounded before decoding) and creates an SDL colour cursor
  (`SDL_CreateRGBSurfaceFrom` + `SDL_CreateColorCursor`). It is shown while
  the pointer is over remote video, including auxiliary monitor windows. The
  toolbar, letterbox bars and the time before the first frame use the normal
  arrow. A hidden host cursor hides the local pointer over video. Mouse lock
  (Ctrl+Alt+M) still hides the pointer independently. The cursor is freed when
  replaced and on session exit, and the arrow is restored.

### X11 hosts

- On opt-in the host starts a watcher thread with its own X connection
  (XFixes `SelectCursorInput(root, DisplayCursor)`), restarts the video with
  `ximagesrc show-pointer=false` (the same restart as a monitor switch, also
  for active auxiliary monitor streams), and sends the current shape.
- Each `CursorNotify` reads the image with `GetCursorImage`, converts the
  premultiplied ARGB to straight RGBA and, if its cursor serial is new, sends
  it. The watcher stops when the session ends.

### Wayland (desktop portal) hosts

- The cursor mode belongs to the portal's ScreenCast session. The host opens
  it with `CursorMode::Metadata` when the portal offers it (`AvailableCursorModes`):
  the compositor then leaves the pointer out of the frames and attaches
  `SPA_META_Cursor` (position, hot spot and, when the shape changed, a
  premultiplied RGBA bitmap) to the same PipeWire buffers, DMA-BUFs included.
  `teleport host --embedded-cursor` keeps the pointer in the video instead.
- A client that does not opt in (an old client, `--embedded-cursor`, or no
  `ClientFeatures` within 3 s) gets the pointer back in the video: the host
  recreates the portal session in embedded mode and restarts every capture
  pipeline, as for a lost session. The next opted-in client switches it back.
  Recreating the session uses the saved restore token or the compositor's
  pre-authorization; without either, the compositor asks locally again.
- GStreamer's `pipewiresrc` (PipeWire 1.x) negotiates the cursor meta but only
  exposes the position. `src/pw_cursor.rs` adds a pad probe on every
  `pipewiresrc` that reads the meta from the PipeWire buffer behind each
  GStreamer buffer (pipewiresrc's pool data, validated against the buffer and
  its pool type before use), parses it with bounds checks and publishes shapes
  and positions to a process-wide hub. Only a changed bitmap is copied.
- KWin sends a cursor-only update as a buffer whose pixels are marked
  corrupted (stale). The probe drops those, and at most every 250 ms repeats
  the last good frame in their place (same memory, new timestamp), so moving
  the pointer over a static desktop costs no encoding but keyframe requests
  still find a frame.
- The client also declares `ClientFeatures::cursor_positions`; the host then
  sends `Update::CursorPosition { monitor, x, y }` (normalised, latest wins, at
  most ~30/s). The client uses it to place its pointer when relative mode ends.

`teleport client --embedded-cursor` keeps the old behaviour (pointer in the
video), e.g. to compare or if a host theme's cursor renders poorly.

## Limits and remaining gaps

- **The first shape on Wayland with DMA-BUF capture.** KWin sends a bitmap
  only when the shape changes, and the first one (sent when the stream starts)
  is lost when the DMA-BUF buffers are renegotiated. Until the shape first
  changes (hovering text or a link, a window edge), the client shows its own
  arrow. The host replays the last visible shape it saw to later sessions.
  CPU (shared-memory) capture gets the first bitmap.
- The cursor is drawn at the host's pixel size. When the client scales the
  video (window smaller or larger than the remote monitor), the pointer does not
  scale with it.
- A cursor larger than 256x256 is not sent (the previous shape stays). No
  current cursor theme reaches that size; KWin caps its bitmaps at 256x256.
- Animated cursors are sent frame by frame (X11: each frame has its own
  serial; KWin: each frame invalidates the bitmap).
- The pointer position is the client's. Host-side warps (an app moving the
  pointer) are not shown until the local mouse moves.
- The pipewiresrc probe depends on its private pool data layout (stable since
  PipeWire 0.3, checked at runtime). If the check fails, no shapes arrive and
  the client keeps its arrow; `--embedded-cursor` on either end avoids it.

## Verification

- `protocol::tests::cursor_updates_roundtrip_compactly_and_reject_bad_images`
  and `local_cursor_is_negotiated_by_both_ends` cover wire format, bounds and
  defaults; `x11_cursor::tests` covers premultiplied-to-straight conversion,
  hidden cursors and rejected images; `pw_cursor::tests` covers SPA cursor
  meta parsing (strides, formats, hidden and malformed bitmaps) and the
  stale-buffer filter.
- `cargo test --test smoke -- --ignored x11_local_cursor_shapes` runs an X11 host
  on Xvfb with a headless client. It checks that an `--embedded-cursor` client
  receives no shapes, that an opted-in client gets the initial cursor, a window
  cursor change and a fully transparent (hidden) cursor.
  `x11_capture_input_and_native_window` also checks that the default headless
  client receives a shape.
- `TELEPORT_TEST_PORTAL=1 cargo test --bin teleport portal_cursor_metadata --
  --ignored --nocapture` opens its own portal session in metadata mode and
  captures each monitor through the real encoder pipeline. On KDE Plasma
  6.7.5 (RTX 3080, 2×2560x1440) the meta was on every buffer with DMA-BUF
  zero-copy capture and NVENC, the position was reported for the monitor with
  the pointer; the 32x32 Breeze arrow arrived with CPU capture
  (`TELEPORT_CAPTURE_ZERO_COPY=0`) but not with DMA-BUFs (see limits).
- By hand against the same KDE desktop (a `--source portal` host on a spare
  port, pre-authorized portal, headless clients): an opted-in client kept the
  metadata session; an `--embedded-cursor` client switched the host to an
  embedded session and the next opted-in client back, each switch about
  0.4 s (portal session plus NVENC restart), with no dialog.
- Not tested: the SDL cursor on a real display (toolbar arrow, hide over
  video, auxiliary windows) and a shape change on Wayland (it needs the local
  pointer to move). Check these by hand.
