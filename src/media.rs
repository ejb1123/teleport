use anyhow::{Context, Result, ensure};
use gst::prelude::*;
use gstreamer as gst;
use gstreamer_app::{AppSink, AppSrc};
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

pub struct Pipeline(pub gst::Pipeline);
impl Drop for Pipeline {
    fn drop(&mut self) {
        let _ = self.0.set_state(gst::State::Null);
    }
}
impl Pipeline {
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
pub fn encoder(
    source: &str,
    width: u32,
    height: u32,
    fps: u32,
    bitrate: u32,
    mut track: moq_net::track::Producer,
) -> Result<Pipeline> {
    let description = format!(
        "{source} ! queue max-size-buffers=2 max-size-bytes=0 max-size-time=0 leaky=downstream \
        ! videoconvert ! videoscale ! videorate drop-only=true \
        ! video/x-raw,format=I420,width={width},height={height},framerate={fps}/1 \
        ! x264enc tune=zerolatency speed-preset=ultrafast bitrate={bitrate} key-int-max={} bframes=0 threads=4 \
        ! video/x-h264,profile=constrained-baseline \
        ! h264parse config-interval=-1 ! video/x-h264,stream-format=byte-stream,alignment=au \
        ! appsink name=encoded sync=false max-buffers=2 drop=false",
        (fps / 4).max(1)
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

pub async fn receive_video(mut track: moq_net::track::Subscriber, source: AppSrc) -> Result<()> {
    let mut current: Option<moq_net::group::Consumer> = None;
    let mut sequence = None;
    loop {
        tokio::select! {
            // A newer GOP starts with a keyframe; cancel stale GOP reads immediately.
            biased;
            next = track.recv_group() => {
                let next = next?.context("video track closed")?;
                if sequence.is_none_or(|seq| next.sequence > seq) {
                    sequence = Some(next.sequence);
                    current = Some(next);
                }
            }
            frame = async {
                match &mut current {
                    Some(group) => crate::protocol::read_frame(group, crate::protocol::MAX_FRAME).await,
                    None => std::future::pending().await,
                }
            } => {
                match frame {
                    Ok(Some(bytes)) => {
                        ensure!(source.current_level_bytes() < 16 * 1024 * 1024, "decoder is not keeping up");
                        source.push_buffer(gst::Buffer::from_slice(bytes))?;
                    }
                    Ok(None) => current = None,
                    Err(error) => { tracing::debug!(%error, "skipping incomplete video group"); current = None; }
                }
            }
        }
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
    ensure!(
        !missing,
        "missing GStreamer plugins; run inside nix develop"
    );
    Ok(())
}
