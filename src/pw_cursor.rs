//! Host cursor from PipeWire cursor metadata (Wayland portal capture with
//! `CursorMode::Metadata`), for clients that draw the pointer locally.
//!
//! The compositor attaches `SPA_META_Cursor` (position, hot spot and, when
//! the shape changed, an RGBA bitmap) to the same buffers that carry the
//! frames, including DMA-BUFs. `pipewiresrc` (PipeWire 1.x) negotiates that
//! meta but only exposes the position (as a region-of-interest meta). A pad
//! probe on each `pipewiresrc` reads the whole meta from the PipeWire buffer
//! behind the GStreamer buffer and publishes shapes and positions to a
//! process-wide hub that the session forwards as `Update::Cursor` and
//! `Update::CursorPosition`.
//!
//! The probe also handles cursor-only buffers: KWin marks a buffer that
//! only updates the cursor as corrupted (its pixels are stale). Those never
//! reach the encoder; at most every [`KEEPALIVE`] the last good frame is
//! repeated in their place, so a static desktop with a moving pointer still
//! produces the frames that keyframe requests and refinement rely on.

use crate::protocol::CursorImage;
use gstreamer as gst;
use gstreamer::prelude::*;
use std::{
    ffi::c_void,
    sync::{Mutex, OnceLock},
    time::{Duration, Instant},
};
use tokio::sync::watch;

/// `None` inside `Some` = hidden cursor (as in `crate::x11_cursor`).
pub type Shape = Option<Option<CursorImage>>;

/// How often a stale (cursor-only) buffer is replaced by the last good frame.
pub const KEEPALIVE: Duration = Duration::from_millis(250);

/// Tests: buffers that carried cursor meta, and stale buffers dropped.
#[cfg(test)]
pub static METAS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
#[cfg(test)]
pub static STALE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Element name of the `pipewiresrc` capturing monitor `index`.
pub fn source_name(index: usize) -> String {
    format!("teleport-monitor-{index}")
}

fn monitor_of(name: &str) -> usize {
    name.strip_prefix("teleport-monitor-")
        .and_then(|index| index.parse().ok())
        .unwrap_or(0)
}

/// Host pointer position, normalised to the monitor's frame (0..=1).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Position {
    pub monitor: usize,
    pub x: f64,
    pub y: f64,
}

struct Hub {
    shapes: watch::Sender<Shape>,
    positions: watch::Sender<Option<Position>>,
}

fn hub() -> &'static Hub {
    static HUB: OnceLock<Hub> = OnceLock::new();
    HUB.get_or_init(|| Hub {
        shapes: watch::channel(None).0,
        positions: watch::channel(None).0,
    })
}

/// Shapes and positions from every portal capture pipeline of this process
/// (latest wins).
pub struct Feed {
    pub shapes: watch::Receiver<Shape>,
    pub positions: watch::Receiver<Option<Position>>,
}

/// The compositor sends a bitmap only when the shape changes, and with
/// DMA-BUF capture KWin's first one is lost when the buffers are
/// renegotiated at stream start. So the last visible shape seen by this
/// process is replayed (marked changed) to a new subscriber. A hidden one
/// is not: a game may have quit since, and a hidden pointer locks the mouse.
pub fn subscribe() -> Feed {
    let hub = hub();
    let mut shapes = hub.shapes.subscribe();
    if matches!(*shapes.borrow(), Some(Some(_))) {
        shapes.mark_changed();
    }
    Feed {
        shapes,
        positions: hub.positions.subscribe(),
    }
}

/// What one buffer's cursor meta says.
#[derive(Debug, PartialEq)]
pub(crate) struct Meta {
    /// The pointer is on this monitor (`spa_meta_cursor.id != 0`).
    pub on_output: bool,
    pub x: i32,
    pub y: i32,
    /// `None`: the shape did not change with this buffer.
    pub shape: Option<Option<CursorImage>>,
}

const SPA_META_CURSOR: u32 = 5;
const SPA_VIDEO_FORMAT_RGBX: u32 = 7;
const SPA_VIDEO_FORMAT_BGRX: u32 = 8;
const SPA_VIDEO_FORMAT_RGBA: u32 = 11;
const SPA_VIDEO_FORMAT_BGRA: u32 = 12;
const SPA_VIDEO_FORMAT_ARGB: u32 = 13;
const SPA_VIDEO_FORMAT_ABGR: u32 = 14;
/// `struct spa_meta_cursor`: id, flags, position, hotspot, bitmap_offset.
const CURSOR_HEADER: usize = 28;
/// `struct spa_meta_bitmap`: format, size, stride, offset.
const BITMAP_HEADER: usize = 20;

