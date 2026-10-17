//! Quality measurements for the encoder presets (parsec-comparison G10) and
//! static-scene refinement (G9): synthetic desktop text at 1440p, encoded
//! and decoded with the direct FFmpeg codecs, scored by luma PSNR and SSIM.
//! Measurements, not pass/fail checks: run them with `--ignored --nocapture`
//! (`TELEPORT_TEST_HARDWARE=1` adds NVENC and VA-API).

use super::{Codec, Config};
use crate::protocol::EncoderPreset;
use anyhow::Result;
use std::time::{Duration, Instant};

const WIDTH: usize = 2560;
const HEIGHT: usize = 1440;
const FPS: u32 = 60;
const KBPS: u32 = 20_000;

/// Desktop-like luma: dark random 5x7 "glyphs" in 8x16 cells on a light
/// background, in paragraphs, `rows` tall (taller than a frame, to scroll).
fn page(rows: usize) -> Vec<u8> {
    let hash = |mut value: u64| {
        value ^= value >> 33;
        value = value.wrapping_mul(0xff51_afd7_ed55_8ccd);
        value ^= value >> 33;
        value = value.wrapping_mul(0xc4ce_b9fe_1a85_ec53);
        value ^ (value >> 33)
    };
    let mut luma = vec![220u8; WIDTH * rows];
    for (y, row) in luma.chunks_exact_mut(WIDTH).enumerate() {
        let (line, gy) = (y / 16, y % 16);
        if line % 12 == 11 || hash(line as u64 * 7919) % 5 == 0 || !(4..11).contains(&gy) {
            continue;
        }
        for (x, pixel) in row.iter_mut().enumerate() {
            let (cell, gx) = (x / 8, x % 8);
            let glyph = hash((line as u64) << 20 | cell as u64);
            if glyph % 7 != 0
                && (1..6).contains(&gx)
                && (glyph >> ((gy - 4) * 5 + (gx - 1))) & 1 == 1
            {
                *pixel = 40;
            }
        }
    }
    luma
}

/// NV12 frame `scroll` rows down `page`, with neutral chroma.
fn frame(page: &[u8], scroll: usize) -> Vec<u8> {
    let mut frame = page[scroll * WIDTH..(scroll + HEIGHT) * WIDTH].to_vec();
    frame.resize(WIDTH * HEIGHT * 3 / 2, 128);
    frame
}

