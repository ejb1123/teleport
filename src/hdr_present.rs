//! Native macOS EDR presentation. No RGB8 conversion is permitted on the video path.
//! See docs/hdr-presentation.md for the frame and UI composition contracts.
#![cfg_attr(not(target_os = "macos"), allow(dead_code))]

use anyhow::{Result, ensure};
use sdl2::rect::Rect;

pub const SDR_WHITE_NITS: f64 = 203.0;

/// BT.2020 non-constant-luminance, limited-range ST 2084 P010, little-endian,
/// with centered (JPEG-style) chroma siting, normalized by the media decoder.
/// Ten significant bits occupy bits 15..6 of each 16-bit component.
pub struct P010Frame<'a> {
    pub width: u32,
    pub height: u32,
    pub y: &'a [u8],
    pub uv: &'a [u8],
    pub y_stride: usize,
    pub uv_stride: usize,
}

impl P010Frame<'_> {
    pub fn validate(&self) -> Result<()> {
        crate::protocol::validate_video_size(self.width, self.height)?;
        ensure!(
            self.width.is_multiple_of(2) && self.height.is_multiple_of(2),
            "P010 dimensions must be even"
        );
        validate_plane(
            self.y,
            self.y_stride,
            self.width as usize * 2,
            self.height as usize,
        )?;
        validate_plane(
            self.uv,
            self.uv_stride,
            self.width as usize * 2,
            self.height as usize / 2,
        )?;
        ensure!(
            self.y_stride.is_multiple_of(2) && self.uv_stride.is_multiple_of(4),
            "P010 strides must align to texture pixels"
        );
        Ok(())
    }
}

/// Straight-alpha RGBA8/sRGB UI pixels, not the video. Rect is in SDL logical
/// coordinates; width/height are actual readback pixels (possibly Retina-scaled).
pub struct Overlay<'a> {
    pub rect: Rect,
    pub width: u32,
    pub height: u32,
    pub stride: usize,
    pub rgba: &'a [u8],
}

impl Overlay<'_> {
    fn validate(&self) -> Result<()> {
        ensure!(
            self.width > 0 && self.height > 0 && self.width <= 16384 && self.height <= 16384,
            "invalid HDR UI overlay dimensions"
        );
        validate_plane(
            self.rgba,
            self.stride,
            self.width as usize * 4,
            self.height as usize,
        )?;
        ensure!(
            self.stride.is_multiple_of(4),
            "RGBA UI stride must align to pixels"
        );
        Ok(())
    }
}

fn validate_plane(bytes: &[u8], stride: usize, row_bytes: usize, rows: usize) -> Result<()> {
    ensure!(rows > 0 && stride >= row_bytes, "invalid image stride");
    let required = stride
        .checked_mul(rows - 1)
        .and_then(|n| n.checked_add(row_bytes));
    ensure!(
        required.is_some_and(|n| n <= bytes.len()),
        "truncated image plane"
    );
    Ok(())
}

#[derive(Debug, Clone, Copy)]
pub enum PresentOutcome {
    Presented {
        headroom: f64,
    },
    /// No HDR presentation confirmed: drawable unavailable or EDR still warming up.
    DrawableUnavailable,
}

#[cfg(target_os = "macos")]
pub use macos::HdrPresenter;

#[cfg(not(target_os = "macos"))]
pub struct HdrPresenter;

#[cfg(not(target_os = "macos"))]
impl HdrPresenter {
    pub fn device_name(&self) -> String {
        "unavailable".to_owned()
    }
    pub fn new_sdr(_window: &sdl2::video::Window) -> Result<Self> {
        anyhow::bail!("GPU surface presentation is only available on macOS")
    }

