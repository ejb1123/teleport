//! Direct libavcodec/libavutil/libswscale calls through a small opaque C ABI.
use anyhow::{Context, Result, ensure};
use std::{
    ffi::{CStr, CString, c_char, c_int, c_void},
    ptr::NonNull,
};

#[repr(C)]
#[derive(Default)]
struct Output {
    data: *const u8,
    size: c_int,
    width: c_int,
    height: c_int,
    key: c_int,
    pts: i64,
}
unsafe extern "C" {
    fn tp_codec_open(
        encoder: c_int,
        name: *const c_char,
        hevc: c_int,
        hdr: c_int,
        width: c_int,
        height: c_int,
        fps: c_int,
        kbps: c_int,
        output: c_int,
        error: *mut c_int,
    ) -> *mut c_void;
    fn tp_codec_free(codec: *mut c_void);
    fn tp_send_packet(codec: *mut c_void, data: *const u8, size: c_int, pts: i64) -> c_int;
    fn tp_send_frame(codec: *mut c_void, data: *const u8, size: c_int, pts: i64) -> c_int;
    fn tp_receive_packet(codec: *mut c_void, output: *mut Output) -> c_int;
    fn tp_receive_frame(codec: *mut c_void, output: *mut Output) -> c_int;
    fn tp_codec_flush(codec: *mut c_void);
    fn tp_codec_error(error: c_int, buffer: *mut c_char, length: c_int);
}

#[derive(Clone)]
pub struct Config {
    pub encoder: bool,
    pub name: String,
    pub hevc: bool,
    pub hdr: bool,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub kbps: u32,
    /// 0 = RGB24, 1 = NV12, 2 = P010LE.
    pub output: i32,
}
pub struct Codec {
    pointer: NonNull<c_void>,
    pub config: Config,
}
// The native instance has no thread affinity and is never concurrently entered:
// all operations require &mut self, and shared users hold a Mutex<Codec>.
unsafe impl Send for Codec {}
impl Drop for Codec {
    fn drop(&mut self) {
        unsafe {
            tp_codec_free(self.pointer.as_ptr());
        }
    }
}

fn check(code: i32) -> Result<i32> {
    if code >= 0 {
        return Ok(code);
    }
    let mut message = [0 as c_char; 256];
    unsafe {
        tp_codec_error(code, message.as_mut_ptr(), message.len() as i32);
    }
    anyhow::bail!(
        "FFmpeg: {} ({code})",
        unsafe { CStr::from_ptr(message.as_ptr()) }.to_string_lossy()
    )
}

pub struct Frame {
    pub bytes: Vec<u8>,
    pub width: u32,
    pub height: u32,
    pub key: bool,
    pub pts: Option<u64>,
}
impl Codec {
    pub fn new(config: Config) -> Result<Self> {
        let name = CString::new(config.name.as_str())?;
        let mut error = 0;
        let pointer = unsafe {
            tp_codec_open(
                config.encoder.into(),
                name.as_ptr(),
                config.hevc.into(),
                config.hdr.into(),
                config.width.try_into()?,
                config.height.try_into()?,
                config.fps.try_into()?,
                config.kbps.try_into()?,
                config.output,
                &mut error,
            )
        };
        check(error).with_context(|| format!("opening {}", config.name))?;
        Ok(Self {
            pointer: NonNull::new(pointer).context("FFmpeg returned no context")?,
            config,
        })
    }
    pub fn submit(&mut self, bytes: &[u8], pts: u64) -> Result<()> {
        let size = bytes.len().try_into()?;
        let result = unsafe {
            if self.config.encoder {
                tp_send_frame(self.pointer.as_ptr(), bytes.as_ptr(), size, pts.try_into()?)
            } else {
                tp_send_packet(self.pointer.as_ptr(), bytes.as_ptr(), size, pts.try_into()?)
            }
        };
        check(result)?;
        Ok(())
    }
    pub fn receive(&mut self) -> Result<Option<Frame>> {
        let mut output = Output::default();
        let result = unsafe {
            if self.config.encoder {
                tp_receive_packet(self.pointer.as_ptr(), &mut output)
            } else {
                tp_receive_frame(self.pointer.as_ptr(), &mut output)
            }
        };
        if check(result)? == 0 {
            return Ok(None);
        }
        ensure!(
            !output.data.is_null() && output.size > 0 && output.size <= 128 * 1024 * 1024,
            "invalid FFmpeg output allocation"
        );
        if !self.config.encoder {
            crate::protocol::validate_video_size(
                output.width.try_into()?,
                output.height.try_into()?,
            )?;
        }
        let bytes =
            unsafe { std::slice::from_raw_parts(output.data, output.size as usize) }.to_vec();
        Ok(Some(Frame {
            bytes,
            width: output.width as u32,
            height: output.height as u32,
            key: output.key != 0,
            pts: output.pts.try_into().ok(),
        }))
    }
    pub fn flush(&mut self) {
        unsafe {
            tp_codec_flush(self.pointer.as_ptr());
        }
    }
}

