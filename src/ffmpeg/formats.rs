//! AV1, 4:4:4 and 10-bit through the direct FFmpeg codecs
//! (parsec-comparison G8): correctness tests, and measurements (`#[ignore]`,
//! run with `--ignored --nocapture`; `TELEPORT_TEST_HARDWARE=1` adds NVENC,
//! NVDEC and VA-API) of encode time, decode time and quality.

use super::{Codec, Config};
use crate::protocol::{self, EncoderPreset, VideoCodec};
use anyhow::{Context, Result, ensure};
use std::time::{Duration, Instant};

fn encoder_config(name: &str, codec: VideoCodec, width: usize, height: usize) -> Config {
    Config {
        encoder: true,
        name: name.into(),
        codec,
        hdr: false,
        chroma444: false,
        ten_bit: false,
        width: width as u32,
        height: height as u32,
        fps: 60,
        kbps: 2000,
        output: 1,
        gop: 600,
        gpu_input: false,
        preset: EncoderPreset::Latency,
        intra_refresh: 0,
    }
}

fn decoder(config: &Config, name: &str, output: i32) -> Result<Codec> {
    Codec::new(Config {
        encoder: false,
        name: name.into(),
        output,
        ..config.clone()
    })
}

/// Diagonal stripes moving with `frame`, limited-range NV12.
fn stripes(width: usize, height: usize, frame: usize) -> Vec<u8> {
    let mut raw: Vec<u8> = (0..width * height)
        .map(|i| (16 + ((i % width) + (i / width) * 2 + frame * 3) % 200) as u8)
        .collect();
    raw.extend(vec![128; width * height / 2]);
    raw
}

/// YUV444P with neutral luma and Cb alternating every pixel column: 4:2:0
/// cannot carry it (each chroma sample covers two columns).
pub(super) fn colour_columns(width: usize, height: usize) -> Vec<u8> {
    let mut raw = vec![126; width * height];
    raw.extend((0..width * height).map(|i| {
        if (i % width).is_multiple_of(2) {
            70
        } else {
            186
        }
    }));
    raw.extend(vec![128; width * height]);
    raw
}

/// Mean blue difference between neighbouring columns of an RGB24 frame,
/// over its middle row: ~0 when the columns were averaged (4:2:0).
pub(super) fn column_contrast(rgb: &[u8], width: usize, height: usize) -> f64 {
    let row = &rgb[height / 2 * width * 3..][..width * 3];
    let total: u32 = (8..width - 8)
        .map(|x| u32::from(row[x * 3 + 2].abs_diff(row[(x + 1) * 3 + 2])))
        .sum();
    f64::from(total) / (width - 16) as f64
}

/// SVT-AV1 into dav1d: every frame comes out at once (no encoder or decoder
/// delay), requested key frames start new groups, and the OBU parser that
/// starts MoQ groups agrees with the encoder's key flag.
#[test]
fn av1_software_roundtrip_keyframes_and_no_delay() -> Result<()> {
    let (width, height) = (320, 240);
    let config = encoder_config("libsvtav1", VideoCodec::Av1, width, height);
    let mut encoder = Codec::new(config.clone())?;
    let mut decoder = decoder(&config, "software", 1)?;
    let (mut keys, mut decoded) = (Vec::new(), 0);
    for frame in 0..12_usize {
        let raw = stripes(width, height, frame);
        encoder.submit_frame(&raw, frame as u64, frame == 6)?;
        let mut emitted = 0;
        while let Some(packet) = encoder.receive()? {
            emitted += 1;
            ensure!(
                packet.key == protocol::keyframe(VideoCodec::Av1, &packet.bytes),
                "key flag {} but OBU parse disagrees at {:?}",
                packet.key,
                packet.pts
            );
            if packet.key {
                keys.push(packet.pts);
            }
            decoder.submit(&packet.bytes, packet.pts.context("no pts")?)?;
            let mut out = 0;
            while let Some(image) = decoder.receive()? {
                ensure!((image.width, image.height) == (width as u32, height as u32));
                ensure!(image.pts == packet.pts, "lost frame identity");
                let error = image.bytes[..width * height]
                    .iter()
                    .zip(&raw)
                    .map(|(a, b)| u64::from(a.abs_diff(*b)))
                    .sum::<u64>()
                    / (width * height) as u64;
                ensure!(error < 12, "frame {frame}: mean luma error {error}");
                out += 1;
            }
            ensure!(out == 1, "dav1d held frame {frame}");
            decoded += 1;
        }
        ensure!(
            emitted == 1,
            "SVT-AV1 emitted {emitted} packets for frame {frame}"
        );
    }
    ensure!(keys == [Some(0), Some(6)], "key frames at {keys:?}");
    ensure!(decoded == 12);
    Ok(())
}

