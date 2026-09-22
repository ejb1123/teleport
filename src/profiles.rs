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
    #[serde(default)]
    pub name: String,
    pub address: String,
    #[serde(default)]
    pub pairing_file: Option<PathBuf>,
    #[serde(default)]
    pub fingerprint: Option<String>,
}

impl Profile {
    pub fn label(&self) -> &str {
        if self.name.is_empty() {
            &self.address
        } else {
            &self.name
        }
    }

    pub fn trusted_fingerprint(&self) -> Result<Option<String>> {
        let saved = self
            .pairing_file
            .as_ref()
            .map(|path| {
                // Losing a credential must not discard a separately saved pin
                // or prevent signing in again against that exact identity.
                if self.fingerprint.is_some() && !path.try_exists()? {
                    return Ok(None);
                }
                check_private_file(path)?;
                Ok::<_, anyhow::Error>(Some(
                    crate::protocol::Pairing::read(path)?
                        .fingerprint
                        .to_ascii_lowercase(),
                ))
            })
            .transpose()?
            .flatten();
        if let Some(pin) = &self.fingerprint {
            let parsed = moq_native::tls::parse_fingerprint(pin)?;
            if let Some(saved) = &saved {
                ensure!(
                    moq_native::tls::parse_fingerprint(saved)? == parsed,
                    "saved host identity mismatch"
                );
            }
            Ok(Some(pin.to_ascii_lowercase()))
        } else {
            Ok(saved)
        }
    }
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
    load_in(&directory()?)
}

fn load_in(directory: &Path) -> Result<Vec<Profile>> {
    let path = directory.join("profiles.json");
    if fs::symlink_metadata(&path).is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound)
    {
        return Ok(Vec::new());
    }
    check_private_file(&path)?;
    Ok(serde_json::from_slice(&fs::read(path)?)?)
}

/// Serialize mutations across launcher/CLI processes. Lock a separate stable
/// inode: locking profiles.json itself would be defeated by atomic replacement.
/// Always refresh the caller's view before checking identity or uniqueness.
fn lock_and_reload(directory: &Path, profiles: &mut Vec<Profile>) -> Result<fs::File> {
    prepare_directory(directory)?;
    let mut options = fs::OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let lock = options.open(directory.join("profiles.lock"))?;
    let metadata = lock.metadata()?;
    ensure!(metadata.is_file(), "profile lock must be a regular file");
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        ensure!(
            metadata.uid() == unsafe { libc::geteuid() } && metadata.mode() & 0o077 == 0,
            "profile lock must be private and owned by this user"
        );
    }
    lock.lock().context("lock saved desktop identities")?;
    *profiles = load_in(directory)?;
    Ok(lock)
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

pub fn save_desktop(name: &str, address: &str, profiles: &mut Vec<Profile>) -> Result<Profile> {
    save_desktop_in(name, address, profiles, &directory()?)
}

fn save_desktop_in(
    name: &str,
    address: &str,
    profiles: &mut Vec<Profile>,
    directory: &Path,
) -> Result<Profile> {
    validate_address(address)?;
    ensure!(
        !name.trim().is_empty() && name.len() <= 120 && !name.chars().any(char::is_control),
        "enter a desktop name (up to 120 characters)"
    );
    let _lock = lock_and_reload(directory, profiles)?;
    ensure!(
        !profiles.iter().any(|profile| profile.address == address),
        "this desktop address is already saved"
    );
    let profile = Profile {
        name: name.trim().into(),
        address: address.into(),
        pairing_file: None,
        fingerprint: None,
    };
    let mut updated = profiles.clone();
    updated.push(profile.clone());
    prepare_directory(directory)?;
    private_write(
        &directory.join("profiles.json"),
        &serde_json::to_vec_pretty(&updated)?,
    )?;
    *profiles = updated;
    Ok(profile)
}

/// Caller must explicitly show and approve a first-contact fingerprint. Existing
/// trust is immutable through this API, including trust in legacy pairing files.
pub fn approve_fingerprint(
    address: &str,
    fingerprint: &str,
    profiles: &mut Vec<Profile>,
) -> Result<Profile> {
    approve_fingerprint_in(address, fingerprint, profiles, &directory()?)
}