/// Luma PSNR (dB) and mean SSIM over 8x8 blocks.
fn luma_quality(reference: &[u8], decoded: &[u8]) -> (f64, f64) {
    let squared: f64 = reference[..WIDTH * HEIGHT]
        .iter()
        .zip(decoded)
        .map(|(a, b)| (f64::from(*a) - f64::from(*b)).powi(2))
        .sum();
    let mse = squared / (WIDTH * HEIGHT) as f64;
    let psnr = if mse == 0.0 {
        99.0
    } else {
        10.0 * (255.0 * 255.0 / mse).log10()
    };
    let (c1, c2) = ((0.01f64 * 255.0).powi(2), (0.03f64 * 255.0).powi(2));
    let (mut total, mut blocks) = (0f64, 0u32);
    for by in (0..HEIGHT - 7).step_by(8) {
        for bx in (0..WIDTH - 7).step_by(8) {
            let (mut sa, mut sb, mut saa, mut sbb, mut sab) = (0f64, 0f64, 0f64, 0f64, 0f64);
            for y in by..by + 8 {
                for x in bx..bx + 8 {
                    let (a, b) = (
                        f64::from(reference[y * WIDTH + x]),
                        f64::from(decoded[y * WIDTH + x]),
                    );
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

struct Pair {
    encoder: Codec,
    decoder: Codec,
}

struct Encoded {
    bytes: usize,
    encode: Duration,
    /// Decoded frames by pts: encoders may output a frame late.
    decoded: Vec<(u64, Vec<u8>)>,
}

impl Pair {
    fn open(name: &str, preset: EncoderPreset) -> Result<Self> {
        Self::open_at(name, preset, KBPS)
    }

    fn open_at(name: &str, preset: EncoderPreset, kbps: u32) -> Result<Self> {
        let hevc = name.starts_with("hevc") || name == "libx265";
        let config = Config {
            encoder: true,
            name: name.into(),
            hevc,
            hdr: false,
            width: WIDTH as u32,
            height: HEIGHT as u32,
            fps: FPS,
            kbps,
            output: 1,
            gop: FPS * 10,
            gpu_input: false,
            preset,
        };
        Ok(Self {
            encoder: Codec::new(config.clone())?,
            decoder: Codec::new(Config {
                encoder: false,
                name: "software".into(),
                ..config
            })?,
        })
    }

    /// Encodes one frame and decodes whatever the encoder put out.
    fn round_trip(&mut self, frame: &[u8], pts: u64) -> Result<Encoded> {
        let start = Instant::now();
        self.encoder.submit_frame(frame, pts, pts == 0)?;
        let mut packets = Vec::new();
        while let Some(packet) = self.encoder.receive()? {
            packets.push((packet.pts.unwrap_or(pts), packet.bytes));
        }
        let encode = start.elapsed();
        let mut decoded = Vec::new();
        for (pts, packet) in &packets {
            self.decoder.submit(packet, *pts)?;
            while let Some(frame) = self.decoder.receive()? {
                decoded.push((frame.pts.unwrap_or(*pts), frame.bytes));
            }
        }
        Ok(Encoded {
            bytes: packets.iter().map(|(_, packet)| packet.len()).sum(),
            encode,
            decoded,
        })
    }
}

fn encoders() -> Vec<&'static str> {
    let mut names = vec!["libx264"];
    if std::env::var("TELEPORT_TEST_HARDWARE").as_deref() == Ok("1") {
        names.extend(["h264_nvenc", "hevc_nvenc", "h264_vaapi", "hevc_vaapi"]);
    }
    if let Ok(only) = std::env::var("TELEPORT_TEST_ENCODERS") {
        names.retain(|name| only.split(',').any(|wanted| wanted == *name));
    }
    names
}

/// G10: encode time and quality per preset, 1440p60 at 20 Mbps, two
/// seconds of text scrolling 3 rows per frame (quality every 10th frame).
#[test]
#[ignore = "measurement; TELEPORT_TEST_HARDWARE=1 adds NVENC and VA-API"]
fn encoder_presets_time_and_quality() -> Result<()> {
    const FRAMES: usize = 120;
    let page = page(HEIGHT + FRAMES * 3);
    for name in encoders() {
        for preset in [
            EncoderPreset::Latency,
            EncoderPreset::Balanced,
            EncoderPreset::Quality,
        ] {
            let mut pair = match Pair::open(name, preset) {
                Ok(pair) => pair,
                Err(error) => {
                    eprintln!("PRESET {name} {preset:?}: unavailable ({error:#})");
                    continue;
                }
            };
            let (mut times, mut bytes, mut psnr, mut ssim, mut scored) =
                (Vec::new(), 0, 0.0, 0.0, 0);
            for index in 0..FRAMES {
                let encoded = pair.round_trip(&frame(&page, index * 3), index as u64)?;
                if index > 0 {
                    times.push(encoded.encode);
                    bytes += encoded.bytes;
                }
                for (pts, decoded) in encoded.decoded {
                    let pts = pts as usize;
                    if pts >= 10 && pts.is_multiple_of(10) {
                        let (p, s) = luma_quality(&frame(&page, pts * 3), &decoded);
                        psnr += p;
                        ssim += s;
                        scored += 1;
                    }
                }
            }
            times.sort();
            let mean = times.iter().sum::<Duration>() / times.len() as u32;
            eprintln!(
                "PRESET {name} {:>14}: encode mean {:.2} ms p95 {:.2} ms | PSNR {:.2} dB SSIM {:.4} ({scored} frames) | {:.1} Mbps",
                preset.label(),
                mean.as_secs_f64() * 1000.0,
                times[times.len() * 95 / 100].as_secs_f64() * 1000.0,
                psnr / f64::from(scored),
                ssim / f64::from(scored),
                bytes as f64 * 8.0 / (FRAMES - 1) as f64 * f64::from(FPS) / 1e6,
            );
        }
    }
    Ok(())
}

/// G9 baseline: one second of scrolling, then the still frame as the
/// capture keepalive repeats it (every 250 ms).
#[test]
#[ignore = "measurement; TELEPORT_TEST_HARDWARE=1 adds NVENC and VA-API"]
fn static_repeats_baseline() -> Result<()> {
    const MOVING: u64 = 60;
    let page = page(HEIGHT + MOVING as usize * 3);
    let still = frame(&page, (MOVING as usize - 1) * 3);
    for name in encoders() {
        let mut pair = Pair::open(name, EncoderPreset::Latency)?;
        let mut line = format!("REPEATS {name}:");
        let score = |decoded: Vec<(u64, Vec<u8>)>, line: &mut String, bytes: usize| {
            for (pts, decoded) in decoded {
                if pts + 1 == MOVING {
                    let (psnr, ssim) = luma_quality(&still, &decoded);
                    *line += &format!(" moving {psnr:.2} dB {ssim:.4}");
                } else if pts >= MOVING {
                    let (psnr, ssim) = luma_quality(&still, &decoded);
                    *line += &format!(" | {psnr:.2} dB {ssim:.4} {} kB", bytes / 1000);
                }
            }
        };
        for index in 0..MOVING {
            let encoded = pair.round_trip(&frame(&page, index as usize * 3), index)?;
            score(encoded.decoded, &mut line, encoded.bytes);
        }
        for repeat in 0..7 {
            let encoded = pair.round_trip(&still, MOVING + repeat * 15)?;
            score(encoded.decoded, &mut line, encoded.bytes);
        }
        eprintln!("{line}");
    }
    Ok(())
}