/// 4:4:4 keeps single-pixel colour detail through libx264 and libx265 (and
/// decodes with the ordinary software decoder); AV1 and HDR refuse it.
#[test]
fn chroma_444_keeps_single_pixel_colour() -> Result<()> {
    let (width, height) = (64, 48);
    let raw = colour_columns(width, height);
    for (name, codec) in [("libx264", VideoCodec::H264), ("libx265", VideoCodec::H265)] {
        let config = Config {
            chroma444: true,
            kbps: 8000,
            ..encoder_config(name, codec, width, height)
        };
        let mut encoder = Codec::new(config.clone())?;
        ensure!(encoder.submit(&raw[1..], 0).is_err(), "short 4:4:4 input");
        let mut decoder = decoder(&config, "software", 0)?;
        let mut last = None;
        for pts in 0..3 {
            encoder.submit(&raw, pts)?;
            while let Some(packet) = encoder.receive()? {
                decoder.submit(&packet.bytes, pts)?;
                while let Some(image) = decoder.receive()? {
                    last = Some(image);
                }
            }
        }
        let image = last.context("nothing decoded")?;
        let contrast = column_contrast(&image.bytes, width, height);
        ensure!(
            contrast > 100.0,
            "{name}: colour columns lost ({contrast:.1})"
        );
    }
    for config in [
        Config {
            chroma444: true,
            ..encoder_config("libsvtav1", VideoCodec::Av1, 64, 64)
        },
        Config {
            chroma444: true,
            hdr: true,
            ..encoder_config("libx265", VideoCodec::H265, 64, 64)
        },
    ] {
        ensure!(
            Codec::new(config.clone()).is_err(),
            "{} 4:4:4 accepted",
            config.name
        );
    }
    Ok(())
}

/// Main 10 from an 8-bit-precision-or-better source (10-bit SDR): x265 and
/// SVT-AV1 keep 10-bit precision with BT.709 tags, the P010 output keeps
/// it, the NV12 output (CPU presentation) is dithered down to 8 bits, and
/// H.264, HDR or 4:4:4 with it are refused.
#[test]
fn ten_bit_sdr_keeps_precision_and_dithers_to_eight_bits() -> Result<()> {
    let (width, height) = (256, 64);
    // Top half: a luma ramp at 10-bit precision (four code values per 8-bit
    // step); bottom half: flat at 258, half way between 8-bit 64 and 65.
    let luma: Vec<u16> = (0..width * height)
        .map(|i| {
            if i / width < height / 2 {
                256 + (i % width) as u16
            } else {
                258
            }
        })
        .collect();
    let raw: Vec<u8> = luma
        .iter()
        .copied()
        .chain(std::iter::repeat_n(512, width * height / 2))
        .flat_map(|value| (value << 6).to_le_bytes())
        .collect();
    for (name, codec) in [
        ("libx265", VideoCodec::H265),
        ("libsvtav1", VideoCodec::Av1),
    ] {
        let config = Config {
            ten_bit: true,
            kbps: 20_000,
            ..encoder_config(name, codec, width, height)
        };
        let mut encoder = Codec::new(config.clone())?;
        let mut packets = Vec::new();
        for pts in 0..4 {
            encoder.submit(&raw, pts)?;
            while let Some(packet) = encoder.receive()? {
                packets.push((pts, packet.bytes));
            }
        }
        let last = |output| -> Result<super::Frame> {
            let (frames, _) = decode_all(&config, &packets, "software", output)?;
            Ok(frames.into_iter().last().context("nothing decoded")?.1)
        };
        // P010: values between 8-bit steps survive.
        let ten = last(2)?;
        let row: Vec<u16> = ten.bytes[height / 4 * width * 2..][..width * 2]
            .chunks_exact(2)
            .map(|pair| u16::from_le_bytes([pair[0], pair[1]]) >> 6)
            .collect();
        let between = row.iter().filter(|&&value| value % 4 != 0).count();
        ensure!(
            between > width / 2,
            "{name}: 10-bit precision lost ({between} of {width})"
        );
        let error = row
            .iter()
            .zip(&luma[height / 4 * width..])
            .map(|(a, b)| u32::from(a.abs_diff(*b)))
            .max()
            .unwrap_or(0);
        ensure!(error <= 4, "{name}: 10-bit luma off by {error}");
        // NV12: the flat half step comes out as a 64/65 pattern averaging
        // 64.5, not as a flat 64 or 65.
        let eight = last(1)?;
        let row = &eight.bytes[height * 3 / 4 * width..][..width];
        let changes = row.windows(2).filter(|pair| pair[0] != pair[1]).count();
        let mean = row.iter().map(|&value| f64::from(value)).sum::<f64>() / width as f64;
        ensure!(
            changes > width / 4 && (mean - 64.5).abs() < 0.2,
            "{name}: 8-bit output not dithered ({changes} changes, mean {mean:.2})"
        );
    }
    for config in [
        Config {
            ten_bit: true,
            ..encoder_config("libx264", VideoCodec::H264, 64, 64)
        },
        Config {
            ten_bit: true,
            hdr: true,
            ..encoder_config("libx265", VideoCodec::H265, 64, 64)
        },
        Config {
            ten_bit: true,
            chroma444: true,
            ..encoder_config("libx265", VideoCodec::H265, 64, 64)
        },
    ] {
        ensure!(
            Codec::new(config).is_err(),
            "invalid 10-bit configuration accepted"
        );
    }
    Ok(())
}

