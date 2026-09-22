//! Bounded, synthetic HDR media diagnostics. Never opens a desktop capture.

use anyhow::{Context, Result, ensure};
use gst::prelude::*;
use gstreamer as gst;
use gstreamer_app::AppSink;
use gstreamer_video::{VideoColorPrimaries, VideoInfo, VideoTransferFunction};

struct ProbePipeline(gst::Pipeline);

impl Drop for ProbePipeline {
    fn drop(&mut self) {
        let _ = self.0.set_state(gst::State::Null);
    }
}

struct Encoder {
    name: &'static str,
    format: &'static str,
    settings: &'static str,
}

const ENCODERS: &[Encoder] = &[
    Encoder {
        name: "nvh265enc",
        format: "P010_10LE",
        settings: "bitrate=2000 gop-size=15 bframes=0 zerolatency=true",
    },
    Encoder {
        name: "vah265enc",
        format: "P010_10LE",
        settings: "bitrate=2000 key-int-max=15 b-frames=0",
    },
    Encoder {
        name: "x265enc",
        format: "I420_10LE",
        settings: "bitrate=2000 key-int-max=15 tune=zerolatency speed-preset=ultrafast option-string=pools=1:frame-threads=1:log-level=error",
    },
];

fn inspect_frame(sample: &gst::Sample) -> Result<String> {
    let caps = sample.caps().context("decoder did not provide caps")?;
    let info = VideoInfo::from_caps(caps)?;
    let format = info.format_info();
    let depth = format.depth().first().copied().unwrap_or(0);
    ensure!(depth >= 10, "decoder reduced precision to {depth} bits");
    ensure!(
        info.colorimetry().transfer() == VideoTransferFunction::Smpte2084,
        "decoder did not preserve PQ transfer: {}",
        info.colorimetry()
    );
    ensure!(
        info.colorimetry().primaries() == VideoColorPrimaries::Bt2020,
        "decoder did not preserve BT.2020 primaries: {}",
        info.colorimetry()
    );
    let buffer = sample
        .buffer()
        .context("decoder returned no frame buffer")?;
    ensure!(buffer.size() > 0, "decoder returned an empty frame");
    Ok(format!(
        "{}x{}, {:?}, {depth}-bit, {}",
        info.width(),
        info.height(),
        info.format(),
        info.colorimetry()
    ))
}

fn probe(encoder: &Encoder, decoder: &str) -> Result<String> {
    // Deliberately synthetic: marking this generated signal PQ says nothing
    // about the desktop's capture format or the client's display capability.
    let description = format!(
        "videotestsrc num-buffers=6 pattern=gradient \
         ! video/x-raw,format={},width=320,height=180,framerate=30/1,colorimetry=bt2100-pq \
         ! {} {} ! h265parse name=parsed \
         ! video/x-h265,profile=main-10 ! {} \
         ! appsink name=decoded sync=false max-buffers=6 drop=false",
        encoder.format, encoder.name, encoder.settings, decoder
    );
    let pipeline = ProbePipeline(
        gst::parse::launch(&description)?
            .downcast::<gst::Pipeline>()
            .map_err(|_| anyhow::anyhow!("probe did not create a pipeline"))?,
    );
    let sink = pipeline
        .0
        .by_name("decoded")
        .context("missing decoded sink")?
        .downcast::<AppSink>()
        .map_err(|_| anyhow::anyhow!("invalid decoded sink"))?;
    pipeline.0.set_state(gst::State::Playing)?;
    let sample = sink.try_pull_sample(gst::ClockTime::from_seconds(5));
    if let Some(error) = pipeline
        .0
        .bus()
        .context("missing probe bus")?
        .pop_filtered(&[gst::MessageType::Error])
        && let gst::MessageView::Error(error) = error.view()
    {
        anyhow::bail!("{} ({:?})", error.error(), error.debug());
    }
    let sample = sample.context("no decoded frame within 5 seconds")?;
    let caps = pipeline
        .0
        .by_name("parsed")
        .context("missing parser")?
        .static_pad("src")
        .context("missing parser output")?
        .current_caps()
        .context("encoder did not negotiate HEVC caps")?;
    let encoded = caps.structure(0).context("empty HEVC caps")?;
    ensure!(
        encoded.get::<&str>("profile")? == "main-10",
        "encoder did not produce HEVC Main 10"
    );
    inspect_frame(&sample)
}

/// Test real encode/decode output without capture, networking or display changes.
/// Missing optional hardware is diagnostic information, not a process failure.
pub fn doctor() -> Result<()> {
    gst::init()?;
    println!("HDR media diagnostics (synthetic, no desktop capture)");
    println!("GStreamer {}", gst::version_string());
    let decoder = if cfg!(target_os = "macos") && gst::ElementFactory::find("vtdec_hw").is_some() {
        "vtdec_hw"
    } else {
        "avdec_h265"
    };
    let mut passed = 0;
    for encoder in ENCODERS {
        if gst::ElementFactory::find(encoder.name).is_none() {
            println!("{}: unavailable (plugin/device)", encoder.name);
            continue;
        }
        if gst::ElementFactory::find(decoder).is_none() {
            println!("{}: untested; {decoder} unavailable", encoder.name);
            continue;
        }
        match probe(encoder, decoder) {
            Ok(description) => {
                passed += 1;
                println!("{} -> {decoder}: PASS ({description})", encoder.name);
            }
            Err(error) => println!("{} -> {decoder}: FAIL ({error:#})", encoder.name),
        }
    }
    println!("Verified Main 10/PQ codec paths: {passed}");
    println!("Desktop HDR capture: NOT TESTED (requires authorized source caps)");
    println!("End-to-end HDR: EXPERIMENTAL, requires patched KWin and Mac Metal/EDR");
    println!("Real desktop capture and physical HDR display: NOT VERIFIED by this test");
    println!("A codec PASS is not HDR desktop support; see docs/hdr.md.");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(format: &str, color: &str) -> gst::Sample {
        gst::init().unwrap();
        let caps = gst::Caps::builder("video/x-raw")
            .field("format", format)
            .field("width", 320_i32)
            .field("height", 180_i32)
            .field("framerate", gst::Fraction::new(30, 1))
            .field("colorimetry", color)
            .build();
        let buffer = gst::Buffer::with_size(320 * 180 * 4).unwrap();
        gst::Sample::builder().caps(&caps).buffer(&buffer).build()
    }

    #[test]
    fn precision_and_color_are_both_required() {
        assert!(inspect_frame(&sample("P010_10LE", "bt2100-pq")).is_ok());
        assert!(inspect_frame(&sample("NV12", "bt2100-pq")).is_err());
        assert!(inspect_frame(&sample("P010_10LE", "bt709")).is_err());
        assert!(inspect_frame(&sample("P010_10LE", "bt2100-hlg")).is_err());
    }

    #[test]
    #[ignore = "requires installed x265enc and avdec_h265 plugins"]
    fn software_main10_roundtrip() {
        gst::init().unwrap();
        let result = probe(&ENCODERS[2], "avdec_h265").unwrap();
        assert!(result.contains("10-bit"));
    }
}
