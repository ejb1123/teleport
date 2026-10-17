//! Direct FFmpeg video codecs. GStreamer is only the capture/appsrc scheduler;
//! no GStreamer encoder, decoder, parser or gst-libav element is used here.
use super::*;
use crate::ffmpeg::{Codec, Config};
use std::{collections::BTreeMap, sync::OnceLock};

/// Reopen-and-continue budget for encoder errors; reset by any encoded frame.
#[cfg(target_os = "linux")]
const MAX_CONSECUTIVE_ENCODE_FAILURES: u32 = 3;

pub(super) struct State {
    codec: Mutex<Codec>,
    /// Set by a client keyframe request; consumed by the next encoded frame.
    keyframe: std::sync::atomic::AtomicBool,
    /// Encoders: force an IDR after this many frames without one. Starts at the
    /// historical fps/4 so every client recovers by itself; lengthened once
    /// the client declares it requests keyframes on loss.
    key_interval: std::sync::atomic::AtomicU32,
    #[cfg(target_os = "linux")]
    pub metrics: Option<Arc<EncoderMetrics>>,
    #[cfg(target_os = "linux")]
    reopen: Reopen,
}

/// Bitrate changes the open encoder cannot apply in place (VA-API, QSV,
/// x265): a replacement is opened on its own thread while the current encoder
/// keeps producing frames, then swapped in before the next frame.
#[cfg(target_os = "linux")]
#[derive(Default)]
struct Reopen {
    replacement: Mutex<Option<Codec>>,
    /// Bitrate requested last; 0 = the open encoder's own.
    target_kbps: std::sync::atomic::AtomicU32,
    /// Set by `set_bitrate`; consumed by the encoding thread.
    pending: std::sync::atomic::AtomicBool,
    running: std::sync::atomic::AtomicBool,
    /// Why the last background reopen failed; reported by the next change.
    error: Mutex<Option<String>>,
}

pub(super) fn state(pipeline: &gst::Pipeline) -> Option<Arc<State>> {
    // Private, immutable qdata slot owns exactly Arc<State>; the pipeline
    // reference keeps that slot alive throughout cloning. No one steals it.
    unsafe {
        pipeline
            .data::<Arc<State>>("teleport-direct-ffmpeg")
            .map(|pointer| pointer.as_ref().clone())
    }
}
fn attach(pipeline: &gst::Pipeline, state: Arc<State>) {
    unsafe {
        pipeline.set_data("teleport-direct-ffmpeg", state);
    }
}
impl State {
    pub fn flush(&self) {
        self.codec.lock().unwrap().flush();
    }
    pub fn software_decoder(&self) -> Result<()> {
        let mut codec = self.codec.lock().unwrap();
        ensure!(!codec.config.encoder, "not a decoder");
        *codec = Codec::new(Config {
            name: "software".into(),
            ..codec.config.clone()
        })?;
        Ok(())
    }
    #[cfg(target_os = "linux")]
    pub fn request_keyframe(&self) {
        self.keyframe.store(true, Ordering::Relaxed);
    }
    #[cfg(target_os = "linux")]
    pub fn set_key_interval(&self, frames: u32) {
        self.key_interval.store(frames.max(1), Ordering::Relaxed);
    }
    #[cfg(target_os = "linux")]
    pub fn bitrate(&self) -> u32 {
        match self.reopen.target_kbps.load(Ordering::Acquire) {
            0 => self.codec.lock().unwrap().config.kbps,
            target => target,
        }
    }
    /// Never waits for the encoder: the change is applied by the encoding
    /// thread before its next frame (whose encode may hold the codec for a
    /// whole frame time), and a reopen happens on a thread of its own.
    #[cfg(target_os = "linux")]
    pub fn set_bitrate(&self, kbps: u32) -> Result<()> {
        ensure!((1..=100_000).contains(&kbps), "invalid video bitrate");
        ensure!(self.metrics.is_some(), "not an encoder");
        if let Some(error) = self.reopen.error.lock().unwrap().take() {
            anyhow::bail!("encoder could not change bitrate: {error}");
        }
        self.reopen.target_kbps.store(kbps, Ordering::Release);
        self.reopen.pending.store(true, Ordering::Release);
        Ok(())
    }
    /// Encoding thread, before each frame: swap in a reopened encoder, then
    /// apply a requested bitrate change.
    #[cfg(target_os = "linux")]
    fn apply_bitrate(self: &Arc<Self>, codec: &mut Codec) -> bool {
        let mut replaced = false;
        if let Some(replacement) = self.reopen.replacement.lock().unwrap().take() {
            *codec = replacement;
            replaced = true;
        }
        if !self.reopen.pending.swap(false, Ordering::AcqRel) {
            return replaced;
        }
        let kbps = self.reopen.target_kbps.load(Ordering::Acquire);
        if codec.config.kbps == kbps {
            return replaced;
        }
        // NVENC and libx264 reconfigure in place at the next frame: no new
        // hardware session, no forced IDR during the congestion that caused
        // this change. (Verified by the in-place bitrate codec test.)
        match codec.set_bitrate_in_place(kbps) {
            Ok(true) => return replaced,
            Ok(false) => (),
            Err(error) => {
                *self.reopen.error.lock().unwrap() = Some(format!("{error:#}"));
                self.reopen.target_kbps.store(0, Ordering::Release);
                return replaced;
            }
        }
        // FFmpeg does not provide a universal mutable bitrate contract. Reopen
        // (a new IDR/GOP) instead of claiming a change merely because
        // AVCodecContext.bit_rate accepted an assignment, but beside the
        // encoder, which keeps producing frames: opening a VA-API or x265
        // encoder takes tens to hundreds of ms.
        if !self.reopen.running.swap(true, Ordering::AcqRel) {
            let (state, config) = (self.clone(), codec.config.clone());
            if let Err(error) = std::thread::Builder::new()
                .name("encoder-reopen".into())
                .spawn(move || state.reopen(config))
            {
                *self.reopen.error.lock().unwrap() = Some(error.to_string());
                self.reopen.target_kbps.store(0, Ordering::Release);
                self.reopen.running.store(false, Ordering::Release);
            }
        }
        replaced
    }
    #[cfg(target_os = "linux")]
    fn reopen(&self, config: Config) {
        loop {
            let kbps = self.reopen.target_kbps.load(Ordering::Acquire);
            match Codec::new(Config {
                kbps,
                ..config.clone()
            }) {
                Ok(codec) => *self.reopen.replacement.lock().unwrap() = Some(codec),
                Err(error) => {
                    tracing::warn!(%error, kbps, "encoder reopen for a bitrate change failed; keeping the current encoder");
                    *self.reopen.error.lock().unwrap() = Some(format!("{error:#}"));
                    self.reopen.target_kbps.store(0, Ordering::Release);
                    self.reopen.running.store(false, Ordering::Release);
                    return;
                }
            }
            self.reopen.running.store(false, Ordering::Release);
            // A change made while this one opened found `running` set and
            // left it here.
            let target = self.reopen.target_kbps.load(Ordering::Acquire);
            if target == kbps || target == 0 || self.reopen.running.swap(true, Ordering::AcqRel) {
                return;
            }
        }
    }
}

fn config(name: &str, format: VideoFormat, output: i32) -> Config {
    Config {
        encoder: false,
        name: name.into(),
        hevc: format.codec == VideoCodec::H265,
        hdr: format.hdr(),
        width: 64,
        height: 48,
        fps: 30,
        kbps: 1000,
        output,
        gop: 0,
        gpu_input: false,
    }
}

