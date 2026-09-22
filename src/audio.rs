//! Opt-in desktop audio: explicitly selected PulseAudio/PipeWire monitor only.
//! Each 10 ms Opus packet is independently delivered so stale audio is discarded.
use crate::media::Pipeline;
use anyhow::{Result, ensure};
use gst::prelude::*;
use gstreamer as gst;
use gstreamer_app::{AppSink, AppSrc};

#[cfg(target_os = "linux")]
fn capture_description(source: &str) -> Result<String> {
    // Do not accept arbitrary launch syntax or silently record the microphone.
    if source == "test" {
        return Ok("audiotestsrc is-live=true wave=sine volume=0.05".into());
    }
    ensure!(
        source.ends_with(".monitor")
            && source.len() <= 512
            && source
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || b"._-:".contains(&c)),
        "audio source must be an explicit PulseAudio/PipeWire .monitor name (use pactl list short sources), or test"
    );
    Ok(format!(
        "pulsesrc device={source} provide-clock=false buffer-time=20000 latency-time=10000"
    ))
}

#[cfg(target_os = "linux")]
pub fn capture(source: &str, mut track: moq_net::track::Producer) -> Result<Pipeline> {
    let source = capture_description(source)?;
    let pipeline = Pipeline(
        gst::parse::launch(&format!(
            "{source} ! audioconvert ! audioresample ! audio/x-raw,rate=48000,channels=2 \
        ! opusenc bitrate=128000 frame-size=10 audio-type=restricted-lowdelay \
        ! appsink name=encoded_audio sync=false max-buffers=4 drop=false"
        ))?
        .downcast::<gst::Pipeline>()
        .map_err(|_| anyhow::anyhow!("not a pipeline"))?,
    );
    let sink = pipeline
        .0
        .by_name("encoded_audio")
        .unwrap()
        .downcast::<AppSink>()
        .unwrap();
    sink.set_callbacks(
        gstreamer_app::AppSinkCallbacks::builder()
            .new_sample(move |sink| {
                let sample = sink.pull_sample().map_err(|_| gst::FlowError::Eos)?;
                let buffer = sample.buffer().ok_or(gst::FlowError::Error)?;
                if buffer.flags().contains(gst::BufferFlags::HEADER) {
                    return Ok(gst::FlowSuccess::Ok);
                }
                let bytes = buffer.map_readable().map_err(|_| gst::FlowError::Error)?;
                let mut group = track.append_group().map_err(|_| gst::FlowError::Error)?;
                group
                    .write_frame(
                        moq_net::Timestamp::now(),
                        bytes::Bytes::copy_from_slice(bytes.as_slice()),
                    )
                    .map_err(|_| gst::FlowError::Error)?;
                group.finish().map_err(|_| gst::FlowError::Error)?;
                Ok(gst::FlowSuccess::Ok)
            })
            .build(),
    );
    pipeline.0.set_state(gst::State::Playing)?;
    Ok(pipeline)
}

pub fn decoder() -> Result<(Pipeline, AppSrc)> {
    decoder_with_sink("autoaudiosink")
}

/// Decode without opening the client's audio device, while proving PCM delivery.
pub fn decoder_for_headless() -> Result<(
    Pipeline,
    AppSrc,
    std::sync::Arc<std::sync::atomic::AtomicU64>,
)> {
    use std::sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    };
    let (pipeline, source) =
        decoder_with_sink("appsink name=audio_pcm sync=false max-buffers=4 drop=true")?;
    let sink = pipeline
        .0
        .by_name("audio_pcm")
        .unwrap()
        .downcast::<AppSink>()
        .unwrap();
    let counter = Arc::new(AtomicU64::new(0));
    let decoded = counter.clone();
    sink.set_callbacks(
        gstreamer_app::AppSinkCallbacks::builder()
            .new_sample(move |sink| {
                let sample = sink.pull_sample().map_err(|_| gst::FlowError::Eos)?;
                if sample.buffer().is_some_and(|buffer| buffer.size() > 0) {
                    decoded.fetch_add(1, Ordering::Relaxed);
                }
                Ok(gst::FlowSuccess::Ok)
            })
            .build(),
    );
    Ok((pipeline, source, counter))
}

