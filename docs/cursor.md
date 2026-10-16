# Local cursor

With an X11 host (`teleport host --source x11`), the client draws the remote
pointer itself instead of showing the one encoded into the video. The local
window system moves it, so the pointer follows the mouse with no network,
encode or decode latency; only its *shape* comes from the host.

## How it works

- The host advertises `Desktop::local_cursor` (X11 sources only). The client
  opts in with `ClientFeatures { local_cursor: true }`. Neither side sends
  anything new to a peer that did not advertise or request it, so old hosts and
  old clients keep the cursor in the video.
- On opt-in the host starts a watcher thread with its own X connection
  (XFixes `SelectCursorInput(root, DisplayCursor)`), restarts the video with
  `ximagesrc show-pointer=false` (the same restart as a monitor switch, also
  for active auxiliary monitor streams), and sends the current shape.
- Each `CursorNotify` reads the image with `GetCursorImage`, converts the
  premultiplied ARGB to straight RGBA and, if its cursor serial is new, sends
  `Update::Cursor` on the reliable updates track. Shapes are latest-wins: a
  burst of changes sends only the newest. A fully transparent cursor (common
  in games) is sent as `cursor: None` (hidden). Cursors over 256x256 are not
  sent; the client keeps the previous shape.
- The client validates every shape (size 1-256, hot spot inside, exact pixel
  length, base64 bounded before decoding) and creates an SDL colour cursor
  (`SDL_CreateRGBSurfaceFrom` + `SDL_CreateColorCursor`). It is shown while
  the pointer is over remote video, including auxiliary monitor windows. The
  toolbar, letterbox bars and the time before the first frame use the normal
  arrow. A hidden host cursor hides the local pointer over video. Mouse lock
  (Ctrl+Alt+M) still hides the pointer independently. The cursor is freed when
  replaced and on session exit, and the arrow is restored.
- The watcher stops when the session ends. The next client starts with the
  pointer back in the video until it opts in.

`teleport client --embedded-cursor` keeps the old behaviour (pointer in the
video), e.g. to compare or if a host theme's cursor renders poorly.

## Limits and remaining gaps

- **Wayland hosts keep the cursor in the video.** The ScreenCast portal can
  deliver the cursor as PipeWire metadata (`CursorMode::Metadata`), but
  GStreamer's `pipewiresrc` does not expose `SPA_META_Cursor` to the pipeline.
  Supporting it needs our own PipeWire consumer (or an upstream change) that
  reads the cursor meta per buffer.
- The cursor is drawn at the host's pixel size. When the client scales the
  video (window smaller or larger than the remote monitor), the pointer does not
  scale with it.
- A cursor larger than 256x256 is not sent (the previous shape stays). No
  current cursor theme reaches that size.
- Animated cursors are sent frame by frame, since each frame has its own
  serial.
- The pointer position is the client's. Host-side warps (an app moving the
  pointer) are not shown until the local mouse moves. Games that need raw
  input should use mouse lock (Ctrl+Alt+M); automatic locking when the host
  hides its cursor is not implemented (review item R3).

## Verification

- `protocol::tests::cursor_updates_roundtrip_compactly_and_reject_bad_images`
  and `local_cursor_is_negotiated_by_both_ends` cover wire format, bounds and
  defaults; `x11_cursor::tests` covers premultiplied-to-straight conversion,
  hidden cursors and rejected images.
- `cargo test --test smoke -- --ignored x11_local_cursor_shapes` runs an X11 host
  on Xvfb with a headless client. It checks that an `--embedded-cursor` client
  receives no shapes, that an opted-in client gets the initial cursor, a window
  cursor change and a fully transparent (hidden) cursor.
  `x11_capture_input_and_native_window` also checks that the default headless
  client receives a shape.
- Not tested automatically: the SDL cursor on a real display (toolbar arrow,
  hide over video, auxiliary windows). Check this by hand on an X11 host.
