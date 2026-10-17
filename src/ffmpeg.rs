//! Direct libavcodec/libavutil/libswscale calls through a small opaque C ABI.
use anyhow::{Context, Result, ensure};
use std::{
    ffi::{CStr, CString, c_char, c_int, c_void},
    ptr::NonNull,
};

#[repr(C)]
struct Output {
    data: *const u8,
    size: c_int,
    width: c_int,
    height: c_int,
    key: c_int,
    pts: i64,
    gpu: *mut c_void,
}
impl Default for Output {
    fn default() -> Self {
        Self {
            data: std::ptr::null(),
            size: 0,
            width: 0,
            height: 0,
            key: 0,
            pts: 0,
            gpu: std::ptr::null_mut(),
        }
    }
}
unsafe extern "C" {
    fn tp_codec_open(
        encoder: c_int,
        name: *const c_char,
        hevc: c_int,
        hdr: c_int,
        width: c_int,
        height: c_int,
        fps: c_int,
        kbps: c_int,
        output: c_int,
        gop: c_int,
        gpu_input: c_int,
        refresh: c_int,
        error: *mut c_int,
    ) -> *mut c_void;
    fn tp_codec_free(codec: *mut c_void);
    fn tp_send_packet(codec: *mut c_void, data: *const u8, size: c_int, pts: i64) -> c_int;
    fn tp_send_frame(
        codec: *mut c_void,
        data: *const u8,
        size: c_int,
        pts: i64,
        force_key: c_int,
    ) -> c_int;
    fn tp_receive_packet(codec: *mut c_void, output: *mut Output) -> c_int;
    fn tp_receive_frame(codec: *mut c_void, output: *mut Output) -> c_int;
    fn tp_codec_flush(codec: *mut c_void);
    fn tp_gpu_frame_free(frame: *mut c_void);
    fn tp_codec_set_bitrate(codec: *mut c_void, kbps: c_int) -> c_int;
    fn tp_codec_error(error: c_int, buffer: *mut c_char, length: c_int);
}

#[derive(Clone)]
pub struct Config {
    pub encoder: bool,
    pub name: String,
    pub hevc: bool,
    pub hdr: bool,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub kbps: u32,
    /// 0 = RGB24, 1 = NV12, 2 = P010LE, 3 = GPU frame (CUDA or VA-API).
    pub output: i32,
    /// Encoder keyframe interval in frames; 0 keeps the fps/4 default.
    pub gop: u32,
    /// NVENC / VA-API encoders: also accept captured DMA-BUFs
    /// ([`Codec::submit_dmabuf`]). NVENC then takes CUDA frames.
    pub gpu_input: bool,
    /// Encoders: periodic intra refresh over this many frames instead of
    /// periodic IDRs (libx264, h264_nvenc; 0 = off). Requested keyframes
    /// stay IDRs.
    pub intra_refresh: u32,
}

/// A captured DMA-BUF frame (see `src/dmabuf_encode.h`). The descriptors are
/// borrowed: they must stay open for the duration of the call using them.
#[cfg(target_os = "linux")]
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct Dmabuf {
    pub fds: [i32; 4],
    pub offsets: [u32; 4],
    pub pitches: [u32; 4],
    pub planes: i32,
    pub fourcc: u32,
    pub modifier: u64,
    pub width: i32,
    pub height: i32,
    pub crop_x: i32,
    pub crop_y: i32,
    pub crop_width: i32,
    pub crop_height: i32,
}

#[cfg(target_os = "linux")]
unsafe extern "C" {
    fn tp_codec_dmabuf_modifiers(
        codec: *mut c_void,
        fourcc: u32,
        modifiers: *mut u64,
        capacity: c_int,
    ) -> c_int;
    fn tp_send_dmabuf(
        codec: *mut c_void,
        source: *const Dmabuf,
        pts: i64,
        force_key: c_int,
    ) -> c_int;
}
pub struct Codec {
    pointer: NonNull<c_void>,
    pub config: Config,
}
// The native instance has no thread affinity and is never concurrently entered:
// all operations require &mut self, and shared users hold a Mutex<Codec>.
unsafe impl Send for Codec {}
impl Drop for Codec {
    fn drop(&mut self) {
        unsafe {
            tp_codec_free(self.pointer.as_ptr());
        }
    }
}

fn check(code: i32) -> Result<i32> {
    if code >= 0 {
        return Ok(code);
    }
    let mut message = [0 as c_char; 256];
    unsafe {
        tp_codec_error(code, message.as_mut_ptr(), message.len() as i32);
    }
    anyhow::bail!(
        "FFmpeg: {} ({code})",
        unsafe { CStr::from_ptr(message.as_ptr()) }.to_string_lossy()
    )
}