fn synthetic(format: VideoFormat) -> Result<Vec<Vec<u8>>> {
    let mut config = config(
        if format.codec == VideoCodec::H265 {
            "libx265"
        } else {
            "libx264"
        },
        format,
        1,
    );
    config.encoder = true;
    let mut encoder = Codec::new(config)?;
    let raw = neutral_frame(64, 48, format.hdr());
    let mut packets = Vec::new();
    for pts in 0..3 {
        encoder.submit(&raw, pts)?;
        while let Some(packet) = encoder.receive()? {
            if packets.is_empty() {
                ensure!(packet.key, "probe must begin with a keyframe");
            }
            packets.push(packet.bytes);
        }
    }
    ensure!(
        !packets.is_empty(),
        "FFmpeg synthetic encoder produced no packets"
    );
    Ok(packets)
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    #[tokio::test]
    #[ignore = "requires GStreamer capture plugins; codecs are direct FFmpeg"]
    async fn session_key_interval_defaults_short_and_lengthens_on_request() -> Result<()> {
        gst::init()?;
        let mut broadcast = moq_net::broadcast::Info::new().produce();
        let track = broadcast.create_track(VideoCodec::H264.track(), None)?;
        let mut subscriber = track.subscribe(None);
        let encode = encoder(
            "videotestsrc is-live=true pattern=ball",
            (320, 180),
            30,
            2000,
            track,
            EncoderKind::Software,
            VideoCodec::H264.into(),
        )?;
        // Count groups (each starts at an IDR) arriving within a window.
        async fn groups_within(
            subscriber: &mut moq_net::track::Subscriber,
            window: Duration,
        ) -> Result<u32> {
            let deadline = tokio::time::Instant::now() + window;
            let mut groups = 0;
            while let Ok(group) = tokio::time::timeout_at(deadline, subscriber.recv_group()).await {
                group?.context("track closed")?;
                groups += 1;
            }
            Ok(groups)
        }
        // Clients that never declare keyframe requests keep fps/4 (4 IDR/s).
        let legacy = groups_within(&mut subscriber, Duration::from_millis(1500)).await?;
        ensure!(
            legacy >= 4,
            "legacy cadence produced only {legacy} groups in 1.5 s"
        );
        encode.set_key_interval(30 * 4);
        let _ = groups_within(&mut subscriber, Duration::from_millis(300)).await?;
        let long = groups_within(&mut subscriber, Duration::from_millis(1500)).await?;
        ensure!(
            long <= 1,
            "long interval still produced {long} groups in 1.5 s"
        );
        encode.request_keyframe()?;
        ensure!(
            groups_within(&mut subscriber, Duration::from_millis(500)).await? >= 1,
            "requested keyframe did not start a group"
        );
        Ok(())
    }

    #[tokio::test]
    #[ignore = "requires GStreamer capture plugins; codecs are direct FFmpeg"]
    async fn direct_pipeline_bitrate_reopen_reset_and_generation() -> Result<()> {
        gst::init()?;
        for codec in [VideoCodec::H264, VideoCodec::H265] {
            let mut broadcast = moq_net::broadcast::Info::new().produce();
            let track = broadcast.create_track(codec.track(), None)?;
            let mut subscriber = track.subscribe(None);
            let encode = encoder(
                "videotestsrc is-live=true",
                (320, 180),
                30,
                20_000,
                track,
                EncoderKind::Software,
                codec.into(),
            )?;
            ensure!(
                encode.0.by_name("video_encoder").is_none(),
                "unexpected GStreamer encoder"
            );
            let stats = Arc::new(crate::stats::StreamStats::default());
            let (decode, input, image) =
                decoder(true, stats.clone(), codec.into(), true, false, None)?;
            ensure!(
                decode.0.by_name("video_decoder").is_none(),
                "unexpected GStreamer decoder"
            );
            for (round, bitrate) in [20_000, 40_000].into_iter().enumerate() {
                if round != 0 {
                    encode.set_video_bitrate(bitrate)?;
                    super::super::reset_decoder(&input, stats.clone()).await?;
                    *image.lock().unwrap() = None;
                }
                ensure!(encode.video_bitrate()? == bitrate);
                let mut group =
                    tokio::time::timeout(Duration::from_secs(3), subscriber.recv_group())
                        .await??
                        .context("no group")?;
                let packet = tokio::time::timeout(
                    Duration::from_secs(3),
                    crate::protocol::read_frame(&mut group, crate::protocol::MAX_FRAME),
                )
                .await??
                .context("no keyframe")?;
                let generation = round as u64 + 10;
                let key = stats.received(packet.len(), generation);
                let mut buffer = gst::Buffer::from_slice(packet);
                stamp_video_buffer(buffer.get_mut().unwrap(), key);
                input.push_buffer(buffer)?;
                let deadline = Instant::now() + Duration::from_secs(3);
                loop {
                    if let Some(frame) = image.lock().unwrap().take() {
                        ensure!(frame.video_group == Some(generation));
                        ensure!((frame.width, frame.height) == (320, 180));
                        ensure!(frame.nv12.is_some());
                        break;
                    }
                    ensure!(Instant::now() < deadline, "direct decode did not recover");
                    decode.error()?;
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
            }
            ensure!(encode.encoder_metrics()?.encoder.starts_with("ffmpeg:"));
            encode.error()?;
            decode.error()?;
        }
        Ok(())
    }

    /// Called with every new capture pipeline before it starts (tests feed
    /// their `appsrc` from here).
    pub(super) static PIPELINE_HOOK: Mutex<Option<PipelineHook>> = Mutex::new(None);
    type PipelineHook = Box<dyn Fn(&gst::Pipeline) + Send>;
    /// Frames the test capture has pushed (to compare with frames encoded).
    static PUSHED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

    unsafe extern "C" {
        fn gst_dmabuf_allocator_new() -> *mut gst::ffi::GstAllocator;
        fn gst_dmabuf_allocator_alloc(
            allocator: *mut gst::ffi::GstAllocator,
            fd: std::ffi::c_int,
            size: usize,
        ) -> *mut gst::ffi::GstMemory;
    }

    const COLORS: [[u8; 3]; 4] = [[200, 40, 40], [40, 180, 60], [50, 60, 210], [230, 230, 230]];

    /// A live test capture: DMA-BUFs exported by a GPU (like a compositor's
    /// screencast buffers), or the same quadrants in CPU memory (like a
    /// PipeWire SHM buffer). Content is static; each frame is a new buffer
    /// referencing it, so the source itself costs (almost) nothing.
    enum TestCapture {
        Dmabuf(Arc<crate::ffmpeg::TestDmabuf>),
        Cpu(bytes::Bytes),
    }

    impl TestCapture {
        fn new(dmabuf: bool, node: Option<&str>, size: (u32, u32)) -> Result<Option<Self>> {
            if dmabuf {
                return Ok(
                    crate::ffmpeg::TestDmabuf::new(node, size.0, size.1, COLORS)?
                        .map(|source| Self::Dmabuf(Arc::new(source))),
                );
            }
            let (width, height) = (size.0 as usize, size.1 as usize);
            let mut rgba = vec![255_u8; width * height * 4];
            for y in 0..height {
                for x in 0..width {
                    let color = COLORS[(y >= height / 2) as usize * 2 + (x >= width / 2) as usize];
                    rgba[(y * width + x) * 4..][..3].copy_from_slice(&color);
                }
            }
            Ok(Some(Self::Cpu(rgba.into())))
        }

        fn description(&self, size: (u32, u32), fps: u32) -> Result<String> {
            let caps = match self {
                Self::Dmabuf(source) => format!(
                    "video/x-raw(memory:DMABuf),format=DMA_DRM,drm-format={}",
                    gstreamer_video::dma_drm_fourcc_to_string(
                        source.frame.fourcc,
                        source.frame.modifier
                    )
                ),
                Self::Cpu(_) => "video/x-raw,format=RGBA".into(),
            };
            Ok(format!(
                "appsrc name=test_capture is-live=true do-timestamp=true format=time max-buffers=2 block=false caps=\"{caps},width={},height={},framerate={fps}/1\"",
                size.0, size.1
            ))
        }

        /// Push a frame every 1/fps (paced to absolute deadlines, so the
        /// rate holds at 180 FPS) from a thread until the pipeline stops.
        /// `TELEPORT_BENCH_JITTER_US` delays each frame by a random 0..=N µs,
        /// like a compositor's vblank-to-delivery scheduling.
        fn feed(&self, pipeline: &gst::Pipeline, fps: u32) {
            let Some(source) = pipeline.by_name("test_capture") else {
                return;
            };
            let source = source.downcast::<gstreamer_app::AppSrc>().unwrap();
            let capture = match self {
                Self::Dmabuf(source) => Self::Dmabuf(source.clone()),
                Self::Cpu(bytes) => Self::Cpu(bytes.clone()),
            };
            let jitter: u64 = std::env::var("TELEPORT_BENCH_JITTER_US")
                .ok()
                .and_then(|value| value.parse().ok())
                .unwrap_or(0);
            std::thread::spawn(move || {
                let allocator = unsafe { gst_dmabuf_allocator_new() };
                let period = Duration::from_secs(1) / fps;
                let mut vblank = Instant::now();
                let mut random = 0x9e37_79b9_7f4a_7c15_u64;
                loop {
                    let buffer = match &capture {
                        Self::Cpu(bytes) => gst::Buffer::from_slice(bytes.clone()),
                        Self::Dmabuf(dmabuf) => {
                            let frame = dmabuf.frame;
                            let size = unsafe { libc::lseek(frame.fds[0], 0, libc::SEEK_END) };
                            let memory = unsafe {
                                gst_dmabuf_allocator_alloc(
                                    allocator,
                                    libc::dup(frame.fds[0]),
                                    size as usize,
                                )
                            };
                            let mut buffer = gst::Buffer::new();
                            let writable = buffer.get_mut().unwrap();
                            writable.append_memory(unsafe {
                                gst::glib::translate::from_glib_full(memory)
                            });
                            gstreamer_video::VideoMeta::add_full(
                                writable,
                                gstreamer_video::VideoFrameFlags::empty(),
                                gstreamer_video::VideoFormat::DmaDrm,
                                frame.width as u32,
                                frame.height as u32,
                                &[frame.offsets[0] as usize],
                                &[frame.pitches[0] as i32],
                            )
                            .unwrap();
                            buffer
                        }
                    };
                    if source.push_buffer(buffer).is_err() {
                        break;
                    }
                    PUSHED.fetch_add(1, Ordering::Relaxed);
                    vblank += period;
                    random ^= random << 13;
                    random ^= random >> 7;
                    random ^= random << 17;
                    let delay = Duration::from_micros(random % (jitter + 1));
                    std::thread::sleep((vblank + delay).saturating_duration_since(Instant::now()));
                }
                unsafe { gst::ffi::gst_object_unref(allocator.cast()) };
            });
        }
    }

    /// Show the encoder's own logs (path, fallback) with `RUST_LOG`.
    fn test_tracing() {
        let _ = tracing_subscriber::fmt()
            .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
            .with_test_writer()
            .try_init();
    }

    fn process_cpu() -> Duration {
        let mut usage: libc::rusage = unsafe { std::mem::zeroed() };
        unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut usage) };
        let time = |t: libc::timeval| {
            Duration::from_secs(t.tv_sec as u64) + Duration::from_micros(t.tv_usec as u64)
        };
        time(usage.ru_utime) + time(usage.ru_stime)
    }

    /// Run `encoder()` on a test capture for `seconds` after a warm-up and
    /// return (encoder label, frames encoded, host CPU ms/frame, capture→packet
    /// ms, submit→packet ms, frames captured, last decoded frame).
    #[allow(clippy::type_complexity)]
    fn run_capture(
        kind: EncoderKind,
        capture: TestCapture,
        size: (u32, u32),
        fps: u32,
        seconds: f64,
    ) -> Result<(String, u64, f64, f64, f64, u64, crate::ffmpeg::Frame)> {
        let description = capture.description(size, fps)?;
        let capture = Arc::new(capture);
        let feeder = capture.clone();
        *PIPELINE_HOOK.lock().unwrap() = Some(Box::new(move |pipeline: &gst::Pipeline| {
            feeder.feed(pipeline, fps)
        }));
        let mut broadcast = moq_net::broadcast::Info::new().produce();
        let track = broadcast.create_track(VideoCodec::H264.track(), None)?;
        let mut subscriber = track.subscribe(None);
        let encode = encoder(
            &description,
            size,
            fps,
            crate::protocol::auto_bitrate(size.0, size.1, fps),
            track,
            kind,
            VideoCodec::H264.into(),
        );
        *PIPELINE_HOOK.lock().unwrap() = None;
        let encode = encode?;
        let metrics = encode.encoder_metrics()?;
        std::thread::sleep(Duration::from_millis(500));
        let (before, cpu_before, pushed_before) = (
            *metrics.totals.lock().unwrap(),
            process_cpu(),
            PUSHED.load(Ordering::Relaxed),
        );
        std::thread::sleep(Duration::from_secs_f64(seconds));
        let (after, cpu_after, pushed_after) = (
            *metrics.totals.lock().unwrap(),
            process_cpu(),
            PUSHED.load(Ordering::Relaxed),
        );
        encode.error()?;
        let label = metrics.encoder.clone();
        let frames = after.2 - before.2;
        ensure!(frames > 0, "{label}: no frames encoded");
        let per_frame = |sum: u64| sum as f64 / frames as f64 / 1000.0;
        // Decode what is being sent: the newest complete group.
        let mut packets = Vec::new();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        let deadline = tokio::time::Instant::now() + Duration::from_millis(600);
        runtime.block_on(async {
            while let Ok(Ok(Some(mut group))) =
                tokio::time::timeout_at(deadline, subscriber.recv_group()).await
            {
                let mut frames = Vec::new();
                while let Ok(Ok(Some(packet))) = tokio::time::timeout(
                    Duration::from_millis(50),
                    crate::protocol::read_frame(&mut group, crate::protocol::MAX_FRAME),
                )
                .await
                {
                    frames.push(packet.to_vec());
                }
                if !frames.is_empty() {
                    packets = frames;
                }
            }
        });
        drop(encode);
        let mut decoder = Codec::new(Config {
            width: size.0,
            height: size.1,
            ..config("software", VideoCodec::H264.into(), 0)
        })?;
        let mut last = None;
        for (index, packet) in packets.iter().enumerate() {
            decoder.submit(packet, index as u64)?;
            while let Some(frame) = decoder.receive()? {
                last = Some(frame);
            }
        }
        Ok((
            label,
            frames,
            (cpu_after - cpu_before).as_secs_f64() * 1000.0 / frames as f64,
            per_frame(after.3 - before.3),
            per_frame(after.1 - before.1),
            pushed_after - pushed_before,
            last.context("nothing decoded")?,
        ))
    }

    fn check_colors(frame: &crate::ffmpeg::Frame, label: &str) -> Result<()> {
        let (width, height) = (frame.width as usize, frame.height as usize);
        for (quadrant, (x, y)) in [
            (width / 4, height / 4),
            (width * 3 / 4, height / 4),
            (width / 4, height * 3 / 4),
            (width * 3 / 4, height * 3 / 4),
        ]
        .into_iter()
        .enumerate()
        {
            let actual = &frame.bytes[(y * width + x) * 3..][..3];
            for channel in 0..3 {
                ensure!(
                    (COLORS[quadrant][channel] as i32 - actual[channel] as i32).abs() <= 8,
                    "{label}: pixel ({x},{y}) {actual:?}, source {:?}",
                    COLORS[quadrant]
                );
            }
        }
        Ok(())
    }

    /// GPUs with a zero-copy encoder here: NVENC fed by the NVIDIA EGL
    /// device, VA-API fed by the first render node's EGL device.
    fn zero_copy_targets() -> Vec<(EncoderKind, Option<&'static str>)> {
        vec![
            (EncoderKind::Nvidia, None),
            (EncoderKind::Vaapi, Some("/dev/dri/renderD128")),
        ]
    }

    /// DMA-BUF capture through the real encoder pipeline on each GPU: the
    /// decoded stream shows the captured colours, the active path is
    /// reported, and a source that cannot negotiate DMA-BUFs falls back to
    /// CPU capture (logged, still streaming).
    #[test]
    #[ignore = "requires NVENC or VA-API and an EGL device exporting DMA-BUFs"]
    fn zero_copy_capture_pipeline_and_fallback() -> Result<()> {
        if std::env::var("TELEPORT_TEST_HARDWARE").as_deref() != Ok("1") {
            eprintln!("SKIP: set TELEPORT_TEST_HARDWARE=1 on a machine with NVENC or VA-API");
            return Ok(());
        }
        test_tracing();
        gst::init()?;
        ZERO_COPY_FAILED.store(false, Ordering::Relaxed);
        let size = (1280, 720);
        let mut passed = 0;
        for (kind, node) in zero_copy_targets() {
            let Some(capture) = TestCapture::new(true, node, size)? else {
                eprintln!("SKIP {kind:?}: no EGL device exporting DMA-BUFs");
                continue;
            };
            let (label, frames, ..) = match run_capture(kind, capture, size, 60, 1.0) {
                Ok(result) => result,
                Err(error) if ZERO_COPY_FAILED.load(Ordering::Relaxed) => {
                    return Err(error.context(format!("{kind:?} zero-copy capture failed")));
                }
                Err(error) => {
                    eprintln!("SKIP {kind:?}: {error:#}");
                    continue;
                }
            };
            ensure!(
                label.ends_with(" DMA-BUF zero-copy"),
                "{kind:?} did not use zero-copy: {label}"
            );
            let (.., decoded) = run_capture(
                kind,
                TestCapture::new(true, node, size)?.context("source vanished")?,
                size,
                60,
                0.3,
            )?;
            check_colors(&decoded, &label)?;
            ensure!(!ZERO_COPY_FAILED.load(Ordering::Relaxed));
            eprintln!("PASS {label}: {frames} frames");
            passed += 1;
        }
        ensure!(passed > 0, "no zero-copy encoder available");
        // A "DMA-BUF" source that only produces CPU memory: negotiation
        // fails, the encoder falls back to CPU capture and keeps streaming.
        let kind = zero_copy_targets()
            .into_iter()
            .map(|(kind, _)| kind)
            .find(|kind| {
                let capture = TestCapture::new(false, None, size).unwrap().unwrap();
                run_capture(*kind, capture, size, 60, 0.2).is_ok()
            })
            .context("no encoder for the fallback check")?;
        let pipeline = encoder(
            "videotestsrc is-live=true ! identity name=memory:DMABuf",
            size,
            30,
            4000,
            moq_net::broadcast::Info::new()
                .produce()
                .create_track(VideoCodec::H264.track(), None)?,
            kind,
            VideoCodec::H264.into(),
        )?;
        let metrics = pipeline.encoder_metrics()?;
        ensure!(
            !metrics.encoder.contains("zero-copy"),
            "{}",
            metrics.encoder
        );
        ensure!(
            ZERO_COPY_FAILED.load(Ordering::Relaxed),
            "fallback not recorded"
        );
        std::thread::sleep(Duration::from_millis(500));
        pipeline.error()?;
        ensure!(
            metrics.totals.lock().unwrap().2 > 0,
            "fallback encoded nothing"
        );
        ZERO_COPY_FAILED.store(false, Ordering::Relaxed);
        eprintln!("PASS fallback to CPU capture on {kind:?}");
        Ok(())
    }

    /// The real desktop through the xdg-desktop-portal: whatever the
    /// compositor negotiates (DMA-BUF unless `TELEPORT_CAPTURE_ZERO_COPY=0`).
    /// Opens its own portal session (no restore token, no input).
    #[tokio::test]
    #[ignore = "opens a desktop portal session; set TELEPORT_TEST_PORTAL=1"]
    async fn portal_capture_benchmark() -> Result<()> {
        if std::env::var("TELEPORT_TEST_PORTAL").as_deref() != Ok("1") {
            eprintln!("SKIP: set TELEPORT_TEST_PORTAL=1 inside a graphical session");
            return Ok(());
        }
        test_tracing();
        gst::init()?;
        let kind = match std::env::var("TELEPORT_TEST_ENCODER").as_deref() {
            Ok("vaapi") => EncoderKind::Vaapi,
            _ => EncoderKind::Nvidia,
        };
        let fps = std::env::var("TELEPORT_BENCH_FPS")
            .ok()
            .and_then(|fps| fps.parse().ok())
            .unwrap_or(60);
        let mut capture =
            crate::capture::Capture::open_with_options("portal", None, 0, false).await?;
        let size = (capture.width & !1, capture.height & !1);
        // Count what PipeWire delivers before the rate limit, and keep the
        // source to report the format the compositor negotiated.
        let source = Arc::new(Mutex::new(None::<gst::Element>));
        let found = source.clone();
        *PIPELINE_HOOK.lock().unwrap() = Some(Box::new(move |pipeline: &gst::Pipeline| {
            let Some(element) = pipeline
                .iterate_elements()
                .into_iter()
                .flatten()
                .find(|element| {
                    element
                        .factory()
                        .is_some_and(|factory| factory.name() == "pipewiresrc")
                })
            else {
                return;
            };
            element
                .static_pad("src")
                .unwrap()
                .add_probe(gst::PadProbeType::BUFFER, |_, _| {
                    PUSHED.fetch_add(1, Ordering::Relaxed);
                    gst::PadProbeReturn::Ok
                });
            *found.lock().unwrap() = Some(element);
        }));
        let encode = encoder(
            &capture.pipeline_source(),
            size,
            fps,
            crate::protocol::auto_bitrate(size.0, size.1, fps),
            moq_net::broadcast::Info::new()
                .produce()
                .create_track(VideoCodec::H264.track(), None)?,
            kind,
            VideoCodec::H264.into(),
        )?;
        *PIPELINE_HOOK.lock().unwrap() = None;
        let metrics = encode.encoder_metrics()?;
        tokio::time::sleep(Duration::from_millis(500)).await;
        if let Some(caps) = source
            .lock()
            .unwrap()
            .as_ref()
            .and_then(|source| source.static_pad("src")?.current_caps())
        {
            eprintln!("PipeWire negotiated {caps}");
        }
        let (before, cpu_before, delivered_before) = (
            *metrics.totals.lock().unwrap(),
            process_cpu(),
            PUSHED.load(Ordering::Relaxed),
        );
        tokio::time::sleep(Duration::from_secs(5)).await;
        let (after, cpu_after, delivered_after) = (
            *metrics.totals.lock().unwrap(),
            process_cpu(),
            PUSHED.load(Ordering::Relaxed),
        );
        encode.error()?;
        let frames = after.2 - before.2;
        ensure!(frames > 0, "no frames captured");
        let per_frame = |sum: u64| sum as f64 / frames as f64 / 1000.0;
        eprintln!(
            "BENCH portal {} {}x{} at {fps}: {} delivered, {frames} frames encoded, CPU {:.2} ms/frame, capture->packet {:.2} ms, submit->packet {:.2} ms",
            metrics.encoder,
            size.0,
            size.1,
            delivered_after - delivered_before,
            (cpu_after - cpu_before).as_secs_f64() * 1000.0 / frames as f64,
            per_frame(after.3 - before.3),
            per_frame(after.1 - before.1),
        );
        drop(encode);
        capture.close().await;
        Ok(())
    }

    /// The portal's cursor metadata mode (G11): negotiates on this
    /// compositor, rides on the captured (DMA-BUF unless
    /// `TELEPORT_CAPTURE_ZERO_COPY=0`) buffers and reaches the cursor hub.
    /// Opens its own portal session (no restore token, no input); a shape
    /// arrives only while the pointer is on the captured monitor.
    #[tokio::test]
    #[ignore = "opens a desktop portal session; set TELEPORT_TEST_PORTAL=1"]
    async fn portal_cursor_metadata() -> Result<()> {
        if std::env::var("TELEPORT_TEST_PORTAL").as_deref() != Ok("1") {
            eprintln!("SKIP: set TELEPORT_TEST_PORTAL=1 inside a graphical session");
            return Ok(());
        }
        test_tracing();
        gst::init()?;
        let mut capture =
            crate::capture::Capture::open_with_options("portal", None, 0, true).await?;
        ensure!(
            capture.cursor_metadata(),
            "the compositor did not offer cursor metadata"
        );
        let mut feed = crate::pw_cursor::subscribe();
        let mut seen = false;
        // The pointer is on one monitor; only that stream carries its shape.
        for monitor in 0..capture.monitors.len() {
            capture.select_monitor(monitor).await?;
            let size = (capture.width & !1, capture.height & !1);
            let (metas, stale) = (
                crate::pw_cursor::METAS.load(Ordering::Relaxed),
                crate::pw_cursor::STALE.load(Ordering::Relaxed),
            );
            let encode = encoder(
                &capture.pipeline_source(),
                size,
                60,
                crate::protocol::auto_bitrate(size.0, size.1, 60),
                moq_net::broadcast::Info::new()
                    .produce()
                    .create_track(VideoCodec::H264.track(), None)?,
                EncoderKind::Nvidia,
                VideoCodec::H264.into(),
            )?;
            let metrics = encode.encoder_metrics()?;
            tokio::time::sleep(Duration::from_secs(2)).await;
            encode.error()?;
            let metas = crate::pw_cursor::METAS.load(Ordering::Relaxed) - metas;
            seen |= metas > 0;
            eprintln!(
                "CURSOR monitor {monitor} {}x{} {}: {metas} buffers with cursor meta, {} stale (cursor-only) dropped, {} frames encoded, shape {:?}, position {:?}",
                size.0,
                size.1,
                metrics.encoder,
                crate::pw_cursor::STALE.load(Ordering::Relaxed) - stale,
                metrics.totals.lock().unwrap().2,
                feed.shapes.borrow_and_update().clone(),
                *feed.positions.borrow_and_update(),
            );
            drop(encode);
        }
        ensure!(seen, "no cursor meta on the captured buffers");
        capture.close().await;
        Ok(())
    }

    /// Host cost per frame, CPU capture vs DMA-BUF zero-copy, at 1080p and
    /// 4K on each GPU (60 FPS live test source; whole-process CPU time, so
    /// it includes driver threads).
    #[test]
    #[ignore = "benchmark; requires NVENC or VA-API and an EGL device exporting DMA-BUFs"]
    fn zero_copy_capture_benchmark() -> Result<()> {
        if std::env::var("TELEPORT_TEST_HARDWARE").as_deref() != Ok("1") {
            eprintln!("SKIP: set TELEPORT_TEST_HARDWARE=1 on a machine with NVENC or VA-API");
            return Ok(());
        }
        test_tracing();
        gst::init()?;
        ZERO_COPY_FAILED.store(false, Ordering::Relaxed);
        let seconds = std::env::var("TELEPORT_BENCH_SECONDS")
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(4.0);
        for (kind, node) in zero_copy_targets() {
            for size in [(1920, 1080), (3840, 2160)] {
                for dmabuf in [false, true] {
                    let Some(capture) = TestCapture::new(dmabuf, node, size)? else {
                        eprintln!("SKIP {kind:?}: no EGL device exporting DMA-BUFs");
                        continue;
                    };
                    match run_capture(kind, capture, size, 60, seconds) {
                        Ok((label, frames, cpu, capture, encode, _, decoded)) => {
                            check_colors(&decoded, &label)?;
                            eprintln!(
                                "BENCH {label} {}x{}: {frames} frames, CPU {cpu:.2} ms/frame, capture->packet {capture:.2} ms, submit->packet {encode:.2} ms",
                                size.0, size.1
                            );
                        }
                        Err(error) => {
                            eprintln!("SKIP {kind:?} {size:?} dmabuf={dmabuf}: {error:#}")
                        }
                    }
                }
            }
        }
        ensure!(
            !ZERO_COPY_FAILED.load(Ordering::Relaxed),
            "zero-copy fell back"
        );
        Ok(())
    }

    /// High refresh rates: a 2560x1440 (`TELEPORT_BENCH_SIZE`) DMA-BUF source
    /// at 60/120/180/240 FPS (`TELEPORT_BENCH_FPS`) through NVENC zero-copy.
    /// Reports frames encoded against frames captured, so a rate limit or a
    /// slow stage shows up as a lower ratio. Static content: NVENC's own cost on busy
    /// content is higher than here.
    #[test]
    #[ignore = "benchmark; requires NVENC and an EGL device exporting DMA-BUFs"]
    fn high_rate_capture_benchmark() -> Result<()> {
        if std::env::var("TELEPORT_TEST_HARDWARE").as_deref() != Ok("1") {
            eprintln!("SKIP: set TELEPORT_TEST_HARDWARE=1 on a machine with NVENC");
            return Ok(());
        }
        test_tracing();
        gst::init()?;
        ZERO_COPY_FAILED.store(false, Ordering::Relaxed);
        let seconds = std::env::var("TELEPORT_BENCH_SECONDS")
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(4.0);
        let rates: Vec<u32> = std::env::var("TELEPORT_BENCH_FPS")
            .ok()
            .map(|list| list.split(',').filter_map(|fps| fps.parse().ok()).collect())
            .unwrap_or_else(|| vec![60, 120, 180, 240]);
        let size = std::env::var("TELEPORT_BENCH_SIZE")
            .ok()
            .and_then(|size| {
                let (width, height) = size.split_once('x')?;
                Some((width.parse().ok()?, height.parse().ok()?))
            })
            .unwrap_or((2560, 1440));
        for fps in rates {
            let Some(capture) = TestCapture::new(true, None, size)? else {
                eprintln!("SKIP: no EGL device exporting DMA-BUFs");
                return Ok(());
            };
            let (label, frames, cpu, capture, encode, pushed, decoded) =
                run_capture(EncoderKind::Nvidia, capture, size, fps, seconds)?;
            check_colors(&decoded, &label)?;
            eprintln!(
                "BENCH {label} {}x{} at {fps}: {:.1} captured/s, {:.1} encoded/s ({:.1}%), CPU {cpu:.2} ms/frame, capture->packet {capture:.2} ms, submit->packet {encode:.2} ms",
                size.0,
                size.1,
                pushed as f64 / seconds,
                frames as f64 / seconds,
                frames as f64 * 100.0 / pushed.max(1) as f64,
            );
        }
        ensure!(
            !ZERO_COPY_FAILED.load(Ordering::Relaxed),
            "zero-copy fell back"
        );
        Ok(())
    }
}
fn neutral_frame(width: u32, height: u32, hdr: bool) -> Vec<u8> {
    let pixels = width as usize * height as usize;
    if hdr {
        (0..pixels * 3 / 2)
            .flat_map(|index| {
                (if index < pixels {
                    256_u16 << 6
                } else {
                    512_u16 << 6
                })
                .to_le_bytes()
            })
            .collect()
    } else {
        let mut raw = vec![90; pixels];
        raw.extend(vec![128; pixels / 2]);
        raw
    }
}

