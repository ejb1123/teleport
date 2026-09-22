//! Native hardware decoder selection, proven with synthetic compressed output.
use super::{Pipeline, VideoFormat, compressed_caps, parser, validate_hdr_color, validate_hdr_raw};
use crate::protocol::VideoCodec;
use anyhow::{Context, Result, ensure};
use gst::prelude::*;
use gstreamer as gst;
use gstreamer_app::AppSink;
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{
        Mutex, OnceLock,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

type Cache = BTreeMap<(&'static str, bool, bool), &'static str>;
static SELECTED: OnceLock<Mutex<Cache>> = OnceLock::new();

fn hardware_candidates(codec: VideoCodec, macos: bool) -> &'static [&'static str] {
    if macos {
        return &["vtdec_hw"];
    }
    // VA covers both AMD and Intel. QSV is an additional Intel path when the
    // installed oneVPL runtime exposes a usable decoder. Every candidate must
    // pass the same CPU-download, precision and per-frame identity probe.
    match codec {
        VideoCodec::H264 => &["nvh264dec", "vah264dec", "qsvh264dec"],
        VideoCodec::H265 => &["nvh265dec", "vah265dec", "qsvh265dec"],
    }
}

pub(super) fn fragment(decoder: &str, codec: VideoCodec) -> String {
    let parser = parser(codec);
    let caps = compressed_caps(codec);
    let parsed_caps = if decoder == "vtdec_hw" {
        let format = match codec {
            VideoCodec::H264 => "avc",
            VideoCodec::H265 => "hvc1",
        };
        format!(" ! {caps},stream-format={format},alignment=au")
    } else {
        String::new()
    };
    let options = if decoder.starts_with("avdec_") {
        " max-threads=2"
    } else if decoder.starts_with("nvh") {
        " max-display-delay=0"
    } else {
        ""
    };
    // Bare video/x-raw requires CPU-mappable output instead of CUDA/VA/GL-only
    // memory. In GStreamer 1.26 the VA base decoder performs any required system
    // copy, and QSV's allocator_download_frame handles its VA download. Do not
    // insert vapostproc unconditionally: no scaling/tone mapping is wanted here,
    // and another transform must not silently reduce precision or lose identity.
    format!("{parser}{parsed_caps} ! {decoder} name=video_decoder{options} ! video/x-raw")
}

pub(super) fn select(force_software: bool, format: VideoFormat) -> Result<&'static str> {
    format.validate()?;
    let codec = format.codec;
    let mut cache = SELECTED.get_or_init(Mutex::default).lock().unwrap();
    let key = (codec.track(), force_software, format.hdr());
    if let Some(&selected) = cache.get(&key) {
        return Ok(selected);
    }
    let software = match codec {
        VideoCodec::H264 => "avdec_h264",
        VideoCodec::H265 => "avdec_h265",
    };
    let hardware = hardware_candidates(codec, cfg!(target_os = "macos"));
    if !force_software {
        for &candidate in hardware {
            if gst::ElementFactory::find(candidate).is_none() {
                continue;
            }
            match probe(candidate, format) {
                Ok(()) => {
                    tracing::info!(
                        candidate,
                        ?codec,
                        dynamic_range = ?format.dynamic_range,
                        "hardware decoder produced verified output"
                    );
                    cache.insert(key, candidate);
                    return Ok(candidate);
                }
                Err(error) => {
                    tracing::warn!(candidate, ?codec, %error, "hardware decoder probe failed; trying fallback")
                }
            }
        }
    }
    probe(software, format).with_context(|| {
        format!(
            "{software} cannot decode {}; check media plugins",
            codec.label()
        )
    })?;
    cache.insert(key, software);
    Ok(software)
}

fn synthetic_encoder(format: VideoFormat) -> &'static str {
    if format.hdr() {
        return "x265enc tune=zerolatency speed-preset=ultrafast option-string=pools=1:frame-threads=1:log-level=error ! video/x-h265,profile=main-10";
    }
    match format.codec {
        VideoCodec::H264 => {
            "x264enc tune=zerolatency speed-preset=ultrafast bframes=0 threads=2 ! video/x-h264,profile=constrained-baseline"
        }
        VideoCodec::H265 => {
            "x265enc tune=zerolatency speed-preset=ultrafast option-string=pools=1:frame-threads=1:log-level=error ! video/x-h265,profile=main"
        }
    }
}

