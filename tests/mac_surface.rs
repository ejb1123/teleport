//! Hardware acceptance test for the pinned applemedia native-surface contract.
//! Run on a Mac in `nix develop`:
//! TELEPORT_MAC_GPU_VIDEO=1 cargo test --test mac_surface -- --ignored
#![cfg(target_os = "macos")]

use std::{
    ffi::c_void,
    time::{Duration, Instant},
};

use gst::prelude::*;
use gstreamer as gst;

#[repr(C)]
struct CoreVideoMeta {
    meta: gst::ffi::GstMeta,
    cvbuf: *mut c_void,
    pixbuf: *mut c_void,
}

#[link(name = "CoreVideo", kind = "framework")]
unsafe extern "C" {
    fn CVPixelBufferGetIOSurface(buffer: *mut c_void) -> *mut c_void;
    fn CVPixelBufferGetWidth(buffer: *mut c_void) -> usize;
    fn CVPixelBufferGetHeight(buffer: *mut c_void) -> usize;
    fn CVPixelBufferGetPixelFormatType(buffer: *mut c_void) -> u32;
}

struct Pipeline(gst::Pipeline);
impl Drop for Pipeline {
    fn drop(&mut self) {
        let _ = self.0.set_state(gst::State::Null);
    }
}

// No GstBuffer/VideoFrame map, base-address lock, or pixel copy occurs here.
// The exact ABI is checked before accessing applemedia's private retained meta.
fn assert_native_surface(sample: &gst::Sample) {
    let buffer = sample.buffer().expect("decoded sample has a buffer");
    unsafe {
        let api = gst::glib::gobject_ffi::g_type_from_name(c"GstCoreVideoMetaAPI".as_ptr());
        assert_ne!(api, 0, "applemedia did not register its native meta");
        let meta = gst::ffi::gst_buffer_get_meta(buffer.as_ptr() as *mut _, api);
        assert!(!meta.is_null(), "native CVPixelBuffer meta was lost");
        assert!(!(*meta).info.is_null());
        assert_eq!((*(*meta).info).size, std::mem::size_of::<CoreVideoMeta>());
        let native = &*meta.cast::<CoreVideoMeta>();
        assert!(!native.pixbuf.is_null());
        assert_eq!(native.cvbuf, native.pixbuf);
        assert_eq!(CVPixelBufferGetWidth(native.pixbuf), 320);
        assert_eq!(CVPixelBufferGetHeight(native.pixbuf), 180);
        assert_eq!(
            CVPixelBufferGetPixelFormatType(native.pixbuf),
            u32::from_be_bytes(*b"420v")
        );
        assert!(
            !CVPixelBufferGetIOSurface(native.pixbuf).is_null(),
            "VideoToolbox output is not IOSurface-backed"
        );
    }
}

#[test]
#[ignore = "requires a Mac with H.264/HEVC VideoToolbox hardware and patched applemedia"]
fn videotoolbox_preserves_native_surfaces_for_both_codecs() {
    assert_eq!(
        std::env::var("TELEPORT_MAC_GPU_VIDEO").as_deref(),
        Ok("1"),
        "run with TELEPORT_MAC_GPU_VIDEO=1 to request Metal-compatible allocation"
    );
    gst::init().unwrap();
    for codec in [
        "x264enc tune=zerolatency speed-preset=ultrafast bframes=0 threads=2 ! h264parse ! video/x-h264,stream-format=avc,alignment=au",
        "x265enc tune=zerolatency speed-preset=ultrafast option-string=pools=1:frame-threads=1:log-level=error ! h265parse ! video/x-h265,stream-format=hvc1,alignment=au",
    ] {
        let pipeline = Pipeline(gst::parse::launch(&format!(
            "videotestsrc num-buffers=6 ! video/x-raw,format=I420,width=320,height=180,framerate=30/1,colorimetry=bt709 ! {codec} ! vtdec_hw ! video/x-raw,format=NV12,colorimetry=bt709 ! appsink name=frames sync=false max-buffers=8 drop=false"
        )).unwrap().downcast::<gst::Pipeline>().unwrap());
        let sink = pipeline
            .0
            .by_name("frames")
            .unwrap()
            .downcast::<gstreamer_app::AppSink>()
            .unwrap();
        pipeline.0.set_state(gst::State::Playing).unwrap();
        let deadline = Instant::now() + Duration::from_secs(15);
        let mut retained = None;
        for _ in 0..6 {
            let remaining = deadline.saturating_duration_since(Instant::now());
            let sample = sink
                .try_pull_sample(gst::ClockTime::from_nseconds(remaining.as_nanos() as u64))
                .unwrap_or_else(|| {
                    panic!(
                        "native decode timed out or ended early: {:?}",
                        pipeline
                            .0
                            .bus()
                            .unwrap()
                            .pop_filtered(&[gst::MessageType::Error])
                    )
                });
            assert_native_surface(&sample);
            retained.get_or_insert(sample);
        }
        // Retaining the sample must keep the CVPixelBuffer alive even after
        // destruction of the decoder/session, independent of its output pool.
        drop(sink);
        drop(pipeline);
        assert_native_surface(&retained.unwrap());
    }
}