/// Decode the synthetic stream with `name` and require timestamped frames of
/// the probe size (and, for output 3, GPU-resident frames).
fn probe_decoder(name: &str, format: VideoFormat, packets: &[Vec<u8>], output: i32) -> Result<()> {
    let mut decoder = Codec::new(config(name, format, output))?;
    let mut frames = 0;
    for (index, packet) in packets.iter().enumerate() {
        decoder.submit(packet, index as u64 + 1)?;
        while let Some(frame) = decoder.receive()? {
            ensure!(
                (frame.width, frame.height) == (64, 48),
                "invalid probe size"
            );
            ensure!(frame.pts.is_some(), "decoder lost frame timestamps");
            ensure!(
                output != 3 || frame.gpu.is_some(),
                "decoder did not keep frames on the GPU"
            );
            frames += 1;
        }
    }
    ensure!(frames > 0, "decoder produced no verified output");
    Ok(())
}

/// Whether `name` (e.g. "vaapi@/dev/dri/renderD128", the presenter's own GPU)
/// decodes GPU-resident frames of `format`. Cached per process.
#[cfg(target_os = "linux")]
fn gpu_decoder_verified(name: &str, format: VideoFormat) -> bool {
    type Cache = BTreeMap<(String, bool), bool>;
    static CACHE: OnceLock<Mutex<Cache>> = OnceLock::new();
    let mut cache = CACHE.get_or_init(Mutex::default).lock().unwrap();
    let key = (name.to_owned(), format.codec == VideoCodec::H265);
    if let Some(&verified) = cache.get(&key) {
        return verified;
    }
    let verified = match synthetic(format)
        .and_then(|packets| probe_decoder(name, format, &packets, 3))
    {
        Ok(()) => {
            tracing::info!(decoder = name, codec = ?format.codec, "GPU-resident decoder produced verified output");
            true
        }
        Err(error) => {
            tracing::warn!(decoder = name, %error, "GPU-resident decoder unavailable");
            false
        }
    };
    cache.insert(key, verified);
    verified
}

