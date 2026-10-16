//! Opt-in desktop audio: explicitly selected PulseAudio/PipeWire monitor only.
//! Each 10 ms Opus packet is independently delivered so stale audio is discarded.
use crate::media::Pipeline;
use anyhow::{Result, ensure};
use gst::prelude::*;
use gstreamer as gst;
use gstreamer_app::{AppSink, AppSrc};
use std::time::Duration;

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
    // pulsesink defaults to a 200 ms buffer; 40 ms keeps 4 packets queued
    // and cut measured playout latency by ~40 ms (docs/review-2026-10.md, M8).
    let sink = if cfg!(target_os = "linux") && gst::ElementFactory::find("pulsesink").is_some() {
        "pulsesink buffer-time=40000 latency-time=10000"
    } else {
        "autoaudiosink"
    };
    decoder_with_sink(sink)
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
        "appsrc name=audio_in is-live=true format=time max-bytes=65536 block=false \
        caps=audio/x-opus,rate=48000,channels=2,channel-mapping-family=0 \
        ! opusdec name=opus plc=true ! audioconvert ! audioresample ! volume name=audio_volume \
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

const PACKET: Duration = Duration::from_millis(10);
/// How early the latest packet of a window should arrive before its slot.
const SLACK: Duration = Duration::from_millis(2);
/// Lateness the sink absorbs (its own buffering); later packets move playout.
const LATE: Duration = Duration::from_millis(10);
/// A packet this far ahead of its slot (sender clock drift) restarts playout.
const AHEAD: Duration = Duration::from_millis(100);
/// Longest gap concealed by Opus PLC; longer gaps start over.
const MAX_CONCEALED: Duration = Duration::from_millis(100);
/// Packets per adaptation window: if every packet in one arrived early by
/// more than SLACK, playout moves earlier by the excess.
const WINDOW: u32 = 200;

/// Adaptive playout timing for 10 ms packets, one per MoQ group: packet n
/// plays 10 ms after packet n - 1. A late packet moves playout later by its
/// lateness at once (the hole is concealed); playout moves earlier again once
/// a whole window arrived early. Network and capture jitter then cost a
/// steady delay instead of clipped or resynchronised audio, and a missing
/// sequence number leaves a hole that Opus conceals.
#[derive(Default)]
struct Timeline {
    /// Sequence and pts of a reference packet, and of the last one played.
    anchor: Option<(u64, Duration)>,
    last: Option<(u64, Duration)>,
    /// Packets in this window, and the least time any arrived before its slot.
    window: u32,
    min_early: Option<Duration>,
    /// The next packet starts a moved timeline.
    resync: bool,
}

#[derive(Debug, PartialEq)]
enum Playout {
    Play {
        pts: Duration,
        /// Playout moved (first packet, a late packet, drift or a long gap).
        discont: bool,
        /// Audio missing before this packet: (pts, duration) to conceal.
        conceal: Option<(Duration, Duration)>,
    },
    Drop,
}

impl Timeline {
    fn packet(&mut self, sequence: u64, now: Duration) -> Playout {
        let (Some((first, start)), Some((last, last_pts))) = (self.anchor, self.last) else {
            return self.restart(sequence, now);
        };
        if sequence <= last {
            return Playout::Drop;
        }
        let mut pts = start + PACKET * (sequence - first) as u32;
        let mut discont = std::mem::take(&mut self.resync);
        if pts + LATE < now {
            let shift = now + SLACK - pts;
            self.anchor = Some((first, start + shift));
            pts += shift;
            discont = true;
        } else if pts > now + AHEAD {
            return self.restart(sequence, now);
        }
        let hole = pts.saturating_sub(last_pts + PACKET);
        if hole > MAX_CONCEALED {
            return self.restart(sequence, now);
        }
        self.last = Some((sequence, pts));
        let early = pts.saturating_sub(now);
        self.min_early = Some(self.min_early.map_or(early, |least| least.min(early)));
        self.window += 1;
        if self.window == WINDOW {
            if let Some(excess) = self.min_early.and_then(|least| least.checked_sub(SLACK))
                && !excess.is_zero()
            {
                // Later packets play earlier; the sink drops the overlap once.
                // (Saturating: the pipeline clock may have started recently.)
                let (first, start) = self.anchor.unwrap();
                self.anchor = Some((first, start.saturating_sub(excess)));
                self.resync = true;
            }
            self.window = 0;
            self.min_early = None;
        }
        Playout::Play {
            pts,
            discont,
            conceal: (!hole.is_zero()).then_some((last_pts + PACKET, hole)),
        }
    }