fn u32_at(bytes: &[u8], offset: usize) -> Option<u32> {
    Some(u32::from_ne_bytes(
        bytes.get(offset..offset.checked_add(4)?)?.try_into().ok()?,
    ))
}

fn i32_at(bytes: &[u8], offset: usize) -> Option<i32> {
    u32_at(bytes, offset).map(|value| value as i32)
}

/// Parses a `spa_meta_cursor` region of `bytes.len()` bytes (the meta's
/// negotiated size). Out-of-bounds offsets yield `None`; an unusable bitmap
/// keeps the previous shape. Bitmaps are premultiplied (KWin, Mutter).
pub(crate) fn parse(bytes: &[u8]) -> Option<Meta> {
    let id = u32_at(bytes, 0)?;
    let x = i32_at(bytes, 8)?;
    let y = i32_at(bytes, 12)?;
    let (hot_x, hot_y) = (i32_at(bytes, 16)?, i32_at(bytes, 20)?);
    let bitmap_offset = u32_at(bytes, 24)? as usize;
    let mut meta = Meta {
        on_output: id != 0,
        x,
        y,
        shape: None,
    };
    if id == 0 || bitmap_offset == 0 {
        return Some(meta);
    }
    if bitmap_offset < CURSOR_HEADER {
        return Some(meta);
    }
    let bitmap = bytes.get(bitmap_offset..)?;
    let format = u32_at(bitmap, 0)?;
    let (width, height) = (u32_at(bitmap, 4)?, u32_at(bitmap, 8)?);
    let stride = i32_at(bitmap, 12)?;
    let offset = u32_at(bitmap, 16)? as usize;
    if format == 0 {
        // "No new bitmap information."
        return Some(meta);
    }
    if offset == 0 || width == 0 || height == 0 {
        // No image data: an invisible pointer (a game hid it).
        meta.shape = Some(None);
        return Some(meta);
    }
    if offset < BITMAP_HEADER {
        return Some(meta);
    }
    let order: [usize; 4] = match format {
        // Byte positions of R, G, B, A.
        SPA_VIDEO_FORMAT_RGBA | SPA_VIDEO_FORMAT_RGBX => [0, 1, 2, 3],
        SPA_VIDEO_FORMAT_BGRA | SPA_VIDEO_FORMAT_BGRX => [2, 1, 0, 3],
        SPA_VIDEO_FORMAT_ARGB => [1, 2, 3, 0],
        SPA_VIDEO_FORMAT_ABGR => [3, 2, 1, 0],
        _ => {
            tracing::debug!(format, "unsupported cursor bitmap format");
            return Some(meta);
        }
    };
    let opaque = matches!(format, SPA_VIDEO_FORMAT_RGBX | SPA_VIDEO_FORMAT_BGRX);
    let (Ok(width16), Ok(height16)) = (u16::try_from(width), u16::try_from(height)) else {
        return Some(meta);
    };
    let row = (width as usize).checked_mul(4)?;
    let stride = usize::try_from(stride)
        .ok()
        .filter(|stride| *stride >= row)?;
    let pixels = bitmap.get(offset..)?;
    let mut argb = Vec::with_capacity(width as usize * height as usize);
    for line in 0..height as usize {
        let start = line.checked_mul(stride)?;
        for pixel in pixels.get(start..start.checked_add(row)?)?.chunks_exact(4) {
            let alpha = if opaque { 255 } else { pixel[order[3]] };
            argb.push(
                u32::from(alpha) << 24
                    | u32::from(pixel[order[0]]) << 16
                    | u32::from(pixel[order[1]]) << 8
                    | u32::from(pixel[order[2]]),
            );
        }
    }
    let clamp = |hot: i32| u16::try_from(hot.max(0)).unwrap_or(u16::MAX);
    // A fully transparent bitmap is a hidden pointer too.
    meta.shape = crate::x11_cursor::convert(width16, height16, clamp(hot_x), clamp(hot_y), &argb);
    Some(meta)
}

// PipeWire ABI (stable since 0.3): `struct spa_buffer`, `struct spa_meta`
// and the start of `struct pw_buffer`.
#[repr(C)]
struct SpaBuffer {
    n_metas: u32,
    n_datas: u32,
    metas: *const SpaMeta,
    datas: *const c_void,
}

#[repr(C)]
struct SpaMeta {
    kind: u32,
    size: u32,
    data: *const u8,
}

#[repr(C)]
struct PwBuffer {
    buffer: *const SpaBuffer,
}