pub(super) fn select_decoder(software: bool, format: VideoFormat) -> Result<String> {
    type Cache = BTreeMap<(bool, bool, bool), String>;
    static CACHE: OnceLock<Mutex<Cache>> = OnceLock::new();
    format.validate()?;
    let mut cache = CACHE.get_or_init(Mutex::default).lock().unwrap();
    let key = (software, format.codec == VideoCodec::H265, format.hdr());
    if let Some(name) = cache.get(&key) {
        return Ok(name.clone());
    }
    let packets = synthetic(format)?;
    let candidates: &[&str] = if software {
        &["software"]
    } else if cfg!(target_os = "macos") {
        &["videotoolbox", "software"]
    } else {
        &["cuda", "vaapi", "qsv", "software"]
    };
    let mut last = None;
    for &name in candidates {
        match probe_decoder(name, format, &packets, if format.hdr() { 2 } else { 1 }) {
            Ok(()) => {
                tracing::info!(backend="ffmpeg", decoder=name, codec=?format.codec, "direct decoder produced verified output");
                cache.insert(key, name.into());
                return Ok(name.into());
            }
            Err(error) => {
                tracing::warn!(decoder=name, %error, "FFmpeg decoder unavailable; trying fallback");
                last = Some(error);
            }
        }
    }
    Err(last.context("no FFmpeg decoder candidates")?)
}

