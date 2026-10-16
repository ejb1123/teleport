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

#[cfg(target_os = "linux")]
pub(super) fn encoder(
    source: &str,
    size: (u32, u32),
    fps: u32,
    bitrate: u32,
    mut track: moq_net::track::Producer,
    kind: EncoderKind,
    format: VideoFormat,
) -> Result<Pipeline> {
    format.validate()?;
    crate::protocol::validate_video_size(size.0, size.1)?;
    let prefix = if format.codec == VideoCodec::H265 {
        "hevc"
    } else {
        "h264"
    };
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
        let name = match choice {
            EncoderKind::Nvidia => format!("{prefix}_nvenc"),
            EncoderKind::Qsv => format!("{prefix}_qsv"),
            EncoderKind::Vaapi => format!("{prefix}_vaapi"),
            _ => if format.codec == VideoCodec::H265 {
                "libx265"
            } else {
                "libx264"
            }
            .into(),
        };
        let config = Config {
            encoder: true,
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
        let probe = || -> Result<()> {
            let mut codec = Codec::new(config.clone())?;
            let raw = neutral_frame(size.0, size.1, format.hdr());
            let mut emitted = false;
            for n in 0..3 {
                codec.submit(&raw, n)?;
                while let Some(packet) = codec.receive()? {
                    emitted |= packet.key;
                }
            }
            ensure!(emitted, "encoder produced no independent keyframe");
            Ok(())
        };
        match probe() {
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
    tracing::info!(encoder=%config.name,backend="ffmpeg",?format.codec,bitrate,"direct video encoder");
    let metrics = Arc::new(EncoderMetrics {
        encoder: format!("ffmpeg:{}", config.name),
        pending: Mutex::default(),
        encode_us: std::sync::atomic::AtomicU64::new(0),
        totals: Mutex::default(),
    });
    let name = config.name.clone();
    let codec = Codec::new(config);
    {
        let mut cache = ENCODERS.get_or_init(Mutex::default).lock().unwrap();
        if codec.is_ok() {
            cache.insert(cache_key, name);
        } else {
            cache.remove(&cache_key);
        }
    }
    let state = Arc::new(State {
        codec: Mutex::new(codec?),
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
    let pipeline = Pipeline(gst::parse::launch(&format!(
        // One-frame leaky queue: a slow encoder always picks up the newest
        // capture. Rate-limit before converting so dropped frames cost nothing,
        // then convert and scale in one multithreaded pass.
        "{source} ! queue max-size-buffers=1 max-size-bytes=0 max-size-time=0 leaky=downstream ! videorate drop-only=true ! video/x-raw,framerate={fps}/1 ! videoconvertscale n-threads=0 ! video/x-raw,format={pixel},width={},height={},framerate={fps}/1 ! appsink name=captured sync=false max-buffers=1 drop=false",size.0,size.1
    ))?.downcast::<gst::Pipeline>().map_err(|_| anyhow::anyhow!("not a capture pipeline"))?);
    attach(&pipeline.0, state.clone());
    let sink = pipeline
        .0
        .by_name("captured")
        .unwrap()
        .downcast::<AppSink>()
        .unwrap();
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
    let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(1);
    let mut ready_tx = Some(ready_tx);
    let mut group: Option<moq_net::group::Producer> = None;
    let mut frame_id = 0_u64;
    let mut encode_failures = 0_u32;
    // Frames submitted since (and including) the last keyframe.
    let mut since_key = u32::MAX;
    sink.set_callbacks(
        gstreamer_app::AppSinkCallbacks::builder()
            .new_sample(move |sink| {
                let mut process = || -> Result<()> {
                    let sample = sink.pull_sample()?;
                    let pulled = Instant::now();
                    if let Some(pad) = &hdr_source {
                        let caps = pad
                            .upgrade()
                            .context("HDR source vanished")?
                            .current_caps()
                            .context("HDR source caps missing")?;
                        validate_hdr_raw(&gstreamer_video::VideoInfo::from_caps(&caps)?, true)?;
                    }
                    let info = gstreamer_video::VideoInfo::from_caps(
                        sample.caps().context("missing capture caps")?,
                    )?;
                    let frame = gstreamer_video::VideoFrameRef::from_buffer_ref_readable(
                        sample.buffer().context("missing capture buffer")?,
                        &info,
                    )?;
                    use gstreamer_video::prelude::*;
                    let stride = size.0 as usize * if format.hdr() { 2 } else { 1 };
                    let mut raw = Vec::with_capacity(stride * size.1 as usize * 3 / 2);
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
                    let mut packets = Vec::new();
                    let force_key = state.keyframe.swap(false, Ordering::Relaxed)
                        || since_key >= state.key_interval.load(Ordering::Relaxed);
                    since_key = since_key.saturating_add(1);
                    let encoded = codec.submit_frame(&raw, frame_id, force_key).and_then(|()| {
                        while let Some(packet) = codec.receive()? {
                            packets.push(packet);
                        }
                        Ok(())
                    });
                    if let Err(error) = encoded {
                        // A transient driver error must not end the session.
                        // Reopen the encoder so its next output is an IDR that
                        // starts a fresh group; persistent failure stays fatal.
                        encode_failures += 1;
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
                        metrics.encode_us.store(
                            elapsed.map_or(0, |elapsed| elapsed.as_micros().max(1) as u64),
                            Ordering::Relaxed,
                        );
                        if let Some(elapsed) = elapsed {
                            let mut totals = metrics.totals.lock().unwrap();
                            totals.0 += prepare_us;
                            totals.1 += elapsed.as_micros() as u64;
                            totals.2 += 1;
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
                            let _ = tx.send(());
                        }
                    }
                    Ok(())
                };
                process().map(|()| gst::FlowSuccess::Ok).map_err(|error| {
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
            .context("HDR capture/encoder did not produce verified output")?;
    }
    Ok(pipeline)
}
