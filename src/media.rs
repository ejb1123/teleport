use crate::protocol::{DynamicRange, VideoCodec};
use anyhow::{Context, Result, ensure};
use futures_util::FutureExt;
use gst::prelude::*;
use gstreamer as gst;
use gstreamer_app::{AppSink, AppSrc};
#[path = "decoder.rs"]
mod decoder;
use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicU32, Ordering},
    },
    time::{Duration, Instant},
};

pub struct Pipeline(pub gst::Pipeline);

#[derive(Debug, Clone, Copy)]
pub struct VideoFormat {
    pub codec: VideoCodec,
    pub dynamic_range: DynamicRange,
}

impl From<VideoCodec> for VideoFormat {
    fn from(codec: VideoCodec) -> Self {
        Self {
            codec,
            dynamic_range: DynamicRange::Sdr,
        }
    }
}

impl VideoFormat {
    fn validate(self) -> Result<()> {
        ensure!(
            self.dynamic_range != DynamicRange::Hdr10 || self.codec == VideoCodec::H265,
            "HDR10 requires H.265 Main 10"
        );
        Ok(())
    }
    fn hdr(self) -> bool {
        self.dynamic_range == DynamicRange::Hdr10
    }
}
impl Drop for Pipeline {
    fn drop(&mut self) {
        let _ = self.0.set_state(gst::State::Null);
    }
}
impl Pipeline {
    #[cfg(target_os = "linux")]
    pub fn encoder_metrics(&self) -> Result<Arc<EncoderMetrics>> {
        let encoder = self
            .0
            .by_name("video_encoder")
            .context("no video encoder")?;
        let metrics = Arc::new(EncoderMetrics {
            encoder: encoder
                .factory()
                .context("encoder factory missing")?
                .name()
                .to_string(),
            pending: Mutex::default(),
            encode_us: std::sync::atomic::AtomicU64::new(0),
        });
        let input = metrics.clone();
        encoder
            .static_pad("sink")
            .context("encoder sink missing")?
            .add_probe(gst::PadProbeType::BUFFER, move |pad, info| {
                if let Some(pts) = buffer_running_time(pad, info) {
                    let mut pending = input.pending.lock().unwrap();
                    pending.insert(pts, Instant::now());
                    while pending.len() > 256 {
                        pending.pop_first();
                    }
                }
                gst::PadProbeReturn::Ok
            });
        let output = metrics.clone();
        encoder
            .static_pad("src")
            .context("encoder source missing")?
            .add_probe(gst::PadProbeType::BUFFER, move |pad, info| {
                let elapsed = buffer_running_time(pad, info)
                    .and_then(|pts| output.pending.lock().unwrap().remove(&pts))
                    .map(|at| at.elapsed());
                output.encode_us.store(
                    elapsed.map_or(0, |d| d.as_micros().max(1).min(u64::MAX as u128) as u64),
                    Ordering::Relaxed,
                );
                gst::PadProbeReturn::Ok
            });
        Ok(metrics)
    }
    pub fn set_audio_muted(&self, muted: bool) -> Result<()> {
        self.0
            .by_name("audio_volume")
            .context("not an audio decoder")?
            .set_property("mute", muted);
        Ok(())
    }
    #[cfg(target_os = "linux")]
    pub fn set_video_bitrate(&self, kbps: u32) -> Result<()> {
        ensure!(kbps > 0 && kbps <= 100_000, "invalid video bitrate");
        let encoder = self
            .0
            .by_name("video_encoder")
            .context("not a video encoder")?;
        let property = encoder
            .find_property("bitrate")
            .context("encoder has no bitrate control")?;
        ensure!(
            property.flags().contains(gst::PARAM_FLAG_MUTABLE_PLAYING),
            "encoder cannot change bitrate while playing"
        );
        encoder.set_property("bitrate", kbps);
        Ok(())
    }

    pub fn error(&self) -> Result<()> {
        if let Some(message) = self
            .0
            .bus()
            .unwrap()
            .pop_filtered(&[gst::MessageType::Error, gst::MessageType::Eos])
        {
            match message.view() {
                gst::MessageView::Error(e) => {
                    anyhow::bail!("GStreamer: {} ({:?})", e.error(), e.debug())
                }
                _ => anyhow::bail!("capture/decoder reached end of stream"),
            }
        }
        Ok(())
    }
}

#[cfg(target_os = "linux")]
fn buffer_running_time(pad: &gst::Pad, info: &gst::PadProbeInfo<'_>) -> Option<u64> {
    let pts = info.buffer()?.pts()?;
    // Encoders can offset output PTS and announce that offset in their segment.
    // Compare the documented running-time mapping, never infer a numeric offset.
    let event = pad.sticky_event::<gst::event::Segment>(0)?;
    event
        .segment()
        .downcast_ref::<gst::ClockTime>()?
        .to_running_time(pts)
        .map(|time| time.nseconds())
}

#[cfg(target_os = "linux")]
pub struct EncoderMetrics {
    pub encoder: String,
    pending: Mutex<std::collections::BTreeMap<u64, Instant>>,
    pub encode_us: std::sync::atomic::AtomicU64,
}

#[cfg(target_os = "linux")]
#[derive(Clone, Copy, Debug, Default, clap::ValueEnum)]
pub enum EncoderKind {
    #[default]
    Auto,
    Software,
    /// Intel Quick Sync (oneVPL); VA-API remains available as a fallback.
    #[value(alias = "quick-sync")]
    Qsv,
    Vaapi,
    Nvidia,
}

#[cfg(target_os = "linux")]
fn encoder_fragment(
    kind: EncoderKind,
    fps: u32,
    bitrate: u32,
    format: impl Into<VideoFormat>,
) -> String {
    let format = format.into();
    let codec = format.codec;
    let profile = if format.hdr() { "main-10" } else { "main" };
    let gop = (fps / 4).max(1);
    match (kind, codec) {
        (EncoderKind::Software, VideoCodec::H264) => format!(
            "x264enc name=video_encoder tune=zerolatency speed-preset=ultrafast bitrate={bitrate} key-int-max={gop} bframes=0 threads=4 ! video/x-h264,profile=constrained-baseline"
        ),
        (EncoderKind::Software, VideoCodec::H265) => format!(
            "x265enc name=video_encoder tune=zerolatency speed-preset=superfast bitrate={bitrate} key-int-max={gop} option-string=pools=4:frame-threads=2:log-level=error ! video/x-h265,profile={profile}"
        ),
        // GStreamer 1.26 QSV: low-latency sets AsyncDepth=1. CBR avoids
        // look-ahead, and zero B frames prevents frame reordering. The codecs
        // intentionally use different IDR intervals: AVC uses 0, HEVC uses 1
        // for every I-frame to be independently decodable across MoQ groups.
        (EncoderKind::Qsv, VideoCodec::H264) => format!(
            "qsvh264enc name=video_encoder bitrate={bitrate} gop-size={gop} idr-interval=0 b-frames=0 low-latency=true target-usage=7 rate-control=cbr"
        ),
        (EncoderKind::Qsv, VideoCodec::H265) => format!(
            "qsvh265enc name=video_encoder bitrate={bitrate} gop-size={gop} idr-interval=1 b-frames=0 low-latency=true target-usage=7 rate-control=cbr ! video/x-h265,profile={profile}"
        ),
        (EncoderKind::Vaapi, VideoCodec::H264) => format!(
            "vah264enc name=video_encoder bitrate={bitrate} key-int-max={gop} b-frames=0 rate-control=cbr"
        ),
        (EncoderKind::Vaapi, VideoCodec::H265) => format!(
            "vah265enc name=video_encoder bitrate={bitrate} key-int-max={gop} b-frames=0 rate-control=cbr"
        ),
        (EncoderKind::Nvidia, VideoCodec::H264) => format!(
            "nvh264enc name=video_encoder bitrate={bitrate} gop-size={gop} bframes=0 zerolatency=true rc-mode=cbr"
        ),
        (EncoderKind::Nvidia, VideoCodec::H265) => format!(
            "nvh265enc name=video_encoder bitrate={bitrate} gop-size={gop} bframes=0 zerolatency=true rc-mode=cbr"
        ),
        (EncoderKind::Auto, _) => unreachable!("auto must be resolved first"),
    }
}

fn parser(codec: VideoCodec) -> &'static str {
    match codec {
        VideoCodec::H264 => "h264parse",
        VideoCodec::H265 => "h265parse",
    }
}