/// AV1 decoders after a lost frame (what intra refresh would rely on, as
/// with H.264): dav1d, and NVDEC with `TELEPORT_TEST_HARDWARE=1`. Prints
/// how many later frames each decodes and their mean luma error.
#[test]
#[ignore = "measurement; TELEPORT_TEST_HARDWARE=1 adds NVDEC"]
fn av1_decoding_past_a_lost_frame() -> Result<()> {
    let (width, height) = (320, 240);
    let config = encoder_config("libsvtav1", VideoCodec::Av1, width, height);
    let mut encoder = Codec::new(config.clone())?;
    let mut packets = Vec::new();
    for frame in 0..30_usize {
        encoder.submit(&stripes(width, height, frame), frame as u64)?;
        while let Some(packet) = encoder.receive()? {
            packets.push((packet.pts.unwrap_or_default(), packet.bytes));
        }
    }
    let mut decoders = vec!["software"];
    if hardware() {
        decoders.push("cuda");
    }
    for name in decoders {
        let mut decoder = decoder(&config, name, 1)?;
        let (mut decoded, mut errors, mut error) = (0, 0, 0_u64);
        for (pts, packet) in &packets {
            if *pts == 10 {
                continue; // lost
            }
            if decoder.submit(packet, *pts).is_err() {
                errors += 1;
                continue;
            }
            while let Ok(Some(frame)) = decoder.receive() {
                let at = frame.pts.unwrap_or_default() as usize;
                if at > 10 {
                    decoded += 1;
                    error += frame.bytes[..width * height]
                        .iter()
                        .zip(&stripes(width, height, at))
                        .map(|(a, b)| u64::from(a.abs_diff(*b)))
                        .sum::<u64>()
                        / (width * height) as u64;
                }
            }
        }
        eprintln!(
            "AV1 LOSS {name}: {decoded} of 19 later frames decoded, {errors} errors, mean luma error {}",
            error / decoded.max(1)
        );
    }
    Ok(())
}

// ---- Measurements ----

fn load() -> String {
    std::fs::read_to_string("/proc/loadavg")
        .ok()
        .and_then(|line| {
            let mut fields = line.split_whitespace();
            Some(format!(
                "{} {} {}",
                fields.next()?,
                fields.next()?,
                fields.next()?
            ))
        })
        .unwrap_or_else(|| "?".into())
}

fn hardware() -> bool {
    std::env::var("TELEPORT_TEST_HARDWARE").as_deref() == Ok("1")
}

fn hash(mut value: u64) -> u64 {
    value ^= value >> 33;
    value = value.wrapping_mul(0xff51_afd7_ed55_8ccd);
    value ^= value >> 33;
    value = value.wrapping_mul(0xc4ce_b9fe_1a85_ec53);
    value ^ (value >> 33)
}

/// Desktop text as RGB: random 5x7 glyphs in 8x16 cells, paragraphs on a
/// light page and on dark panels, in a few colours (syntax highlighting)
/// or `grey`.
fn text_page(width: usize, rows: usize, grey: bool) -> Vec<[u8; 3]> {
    const INK: [[u8; 3]; 5] = [
        [30, 30, 30],
        [200, 30, 40],
        [20, 110, 200],
        [30, 150, 60],
        [150, 60, 170],
    ];
    const DARK_INK: [[u8; 3]; 4] = [
        [230, 230, 230],
        [250, 200, 60],
        [120, 220, 250],
        [250, 120, 160],
    ];
    let mut page = vec![[0_u8; 3]; width * rows];
    for (y, row) in page.chunks_exact_mut(width).enumerate() {
        let (line, gy) = (y / 16, y % 16);
        let dark = (line / 12) % 3 == 2;
        let background = if dark { [30, 32, 40] } else { [236, 236, 236] };
        row.fill(background);
        if line % 12 == 11 || hash(line as u64 * 7919).is_multiple_of(5) || !(4..11).contains(&gy) {
            continue;
        }
        for (x, pixel) in row.iter_mut().enumerate() {
            let (cell, gx) = (x / 8, x % 8);
            let glyph = hash((line as u64) << 20 | cell as u64);
            if !glyph.is_multiple_of(7)
                && (1..6).contains(&gx)
                && (glyph >> ((gy - 4) * 5 + (gx - 1))) & 1 == 1
            {
                let word = hash((line as u64) << 24 | (cell / 6) as u64) as usize;
                *pixel = if grey {
                    if dark { [230, 230, 230] } else { [30, 30, 30] }
                } else if dark {
                    DARK_INK[word % DARK_INK.len()]
                } else {
                    INK[word % INK.len()]
                };
            }
        }
    }
    page
}