pub(super) fn decoder(
    software: bool,
    stats: Arc<crate::stats::StreamStats>,
    format: VideoFormat,
    nv12: bool,
    native: bool,
    gpu: Option<&str>,
) -> Result<(Pipeline, AppSrc, LatestImage)> {
    format.validate()?;
    ensure!(
        !native,
        "direct FFmpeg native-surface presentation is not integrated yet; unset TELEPORT_MAC_GPU_VIDEO or explicitly select TELEPORT_VIDEO_BACKEND=gstreamer"
    );
    let selected = select_decoder(software, format)?;
    // GPU-resident frames only for an 8-bit decode on the presenter's own GPU
    // ("cuda", or "vaapi@<render node>" of its EGL display).
    #[cfg(target_os = "linux")]
    let gpu = gpu.filter(|preferred| {
        !software
            && !format.hdr()
            && (*preferred == selected || gpu_decoder_verified(preferred, format))
    });
    #[cfg(not(target_os = "linux"))]
    let gpu = gpu.filter(|preferred| !format.hdr() && *preferred == selected);
    let (name, output) = if format.hdr() {
        (selected, 2)
    } else if let Some(preferred) = gpu {
        (preferred.to_owned(), 3)
    } else if nv12 {
        (selected, 1)
    } else {
        (selected, 0)
    };
    let state = Arc::new(State {
        codec: Mutex::new(Codec::new(config(&name, format, output))?),
        keyframe: Default::default(),
        key_interval: Default::default(),
        #[cfg(target_os = "linux")]
        metrics: None,
        #[cfg(target_os = "linux")]
        reopen: Default::default(),
    });
    *stats.decoder.lock().unwrap() = format!("ffmpeg:{}:{name}", format.codec.track());
    let pipeline = Pipeline(gst::parse::launch(&format!(
        "appsrc name=in is-live=true format=time max-bytes=8388608 block=false caps={},stream-format=byte-stream,alignment=au ! appsink name=compressed sync=false max-buffers=1 drop=false",
        compressed_caps(format.codec)
    ))?.downcast::<gst::Pipeline>().map_err(|_| anyhow::anyhow!("not a pipeline"))?);
    attach(&pipeline.0, state.clone());
    let source = pipeline
        .0
        .by_name("in")
        .unwrap()
        .downcast::<AppSrc>()
        .unwrap();
    let sink = pipeline
        .0
        .by_name("compressed")
        .unwrap()
        .downcast::<AppSink>()
        .unwrap();
    let image: LatestImage = Arc::new(Mutex::new(None));
    let latest = image.clone();
    sink.set_callbacks(
        gstreamer_app::AppSinkCallbacks::builder()
            .new_sample(move |sink| {
                let sample = sink.pull_sample().map_err(|_| gst::FlowError::Flushing)?;
                let process = || -> Result<()> {
                    let buffer = sample
                        .buffer()
                        .context("compressed sample missing buffer")?;
                    let key = video_reference(buffer).context("missing receive identity")?;
                    stats.decoder_entered(key);
                    let bytes = buffer.map_readable()?;
                    let mut codec = state.codec.lock().unwrap();
                    codec.submit(bytes.as_slice(), key)?;
                    while let Some(frame) = codec.receive()? {
                        if let Some(key) = frame.pts {
                            stats.decoder_output(key);
                        }
                        let stale = frame
                            .pts
                            .and_then(|key| stats.frame_age(key))
                            .is_some_and(|age| age > Duration::from_millis(250));
                        let group = stats.decoded(frame.pts);
                        if stale {
                            stats.stale_frames.fetch_add(1, Ordering::Relaxed);
                            continue;
                        }
                        let width = frame.width;
                        let height = frame.height;
                        let mut data = frame.bytes;
                        let mut nv12 = None;
                        let mut hdr = None;
                        let gpu_frame = frame.gpu;
                        let stride = width as usize
                            * if output == 0 {
                                3
                            } else if output == 2 {
                                2
                            } else {
                                1
                            };
                        if output == 3 {
                            ensure!(gpu_frame.is_some(), "decoder returned no GPU frame");
                        } else if output != 0 {
                            ensure!(
                                data.len() == stride * height as usize * 3 / 2,
                                "invalid direct decoder plane size"
                            );
                            let uv = data.split_off(stride * height as usize);
                            if output == 2 {
                                hdr = Some(HdrImage {
                                    y: std::mem::take(&mut data),
                                    uv,
                                    y_stride: stride,
                                    uv_stride: stride,
                                });
                            } else {
                                nv12 = Some(Nv12Image {
                                    y: std::mem::take(&mut data),
                                    uv,
                                    y_stride: stride,
                                    uv_stride: stride,
                                });
                            }
                        }
                        let mut latest = latest.lock().unwrap();
                        if latest.is_some() {
                            stats.overwritten_frames.fetch_add(1, Ordering::Relaxed);
                        }
                        *latest = Some(Image {
                            decoded_at: Instant::now(),
                            decoder_recovery: stats.decoder_recoveries.load(Ordering::Relaxed),
                            video_group: group,
                            data,
                            width,
                            height,
                            stride,
                            hdr,
                            nv12,
                            surface: None,
                            gpu: gpu_frame,
                        });
                        if group.is_some() {
                            stats.ready_frames.fetch_add(1, Ordering::Relaxed);
                        }
                        drop(latest);
                        wake_presenter();
                    }
                    Ok(())
                };
                // One corrupt or truncated access unit must not end the session.
                // The receiver sees the counter, abandons this GOP and resets
                // the decoder at the next keyframe; its recovery budget still
                // ends sessions whose decoder never makes progress.
                if let Err(error) = process() {
                    tracing::warn!(%error, "direct FFmpeg decode failed; recovering at next keyframe");
                    stats.decode_errors.fetch_add(1, Ordering::Relaxed);
                }
                Ok(gst::FlowSuccess::Ok)
            })
            .build(),
    );
    pipeline.0.set_state(gst::State::Playing)?;
    Ok((pipeline, source, image))
}

