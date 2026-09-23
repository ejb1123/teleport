# Fullscreen monitor mapping

Update **both host and client** for simultaneous independent monitors. An older
host can still use single-display fullscreen; its stream is not silently mirrored
or stretched to impersonate multiple remote monitors. Only monitors already
authorized in the host's portal/session are available.

Click **Full** to fill the current local display and, where available, open
additional fullscreen windows for other remote monitors. Each window uses that
display's native desktop mode; no resolution/refresh-rate changes or exclusive
display capture are performed. The initial mapping assigns distinct remote
monitors to local displays, up to four additional windows. Extra local displays
without a corresponding remote monitor are initially left alone.

- **Display N** on the main toolbar changes the main window's remote monitor.
- **Local N / Remote N - Next** on an additional window cycles its remote monitor.
  Duplicate remote mappings are allowed; shared streams remain active until their
  last additional window closes. Mappings currently last for this session only.
- **Use one display** closes additional windows but keeps the main fullscreen
  window. **Use all displays** restores the default multi-display mapping.
- **Window** on the main toolbar exits fullscreen and restores its window size
  and placement. **Ctrl+Alt+Q** disconnects from any session window.
- The toolbar overlays video, hides after 1.2 seconds away from it, and reappears
  at the top four pixels. Revealing it does not resize video or shift input
  coordinates. A small top-edge marker identifies the reveal area.

On macOS, SDL fullscreen Spaces are disabled before window creation so sibling
fullscreen windows can remain visible together. This is borderless per-display
fullscreen, not a separate macOS Space per window. Physical multi-display macOS
verification is still required. Compositors may constrain requested window
placement; test the actual Linux display backend in use.

## Transport, resources and safety

All windows use one authenticated QUIC session. The main stream and each active
additional remote monitor have independent bounded decoder queues and generation
barriers. Pointer events name the remote monitor; keyboard and button state is
shared and released on focus changes, remapping, and disconnect. Auxiliary errors
do not terminate the main stream. Local display-change events close additional
windows; use **Use all displays** after the new layout settles. Remote monitor
hotplug/re-enumeration is not implemented.

Additional streams are created on demand. Each uses the requested resolution,
FPS, codec, dynamic range, and **per-stream** bitrate. Auxiliary encoders currently
use fixed targets; primary adaptation remains enabled unless the host disables
it. Several 20 Mbps streams may therefore require substantially more than
20 Mbps total bandwidth and multiple hardware encoder/decoder sessions. Audio
plays only once. Linux NV12 and opt-in macOS GPU-surface presentation are preserved.

## Verification

Automated Linux tests cover fullscreen sizing, toolbar hide/reveal, window restore,
coordinate mapping, simultaneous authenticated streams, remapping helpers,
generation barriers, and close/reopen ordering. They do not prove physical monitor
placement, macOS Spaces behavior, HDR headroom, or GPU performance on every device.

On two real monitors, test both display orders, negative/vertical layouts,
different DPI and refresh rates, independent remote selection, duplicate mappings,
click/drag/scroll at all corners, focus changes while holding keys, toolbar clicks,
window restore, rapid one/all toggles, unplug/replug, and disconnect. Verify input
never goes to the wrong remote monitor and no keys remain held.

## Understanding the bitrate readout

The host CLI default is 8,000 kbit/s; launcher connections default to requesting
20,000 kbit/s and offer 8/20/40 Mbps in Connection settings. An explicit client
`--bitrate 40000` requests 40 Mbps. There is no separate Intel/Arch 8 Mbps cap in
the application. Host acknowledgements and encoder-property readback are tested
at 20 and 40 Mbps for H.264/H.265 software encoders; this is not an acceptance test
of every hardware encoder/driver.

F8 distinguishes the client request, host-accepted per-stream ceiling, current
encoder target, and measured **main-stream encoded payload**. The last is neither
aggregate multi-monitor traffic nor network capacity. Adaptation may lower the
target after backlog/drops; clean samples recover toward the accepted ceiling.
Static/simple video may use much less than its target. Raising a target does not
force padding traffic or guarantee image-quality improvement. If a session seems
stuck at 8 Mbps, compare these four values before blaming network negotiation.