pub fn enabled() -> Result<bool> {
    match std::env::var("TELEPORT_VIDEO_BACKEND").as_deref() {
        Ok("ffmpeg") => Ok(true),
        Ok("gstreamer") => Ok(false),
        Err(std::env::VarError::NotPresent) => Ok(true),
        _ => anyhow::bail!("TELEPORT_VIDEO_BACKEND must be ffmpeg or gstreamer"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    pub(super) fn software_config(hevc: bool) -> Config {
        Config {
            encoder: true,
            name: if hevc { "libx265" } else { "libx264" }.into(),
            hevc,
            hdr: false,
            width: 64,
            height: 48,
            fps: 30,
            kbps: 1000,
            output: 1,
        }
    }
    #[test]
    fn direct_library_roundtrip_and_flush_both_codecs() -> Result<()> {
        for hevc in [false, true] {
            let config = software_config(hevc);
            let mut encoder = Codec::new(config.clone())?;
            let mut decoder = Codec::new(Config {
                encoder: false,
                name: "software".into(),
                ..config
            })?;
            let mut raw = vec![90; 64 * 48];
            raw.extend(vec![128; 64 * 48 / 2]);
            let mut decoded = 0;
            for sequence in 0..24 {
                encoder.submit(&raw, sequence)?;
                while let Some(packet) = encoder.receive()? {
                    if sequence == 14 {
                        decoder.flush();
                    }
                    decoder.submit(&packet.bytes, sequence + 100)?;
                    while let Some(frame) = decoder.receive()? {
                        ensure!((frame.width, frame.height) == (64, 48), "wrong dimensions");
                        ensure!(frame.bytes.len() == raw.len(), "wrong NV12 layout");
                        ensure!(frame.pts == Some(sequence + 100), "lost frame identity");
                        ensure!((frame.bytes[100] as i32 - 90).abs() < 12, "corrupt output");
                        decoded += 1;
                    }
                }
            }
            ensure!(decoded >= 20, "insufficient decoded frames: {decoded}");
        }
        Ok(())
    }
    #[test]
    fn invalid_configuration_and_short_input_are_rejected() -> Result<()> {
        let config = software_config(false);
        ensure!(
            Codec::new(Config {
                width: 0,
                ..config.clone()
            })
            .is_err()
        );
        ensure!(
            Codec::new(Config {
                name: "not_a_codec".into(),
                ..config.clone()
            })
            .is_err()
        );
        let mut encoder = Codec::new(config)?;
        ensure!(encoder.submit(&[1; 4], 0).is_err());
        Ok(())
    }

    #[test]
    fn hdr_ten_bit_roundtrip_and_sdr_rejection() -> Result<()> {
        let config = Config {
            hdr: true,
            output: 2,
            ..software_config(true)
        };
        let mut encoder = Codec::new(config.clone())?;
        let mut decoder = Codec::new(Config {
            encoder: false,
            name: "software".into(),
            ..config
        })?;
        let raw: Vec<u8> = (0..64 * 48 * 3 / 2)
            .flat_map(|index| {
                let value = if index < 64 * 48 { 257_u16 } else { 512_u16 };
                (value << 6).to_le_bytes()
            })
            .collect();
        encoder.submit(&raw, 0)?;
        let packet = encoder.receive()?.context("no HDR packet")?;
        decoder.submit(&packet.bytes, 123)?;
        let frame = decoder.receive()?.context("no HDR frame")?;
        ensure!(frame.bytes.len() == raw.len() && frame.pts == Some(123));
        let luma = u16::from_le_bytes([frame.bytes[0], frame.bytes[1]]) >> 6;
        ensure!(
            (luma as i32 - 257).abs() <= 4,
            "ten-bit luma was lost: {luma}"
        );
        let mut sdr_encoder = Codec::new(software_config(true))?;
        let mut sdr = vec![90; 64 * 48];
        sdr.extend(vec![128; 64 * 48 / 2]);
        sdr_encoder.submit(&sdr, 0)?;
        let sdr = sdr_encoder.receive()?.context("no SDR packet")?;
        decoder.flush();
        decoder.submit(&sdr.bytes, 124)?;
        ensure!(decoder.receive().is_err(), "SDR must not be mislabeled HDR");
        Ok(())
    }

    #[test]
    #[ignore = "requires hardware encoder and at least one hardware decoder"]
    fn direct_hardware_encode_decode_both_codecs() -> Result<()> {
        if std::env::var("TELEPORT_TEST_HARDWARE").as_deref() != Ok("1") {
            eprintln!("SKIP: set TELEPORT_TEST_HARDWARE=1 on a machine with hardware encoding");
            return Ok(());
        }
        let backend = std::env::var("TELEPORT_TEST_ENCODER").unwrap_or_else(|_| "nvenc".into());
        ensure!(matches!(backend.as_str(), "nvenc" | "vaapi" | "qsv"));
        for hevc in [false, true] {
            let config = Config {
                name: format!("{}_{backend}", if hevc { "hevc" } else { "h264" }),
                width: 640,
                height: 360,
                ..software_config(hevc)
            };
            let mut encoder = Codec::new(config.clone())?;
            let mut raw = vec![90; 640 * 360];
            raw.extend(vec![128; 640 * 360 / 2]);
            let mut packets = Vec::new();
            for index in 0..30 {
                encoder.submit(&raw, index)?;
                while let Some(packet) = encoder.receive()? {
                    packets.push(packet.bytes);
                }
            }
            ensure!(
                packets.len() == 30,
                "hardware encoder buffered too many frames: {} of 30 emitted",
                packets.len()
            );
            let mut passed = 0;
            for name in ["cuda", "vaapi", "qsv"] {
                let mut decoder = match Codec::new(Config {
                    encoder: false,
                    name: name.into(),
                    ..config.clone()
                }) {
                    Ok(decoder) => decoder,
                    Err(error) => {
                        eprintln!("SKIP {name}: {error:#}");
                        continue;
                    }
                };
                let mut count = 0;
                for (index, packet) in packets.iter().enumerate() {
                    decoder.submit(packet, index as u64 + 1)?;
                    while let Some(frame) = decoder.receive()? {
                        ensure!((frame.width, frame.height) == (640, 360));
                        ensure!((frame.bytes[0] as i32 - 90).abs() < 12, "bad hardware luma");
                        count += 1;
                    }
                }
                ensure!(
                    count >= 28,
                    "hardware decoder buffered too many frames: {count}"
                );
                eprintln!("PASS {} -> {name}: {count} frames", config.name);
                passed += 1;
            }
            ensure!(passed > 0, "no hardware decoder tested");
        }
        Ok(())
    }
}