/// Set when DMA-BUF capture failed in this process (negotiation, import or
/// conversion). Later encoders capture through the CPU path instead of
/// retrying a broken driver path.
#[cfg(target_os = "linux")]
static ZERO_COPY_FAILED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// How long a new DMA-BUF capture may take to encode its first frame before
/// it is trusted; a failure within this window falls back to CPU capture.
#[cfg(target_os = "linux")]
const DMABUF_VERIFY: Duration = Duration::from_secs(2);

/// RGB DRM formats offered for DMA-BUF capture (KWin, Mutter and wlroots
/// share 8-bit desktops as one of these).
#[cfg(target_os = "linux")]
const DMABUF_FORMATS: [&str; 4] = ["XR24", "AR24", "XB24", "AB24"];

/// The DMA-BUF variant of a capture description, if the source can produce
/// DMA-BUFs: a PipeWire source without its system-memory caps (pipewiresrc
/// then offers our modifiers to the compositor), or a test source that
/// already negotiates `memory:DMABuf`. `TELEPORT_CAPTURE_ZERO_COPY=0` opts
/// out.
#[cfg(target_os = "linux")]
fn dmabuf_source(source: &str) -> Option<String> {
    if std::env::var("TELEPORT_CAPTURE_ZERO_COPY").as_deref() == Ok("0") {
        return None;
    }
    if source.contains("memory:DMABuf") {
        return Some(source.into());
    }
    source
        .strip_suffix(&format!(" ! {}", crate::capture::PIPEWIRE_SYSTEM_MEMORY))
        .filter(|source| source.starts_with("pipewiresrc ") && !source.contains(" ! "))
        .map(Into::into)
}

/// `memory:DMABuf` caps listing every format/modifier `codec` can import, or
/// `None` when it can import none.
#[cfg(target_os = "linux")]
fn dmabuf_caps(codec: &mut Codec) -> Result<Option<gst::Caps>> {
    let mut formats = Vec::new();
    for name in DMABUF_FORMATS {
        let (fourcc, _) = gstreamer_video::dma_drm_fourcc_from_str(name)?;
        for modifier in codec.dmabuf_modifiers(fourcc)? {
            formats.push(gstreamer_video::dma_drm_fourcc_to_string(fourcc, modifier).to_string());
        }
    }
    if formats.is_empty() {
        return Ok(None);
    }
    Ok(Some(
        gst::Caps::builder("video/x-raw")
            .features(["memory:DMABuf"])
            .field("format", "DMA_DRM")
            .field("drm-format", gst::List::new(formats))
            .build(),
    ))
}

#[cfg(target_os = "linux")]
unsafe extern "C" {
    fn gst_is_dmabuf_memory(memory: *mut gst::ffi::GstMemory) -> gst::glib::ffi::gboolean;
    fn gst_dmabuf_memory_get_fd(memory: *mut gst::ffi::GstMemory) -> std::ffi::c_int;
}

/// Describe a captured DMA-BUF sample for the encoder. The descriptors
/// belong to the sample, which must outlive their use.
#[cfg(target_os = "linux")]
fn dmabuf_frame(sample: &gst::Sample) -> Result<crate::ffmpeg::Dmabuf> {
    let info = gstreamer_video::VideoInfoDmaDrm::from_caps(
        sample.caps().context("missing capture caps")?,
    )?;
    let buffer = sample.buffer().context("missing capture buffer")?;
    // pipewiresrc (and any DMA-BUF producer) describes plane layout here.
    let meta = buffer
        .meta::<gstreamer_video::VideoMeta>()
        .context("DMA-BUF capture without plane layout")?;
    let planes = meta.n_planes() as usize;
    ensure!((1..=4).contains(&planes), "unsupported DMA-BUF plane count");
    let mut frame = crate::ffmpeg::Dmabuf {
        fds: [-1; 4],
        planes: planes as i32,
        fourcc: info.fourcc(),
        modifier: info.modifier(),
        width: info.width().try_into()?,
        height: info.height().try_into()?,
        crop_width: info.width().try_into()?,
        crop_height: info.height().try_into()?,
        ..Default::default()
    };
    for plane in 0..planes {
        let offset = meta.offset()[plane];
        let (memories, skip) = buffer
            .find_memory(offset..offset + 1)
            .context("DMA-BUF plane outside its buffer")?;
        let memory = buffer.peek_memory(memories.start);
        let pointer = memory.as_ptr() as *mut gst::ffi::GstMemory;
        ensure!(
            unsafe { gst_is_dmabuf_memory(pointer) } != 0,
            "capture memory is not a DMA-BUF"
        );
        frame.fds[plane] = unsafe { gst_dmabuf_memory_get_fd(pointer) };
        frame.offsets[plane] = (memory.offset() + skip).try_into()?;
        frame.pitches[plane] = meta.stride()[plane].try_into()?;
    }
    // The compositor may share a larger buffer than the visible area.
    if let Some(crop) = buffer.meta::<gstreamer_video::VideoCropMeta>() {
        let (x, y, width, height) = crop.rect();
        if width > 0 && height > 0 {
            frame.crop_x = x.try_into()?;
            frame.crop_y = y.try_into()?;
            frame.crop_width = width.try_into()?;
            frame.crop_height = height.try_into()?;
        }
    }
    Ok(frame)
}

#[cfg(target_os = "linux")]
fn encoder_name(kind: EncoderKind, codec: VideoCodec) -> String {
    let prefix = if codec == VideoCodec::H265 {
        "hevc"
    } else {
        "h264"
    };
    match kind {
        EncoderKind::Nvidia => format!("{prefix}_nvenc"),
        EncoderKind::Qsv => format!("{prefix}_qsv"),
        EncoderKind::Vaapi => format!("{prefix}_vaapi"),
        _ if codec == VideoCodec::H265 => "libx265".into(),
        _ => "libx264".into(),
    }
}

/// Opens `config` and requires a keyframe from three neutral frames.
#[cfg(target_os = "linux")]
fn probe_encoder(config: &Config) -> Result<()> {
    let mut codec = Codec::new(config.clone())?;
    let raw = neutral_frame(config.width, config.height, config.hdr);
    let mut emitted = false;
    for n in 0..3 {
        codec.submit(&raw, n)?;
        while let Some(packet) = codec.receive()? {
            emitted |= packet.key;
        }
    }
    ensure!(emitted, "encoder produced no independent keyframe");
    Ok(())
}