pub struct Frame {
    pub bytes: Vec<u8>,
    pub width: u32,
    pub height: u32,
    pub key: bool,
    pub pts: Option<u64>,
    /// Output 3: the decoded CUDA or VA-API frame itself (no download).
    pub gpu: Option<GpuFrame>,
}
impl Codec {
    pub fn new(config: Config) -> Result<Self> {
        let name = CString::new(config.name.as_str())?;
        let mut error = 0;
        let pointer = unsafe {
            tp_codec_open(
                config.encoder.into(),
                name.as_ptr(),
                config.hevc.into(),
                config.hdr.into(),
                config.width.try_into()?,
                config.height.try_into()?,
                config.fps.try_into()?,
                config.kbps.try_into()?,
                config.output,
                config.gop.try_into()?,
                config.gpu_input.into(),
                config.intra_refresh.try_into()?,
                &mut error,
            )
        };
        check(error).with_context(|| format!("opening {}", config.name))?;
        Ok(Self {
            pointer: NonNull::new(pointer).context("FFmpeg returned no context")?,
            config,
        })
    }
    pub fn submit(&mut self, bytes: &[u8], pts: u64) -> Result<()> {
        self.submit_frame(bytes, pts, false)
    }
    /// Encoders: `force_key` makes this frame an IDR starting a new closed GOP.
    pub fn submit_frame(&mut self, bytes: &[u8], pts: u64, force_key: bool) -> Result<()> {
        let size = bytes.len().try_into()?;
        let result = unsafe {
            if self.config.encoder {
                tp_send_frame(
                    self.pointer.as_ptr(),
                    bytes.as_ptr(),
                    size,
                    pts.try_into()?,
                    force_key.into(),
                )
            } else {
                tp_send_packet(self.pointer.as_ptr(), bytes.as_ptr(), size, pts.try_into()?)
            }
        };
        check(result)?;
        Ok(())
    }
    /// Modifiers of DRM `fourcc` this encoder can take as DMA-BUFs; empty
    /// unless it was opened with `gpu_input` on a GPU that can import them.
    #[cfg(target_os = "linux")]
    pub fn dmabuf_modifiers(&mut self, fourcc: u32) -> Result<Vec<u64>> {
        let mut modifiers = [0_u64; 64];
        let count = check(unsafe {
            tp_codec_dmabuf_modifiers(
                self.pointer.as_ptr(),
                fourcc,
                modifiers.as_mut_ptr(),
                modifiers.len() as c_int,
            )
        })?;
        Ok(modifiers[..count as usize].to_vec())
    }
    /// Encode a captured DMA-BUF, converted on the GPU (no CPU copy). The
    /// buffer may be reused by its producer once this returns.
    #[cfg(target_os = "linux")]
    pub fn submit_dmabuf(&mut self, source: &Dmabuf, pts: u64, force_key: bool) -> Result<()> {
        check(unsafe {
            tp_send_dmabuf(
                self.pointer.as_ptr(),
                source,
                pts.try_into()?,
                force_key.into(),
            )
        })?;
        Ok(())
    }
    pub fn receive(&mut self) -> Result<Option<Frame>> {
        let mut output = Output::default();
        let result = unsafe {
            if self.config.encoder {
                tp_receive_packet(self.pointer.as_ptr(), &mut output)
            } else {
                tp_receive_frame(self.pointer.as_ptr(), &mut output)
            }
        };
        if check(result)? == 0 {
            return Ok(None);
        }
        if let Some(gpu) = NonNull::new(output.gpu) {
            let gpu = GpuFrame(gpu);
            crate::protocol::validate_video_size(
                output.width.try_into()?,
                output.height.try_into()?,
            )?;
            return Ok(Some(Frame {
                bytes: Vec::new(),
                width: output.width as u32,
                height: output.height as u32,
                key: false,
                pts: output.pts.try_into().ok(),
                gpu: Some(gpu),
            }));
        }
        ensure!(
            !output.data.is_null() && output.size > 0 && output.size <= 128 * 1024 * 1024,
            "invalid FFmpeg output allocation"
        );
        if !self.config.encoder {
            crate::protocol::validate_video_size(
                output.width.try_into()?,
                output.height.try_into()?,
            )?;
        }
        let bytes =
            unsafe { std::slice::from_raw_parts(output.data, output.size as usize) }.to_vec();
        Ok(Some(Frame {
            bytes,
            width: output.width as u32,
            height: output.height as u32,
            key: output.key != 0,
            pts: output.pts.try_into().ok(),
            gpu: None,
        }))
    }
    /// Change an open encoder's bitrate without reopening it. `Ok(false)` means
    /// this encoder cannot, and the caller must reopen to apply the change.
    pub fn set_bitrate_in_place(&mut self, kbps: u32) -> Result<bool> {
        let result = unsafe { tp_codec_set_bitrate(self.pointer.as_ptr(), kbps.try_into()?) };
        if result == -libc::ENOSYS {
            return Ok(false);
        }
        check(result)?;
        self.config.kbps = kbps;
        Ok(true)
    }
    pub fn flush(&mut self) {
        unsafe {
            tp_codec_flush(self.pointer.as_ptr());
        }
    }
}

/// Test DMA-BUF: an RGBA texture with four solid quadrants exported from an
/// EGL device (render `node`, or the NVIDIA device for `None`).
#[cfg(all(test, target_os = "linux"))]
pub(crate) struct TestDmabuf {
    pointer: NonNull<c_void>,
    pub frame: Dmabuf,
}

#[cfg(all(test, target_os = "linux"))]
unsafe extern "C" {
    fn tp_test_dmabuf_new(
        node: *const c_char,
        width: c_int,
        height: c_int,
        colors: *const u8,
        out: *mut Dmabuf,
        error: *mut c_int,
    ) -> *mut c_void;
    fn tp_test_dmabuf_free(source: *mut c_void);
}

#[cfg(all(test, target_os = "linux"))]
impl TestDmabuf {
    /// `None` when no such EGL device can export DMA-BUFs.
    pub fn new(
        node: Option<&str>,
        width: u32,
        height: u32,
        colors: [[u8; 3]; 4],
    ) -> Result<Option<Self>> {
        let node = node.map(CString::new).transpose()?;
        let mut frame = Dmabuf::default();
        let mut error = 0;
        let pointer = unsafe {
            tp_test_dmabuf_new(
                node.as_ref().map_or(std::ptr::null(), |node| node.as_ptr()),
                width.try_into()?,
                height.try_into()?,
                colors.as_flattened().as_ptr(),
                &mut frame,
                &mut error,
            )
        };
        if error == -libc::ENODEV {
            return Ok(None);
        }
        check(error).context("exporting a test DMA-BUF")?;
        Ok(Some(Self {
            pointer: NonNull::new(pointer).context("no test DMA-BUF")?,
            frame,
        }))
    }
}