/// The start of pipewiresrc's private `GstPipeWirePoolData` (identical in
/// PipeWire 0.3.x and 1.x). Validated before use: `buf` must point back at
/// the buffer carrying it, and `pool` must be a `GstPipeWirePool`.
#[repr(C)]
struct PoolData {
    pool: *mut gst::glib::gobject_ffi::GObject,
    owner: *mut c_void,
    header: *mut c_void,
    flags: u32,
    b: *const PwBuffer,
    buf: *mut gst::ffi::GstBuffer,
}

/// The cursor meta of the PipeWire buffer behind `buffer`, if any.
fn cursor_meta(buffer: &gst::BufferRef) -> Option<Meta> {
    // Safety: g_quark_try_string only looks the string up.
    let quark = unsafe { gst::glib::ffi::g_quark_try_string(c"GstPipeWirePoolDataQuark".as_ptr()) };
    if quark == 0 {
        return None;
    }
    for meta in buffer.iter_meta::<gst::meta::ParentBufferMeta>() {
        let parent = meta.parent();
        let pointer = parent.as_ptr() as *mut gst::ffi::GstMiniObject;
        // Safety: reading qdata of a live buffer we hold a reference to.
        let data =
            unsafe { gst::ffi::gst_mini_object_get_qdata(pointer, quark) } as *const PoolData;
        if data.is_null() {
            continue;
        }
        // Safety: the qdata under this quark is pipewiresrc's pool data,
        // whose leading fields are checked against the buffer and its pool
        // before anything else is read. The PipeWire buffer stays dequeued
        // (and its meta unchanged) while the GStreamer buffer is alive.
        unsafe {
            let data = &*data;
            if !std::ptr::eq(data.buf, parent.as_ptr()) || data.b.is_null() || data.pool.is_null() {
                continue;
            }
            let type_name =
                gst::glib::gobject_ffi::g_type_name((*(*data.pool).g_type_instance.g_class).g_type);
            if type_name.is_null()
                || std::ffi::CStr::from_ptr(type_name).to_bytes() != b"GstPipeWirePool"
            {
                continue;
            }
            let spa = (*data.b).buffer;
            if spa.is_null() || (*spa).metas.is_null() {
                continue;
            }
            let metas = std::slice::from_raw_parts((*spa).metas, (*spa).n_metas as usize);
            let cursor = metas
                .iter()
                .find(|meta| meta.kind == SPA_META_CURSOR && !meta.data.is_null())?;
            let size = cursor.size as usize;
            if !(CURSOR_HEADER..=4 << 20).contains(&size) {
                return None;
            }
            // Parsed in place: only a changed bitmap is copied.
            return parse(std::slice::from_raw_parts(cursor.data, size));
        }
    }
    None
}

/// Probe state of one `pipewiresrc`.
struct Source {
    monitor: usize,
    /// Last frame with valid pixels, repeated in place of stale buffers.
    last_good: Option<gst::Buffer>,
    last_pass: Instant,
    /// Frame size in pixels, for normalising positions.
    size: Option<(u32, u32)>,
    last_shape: Option<Option<CursorImage>>,
}

impl Source {
    fn publish(&mut self, meta: Meta) {
        let hub = hub();
        if let Some(shape) = meta.shape
            && self.last_shape.as_ref() != Some(&shape)
        {
            // Always notify: a hidden pointer the hub already holds was not
            // replayed to the current session (see `subscribe`).
            self.last_shape = Some(shape.clone());
            hub.shapes.send_replace(Some(shape));
        }
        if meta.on_output
            && let Some((width, height)) = self.size
        {
            let position = Position {
                monitor: self.monitor,
                x: (f64::from(meta.x) / f64::from(width.max(2) - 1)).clamp(0.0, 1.0),
                y: (f64::from(meta.y) / f64::from(height.max(2) - 1)).clamp(0.0, 1.0),
            };
            hub.positions.send_if_modified(|current| {
                if *current == Some(position) {
                    return false;
                }
                *current = Some(position);
                true
            });
        }
    }
}