/// The first hardware encoder for an 8-bit `codec` stream, in the order
/// `encoder` tries them for `kind`, probed at a small size (720p). `None`
/// when only software would encode it. Cached per process: hosts advertise
/// this to clients choosing a codec automatically.
#[cfg(target_os = "linux")]
pub(super) fn hardware_encoder(kind: EncoderKind, codec: VideoCodec) -> Option<String> {
    type Cache = BTreeMap<(String, bool), Option<String>>;
    static CACHE: OnceLock<Mutex<Cache>> = OnceLock::new();
    let mut cache = CACHE.get_or_init(Mutex::default).lock().unwrap();
    let key = (format!("{kind:?}"), codec == VideoCodec::H265);
    if let Some(found) = cache.get(&key) {
        return found.clone();
    }
    let kinds: &[EncoderKind] = match kind {
        EncoderKind::Auto => &[EncoderKind::Nvidia, EncoderKind::Qsv, EncoderKind::Vaapi],
        EncoderKind::Software => &[],
        _ => std::slice::from_ref(&kind),
    };
    let format = VideoFormat {
        codec,
        dynamic_range: crate::protocol::DynamicRange::Sdr,
    };
    let found = kinds.iter().find_map(|&choice| {
        let config = Config {
            encoder: true,
            name: encoder_name(choice, codec),
            width: 1280,
            height: 720,
            fps: 60,
            kbps: 8000,
            gop: 60,
            ..config("", format, 1)
        };
        match probe_encoder(&config) {
            Ok(()) => Some(config.name),
            Err(error) => {
                tracing::debug!(encoder = %config.name, %error, "no hardware encoder");
                None
            }
        }
    });
    tracing::info!(?codec, encoder = ?found, "hardware encoder probe");
    cache.insert(key, found.clone());
    found
}

#[cfg(target_os = "linux")]
pub(super) fn encoder(
    source: &str,
    size: (u32, u32),
    fps: u32,
    bitrate: u32,
    track: moq_net::track::Producer,
    kind: EncoderKind,
    format: VideoFormat,
) -> Result<Pipeline> {
    format.validate()?;
    crate::protocol::validate_video_size(size.0, size.1)?;
    // Zero-copy capture needs a DMA-BUF source, an 8-bit stream and an
    // encoder whose GPU can import (NVENC through CUDA, or VA-API).
    let zero_copy = dmabuf_source(source)
        .filter(|_| !format.hdr() && !ZERO_COPY_FAILED.load(Ordering::Relaxed));
    // Probing opens and exercises up to four encoders. Quality and monitor
    // changes re-create the encoder inside the session loop, so remember which
    // one worked for this request and only probe again if it later fails.
    type Cache = BTreeMap<(String, bool, bool, u32, u32), String>;
    static ENCODERS: OnceLock<Mutex<Cache>> = OnceLock::new();
    let cache_key = (
        format!("{kind:?}"),
        format.codec == VideoCodec::H265,
        format.hdr(),
        size.0,
        size.1,
    );
    let cached = ENCODERS
        .get_or_init(Mutex::default)
        .lock()
        .unwrap()
        .get(&cache_key)
        .cloned();
    let choices = if matches!(kind, EncoderKind::Auto) {
        vec![
            EncoderKind::Nvidia,
            EncoderKind::Qsv,
            EncoderKind::Vaapi,
            EncoderKind::Software,
        ]
    } else {
        vec![kind]
    };
    let mut selected = None;
    let mut last = None;
    for choice in choices {
        let name = encoder_name(choice, format.codec);
        let config = Config {
            encoder: true,
            gpu_input: zero_copy.is_some()
                && (name.ends_with("_nvenc") || name.ends_with("_vaapi")),
            name,
            width: size.0,
            height: size.1,
            fps,
            kbps: bitrate,
            // The callback forces IDRs at the session interval (see State::
            // key_interval); this is only a ceiling the encoder never reaches.
            gop: fps.saturating_mul(10).max(1),
            ..config("", format, 1)
        };
        if let Some(cached) = &cached {
            if &config.name != cached {
                continue;
            }
            selected = Some(config);
            break;
        }
        match probe_encoder(&config) {
            Ok(()) => {
                selected = Some(config);
                break;
            }
            Err(error) => {
                tracing::warn!(encoder=%config.name,%error,"FFmpeg encoder unavailable; trying fallback");
                last = Some(error);
            }
        }
    }
    let config = match selected {
        Some(config) => config,
        None => return Err(last.context("no FFmpeg encoders")?),
    };
    let name = config.name.clone();
    let codec = Codec::new(config.clone());
    {
        let mut cache = ENCODERS.get_or_init(Mutex::default).lock().unwrap();
        if codec.is_ok() {
            cache.insert(cache_key, name);
        } else {
            cache.remove(&cache_key);
        }
    }
    let mut codec = codec?;
    if let Some(dmabuf) = zero_copy.filter(|_| config.gpu_input) {
        let attempt = dmabuf_caps(&mut codec).and_then(|caps| {
            let caps = caps.context("encoder GPU imports none of the offered DMA-BUF formats")?;
            start(
                &dmabuf,
                Some(caps),
                codec,
                size,
                fps,
                bitrate,
                track.clone(),
                format,
            )
        });
        match attempt {
            Ok(pipeline) => return Ok(pipeline),
            Err(error) => {
                ZERO_COPY_FAILED.store(true, Ordering::Relaxed);
                tracing::warn!(
                    encoder = %config.name,
                    error = format!("{error:#}"),
                    "DMA-BUF zero-copy capture unavailable; using CPU capture"
                );
                codec = Codec::new(config)?;
            }
        }
    }
    start(source, None, codec, size, fps, bitrate, track, format)
}