#[cfg(all(test, target_os = "linux"))]
impl Drop for TestDmabuf {
    fn drop(&mut self) {
        unsafe { tp_test_dmabuf_free(self.pointer.as_ptr()) }
    }
}

// Safety: the exported descriptors and the EGL objects behind them are not
// tied to the creating thread (its context is released after creation).
#[cfg(all(test, target_os = "linux"))]
unsafe impl Send for TestDmabuf {}
#[cfg(all(test, target_os = "linux"))]
unsafe impl Sync for TestDmabuf {}

/// A reference to a decoded CUDA or VA-API frame kept on the GPU (decoder
/// output 3). Holding it keeps one decoder surface alive; dropping it
/// releases it.
pub struct GpuFrame(NonNull<c_void>);

// Safety: an AVFrame reference is reference-counted and may be released or
// read (with its CUDA context pushed, or via a VA-API export) from any thread.
unsafe impl Send for GpuFrame {}

impl Drop for GpuFrame {
    fn drop(&mut self) {
        unsafe { tp_gpu_frame_free(self.0.as_ptr()) }
    }
}

#[cfg(target_os = "linux")]
type GetProc = unsafe extern "C" fn(*const c_char) -> *mut c_void;

#[cfg(target_os = "linux")]
unsafe extern "C" {
    fn tp_cuda_gl_new(get: GetProc) -> *mut c_void;
    fn tp_cuda_gl_draw(
        presenter: *mut c_void,
        frame: *const c_void,
        x: c_int,
        y: c_int,
        width: c_int,
        height: c_int,
        framebuffer_height: c_int,
    ) -> c_int;
    fn tp_cuda_gl_free(presenter: *mut c_void);
    fn tp_vaapi_egl_new(get: GetProc) -> *mut c_void;
    fn tp_vaapi_egl_render_node(presenter: *const c_void) -> *const c_char;
    fn tp_vaapi_egl_draw(
        presenter: *mut c_void,
        frame: *const c_void,
        x: c_int,
        y: c_int,
        width: c_int,
        height: c_int,
        framebuffer_height: c_int,
    ) -> c_int;
    fn tp_vaapi_egl_free(presenter: *mut c_void);
    #[cfg(test)]
    fn tp_cuda_gl_self_test(frame: *const c_void, rgba: *mut u8) -> c_int;
    #[cfg(test)]
    fn tp_vaapi_egl_self_test(
        node: *const c_char,
        frame: *const c_void,
        rgba: *mut u8,
        iterations: c_int,
        draw_ns: *mut i64,
        download_ns: *mut i64,
    ) -> c_int;
}

/// How a [`GlPresenter`] gets decoded frames into GL without a CPU copy.
#[cfg(target_os = "linux")]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GpuInterop {
    /// NVIDIA: CUDA copies the planes into registered GL textures.
    Cuda,
    /// Intel/AMD: VA-API surfaces exported as dma-bufs and imported as
    /// EGLImages (EGL contexts only).
    Vaapi,
}

/// Zero-copy presenter for [`GpuFrame`]s in the current OpenGL context: the
/// frame never leaves the GPU and one shader converts limited-range BT.709
/// NV12 while drawing. Must be used on the thread that owns the GL context
/// it was created in.
#[cfg(target_os = "linux")]
pub struct GlPresenter {
    pointer: NonNull<c_void>,
    interop: GpuInterop,
    render_node: Option<String>,
}

#[cfg(target_os = "linux")]
impl GlPresenter {
    /// CUDA interop. `None` unless the current context is NVIDIA GL with
    /// GL 2.1 shaders and libcuda loads.
    pub fn cuda(get: GetProc) -> Option<Self> {
        NonNull::new(unsafe { tp_cuda_gl_new(get) }).map(|pointer| Self {
            pointer,
            interop: GpuInterop::Cuda,
            render_node: None,
        })
    }

    /// DMA-BUF/EGLImage import. `None` unless the current context is EGL
    /// (not GLX) on a DRM device with dma-buf import and GL_OES_EGL_image.
    pub fn vaapi(get: GetProc) -> Option<Self> {
        let pointer = NonNull::new(unsafe { tp_vaapi_egl_new(get) })?;
        let node = unsafe { tp_vaapi_egl_render_node(pointer.as_ptr()) };
        let render_node = (!node.is_null()).then(|| {
            unsafe { CStr::from_ptr(node) }
                .to_string_lossy()
                .into_owned()
        });
        Some(Self {
            pointer,
            interop: GpuInterop::Vaapi,
            render_node,
        })
    }

    pub fn interop(&self) -> GpuInterop {
        self.interop
    }

    /// The decoder that produces frames this presenter can draw: CUDA, or
    /// VA-API pinned to the EGL display's own render node (same GPU).
    pub fn decoder_name(&self) -> String {
        match (self.interop, &self.render_node) {
            (GpuInterop::Vaapi, Some(node)) => format!("vaapi@{node}"),
            (GpuInterop::Vaapi, None) => "vaapi".into(),
            (GpuInterop::Cuda, _) => "cuda".into(),
        }
    }

    /// Draw at `rect` (framebuffer pixels, top-left origin). GL state is
    /// restored afterwards.
    pub fn draw(
        &mut self,
        frame: &GpuFrame,
        rect: (i32, i32, u32, u32),
        framebuffer_height: u32,
    ) -> Result<()> {
        let draw = match self.interop {
            GpuInterop::Cuda => tp_cuda_gl_draw,
            GpuInterop::Vaapi => tp_vaapi_egl_draw,
        };
        let result = unsafe {
            draw(
                self.pointer.as_ptr(),
                frame.0.as_ptr(),
                rect.0,
                rect.1,
                rect.2.try_into()?,
                rect.3.try_into()?,
                framebuffer_height.try_into()?,
            )
        };
        check(result).with_context(|| format!("zero-copy {:?}/GL presentation", self.interop))?;
        Ok(())
    }
}