/// Limited-range BT.709 (the matrix every path here uses).
fn rgb_to_ycbcr([r, g, b]: [u8; 3]) -> [f64; 3] {
    let (r, g, b) = (
        f64::from(r) / 255.0,
        f64::from(g) / 255.0,
        f64::from(b) / 255.0,
    );
    let y = 0.2126 * r + 0.7152 * g + 0.0722 * b;
    [
        16.0 + 219.0 * y,
        128.0 + 224.0 * (b - y) / 1.8556,
        128.0 + 224.0 * (r - y) / 1.5748,
    ]
}

/// `rows` of `page` from `scroll` as YUV444P (`chroma444`) or NV12 (2x2
/// averaged chroma).
fn yuv(page: &[[u8; 3]], width: usize, height: usize, scroll: usize, chroma444: bool) -> Vec<u8> {
    let pixels = &page[scroll * width..(scroll + height) * width];
    let ycbcr: Vec<[f64; 3]> = pixels.iter().map(|&pixel| rgb_to_ycbcr(pixel)).collect();
    let round = |value: f64| value.round().clamp(0.0, 255.0) as u8;
    let mut raw: Vec<u8> = ycbcr.iter().map(|pixel| round(pixel[0])).collect();
    if chroma444 {
        raw.extend(ycbcr.iter().map(|pixel| round(pixel[1])));
        raw.extend(ycbcr.iter().map(|pixel| round(pixel[2])));
    } else {
        for y in (0..height).step_by(2) {
            for x in (0..width).step_by(2) {
                let block = [
                    ycbcr[y * width + x],
                    ycbcr[y * width + x + 1],
                    ycbcr[(y + 1) * width + x],
                    ycbcr[(y + 1) * width + x + 1],
                ];
                for channel in [1, 2] {
                    raw.push(round(
                        block.iter().map(|pixel| pixel[channel]).sum::<f64>() / 4.0,
                    ));
                }
            }
        }
    }
    raw
}

/// PSNR (dB) and mean SSIM over 8x8 blocks of one 8-bit plane.
fn plane_quality(
    reference: &[u8],
    decoded: &[u8],
    width: usize,
    height: usize,
    step: usize,
) -> (f64, f64) {
    let at = |plane: &[u8], x: usize, y: usize| f64::from(plane[(y * width + x) * step]);
    let mut squared = 0.0;
    for y in 0..height {
        for x in 0..width {
            squared += (at(reference, x, y) - at(decoded, x, y)).powi(2);
        }
    }
    let mse = squared / (width * height) as f64;
    let psnr = if mse == 0.0 {
        99.0
    } else {
        10.0 * (255.0 * 255.0 / mse).log10()
    };
    let (c1, c2) = ((0.01f64 * 255.0).powi(2), (0.03f64 * 255.0).powi(2));
    let (mut total, mut blocks) = (0f64, 0u32);
    for by in (0..height - 7).step_by(8) {
        for bx in (0..width - 7).step_by(8) {
            let (mut sa, mut sb, mut saa, mut sbb, mut sab) = (0f64, 0f64, 0f64, 0f64, 0f64);
            for y in by..by + 8 {
                for x in bx..bx + 8 {
                    let (a, b) = (at(reference, x, y), at(decoded, x, y));
                    sa += a;
                    sb += b;
                    saa += a * a;
                    sbb += b * b;
                    sab += a * b;
                }
            }
            let (ma, mb) = (sa / 64.0, sb / 64.0);
            let va = (saa - 64.0 * ma * ma) / 63.0;
            let vb = (sbb - 64.0 * mb * mb) / 63.0;
            let cov = (sab - 64.0 * ma * mb) / 63.0;
            total += ((2.0 * ma * mb + c1) * (2.0 * cov + c2))
                / ((ma * ma + mb * mb + c1) * (va + vb + c2));
            blocks += 1;
        }
    }
    (psnr, total / f64::from(blocks))
}

