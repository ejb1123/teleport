//! `--source kms`: capture the framebuffer a display controller (CRTC) is
//! scanning out, for hosts with no desktop portal and no X server, such as a
//! kiosk compositor that drives the display itself (docs/kms-capture.md).
//!
//! The capture side of FFmpeg's `kmsgrab`, done here so its frames enter the
//! existing pipeline: every frame, the CRTC's current framebuffer is exported
//! as DMA-BUFs (`src/kms_grab.c`) and pushed into an `appsrc`. With a
//! VA-API or NVENC encoder that imports the framebuffer's format and
//! modifier, the frame goes to the GPU encoder without a copy; otherwise a
//! linear framebuffer is read by the CPU. A tiled framebuffer needs the GPU
//! path.
//!
//! Reading another client's framebuffer needs `CAP_SYS_ADMIN` (or DRM
//! master). The capture is view-only: there is no input injection, and the
//! hardware cursor plane is not in the picture.
use anyhow::{Context, Result, ensure};
use gstreamer as gst;
use gstreamer::prelude::*;
use std::{
    ffi::{CStr, CString, c_char, c_int},
    os::fd::{FromRawFd, OwnedFd},
    path::{Path, PathBuf},
    sync::Mutex,
    time::{Duration, Instant},
};

/// `appsrc` names start with this, followed by the CRTC id.
const ELEMENT: &str = "kms_capture_";
/// The DRM format modifier of a linear (untiled) buffer.
const MOD_LINEAR: u64 = 0;
/// `DRM_FORMAT_MOD_INVALID`: the framebuffer was made without a modifier.
const MOD_INVALID: u64 = 0x00ff_ffff_ffff_ffff;
/// Fastest the framebuffer is sampled, whatever the display's rate.
const MAX_RATE_HZ: u32 = 240;

#[repr(C)]
struct RawOutput {
    crtc_id: u32,
    width: u32,
    height: u32,
    refresh_mhz: u32,
    name: [c_char; 32],
}

#[repr(C)]
struct RawFrame {
    fds: [i32; 4],
    source_plane: [i32; 4],
    offsets: [u32; 4],
    pitches: [u32; 4],
    planes: i32,
    fourcc: u32,
    modifier: u64,
    width: u32,
    height: u32,
}

unsafe extern "C" {
    fn tp_kms_open(path: *const c_char) -> c_int;
    fn tp_kms_outputs(fd: c_int, outputs: *mut RawOutput, capacity: c_int) -> c_int;
    fn tp_kms_grab(fd: c_int, crtc_id: u32, frame: *mut RawFrame) -> c_int;
    fn tp_kms_frame_close(frame: *mut RawFrame);
    fn gst_dmabuf_allocator_new() -> *mut gst::ffi::GstAllocator;
    fn gst_dmabuf_allocator_alloc(
        allocator: *mut gst::ffi::GstAllocator,
        fd: c_int,
        size: usize,
    ) -> *mut gst::ffi::GstMemory;
}

/// The device `--kms-device` chose; `None` picks the first card showing
/// something.
static DEVICE: Mutex<Option<PathBuf>> = Mutex::new(None);

/// Host `--kms-device`.
pub fn set_device(path: Option<PathBuf>) {
    *DEVICE.lock().unwrap() = path;
}

/// An active CRTC (one physical display).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Output {
    pub crtc: u32,
    pub name: String,
    pub width: u32,
    pub height: u32,
    pub refresh_mhz: u32,
}

/// What a CRTC's framebuffer holds, as negotiated in caps.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Format {
    pub fourcc: u32,
    pub modifier: u64,
    pub width: u32,
    pub height: u32,
}

impl Format {
    /// Framebuffers without an explicit modifier are taken as linear, as
    /// FFmpeg's kmsgrab does by default.
    fn modifier(&self) -> u64 {
        if self.modifier == MOD_INVALID {
            MOD_LINEAR
        } else {
            self.modifier
        }
    }

    /// The GStreamer raw format the CPU reads a linear buffer as.
    fn cpu_format(&self) -> Option<&'static str> {
        cpu_format(self.fourcc)
    }
}