#[cfg(target_os = "linux")]
impl Drop for GlPresenter {
    fn drop(&mut self) {
        unsafe {
            match self.interop {
                GpuInterop::Cuda => tp_cuda_gl_free(self.pointer.as_ptr()),
                GpuInterop::Vaapi => tp_vaapi_egl_free(self.pointer.as_ptr()),
            }
        }
    }
}

pub fn enabled() -> Result<bool> {
    match std::env::var("TELEPORT_VIDEO_BACKEND").as_deref() {
        Ok("ffmpeg") => Ok(true),
        Ok("gstreamer") => Ok(false),
        Err(std::env::VarError::NotPresent) => Ok(true),
        _ => anyhow::bail!("TELEPORT_VIDEO_BACKEND must be ffmpeg or gstreamer"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    pub(super) fn software_config(hevc: bool) -> Config {
        Config {
            encoder: true,
            name: if hevc { "libx265" } else { "libx264" }.into(),
            hevc,
            hdr: false,
            width: 64,
            height: 48,
            fps: 30,
            kbps: 1000,
            output: 1,
            gop: 0,
            gpu_input: false,
            intra_refresh: 0,
        }
    }
    #[test]
    fn direct_library_roundtrip_and_flush_both_codecs() -> Result<()> {
        for hevc in [false, true] {
            let config = software_config(hevc);
            let mut encoder = Codec::new(config.clone())?;
            let mut decoder = Codec::new(Config {
                encoder: false,
                name: "software".into(),
                ..config
            })?;
            let mut raw = vec![90; 64 * 48];
            raw.extend(vec![128; 64 * 48 / 2]);
            let mut decoded = 0;
            for sequence in 0..24 {
                encoder.submit(&raw, sequence)?;
                while let Some(packet) = encoder.receive()? {
                    if sequence == 14 {
                        decoder.flush();
                    }
                    decoder.submit(&packet.bytes, sequence + 100)?;
                    while let Some(frame) = decoder.receive()? {
                        ensure!((frame.width, frame.height) == (64, 48), "wrong dimensions");
                        ensure!(frame.bytes.len() == raw.len(), "wrong NV12 layout");
                        ensure!(frame.pts == Some(sequence + 100), "lost frame identity");
                        ensure!((frame.bytes[100] as i32 - 90).abs() < 12, "corrupt output");
                        decoded += 1;
                    }
                }
            }
            ensure!(decoded >= 20, "insufficient decoded frames: {decoded}");
        }
        Ok(())
    }
    #[test]
    fn forced_keyframe_starts_new_gop_mid_interval() -> Result<()> {
        for hevc in [false, true] {
            // 30 fps gives a 7-frame GOP; frame 3 is mid-interval.
            let mut encoder = Codec::new(software_config(hevc))?;
            let mut raw = vec![90; 64 * 48];
            raw.extend(vec![128; 64 * 48 / 2]);
            let mut keys = Vec::new();
            for sequence in 0..6 {
                raw[sequence as usize] = 200; // vary content so frames are not skipped
                encoder.submit_frame(&raw, sequence, sequence == 3)?;
                while let Some(packet) = encoder.receive()? {
                    if packet.key {
                        keys.push(packet.pts);
                    }
                }
            }
            ensure!(
                keys == [Some(0), Some(3)],
                "hevc={hevc}: expected keyframes at 0 and 3, got {keys:?}"
            );
        }
        Ok(())
    }
    /// G7: with intra refresh a frame lost after the start heals within the
    /// refresh period, without an IDR; without it the error persists until
    /// the next keyframe. Requested keyframes stay IDRs. NVENC is included
    /// with TELEPORT_TEST_HARDWARE=1.
    #[test]
    fn intra_refresh_heals_a_lost_frame_without_an_idr() -> Result<()> {
        // H.264 only: FFmpeg's H.265 decoder drops every picture whose
        // reference is missing ("Could not find ref with POC"), so an H.265
        // stream cannot be decoded through a gap at all.
        let mut names = vec![("libx264", false)];
        if std::env::var("TELEPORT_TEST_HARDWARE").as_deref() == Ok("1") {
            names.push(("h264_nvenc", false));
        }
        let (width, height) = (320_usize, 240_usize);
        // Diagonal stripes moving every frame: a wrong reference stays wrong.
        let source = |frame: usize| -> Vec<u8> {
            let mut raw: Vec<u8> = (0..width * height)
                .map(|i| (((i % width) + (i / width) * 2 + frame * 3) % 256) as u8)
                .collect();
            raw.extend(vec![128; width * height / 2]);
            raw
        };
        for (name, hevc) in names {
            // (refresh, mean luma error over frames 70-89)
            let mut errors = Vec::new();
            for refresh in [30, 0] {
                let config = Config {
                    name: name.into(),
                    width: width as u32,
                    height: height as u32,
                    kbps: 2000,
                    gop: 3000,
                    intra_refresh: refresh,
                    ..software_config(hevc)
                };
                let mut encoder = Codec::new(config.clone())
                    .with_context(|| format!("{name} refresh={refresh}"))?;
                let mut decoder = Codec::new(Config {
                    encoder: false,
                    name: "software".into(),
                    ..config
                })?;
                let codec = if hevc {
                    crate::protocol::VideoCodec::H265
                } else {
                    crate::protocol::VideoCodec::H264
                };
                let (mut idrs, mut error, mut counted) = (Vec::new(), 0_u64, 0_u64);
                for frame in 0..100_usize {
                    encoder.submit_frame(&source(frame), frame as u64, frame == 90)?;
                    while let Some(packet) = encoder.receive()? {
                        let pts = packet.pts.unwrap_or_default() as usize;
                        if crate::protocol::annexb_keyframe(codec, &packet.bytes) {
                            idrs.push(pts);
                        }
                        // Lost in transit.
                        if pts == 20 {
                            continue;
                        }
                        let _ = decoder.submit(&packet.bytes, pts as u64);
                        while let Ok(Some(decoded)) = decoder.receive() {
                            let at = decoded.pts.unwrap_or_default() as usize;
                            if (70..90).contains(&at) {
                                let expected = source(at);
                                error += decoded.bytes[..width * height]
                                    .iter()
                                    .zip(&expected)
                                    .map(|(a, b)| u64::from(a.abs_diff(*b)))
                                    .sum::<u64>()
                                    / (width * height) as u64;
                                counted += 1;
                            }
                        }
                    }
                }
                ensure!(
                    idrs == [0, 90],
                    "{name} refresh={refresh}: IDRs only at the start and when forced, got {idrs:?}"
                );
                ensure!(counted >= 8, "{name} refresh={refresh}: decoded {counted}");
                errors.push(error / counted);
            }
            ensure!(
                errors[0] <= 3 && errors[1] > errors[0],
                "{name}: mean luma error with/without refresh {errors:?}"
            );
        }
        Ok(())
    }
    #[test]
    fn x264_bitrate_changes_in_place_and_x265_requires_reopen() -> Result<()> {
        let config = Config {
            width: 320,
            height: 240,
            kbps: 4000,
            ..software_config(false)
        };
        let mut encoder = Codec::new(config)?;
        // Pseudo-random content so the rate controller, not the content, limits size.
        let mut seed = 0x2545_f491_u32;
        let mut encode = |encoder: &mut Codec, from: u64| -> Result<usize> {
            let mut bytes = 0;
            for pts in from..from + 30 {
                let raw: Vec<u8> = (0..320 * 240 * 3 / 2)
                    .map(|_| {
                        seed ^= seed << 13;
                        seed ^= seed >> 17;
                        seed ^= seed << 5;
                        seed as u8
                    })
                    .collect();
                encoder.submit(&raw, pts)?;
                while let Some(packet) = encoder.receive()? {
                    bytes += packet.bytes.len();
                }
            }
            Ok(bytes)
        };
        let high = encode(&mut encoder, 0)?;
        ensure!(
            encoder.set_bitrate_in_place(300)?,
            "libx264 must reconfigure in place"
        );
        ensure!(encoder.config.kbps == 300);
        let low = encode(&mut encoder, 30)?;
        ensure!(
            low * 3 < high,
            "in-place bitrate change had no effect: {high} -> {low} bytes"
        );
        let mut hevc = Codec::new(software_config(true))?;
        ensure!(
            !hevc.set_bitrate_in_place(300)?,
            "libx265 must report that it needs a reopen"
        );
        ensure!(hevc.config.kbps == 1000);
        Ok(())
    }
    #[test]
    fn invalid_configuration_and_short_input_are_rejected() -> Result<()> {
        let config = software_config(false);
        ensure!(
            Codec::new(Config {
                width: 0,
                ..config.clone()
            })
            .is_err()
        );
        ensure!(
            Codec::new(Config {
                name: "not_a_codec".into(),
                ..config.clone()
            })
            .is_err()
        );
        let mut encoder = Codec::new(config)?;
        ensure!(encoder.submit(&[1; 4], 0).is_err());
        Ok(())
    }

    #[test]
    fn hdr_ten_bit_roundtrip_and_sdr_rejection() -> Result<()> {
        let config = Config {
            hdr: true,
            output: 2,
            ..software_config(true)
        };
        let mut encoder = Codec::new(config.clone())?;
        let mut decoder = Codec::new(Config {
            encoder: false,
            name: "software".into(),
            ..config
        })?;
        let raw: Vec<u8> = (0..64 * 48 * 3 / 2)
            .flat_map(|index| {
                let value = if index < 64 * 48 { 257_u16 } else { 512_u16 };
                (value << 6).to_le_bytes()
            })
            .collect();
        encoder.submit(&raw, 0)?;
        let packet = encoder.receive()?.context("no HDR packet")?;
        decoder.submit(&packet.bytes, 123)?;
        let frame = decoder.receive()?.context("no HDR frame")?;
        ensure!(frame.bytes.len() == raw.len() && frame.pts == Some(123));
        let luma = u16::from_le_bytes([frame.bytes[0], frame.bytes[1]]) >> 6;
        ensure!(
            (luma as i32 - 257).abs() <= 4,
            "ten-bit luma was lost: {luma}"
        );
        let mut sdr_encoder = Codec::new(software_config(true))?;
        let mut sdr = vec![90; 64 * 48];
        sdr.extend(vec![128; 64 * 48 / 2]);
        sdr_encoder.submit(&sdr, 0)?;
        let sdr = sdr_encoder.receive()?.context("no SDR packet")?;
        decoder.flush();
        decoder.submit(&sdr.bytes, 124)?;
        ensure!(decoder.receive().is_err(), "SDR must not be mislabeled HDR");
        Ok(())
    }

    #[test]
    fn device_suffix_is_validated() {
        for name in ["vaapi@", "software@x", "@/dev/dri/renderD128"] {
            let config = Config {
                encoder: false,
                name: name.into(),
                ..software_config(false)
            };
            assert!(Codec::new(config).is_err(), "{name} must be rejected");
        }
    }

    /// Four flat quadrants (limited-range NV12): grey, red-ish, green-ish,
    /// blue-ish, encoded with libx264 (or libx265). Returns the config and packets.
    #[cfg(target_os = "linux")]
    fn quadrant_stream(width: usize, height: usize, hevc: bool) -> Result<(Config, Vec<Vec<u8>>)> {
        let quadrants = [
            (126_u8, 128_u8, 128_u8),
            (81, 90, 240),
            (145, 54, 34),
            (41, 240, 110),
        ];
        let quadrant = |x: usize, y: usize| {
            quadrants[(y >= height / 2) as usize * 2 + (x >= width / 2) as usize]
        };
        let mut raw = vec![0_u8; width * height * 3 / 2];
        for y in 0..height {
            for x in 0..width {
                raw[y * width + x] = quadrant(x, y).0;
            }
        }
        for y in 0..height / 2 {
            for x in 0..width / 2 {
                let (_, u, v) = quadrant(x * 2, y * 2);
                raw[width * height + y * width + x * 2] = u;
                raw[width * height + y * width + x * 2 + 1] = v;
            }
        }
        let config = Config {
            width: width as u32,
            height: height as u32,
            kbps: 4000,
            ..software_config(hevc)
        };
        let mut encoder = Codec::new(config.clone())?;
        let mut packets = Vec::new();
        for index in 0..3 {
            encoder.submit(&raw, index)?;
            while let Some(packet) = encoder.receive()? {
                packets.push(packet.bytes);
            }
        }
        Ok((config, packets))
    }

    #[cfg(target_os = "linux")]
    fn decode_last(config: &Config, packets: &[Vec<u8>], name: &str, output: i32) -> Result<Frame> {
        let mut decoder = Codec::new(Config {
            encoder: false,
            name: name.into(),
            output,
            ..config.clone()
        })?;
        let mut last = None;
        for (index, packet) in packets.iter().enumerate() {
            decoder.submit(packet, index as u64)?;
            while let Some(frame) = decoder.receive()? {
                last = Some(frame);
            }
        }
        last.context("no decoded frame")
    }

    /// Compare quadrant centres of a bottom-up RGBA readback with the CPU
    /// RGB24 decode, within ±4 per channel.
    #[cfg(target_os = "linux")]
    fn compare_quadrants(cpu: &Frame, rgba: &[u8], width: usize, height: usize) -> Result<()> {
        for (cx, cy) in [
            (width / 4, height / 4),
            (width * 3 / 4, height / 4),
            (width / 4, height * 3 / 4),
            (width * 3 / 4, height * 3 / 4),
        ] {
            let expected = &cpu.bytes[(cy * width + cx) * 3..][..3];
            let actual = &rgba[((height - 1 - cy) * width + cx) * 4..][..3];
            for channel in 0..3 {
                ensure!(
                    (expected[channel] as i32 - actual[channel] as i32).abs() <= 4,
                    "pixel ({cx},{cy}): GPU {actual:?} vs CPU {expected:?}"
                );
            }
        }
        Ok(())
    }

    #[test]
    #[ignore = "requires hardware encoder and at least one hardware decoder"]
    fn zero_copy_cuda_gl_presentation_matches_cpu_decode() -> Result<()> {
        if std::env::var("TELEPORT_TEST_HARDWARE").as_deref() != Ok("1") {
            eprintln!("SKIP: set TELEPORT_TEST_HARDWARE=1 on a machine with NVIDIA CUDA and EGL");
            return Ok(());
        }
        let (width, height) = (640_usize, 360_usize);
        // H.264 and H.265 (the automatic choice with hardware on both ends).
        for hevc in [false, true] {
            let (config, packets) = quadrant_stream(width, height, hevc)?;
            let cpu = decode_last(&config, &packets, "software", 0)?; // RGB24 reference
            let gpu = decode_last(&config, &packets, "cuda", 3)?;
            let gpu = gpu
                .gpu
                .context("CUDA decoder did not keep the frame on the GPU")?;
            let mut rgba = vec![0_u8; width * height * 4];
            let result = unsafe { tp_cuda_gl_self_test(gpu.0.as_ptr(), rgba.as_mut_ptr()) };
            if result == -libc::ENODEV {
                eprintln!("SKIP: no NVIDIA EGL device");
                return Ok(());
            }
            check(result)?;
            compare_quadrants(&cpu, &rgba, width, height)
                .with_context(|| format!("hevc={hevc}"))?;
            eprintln!("PASS cuda hevc={hevc}");
        }
        Ok(())
    }

    /// VA-API zero-copy: decode on each DRM render node with a VA driver,
    /// export the surface as dma-bufs, import it on that node's EGL device
    /// and compare the offscreen readback with the CPU decode. Also prints
    /// import+draw vs plain download time per frame at 1080p and 4K.
    #[test]
    #[ignore = "requires a VA-API decoder and an EGL device on the same GPU"]
    fn zero_copy_vaapi_egl_presentation_matches_cpu_decode() -> Result<()> {
        if std::env::var("TELEPORT_TEST_HARDWARE").as_deref() != Ok("1") {
            eprintln!("SKIP: set TELEPORT_TEST_HARDWARE=1 on a machine with VA-API and EGL");
            return Ok(());
        }
        let mut nodes: Vec<String> = std::fs::read_dir("/dev/dri")?
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.path().to_string_lossy().into_owned())
            .filter(|path| path.starts_with("/dev/dri/renderD"))
            .collect();
        nodes.sort();
        let mut passed = 0;
        for (width, height, iterations, hevc) in [
            (640_usize, 360_usize, 0, false),
            (640, 360, 0, true),
            (1920, 1080, 50, false),
            (3840, 2160, 20, false),
        ] {
            let (config, packets) = quadrant_stream(width, height, hevc)?;
            let cpu = decode_last(&config, &packets, "software", 0)?;
            for node in &nodes {
                let name = format!("vaapi@{node}");
                let gpu = match decode_last(&config, &packets, &name, 3) {
                    Ok(frame) => frame,
                    Err(error) => {
                        eprintln!("SKIP {name}: {error:#}");
                        continue;
                    }
                };
                let gpu = gpu
                    .gpu
                    .context("VA-API decoder did not keep the frame on the GPU")?;
                let mut rgba = vec![0_u8; width * height * 4];
                let (mut draw_ns, mut download_ns) = (0_i64, 0_i64);
                let node_c = CString::new(node.as_str())?;
                let result = unsafe {
                    tp_vaapi_egl_self_test(
                        node_c.as_ptr(),
                        gpu.0.as_ptr(),
                        rgba.as_mut_ptr(),
                        iterations,
                        &mut draw_ns,
                        &mut download_ns,
                    )
                };
                if result == -libc::ENODEV {
                    eprintln!("SKIP {name}: no EGL device for this render node");
                    continue;
                }
                check(result).with_context(|| format!("{name} at {width}x{height}"))?;
                compare_quadrants(&cpu, &rgba, width, height)
                    .with_context(|| format!("{name} at {width}x{height}"))?;
                if iterations > 0 {
                    eprintln!(
                        "{name} {width}x{height}: zero-copy import+draw {:.3} ms, download alone {:.3} ms",
                        draw_ns as f64 / 1e6,
                        download_ns as f64 / 1e6
                    );
                }
                eprintln!("PASS {name} {width}x{height} hevc={hevc}");
                passed += 1;
            }
        }
        if passed == 0 {
            eprintln!("SKIP: no VA-API render node with a matching EGL device");
        }
        Ok(())
    }

    #[test]
    #[ignore = "requires hardware encoder and at least one hardware decoder"]
    fn direct_hardware_encode_decode_both_codecs() -> Result<()> {
        if std::env::var("TELEPORT_TEST_HARDWARE").as_deref() != Ok("1") {
            eprintln!("SKIP: set TELEPORT_TEST_HARDWARE=1 on a machine with hardware encoding");
            return Ok(());
        }
        let backend = std::env::var("TELEPORT_TEST_ENCODER").unwrap_or_else(|_| "nvenc".into());
        ensure!(matches!(backend.as_str(), "nvenc" | "vaapi" | "qsv"));
        for hevc in [false, true] {
            let config = Config {
                name: format!("{}_{backend}", if hevc { "hevc" } else { "h264" }),
                width: 640,
                height: 360,
                ..software_config(hevc)
            };
            let mut encoder = Codec::new(config.clone())?;
            let mut raw = vec![90; 640 * 360];
            raw.extend(vec![128; 640 * 360 / 2]);
            let mut packets = Vec::new();
            for index in 0..30 {
                encoder.submit(&raw, index)?;
                while let Some(packet) = encoder.receive()? {
                    packets.push(packet.bytes);
                }
            }
            ensure!(
                packets.len() == 30,
                "hardware encoder buffered too many frames: {} of 30 emitted",
                packets.len()
            );
            let mut passed = 0;
            for name in ["cuda", "vaapi", "qsv"] {
                let mut decoder = match Codec::new(Config {
                    encoder: false,
                    name: name.into(),
                    ..config.clone()
                }) {
                    Ok(decoder) => decoder,
                    Err(error) => {
                        eprintln!("SKIP {name}: {error:#}");
                        continue;
                    }
                };
                let mut count = 0;
                for (index, packet) in packets.iter().enumerate() {
                    decoder.submit(packet, index as u64 + 1)?;
                    while let Some(frame) = decoder.receive()? {
                        ensure!((frame.width, frame.height) == (640, 360));
                        ensure!((frame.bytes[0] as i32 - 90).abs() < 12, "bad hardware luma");
                        count += 1;
                    }
                }
                ensure!(
                    count >= 28,
                    "hardware decoder buffered too many frames: {count}"
                );
                eprintln!("PASS {} -> {name}: {count} frames", config.name);
                passed += 1;
            }
            ensure!(passed > 0, "no hardware decoder tested");
        }
        Ok(())
    }

    /// Client decode capacity at high rates: 2560x1440 NVENC H.264 of moving
    /// content at 180 FPS (auto bitrate, one group), decoded as fast as
    /// possible by CUDA into CPU NV12 (headless and CPU presentation) and
    /// into GPU frames (zero-copy presentation). Prints frames per second
    /// and per-frame times; 180 FPS needs under 5.6 ms per frame.
    #[test]
    #[ignore = "benchmark; requires NVENC and CUDA decoding"]
    fn high_rate_decode_benchmark() -> Result<()> {
        if std::env::var("TELEPORT_TEST_HARDWARE").as_deref() != Ok("1") {
            eprintln!("SKIP: set TELEPORT_TEST_HARDWARE=1 on a machine with NVENC");
            return Ok(());
        }
        let (width, height, fps, frames) = (2560_usize, 1440_usize, 180, 360);
        let config = Config {
            name: "h264_nvenc".into(),
            width: width as u32,
            height: height as u32,
            fps,
            kbps: crate::protocol::auto_bitrate(width as u32, height as u32, fps),
            gop: frames as u32,
            ..software_config(false)
        };
        let mut encoder = Codec::new(config.clone())?;
        let mut packets = Vec::new();
        let mut raw = vec![128_u8; width * height * 3 / 2];
        for index in 0..frames {
            // Diagonal bands scrolling 8 px a frame, like a window drag.
            for y in 0..height {
                for (x, pixel) in raw[y * width..][..width].iter_mut().enumerate() {
                    *pixel = (((x + y + index * 8) / 4) as u8) ^ ((y / 16) as u8);
                }
            }
            encoder.submit(&raw, index as u64 + 1)?;
            while let Some(packet) = encoder.receive()? {
                packets.push(packet.bytes);
            }
        }
        let bytes: usize = packets.iter().map(Vec::len).sum();
        eprintln!(
            "BENCH encoded {} frames, {:.1} Mbps at {fps} FPS",
            packets.len(),
            bytes as f64 * 8.0 * f64::from(fps) / packets.len() as f64 / 1e6
        );
        for (output, label) in [(1, "CUDA -> CPU NV12"), (3, "CUDA -> GPU frame")] {
            let mut decoder = Codec::new(Config {
                encoder: false,
                name: "cuda".into(),
                output,
                ..config.clone()
            })?;
            let mut times = Vec::new();
            let started = std::time::Instant::now();
            for (index, packet) in packets.iter().enumerate() {
                let start = std::time::Instant::now();
                decoder.submit(packet, index as u64 + 1)?;
                while decoder.receive()?.is_some() {}
                times.push(start.elapsed().as_secs_f64() * 1000.0);
            }
            let total = started.elapsed().as_secs_f64();
            times.sort_by(f64::total_cmp);
            eprintln!(
                "BENCH decode {label} {width}x{height}: {:.0} frames/s, mean {:.2} ms, p95 {:.2} ms, max {:.2} ms",
                times.len() as f64 / total,
                times.iter().sum::<f64>() / times.len() as f64,
                times[times.len() * 95 / 100],
                times[times.len() - 1],
            );
        }
        Ok(())
    }

    /// Host zero-copy capture: an RGBA DMA-BUF exported by the GPU is
    /// scaled/cropped and converted on that GPU straight into NVENC's CUDA
    /// frames or VA-API surfaces; a CPU decode must show the source colours.
    #[test]
    #[ignore = "requires NVENC or a VA-API encoder and an EGL device exporting DMA-BUFs"]
    fn zero_copy_dmabuf_encode_matches_source_colours() -> Result<()> {
        if std::env::var("TELEPORT_TEST_HARDWARE").as_deref() != Ok("1") {
            eprintln!("SKIP: set TELEPORT_TEST_HARDWARE=1 on a machine with NVENC or VA-API");
            return Ok(());
        }
        let colors = [[200, 40, 40], [40, 180, 60], [50, 60, 210], [230, 230, 230]];
        let mut targets = vec![("h264_nvenc", None), ("hevc_nvenc", None)];
        let mut nodes: Vec<String> = std::fs::read_dir("/dev/dri")?
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.path().to_string_lossy().into_owned())
            .filter(|path| path.starts_with("/dev/dri/renderD"))
            .collect();
        nodes.sort();
        for node in nodes {
            targets.push(("h264_vaapi", Some(node.clone())));
            targets.push(("hevc_vaapi", Some(node)));
        }
        let mut passed = 0;
        for (name, node) in targets {
            // Same size, 2x downscale, and a crop of the top-left quadrant.
            for (source_size, size, crop) in [
                ((640, 360), (640, 360), false),
                ((1280, 720), (640, 360), false),
                ((1280, 720), (640, 360), true),
            ] {
                let label = format!("{name} {node:?} {source_size:?} -> {size:?} crop={crop}");
                let config = Config {
                    name: name.into(),
                    width: size.0,
                    height: size.1,
                    kbps: 4000,
                    gpu_input: true,
                    ..software_config(name.starts_with("hevc"))
                };
                let mut encoder = match Codec::new(config.clone()) {
                    Ok(encoder) => encoder,
                    Err(error) => {
                        eprintln!("SKIP {label}: {error:#}");
                        break;
                    }
                };
                let Some(source) =
                    TestDmabuf::new(node.as_deref(), source_size.0, source_size.1, colors)?
                else {
                    eprintln!("SKIP {label}: no EGL device exporting DMA-BUFs");
                    break;
                };
                let mut frame = source.frame;
                if !encoder
                    .dmabuf_modifiers(frame.fourcc)?
                    .contains(&frame.modifier)
                {
                    eprintln!(
                        "SKIP {label}: encoder GPU cannot import {:#x}/{:#x}",
                        frame.fourcc, frame.modifier
                    );
                    break;
                }
                if crop {
                    frame.crop_width = frame.width / 2;
                    frame.crop_height = frame.height / 2;
                }
                let mut packets = Vec::new();
                for index in 0..5 {
                    encoder.submit_dmabuf(&frame, index, index == 0)?;
                    while let Some(packet) = encoder.receive()? {
                        packets.push(packet.bytes);
                    }
                }
                let decoded = decode_last(&config, &packets, "software", 0)?;
                let (width, height) = (size.0 as usize, size.1 as usize);
                for (quadrant, (x, y)) in [
                    (width / 4, height / 4),
                    (width * 3 / 4, height / 4),
                    (width / 4, height * 3 / 4),
                    (width * 3 / 4, height * 3 / 4),
                ]
                .into_iter()
                .enumerate()
                {
                    let expected = colors[if crop { 0 } else { quadrant }];
                    let actual = &decoded.bytes[(y * width + x) * 3..][..3];
                    for channel in 0..3 {
                        ensure!(
                            (expected[channel] as i32 - actual[channel] as i32).abs() <= 8,
                            "{label}: pixel ({x},{y}) {actual:?}, source {expected:?}"
                        );
                    }
                }
                eprintln!("PASS {label}");
                passed += 1;
            }
        }
        ensure!(passed > 0, "no zero-copy encoder tested");
        Ok(())
    }
}
