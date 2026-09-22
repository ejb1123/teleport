//! Locally trusted host profiles. Credentials never belong in the repository.
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
};

#[derive(Clone, Serialize, Deserialize)]
pub struct Profile {
    pub address: String,
    pub pairing_file: PathBuf,
}

pub fn directory() -> Result<PathBuf> {
    let home = std::env::var_os("HOME").context("HOME is not set")?;
    #[cfg(target_os = "macos")]
    let path = PathBuf::from(home).join("Library/Application Support/Teleport");
    #[cfg(not(target_os = "macos"))]
    let path = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(home).join(".config"))
        .join("teleport");
    prepare_directory(&path)?;
    Ok(path)
}

fn prepare_directory(path: &Path) -> Result<()> {
    if let Ok(metadata) = fs::symlink_metadata(path) {
        ensure!(
            metadata.is_dir() && !metadata.file_type().is_symlink(),
            "profile directory must be a real directory, not a symlink"
        );
    }
    fs::create_dir_all(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

pub fn check_private_file(path: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    ensure!(
        metadata.is_file() && !metadata.file_type().is_symlink(),
        "credentials must be a regular file, not a symlink"
    );
    ensure!(
        metadata.len() <= 64 * 1024,
        "credential/profile file is too large"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        ensure!(
            metadata.permissions().mode() & 0o077 == 0,
            "credentials are accessible to other users; run chmod 600 on this file"
        );
    }
    Ok(())
}

pub fn load() -> Result<Vec<Profile>> {
    let path = directory()?.join("profiles.json");
    if fs::symlink_metadata(&path).is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound)
    {
        return Ok(Vec::new());
    }
    check_private_file(&path)?;
    Ok(serde_json::from_slice(&fs::read(path)?)?)
}

fn private_write(path: &Path, data: &[u8]) -> Result<()> {
    let temporary = path.with_extension(format!("{}.tmp", rand::random::<u64>()));
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&temporary)?;
    file.write_all(data)?;
    file.sync_all()?;
    fs::rename(temporary, path)?;
    Ok(())
}

pub fn import(address: &str, source: &Path, profiles: &mut Vec<Profile>) -> Result<Profile> {
    check_private_file(source)?;
    let pairing = crate::protocol::Pairing::read(source)?;
    save_pairing(address, &pairing, profiles)
}

pub fn validate_address(address: &str) -> Result<()> {
    ensure!(
        !address.is_empty()
            && !address.contains(['/', '@', '?', '#'])
            && !address.chars().any(char::is_whitespace),
        "enter a host:port address, not a URL"
    );
    let url: url::Url = format!("moqt://{address}").parse()?;
    ensure!(
        url.host_str().is_some() && url.port().is_some(),
        "include the port, e.g. desktop.local:4443"
    );
    Ok(())
}

/// Save only credentials received through an authenticated pairing exchange or
/// explicitly imported from a trusted file. Network discovery is not trust.
pub fn save_pairing(
    address: &str,
    pairing: &crate::protocol::Pairing,
    profiles: &mut Vec<Profile>,
) -> Result<Profile> {
    save_pairing_in(address, pairing, profiles, &directory()?)
}

fn save_pairing_in(
    address: &str,
    pairing: &crate::protocol::Pairing,
    profiles: &mut Vec<Profile>,
    directory: &Path,
) -> Result<Profile> {
    validate_address(address)?;
    pairing.validate()?;
    prepare_directory(directory)?;
    let pairing_file = directory.join(format!("pairing-{:016x}.json", rand::random::<u64>()));
    private_write(&pairing_file, &serde_json::to_vec(&pairing)?)?;
    let profile = Profile {
        address: address.to_owned(),
        pairing_file,
    };
    // Keep the previous trusted credential until the updated index has been saved.
    let mut updated = profiles.clone();
    updated.retain(|p| p.address != address);
    updated.push(profile.clone());
    private_write(
        &directory.join("profiles.json"),
        &serde_json::to_vec_pretty(&updated)?,
    )?;
    *profiles = updated;
    Ok(profile)
}

#[cfg(test)]
mod tests {
    #[test]
    fn verified_pairing_is_saved_privately_and_replaces_selected_host() {
        let directory = tempfile::tempdir().unwrap();
        let mut profiles = Vec::new();
        let mut pairing = crate::protocol::Pairing {
            token: "aa".repeat(32),
            fingerprint: "bb".repeat(32),
        };
        let old = super::save_pairing_in(
            "desktop.local:4443",
            &pairing,
            &mut profiles,
            directory.path(),
        )
        .unwrap();
        super::check_private_file(&old.pairing_file).unwrap();
        super::check_private_file(&directory.path().join("profiles.json")).unwrap();
        pairing.token = "cc".repeat(32);
        let new = super::save_pairing_in(
            "desktop.local:4443",
            &pairing,
            &mut profiles,
            directory.path(),
        )
        .unwrap();
        assert_eq!(profiles.len(), 1);
        assert_eq!(profiles[0].pairing_file, new.pairing_file);
        assert_ne!(old.pairing_file, new.pairing_file);
        assert_eq!(
            crate::protocol::Pairing::read(&new.pairing_file)
                .unwrap()
                .token,
            pairing.token
        );
        assert!(
            super::save_pairing_in(
                "https://desktop.local:4443",
                &pairing,
                &mut profiles,
                directory.path(),
            )
            .is_err()
        );
        pairing.token = "bad".into();
        assert!(
            super::save_pairing_in(
                "desktop.local:4443",
                &pairing,
                &mut profiles,
                directory.path(),
            )
            .is_err()
        );
        assert_eq!(profiles.len(), 1);
        assert_eq!(profiles[0].pairing_file, new.pairing_file);
    }

    #[test]
    fn addresses_require_a_host_and_port_without_url_credentials() {
        for address in ["desktop.local:4443", "192.168.1.2:4443", "[::1]:4443"] {
            assert!(super::validate_address(address).is_ok(), "{address}");
        }
        for address in [
            "",
            "desktop.local",
            "user@desktop.local:4443",
            "desktop.local:4443/path",
            "desktop.local:4443?query",
            " desktop.local:4443",
        ] {
            assert!(super::validate_address(address).is_err(), "{address}");
        }
    }

    #[test]
    #[cfg(unix)]
    fn refuses_symlinks_and_public_credentials() {
        use std::os::unix::fs::{PermissionsExt, symlink};
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("pairing.json");
        super::private_write(&file, b"{}").unwrap();
        assert!(super::check_private_file(&file).is_ok());
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(super::check_private_file(&file).is_err());
        let link = dir.path().join("link");
        symlink(&file, &link).unwrap();
        assert!(super::check_private_file(&link).is_err());
        let dir_link = dir.path().join("dir-link");
        symlink(dir.path(), &dir_link).unwrap();
        assert!(super::prepare_directory(&dir_link).is_err());
    }
    #[test]
    fn private_writes_replace_atomically() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("credentials.json");
        super::private_write(&file, b"first").unwrap();
        super::private_write(&file, b"second").unwrap();
        assert_eq!(std::fs::read(&file).unwrap(), b"second");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(file).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
    }
}