/// GStreamer's name for a 32-bit RGB DRM format (little-endian byte order).
fn cpu_format(fourcc: u32) -> Option<&'static str> {
    Some(match &fourcc.to_le_bytes() {
        b"XR24" => "BGRx",
        b"AR24" => "BGRA",
        b"XB24" => "RGBx",
        b"AB24" => "RGBA",
        _ => return None,
    })
}

fn drm_format(fourcc: u32, modifier: u64) -> String {
    gstreamer_video::dma_drm_fourcc_to_string(fourcc, modifier).to_string()
}

/// An open DRM device node.
struct Device(OwnedFd);

impl Device {
    fn open(path: &Path) -> Result<Self> {
        let name = CString::new(path.as_os_str().as_encoded_bytes())?;
        let fd = unsafe { tp_kms_open(name.as_ptr()) };
        if fd < 0 {
            return Err(std::io::Error::from_raw_os_error(-fd))
                .with_context(|| format!("cannot open {}", path.display()));
        }
        Ok(Self(unsafe { OwnedFd::from_raw_fd(fd) }))
    }

    fn fd(&self) -> c_int {
        use std::os::fd::AsRawFd;
        self.0.as_raw_fd()
    }

    fn outputs(&self) -> Result<Vec<Output>> {
        let mut raw: Vec<RawOutput> = (0..16)
            .map(|_| RawOutput {
                crtc_id: 0,
                width: 0,
                height: 0,
                refresh_mhz: 0,
                name: [0; 32],
            })
            .collect();
        let count = unsafe { tp_kms_outputs(self.fd(), raw.as_mut_ptr(), raw.len() as c_int) };
        if count < 0 {
            return Err(std::io::Error::from_raw_os_error(-count))
                .context("cannot read the display configuration");
        }
        raw.truncate(count as usize);
        Ok(raw
            .iter()
            .map(|output| Output {
                crtc: output.crtc_id,
                name: unsafe { CStr::from_ptr(output.name.as_ptr()) }
                    .to_string_lossy()
                    .into_owned(),
                width: output.width,
                height: output.height,
                refresh_mhz: output.refresh_mhz,
            })
            .collect())
    }

    /// The CRTC's current framebuffer; `Ok(None)` while it shows nothing.
    fn grab(&self, crtc: u32) -> Result<Option<Frame>> {
        let mut frame = Frame(RawFrame {
            fds: [-1; 4],
            source_plane: [0; 4],
            offsets: [0; 4],
            pitches: [0; 4],
            planes: 0,
            fourcc: 0,
            modifier: 0,
            width: 0,
            height: 0,
        });
        match unsafe { tp_kms_grab(self.fd(), crtc, &mut frame.0) } {
            0 => Ok(Some(frame)),
            code if code == -libc::ENOENT => Ok(None),
            code if code == -libc::EPERM => anyhow::bail!(
                "KMS capture needs CAP_SYS_ADMIN (or DRM master) to read the display's framebuffer"
            ),
            code => Err(std::io::Error::from_raw_os_error(-code))
                .context("cannot read the display's framebuffer"),
        }
    }
}

/// DMA-BUFs of one framebuffer, closed on drop unless handed to GStreamer.
struct Frame(RawFrame);

impl Frame {
    fn format(&self) -> Format {
        Format {
            fourcc: self.0.fourcc,
            modifier: self.0.modifier,
            width: self.0.width,
            height: self.0.height,
        }
    }