fn compressed_caps(codec: VideoCodec) -> &'static str {
    match codec {
        VideoCodec::H264 => "video/x-h264",
        VideoCodec::H265 => "video/x-h265",
    }
}

#[cfg(target_os = "linux")]
fn validate_hdr_encoded(caps: &gst::CapsRef) -> Result<()> {
    let s = caps.structure(0).context("missing HEVC caps")?;
    ensure!(
        s.name() == "video/x-h265" && s.get::<&str>("profile")? == "main-10",
        "HDR encoder did not negotiate HEVC Main 10"
    );
    ensure!(
        s.get::<u32>("bit-depth-luma")? == 10 && s.get::<u32>("bit-depth-chroma")? == 10,
        "HDR encoder did not produce 10-bit luma/chroma"
    );
    let color = s
        .get::<&str>("colorimetry")?
        .parse::<gstreamer_video::VideoColorimetry>()?;
    validate_hdr_color(&color, false)
}

fn validate_hdr_color(color: &gstreamer_video::VideoColorimetry, rgb: bool) -> Result<()> {
    use gstreamer_video::{
        VideoColorMatrix, VideoColorPrimaries, VideoColorRange, VideoTransferFunction,
    };
    ensure!(
        color.transfer() == VideoTransferFunction::Smpte2084
            && color.primaries() == VideoColorPrimaries::Bt2020,
        "HDR requires explicit PQ transfer and BT.2020 primaries, got {color}"
    );
    ensure!(
        color.range()
            == if rgb {
                VideoColorRange::Range0_255
            } else {
                VideoColorRange::Range16_235
            },
        "incorrect HDR quantization range: {color}"
    );
    ensure!(
        color.matrix()
            == if rgb {
                VideoColorMatrix::Rgb
            } else {
                VideoColorMatrix::Bt2020
            },
        "incorrect HDR matrix: {color}"
    );
    Ok(())
}

fn validate_hdr_raw(info: &gstreamer_video::VideoInfo, source: bool) -> Result<()> {
    let expected = if source {
        gstreamer_video::VideoFormat::Rgb10a2Le
    } else {
        gstreamer_video::VideoFormat::P01010le
    };
    ensure!(
        info.format() == expected && info.format_info().depth().first() == Some(&10),
        "HDR requires {expected:?} 10-bit pixels, got {:?}",
        info.format()
    );
    validate_hdr_color(&info.colorimetry(), source)?;
    ensure!(
        source || info.chroma_site() == gstreamer_video::VideoChromaSite::JPEG,
        "HDR P010 presentation requires centered/JPEG chroma"
    );
    Ok(())
}

#[cfg(target_os = "linux")]
fn encoder_pixel_format(kind: EncoderKind, format: impl Into<VideoFormat>) -> &'static str {
    let format = format.into();
    let codec = format.codec;
    if format.hdr() {
        return if matches!(kind, EncoderKind::Software) {
            "I420_10LE"
        } else {
            "P010_10LE"
        };
    }
    if matches!((kind, codec), (EncoderKind::Software, VideoCodec::H265)) {
        "I420"
    } else {
        "NV12"
    }
}

#[cfg(target_os = "linux")]
fn select_encoder(
    kind: EncoderKind,
    width: u32,
    height: u32,
    fps: u32,
    bitrate: u32,
    format: impl Into<VideoFormat>,
) -> Result<EncoderKind> {
    let format = format.into();
    format.validate()?;
    let codec = format.codec;
    if matches!(kind, EncoderKind::Software) {
        return Ok(kind);
    }
    let choices: &[EncoderKind] = if matches!(kind, EncoderKind::Auto) {
        // Prefer Intel's low-latency path when available, then generic VA-API
        // (AMD/Intel), then NVIDIA. Every choice must produce an actual frame.
        &[EncoderKind::Qsv, EncoderKind::Vaapi, EncoderKind::Nvidia]
    } else {
        std::slice::from_ref(&kind)
    };
    for &candidate in choices {
        // Probe real encoded output, not just plugin presence. Drivers/devices may
        // be unavailable even when a factory is registered. Never open capture here.
        let probe = || -> Result<()> {
            let pixels = encoder_pixel_format(candidate, format);
            let color = if format.hdr() {
                ",colorimetry=bt2100-pq"
            } else {
                ""
            };
            let description = format!(
                "videotestsrc num-buffers=3 ! video/x-raw,format={pixels}{color},width={width},height={height},framerate={fps}/1 ! {} ! {} ! appsink name=probe sync=false",
                encoder_fragment(candidate, fps, bitrate, format),
                parser(codec)
            );
            let pipeline = Pipeline(
                gst::parse::launch(&description)?
                    .downcast::<gst::Pipeline>()
                    .map_err(|_| anyhow::anyhow!("not a pipeline"))?,
            );
            let sink = pipeline
                .0
                .by_name("probe")
                .unwrap()
                .downcast::<AppSink>()
                .unwrap();
            pipeline.0.set_state(gst::State::Playing)?;
            let sample = sink.try_pull_sample(gst::ClockTime::from_seconds(3));
            // EOS is expected for this finite probe; only actual errors fail it.
            if let Some(message) = pipeline
                .0
                .bus()
                .context("missing probe bus")?
                .pop_filtered(&[gst::MessageType::Error])
                && let gst::MessageView::Error(error) = message.view()
            {
                anyhow::bail!("{} ({:?})", error.error(), error.debug());
            }
            ensure!(
                sample.is_some(),
                "hardware encoder produced no frame within 3 seconds"
            );
            if format.hdr() {
                validate_hdr_encoded(
                    sample
                        .as_ref()
                        .unwrap()
                        .caps()
                        .context("missing encoded caps")?,
                )?;
            }
            Ok(())
        };
        match probe() {
            Ok(()) => return Ok(candidate),
            Err(error) if matches!(kind, EncoderKind::Auto) => {
                tracing::info!(?candidate, %error, "hardware encoder unavailable; trying fallback")
            }
            Err(error) => {
                return Err(error.context(format!(
                    "requested {candidate:?} encoder failed; try --encoder software"
                )));
            }
        }
    }
    Ok(EncoderKind::Software)
}

#[cfg(target_os = "linux")]
pub fn encoder(
    source: &str,
    size: (u32, u32),
    fps: u32,
    bitrate: u32,
    mut track: moq_net::track::Producer,
    kind: EncoderKind,
    video_format: impl Into<VideoFormat>,
) -> Result<Pipeline> {
    let video_format = video_format.into();
    video_format.validate()?;
    let codec = video_format.codec;
    let (width, height) = size;
    let kind = select_encoder(kind, width, height, fps, bitrate, video_format)?;
    tracing::info!(?kind, ?codec, bitrate, "video encoder");
    let fragment = encoder_fragment(kind, fps, bitrate, video_format);
    let format = encoder_pixel_format(kind, video_format);
    let color = if video_format.hdr() {
        ",colorimetry=bt2100-pq"
    } else {
        ""
    };
    let encoded_color = if video_format.hdr() {
        ",profile=main-10,colorimetry=bt2100-pq"
    } else {
        ""
    };
    let parser = parser(codec);
    let caps = compressed_caps(codec);
    let description = format!(
        "{source} ! queue max-size-buffers=2 max-size-bytes=0 max-size-time=0 leaky=downstream \
        ! videoconvert ! videoscale ! videorate drop-only=true \
        ! video/x-raw,format={format}{color},width={width},height={height},framerate={fps}/1 \
        ! {fragment} \
        ! {parser} config-interval=-1 ! {caps}{encoded_color},stream-format=byte-stream,alignment=au \
        ! appsink name=encoded sync=false max-buffers=2 drop=false"
    );
    let pipeline = Pipeline(
        gst::parse::launch(&description)?
            .downcast::<gst::Pipeline>()
            .map_err(|_| anyhow::anyhow!("not a pipeline"))?,
    );
    let sink = pipeline
        .0
        .by_name("encoded")
        .unwrap()
        .downcast::<AppSink>()
        .unwrap();
    let mut group: Option<moq_net::group::Producer> = None;
    let source_pad = if video_format.hdr() {
        Some(
            pipeline
                .0
                .by_name("hdr_source")
                .context("HDR source must provide an explicit hdr_source capsfilter")?
                .static_pad("src")
                .context("HDR source pad missing")?
                .downgrade(),
        )
    } else {
        None
    };
    let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(1);
    let mut ready_tx = video_format.hdr().then_some(ready_tx);
    sink.set_callbacks(
        gstreamer_app::AppSinkCallbacks::builder()
            .new_sample(move |sink| {
                let sample = sink.pull_sample().map_err(|_| gst::FlowError::Eos)?;
                if let Some(source_pad) = &source_pad {
                    let validation = (|| -> Result<()> {
                        let source = source_pad.upgrade().context("HDR source disappeared")?;
                        let caps = source
                            .current_caps()
                            .context("HDR source has not negotiated caps")?;
                        validate_hdr_raw(&gstreamer_video::VideoInfo::from_caps(&caps)?, true)?;
                        validate_hdr_encoded(sample.caps().context("HDR output has no caps")?)
                    })();
                    if let Err(error) = validation {
                        if let Some(tx) = ready_tx.take() {
                            let _ = tx.send(Err(format!("{error:#}")));
                        }
                        tracing::error!(%error, "rejecting invalid HDR capture/encode output");
                        return Err(gst::FlowError::Error);
                    }
                    if let Some(tx) = ready_tx.take() {
                        let _ = tx.send(Ok(()));
                    }
                }
                let buffer = sample.buffer().ok_or(gst::FlowError::Error)?;
                let map = buffer.map_readable().map_err(|_| gst::FlowError::Error)?;
                if !buffer.flags().contains(gst::BufferFlags::DELTA_UNIT) {
                    if let Some(mut old) = group.take() {
                        let _ = old.finish();
                    }
                    group = Some(track.append_group().map_err(|_| gst::FlowError::Error)?);
                }
                if let Some(group) = &mut group {
                    group
                        .write_frame(
                            moq_net::Timestamp::now(),
                            bytes::Bytes::copy_from_slice(map.as_slice()),
                        )
                        .map_err(|_| gst::FlowError::Error)?;
                }
                Ok(gst::FlowSuccess::Ok)
            })
            .build(),
    );
    pipeline.0.set_state(gst::State::Playing)?;
    if video_format.hdr() {
        ready_rx
            .recv_timeout(Duration::from_secs(3))
            .context("HDR source/encoder produced no verified frame within 3 seconds")?
            .map_err(anyhow::Error::msg)?;
    }
    Ok(pipeline)
}