    pub fn present_surface(
        &mut self,
        _sample: &gstreamer::Sample,
        _desktop: Rect,
        _overlays: &[Overlay<'_>],
        _window_size: (u32, u32),
    ) -> Result<PresentOutcome> {
        anyhow::bail!("GPU surface presentation is only available on macOS")
    }

    pub fn new(_window: &sdl2::video::Window) -> Result<Self> {
        anyhow::bail!(
            "native HDR presentation is currently implemented only for macOS; request an SDR stream on this platform"
        )
    }

    pub fn present(
        &mut self,
        _frame: &P010Frame<'_>,
        _desktop: Rect,
        _overlays: &[Overlay<'_>],
        _window_size: (u32, u32),
    ) -> Result<PresentOutcome> {
        anyhow::bail!("native HDR presentation is unavailable on this platform")
    }
}

// Reference implementation used for shader coefficient tests, not per-pixel CPU work.
#[cfg(test)]
fn pq_to_linear_edr(pq: f64) -> f64 {
    let p = pq.clamp(0.0, 1.0).powf(1.0 / (2523.0 / 32.0));
    let ratio = (p - 3424.0 / 4096.0).max(0.0) / (2413.0 / 128.0 - 2392.0 / 128.0 * p);
    ratio.powf(1.0 / (2610.0 / 16384.0)) * 10000.0 / SDR_WHITE_NITS
}

#[cfg(target_os = "macos")]
mod macos {
    use super::*;
    use anyhow::Context;
    use objc2::{
        MainThreadMarker, msg_send,
        rc::{Retained, autoreleasepool},
        runtime::{AnyObject, ProtocolObject},
    };
    use objc2_core_graphics::{CGColorSpace, kCGColorSpaceExtendedLinearITUR_2020};
    use objc2_foundation::NSString;
    use objc2_metal::*;
    use objc2_quartz_core::{CAMetalDrawable, CAMetalLayer};
    use std::{
        collections::VecDeque,
        ffi::c_void,
        ptr::NonNull,
        time::{Duration, Instant},
    };

    struct MetalView(sdl2::sys::SDL_MetalView);

    impl Drop for MetalView {
        fn drop(&mut self) {
            // SDL owns this retained NSView. Always destroy on the creating thread.
            unsafe { sdl2::sys::SDL_Metal_DestroyView(self.0) };
        }
    }

    struct Planes {
        size: (u32, u32),
        y: Retained<ProtocolObject<dyn MTLTexture>>,
        uv: Retained<ProtocolObject<dyn MTLTexture>>,
    }

    struct InFlight {
        command: Retained<ProtocolObject<dyn MTLCommandBuffer>>,
        planes: Planes,
    }

    // Core Video wrappers MUST survive GPU completion, not just MTLTexture.
    struct CfOwned(NonNull<c_void>);
    impl Drop for CfOwned {
        fn drop(&mut self) {
            unsafe { CFRelease(self.0.as_ptr()) }
        }
    }
    struct SurfaceFlight {
        command: Retained<ProtocolObject<dyn MTLCommandBuffer>>,
        _sample: gstreamer::Sample,
        _y: CfOwned,
        _uv: CfOwned,
    }
    #[link(name = "CoreFoundation", kind = "framework")]
    unsafe extern "C" {
        fn CFRelease(value: *const c_void);
    }
    #[link(name = "CoreVideo", kind = "framework")]
    unsafe extern "C" {
        fn CVMetalTextureCacheCreate(
            allocator: *const c_void,
            attributes: *const c_void,
            device: *const c_void,
            texture_attributes: *const c_void,
            output: *mut *mut c_void,
        ) -> i32;
        fn CVMetalTextureCacheCreateTextureFromImage(
            allocator: *const c_void,
            cache: *mut c_void,
            image: *mut c_void,
            attributes: *const c_void,
            format: usize,
            width: usize,
            height: usize,
            plane: usize,
            output: *mut *mut c_void,
        ) -> i32;
        fn CVMetalTextureGetTexture(texture: *mut c_void) -> *mut ProtocolObject<dyn MTLTexture>;
        fn CVPixelBufferGetPixelFormatType(buffer: *mut c_void) -> u32;
        fn CVPixelBufferGetPlaneCount(buffer: *mut c_void) -> usize;
        fn CVPixelBufferGetIOSurface(buffer: *mut c_void) -> *mut c_void;
        fn CVPixelBufferGetWidthOfPlane(buffer: *mut c_void, plane: usize) -> usize;
        fn CVPixelBufferGetHeightOfPlane(buffer: *mut c_void, plane: usize) -> usize;
    }

    pub struct HdrPresenter {
        // The main-thread marker prevents moving Cocoa state to another thread.
        _main_thread: MainThreadMarker,
        view: MetalView,
        layer: Retained<CAMetalLayer>,
        device: Retained<ProtocolObject<dyn MTLDevice>>,
        queue: Retained<ProtocolObject<dyn MTLCommandQueue>>,
        video_pipeline: Retained<ProtocolObject<dyn MTLRenderPipelineState>>,
        ui_pipeline: Retained<ProtocolObject<dyn MTLRenderPipelineState>>,
        in_flight: VecDeque<InFlight>,
        surface_flights: VecDeque<SurfaceFlight>,
        surface_cache: Option<CfOwned>,
        sdr: bool,
        edr_started: Option<Instant>,
        has_presented_hdr: bool,
        // A cloned SDL handle ensures the window outlives the attached view.
        _window: sdl2::video::Window,
    }

    impl HdrPresenter {
        pub fn device_name(&self) -> String {
            self.device.name().to_string()
        }
        pub fn new(window: &sdl2::video::Window) -> Result<Self> {
            Self::new_mode(window, false)
        }

        pub fn new_sdr(window: &sdl2::video::Window) -> Result<Self> {
            Self::new_mode(window, true)
        }

        fn new_mode(window: &sdl2::video::Window, sdr: bool) -> Result<Self> {
            let main_thread = MainThreadMarker::new()
                .context("HDR presentation must run on the macOS main thread")?;
            autoreleasepool(|_| {
                let device = MTLCreateSystemDefaultDevice().context("no Metal device available")?;
                // Create only after the SDL UI renderer so our view is the topmost
                // compositor. SDL's metal view hitTest returns nil: input stays SDL.
                let raw = unsafe { sdl2::sys::SDL_Metal_CreateView(window.raw()) };
                ensure!(
                    !raw.is_null(),
                    "creating native HDR view failed: {}",
                    sdl2::get_error()
                );
                let view = MetalView(raw);
                let layer_ptr = unsafe { sdl2::sys::SDL_Metal_GetLayer(view.0) };
                let layer = unsafe { Retained::<CAMetalLayer>::retain(layer_ptr.cast()) }
                    .context("SDL did not provide a CAMetalLayer")?;
                layer.setDevice(Some(&device));
                layer.setPixelFormat(MTLPixelFormat::RGBA16Float);
                layer.setFramebufferOnly(true);
                layer.setWantsExtendedDynamicRangeContent(!sdr);
                let colorspace =
                    CGColorSpace::with_name(Some(unsafe { kCGColorSpaceExtendedLinearITUR_2020 }))
                        .context("extended linear BT.2020 color space unavailable")?;
                layer.setColorspace(Some(&colorspace));
                if sdr {
                    tracing::info!(
                        "created SDR Metal layer for GPU-resident VideoToolbox surfaces"
                    );
                } else {
                    let headroom = screen_headroom(view.0)?;
                    tracing::info!(
                        headroom,
                        sdr_white_nits = SDR_WHITE_NITS,
                        "requested floating-point BT.2020 EDR layer; waiting for display headroom"
                    );
                }
                let library = device
                    .newLibraryWithSource_options_error(&NSString::from_str(SHADER), None)
                    .map_err(|error| anyhow::anyhow!("compiling HDR Metal shaders: {error}"))?;
                let video_pipeline = make_pipeline(
                    &device,
                    &library,
                    if sdr { "nv12_video" } else { "pq_video" },
                    false,
                )?;
                let ui_pipeline = make_pipeline(&device, &library, "sdr_ui", true)?;
                let queue = device
                    .newCommandQueue()
                    .context("creating HDR Metal command queue")?;
                Ok(Self {
                    _main_thread: main_thread,
                    view,
                    layer,
                    device,
                    queue,
                    video_pipeline,
                    ui_pipeline,
                    in_flight: VecDeque::new(),
                    surface_flights: VecDeque::new(),
                    surface_cache: None,
                    sdr,
                    edr_started: None,
                    has_presented_hdr: false,
                    _window: window.clone(),
                })
            })
        }

        pub fn present(
            &mut self,
            frame: &P010Frame<'_>,
            desktop: Rect,
            overlays: &[Overlay<'_>],
            window_size: (u32, u32),
        ) -> Result<PresentOutcome> {
            ensure!(!self.sdr, "P010 CPU presentation requires an HDR presenter");
            frame.validate()?;
            ensure!(window_size.0 > 0 && window_size.1 > 0, "empty HDR window");
            ensure!(overlays.len() <= 16, "too many HDR UI overlays");
            for overlay in overlays {
                overlay.validate()?;
            }
            autoreleasepool(|_| self.present_inner(frame, desktop, overlays, window_size))
        }

        fn present_inner(
            &mut self,
            frame: &P010Frame<'_>,
            desktop: Rect,
            overlays: &[Overlay<'_>],
            window_size: (u32, u32),
        ) -> Result<PresentOutcome> {
            let headroom = screen_headroom(self.view.0)?;
            let warming_up = headroom <= 1.0;
            let edr_started = *self.edr_started.get_or_insert_with(Instant::now);
            ensure!(
                !warming_up
                    || (!self.has_presented_hdr && edr_started.elapsed() < Duration::from_secs(3)),
                "display has no current EDR headroom; request an SDR stream or enable an HDR-capable display"
            );
            // SDL updates this view's frame for resize/fullscreen and display moves.
            let view: &AnyObject = unsafe { &*self.view.0.cast() };
            let _: () = unsafe { msg_send![view, updateDrawableSize] };
            let drawable = match self.layer.nextDrawable() {
                Some(drawable) => drawable,
                None => return Ok(PresentOutcome::DrawableUnavailable),
            };
            let reuse = if self.in_flight.len() >= 3 {
                let old = self.in_flight.pop_front().unwrap();
                old.command.waitUntilCompleted();
                ensure!(
                    old.command.status() != MTLCommandBufferStatus::Error,
                    "HDR GPU command failed: {:?}",
                    old.command.error()
                );
                Some(old.planes)
            } else {
                None
            };
            let planes = match reuse {
                Some(planes) if planes.size == (frame.width, frame.height) => planes,
                _ => Planes {
                    size: (frame.width, frame.height),
                    y: self.texture(MTLPixelFormat::R16Unorm, frame.width, frame.height)?,
                    uv: self.texture(
                        MTLPixelFormat::RG16Unorm,
                        frame.width / 2,
                        frame.height / 2,
                    )?,
                },
            };
            upload(
                &planes.y,
                frame.y,
                frame.y_stride,
                frame.width,
                frame.height,
            );
            upload(
                &planes.uv,
                frame.uv,
                frame.uv_stride,
                frame.width / 2,
                frame.height / 2,
            );
            let pass = MTLRenderPassDescriptor::new();
            let attachment = unsafe { pass.colorAttachments().objectAtIndexedSubscript(0) };
            attachment.setTexture(Some(&drawable.texture()));
            attachment.setLoadAction(MTLLoadAction::Clear);
            attachment.setStoreAction(MTLStoreAction::Store);
            attachment.setClearColor(MTLClearColor {
                red: 0.0,
                green: 0.0,
                blue: 0.0,
                alpha: 1.0,
            });
            let command = self
                .queue
                .commandBuffer()
                .context("allocating HDR command buffer")?;
            let encoder = command
                .renderCommandEncoderWithDescriptor(&pass)
                .context("creating HDR render encoder")?;
            encoder.setRenderPipelineState(&self.video_pipeline);
            unsafe {
                encoder.setFragmentTexture_atIndex(Some(&planes.y), 0);
                encoder.setFragmentTexture_atIndex(Some(&planes.uv), 1);
            }
            draw_quad(&encoder, desktop, window_size);
            encoder.setRenderPipelineState(&self.ui_pipeline);
            for overlay in overlays {
                let texture =
                    self.texture(MTLPixelFormat::RGBA8Unorm, overlay.width, overlay.height)?;
                upload(
                    &texture,
                    overlay.rgba,
                    overlay.stride,
                    overlay.width,
                    overlay.height,
                );
                unsafe {
                    encoder.setFragmentTexture_atIndex(Some(&texture), 0);
                }
                draw_quad(&encoder, overlay.rect, window_size);
                // Normal Metal command buffers retain referenced textures until done.
            }
            encoder.endEncoding();
            command.presentDrawable(ProtocolObject::from_ref(&*drawable));
            command.commit();
            self.in_flight.push_back(InFlight { command, planes });
            // Some displays grant headroom only after an EDR layer submits its
            // first drawable. Submit during a bounded startup grace period, but
            // never count potentially SDR-clamped warmup output as HDR success.
            if warming_up {
                return Ok(PresentOutcome::DrawableUnavailable);
            }
            self.has_presented_hdr = true;
            Ok(PresentOutcome::Presented { headroom })
        }

        pub fn present_surface(
            &mut self,
            sample: &gstreamer::Sample,
            desktop: Rect,
            overlays: &[Overlay<'_>],
            window_size: (u32, u32),
        ) -> Result<PresentOutcome> {
            ensure!(
                self.sdr,
                "NV12 surface presentation requires an SDR presenter"
            );
            ensure!(window_size.0 > 0 && window_size.1 > 0, "empty window");
            ensure!(overlays.len() <= 16, "too many UI overlays");
            for overlay in overlays {
                overlay.validate()?;
            }
            autoreleasepool(|_| self.present_surface_inner(sample, desktop, overlays, window_size))
        }

        fn present_surface_inner(
            &mut self,
            sample: &gstreamer::Sample,
            desktop: Rect,
            overlays: &[Overlay<'_>],
            window_size: (u32, u32),
        ) -> Result<PresentOutcome> {
            use gstreamer_video::{
                VideoChromaSite, VideoColorMatrix, VideoColorPrimaries, VideoColorRange,
                VideoFormat, VideoInfo, VideoTransferFunction,
            };
            let caps = sample.caps().context("native surface has no caps")?;
            let info = VideoInfo::from_caps(caps)?;
            crate::protocol::validate_video_size(info.width(), info.height())?;
            let color = info.colorimetry();
            ensure!(
                info.format() == VideoFormat::Nv12
                    && info.width().is_multiple_of(2)
                    && info.height().is_multiple_of(2),
                "native SDR surfaces must be even-sized NV12"
            );
            ensure!(
                color.matrix() == VideoColorMatrix::Bt709
                    && color.range() == VideoColorRange::Range16_235
                    && color.transfer() == VideoTransferFunction::Bt709
                    && color.primaries() == VideoColorPrimaries::Bt709,
                "native SDR surface requires limited-range BT.709 metadata"
            );
            let site = info.chroma_site();
            ensure!(
                site == VideoChromaSite::MPEG2 || site == VideoChromaSite::JPEG,
                "unsupported native NV12 chroma siting: {site:?}"
            );
            let pixel = pixel_buffer(sample)?;
            let pixel = pixel.as_ptr();
            unsafe {
                ensure!(
                    !CVPixelBufferGetIOSurface(pixel).is_null(),
                    "VideoToolbox pixel buffer has no IOSurface backing"
                );
                ensure!(
                    CVPixelBufferGetPixelFormatType(pixel) == u32::from_be_bytes(*b"420v"),
                    "Core Video surface is not limited-range NV12"
                );
                ensure!(
                    CVPixelBufferGetPlaneCount(pixel) == 2,
                    "Core Video surface must have two planes"
                );
                for plane in 0..2 {
                    let divisor = if plane == 0 { 1 } else { 2 };
                    ensure!(
                        CVPixelBufferGetWidthOfPlane(pixel, plane)
                            == info.width() as usize / divisor
                            && CVPixelBufferGetHeightOfPlane(pixel, plane)
                                == info.height() as usize / divisor,
                        "Core Video dimensions disagree with negotiated caps"
                    );
                }
            }
            if self.surface_cache.is_none() {
                let mut cache = std::ptr::null_mut();
                let status = unsafe {
                    CVMetalTextureCacheCreate(
                        std::ptr::null(),
                        std::ptr::null(),
                        Retained::as_ptr(&self.device).cast(),
                        std::ptr::null(),
                        &mut cache,
                    )
                };
                ensure!(
                    status == 0,
                    "creating Core Video Metal cache failed: {status}"
                );
                self.surface_cache = Some(CfOwned(
                    NonNull::new(cache).context("Core Video returned no texture cache")?,
                ));
            }
            let view: &AnyObject = unsafe { &*self.view.0.cast() };
            let _: () = unsafe { msg_send![view, updateDrawableSize] };
            let Some(drawable) = self.layer.nextDrawable() else {
                return Ok(PresentOutcome::DrawableUnavailable);
            };
            if self.surface_flights.len() >= 3 {
                let previous = self.surface_flights.pop_front().unwrap();
                previous.command.waitUntilCompleted();
                ensure!(
                    previous.command.status() != MTLCommandBufferStatus::Error,
                    "native surface GPU command failed: {:?}",
                    previous.command.error()
                );
            }
            let cache = self.surface_cache.as_ref().unwrap().0.as_ptr();
            let (y_owner, y) = import_plane(
                cache,
                pixel,
                MTLPixelFormat::R8Unorm,
                info.width() as usize,
                info.height() as usize,
                0,
            )?;
            let (uv_owner, uv) = import_plane(
                cache,
                pixel,
                MTLPixelFormat::RG8Unorm,
                info.width() as usize / 2,
                info.height() as usize / 2,
                1,
            )?;
            let pass = MTLRenderPassDescriptor::new();
            let attachment = unsafe { pass.colorAttachments().objectAtIndexedSubscript(0) };
            attachment.setTexture(Some(&drawable.texture()));
            attachment.setLoadAction(MTLLoadAction::Clear);
            attachment.setStoreAction(MTLStoreAction::Store);
            attachment.setClearColor(MTLClearColor {
                red: 0.0,
                green: 0.0,
                blue: 0.0,
                alpha: 1.0,
            });
            let command = self
                .queue
                .commandBuffer()
                .context("allocating surface command buffer")?;
            let encoder = command
                .renderCommandEncoderWithDescriptor(&pass)
                .context("creating surface render encoder")?;
            encoder.setRenderPipelineState(&self.video_pipeline);
            // MPEG-2 chroma is horizontally co-sited with the left luma sample.
            let offset = [
                if site == VideoChromaSite::MPEG2 {
                    0.5 / info.width() as f32
                } else {
                    0.0
                },
                0.0f32,
            ];
            unsafe {
                encoder.setFragmentTexture_atIndex(Some(&y), 0);
                encoder.setFragmentTexture_atIndex(Some(&uv), 1);
                encoder.setFragmentBytes_length_atIndex(
                    NonNull::from(&offset).cast(),
                    size_of_val(&offset),
                    0,
                );
            }
            draw_quad(&encoder, desktop, window_size);
            encoder.setRenderPipelineState(&self.ui_pipeline);
            for overlay in overlays {
                let texture =
                    self.texture(MTLPixelFormat::RGBA8Unorm, overlay.width, overlay.height)?;
                upload(
                    &texture,
                    overlay.rgba,
                    overlay.stride,
                    overlay.width,
                    overlay.height,
                );
                unsafe {
                    encoder.setFragmentTexture_atIndex(Some(&texture), 0);
                }
                draw_quad(&encoder, overlay.rect, window_size);
            }
            encoder.endEncoding();
            command.presentDrawable(ProtocolObject::from_ref(&*drawable));
            command.commit();
            self.surface_flights.push_back(SurfaceFlight {
                command,
                _sample: sample.clone(),
                _y: y_owner,
                _uv: uv_owner,
            });
            Ok(PresentOutcome::Presented { headroom: 1.0 })
        }

        fn texture(
            &self,
            format: MTLPixelFormat,
            width: u32,
            height: u32,
        ) -> Result<Retained<ProtocolObject<dyn MTLTexture>>> {
            let descriptor = unsafe {
                MTLTextureDescriptor::texture2DDescriptorWithPixelFormat_width_height_mipmapped(
                    format,
                    width as usize,
                    height as usize,
                    false,
                )
            };
            descriptor.setUsage(MTLTextureUsage::ShaderRead);
            // Shared storage supports CPU uploads on Apple Silicon; managed is
            // required for the equivalent CPU-visible path on discrete Intel GPUs.
            descriptor.setStorageMode(if self.device.hasUnifiedMemory() {
                MTLStorageMode::Shared
            } else {
                MTLStorageMode::Managed
            });
            self.device
                .newTextureWithDescriptor(&descriptor)
                .context("allocating HDR texture")
        }
    }

    impl Drop for HdrPresenter {
        fn drop(&mut self) {
            for frame in &self.surface_flights {
                frame.command.waitUntilCompleted();
            }
            for frame in &self.in_flight {
                frame.command.waitUntilCompleted();
            }
        }
    }

    fn pixel_buffer(sample: &gstreamer::Sample) -> Result<NonNull<c_void>> {
        #[repr(C)]
        struct CoreVideoMeta {
            meta: gstreamer::ffi::GstMeta,
            cvbuf: *mut c_void,
            pixbuf: *mut c_void,
        }
        let buffer = sample.buffer().context("native sample has no buffer")?;
        // GStreamer applemedia's GstCoreVideoMeta owns its CVPixelBuffer. The
        // caller retains this Sample until GPU completion, keeping it alive.
        unsafe {
            let api =
                gstreamer::glib::gobject_ffi::g_type_from_name(c"GstCoreVideoMetaAPI".as_ptr());
            ensure!(
                api != 0,
                "VideoToolbox native surface metadata is unavailable"
            );
            let meta = gstreamer::ffi::gst_buffer_get_meta(buffer.as_ptr() as *mut _, api);
            ensure!(
                !meta.is_null()
                    && !(*meta).info.is_null()
                    && (*(*meta).info).size == size_of::<CoreVideoMeta>(),
                "VideoToolbox buffer is missing compatible native surface metadata"
            );
            ensure!(
                (*(meta.cast::<CoreVideoMeta>())).cvbuf == (*(meta.cast::<CoreVideoMeta>())).pixbuf,
                "unexpected Core Video metadata ownership layout"
            );
            NonNull::new((*(meta.cast::<CoreVideoMeta>())).pixbuf)
                .context("VideoToolbox metadata has no pixel buffer")
        }
    }

    fn import_plane(
        cache: *mut c_void,
        pixel: *mut c_void,
        format: MTLPixelFormat,
        width: usize,
        height: usize,
        plane: usize,
    ) -> Result<(CfOwned, Retained<ProtocolObject<dyn MTLTexture>>)> {
        let mut output = std::ptr::null_mut();
        let status = unsafe {
            CVMetalTextureCacheCreateTextureFromImage(
                std::ptr::null(),
                cache,
                pixel,
                std::ptr::null(),
                format.0,
                width,
                height,
                plane,
                &mut output,
            )
        };
        ensure!(
            status == 0,
            "importing Core Video plane {plane} into Metal failed: {status}"
        );
        let owner = CfOwned(NonNull::new(output).context("Core Video returned no texture")?);
        let texture = unsafe { Retained::retain(CVMetalTextureGetTexture(owner.0.as_ptr())) }
            .context("Core Video texture has no Metal backing")?;
        Ok((owner, texture))
    }

    fn screen_headroom(view: sdl2::sys::SDL_MetalView) -> Result<f64> {
        let view: &AnyObject = unsafe { &*view.cast() };
        let window: Option<Retained<AnyObject>> = unsafe { msg_send![view, window] };
        let screen: Option<Retained<AnyObject>> = match window {
            Some(window) => unsafe { msg_send![&*window, screen] },
            None => None,
        };
        let screen = screen.context("HDR view is not attached to a display")?;
        let headroom: f64 =
            unsafe { msg_send![&*screen, maximumExtendedDynamicRangeColorComponentValue] };
        ensure!(
            headroom.is_finite() && headroom >= 1.0,
            "invalid display EDR headroom"
        );
        Ok(headroom)
    }

    fn make_pipeline(
        device: &ProtocolObject<dyn MTLDevice>,
        library: &ProtocolObject<dyn MTLLibrary>,
        fragment: &str,
        blend: bool,
    ) -> Result<Retained<ProtocolObject<dyn MTLRenderPipelineState>>> {
        let vertex = library
            .newFunctionWithName(&NSString::from_str("quad_vertex"))
            .context("missing HDR vertex shader")?;
        let fragment = library
            .newFunctionWithName(&NSString::from_str(fragment))
            .context("missing HDR fragment shader")?;
        let descriptor = MTLRenderPipelineDescriptor::new();
        descriptor.setVertexFunction(Some(&vertex));
        descriptor.setFragmentFunction(Some(&fragment));
        let attachment = unsafe { descriptor.colorAttachments().objectAtIndexedSubscript(0) };
        attachment.setPixelFormat(MTLPixelFormat::RGBA16Float);
        attachment.setBlendingEnabled(blend);
        if blend {
            attachment.setSourceRGBBlendFactor(MTLBlendFactor::SourceAlpha);
            attachment.setDestinationRGBBlendFactor(MTLBlendFactor::OneMinusSourceAlpha);
            attachment.setSourceAlphaBlendFactor(MTLBlendFactor::One);
            attachment.setDestinationAlphaBlendFactor(MTLBlendFactor::OneMinusSourceAlpha);
        }
        device
            .newRenderPipelineStateWithDescriptor_error(&descriptor)
            .map_err(|error| anyhow::anyhow!("creating HDR render pipeline: {error}"))
    }

    fn upload(
        texture: &ProtocolObject<dyn MTLTexture>,
        bytes: &[u8],
        stride: usize,
        width: u32,
        height: u32,
    ) {
        // Callers validated full plane bounds; Metal copies synchronously here.
        unsafe {
            texture.replaceRegion_mipmapLevel_withBytes_bytesPerRow(
                MTLRegion {
                    origin: MTLOrigin { x: 0, y: 0, z: 0 },
                    size: MTLSize {
                        width: width as usize,
                        height: height as usize,
                        depth: 1,
                    },
                },
                0,
                NonNull::new(bytes.as_ptr().cast_mut().cast::<c_void>()).unwrap(),
                stride,
            );
        }
    }

    fn draw_quad(
        encoder: &ProtocolObject<dyn MTLRenderCommandEncoder>,
        rect: Rect,
        window: (u32, u32),
    ) {
        // float4 maps the rectangle's top-left/bottom-right to NDC, with UV top-left.
        let bounds = [
            rect.x() as f32 / window.0 as f32 * 2.0 - 1.0,
            1.0 - rect.y() as f32 / window.1 as f32 * 2.0,
            rect.right() as f32 / window.0 as f32 * 2.0 - 1.0,
            1.0 - rect.bottom() as f32 / window.1 as f32 * 2.0,
        ];
        unsafe {
            encoder.setVertexBytes_length_atIndex(
                NonNull::from(&bounds).cast(),
                size_of_val(&bounds),
                0,
            );
            encoder.drawPrimitives_vertexStart_vertexCount(MTLPrimitiveType::TriangleStrip, 0, 4);
        }
    }

    const SHADER: &str = r#"
        #include <metal_stdlib>
        using namespace metal;
        struct Vertex { float4 position [[position]]; float2 uv; };
        vertex Vertex quad_vertex(uint i [[vertex_id]], constant float4 &bounds [[buffer(0)]]) {
            const float2 uv[] = {float2(0,0), float2(0,1), float2(1,0), float2(1,1)};
            Vertex v; v.uv = uv[i];
            v.position = float4(mix(bounds.xy, bounds.zw, uv[i]), 0.0, 1.0);
            return v;
        }
        float3 pq_to_edr(float3 signal) {
            float3 p = pow(clamp(signal, 0.0, 1.0), float3(1.0 / (2523.0 / 32.0)));
            float3 ratio = max(p - 3424.0 / 4096.0, 0.0) / (2413.0 / 128.0 - 2392.0 / 128.0 * p);
            return pow(ratio, float3(1.0 / (2610.0 / 16384.0))) * (10000.0 / 203.0);
        }
        fragment float4 pq_video(Vertex v [[stage_in]], texture2d<float> luma [[texture(0)]], texture2d<float> chroma [[texture(1)]]) {
            constexpr sampler sample_filter(coord::normalized, address::clamp_to_edge, filter::linear);
            float y = (luma.sample(sample_filter, v.uv).r * (65535.0 / 64.0) - 64.0) / 876.0;
            float2 uv = (chroma.sample(sample_filter, v.uv).rg * (65535.0 / 64.0) - 512.0) / 896.0;
            float3 rgb = float3(y + 1.4746 * uv.y, y - 0.1645531268 * uv.x - 0.5713531268 * uv.y, y + 1.8814 * uv.x);
            return float4(pq_to_edr(rgb), 1.0);
        }
        fragment float4 nv12_video(Vertex v [[stage_in]], texture2d<float> luma [[texture(0)]], texture2d<float> chroma [[texture(1)]], constant float2 &offset [[buffer(0)]]) {
            constexpr sampler sample_filter(coord::normalized, address::clamp_to_edge, filter::linear);
            float y = (luma.sample(sample_filter, v.uv).r * 255.0 - 16.0) / 219.0;
            float2 uv = (chroma.sample(sample_filter, v.uv + offset).rg * 255.0 - 128.0) / 224.0;
            float3 rgb = clamp(float3(y + 1.5748 * uv.y, y - 0.187324 * uv.x - 0.468124 * uv.y, y + 1.8556 * uv.x), 0.0, 1.0);
            float3 linear = select(pow((rgb + 0.099) / 1.099, float3(1.0 / 0.45)), rgb / 4.5, rgb < 0.081);
            float3 bt2020 = float3(dot(linear, float3(0.627404, 0.329283, 0.043313)), dot(linear, float3(0.069097, 0.919540, 0.011362)), dot(linear, float3(0.016391, 0.088013, 0.895595)));
            return float4(bt2020, 1.0);
        }
        fragment float4 sdr_ui(Vertex v [[stage_in]], texture2d<float> image [[texture(0)]]) {
            constexpr sampler sample_filter(coord::normalized, address::clamp_to_edge, filter::linear);
            float4 rgba = image.sample(sample_filter, v.uv);
            float3 linear = select(pow((rgba.rgb + 0.055) / 1.055, float3(2.4)), rgba.rgb / 12.92, rgba.rgb <= 0.04045);
            float3 bt2020 = float3(dot(linear, float3(0.627404, 0.329283, 0.043313)),
                dot(linear, float3(0.069097, 0.919540, 0.011362)),
                dot(linear, float3(0.016391, 0.088013, 0.895595)));
            return float4(bt2020, rgba.a);
        }
    "#;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pq_reference_luminances_preserve_values_above_sdr() {
        assert!(pq_to_linear_edr(0.0).abs() < 1e-9);
        assert!((pq_to_linear_edr(1.0) - 10000.0 / 203.0).abs() < 1e-6);
        assert!((pq_to_linear_edr(0.580688881) - 1.0).abs() < 1e-6);
        assert!((pq_to_linear_edr(0.751827096) - 1000.0 / 203.0).abs() < 1e-5);
    }

    #[test]
    fn plane_bounds_allow_padding_but_reject_truncation_and_overflow() {
        assert!(validate_plane(&[0; 12], 8, 4, 2).is_ok());
        assert!(validate_plane(&[0; 11], 8, 4, 2).is_err());
        assert!(validate_plane(&[], usize::MAX, 4, 3).is_err());
        assert!(validate_plane(&[0; 12], 2, 4, 2).is_err());
    }

    #[test]
    fn p010_requires_even_dimensions_and_pixel_aligned_strides() {
        let mut frame = P010Frame {
            width: 2,
            height: 2,
            y: &[0; 8],
            uv: &[0; 4],
            y_stride: 4,
            uv_stride: 4,
        };
        assert!(frame.validate().is_ok());
        frame.uv_stride = 5;
        assert!(frame.validate().is_err());
        frame.uv_stride = 4;
        frame.width = 3;
        assert!(frame.validate().is_err());
    }
}