/// Build and start the capture → encode pipeline around an open encoder.
/// With `dmabuf` caps the source must produce DMA-BUFs, which go to the
/// encoder's GPU without a CPU copy; the first frame is verified before
/// returning (an error means: fall back to CPU capture).
#[cfg(target_os = "linux")]
#[allow(clippy::too_many_arguments)]
fn start(
    source: &str,
    dmabuf: Option<gst::Caps>,
    codec: Codec,
    size: (u32, u32),
    fps: u32,
    bitrate: u32,
    mut track: moq_net::track::Producer,
    format: VideoFormat,
) -> Result<Pipeline> {
    let zero_copy = dmabuf.is_some();
    tracing::info!(
        encoder = %codec.config.name,
        backend = "ffmpeg",
        ?format.codec,
        bitrate,
        capture = if zero_copy { "DMA-BUF zero-copy" } else { "CPU" },
        "direct video encoder"
    );
    let metrics = Arc::new(EncoderMetrics {
        encoder: format!(
            "ffmpeg:{}{}",
            codec.config.name,
            if zero_copy { " DMA-BUF zero-copy" } else { "" }
        ),
        pending: Mutex::default(),
        encode_us: std::sync::atomic::AtomicU64::new(0),
        totals: Mutex::default(),
    });
    let state = Arc::new(State {
        codec: Mutex::new(codec),
        keyframe: Default::default(),
        key_interval: std::sync::atomic::AtomicU32::new((fps / 4).max(1)),
        metrics: Some(metrics.clone()),
        reopen: Default::default(),
    });
    let pixel = if format.hdr() {
        "P010_10LE,colorimetry=bt2100-pq"
    } else {
        "NV12,colorimetry=bt709"
    };
    // One-frame leaky queue: a slow encoder always picks up the newest
    // capture. Rate-limit before converting so dropped frames cost nothing.
    // CPU capture then converts and scales in one multithreaded pass; DMA-BUF
    // capture leaves both to the encoder's GPU.
    let queue = "queue max-size-buffers=1 max-size-bytes=0 max-size-time=0 leaky=downstream ! videorate drop-only=true";
    let description = if zero_copy {
        format!(
            "{source} ! {queue} ! video/x-raw(memory:DMABuf),framerate={fps}/1 ! appsink name=captured sync=false max-buffers=1 drop=false"
        )
    } else {
        format!(
            "{source} ! {queue} ! video/x-raw,framerate={fps}/1 ! videoconvertscale n-threads=0 ! video/x-raw,format={pixel},width={},height={},framerate={fps}/1 ! appsink name=captured sync=false max-buffers=1 drop=false",
            size.0, size.1
        )
    };
    let pipeline = Pipeline(
        gst::parse::launch(&description)?
            .downcast::<gst::Pipeline>()
            .map_err(|_| anyhow::anyhow!("not a capture pipeline"))?,
    );
    attach(&pipeline.0, state.clone());
    crate::pw_cursor::attach(&pipeline.0);
    #[cfg(test)]
    if let Some(hook) = tests::PIPELINE_HOOK.lock().unwrap().as_ref() {
        hook(&pipeline.0);
    }
    let sink = pipeline
        .0
        .by_name("captured")
        .unwrap()
        .downcast::<AppSink>()
        .unwrap();
    if let Some(caps) = &dmabuf {
        sink.set_caps(Some(caps));
    }
    let hdr_source = if format.hdr() {
        Some(
            pipeline
                .0
                .by_name("hdr_source")
                .context("HDR source must expose hdr_source caps")?
                .static_pad("src")
                .context("missing HDR pad")?
                .downgrade(),
        )
    } else {
        None
    };
    // First frame encoded (Ok) or failed (Err), for HDR and DMA-BUF checks.
    let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel::<Result<(), String>>(1);
    let mut ready_tx = Some(ready_tx);
    let mut group: Option<moq_net::group::Producer> = None;
    let mut frame_id = 0_u64;
    let mut encode_failures = 0_u32;
    // Frames submitted since (and including) the last keyframe.
    let mut since_key = u32::MAX;
    // When each pending frame was captured (its running-time timestamp).
    let mut captured: BTreeMap<u64, Instant> = BTreeMap::new();
    sink.set_callbacks(
        gstreamer_app::AppSinkCallbacks::builder()
            .new_sample(move |sink| {
                let mut process = || -> Result<()> {
                    let sample = sink.pull_sample()?;
                    let pulled = Instant::now();
                    // Age of the frame since the source timestamped it (live
                    // sources stamp capture time), so latency covers conversion.
                    let age = sample
                        .buffer()
                        .and_then(|buffer| buffer.pts())
                        .zip(sample.segment().and_then(|segment| {
                            segment.downcast_ref::<gst::ClockTime>().cloned()
                        }))
                        .and_then(|(pts, segment)| segment.to_running_time(pts))
                        .zip(sink.current_running_time())
                        .and_then(|(at, now)| now.checked_sub(at))
                        .map(|age| Duration::from_nanos(age.nseconds()))
                        .unwrap_or_default();
                    if let Some(pad) = &hdr_source {
                        let caps = pad
                            .upgrade()
                            .context("HDR source vanished")?
                            .current_caps()
                            .context("HDR source caps missing")?;
                        validate_hdr_raw(&gstreamer_video::VideoInfo::from_caps(&caps)?, true)?;
                    }
                    let dmabuf = if zero_copy {
                        Some(dmabuf_frame(&sample)?)
                    } else {
                        None
                    };
                    let mut raw = Vec::new();
                    if dmabuf.is_none() {
                        let info = gstreamer_video::VideoInfo::from_caps(
                            sample.caps().context("missing capture caps")?,
                        )?;
                        let frame = gstreamer_video::VideoFrameRef::from_buffer_ref_readable(
                            sample.buffer().context("missing capture buffer")?,
                            &info,
                        )?;
                        use gstreamer_video::prelude::*;
                        let stride = size.0 as usize * if format.hdr() { 2 } else { 1 };
                        raw.reserve(stride * size.1 as usize * 3 / 2);
                        for (plane, rows) in [(0, size.1 as usize), (1, size.1 as usize / 2)] {
                            let pitch = usize::try_from(frame.plane_stride()[plane])?;
                            ensure!(pitch >= stride, "invalid capture stride");
                            let bytes = frame.plane_data(plane as u32)?;
                            for row in 0..rows {
                                raw.extend_from_slice(
                                    bytes
                                        .get(row * pitch..row * pitch + stride)
                                        .context("short capture plane")?,
                                );
                            }
                        }
                    }
                    let start = Instant::now();
                    let prepare_us = start.duration_since(pulled).as_micros() as u64;
                    let mut codec = state.codec.lock().unwrap();
                    if state.apply_bitrate(&mut codec) {
                        // Its first frame is an IDR, which starts a new group below.
                        metrics.pending.lock().unwrap().clear();
                    }
                    frame_id = frame_id
                        .checked_add(1)
                        .context("encoder timestamp exhausted")?;
                    {
                        let mut pending = metrics.pending.lock().unwrap();
                        pending.insert(frame_id, start);
                        while pending.len() > 256 {
                            pending.pop_first();
                        }
                    }
                    captured.insert(frame_id, pulled.checked_sub(age).unwrap_or(pulled));
                    while captured.len() > 256 {
                        captured.pop_first();
                    }
                    let mut packets = Vec::new();
                    let force_key = state.keyframe.swap(false, Ordering::Relaxed)
                        || since_key >= state.key_interval.load(Ordering::Relaxed);
                    since_key = since_key.saturating_add(1);
                    let submitted = match &dmabuf {
                        // The sample (and the compositor's buffer) stays
                        // alive until the GPU has finished reading it.
                        Some(dmabuf) => codec.submit_dmabuf(dmabuf, frame_id, force_key),
                        None => codec.submit_frame(&raw, frame_id, force_key),
                    };
                    drop(sample);
                    let encoded = submitted.and_then(|()| {
                        while let Some(packet) = codec.receive()? {
                            packets.push(packet);
                        }
                        Ok(())
                    });
                    if let Err(error) = encoded {
                        // An import or conversion failure before the first
                        // frame means DMA-BUF capture does not work here: let
                        // the caller fall back to CPU capture.
                        if zero_copy && ready_tx.is_some() {
                            return Err(error);
                        }
                        // A transient driver error must not end the session.
                        // Reopen the encoder so its next output is an IDR that
                        // starts a fresh group; persistent failure stays fatal.
                        encode_failures += 1;
                        if zero_copy && encode_failures > MAX_CONSECUTIVE_ENCODE_FAILURES {
                            // Later encoders (the host recreates capture after
                            // this pipeline error) use CPU capture.
                            ZERO_COPY_FAILED.store(true, Ordering::Relaxed);
                        }
                        ensure!(
                            encode_failures <= MAX_CONSECUTIVE_ENCODE_FAILURES,
                            "encoder failed {encode_failures} consecutive frames: {error:#}"
                        );
                        tracing::warn!(%error, encode_failures, "direct FFmpeg encode failed; reopening encoder");
                        *codec = Codec::new(codec.config.clone())?;
                        metrics.pending.lock().unwrap().clear();
                        if let Some(mut old) = group.take() {
                            let _ = old.finish();
                        }
                        return Ok(());
                    }
                    encode_failures = 0;
                    for packet in packets {
                        if packet.key {
                            since_key = 1;
                        }
                        let elapsed = packet
                            .pts
                            .and_then(|pts| metrics.pending.lock().unwrap().remove(&pts))
                            .map(|at| at.elapsed());
                        let capture = packet
                            .pts
                            .and_then(|pts| captured.remove(&pts))
                            .map(|at| at.elapsed());
                        metrics.encode_us.store(
                            elapsed.map_or(0, |elapsed| elapsed.as_micros().max(1) as u64),
                            Ordering::Relaxed,
                        );
                        if let Some(elapsed) = elapsed {
                            let mut totals = metrics.totals.lock().unwrap();
                            totals.0 += prepare_us;
                            totals.1 += elapsed.as_micros() as u64;
                            totals.2 += 1;
                            totals.3 += capture.unwrap_or(elapsed).as_micros() as u64;
                        }
                        if packet.key {
                            if let Some(mut old) = group.take() {
                                let _ = old.finish();
                            }
                            group = Some(track.append_group()?);
                        }
                        if let Some(group) = &mut group {
                            group.write_frame(
                                moq_net::Timestamp::now(),
                                bytes::Bytes::from(packet.bytes),
                            )?;
                        }
                        if let Some(tx) = ready_tx.take() {
                            let _ = tx.send(Ok(()));
                        }
                    }
                    Ok(())
                };
                process().map(|()| gst::FlowSuccess::Ok).map_err(|error| {
                    if let Some(tx) = ready_tx.take() {
                        let _ = tx.send(Err(format!("{error:#}")));
                    }
                    tracing::error!(%error,"direct FFmpeg encode failed");
                    gst::FlowError::Error
                })
            })
            .build(),
    );
    pipeline.0.set_state(gst::State::Playing)?;
    if format.hdr() {
        ready_rx
            .recv_timeout(Duration::from_secs(3))
            .context("HDR capture/encoder did not produce verified output")?
            .map_err(|error| anyhow::anyhow!(error))?;
    } else if zero_copy {
        // Negotiation errors (no common DMA-BUF format) arrive on the bus;
        // import errors through the first frame. A source that is merely
        // slow to start keeps zero-copy: runtime failures still end this
        // pipeline and later ones use CPU capture.
        let deadline = Instant::now() + DMABUF_VERIFY;
        loop {
            match ready_rx.recv_timeout(Duration::from_millis(20)) {
                Ok(Ok(())) => break,
                Ok(Err(error)) => anyhow::bail!(error),
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                    anyhow::bail!("DMA-BUF capture stopped before its first frame")
                }
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                    pipeline.error()?;
                    if Instant::now() >= deadline {
                        tracing::warn!("DMA-BUF capture produced no frame yet; keeping it");
                        break;
                    }
                }
            }
        }
        tracing::info!("DMA-BUF zero-copy capture active");
    }
    Ok(pipeline)
}