pub struct Image {
    pub decoded_at: Instant,
    pub decoder_recovery: u64,
    pub video_group: Option<u64>,
    pub data: Vec<u8>,
    pub width: u32,
    pub height: u32,
    pub stride: usize,
    pub hdr: Option<HdrImage>,
}

pub struct HdrImage {
    pub y: Vec<u8>,
    pub uv: Vec<u8>,
    pub y_stride: usize,
    pub uv_stride: usize,
}
pub type LatestImage = Arc<Mutex<Option<Image>>>;

fn add_video_reference(buffer: &mut gst::BufferRef, key: u64) {
    static REFERENCE: std::sync::OnceLock<gst::Caps> = std::sync::OnceLock::new();
    let reference =
        REFERENCE.get_or_init(|| gst::Caps::new_empty_simple("timestamp/x-teleport-receive"));
    gst::ReferenceTimestampMeta::add(
        buffer,
        reference,
        gst::ClockTime::from_nseconds(key),
        gst::ClockTime::NONE,
    );
}

fn stamp_video_buffer(buffer: &mut gst::BufferRef, key: u64) {
    buffer.set_pts(gst::ClockTime::from_nseconds(key));
    // Presentation timestamps may be rewritten by parsers/decoders. Keep the
    // exact receive-ledger identity separately, in metadata copied per frame.
    add_video_reference(buffer, key);
}

fn video_reference(buffer: &gst::BufferRef) -> Option<u64> {
    buffer
        .iter_meta::<gst::ReferenceTimestampMeta>()
        .find(|meta| {
            meta.reference()
                .structure(0)
                .is_some_and(|s| s.name() == "timestamp/x-teleport-receive")
        })
        .map(|meta| meta.timestamp().nseconds())
}

/// Call before opening QUIC: bounded hardware probes must not delay heartbeats.
pub fn prepare_decoder(force_software: bool, format: impl Into<VideoFormat>) -> Result<()> {
    decoder::select(force_software, format.into()).map(|_| ())
}

pub fn decoder_with_stats(
    force_software: bool,
    stats: Arc<crate::stats::StreamStats>,
    format: impl Into<VideoFormat>,
) -> Result<(Pipeline, AppSrc, LatestImage)> {
    let format = format.into();
    format.validate()?;
    let codec = format.codec;
    let decoder = decoder::select(force_software, format)?;
    *stats.decoder.lock().unwrap() = decoder.to_owned();
    tracing::info!(decoder, ?codec, "video decoder");
    let caps = compressed_caps(codec);
    let fragment = decoder::fragment(decoder, codec);
    let conversion = decoder::output_fragment(format);
    let pipeline = Pipeline(gst::parse::launch(&format!(
        "appsrc name=in is-live=true format=time do-timestamp=true max-bytes=8388608 block=false \
        caps={caps},stream-format=byte-stream,alignment=au \
        ! {fragment} ! {conversion} \
        ! appsink name=frames sync=false max-buffers=1 drop=true"
    ))?.downcast::<gst::Pipeline>().map_err(|_| anyhow::anyhow!("not a pipeline"))?);
    let source = pipeline
        .0
        .by_name("in")
        .unwrap()
        .downcast::<AppSrc>()
        .unwrap();
    let sink = pipeline
        .0
        .by_name("frames")
        .unwrap()
        .downcast::<AppSink>()
        .unwrap();
    let entered_stats = stats.clone();
    source
        .static_pad("src")
        .context("missing decoder input pad")?
        .add_probe(gst::PadProbeType::BUFFER, move |_, info| {
            if let Some(key) = info.buffer().and_then(|buffer| video_reference(buffer)) {
                entered_stats.decoder_entered(key);
            }
            gst::PadProbeReturn::Ok
        });
    let output_stats = stats.clone();
    pipeline
        .0
        .by_name("video_decoder")
        .context("missing decoder")?
        .static_pad("src")
        .context("missing decoder output pad")?
        .add_probe(gst::PadProbeType::BUFFER, move |_, info| {
            if let Some(key) = info.buffer().and_then(|buffer| video_reference(buffer)) {
                output_stats.decoder_output(key);
            }
            gst::PadProbeReturn::Ok
        });
    let image: LatestImage = Arc::new(Mutex::new(None));
    let latest = image.clone();
    let hdr_pipeline = format.hdr().then(|| pipeline.0.downgrade());
    sink.set_callbacks(
        gstreamer_app::AppSinkCallbacks::builder()
            .new_sample(move |sink| {
                let sample = sink.pull_sample().map_err(|_| gst::FlowError::Eos)?;
                if let Some(weak) = &hdr_pipeline {
                    let pipeline = weak.upgrade().ok_or(gst::FlowError::Error)?;
                    decoder::verify_hdr_decoder_output(&pipeline)
                        .map_err(|_| gst::FlowError::Error)?;
                }
                let key = sample.buffer().and_then(|buffer| {
                    video_reference(buffer).or_else(|| buffer.pts().map(|pts| pts.nseconds()))
                });
                // Do not present seconds-old output even if a driver holds frames
                // internally. Receive-side recovery will resume at a fresh GOP.
                if key
                    .and_then(|key| stats.frame_age(key))
                    .is_some_and(|age| age > Duration::from_millis(250))
                {
                    stats.decoded(key);
                    stats.stale_frames.fetch_add(1, Ordering::Relaxed);
                    return Ok(gst::FlowSuccess::Ok);
                }
                let info = gstreamer_video::VideoInfo::from_caps(
                    sample.caps().ok_or(gst::FlowError::Error)?,
                )
                .map_err(|_| gst::FlowError::Error)?;
                crate::protocol::validate_video_size(info.width(), info.height())
                    .map_err(|_| gst::FlowError::Error)?;
                if format.hdr() {
                    validate_hdr_raw(&info, false).map_err(|_| gst::FlowError::Error)?;
                }
                let frame = gstreamer_video::VideoFrameRef::from_buffer_ref_readable(
                    sample.buffer().ok_or(gst::FlowError::Error)?,
                    &info,
                )
                .map_err(|_| gst::FlowError::Error)?;
                use gstreamer_video::prelude::*;
                let (data, hdr) = if format.hdr() {
                    let strides = frame.plane_stride();
                    if strides[0] <= 0 || strides[1] <= 0 {
                        return Err(gst::FlowError::Error);
                    }
                    (
                        Vec::new(),
                        Some(HdrImage {
                            y: frame
                                .plane_data(0)
                                .map_err(|_| gst::FlowError::Error)?
                                .to_vec(),
                            uv: frame
                                .plane_data(1)
                                .map_err(|_| gst::FlowError::Error)?
                                .to_vec(),
                            y_stride: strides[0] as usize,
                            uv_stride: strides[1] as usize,
                        }),
                    )
                } else {
                    (
                        frame
                            .plane_data(0)
                            .map_err(|_| gst::FlowError::Error)?
                            .to_vec(),
                        None,
                    )
                };
                let stale_after_copy = key
                    .and_then(|key| stats.frame_age(key))
                    .is_some_and(|age| age > Duration::from_millis(250));
                let video_group = stats.decoded(key);
                if stale_after_copy {
                    stats.stale_frames.fetch_add(1, Ordering::Relaxed);
                    return Ok(gst::FlowSuccess::Ok);
                }
                let mut latest = latest.lock().unwrap();
                if latest.is_some() {
                    stats.overwritten_frames.fetch_add(1, Ordering::Relaxed);
                }
                *latest = Some(Image {
                    hdr,
                    decoded_at: Instant::now(),
                    decoder_recovery: stats.decoder_recoveries.load(Ordering::Relaxed),
                    video_group,
                    data,
                    width: info.width(),
                    height: info.height(),
                    stride: frame.plane_stride()[0] as usize,
                });
                if video_group.is_some() {
                    stats.ready_frames.fetch_add(1, Ordering::Relaxed);
                }
                Ok(gst::FlowSuccess::Ok)
            })
            .build(),
    );
    pipeline.0.set_state(gst::State::Playing)?;
    Ok((pipeline, source, image))
}