pub(super) fn output_fragment(format: VideoFormat) -> &'static str {
    if format.hdr() {
        // Restrict precision and transfer before videoconvert: it must never
        // silently promote an 8-bit/SDR decoded buffer into the HDR output caps.
        "capsfilter name=hdr_decoded caps=\"video/x-raw,format=(string){P010_10LE,I420_10LE,AYUV64},colorimetry=bt2100-pq;video/x-raw,format=(string){ARGB64_BE,RGBA64_LE},colorimetry=1:1:14:7\" ! videoconvert ! video/x-raw,format=P010_10LE,colorimetry=bt2100-pq,chroma-site=jpeg,width=[2,7680],height=[2,8192]"
    } else {
        "videoconvert ! video/x-raw,format=RGB,width=[2,7680],height=[2,8192]"
    }
}

pub(super) fn verify_hdr_decoder_output(pipeline: &gst::Pipeline) -> Result<()> {
    let caps = pipeline
        .by_name("hdr_decoded")
        .context("missing HDR decoder output checkpoint")?
        .static_pad("src")
        .context("missing HDR decoder output pad")?
        .current_caps()
        .context("HDR decoder output not negotiated")?;
    let info = gstreamer_video::VideoInfo::from_caps(&caps)?;
    ensure!(
        info.format_info()
            .depth()
            .first()
            .is_some_and(|depth| *depth >= 10),
        "decoder reduced HDR precision before conversion"
    );
    validate_hdr_color(&info.colorimetry(), info.format_info().is_rgb())
}

pub(super) fn probe(decoder: &str, format: impl Into<VideoFormat>) -> Result<()> {
    let format = format.into();
    probe_inner(decoder, format, true)
}