/// Adds the cursor probe to every `pipewiresrc` in `pipeline`.
pub fn attach(pipeline: &gst::Pipeline) {
    for element in pipeline.iterate_recurse().into_iter().flatten() {
        if element
            .factory()
            .is_none_or(|factory| factory.name() != "pipewiresrc")
        {
            continue;
        }
        let Some(pad) = element.static_pad("src") else {
            continue;
        };
        let source = Mutex::new(Source {
            monitor: monitor_of(&element.name()),
            last_good: None,
            last_pass: Instant::now(),
            size: None,
            last_shape: None,
        });
        pad.add_probe(
            gst::PadProbeType::BUFFER | gst::PadProbeType::EVENT_DOWNSTREAM,
            move |_, info| {
                let mut source = source.lock().unwrap();
                if let Some(gst::PadProbeData::Event(event)) = &info.data {
                    if let gst::EventView::Caps(caps) = event.view()
                        && let Some(structure) = caps.caps().structure(0)
                        && let (Ok(width), Ok(height)) = (
                            structure.get::<i32>("width"),
                            structure.get::<i32>("height"),
                        )
                    {
                        source.size = Some((width.max(0) as u32, height.max(0) as u32));
                    }
                    return gst::PadProbeReturn::Ok;
                }
                let Some(buffer) = info.buffer() else {
                    return gst::PadProbeReturn::Ok;
                };
                if let Some(meta) = cursor_meta(buffer) {
                    #[cfg(test)]
                    METAS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    source.publish(meta);
                }
                filter(&mut source, info)
            },
        );
    }
}

/// Drops cursor-only (stale) buffers, repeating the last good frame at most
/// every [`KEEPALIVE`] instead.
fn filter(source: &mut Source, info: &mut gst::PadProbeInfo) -> gst::PadProbeReturn {
    let Some(buffer) = info.buffer() else {
        return gst::PadProbeReturn::Ok;
    };
    let now = Instant::now();
    if !buffer.flags().contains(gst::BufferFlags::CORRUPTED) {
        source.last_good = Some(buffer.to_owned());
        source.last_pass = now;
        return gst::PadProbeReturn::Ok;
    }
    #[cfg(test)]
    STALE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let Some(good) = source
        .last_good
        .as_ref()
        .filter(|_| now.duration_since(source.last_pass) >= KEEPALIVE)
    else {
        return gst::PadProbeReturn::Drop;
    };
    // Same memory (no copy of the pixels), current timestamps.
    let mut repeat = good.copy();
    {
        let repeat = repeat.make_mut();
        repeat.set_pts(buffer.pts());
        repeat.set_dts(buffer.dts());
        repeat.set_duration(buffer.duration());
    }
    source.last_pass = now;
    info.data = Some(gst::PadProbeData::Buffer(repeat));
    gst::PadProbeReturn::Ok
}

#[cfg(test)]
mod tests {
    use super::*;

    fn meta(id: u32, position: (i32, i32), hot: (i32, i32), bitmap: Option<&[u8]>) -> Vec<u8> {
        let mut bytes = Vec::new();
        for value in [
            id,
            0,
            position.0 as u32,
            position.1 as u32,
            hot.0 as u32,
            hot.1 as u32,
        ] {
            bytes.extend_from_slice(&value.to_ne_bytes());
        }
        let offset = if bitmap.is_some() {
            CURSOR_HEADER as u32
        } else {
            0
        };
        bytes.extend_from_slice(&offset.to_ne_bytes());
        if let Some(bitmap) = bitmap {
            bytes.extend_from_slice(bitmap);
        }
        bytes
    }

    fn bitmap(format: u32, size: (u32, u32), stride: i32, pixels: &[u8]) -> Vec<u8> {
        let mut bytes = Vec::new();
        let offset = if pixels.is_empty() {
            0
        } else {
            BITMAP_HEADER as u32
        };
        for value in [format, size.0, size.1, stride as u32, offset] {
            bytes.extend_from_slice(&value.to_ne_bytes());
        }
        bytes.extend_from_slice(pixels);
        bytes
    }

    #[test]
    fn position_only_and_off_output_metas_keep_the_shape() {
        let parsed = parse(&meta(1, (10, -3), (0, 0), None)).unwrap();
        assert_eq!(
            parsed,
            Meta {
                on_output: true,
                x: 10,
                y: -3,
                shape: None
            }
        );
        let parsed = parse(&meta(0, (5, 5), (0, 0), None)).unwrap();
        assert!(!parsed.on_output && parsed.shape.is_none());
        assert!(parse(&[0; 8]).is_none());
    }

    #[test]
    fn premultiplied_bitmaps_become_straight_rgba_with_stride() {
        // 2x1 RGBA premultiplied, stride padded to 12 bytes: opaque red and
        // half-transparent white.
        let pixels = [255, 0, 0, 255, 128, 128, 128, 128, 9, 9, 9, 9];
        let bytes = meta(
            1,
            (100, 200),
            (1, 0),
            Some(&bitmap(SPA_VIDEO_FORMAT_RGBA, (2, 1), 12, &pixels)),
        );
        let image = parse(&bytes).unwrap().shape.unwrap().unwrap();
        assert_eq!(
            (image.width, image.height, image.hot_x, image.hot_y),
            (2, 1, 1, 0)
        );
        assert_eq!(image.rgba, [255, 0, 0, 255, 255, 255, 255, 128]);
        // BGRA swaps red and blue.
        let bytes = meta(
            1,
            (0, 0),
            (0, 0),
            Some(&bitmap(SPA_VIDEO_FORMAT_BGRA, (1, 1), 4, &[255, 0, 0, 255])),
        );
        let image = parse(&bytes).unwrap().shape.unwrap().unwrap();
        assert_eq!(image.rgba, [0, 0, 255, 255]);
    }