pub async fn receive_video_with_stats(
    track: moq_net::track::Subscriber,
    source: AppSrc,
    events: tokio::sync::mpsc::Sender<crate::protocol::Event>,
    stats: Arc<crate::stats::StreamStats>,
) -> Result<()> {
    let dropped = Arc::new(AtomicU32::new(0));
    let monitor = async {
        let mut interval = tokio::time::interval(Duration::from_secs(1));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut previous_recoveries = stats.decoder_recoveries.load(Ordering::Relaxed);
        loop {
            interval.tick().await;
            let mut queue_ms = source
                .current_level_time()
                .map_or(0, |time| time.mseconds().min(u32::MAX as u64) as u32);
            let recoveries = stats.decoder_recoveries.load(Ordering::Relaxed);
            if recoveries != previous_recoveries {
                // A reset emptied the queue, but must not conceal overload from
                // the host's adaptive bitrate controller at its next sample.
                queue_ms = queue_ms.max(MAX_DECODE_BACKLOG_AGE.as_millis() as u32);
            }
            previous_recoveries = recoveries;
            let dropped_groups = dropped.swap(0, Ordering::Relaxed);
            stats
                .decoder_queue_bytes
                .store(source.current_level_bytes(), Ordering::Relaxed);
            let _ = events.try_send(crate::protocol::Event::Feedback {
                queue_ms,
                dropped_groups,
            });
        }
    };
    // Keep the read future alive across feedback ticks. Cancelling read_frame in
    // the middle of a GOP would lose a partially-read H.264 access unit.
    tokio::select! {
        result = receive_video_inner(track, source.clone(), dropped.clone(), stats.clone()) => result,
        () = monitor => unreachable!(),
    }
}

const MAX_DECODE_BACKLOG_AGE: Duration = Duration::from_millis(150);
const MAX_DECODE_BACKLOG_FRAMES: u64 = 12;
const MAX_DECODE_BACKLOG_BYTES: u64 = 8 * 1024 * 1024;

#[derive(Default)]
struct RecoveryBudget {
    recent: std::collections::VecDeque<Instant>,
    fresh_frames: u64,
    without_progress: u32,
}

impl RecoveryBudget {
    fn attempt(&mut self, fresh_frames: u64, now: Instant) -> Result<()> {
        while self
            .recent
            .front()
            .is_some_and(|at| now.duration_since(*at) > Duration::from_secs(5))
        {
            self.recent.pop_front();
        }
        if fresh_frames != self.fresh_frames {
            self.without_progress = 0;
            self.fresh_frames = fresh_frames;
        }
        ensure!(
            self.recent.len() < 5 && self.without_progress < 5,
            "decoder repeatedly exceeded latency budget; reduce resolution/FPS, try H.265, or use --software-decoder"
        );
        self.recent.push_back(now);
        self.without_progress += 1;
        Ok(())
    }
}

fn decoder_backlogged(source: &AppSrc, stats: &crate::stats::StreamStats) -> bool {
    let frames = source.current_level_buffers();
    frames >= MAX_DECODE_BACKLOG_FRAMES
        || source.current_level_bytes() >= MAX_DECODE_BACKLOG_BYTES
        || (frames > 0
            && stats
                .oldest_queued_age()
                .is_some_and(|age| age > MAX_DECODE_BACKLOG_AGE))
}

fn decoder_needs_recovery(
    source: &AppSrc,
    stats: &crate::stats::StreamStats,
    stale_checkpoint: u64,
) -> bool {
    decoder_backlogged(source, stats)
        || stats
            .stale_frames
            .load(Ordering::Relaxed)
            .saturating_sub(stale_checkpoint)
            >= 3
}

async fn reset_decoder(source: &AppSrc, stats: Arc<crate::stats::StreamStats>) -> Result<()> {
    let pipeline = source
        .parent()
        .context("decoder source has no pipeline")?
        .downcast::<gst::Pipeline>()
        .map_err(|_| anyhow::anyhow!("decoder parent is not a pipeline"))?;
    // A state reset clears appsrc, parser, decoder surfaces and queued callbacks
    // together. Never leak/drop arbitrary dependent compressed frames in appsrc.
    // State changes may wait for a driver; keep them off Tokio's I/O workers.
    // Aborting an async receive task must not leave its blocking reset free to
    // resurrect the pipeline after the session owner has torn it down.
    struct PendingReset {
        pipeline: gst::Pipeline,
        active: Arc<Mutex<bool>>,
        complete: bool,
    }
    impl Drop for PendingReset {
        fn drop(&mut self) {
            if !self.complete {
                let mut active = self.active.lock().unwrap();
                *active = false;
                let _ = self.pipeline.set_state(gst::State::Null);
            }
        }
    }
    let mut guard = PendingReset {
        pipeline: pipeline.clone(),
        active: Arc::new(Mutex::new(true)),
        complete: false,
    };
    let active = guard.active.clone();
    tokio::task::spawn_blocking(move || -> Result<()> {
        let active = active.lock().unwrap();
        if !*active {
            return Ok(());
        }
        pipeline.set_state(gst::State::Ready)?;
        stats.reset_pending();
        pipeline.set_state(gst::State::Playing)?;
        Ok(())
    })
    .await??;
    guard.complete = true;
    Ok(())
}

async fn receive_video_inner(
    mut track: moq_net::track::Subscriber,
    source: AppSrc,
    dropped: Arc<AtomicU32>,
    stats: Arc<crate::stats::StreamStats>,
) -> Result<()> {
    let mut current: Option<moq_net::group::Consumer> = None;
    let mut sequence = None;
    let mut stale_checkpoint = stats.stale_frames.load(Ordering::Relaxed);
    let mut recoveries = RecoveryBudget::default();
    let record_drop = |count: u32| {
        stats
            .skipped_groups
            .fetch_add(count as u64, Ordering::Relaxed);
        let _ = dropped.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |old| {
            Some(old.saturating_add(count))
        });
    };
    loop {
        match next_media_event(
            &mut track,
            &mut current,
            sequence,
            crate::protocol::MAX_FRAME,
        )
        .await?
        {
            MediaEvent::Group(next) => {
                if let Some(previous) = sequence {
                    record_drop(
                        next.sequence
                            .saturating_sub(previous)
                            .saturating_sub(1)
                            .min(u32::MAX as u64) as u32,
                    );
                }
                if let Some(old) = &mut current {
                    // A clean drained group may simply not have had its EOF
                    // branch polled yet. Count only genuinely unfinished GOPs.
                    if !matches!(old.finished().now_or_never(), Some(Ok(_))) {
                        record_drop(1);
                    }
                }
                sequence = Some(next.sequence);
                current = Some(next);
            }
            MediaEvent::Frame(frame) => match frame {
                Ok(Some(bytes)) => {
                    if decoder_needs_recovery(&source, &stats, stale_checkpoint) {
                        recoveries
                            .attempt(stats.ready_frames.load(Ordering::Relaxed), Instant::now())?;
                        tracing::warn!(
                            queued_bytes = source.current_level_bytes(),
                            queued_frames = source.current_level_buffers(),
                            "decoder backlog exceeded latency budget; resetting at next keyframe group"
                        );
                        // Abandon the complete remainder of this GOP, including
                        // this frame. next_media_event accepts only a newer group;
                        // the host starts every group with an independent keyframe.
                        current = None;
                        record_drop(1);
                        reset_decoder(&source, stats.clone()).await?;
                        stale_checkpoint = stats.stale_frames.load(Ordering::Relaxed);
                        continue;
                    }
                    let video_group = sequence.context("video frame arrived without a group")?;
                    let pts = stats.received(bytes.len(), video_group);
                    let mut buffer = gst::Buffer::from_slice(bytes);
                    stamp_video_buffer(buffer.get_mut().unwrap(), pts);
                    source.push_buffer(buffer)?;
                }
                Ok(None) => current = None,
                Err(error) => {
                    tracing::debug!(%error, "skipping incomplete video group");
                    record_drop(1);
                    current = None;
                }
            },
        }
    }
}

