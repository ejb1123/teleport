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
        edr_started: Option<Instant>,
        has_presented_hdr: bool,
        // A cloned SDL handle ensures the window outlives the attached view.
        _window: sdl2::video::Window,
    }

    impl HdrPresenter {
        pub fn new(window: &sdl2::video::Window) -> Result<Self> {
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
                layer.setWantsExtendedDynamicRangeContent(true);
                let colorspace =
                    CGColorSpace::with_name(Some(unsafe { kCGColorSpaceExtendedLinearITUR_2020 }))
                        .context("extended linear BT.2020 color space unavailable")?;
                layer.setColorspace(Some(&colorspace));
                let headroom = screen_headroom(view.0)?;
                tracing::info!(
                    headroom,
                    sdr_white_nits = SDR_WHITE_NITS,
                    "requested floating-point BT.2020 EDR layer; waiting for display headroom"
                );
                let library = device
                    .newLibraryWithSource_options_error(&NSString::from_str(SHADER), None)
                    .map_err(|error| anyhow::anyhow!("compiling HDR Metal shaders: {error}"))?;
                let video_pipeline = make_pipeline(&device, &library, "pq_video", false)?;
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
            for frame in &self.in_flight {
                frame.command.waitUntilCompleted();
            }
        }
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
