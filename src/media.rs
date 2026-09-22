use anyhow::{Context, Result, ensure};
use futures_util::FutureExt;
use gst::prelude::*;
use gstreamer as gst;
use gstreamer_app::{AppSink, AppSrc};
use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicU32, Ordering},
    },
    time::Duration,
};

pub struct Pipeline(pub gst::Pipeline);
impl Drop for Pipeline {
    fn drop(&mut self) {
        let _ = self.0.set_state(gst::State::Null);
    }
}
impl Pipeline {
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
#[derive(Clone, Copy, Debug, Default, clap::ValueEnum)]
pub enum EncoderKind {
    #[default]
    Auto,
    Software,
    Vaapi,
    Nvidia,
}

#[cfg(target_os = "linux")]
fn encoder_fragment(kind: EncoderKind, fps: u32, bitrate: u32) -> String {
    let gop = (fps / 4).max(1);
    match kind {
        EncoderKind::Software => format!(
            "x264enc name=video_encoder tune=zerolatency speed-preset=ultrafast bitrate={bitrate} key-int-max={gop} bframes=0 threads=4 ! video/x-h264,profile=constrained-baseline"
        ),
        EncoderKind::Vaapi => format!(
            "vah264enc name=video_encoder bitrate={bitrate} key-int-max={gop} b-frames=0 rate-control=cbr"
        ),
        EncoderKind::Nvidia => format!(
            "nvh264enc name=video_encoder bitrate={bitrate} gop-size={gop} bframes=0 zerolatency=true rc-mode=cbr"
        ),
        EncoderKind::Auto => unreachable!("auto must be resolved first"),
    }
}

#[cfg(target_os = "linux")]
fn select_encoder(
    kind: EncoderKind,
    width: u32,
    height: u32,
    fps: u32,
    bitrate: u32,
) -> Result<EncoderKind> {
    if matches!(kind, EncoderKind::Software) {
        return Ok(kind);
    }
    let choices: &[EncoderKind] = if matches!(kind, EncoderKind::Auto) {
        &[EncoderKind::Vaapi, EncoderKind::Nvidia]
    } else {
        std::slice::from_ref(&kind)
    };
    for &candidate in choices {
        // Probe real encoded output, not just plugin presence. Drivers/devices may
        // be unavailable even when a factory is registered. Never open capture here.
        let probe = || -> Result<()> {
            let description = format!(
                "videotestsrc num-buffers=3 ! video/x-raw,format=NV12,width={width},height={height},framerate={fps}/1 ! {} ! h264parse ! appsink name=probe sync=false",
                encoder_fragment(candidate, fps, bitrate)
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
            pipeline.error()?;
            ensure!(
                sample.is_some(),
                "hardware encoder produced no frame within 3 seconds"
            );
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
    width: u32,
    height: u32,
    fps: u32,
    bitrate: u32,
    mut track: moq_net::track::Producer,
    kind: EncoderKind,
) -> Result<Pipeline> {
    let kind = select_encoder(kind, width, height, fps, bitrate)?;
    tracing::info!(?kind, bitrate, "video encoder");
    let fragment = encoder_fragment(kind, fps, bitrate);
    let description = format!(
        "{source} ! queue max-size-buffers=2 max-size-bytes=0 max-size-time=0 leaky=downstream \
        ! videoconvert ! videoscale ! videorate drop-only=true \
        ! video/x-raw,format=NV12,width={width},height={height},framerate={fps}/1 \
        ! {fragment} \
        ! h264parse config-interval=-1 ! video/x-h264,stream-format=byte-stream,alignment=au \
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
    sink.set_callbacks(
        gstreamer_app::AppSinkCallbacks::builder()
            .new_sample(move |sink| {
                let sample = sink.pull_sample().map_err(|_| gst::FlowError::Eos)?;
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
    Ok(pipeline)
}

pub struct Image {
    pub data: Vec<u8>,
    pub width: u32,
    pub height: u32,
    pub stride: usize,
}
pub type LatestImage = Arc<Mutex<Option<Image>>>;

pub fn decoder(force_software: bool) -> Result<(Pipeline, AppSrc, LatestImage)> {
    // Explicit software baseline on Linux; native VideoToolbox decoder on macOS
    // when installed, with software fallback for diagnosis.
    let decoder = if !force_software
        && cfg!(target_os = "macos")
        && gst::ElementFactory::find("vtdec_hw").is_some()
    {
        "vtdec_hw"
    } else {
        "avdec_h264 max-threads=2"
    };
    tracing::info!(decoder, "video decoder");
    let pipeline = Pipeline(gst::parse::launch(&format!(
        "appsrc name=in is-live=true format=time do-timestamp=true max-bytes=16777216 block=false \
        caps=video/x-h264,stream-format=byte-stream,alignment=au \
        ! h264parse ! {decoder} ! videoconvert ! video/x-raw,format=RGB \
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
    let image: LatestImage = Arc::new(Mutex::new(None));
    let latest = image.clone();
    sink.set_callbacks(
        gstreamer_app::AppSinkCallbacks::builder()
            .new_sample(move |sink| {
                let sample = sink.pull_sample().map_err(|_| gst::FlowError::Eos)?;
                let info = gstreamer_video::VideoInfo::from_caps(
                    sample.caps().ok_or(gst::FlowError::Error)?,
                )
                .map_err(|_| gst::FlowError::Error)?;
                let frame = gstreamer_video::VideoFrameRef::from_buffer_ref_readable(
                    sample.buffer().ok_or(gst::FlowError::Error)?,
                    &info,
                )
                .map_err(|_| gst::FlowError::Error)?;
                use gstreamer_video::prelude::*;
                let data = frame
                    .plane_data(0)
                    .map_err(|_| gst::FlowError::Error)?
                    .to_vec();
                *latest.lock().unwrap() = Some(Image {
                    data,
                    width: info.width(),
                    height: info.height(),
                    stride: frame.plane_stride()[0] as usize,
                });
                Ok(gst::FlowSuccess::Ok)
            })
            .build(),
    );
    pipeline.0.set_state(gst::State::Playing)?;
    Ok((pipeline, source, image))
}

pub async fn receive_video_with_feedback(
    track: moq_net::track::Subscriber,
    source: AppSrc,
    events: tokio::sync::mpsc::Sender<crate::protocol::Event>,
) -> Result<()> {
    let dropped = Arc::new(AtomicU32::new(0));
    let monitor = async {
        let mut interval = tokio::time::interval(Duration::from_secs(1));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            interval.tick().await;
            let queue_ms = source
                .current_level_time()
                .map_or(0, |time| time.mseconds().min(u32::MAX as u64) as u32);
            let dropped_groups = dropped.swap(0, Ordering::Relaxed);
            let _ = events.try_send(crate::protocol::Event::Feedback {
                queue_ms,
                dropped_groups,
            });
        }
    };
    // Keep the read future alive across feedback ticks. Cancelling read_frame in
    // the middle of a GOP would lose a partially-read H.264 access unit.
    tokio::select! {
        result = receive_video_inner(track, source.clone(), dropped.clone()) => result,
        () = monitor => unreachable!(),
    }
}

async fn receive_video_inner(
    mut track: moq_net::track::Subscriber,
    source: AppSrc,
    dropped: Arc<AtomicU32>,
) -> Result<()> {
    let mut current: Option<moq_net::group::Consumer> = None;
    let mut sequence = None;
    let record_drop = |count: u32| {
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
                    ensure!(
                        source.current_level_bytes() < 16 * 1024 * 1024,
                        "decoder is not keeping up"
                    );
                    source.push_buffer(gst::Buffer::from_slice(bytes))?;
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
        "vah264enc",
        "nvh264enc",
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
        for requested in [EncoderKind::Auto, EncoderKind::Software] {
            let kind = select_encoder(requested, 640, 480, 30, 1_000)?;
            eprintln!("actual selected encoder: {kind:?}");
            let pipeline = Pipeline(gst::parse::launch(&format!(
            "videotestsrc is-live=true ! video/x-raw,format=NV12,width=640,height=480,framerate=30/1 ! {} ! h264parse ! avdec_h264 ! appsink name=decoded sync=false max-buffers=2 drop=true",
            encoder_fragment(kind, 30, 1_000)
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
            // Not all VA drivers permit live mutation; the API must report this rather
            // than claim it succeeded. Software and NVENC advertise mutable bitrate.
            if !matches!(kind, EncoderKind::Vaapi) {
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
            }
            pipeline.error()?;
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