fn decoder_with_sink(sink: &str) -> Result<(Pipeline, AppSrc)> {
    let pipeline = Pipeline(gst::parse::launch(&format!(
        "appsrc name=audio_in is-live=true format=time do-timestamp=true max-bytes=65536 block=false \
        caps=audio/x-opus,rate=48000,channels=2,channel-mapping-family=0 \
        ! opusdec ! audioconvert ! audioresample ! volume name=audio_volume \
        ! queue max-size-time=100000000 max-size-buffers=0 max-size-bytes=0 leaky=downstream ! {sink}"
    ))?.downcast::<gst::Pipeline>().map_err(|_| anyhow::anyhow!("not a pipeline"))?);
    let source = pipeline
        .0
        .by_name("audio_in")
        .unwrap()
        .downcast::<AppSrc>()
        .unwrap();
    pipeline.0.set_state(gst::State::Playing)?;
    Ok((pipeline, source))
}

pub async fn receive(mut track: moq_net::track::Subscriber, source: AppSrc) -> Result<()> {
    let mut sequence = None;
    let mut current: Option<moq_net::group::Consumer> = None;
    loop {
        match crate::media::next_media_event(&mut track, &mut current, sequence, 4096).await? {
            crate::media::MediaEvent::Group(next) => {
                sequence = Some(next.sequence);
                current = Some(next);
            }
            crate::media::MediaEvent::Frame(packet) => {
                current = None;
                if let Ok(Some(bytes)) = packet {
                    ensure!(
                        source.current_level_bytes() < 65536,
                        "audio decoder is not keeping up"
                    );
                    let mut buffer = gst::Buffer::from_slice(bytes);
                    buffer
                        .get_mut()
                        .unwrap()
                        .set_duration(gst::ClockTime::from_mseconds(10));
                    source.push_buffer(buffer)?;
                }
            }
        }
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use anyhow::Context;

    #[test]
    fn refuses_microphones_and_pipeline_injection() {
        assert!(capture_description("alsa_output.pci-0000_00_1f.3.analog-stereo.monitor").is_ok());
        for source in [
            "",
            "default",
            "alsa_input.mic",
            "x.monitor ! filesink location=/tmp/evil",
            "x\".monitor",
        ] {
            assert!(capture_description(source).is_err());
        }
    }

    #[test]
    #[ignore = "requires GStreamer Opus plugins; run inside nix develop"]
    fn raw_opus_packets_decode_without_external_headers() -> Result<()> {
        gst::init()?;
        let encoder = Pipeline(gst::parse::launch(
            "audiotestsrc num-buffers=20 ! audioconvert ! audioresample ! audio/x-raw,rate=48000,channels=2 ! opusenc frame-size=10 audio-type=restricted-lowdelay ! appsink name=packets sync=false"
        )?.downcast::<gst::Pipeline>().unwrap());
        let packets = encoder
            .0
            .by_name("packets")
            .unwrap()
            .downcast::<AppSink>()
            .unwrap();
        let (decoder, source) = decoder_with_sink("appsink name=pcm sync=false")?;
        let pcm = decoder
            .0
            .by_name("pcm")
            .unwrap()
            .downcast::<AppSink>()
            .unwrap();
        encoder.0.set_state(gst::State::Playing)?;
        for _ in 0..10 {
            let sample = packets
                .try_pull_sample(gst::ClockTime::from_seconds(2))
                .context("no Opus packet")?;
            let bytes = sample.buffer().unwrap().map_readable()?;
            let mut packet = gst::Buffer::from_slice(bytes.as_slice().to_vec());
            packet
                .get_mut()
                .unwrap()
                .set_duration(gst::ClockTime::from_mseconds(10));
            source.push_buffer(packet)?;
        }
        let decoded = pcm
            .try_pull_sample(gst::ClockTime::from_seconds(2))
            .context("no decoded audio")?;
        ensure!(decoded.buffer().unwrap().size() > 0, "empty PCM");
        decoder.set_audio_muted(true)?;
        assert!(
            decoder
                .0
                .by_name("audio_volume")
                .unwrap()
                .property::<bool>("mute")
        );
        encoder.error()?;
        decoder.error()?;
        Ok(())
    }
}