    /// A GStreamer buffer of DMA-BUF memories, one per buffer object, with
    /// the plane layout in its `VideoMeta`. Takes ownership of the fds.
    fn into_buffer(mut self, allocator: *mut gst::ffi::GstAllocator) -> Result<gst::Buffer> {
        let planes = usize::try_from(self.0.planes)?;
        ensure!((1..=4).contains(&planes), "unsupported framebuffer layout");
        let mut buffer = gst::Buffer::new();
        let writable = buffer.get_mut().context("new buffer is shared")?;
        // Where each buffer object starts in the concatenated buffer.
        let mut starts = [0usize; 4];
        let mut total = 0;
        let mut offsets = Vec::with_capacity(planes);
        let mut strides = Vec::with_capacity(planes);
        for plane in 0..planes {
            let source = usize::try_from(self.0.source_plane[plane])?;
            ensure!(source <= plane, "invalid framebuffer layout");
            if source == plane {
                let fd = std::mem::replace(&mut self.0.fds[plane], -1);
                ensure!(fd >= 0, "framebuffer plane without a buffer");
                let size = unsafe { libc::lseek(fd, 0, libc::SEEK_END) };
                if size <= 0 {
                    unsafe { libc::close(fd) };
                    anyhow::bail!("cannot size the framebuffer's DMA-BUF");
                }
                // The allocator owns the fd from here.
                let memory = unsafe { gst_dmabuf_allocator_alloc(allocator, fd, size as usize) };
                ensure!(!memory.is_null(), "cannot wrap the framebuffer's DMA-BUF");
                writable.append_memory(unsafe { gst::glib::translate::from_glib_full(memory) });
                starts[plane] = total;
                total += size as usize;
            }
            offsets.push(starts[source] + self.0.offsets[plane] as usize);
            strides.push(i32::try_from(self.0.pitches[plane])?);
        }
        gstreamer_video::VideoMeta::add_full(
            writable,
            gstreamer_video::VideoFrameFlags::empty(),
            gstreamer_video::VideoFormat::DmaDrm,
            self.0.width,
            self.0.height,
            &offsets,
            &strides,
        )?;
        Ok(buffer)
    }
}

impl Drop for Frame {
    fn drop(&mut self) {
        unsafe { tp_kms_frame_close(&mut self.0) };
    }
}

/// The device to capture: `--kms-device`, or the first `/dev/dri/card*`
/// with an active CRTC.
fn choose_device() -> Result<(PathBuf, Device, Vec<Output>)> {
    if let Some(path) = DEVICE.lock().unwrap().clone() {
        let device = Device::open(&path)?;
        let outputs = device.outputs()?;
        ensure!(
            !outputs.is_empty(),
            "{} shows nothing (no active display)",
            path.display()
        );
        return Ok((path, device, outputs));
    }
    let mut cards: Vec<PathBuf> = std::fs::read_dir("/dev/dri")
        .context("no DRM devices (/dev/dri)")?
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("card"))
        })
        .collect();
    cards.sort();
    for path in cards {
        let Ok(device) = Device::open(&path) else {
            continue;
        };
        if let Ok(outputs) = device.outputs()
            && !outputs.is_empty()
        {
            return Ok((path, device, outputs));
        }
    }
    anyhow::bail!("no DRM device has an active display; set --kms-device")
}

/// A KMS capture: its active displays and each display's framebuffer
/// format when it was opened.
#[derive(Debug)]
pub struct Kms {
    pub outputs: Vec<(Output, Format)>,
}

impl Kms {
    /// Open the device and read every active display once, which also
    /// proves the permission to read framebuffers.
    pub fn open() -> Result<Self> {
        let (path, device, outputs) = choose_device()?;
        let mut found = Vec::new();
        for output in outputs {
            match device.grab(output.crtc)? {
                Some(frame) => found.push((output, frame.format())),
                None => tracing::warn!(display = %output.name, "display shows nothing; skipped"),
            }
        }
        ensure!(!found.is_empty(), "no display shows a framebuffer");
        tracing::info!(
            device = %path.display(),
            displays = ?found.iter().map(|(output, format)| format!(
                "{} {}x{} {}", output.name, format.width, format.height,
                drm_format(format.fourcc, format.modifier())
            )).collect::<Vec<_>>(),
            "KMS capture opened"
        );
        *DEVICE.lock().unwrap() = Some(path);
        Ok(Self { outputs: found })
    }
}

/// The source description for display `output`: CPU-readable caps when the
/// framebuffer is linear (the encoder derives the DMA-BUF variant with
/// [`dmabuf_source`]), DMA-BUF caps otherwise.
pub fn description(output: &Output, format: &Format) -> String {
    let rate = rate_hz(output);
    match format
        .cpu_format()
        .filter(|_| format.modifier() == MOD_LINEAR)
    {
        Some(cpu) => appsrc(
            output.crtc,
            &format!("video/x-raw,format={cpu}"),
            format,
            rate,
        ),
        None => appsrc(output.crtc, &dmabuf_caps(format), format, rate),
    }
}

fn dmabuf_caps(format: &Format) -> String {
    format!(
        "video/x-raw(memory:DMABuf),format=DMA_DRM,drm-format={}",
        drm_format(format.fourcc, format.modifier())
    )
}

