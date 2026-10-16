//! X11 cursor shapes for clients that draw the pointer locally.
//!
//! A watcher thread owns its own X connection, subscribes to XFixes cursor
//! changes on the root window and publishes each new shape (latest wins) to
//! the session loop, which forwards it as `Update::Cursor`.

use crate::protocol::{CursorImage, MAX_CURSOR_SIZE};
use anyhow::{Context, Result, ensure};
use std::{
    os::fd::AsRawFd,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tokio::sync::watch;
use x11rb::{
    connection::{Connection, RequestConnection},
    protocol::{
        Event as XEvent,
        xfixes::{self, ConnectionExt as _, CursorNotifyMask},
    },
};

/// How often the thread checks for shutdown while the cursor is idle.
const STOP_POLL: Duration = Duration::from_millis(200);

/// `None` inside `Some` = hidden cursor. The outer `None` is the initial
/// placeholder before the first shape was read.
pub type Shape = Option<Option<CursorImage>>;

/// Stops and joins the watcher thread when dropped (session end).
pub struct Watcher {
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
    pub shapes: watch::Receiver<Shape>,
}

impl Drop for Watcher {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Connects to `$DISPLAY`, reads the current cursor and starts watching. The
/// initial shape is already in `shapes` when this returns.
pub fn watch() -> Result<Watcher> {
    let (connection, screen) = x11rb::connect(None).context("connecting to X for cursor shapes")?;
    ensure!(
        connection
            .extension_information(xfixes::X11_EXTENSION_NAME)?
            .is_some(),
        "X server lacks the XFixes extension"
    );
    // XFixes requires the version handshake before any other request.
    let version = connection.xfixes_query_version(4, 0)?.reply()?;
    ensure!(version.major_version >= 2, "XFixes 2 or newer required");
    let root = connection.setup().roots[screen].root;
    connection
        .xfixes_select_cursor_input(root, CursorNotifyMask::DISPLAY_CURSOR)?
        .check()?;
    let mut last_serial = None;
    let (sender, shapes) = watch::channel(read_shape(&connection, &mut last_serial)?);
    let stop = Arc::new(AtomicBool::new(false));
    let thread_stop = stop.clone();
    let thread = std::thread::Builder::new()
        .name("x11-cursor".into())
        .spawn(move || {
            if let Err(error) = run(&connection, &sender, &thread_stop, last_serial) {
                tracing::warn!(error = format!("{error:#}"), "X11 cursor watcher stopped");
            }
        })?;
    Ok(Watcher {
        stop,
        thread: Some(thread),
        shapes,
    })
}

fn run(
    connection: &x11rb::rust_connection::RustConnection,
    sender: &watch::Sender<Shape>,
    stop: &AtomicBool,
    mut last_serial: Option<u32>,
) -> Result<()> {
    let fd = connection.stream().as_raw_fd();
    while !stop.load(Ordering::Relaxed) && !sender.is_closed() {
        let mut changed = false;
        while let Some(event) = connection.poll_for_event()? {
            changed |= matches!(event, XEvent::XfixesCursorNotify(_));
        }
        if changed && let Some(shape) = read_shape(connection, &mut last_serial)? {
            // Latest wins: a burst of shape changes only sends the newest.
            sender.send_replace(Some(shape));
            continue;
        }
        // Wait for X traffic without blocking shutdown for long.
        let mut poll = libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        };
        // Safety: one valid pollfd for a socket this connection keeps open.
        let ready = unsafe { libc::poll(&mut poll, 1, STOP_POLL.as_millis() as i32) };
        if ready < 0 {
            let error = std::io::Error::last_os_error();
            ensure!(
                error.kind() == std::io::ErrorKind::Interrupted,
                "polling X connection: {error}"
            );
        }
    }
    Ok(())
}

/// Reads the displayed cursor. `Ok(None)` when its serial was already sent.
fn read_shape(
    connection: &impl Connection,
    last_serial: &mut Option<u32>,
) -> Result<Option<Option<CursorImage>>> {
    let reply = connection.xfixes_get_cursor_image()?.reply()?;
    if *last_serial == Some(reply.cursor_serial) {
        return Ok(None);
    }
    *last_serial = Some(reply.cursor_serial);
    let shape = convert(
        reply.width,
        reply.height,
        reply.xhot,
        reply.yhot,
        &reply.cursor_image,
    );
    if let Some(Some(image)) = &shape {
        tracing::debug!(
            width = image.width,
            height = image.height,
            serial = reply.cursor_serial,
            "host cursor changed"
        );
    }
    Ok(shape)
}

/// XFixes pixels are premultiplied ARGB in native `u32`s. Returns straight
/// RGBA, `Some(None)` for a fully transparent (hidden) cursor and `None` for an
/// image that cannot be sent (too large or malformed); the client then keeps
/// the previous shape.
pub fn convert(
    width: u16,
    height: u16,
    hot_x: u16,
    hot_y: u16,
    argb: &[u32],
) -> Option<Option<CursorImage>> {
    let pixels = usize::from(width) * usize::from(height);
    if width == 0 || height == 0 {
        return Some(None);
    }
    let argb = argb.get(..pixels)?;
    if argb.iter().all(|pixel| pixel >> 24 == 0) {
        return Some(None);
    }
    if width > MAX_CURSOR_SIZE || height > MAX_CURSOR_SIZE {
        tracing::warn!(
            width,
            height,
            "host cursor too large to send; keeping the previous shape"
        );
        return None;
    }
    let mut rgba = Vec::with_capacity(pixels * 4);
    for &pixel in argb {
        let alpha = (pixel >> 24) as u8;
        let straight = |shift: u32| {
            let channel = (pixel >> shift) as u8;
            if alpha == 0 {
                0
            } else {
                ((u32::from(channel) * 255 + u32::from(alpha) / 2) / u32::from(alpha)).min(255)
                    as u8
            }
        };
        rgba.extend_from_slice(&[straight(16), straight(8), straight(0), alpha]);
    }
    let image = CursorImage {
        width,
        height,
        hot_x: hot_x.min(width - 1),
        hot_y: hot_y.min(height - 1),
        rgba,
    };
    image.validate().ok()?;
    Some(Some(image))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn premultiplied_argb_becomes_straight_rgba() {
        // Opaque red, half-transparent premultiplied white, transparent.
        let argb = [0xffff_0000, 0x8080_8080, 0x0000_0000, 0xff00_ff00];
        let image = convert(2, 2, 1, 0, &argb).unwrap().unwrap();
        assert_eq!(
            (image.width, image.height, image.hot_x, image.hot_y),
            (2, 2, 1, 0)
        );
        assert_eq!(
            image.rgba,
            [
                255, 0, 0, 255, 255, 255, 255, 128, 0, 0, 0, 0, 0, 255, 0, 255
            ]
        );
        image.validate().unwrap();
    }

    #[test]
    fn transparent_cursor_is_hidden_and_bad_images_are_skipped() {
        assert_eq!(convert(16, 16, 0, 0, &[0; 256]), Some(None));
        assert_eq!(convert(0, 0, 0, 0, &[]), Some(None));
        // Short pixel data and oversized cursors keep the previous shape.
        assert_eq!(convert(16, 16, 0, 0, &[0xffff_ffff; 10]), None);
        let big = vec![0xffff_ffff; 300 * 300];
        assert_eq!(convert(300, 300, 0, 0, &big), None);
        // A hot spot past the edge is clamped rather than rejected.
        let image = convert(2, 2, 9, 9, &[0xffff_ffff; 4]).unwrap().unwrap();
        assert_eq!((image.hot_x, image.hot_y), (1, 1));
    }
}