/// RGB PSNR and SSIM (means over the three channels) of an RGB24 decode.
fn rgb_quality(reference: &[[u8; 3]], decoded: &[u8], width: usize, height: usize) -> (f64, f64) {
    let flat: Vec<u8> = reference.iter().flatten().copied().collect();
    let (mut psnr, mut ssim) = (0.0, 0.0);
    for channel in 0..3 {
        let (p, s) = plane_quality(&flat[channel..], &decoded[channel..], width, height, 3);
        psnr += p / 3.0;
        ssim += s / 3.0;
    }
    (psnr, ssim)
}

fn stats(times: &mut [Duration]) -> (f64, f64) {
    if times.is_empty() {
        return (0.0, 0.0);
    }
    times.sort();
    let mean = times.iter().sum::<Duration>() / times.len() as u32;
    (
        mean.as_secs_f64() * 1000.0,
        times[times.len() * 95 / 100].as_secs_f64() * 1000.0,
    )
}

/// Encodes `frames`, recording encode time per frame; returns the packets
/// (pts, bytes).
fn encode_all(
    encoder: &mut Codec,
    frames: &[Vec<u8>],
    times: &mut Vec<Duration>,
) -> Result<Vec<(u64, Vec<u8>)>> {
    let mut packets = Vec::new();
    for (index, frame) in frames.iter().enumerate() {
        let start = Instant::now();
        encoder.submit_frame(frame, index as u64, index == 0)?;
        while let Some(packet) = encoder.receive()? {
            packets.push((packet.pts.unwrap_or(index as u64), packet.bytes));
        }
        if index > 0 {
            times.push(start.elapsed());
        }
    }
    Ok(packets)
}

/// Decodes `packets` with `name`, timing each packet (submit to frame out,
/// download and conversion to `output` included).
/// Decoded frames by pts, and the time each packet took.
type Decoded = (Vec<(u64, super::Frame)>, Vec<Duration>);

fn decode_all(
    config: &Config,
    packets: &[(u64, Vec<u8>)],
    name: &str,
    output: i32,
) -> Result<Decoded> {
    let mut decoder = decoder(config, name, output)?;
    let (mut frames, mut times) = (Vec::new(), Vec::new());
    for (pts, packet) in packets {
        let start = Instant::now();
        decoder.submit(packet, *pts)?;
        while let Some(frame) = decoder.receive()? {
            frames.push((frame.pts.unwrap_or(*pts), frame));
        }
        times.push(start.elapsed());
    }
    Ok((frames, times))
}

