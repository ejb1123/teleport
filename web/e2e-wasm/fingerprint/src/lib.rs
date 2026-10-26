//! Stand-in for `moq_native::tls::parse_fingerprint`: a SHA-256 certificate
//! fingerprint as 64 hex digits (surrounding whitespace ignored), like
//! moq-native 0.19.17's.
pub mod tls {
    pub fn parse_fingerprint(value: &str) -> anyhow::Result<[u8; 32]> {
        let value = value.trim();
        anyhow::ensure!(
            value.len() == 64 && value.bytes().all(|b| b.is_ascii_hexdigit()),
            "invalid fingerprint"
        );
        let mut out = [0u8; 32];
        for (byte, pair) in out.iter_mut().zip(value.as_bytes().chunks(2)) {
            *byte = u8::from_str_radix(std::str::from_utf8(pair)?, 16)?;
        }
        Ok(out)
    }
}