fn approve_fingerprint_in(
    address: &str,
    fingerprint: &str,
    profiles: &mut Vec<Profile>,
    directory: &Path,
) -> Result<Profile> {
    let parsed = moq_native::tls::parse_fingerprint(fingerprint)?;
    let _lock = lock_and_reload(directory, profiles)?;
    let mut updated = profiles.clone();
    let profile = updated
        .iter_mut()
        .find(|profile| profile.address == address)
        .context("save the desktop first")?;
    if let Some(existing) = profile.trusted_fingerprint()? {
        ensure!(
            moq_native::tls::parse_fingerprint(&existing)? == parsed,
            "HOST IDENTITY CHANGED: refusing to replace saved trust"
        );
    }
    profile.fingerprint = Some(fingerprint.trim().to_ascii_lowercase());
    let result = profile.clone();
    private_write(
        &directory.join("profiles.json"),
        &serde_json::to_vec_pretty(&updated)?,
    )?;
    *profiles = updated;
    Ok(result)
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
    let _lock = lock_and_reload(directory, profiles)?;
    let previous = profiles.iter().find(|profile| profile.address == address);
    if let Some(previous) = previous
        && let Some(pin) = previous.trusted_fingerprint()?
    {
        ensure!(
            moq_native::tls::parse_fingerprint(&pin)?
                == moq_native::tls::parse_fingerprint(&pairing.fingerprint)?,
            "HOST IDENTITY CHANGED: refusing to replace saved trust"
        );
    }
    prepare_directory(directory)?;
    let pairing_file = directory.join(format!("pairing-{:016x}.json", rand::random::<u64>()));
    private_write(&pairing_file, &serde_json::to_vec(&pairing)?)?;
    let profile = Profile {
        name: previous.map_or_else(String::new, |profile| profile.name.clone()),
        address: address.to_owned(),
        pairing_file: Some(pairing_file),
        fingerprint: Some(pairing.fingerprint.to_ascii_lowercase()),
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
    fn stale_instances_cannot_replace_pins_or_erase_other_desktops() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let mut first = Vec::new();
        super::save_desktop_in("Home", "home:4443", &mut first, dir.path())?;
        let mut stale_approval = first.clone();
        let mut stale_pairing = first.clone();
        let mut stale_add = first.clone();
        let pin = "ab".repeat(32);
        super::approve_fingerprint_in("home:4443", &pin, &mut first, dir.path())?;
        let before = std::fs::read(dir.path().join("profiles.json"))?;
        assert!(
            super::approve_fingerprint_in(
                "home:4443",
                &"cd".repeat(32),
                &mut stale_approval,
                dir.path()
            )
            .is_err()
        );
        let wrong = crate::protocol::Pairing {
            token: "aa".repeat(32),
            fingerprint: "cd".repeat(32),
        };
        assert!(
            super::save_pairing_in("home:4443", &wrong, &mut stale_pairing, dir.path()).is_err()
        );
        assert_eq!(std::fs::read(dir.path().join("profiles.json"))?, before);
        assert_eq!(
            stale_approval[0].trusted_fingerprint()?.as_deref(),
            Some(pin.as_str())
        );
        assert_eq!(
            stale_pairing[0].trusted_fingerprint()?.as_deref(),
            Some(pin.as_str())
        );
        super::save_desktop_in("Work", "work:4443", &mut stale_add, dir.path())?;
        let current = super::load_in(dir.path())?;
        assert_eq!(current.len(), 2);
        assert_eq!(current[0].label(), "Home");
        assert_eq!(
            current[0].trusted_fingerprint()?.as_deref(),
            Some(pin.as_str())
        );
        assert_eq!(current[1].label(), "Work");
        // A stale successful same-identity pairing also preserves the other entry.
        let correct = crate::protocol::Pairing {
            fingerprint: pin.clone(),
            ..wrong
        };
        let saved = super::save_pairing_in("home:4443", &correct, &mut first, dir.path())?;
        assert_eq!(saved.label(), "Home");
        assert_eq!(first.len(), 2);
        assert!(first.iter().any(|profile| profile.label() == "Work"));
        Ok(())
    }

    #[test]
    fn simultaneous_adds_preserve_both_desktops() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
        let handles: Vec<_> = ["one", "two"]
            .into_iter()
            .map(|name| {
                let directory = dir.path().to_owned();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    let mut profiles = Vec::new();
                    barrier.wait();
                    super::save_desktop_in(name, &format!("{name}:4443"), &mut profiles, &directory)
                })
            })
            .collect();
        for handle in handles {
            handle.join().unwrap()?;
        }
        let profiles = super::load_in(dir.path())?;
        assert_eq!(profiles.len(), 2);
        assert!(profiles.iter().any(|profile| profile.label() == "one"));
        assert!(profiles.iter().any(|profile| profile.label() == "two"));
        Ok(())
    }

    #[test]
    #[cfg(unix)]
    fn profile_lock_rejects_symlink_and_public_file() -> anyhow::Result<()> {
        use std::os::unix::fs::{PermissionsExt, symlink};
        let dir = tempfile::tempdir()?;
        let target = dir.path().join("other");
        super::private_write(&target, b"untouched")?;
        let lock = dir.path().join("profiles.lock");
        symlink(&target, &lock)?;
        assert!(super::save_desktop_in("Home", "home:4443", &mut Vec::new(), dir.path()).is_err());
        assert_eq!(std::fs::read(&target)?, b"untouched");
        std::fs::remove_file(&lock)?;
        super::private_write(&lock, b"")?;
        std::fs::set_permissions(&lock, std::fs::Permissions::from_mode(0o644))?;
        assert!(super::save_desktop_in("Home", "home:4443", &mut Vec::new(), dir.path()).is_err());
        Ok(())
    }

    #[test]
    fn legacy_profiles_load_and_drafts_need_no_credentials() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let legacy = crate::protocol::Pairing {
            token: "aa".repeat(32),
            fingerprint: "bb".repeat(32),
        };
        let credential = dir.path().join("legacy.json");
        super::private_write(&credential, &serde_json::to_vec(&legacy)?)?;
        super::private_write(
            &dir.path().join("profiles.json"),
            &serde_json::to_vec(&serde_json::json!([
                { "address": "old:4443", "pairing_file": credential }
            ]))?,
        )?;
        let mut profiles = super::load_in(dir.path())?;
        assert_eq!(profiles[0].label(), "old:4443");
        assert_eq!(
            profiles[0].trusted_fingerprint()?.as_deref(),
            Some(legacy.fingerprint.as_str())
        );
        let draft = super::save_desktop_in("Work desktop", "new:4443", &mut profiles, dir.path())?;
        assert_eq!(draft.label(), "Work desktop");
        assert!(draft.pairing_file.is_none());
        assert!(draft.trusted_fingerprint()?.is_none());
        assert_eq!(super::load_in(dir.path())?.len(), 2);
        assert!(
            super::save_desktop_in("Duplicate", "new:4443", &mut profiles, dir.path()).is_err()
        );
        assert!(
            super::save_desktop_in("", "missing-name:4443", &mut profiles, dir.path()).is_err()
        );
        Ok(())
    }

    #[test]
    fn first_pin_persists_and_all_replacements_fail_closed() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let mut profiles = Vec::new();
        super::save_desktop_in("Home", "home:4443", &mut profiles, dir.path())?;
        let pin = "ab".repeat(32);
        let approved = super::approve_fingerprint_in("home:4443", &pin, &mut profiles, dir.path())?;
        assert!(approved.pairing_file.is_none());
        assert_eq!(
            super::load_in(dir.path())?[0].fingerprint.as_deref(),
            Some(pin.as_str())
        );
        let mut pairing = crate::protocol::Pairing {
            token: "aa".repeat(32),
            fingerprint: pin.clone(),
        };
        let trusted = super::save_pairing_in("home:4443", &pairing, &mut profiles, dir.path())?;
        assert_eq!(trusted.label(), "Home");
        let before = std::fs::read(dir.path().join("profiles.json"))?;
        pairing.fingerprint = "cd".repeat(32);
        assert!(super::save_pairing_in("home:4443", &pairing, &mut profiles, dir.path()).is_err());
        assert!(
            super::approve_fingerprint_in(
                "home:4443",
                &pairing.fingerprint,
                &mut profiles,
                dir.path()
            )
            .is_err()
        );
        assert_eq!(std::fs::read(dir.path().join("profiles.json"))?, before);
        assert_eq!(
            profiles[0].trusted_fingerprint()?.as_deref(),
            Some(pin.as_str())
        );
        // A legacy credential's certificate is equally immutable without an
        // explicit fingerprint field in the profile index.
        profiles[0].fingerprint = None;
        super::private_write(
            &dir.path().join("profiles.json"),
            &serde_json::to_vec(&profiles)?,
        )?;
        assert!(super::save_pairing_in("home:4443", &pairing, &mut profiles, dir.path()).is_err());
        assert!(
            super::approve_fingerprint_in(
                "home:4443",
                &pairing.fingerprint,
                &mut profiles,
                dir.path()
            )
            .is_err()
        );
        profiles[0].fingerprint = Some(pin.clone());
        std::fs::remove_file(trusted.pairing_file.as_ref().unwrap())?;
        assert_eq!(
            profiles[0].trusted_fingerprint()?.as_deref(),
            Some(pin.as_str())
        );
        Ok(())
    }

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
        super::check_private_file(old.pairing_file.as_ref().unwrap()).unwrap();
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
            crate::protocol::Pairing::read(new.pairing_file.as_ref().unwrap())
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