/// G8 / AV1: SVT-AV1's encode time and quality at 720p and 1080p (60 FPS,
/// 5 and 10 Mbps CBR, two seconds of grey text scrolling 2 rows per frame)
/// next to x264/x265 and, with `TELEPORT_TEST_HARDWARE=1`, NVENC and
/// hardware AV1 encoders; then each stream's decode time in software (dav1d
/// for AV1) and on NVDEC (CPU NV12 out, and GPU frames as zero-copy
/// presentation takes them). `TELEPORT_TEST_ENCODERS` limits the encoders.
#[test]
#[ignore = "measurement; TELEPORT_TEST_HARDWARE=1 adds NVENC and NVDEC"]
fn av1_encode_decode_time_and_quality() -> Result<()> {
    const FRAMES: usize = 120;
    let mut encoders = vec![
        ("libsvtav1", VideoCodec::Av1, EncoderPreset::Latency),
        ("libsvtav1", VideoCodec::Av1, EncoderPreset::Balanced),
        ("libsvtav1", VideoCodec::Av1, EncoderPreset::Quality),
        ("libx264", VideoCodec::H264, EncoderPreset::Latency),
        ("libx265", VideoCodec::H265, EncoderPreset::Latency),
    ];
    if hardware() {
        encoders.extend([
            ("h264_nvenc", VideoCodec::H264, EncoderPreset::Latency),
            ("hevc_nvenc", VideoCodec::H265, EncoderPreset::Latency),
            ("av1_nvenc", VideoCodec::Av1, EncoderPreset::Latency),
            ("av1_vaapi", VideoCodec::Av1, EncoderPreset::Latency),
        ]);
    }
    if let Ok(only) = std::env::var("TELEPORT_TEST_ENCODERS") {
        encoders.retain(|(name, ..)| only.split(',').any(|wanted| wanted == *name));
    }
    for (width, height, kbps) in [(1280_usize, 720_usize, 5000), (1920, 1080, 10_000)] {
        let page = text_page(width, height + FRAMES * 2, true);
        let frames: Vec<Vec<u8>> = (0..FRAMES)
            .map(|index| yuv(&page, width, height, index * 2, false))
            .collect();
        for &(name, codec, preset) in &encoders {
            let config = Config {
                kbps,
                preset,
                ..encoder_config(name, codec, width, height)
            };
            let mut encoder = match Codec::new(config.clone()) {
                Ok(encoder) => encoder,
                Err(error) => {
                    eprintln!("AV1 {name} {width}x{height}: unavailable ({error:#})");
                    continue;
                }
            };
            let load_before = load();
            let mut times = Vec::new();
            let packets = encode_all(&mut encoder, &frames, &mut times)?;
            drop(encoder);
            let (encode_mean, encode_p95) = stats(&mut times);
            let bytes: usize = packets.iter().skip(1).map(|(_, packet)| packet.len()).sum();
            let (decoded, mut software) = decode_all(&config, &packets, "software", 1)?;
            let (mut psnr, mut ssim, mut scored) = (0.0, 0.0, 0);
            for (pts, frame) in &decoded {
                let pts = *pts as usize;
                if pts >= 10 && pts.is_multiple_of(10) {
                    let (p, s) = plane_quality(&frames[pts], &frame.bytes, width, height, 1);
                    psnr += p;
                    ssim += s;
                    scored += 1;
                }
            }
            let (software_mean, software_p95) = stats(&mut software);
            let mut line = format!(
                "AV1 {name} {:>8} {width}x{height} {kbps} kbps: encode {encode_mean:.2} / {encode_p95:.2} ms | PSNR {:.2} dB SSIM {:.4} | {:.1} Mbps | decode software {software_mean:.2} / {software_p95:.2} ms",
                preset.label(),
                psnr / f64::from(scored.max(1)),
                ssim / f64::from(scored.max(1)),
                bytes as f64 * 8.0 / (FRAMES - 1) as f64 * 60.0 / 1e6,
            );
            if hardware() {
                for (output, label) in [(1, "NV12"), (3, "GPU frame")] {
                    match decode_all(&config, &packets, "cuda", output) {
                        Ok((frames, mut times)) => {
                            ensure!(frames.len() >= FRAMES - 1, "NVDEC held frames");
                            let (mean, p95) = stats(&mut times);
                            line += &format!(" | NVDEC {label} {mean:.2} / {p95:.2} ms");
                        }
                        Err(error) => line += &format!(" | NVDEC {label}: {error:#}"),
                    }
                }
            }
            eprintln!("{line} | load {load_before} -> {}", load());
        }
    }
    Ok(())
}