fn appsrc(crtc: u32, caps: &str, format: &Format, rate: u32) -> String {
    format!(
        "appsrc name={ELEMENT}{crtc} is-live=true do-timestamp=true format=time max-buffers=2 block=false caps=\"{caps},width={},height={},framerate={rate}/1\"",
        format.width, format.height
    )
}

fn rate_hz(output: &Output) -> u32 {
    output.refresh_mhz.div_ceil(1000).clamp(1, MAX_RATE_HZ)
}

/// The DMA-BUF variant of a KMS source description with CPU caps (a linear
/// framebuffer), for zero-copy encoding; `None` for anything else.
pub fn dmabuf_source(source: &str) -> Option<String> {
    let rest = source.strip_prefix(&format!("appsrc name={ELEMENT}"))?;
    let (before, caps) = rest.split_once("caps=\"video/x-raw,format=")?;
    let (cpu, after) = caps.split_once(',')?;
    let fourcc = [b"XR24", b"AR24", b"XB24", b"AB24"]
        .into_iter()
        .map(|name| u32::from_le_bytes(*name))
        .find(|fourcc| cpu_format(*fourcc) == Some(cpu))?;
    Some(format!(
        "appsrc name={ELEMENT}{before}caps=\"video/x-raw(memory:DMABuf),format=DMA_DRM,drm-format={},{after}",
        drm_format(fourcc, MOD_LINEAR)
    ))
}

/// Start feeding every KMS `appsrc` in a new capture pipeline. Feeding
/// stops when the pipeline stops accepting buffers.
pub fn attach(pipeline: &gst::Pipeline) {
    for element in pipeline.iterate_elements().into_iter().flatten() {
        let Some(crtc) = element
            .name()
            .strip_prefix(ELEMENT)
            .and_then(|crtc| crtc.parse::<u32>().ok())
        else {
            continue;
        };
        let Ok(source) = element.downcast::<gstreamer_app::AppSrc>() else {
            continue;
        };
        let Some(path) = DEVICE.lock().unwrap().clone() else {
            source.post_error_message(gst::error_msg!(
                gst::ResourceError::NotFound,
                ["KMS capture was not opened"]
            ));
            continue;
        };
        std::thread::Builder::new()
            .name(format!("kms-{crtc}"))
            .spawn(move || {
                if let Err(error) = feed(&source, &path, crtc) {
                    tracing::error!(error = format!("{error:#}"), "KMS capture stopped");
                    source.post_error_message(gst::error_msg!(
                        gst::ResourceError::Read,
                        ["{error:#}"]
                    ));
                }
            })
            .ok();
    }
}

/// The mode `source` was described with: DMA-BUF caps or CPU caps, and the
/// sampling rate.
fn mode(source: &gstreamer_app::AppSrc) -> Result<(bool, u32)> {
    let caps = source.caps().context("KMS source without caps")?;
    let dmabuf = caps
        .features(0)
        .is_some_and(|features| features.contains("memory:DMABuf"));
    let structure = caps.structure(0).context("empty KMS caps")?;
    let rate = structure
        .get::<gst::Fraction>("framerate")
        .ok()
        .and_then(|rate| u32::try_from(rate.numer()).ok())
        .unwrap_or(60)
        .clamp(1, MAX_RATE_HZ);
    Ok((dmabuf, rate))
}

/// Caps for `format` in the source's mode, or `None` when the CPU cannot
/// read it (a tiled or unknown framebuffer without the DMA-BUF path).
fn caps_for(format: &Format, dmabuf: bool, rate: u32) -> Option<gst::Caps> {
    let description = if dmabuf {
        dmabuf_caps(format)
    } else {
        let cpu = format.cpu_format()?;
        if format.modifier() != MOD_LINEAR {
            return None;
        }
        format!("video/x-raw,format={cpu}")
    };
    format!(
        "{description},width={},height={},framerate={rate}/1",
        format.width, format.height
    )
    .parse()
    .ok()
}