    fn restart(&mut self, sequence: u64, now: Duration) -> Playout {
        let pts = now + SLACK;
        *self = Self {
            anchor: Some((sequence, pts)),
            last: Some((sequence, pts)),
            ..Self::default()
        };
        Playout::Play {
            pts,
            discont: true,
            conceal: None,
        }
    }
}

pub async fn receive(mut track: moq_net::track::Subscriber, source: AppSrc) -> Result<()> {
    let mut sequence = None;
    let mut current: Option<moq_net::group::Consumer> = None;
    let mut timeline = Timeline::default();
    loop {
        match crate::media::next_media_event(&mut track, &mut current, sequence, 4096).await? {
            crate::media::MediaEvent::Group(next) => {
                sequence = Some(next.sequence);
                current = Some(next);
            }
            crate::media::MediaEvent::Frame(packet) => {
                current = None;
                let Ok(Some(bytes)) = packet else { continue };
                ensure!(
                    source.current_level_bytes() < 65536,
                    "audio decoder is not keeping up"
                );
                let mut buffer = gst::Buffer::from_slice(bytes);
                let buffer_mut = buffer.get_mut().unwrap();
                buffer_mut.set_duration(gst::ClockTime::from_mseconds(10));
                // Before the pipeline runs there is no clock: play at once.
                if let (Some(now), Some(sequence)) = (source.current_running_time(), sequence) {
                    match timeline.packet(sequence, Duration::from_nanos(now.nseconds())) {
                        Playout::Drop => continue,
                        Playout::Play {
                            pts,
                            discont,
                            conceal,
                        } => {
                            if let Some((at, duration)) = conceal {
                                // A GAP event (not a timestamp hole) makes opusdec run PLC.
                                source.send_event(
                                    gst::event::Gap::builder(clock_time(at))
                                        .duration(clock_time(duration))
                                        .build(),
                                );
                            }
                            buffer_mut.set_pts(clock_time(pts));
                            if discont {
                                buffer_mut.set_flags(gst::BufferFlags::DISCONT);
                            }
                        }
                    }
                }
                source.push_buffer(buffer)?;
            }
        }
    }
}