/// G8 / 4:4:4: coloured desktop text (dark and light panels, five ink
/// colours) at 1920x1080 60 FPS, the same bitrate as 4:2:0 and 4:4:4, scored
/// in RGB against the source (PSNR and SSIM averaged over R, G, B, every
/// 10th frame of two seconds scrolling 2 rows per frame, then the still
/// frame). x264/x265, and NVENC with `TELEPORT_TEST_HARDWARE=1` (decoded by
/// NVDEC where it can, else in software; decode times printed).
#[test]
#[ignore = "measurement; TELEPORT_TEST_HARDWARE=1 adds NVENC and NVDEC"]
fn chroma_444_text_sharpness() -> Result<()> {
    const FRAMES: usize = 120;
    let (width, height) = (1920_usize, 1080_usize);
    let page = text_page(width, height + FRAMES * 2, false);
    let mut encoders = vec![("libx264", VideoCodec::H264), ("libx265", VideoCodec::H265)];
    if hardware() {
        encoders.extend([
            ("h264_nvenc", VideoCodec::H264),
            ("hevc_nvenc", VideoCodec::H265),
        ]);
    }
    if let Ok(only) = std::env::var("TELEPORT_TEST_ENCODERS") {
        encoders.retain(|(name, _)| only.split(',').any(|wanted| wanted == *name));
    }
    // Two seconds scrolling, then half a second still; 4:2:0 and 4:4:4.
    let streams = [false, true].map(|chroma444| -> Vec<Vec<u8>> {
        (0..FRAMES)
            .map(|index| yuv(&page, width, height, index * 2, chroma444))
            .chain(std::iter::repeat_n(
                yuv(&page, width, height, (FRAMES - 1) * 2, chroma444),
                30,
            ))
            .collect()
    });
    for kbps in [8000, 20_000] {
        for &(name, codec) in &encoders {
            for chroma444 in [false, true] {
                let frames = &streams[usize::from(chroma444)];
                let config = Config {
                    kbps,
                    chroma444,
                    ..encoder_config(name, codec, width, height)
                };
                let mut encoder = match Codec::new(config.clone()) {
                    Ok(encoder) => encoder,
                    Err(error) => {
                        eprintln!("CHROMA {name} 4:4:4={chroma444}: unavailable ({error:#})");
                        continue;
                    }
                };
                let load_before = load();
                let mut times = Vec::new();
                let packets = encode_all(&mut encoder, frames, &mut times)?;
                drop(encoder);
                let (encode_mean, _) = stats(&mut times);
                let bytes: usize = packets.iter().skip(1).map(|(_, packet)| packet.len()).sum();
                let hardware_decoder = hardware() && (codec == VideoCodec::H265 || !chroma444);
                let name_decoder = if hardware_decoder { "cuda" } else { "software" };
                let (decoded, mut decode_times) = decode_all(&config, &packets, name_decoder, 0)?;
                let (decode_mean, _) = stats(&mut decode_times);
                let (mut psnr, mut ssim, mut scored) = (0.0, 0.0, 0);
                let mut still = (0.0, 0.0);
                for (pts, frame) in &decoded {
                    let pts = *pts as usize;
                    let scroll = pts.min(FRAMES - 1) * 2;
                    let reference = &page[scroll * width..(scroll + height) * width];
                    if (10..FRAMES).contains(&pts) && pts.is_multiple_of(10) {
                        let (p, s) = rgb_quality(reference, &frame.bytes, width, height);
                        psnr += p;
                        ssim += s;
                        scored += 1;
                    }
                    if pts == frames.len() - 1 {
                        still = rgb_quality(reference, &frame.bytes, width, height);
                    }
                }
                eprintln!(
                    "CHROMA {name} {} {kbps} kbps: moving RGB PSNR {:.2} dB SSIM {:.4} | still {:.2} dB {:.4} | {:.1} Mbps | encode {encode_mean:.2} ms | decode ({name_decoder}, RGB) {decode_mean:.2} ms | load {load_before}",
                    if chroma444 { "4:4:4" } else { "4:2:0" },
                    psnr / f64::from(scored.max(1)),
                    ssim / f64::from(scored.max(1)),
                    still.0,
                    still.1,
                    bytes as f64 * 8.0 / (frames.len() - 1) as f64 * 60.0 / 1e6,
                );
            }
        }
    }
    Ok(())
}