fn feed(source: &gstreamer_app::AppSrc, path: &Path, crtc: u32) -> Result<()> {
    let device = Device::open(path)?;
    let (dmabuf, rate) = mode(source)?;
    // Attached before the pipeline starts; an appsrc refuses buffers until
    // it is at least paused.
    let started = Instant::now();
    while source.current_state() < gst::State::Paused {
        if started.elapsed() > Duration::from_secs(10) {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let allocator = Allocator::new()?;
    let period = Duration::from_secs(1) / rate;
    let mut next = Instant::now();
    let mut negotiated: Option<Format> = None;
    let mut unreadable: Option<Format> = None;
    loop {
        if let Some(frame) = device.grab(crtc)? {
            let format = frame.format();
            if negotiated != Some(format) {
                match caps_for(&format, dmabuf, rate) {
                    Some(caps) => {
                        source.set_caps(Some(&caps));
                        negotiated = Some(format);
                        unreadable = None;
                    }
                    None => {
                        if unreadable != Some(format) {
                            tracing::warn!(
                                format = drm_format(format.fourcc, format.modifier()),
                                "the display's framebuffer is not CPU-readable (tiled); \
                                 frames are skipped until it is, or use a VA-API/NVENC encoder"
                            );
                            unreadable = Some(format);
                        }
                    }
                }
            }
            if negotiated == Some(format)
                && source.push_buffer(frame.into_buffer(allocator.0)?).is_err()
            {
                return Ok(());
            }
        } else if source.current_state() < gst::State::Paused {
            // Stopped while the display showed nothing.
            return Ok(());
        }
        next += period;
        let now = Instant::now();
        if next < now {
            next = now;
        }
        std::thread::sleep(next.saturating_duration_since(now));
    }
}

struct Allocator(*mut gst::ffi::GstAllocator);

impl Allocator {
    fn new() -> Result<Self> {
        let allocator = unsafe { gst_dmabuf_allocator_new() };
        ensure!(!allocator.is_null(), "no GStreamer DMA-BUF allocator");
        Ok(Self(allocator))
    }
}

impl Drop for Allocator {
    fn drop(&mut self) {
        unsafe { gst::ffi::gst_object_unref(self.0.cast()) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn output() -> Output {
        Output {
            crtc: 42,
            name: "HDMI-A-1".into(),
            width: 1920,
            height: 1080,
            refresh_mhz: 59_940,
        }
    }

    fn format(fourcc: &[u8; 4], modifier: u64) -> Format {
        Format {
            fourcc: u32::from_le_bytes(*fourcc),
            modifier,
            width: 1920,
            height: 1080,
        }
    }

    #[test]
    fn linear_framebuffers_are_cpu_readable_and_have_a_dmabuf_variant() {
        let cpu = description(&output(), &format(b"XR24", MOD_LINEAR));
        assert_eq!(
            cpu,
            "appsrc name=kms_capture_42 is-live=true do-timestamp=true format=time max-buffers=2 block=false caps=\"video/x-raw,format=BGRx,width=1920,height=1080,framerate=60/1\""
        );
        // A framebuffer made without a modifier is linear (as kmsgrab).
        assert_eq!(description(&output(), &format(b"XR24", MOD_INVALID)), cpu);
        let dmabuf = dmabuf_source(&cpu).unwrap();
        assert_eq!(
            dmabuf,
            "appsrc name=kms_capture_42 is-live=true do-timestamp=true format=time max-buffers=2 block=false caps=\"video/x-raw(memory:DMABuf),format=DMA_DRM,drm-format=XR24,width=1920,height=1080,framerate=60/1\""
        );
        // The encoder recognises it as a DMA-BUF source as it is.
        assert!(dmabuf.contains("memory:DMABuf"));
        for (fourcc, cpu) in [(b"AR24", "BGRA"), (b"XB24", "RGBx"), (b"AB24", "RGBA")] {
            let description = description(&output(), &format(fourcc, MOD_LINEAR));
            assert!(
                description.contains(&format!("format={cpu},")),
                "{description}"
            );
            assert!(dmabuf_source(&description).is_some());
        }
    }

    #[test]
    fn tiled_framebuffers_are_dmabuf_only() {
        gst::init().unwrap();
        // I915_FORMAT_MOD_Y_TILED
        let tiled = format(b"XR24", 0x0100_0000_0000_0002);
        let source = description(&output(), &tiled);
        assert!(source.contains(
            "caps=\"video/x-raw(memory:DMABuf),format=DMA_DRM,drm-format=XR24:0x0100000000000002,"
        ));
        // Already a DMA-BUF description: nothing to derive.
        assert_eq!(dmabuf_source(&source), None);
        assert!(caps_for(&tiled, false, 60).is_none());
        assert!(caps_for(&tiled, true, 60).is_some());
        // An unknown format is never handed to the CPU.
        assert!(caps_for(&format(b"NV12", MOD_LINEAR), false, 60).is_none());
    }

    #[test]
    fn other_sources_have_no_kms_variant() {
        for source in [
            "videotestsrc is-live=true pattern=ball",
            "pipewiresrc fd=3 path=7 ! video/x-raw",
            "appsrc name=test_capture caps=\"video/x-raw,format=RGBA\"",
        ] {
            assert_eq!(dmabuf_source(source), None, "{source}");
        }
    }

    #[test]
    fn sampling_rate_follows_the_display_within_bounds() {
        let mut display = output();
        assert_eq!(rate_hz(&display), 60);
        display.refresh_mhz = 143_998;
        assert_eq!(rate_hz(&display), 144);
        display.refresh_mhz = 360_000;
        assert_eq!(rate_hz(&display), MAX_RATE_HZ);
        display.refresh_mhz = 0;
        assert_eq!(rate_hz(&display), 1);
    }

    #[test]
    fn caps_parse_and_follow_the_mode() {
        gst::init().unwrap();
        let linear = format(b"XR24", MOD_LINEAR);
        let cpu = caps_for(&linear, false, 30).unwrap();
        assert!(cpu.features(0).is_none_or(|f| !f.contains("memory:DMABuf")));
        let gpu = caps_for(&linear, true, 30).unwrap();
        assert!(gpu.features(0).unwrap().contains("memory:DMABuf"));
        let structure = gpu.structure(0).unwrap();
        assert_eq!(structure.get::<i32>("width").unwrap(), 1920);
        assert_eq!(
            structure.get::<gst::Fraction>("framerate").unwrap(),
            gst::Fraction::new(30, 1)
        );
    }

    /// End to end on a real display controller: `vkms` in a VM (never a
    /// desktop someone is using). Needs CAP_SYS_ADMIN; see
    /// docs/kms-capture.md.
    #[test]
    #[ignore = "needs a KMS device showing a framebuffer and CAP_SYS_ADMIN; set TELEPORT_TEST_KMS=/dev/dri/cardN"]
    fn captures_a_real_framebuffer() -> Result<()> {
        let Ok(device) = std::env::var("TELEPORT_TEST_KMS") else {
            eprintln!("SKIP: set TELEPORT_TEST_KMS=/dev/dri/cardN (a vkms device in a VM)");
            return Ok(());
        };
        gst::init()?;
        set_device(Some(device.into()));
        let kms = Kms::open()?;
        let (output, format) = kms.outputs[0].clone();
        let source = description(&output, &format);
        // CPU path: the pixels the CPU reads are the framebuffer's.
        let pipeline = gst::parse::launch(&format!(
            "{source} ! videoconvert ! video/x-raw,format=BGRx ! appsink name=out sync=false max-buffers=2 drop=true"
        ))?
        .downcast::<gst::Pipeline>()
        .unwrap();
        attach(&pipeline);
        pipeline.set_state(gst::State::Playing)?;
        let sink = pipeline
            .by_name("out")
            .unwrap()
            .downcast::<gstreamer_app::AppSink>()
            .unwrap();
        let mut frames = 0;
        let mut colors = std::collections::BTreeSet::new();
        while frames < 10 {
            let sample = sink
                .try_pull_sample(gst::ClockTime::from_seconds(5))
                .context("no frame from the KMS capture")?;
            let buffer = sample.buffer().context("no buffer")?;
            let map = buffer.map_readable()?;
            colors = map
                .chunks_exact(4)
                .step_by(97)
                .map(|pixel| [pixel[0], pixel[1], pixel[2]])
                .collect();
            frames += 1;
        }
        pipeline.set_state(gst::State::Null)?;
        eprintln!(
            "KMS {} {}x{} {}: {frames} frames, {} distinct colours sampled",
            output.name,
            format.width,
            format.height,
            drm_format(format.fourcc, format.modifier()),
            colors.len()
        );
        // A test pattern (modetest), not a blank or unreadable buffer.
        ensure!(
            colors.len() >= 4,
            "captured frames are blank: {} distinct colours",
            colors.len()
        );
        Ok(())
    }
}
