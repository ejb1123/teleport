//! Native hardware decoder selection, proven with synthetic compressed output.
use super::{Pipeline, VideoFormat, compressed_caps, parser, validate_hdr_color, validate_hdr_raw};
use crate::protocol::VideoCodec;
use anyhow::{Context, Result, ensure};
use gst::prelude::*;
use gstreamer as gst;
use gstreamer_app::AppSink;
use std::{
    collections::BTreeMap,
    sync::{Mutex, OnceLock},
};

type Cache = BTreeMap<(&'static str, bool, bool), &'static str>;
static SELECTED: OnceLock<Mutex<Cache>> = OnceLock::new();

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
    // Require system-memory output: CUDA/VA/GL textures cannot be mapped by our
    // CPU RGB upload path. Decoder negotiation must perform the download.
    format!("{parser}{parsed_caps} ! {decoder}{options} ! video/x-raw")
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
    let hardware: &[&str] = if cfg!(target_os = "macos") {
        &["vtdec_hw"]
    } else {
        match codec {
            VideoCodec::H264 => &["nvh264dec", "vah264dec"],
            VideoCodec::H265 => &["nvh265dec", "vah265dec"],
        }
    };
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
    format.validate()?;
    let codec = format.codec;
    let raw = if format.hdr() {
        "I420_10LE,colorimetry=bt2100-pq"
    } else {
        "I420"
    };
    let pipeline = Pipeline(gst::parse::launch(&format!(
        "videotestsrc num-buffers=6 ! video/x-raw,format={raw},width=320,height=180,framerate=30/1 \
         ! {} ! identity name=stamp ! {} ! {} \
         ! appsink name=probe sync=false max-buffers=6 drop=false",
        synthetic_encoder(format), fragment(decoder, codec), output_fragment(format)
    ))?.downcast::<gst::Pipeline>().map_err(|_| anyhow::anyhow!("invalid decoder probe pipeline"))?);
    let sink = pipeline
        .0
        .by_name("probe")
        .context("missing decoder probe sink")?
        .downcast::<AppSink>()
        .map_err(|_| anyhow::anyhow!("invalid decoder probe sink"))?;
    pipeline
        .0
        .by_name("stamp")
        .context("missing probe stamp")?
        .static_pad("src")
        .context("missing stamp pad")?
        .add_probe(gst::PadProbeType::BUFFER, |_, info| {
            if let Some(buffer) = info.buffer_mut() {
                super::add_video_reference(buffer.make_mut(), 123_456_789);
            }
            gst::PadProbeReturn::Ok
        });
    pipeline.0.set_state(gst::State::Playing)?;
    let sample = sink.try_pull_sample(gst::ClockTime::from_seconds(3));
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
    ensure!(
        !require_identity || super::video_reference(buffer) == Some(123_456_789),
        "decoder did not preserve per-frame identity metadata"
    );
    let map = buffer
        .map_readable()
        .context("cannot download decoded frame to CPU memory")?;
    ensure!(
        map.size() >= 320 * 180 * 3,
        "decoded RGB buffer is incomplete"
    );
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