/// G8 / 10-bit SDR: does encoding an 8-bit source as Main 10 reduce banding?
/// Two 1920x1080 sources: a dark luma ramp (16..48 across the width, one
/// code value per 60 columns) and the same ramp at 10-bit precision
/// dithered to 8 bits (what a renderer's dithering produces), each still
/// and slowly panning. Encoded 8-bit (NV12) and 10-bit SDR (P010 = 8-bit
/// << 2) at 2 and 6 Mbps, decoded and brought back to 8 bits as the client
/// shows it: rounded (zero-copy GL presentation) and dithered by swscale
/// (CPU presentation, NV12).
/// Banding: RMS error of the 16x16 box-filtered luma against the source's
/// (in 8-bit code values; contours show up there, dither noise does not),
/// the mean run of equal values along rows (long runs: visible bands), and
/// plain PSNR. `TELEPORT_TEST_ENCODERS` limits the encoders.
#[test]
#[ignore = "measurement; TELEPORT_TEST_HARDWARE=1 adds NVENC"]
fn ten_bit_sdr_banding() -> Result<()> {
    const FRAMES: usize = 60;
    let (width, height) = (1920_usize, 1080_usize);
    let ramp = |x: usize, y: usize, shift: usize, dither: bool| -> u8 {
        let precise =
            16.0 + 32.0 * ((x + shift) % width) as f64 / width as f64 + y as f64 / height as f64;
        if dither {
            let noise = (hash((y * width + x) as u64 ^ 0x5bd1) % 1024) as f64 / 1024.0;
            (precise + noise - 0.5).round() as u8
        } else {
            precise.floor() as u8
        }
    };
    let mut encoders = vec!["libx265"];
    if hardware() {
        encoders.push("hevc_nvenc");
    }
    if let Ok(only) = std::env::var("TELEPORT_TEST_ENCODERS") {
        encoders.retain(|name| only.split(',').any(|wanted| wanted == *name));
    }
    let box_filter = |plane: &[u8]| -> Vec<f64> {
        let mut out = Vec::new();
        for by in (0..height - 15).step_by(16) {
            for bx in (0..width - 15).step_by(16) {
                let mut sum = 0u32;
                for y in by..by + 16 {
                    for x in bx..bx + 16 {
                        sum += u32::from(plane[y * width + x]);
                    }
                }
                out.push(f64::from(sum) / 256.0);
            }
        }
        out
    };
    for dither in [false, true] {
        // Still for half the frames, then panning a column per frame.
        let sources: Vec<Vec<u8>> = (0..FRAMES)
            .map(|frame| {
                let shift = frame.saturating_sub(FRAMES / 2);
                (0..width * height)
                    .map(|i| ramp(i % width, i / width, shift, dither))
                    .collect()
            })
            .collect();
        for kbps in [2000, 6000] {
            for name in &encoders {
                for ten_bit in [false, true] {
                    let input: Vec<Vec<u8>> = sources
                        .iter()
                        .map(|luma| {
                            if ten_bit {
                                luma.iter()
                                    .chain(std::iter::repeat_n(&128, width * height / 2))
                                    .flat_map(|&value| (u16::from(value) << 8).to_le_bytes())
                                    .collect()
                            } else {
                                let mut raw = luma.clone();
                                raw.resize(width * height * 3 / 2, 128);
                                raw
                            }
                        })
                        .collect();
                    let config = Config {
                        kbps,
                        ten_bit,
                        output: if ten_bit { 2 } else { 1 },
                        ..encoder_config(name, VideoCodec::H265, width, height)
                    };
                    let mut encoder = match Codec::new(config.clone()) {
                        Ok(encoder) => encoder,
                        Err(error) => {
                            eprintln!("BANDING {name} 10-bit={ten_bit}: unavailable ({error:#})");
                            continue;
                        }
                    };
                    let mut times = Vec::new();
                    let packets = encode_all(&mut encoder, &input, &mut times)?;
                    drop(encoder);
                    let (encode_mean, _) = stats(&mut times);
                    let (decoded, _) = decode_all(&config, &packets, "software", config.output)?;
                    let dithered = if ten_bit {
                        Some(decode_all(&config, &packets, "software", 1)?.0)
                    } else {
                        None
                    };
                    let mut line = format!(
                        "BANDING {} {kbps} kbps {name} {}:",
                        if dither { "dithered" } else { "stepped" },
                        if ten_bit { "Main 10" } else { "Main" }
                    );
                    let find = |frames: &[(u64, super::Frame)], pts: usize| -> Result<Vec<u8>> {
                        Ok(frames
                            .iter()
                            .find(|(at, _)| *at as usize == pts)
                            .context("frame not decoded")?
                            .1
                            .bytes
                            .clone())
                    };
                    for (label, pts) in [("still", FRAMES / 2 - 1), ("panning", FRAMES - 1)] {
                        let frame = find(&decoded, pts)?;
                        let variants = if ten_bit {
                            let rounded = frame[..width * height * 2]
                                .chunks_exact(2)
                                .map(|pair| {
                                    let value = u16::from_le_bytes([pair[0], pair[1]]) >> 6;
                                    ((value + 2) >> 2).min(255) as u8
                                })
                                .collect();
                            let dithered =
                                find(dithered.as_deref().context("no NV12 decode")?, pts)?;
                            vec![
                                ("rounded", rounded),
                                ("dithered", dithered[..width * height].to_vec()),
                            ]
                        } else {
                            vec![("", frame[..width * height].to_vec())]
                        };
                        for (variant, luma) in variants {
                            let (psnr, _) = plane_quality(&sources[pts], &luma, width, height, 1);
                            let (reference, smooth) =
                                (box_filter(&sources[pts]), box_filter(&luma));
                            let banding = (reference
                                .iter()
                                .zip(&smooth)
                                .map(|(a, b)| (a - b).powi(2))
                                .sum::<f64>()
                                / reference.len() as f64)
                                .sqrt();
                            // Visible contours: how long runs of one code value
                            // get along a row (dither keeps them short).
                            let runs = |plane: &[u8]| {
                                let changes = plane
                                    .chunks_exact(width)
                                    .map(|row| {
                                        row.windows(2).filter(|pair| pair[0] != pair[1]).count()
                                    })
                                    .sum::<usize>();
                                (width * height) as f64 / (changes + height) as f64
                            };
                            line += &format!(
                                " {label} {variant} PSNR {psnr:.2} dB, box RMSE {banding:.3}, runs {:.1} px (source {:.1}) |",
                                runs(&luma),
                                runs(&sources[pts])
                            );
                        }
                    }
                    eprintln!(
                        "{line} encode {encode_mean:.2} ms | {:.2} Mbps | load {}",
                        packets
                            .iter()
                            .skip(1)
                            .map(|(_, packet)| packet.len())
                            .sum::<usize>() as f64
                            * 8.0
                            / (FRAMES - 1) as f64
                            * 60.0
                            / 1e6,
                        load()
                    );
                }
            }
        }
    }
    Ok(())
}