pub(crate) enum MediaEvent {
    Group(moq_net::group::Consumer),
    Frame(Result<Option<bytes::Bytes>>),
}

/// Preserve a partially-read frame when an older, reordered group arrives.
/// Only a genuinely newer group is allowed to cancel the in-flight payload.
pub(crate) async fn next_media_event(
    track: &mut moq_net::track::Subscriber,
    current: &mut Option<moq_net::group::Consumer>,
    sequence: Option<u64>,
    limit: usize,
) -> Result<MediaEvent> {
    let frame = async {
        match current {
            Some(group) => crate::protocol::read_frame(group, limit).await,
            None => std::future::pending().await,
        }
    };
    tokio::pin!(frame);
    loop {
        tokio::select! {
            biased;
            next = track.recv_group() => {
                let next = next?.context("media track closed")?;
                if sequence.is_none_or(|previous| next.sequence > previous) {
                    return Ok(MediaEvent::Group(next));
                }
            }
            frame = &mut frame => return Ok(MediaEvent::Frame(frame)),
        }
    }
}

/// Conservative AIMD policy; call at most once per second using interval deltas.
/// A clean five-second window permits recovery, never above the configured cap.
#[cfg(target_os = "linux")]
pub struct BitrateController {
    current: u32,
    ceiling: u32,
    clean_samples: u32,
}

#[cfg(target_os = "linux")]
impl BitrateController {
    pub fn new(ceiling: u32) -> Self {
        Self {
            current: ceiling,
            ceiling,
            clean_samples: 0,
        }
    }

    pub fn observe(&mut self, queue_ms: u32, dropped_groups: u32) -> Option<u32> {
        let previous = self.current;
        if queue_ms > 80 || dropped_groups >= 2 {
            self.current = (self.current.saturating_mul(3) / 4).max(self.ceiling.min(500));
            self.clean_samples = 0;
        } else if queue_ms < 30 && dropped_groups == 0 {
            self.clean_samples += 1;
            if self.clean_samples >= 5 {
                self.current = self
                    .current
                    .saturating_add((self.ceiling / 20).max(100))
                    .min(self.ceiling);
                self.clean_samples = 0;
            }
        } else {
            self.clean_samples = 0;
        }
        (previous != self.current).then_some(self.current)
    }
}

pub fn video_subscription() -> moq_net::track::Subscription {
    moq_net::track::Subscription::default().with_latency_max(Duration::from_millis(100))
}