    #[test]
    fn empty_or_transparent_bitmaps_hide_and_bad_ones_are_skipped() {
        // KWin: a hidden pointer has a 0x0 bitmap.
        let hidden = meta(
            1,
            (0, 0),
            (0, 0),
            Some(&bitmap(SPA_VIDEO_FORMAT_RGBA, (0, 0), 0, &[])),
        );
        assert_eq!(parse(&hidden).unwrap().shape, Some(None));
        let transparent = meta(
            1,
            (0, 0),
            (0, 0),
            Some(&bitmap(SPA_VIDEO_FORMAT_RGBA, (1, 1), 4, &[0, 0, 0, 0])),
        );
        assert_eq!(parse(&transparent).unwrap().shape, Some(None));
        // Pixels past the meta, a short stride and unknown formats.
        let short = meta(
            1,
            (0, 0),
            (0, 0),
            Some(&bitmap(SPA_VIDEO_FORMAT_RGBA, (4, 4), 16, &[255; 8])),
        );
        assert!(parse(&short).is_none());
        let stride = meta(
            1,
            (0, 0),
            (0, 0),
            Some(&bitmap(SPA_VIDEO_FORMAT_RGBA, (2, 1), 4, &[255; 8])),
        );
        assert!(parse(&stride).is_none());
        let unknown = meta(1, (0, 0), (0, 0), Some(&bitmap(99, (1, 1), 4, &[255; 4])));
        assert_eq!(parse(&unknown).unwrap().shape, None);
        // Too large to send: the previous shape stays.
        let big = vec![255; 300 * 300 * 4];
        let large = meta(
            1,
            (0, 0),
            (0, 0),
            Some(&bitmap(SPA_VIDEO_FORMAT_RGBA, (300, 300), 1200, &big)),
        );
        assert_eq!(parse(&large).unwrap().shape, None);
    }

    #[test]
    fn monitor_index_comes_from_the_source_name() {
        assert_eq!(monitor_of(&source_name(3)), 3);
        assert_eq!(monitor_of("pipewiresrc0"), 0);
    }

    #[test]
    fn stale_buffers_are_dropped_and_the_last_good_frame_repeats() {
        gst::init().unwrap();
        let mut source = Source {
            monitor: 0,
            last_good: None,
            last_pass: Instant::now(),
            size: None,
            last_shape: None,
        };
        let probe = |source: &mut Source, buffer: gst::Buffer| {
            let mut info = gst::PadProbeInfo {
                mask: gst::PadProbeType::BUFFER,
                id: None,
                offset: 0,
                size: 0,
                data: Some(gst::PadProbeData::Buffer(buffer)),
                flow_res: Ok(gst::FlowSuccess::Ok),
            };
            let result = filter(source, &mut info);
            (result, info.data)
        };
        let good = gst::Buffer::from_slice([1u8, 2, 3]);
        let (result, _) = probe(&mut source, good.clone());
        assert_eq!(result, gst::PadProbeReturn::Ok);
        let mut stale = gst::Buffer::from_slice([9u8, 9, 9]);
        stale.make_mut().set_flags(gst::BufferFlags::CORRUPTED);
        stale.make_mut().set_pts(gst::ClockTime::from_seconds(7));
        let (result, _) = probe(&mut source, stale.clone());
        assert_eq!(result, gst::PadProbeReturn::Drop);
        source.last_pass = Instant::now() - KEEPALIVE;
        let (result, data) = probe(&mut source, stale);
        assert_eq!(result, gst::PadProbeReturn::Ok);
        let Some(gst::PadProbeData::Buffer(repeat)) = data else {
            panic!("no repeated frame")
        };
        assert_eq!(repeat.map_readable().unwrap().as_slice(), [1, 2, 3]);
        assert_eq!(repeat.pts(), Some(gst::ClockTime::from_seconds(7)));
        assert!(!repeat.flags().contains(gst::BufferFlags::CORRUPTED));
        // Shares the frame's memory: refinement sees an unchanged frame.
        assert_eq!(repeat.peek_memory(0).as_ptr(), good.peek_memory(0).as_ptr());
    }
}
