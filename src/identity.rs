//! Persistent host identity is opt-in. Never silently replace existing credentials.
use crate::protocol::Pairing;
use anyhow::{Context, Result, ensure};
use base64::Engine;
use std::{
    fs,
    io::Write,
    os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt},
    path::{Path, PathBuf},
};

fn pem(label: &str, der: &[u8]) -> String {
    let encoded = base64::engine::general_purpose::STANDARD.encode(der);
    let mut result = format!("-----BEGIN {label}-----\n");
    for chunk in encoded.as_bytes().chunks(64) {
        result.push_str(std::str::from_utf8(chunk).unwrap());
        result.push('\n');
    }
    result.push_str(&format!("-----END {label}-----\n"));
    result
}

pub struct Identity {
    pub certificate: PathBuf,
    pub key: PathBuf,
    pub pairing: PathBuf,
    pub token: String,
}

pub fn private_file(path: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    // geteuid takes no pointers and cannot fail.
    ensure!(
        metadata.uid() == unsafe { libc::geteuid() },
        "identity must be owned by the current user"
    );
    ensure!(
        metadata.is_file() && metadata.mode() & 0o077 == 0,
        "{} must be a regular private file (chmod 600)",
        path.display()
    );
    Ok(())
}

pub fn write_new(path: &Path, contents: &[u8]) -> Result<()> {
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(contents)?;
    file.sync_all()?;
    Ok(())
}

pub fn open(directory: &Path) -> Result<Identity> {
    if !directory.exists() {
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(directory)?;
    }
    let metadata = fs::symlink_metadata(directory)?;
    ensure!(
        metadata.uid() == unsafe { libc::geteuid() },
        "identity directory must be owned by the current user"
    );
    ensure!(
        metadata.is_dir() && metadata.mode() & 0o077 == 0,
        "identity directory must be private (chmod 700), not a symlink"
    );
    let certificate = directory.join("certificate.pem");
    let key = directory.join("key.pem");
    let pairing = directory.join("pairing.json");
    let token_file = directory.join("token");
    let existing = [&certificate, &key, &token_file]
        .iter()
        .filter(|p| p.exists())
        .count();
    ensure!(
        existing == 0 || existing == 3,
        "incomplete host identity; inspect {} instead of silently regenerating credentials",
        directory.display()
    );
    if existing == 0 {
        ensure!(
            !pairing.exists(),
            "pairing file exists without its identity"
        );
        let certified = rcgen::generate_simple_self_signed(vec!["teleport.local".into()])?;
        write_new(
            &key,
            pem("PRIVATE KEY", &certified.signing_key.serialize_der()).as_bytes(),
        )?;
        write_new(
            &certificate,
            pem("CERTIFICATE", certified.cert.der()).as_bytes(),
        )?;
        write_new(&token_file, token().as_bytes())?;
    }
    for path in [&certificate, &key, &token_file] {
        private_file(path)?;
    }
    let token = fs::read_to_string(&token_file).context("read persistent host token")?;
    ensure!(
        token.len() == 64 && token.bytes().all(|b| b.is_ascii_hexdigit()),
        "invalid persistent token"
    );
    if pairing.exists() {
        private_file(&pairing)?;
        ensure!(
            Pairing::read(&pairing)?.token == token,
            "pairing token does not match persistent host identity"
        );
    }
    Ok(Identity {
        certificate,
        key,
        pairing,
        token,
    })
}

pub fn token() -> String {
    rand::random::<[u8; 32]>()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn stable_identity_and_partial_identity_rejected() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let dir = temp.path().join("identity");
        let first = open(&dir)?;
        let second = open(&dir)?;
        assert_eq!(first.token, second.token);
        assert_eq!(fs::read(first.key)?, fs::read(second.key)?);
        fs::remove_file(second.certificate)?;
        assert!(open(&dir).is_err());
        Ok(())
    }
}