pub fn doctor() -> Result<()> {
    let mut missing = false;
    for name in [
        "queue",
        "capsfilter",
        "appsrc",
        "appsink",
        "h264parse",
        "avdec_h264",
        "videoconvert",
        "videoscale",
    ] {
        let found = gst::ElementFactory::find(name).is_some();
        println!("{name}: {}", if found { "ok" } else { "MISSING" });
        missing |= !found;
    }
    #[cfg(target_os = "linux")]
    for name in [
        "pipewiresrc",
        "ximagesrc",
        "videotestsrc",
        "videorate",
        "x264enc",
    ] {
        let found = gst::ElementFactory::find(name).is_some();
        println!("{name}: {}", if found { "ok" } else { "MISSING" });
        missing |= !found;
    }
    println!("GStreamer {}", gst::version_string());
    for name in [
        "qsvh264enc",
        "qsvh265enc",
        "vah264enc",
        "nvh264enc",
        "h265parse",
        "x265enc",
        "vah265enc",
        "nvh265enc",
        "nvh264dec",
        "nvh265dec",
        "vah264dec",
        "vah265dec",
        "qsvh264dec",
        "qsvh265dec",
        "vtdec_hw",
        "avdec_h265",
        "opusenc",
        "opusdec",
        "pulsesrc",
        "autoaudiosink",
    ] {
        println!(
            "{name}: {} (optional)",
            if gst::ElementFactory::find(name).is_some() {
                "available"
            } else {
                "unavailable"
            }
        );
    }
    ensure!(
        !missing,
        "missing GStreamer plugins; run inside nix develop"
    );
    Ok(())
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;

    #[test]
    fn recovery_requires_fresh_progress_even_when_failures_are_slow() {
        let now = Instant::now();
        let mut budget = RecoveryBudget::default();
        for index in 0..5 {
            budget
                .attempt(1, now + Duration::from_secs(index * 4))
                .unwrap();
        }
        assert!(budget.attempt(1, now + Duration::from_secs(20)).is_err());
        // Stale outputs do not count as fresh progress; a real ready frame does.
        assert!(budget.attempt(2, now + Duration::from_secs(24)).is_ok());
        let mut rapid = RecoveryBudget::default();
        for index in 0..5 {
            rapid.attempt(index, now).unwrap();
        }
        assert!(rapid.attempt(6, now).is_err());
    }

    const TEST_ENCODERS: [EncoderKind; 4] = [
        EncoderKind::Software,
        EncoderKind::Qsv,
        EncoderKind::Vaapi,
        EncoderKind::Nvidia,
    ];

    #[test]
    fn qsv_cli_and_low_latency_fragments_preserve_codec_precision() {
        use clap::ValueEnum;
        for value in ["qsv", "quick-sync"] {
            assert!(matches!(
                EncoderKind::from_str(value, false).unwrap(),
                EncoderKind::Qsv
            ));
        }
        for codec in [VideoCodec::H264, VideoCodec::H265] {
            let fragment = encoder_fragment(EncoderKind::Qsv, 60, 8000, codec);
            assert!(fragment.starts_with(match codec {
                VideoCodec::H264 => "qsvh264enc ",
                VideoCodec::H265 => "qsvh265enc ",
            }));
            for property in [
                "bitrate=8000",
                "gop-size=15",
                "b-frames=0",
                "low-latency=true",
                "target-usage=7",
                "rate-control=cbr",
            ] {
                assert!(
                    fragment.contains(property),
                    "missing {property}: {fragment}"
                );
            }
            assert!(fragment.contains(match codec {
                VideoCodec::H264 => "idr-interval=0",
                VideoCodec::H265 => "idr-interval=1",
            }));
            assert_eq!(encoder_pixel_format(EncoderKind::Qsv, codec), "NV12");
        }
        let hdr = VideoFormat {
            codec: VideoCodec::H265,
            dynamic_range: DynamicRange::Hdr10,
        };
        assert_eq!(encoder_pixel_format(EncoderKind::Qsv, hdr), "P010_10LE");
        assert!(encoder_fragment(EncoderKind::Qsv, 60, 8000, hdr).ends_with("profile=main-10"));
        assert!(
            encoder_fragment(EncoderKind::Qsv, 60, 8000, VideoCodec::H265)
                .ends_with("profile=main")
        );
        assert!(
            encoder_fragment(EncoderKind::Qsv, 1, 8000, VideoCodec::H264).contains("gop-size=1")
        );
        assert!(
            VideoFormat {
                codec: VideoCodec::H264,
                dynamic_range: DynamicRange::Hdr10
            }
            .validate()
            .is_err()
        );
    }

    fn test_encoder_available(kind: EncoderKind, format: impl Into<VideoFormat>) -> bool {
        if matches!(kind, EncoderKind::Software) {
            return true;
        }
        match select_encoder(kind, 640, 360, 60, 2000, format) {
            Ok(_) => true,
            Err(error) => {
                eprintln!("SKIP {kind:?}: hardware output unavailable: {error:#}");
                false
            }
        }
    }

    #[tokio::test]
    #[ignore = "requires software/hardware HEVC Main 10 and media plugins"]
    async fn hdr_main10_stream_preserves_p010_levels_and_generation() -> Result<()> {
        gst::init()?;
        let format = VideoFormat {
            codec: VideoCodec::H265,
            dynamic_range: DynamicRange::Hdr10,
        };
        for kind in TEST_ENCODERS {
            if !test_encoder_available(kind, format) {
                continue;
            }
            for (pattern, expected) in [("black", 64_i32), ("white", 940), ("gradient", -1)] {
                let mut broadcast = moq_net::broadcast::Info::new().produce();
                let video = broadcast.create_track("h265", None)?;
                let mut consumer = video
                    .clone()
                    .subscribe(moq_net::track::Subscription::default().with_group_start(0));
                let _encoder = encoder(
                    &format!(
                        "videotestsrc is-live=true pattern={pattern} ! capsfilter name=hdr_source caps=video/x-raw,format=RGB10A2_LE,colorimetry=1:1:14:7,width=320,height=512,framerate=30/1"
                    ),
                    (320, 512),
                    30,
                    8000,
                    video,
                    kind,
                    format,
                )?;
                let stats = Arc::new(crate::stats::StreamStats::default());
                prepare_decoder(false, format)?;
                let (_decoder, source, image) = decoder_with_stats(false, stats.clone(), format)?;
                let mut group = tokio::time::timeout(Duration::from_secs(3), consumer.recv_group())
                    .await??
                    .context("missing HDR group")?;
                let sequence = group.sequence;
                let bytes = crate::protocol::read_frame(&mut group, crate::protocol::MAX_FRAME)
                    .await?
                    .context("missing HDR frame")?;
                let pts = stats.received(bytes.len(), sequence);
                let mut buffer = gst::Buffer::from_slice(bytes);
                stamp_video_buffer(buffer.get_mut().unwrap(), pts);
                source.push_buffer(buffer)?;
                source.end_of_stream()?;
                let deadline = Instant::now() + Duration::from_secs(3);
                let frame = loop {
                    if let Some(frame) = image.lock().unwrap().take() {
                        break frame;
                    }
                    ensure!(Instant::now() < deadline, "no decoded HDR frame");
                    tokio::time::sleep(Duration::from_millis(2)).await;
                };
                assert_eq!(frame.video_group, Some(sequence));
                assert!(frame.data.is_empty());
                let hdr = frame.hdr.context("missing HDR planes")?;
                assert!(hdr.y_stride >= 640 && hdr.uv_stride >= 640);
                let luma = u16::from_le_bytes([hdr.y[0], hdr.y[1]]) >> 6;
                let chroma = u16::from_le_bytes([hdr.uv[0], hdr.uv[1]]) >> 6;
                if expected >= 0 {
                    ensure!(
                        (i32::from(luma) - expected).abs() <= 12,
                        "{kind:?} {pattern}: expected Y {expected}, got {luma}"
                    );
                } else {
                    let levels: std::collections::BTreeSet<_> = hdr
                        .y
                        .chunks_exact(2)
                        .map(|v| u16::from_le_bytes([v[0], v[1]]) >> 6)
                        .collect();
                    ensure!(
                        levels.len() > 256,
                        "{kind:?} gradient retained only {} distinct luma levels",
                        levels.len()
                    );
                    eprintln!(
                        "{kind:?} HDR gradient: {} distinct decoded luma levels",
                        levels.len()
                    );
                }
                ensure!(
                    (i32::from(chroma) - 512).abs() <= 12,
                    "wrong neutral chroma {chroma}"
                );
                eprintln!("{kind:?} HDR {pattern}: P010 Y={luma}, UV={chroma}, group={sequence}");
            }
        }
        Ok(())
    }

    #[test]
    fn hdr_contract_rejects_sdr_or_wrong_range() {
        gst::init().unwrap();
        assert!(
            VideoFormat {
                codec: VideoCodec::H264,
                dynamic_range: DynamicRange::Hdr10
            }
            .validate()
            .is_err()
        );
        let caps: gst::Caps =
            "video/x-raw,format=RGB10A2_LE,width=320,height=180,colorimetry=1:1:14:7"
                .parse()
                .unwrap();
        assert!(
            validate_hdr_raw(&gstreamer_video::VideoInfo::from_caps(&caps).unwrap(), true).is_ok()
        );
        let caps: gst::Caps = "video/x-raw,format=RGB10A2_LE,width=320,height=180,colorimetry=sRGB"
            .parse()
            .unwrap();
        assert!(
            validate_hdr_raw(&gstreamer_video::VideoInfo::from_caps(&caps).unwrap(), true).is_err()
        );
        assert!(validate_hdr_color(&"bt2100-pq".parse().unwrap(), true).is_err());
    }

    #[test]
    #[ignore = "requires x265 encoder plugins"]
    fn hdr_encoder_rejects_sdr_capture_without_publishing_frames() -> Result<()> {
        gst::init()?;
        let mut broadcast = moq_net::broadcast::Info::new().produce();
        let video = broadcast.create_track("h265", None)?;
        let format = VideoFormat {
            codec: VideoCodec::H265,
            dynamic_range: DynamicRange::Hdr10,
        };
        let result = encoder(
            "videotestsrc is-live=true ! capsfilter name=hdr_source caps=video/x-raw,format=RGB,width=320,height=180,framerate=30/1,colorimetry=sRGB",
            (320, 180),
            30,
            2000,
            video,
            EncoderKind::Software,
            format,
        );
        ensure!(result.is_err(), "SDR source was accepted as HDR");
        Ok(())
    }

    #[test]
    #[ignore = "requires software/hardware video encoders"]
    fn encoder_timings_use_segment_running_time() -> Result<()> {
        gst::init()?;
        for codec in [VideoCodec::H264, VideoCodec::H265] {
            for kind in TEST_ENCODERS {
                if !test_encoder_available(kind, codec) {
                    continue;
                }
                let pipeline = Pipeline(gst::parse::launch(&format!(
                    "videotestsrc num-buffers=6 ! video/x-raw,format={},width=640,height=360,framerate=60/1 ! {} ! appsink name=out sync=false",
                    encoder_pixel_format(kind, codec), encoder_fragment(kind, 60, 2000, codec)
                ))?.downcast::<gst::Pipeline>().unwrap());
                let metrics = pipeline.encoder_metrics()?;
                let sink = pipeline
                    .0
                    .by_name("out")
                    .unwrap()
                    .downcast::<AppSink>()
                    .unwrap();
                pipeline.0.set_state(gst::State::Playing)?;
                for _ in 0..6 {
                    ensure!(
                        sink.try_pull_sample(gst::ClockTime::from_seconds(3))
                            .is_some(),
                        "missing encoded frame"
                    );
                }
                ensure!(
                    metrics.encode_us.load(Ordering::Relaxed) > 0,
                    "missing encoder timing for {kind:?} {codec:?}"
                );
                eprintln!("encoder timing matched {kind:?} {codec:?}");
            }
        }
        Ok(())
    }

    #[test]
    #[ignore = "requires GStreamer software encoders and available video decoders"]
    fn decoded_frames_preserve_generation_for_both_codecs() -> Result<()> {
        gst::init()?;
        for codec in [VideoCodec::H264, VideoCodec::H265] {
            for force_software in [true, false] {
                let stats = Arc::new(crate::stats::StreamStats::default());
                prepare_decoder(force_software, codec)?;
                let (decoded, input, image) =
                    decoder_with_stats(force_software, stats.clone(), codec)?;
                let encoder = Pipeline(gst::parse::launch(&format!(
                    "videotestsrc num-buffers=1 ! video/x-raw,format=I420,width=320,height=180,framerate=30/1 \
                     ! {} ! {} ! {},stream-format=byte-stream,alignment=au \
                     ! appsink name=encoded sync=false",
                    encoder_fragment(EncoderKind::Software, 30, 1000, codec), parser(codec), compressed_caps(codec)
                ))?.downcast::<gst::Pipeline>().unwrap());
                let sink = encoder
                    .0
                    .by_name("encoded")
                    .unwrap()
                    .downcast::<AppSink>()
                    .unwrap();
                encoder.0.set_state(gst::State::Playing)?;
                let sample = sink
                    .try_pull_sample(gst::ClockTime::from_seconds(3))
                    .context("no encoded fixture")?;
                let bytes = sample
                    .buffer()
                    .context("missing fixture buffer")?
                    .map_readable()?;
                let pts = stats.received(bytes.len(), 77);
                let mut buffer = gst::Buffer::from_mut_slice(bytes.to_vec());
                stamp_video_buffer(buffer.get_mut().unwrap(), pts);
                input.push_buffer(buffer)?;
                input.end_of_stream()?;
                let deadline = Instant::now() + Duration::from_secs(3);
                let frame = loop {
                    if let Some(frame) = image.lock().unwrap().take() {
                        break frame;
                    }
                    ensure!(
                        Instant::now() < deadline,
                        "no decoded generation frame for {codec:?}"
                    );
                    std::thread::sleep(Duration::from_millis(5));
                };
                eprintln!(
                    "generation preserved: {codec:?} {}",
                    stats.decoder.lock().unwrap()
                );
                assert_eq!(frame.video_group, Some(77));
                assert_eq!((frame.width, frame.height), (320, 180));
                assert!(stats.snapshot().decode_us > 0);
                drop(decoded);
            }
        }
        Ok(())
    }

    #[test]
    #[ignore = "requires software encoders and native hardware decoder plugins"]
    fn sustained_decodes_preserve_every_generation() -> Result<()> {
        gst::init()?;
        let _ = tracing_subscriber::fmt()
            .with_env_filter("teleport=debug")
            .try_init();
        for codec in [VideoCodec::H264, VideoCodec::H265] {
            for kind in TEST_ENCODERS {
                if !test_encoder_available(kind, codec) {
                    continue;
                }
                let stats = Arc::new(crate::stats::StreamStats::default());
                prepare_decoder(false, codec)?;
                let (decoded, input, _image) = decoder_with_stats(false, stats.clone(), codec)?;
                // Deliberately destroy presentation PTS on every decoded frame:
                // generation and receive timing must use reference metadata.
                decoded
                    .0
                    .by_name("frames")
                    .unwrap()
                    .static_pad("sink")
                    .unwrap()
                    .add_probe(gst::PadProbeType::BUFFER, |_, info| {
                        if let Some(buffer) = info.buffer_mut() {
                            buffer.make_mut().set_pts(gst::ClockTime::ZERO);
                        }
                        gst::PadProbeReturn::Ok
                    });
                let encoder = Pipeline(gst::parse::launch(&format!(
                "videotestsrc num-buffers=60 ! video/x-raw,format={},width=1920,height=1080,framerate=60/1 \
                ! {} ! {} ! {},stream-format=byte-stream,alignment=au ! appsink name=encoded sync=false",
                encoder_pixel_format(kind, codec), encoder_fragment(kind, 60, 8000, codec), parser(codec), compressed_caps(codec)
            ))?.downcast::<gst::Pipeline>().unwrap());
                let sink = encoder
                    .0
                    .by_name("encoded")
                    .unwrap()
                    .downcast::<AppSink>()
                    .unwrap();
                encoder.0.set_state(gst::State::Playing)?;
                for index in 0..60 {
                    let sample = sink
                        .try_pull_sample(gst::ClockTime::from_seconds(3))
                        .context("missing encoded frame")?;
                    let bytes = sample.buffer().unwrap().map_readable()?;
                    let group = 10 + index / 15;
                    let pts = stats.received(bytes.len(), group);
                    let mut buffer = gst::Buffer::from_mut_slice(bytes.to_vec());
                    stamp_video_buffer(buffer.get_mut().unwrap(), pts);
                    input.push_buffer(buffer)?;
                    // Model bursty QUIC delivery rather than perfect frame pacing.
                    std::thread::sleep(Duration::from_millis(
                        [0, 0, 50, 0, 10, 40][index as usize % 6],
                    ));
                }
                input.end_of_stream()?;
                let deadline = Instant::now() + Duration::from_secs(3);
                while stats.decoded_frames.load(Ordering::Relaxed) < 60 {
                    ensure!(
                        Instant::now() < deadline,
                        "only {} decoded frames for {codec:?}",
                        stats.decoded_frames.load(Ordering::Relaxed)
                    );
                    std::thread::sleep(Duration::from_millis(1));
                }
                ensure!(
                    stats.unmatched_frames.load(Ordering::Relaxed) == 0,
                    "{codec:?}: {} unmatched decoded PTS",
                    stats.unmatched_frames.load(Ordering::Relaxed)
                );
                eprintln!("60/60 generation timestamps matched for {codec:?} {kind:?}");
            }
        }
        Ok(())
    }

    #[tokio::test]
    #[ignore = "requires GStreamer appsrc; checks delayed internal-output pressure"]
    async fn stale_internal_output_recovers_even_with_empty_input_queue() -> Result<()> {
        gst::init()?;
        let pipeline = Pipeline(
            gst::parse::launch("appsrc name=in is-live=true ! fakesink sync=false")?
                .downcast::<gst::Pipeline>()
                .unwrap(),
        );
        let input = pipeline
            .0
            .by_name("in")
            .unwrap()
            .downcast::<AppSrc>()
            .unwrap();
        pipeline.0.set_state(gst::State::Playing)?;
        let stats = Arc::new(crate::stats::StreamStats::default());
        assert_eq!(input.current_level_buffers(), 0);
        assert!(!decoder_needs_recovery(&input, &stats, 0));
        // Model a driver accepting input promptly but delivering late output.
        stats.stale_frames.store(3, Ordering::Relaxed);
        assert!(decoder_needs_recovery(&input, &stats, 0));
        reset_decoder(&input, stats.clone()).await?;
        assert_eq!(stats.snapshot().decoder_recoveries, 1);
        assert!(!decoder_needs_recovery(&input, &stats, 3));
        Ok(())
    }

    #[tokio::test]
    #[ignore = "requires GStreamer; exercises cancellation of a blocked state reset"]
    async fn cancelled_decoder_reset_finishes_in_null_state() -> Result<()> {
        gst::init()?;
        let pipeline = Pipeline(gst::parse::launch(
            "appsrc name=in is-live=true format=time ! identity name=slow ! fakesink sync=false"
        )?.downcast::<gst::Pipeline>().unwrap());
        let input = pipeline
            .0
            .by_name("in")
            .unwrap()
            .downcast::<AppSrc>()
            .unwrap();
        let entered = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let signal = entered.clone();
        pipeline
            .0
            .by_name("slow")
            .unwrap()
            .static_pad("sink")
            .unwrap()
            .add_probe(gst::PadProbeType::BUFFER, move |_, _| {
                signal.store(true, Ordering::Relaxed);
                std::thread::sleep(Duration::from_millis(250));
                gst::PadProbeReturn::Ok
            });
        pipeline.0.set_state(gst::State::Playing)?;
        input.push_buffer(gst::Buffer::from_slice(vec![0u8; 16]))?;
        let deadline = Instant::now() + Duration::from_secs(3);
        while !entered.load(Ordering::Relaxed) {
            ensure!(Instant::now() < deadline, "probe never blocked");
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        let reset = tokio::spawn(async move {
            reset_decoder(&input, Arc::new(crate::stats::StreamStats::default())).await
        });
        tokio::time::sleep(Duration::from_millis(20)).await;
        reset.abort();
        let _ = reset.await;
        // Any detached worker has completed or observed cancellation before this
        // assertion. It cannot set Playing after cleanup.
        assert_eq!(pipeline.0.current_state(), gst::State::Null);
        Ok(())
    }

    #[tokio::test]
    #[ignore = "requires GStreamer software codecs; injects a decoder stall"]
    async fn decoder_backlog_discards_gop_and_recovers_at_fresh_keyframe() -> Result<()> {
        gst::init()?;
        for codec in [VideoCodec::H264, VideoCodec::H265] {
            let encoder = Pipeline(gst::parse::launch(&format!(
                "videotestsrc num-buffers=20 ! video/x-raw,format=I420,width=320,height=180,framerate=30/1 ! {} ! {} config-interval=-1 ! {},stream-format=byte-stream,alignment=au ! appsink name=out sync=false",
                encoder_fragment(EncoderKind::Software, 30, 1000, codec), parser(codec), compressed_caps(codec)
            ))?.downcast::<gst::Pipeline>().unwrap());
            let output = encoder
                .0
                .by_name("out")
                .unwrap()
                .downcast::<AppSink>()
                .unwrap();
            encoder.0.set_state(gst::State::Playing)?;
            let mut frames = Vec::new();
            for _ in 0..20 {
                let sample = output
                    .try_pull_sample(gst::ClockTime::from_seconds(3))
                    .context("encoder stalled")?;
                frames.push(bytes::Bytes::copy_from_slice(
                    sample.buffer().unwrap().map_readable()?.as_slice(),
                ));
            }
            let stats = Arc::new(crate::stats::StreamStats::default());
            let (pipeline, input, image) = decoder_with_stats(true, stats.clone(), codec)?;
            let stall = Arc::new(std::sync::atomic::AtomicBool::new(true));
            let entered = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let entered_probe = entered.clone();
            let slow = stall.clone();
            pipeline
                .0
                .by_name("video_decoder")
                .unwrap()
                .static_pad("sink")
                .unwrap()
                .add_probe(gst::PadProbeType::BUFFER, move |_, _| {
                    if slow.load(Ordering::Relaxed) {
                        entered_probe.store(true, Ordering::Relaxed);
                        std::thread::sleep(Duration::from_millis(400));
                    }
                    gst::PadProbeReturn::Ok
                });
            let origin = moq_net::Origin::random().produce();
            let mut broadcast =
                origin.create_broadcast("backlog", moq_net::broadcast::Route::new())?;
            let mut track = broadcast.create_track("video", moq_net::track::Info::default())?;
            let subscriber = track.subscribe(None);
            let reader_stats = stats.clone();
            let task = tokio::spawn(async move {
                receive_video_inner(subscriber, input, Arc::new(AtomicU32::new(0)), reader_stats)
                    .await
            });
            let mut old = track.create_group(moq_net::group::Info { sequence: 1 })?;
            old.write_frame(moq_net::Timestamp::now(), frames[0].clone())?;
            let deadline = Instant::now() + Duration::from_secs(3);
            while !entered.load(Ordering::Relaxed) {
                ensure!(Instant::now() < deadline, "test stall was not entered");
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            for bytes in &frames[1..3] {
                old.write_frame(moq_net::Timestamp::now(), bytes.clone())?;
            }
            // Exercise the age limit while well below byte/frame limits.
            tokio::time::sleep(Duration::from_millis(180)).await;
            old.write_frame(moq_net::Timestamp::now(), frames[3].clone())?;
            old.finish()?;
            let deadline = Instant::now() + Duration::from_secs(5);
            while stats.decoder_recoveries.load(Ordering::Relaxed) == 0 {
                ensure!(
                    Instant::now() < deadline,
                    "backlog did not recover for {codec:?}"
                );
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            stall.store(false, Ordering::Relaxed);
            // Publish only a keyframe from a fresh group, not the abandoned GOP's
            // dependent frames. Recovery must produce output without an EOS drain.
            let mut fresh = track.create_group(moq_net::group::Info { sequence: 2 })?;
            fresh.write_frame(moq_net::Timestamp::now(), frames[0].clone())?;
            let deadline = Instant::now() + Duration::from_secs(3);
            loop {
                if image
                    .lock()
                    .unwrap()
                    .as_ref()
                    .is_some_and(|frame| frame.video_group == Some(2))
                {
                    break;
                }
                ensure!(
                    Instant::now() < deadline,
                    "no fresh frame after {codec:?} recovery"
                );
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            assert!(stats.snapshot().decode_us < 250_000);
            assert!(stats.snapshot().queue_us > 0);
            assert!(stats.snapshot().decoder_us > 0);
            assert!(stats.snapshot().conversion_us > 0);
            task.abort();
            let _ = task.await;
        }
        Ok(())
    }

    #[tokio::test]
    async fn reordered_old_group_does_not_cancel_partial_frame() -> Result<()> {
        let origin = moq_net::Origin::random().produce();
        let mut broadcast = origin.create_broadcast("test", moq_net::broadcast::Route::new())?;
        let mut producer = broadcast.create_track("video", moq_net::track::Info::default())?;
        let mut subscriber = producer.subscribe(None);
        let mut latest = producer.create_group(moq_net::group::Info { sequence: 2 })?;
        let mut frame = latest.create_frame(moq_net::frame::Info {
            size: 6,
            timestamp: moq_net::Timestamp::now(),
        })?;
        frame.write(bytes::Bytes::from_static(b"abc"))?;
        let mut current = Some(subscriber.recv_group().await?.context("missing group")?);
        let read = next_media_event(&mut subscriber, &mut current, Some(2), 1024);
        tokio::pin!(read);
        assert!(read.as_mut().now_or_never().is_none());
        let mut old = producer.create_group(moq_net::group::Info { sequence: 1 })?;
        old.write_frame(moq_net::Timestamp::now(), bytes::Bytes::from_static(b"old"))?;
        old.finish()?;
        assert!(read.as_mut().now_or_never().is_none());
        frame.write(bytes::Bytes::from_static(b"def"))?;
        frame.finish()?;
        match tokio::time::timeout(Duration::from_secs(1), read).await?? {
            MediaEvent::Frame(Ok(Some(bytes))) => assert_eq!(bytes.as_ref(), b"abcdef"),
            _ => anyhow::bail!("partial frame was lost after older group arrived"),
        }
        Ok(())
    }

    #[test]
    #[ignore = "requires GStreamer video plugins; run inside nix develop"]
    fn selected_encoder_produces_decodable_video_and_updates_bitrate() -> Result<()> {
        gst::init()?;
        for codec in [VideoCodec::H264, VideoCodec::H265] {
            for requested in std::iter::once(EncoderKind::Auto).chain(TEST_ENCODERS) {
                if !matches!(requested, EncoderKind::Auto)
                    && !test_encoder_available(requested, codec)
                {
                    continue;
                }
                let kind = select_encoder(requested, 640, 480, 30, 1_000, codec)?;
                eprintln!("actual selected encoder: {kind:?}");
                let format = encoder_pixel_format(kind, codec);
                let decoder = match codec {
                    VideoCodec::H264 => "avdec_h264",
                    VideoCodec::H265 => "avdec_h265",
                };
                let pipeline = Pipeline(gst::parse::launch(&format!(
            "videotestsrc is-live=true ! video/x-raw,format={format},width=640,height=480,framerate=30/1 ! {} ! {} ! {decoder} ! appsink name=decoded sync=false max-buffers=2 drop=true",
            encoder_fragment(kind, 30, 1_000, codec), parser(codec)
        ))?.downcast::<gst::Pipeline>().unwrap());
                let sink = pipeline
                    .0
                    .by_name("decoded")
                    .unwrap()
                    .downcast::<AppSink>()
                    .unwrap();
                pipeline.0.set_state(gst::State::Playing)?;
                assert!(
                    sink.try_pull_sample(gst::ClockTime::from_seconds(3))
                        .is_some()
                );
                // Respect the installed plugin's advertised live mutation support.
                // QSV 1.26 and some VA drivers do not advertise this capability.
                let supports_live_bitrate = pipeline
                    .0
                    .by_name("video_encoder")
                    .unwrap()
                    .find_property("bitrate")
                    .unwrap()
                    .flags()
                    .contains(gst::PARAM_FLAG_MUTABLE_PLAYING);
                if supports_live_bitrate {
                    pipeline.set_video_bitrate(750)?;
                    assert_eq!(
                        pipeline
                            .0
                            .by_name("video_encoder")
                            .unwrap()
                            .property::<u32>("bitrate"),
                        750
                    );
                    assert!(
                        sink.try_pull_sample(gst::ClockTime::from_seconds(3))
                            .is_some()
                    );
                } else {
                    assert!(pipeline.set_video_bitrate(750).is_err());
                }
                pipeline.error()?;
            }
        }
        Ok(())
    }

    #[test]
    fn bitrate_reduces_quickly_and_recovers_slowly() {
        let mut rate = BitrateController::new(8_000);
        assert_eq!(rate.observe(100, 0), Some(6_000));
        for _ in 0..4 {
            assert_eq!(rate.observe(0, 0), None);
        }
        assert_eq!(rate.observe(0, 0), Some(6_400));
        assert_eq!(rate.observe(0, 2), Some(4_800));
        for _ in 0..100 {
            rate.observe(1000, 10);
        }
        assert_eq!(rate.current, 500);
        for _ in 0..1000 {
            rate.observe(0, 0);
        }
        assert_eq!(rate.current, 8_000);
    }

    #[test]
    fn bitrate_never_exceeds_a_low_configured_cap() {
        let mut rate = BitrateController::new(200);
        assert_eq!(rate.observe(1000, 10), None);
        assert_eq!(rate.current, 200);
    }
}