fn probe_inner(decoder: &str, format: VideoFormat, require_identity: bool) -> Result<()> {
    const FRAMES: usize = 6;
    const FIRST_REFERENCE: u64 = 123_456_789;
    format.validate()?;
    let codec = format.codec;
    let raw = if format.hdr() {
        "I420_10LE,colorimetry=bt2100-pq"
    } else {
        "I420"
    };
    let pipeline = Pipeline(gst::parse::launch(&format!(
        "videotestsrc num-buffers={FRAMES} ! video/x-raw,format={raw},width=320,height=180,framerate=30/1 \
         ! {} ! identity name=stamp ! {} ! {} \
         ! appsink name=probe sync=false max-buffers={FRAMES} drop=false",
        synthetic_encoder(format), fragment(decoder, codec), output_fragment(format)
    ))?.downcast::<gst::Pipeline>().map_err(|_| anyhow::anyhow!("invalid decoder probe pipeline"))?);
    let sink = pipeline
        .0
        .by_name("probe")
        .context("missing decoder probe sink")?
        .downcast::<AppSink>()
        .map_err(|_| anyhow::anyhow!("invalid decoder probe sink"))?;
    let next_reference = AtomicU64::new(FIRST_REFERENCE);
    pipeline
        .0
        .by_name("stamp")
        .context("missing probe stamp")?
        .static_pad("src")
        .context("missing stamp pad")?
        .add_probe(gst::PadProbeType::BUFFER, move |_, info| {
            if let Some(buffer) = info.buffer_mut() {
                super::add_video_reference(
                    buffer.make_mut(),
                    next_reference.fetch_add(1, Ordering::Relaxed),
                );
            }
            gst::PadProbeReturn::Ok
        });
    pipeline.0.set_state(gst::State::Playing)?;
    let deadline = Instant::now() + Duration::from_secs(3);
    let mut references = BTreeSet::new();
    for _ in 0..FRAMES {
        let remaining = deadline.saturating_duration_since(Instant::now());
        let sample =
            sink.try_pull_sample(gst::ClockTime::from_nseconds(remaining.as_nanos() as u64));
        // A finite test may have already posted EOS after yielding valid frames.
        if let Some(message) = pipeline
            .0
            .bus()
            .context("missing bus")?
            .pop_filtered(&[gst::MessageType::Error])
            && let gst::MessageView::Error(error) = message.view()
        {
            anyhow::bail!("{} ({:?})", error.error(), error.debug());
        }
        let sample = sample.context("decoder produced no frame within 3 seconds")?;
        let info = gstreamer_video::VideoInfo::from_caps(
            sample.caps().context("decoder output has no caps")?,
        )?;
        if format.hdr() {
            verify_hdr_decoder_output(&pipeline.0)?;
            validate_hdr_raw(&info, false)?;
        } else {
            ensure!(
                info.format() == gstreamer_video::VideoFormat::Rgb,
                "decoder did not produce RGB"
            );
        }
        ensure!(
            info.width() == 320 && info.height() == 180,
            "unexpected decoded frame format"
        );
        let buffer = sample.buffer().context("decoder output has no buffer")?;
        if require_identity {
            let reference = super::video_reference(buffer)
                .context("decoder did not preserve per-frame identity metadata")?;
            ensure!(
                (FIRST_REFERENCE..FIRST_REFERENCE + FRAMES as u64).contains(&reference)
                    && references.insert(reference),
                "decoder altered or duplicated per-frame identity metadata"
            );
        }
        let map = buffer
            .map_readable()
            .context("cannot download decoded frame to CPU memory")?;
        ensure!(
            map.size() >= 320 * 180 * 3,
            "decoded CPU buffer is incomplete"
        );
        // Check the actual plane layout/strides as well as a contiguous byte count.
        // VA/QSV may expose driver-aligned surfaces through mappable VideoMeta.
        gstreamer_video::VideoFrameRef::from_buffer_ref_readable(buffer, &info)
            .context("decoded CPU frame layout is invalid")?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[ignore = "requires x265 and NVIDIA Main 10 decoding"]
    fn main10_decode_preserves_precision_and_color() -> Result<()> {
        gst::init()?;
        let format = VideoFormat {
            codec: VideoCodec::H265,
            dynamic_range: crate::protocol::DynamicRange::Hdr10,
        };
        probe("avdec_h265", format)?;
        if let Err(error) = probe_inner("nvh265dec", format, false) {
            eprintln!("SKIP NVIDIA Main 10: hardware output unavailable: {error:#}");
            return Ok(());
        }
        probe("nvh265dec", format)
    }
    #[test]
    fn videotoolbox_uses_parsed_codec_configuration() {
        assert!(fragment("vtdec_hw", VideoCodec::H264).contains("stream-format=avc"));
        assert!(fragment("vtdec_hw", VideoCodec::H265).contains("stream-format=hvc1"));
        assert!(fragment("nvh265dec", VideoCodec::H265).ends_with("video/x-raw"));
    }
    #[test]
    fn candidates_cover_amd_intel_without_changing_macos() {
        assert_eq!(
            hardware_candidates(VideoCodec::H264, false),
            &["nvh264dec", "vah264dec", "qsvh264dec"]
        );
        assert_eq!(
            hardware_candidates(VideoCodec::H265, false),
            &["nvh265dec", "vah265dec", "qsvh265dec"]
        );
        for codec in [VideoCodec::H264, VideoCodec::H265] {
            assert_eq!(hardware_candidates(codec, true), &["vtdec_hw"]);
        }
    }
    #[test]
    fn va_qsv_fragments_require_download_without_hdr_conversion() {
        for (codec, decoders) in [
            (VideoCodec::H264, ["vah264dec", "qsvh264dec"]),
            (VideoCodec::H265, ["vah265dec", "qsvh265dec"]),
        ] {
            for decoder in decoders {
                let pipeline = fragment(decoder, codec);
                assert_eq!(
                    pipeline,
                    format!(
                        "{} ! {decoder} name=video_decoder ! video/x-raw",
                        parser(codec)
                    )
                );
                assert!(!pipeline.contains("memory:"));
                assert!(!pipeline.contains("tone-mapping"));
            }
        }
    }
    #[test]
    #[ignore = "requires AMD/Intel VA-API or Intel QSV hardware and plugins"]
    fn va_qsv_decode_both_codecs_and_main10_when_available() -> Result<()> {
        gst::init()?;
        for (decoder, codec) in [
            ("vah264dec", VideoCodec::H264),
            ("vah265dec", VideoCodec::H265),
            ("qsvh264dec", VideoCodec::H264),
            ("qsvh265dec", VideoCodec::H265),
        ] {
            let mut formats = vec![VideoFormat::from(codec)];
            if codec == VideoCodec::H265 {
                formats.push(VideoFormat {
                    codec,
                    dynamic_range: crate::protocol::DynamicRange::Hdr10,
                });
            }
            for format in formats {
                // Availability/driver support is optional; once CPU output is
                // possible, dropped identity metadata is a regression, not SKIP.
                if let Err(error) = probe_inner(decoder, format, false) {
                    eprintln!(
                        "SKIP {decoder} {:?}: hardware output unavailable: {error:#}",
                        format.dynamic_range
                    );
                    continue;
                }
                probe(decoder, format)?;
            }
        }
        Ok(())
    }
    #[test]
    #[ignore = "requires installed software encode/decode plugins"]
    fn software_decodes_both_codecs() -> Result<()> {
        gst::init()?;
        probe("avdec_h264", VideoCodec::H264)?;
        probe("avdec_h265", VideoCodec::H265)
    }
    #[test]
    #[ignore = "requires NVIDIA hardware and nvcodec decoder plugins"]
    fn nvidia_decodes_both_codecs() -> Result<()> {
        gst::init()?;
        for (decoder, codec) in [
            ("nvh264dec", VideoCodec::H264),
            ("nvh265dec", VideoCodec::H265),
        ] {
            if let Err(error) = probe_inner(decoder, codec.into(), false) {
                eprintln!("SKIP {decoder}: hardware output unavailable: {error:#}");
                continue;
            }
            probe(decoder, codec)?;
        }
        Ok(())
    }
}