fn clock_time(duration: Duration) -> gst::ClockTime {
    gst::ClockTime::from_nseconds(duration.as_nanos() as u64)
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use anyhow::Context;

    fn ms(value: u64) -> Duration {
        Duration::from_millis(value)
    }

    #[test]
    fn steady_packets_play_back_to_back_and_holes_are_concealed() {
        let mut timeline = Timeline::default();
        assert_eq!(
            timeline.packet(7, ms(1000)),
            Playout::Play {
                pts: ms(1002),
                discont: true,
                conceal: None
            }
        );
        // Arrival jitter within the sink's buffering does not move playout.
        assert_eq!(
            timeline.packet(8, ms(1015)),
            Playout::Play {
                pts: ms(1012),
                discont: false,
                conceal: None
            }
        );
        // Packet 9 never came: its slot is concealed, 10 keeps its own.
        assert_eq!(
            timeline.packet(10, ms(1030)),
            Playout::Play {
                pts: ms(1032),
                discont: false,
                conceal: Some((ms(1022), ms(10)))
            }
        );
        assert_eq!(timeline.packet(10, ms(1031)), Playout::Drop);
        assert_eq!(timeline.packet(9, ms(1031)), Playout::Drop);
    }

    #[test]
    fn late_packets_move_playout_later_and_early_windows_move_it_back() {
        let mut timeline = Timeline::default();
        timeline.packet(0, ms(0));
        // 30 ms late: play it now (plus slack) and conceal the gap before it.
        assert_eq!(
            timeline.packet(1, ms(42)),
            Playout::Play {
                pts: ms(44),
                discont: true,
                conceal: Some((ms(12), ms(32)))
            }
        );
        // Then every packet arrives 12 ms early. Packet 1 (2 ms early) is in
        // the first window; after the second, playout moves 10 ms back.
        let mut now = ms(32);
        for sequence in 2..=2 * u64::from(WINDOW) {
            now += PACKET;
            assert!(matches!(
                timeline.packet(sequence, now),
                Playout::Play { discont: false, .. }
            ));
        }
        let next = 2 * u64::from(WINDOW) + 1;
        assert_eq!(
            timeline.packet(next, now + PACKET),
            Playout::Play {
                pts: now + PACKET + ms(2),
                discont: true,
                conceal: None
            }
        );
        // A long silence starts over.
        assert_eq!(
            timeline.packet(next + 30, now + ms(400)),
            Playout::Play {
                pts: now + ms(402),
                discont: true,
                conceal: None
            }
        );
    }

    #[test]
    fn moving_earlier_right_after_the_clock_started_does_not_underflow() {
        let mut timeline = Timeline::default();
        timeline.packet(0, ms(0));
        // A burst: the next packets all arrive 90 ms before their slots.
        for sequence in 1..=2 * u64::from(WINDOW) + 1 {
            let now = (PACKET * sequence as u32).saturating_sub(ms(90));
            assert!(matches!(
                timeline.packet(sequence, now),
                Playout::Play { .. }
            ));
        }
    }

    #[test]
    #[ignore = "requires GStreamer Opus plugins; run inside nix develop"]
    fn gap_events_are_concealed_by_opus_plc() -> Result<()> {
        gst::init()?;
        let encoder = Pipeline(gst::parse::launch(
            "audiotestsrc num-buffers=20 samplesperbuffer=480 ! audio/x-raw,rate=48000,channels=2 ! opusenc frame-size=10 audio-type=restricted-lowdelay ! appsink name=packets sync=false"
        )?.downcast::<gst::Pipeline>().unwrap());
        let packets = encoder
            .0
            .by_name("packets")
            .unwrap()
            .downcast::<AppSink>()
            .unwrap();
        let (decoder, source) = decoder_with_sink("appsink name=pcm sync=false")?;
        encoder.0.set_state(gst::State::Playing)?;
        let mut index = 0;
        while let Some(sample) = packets.try_pull_sample(gst::ClockTime::from_seconds(2)) {
            let buffer = sample.buffer().unwrap();
            if buffer.flags().contains(gst::BufferFlags::HEADER) {
                continue;
            }
            let pts = clock_time(PACKET * index);
            index += 1;
            if index == 6 {
                // As `receive` does for a missing sequence number.
                source.send_event(
                    gst::event::Gap::builder(pts)
                        .duration(clock_time(PACKET))
                        .build(),
                );
                continue;
            }
            let mut packet = gst::Buffer::from_slice(buffer.map_readable()?.as_slice().to_vec());
            let packet_mut = packet.get_mut().unwrap();
            packet_mut.set_pts(pts);
            packet_mut.set_duration(clock_time(PACKET));
            source.push_buffer(packet)?;
        }
        source.end_of_stream()?;
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        let decoded = || -> Result<gst::Structure> {
            Ok(decoder
                .0
                .by_name("opus")
                .context("opus decoder")?
                .property::<gst::Structure>("stats"))
        };
        while decoded()?.get::<u64>("num-pushed")? < u64::from(index) - 1 {
            ensure!(std::time::Instant::now() < deadline, "packets not decoded");
            std::thread::sleep(Duration::from_millis(10));
        }
        let stats = decoded()?;
        ensure!(
            stats.get::<u64>("plc-num-samples")? == 480,
            "lost packet not concealed: {stats}"
        );
        Ok(())
    }

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
