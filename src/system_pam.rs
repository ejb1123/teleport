//! Same-UID PAM authentication, isolated from the network host in a short-lived
//! host-distribution helper. Never load arbitrary host PAM modules into Nix Rust.

use anyhow::{Context, Result, bail, ensure};
use std::{ffi::CString, os::unix::fs::MetadataExt, path::Path, process::Stdio, time::Duration};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use zeroize::Zeroizing;

const WORKER_TIMEOUT: Duration = Duration::from_secs(20);

/// Reject root, privilege transitions, aliases and attachment to another UID.
pub fn validate_self_user(username: &str) -> Result<()> {
    ensure!(
        !username.is_empty() && username.len() <= 256,
        "invalid account name"
    );
    let username = CString::new(username).context("invalid account name")?;
    // getpwnam_r owns no global storage; the returned pointers live in buffer.
    let mut entry: libc::passwd = unsafe { std::mem::zeroed() };
    let mut found = std::ptr::null_mut();
    let mut buffer = vec![0u8; 65536];
    let uid = unsafe { libc::getuid() };
    ensure!(
        uid != 0
            && uid == unsafe { libc::geteuid() }
            && unsafe { libc::getgid() } == unsafe { libc::getegid() },
        "system login requires an unprivileged user host"
    );
    let status = unsafe {
        libc::getpwnam_r(
            username.as_ptr(),
            &mut entry,
            buffer.as_mut_ptr().cast(),
            buffer.len(),
            &mut found,
        )
    };
    ensure!(status == 0 && !found.is_null(), "account is unavailable");
    ensure!(
        entry.pw_uid == uid && !entry.pw_name.is_null(),
        "account does not own this host"
    );
    ensure!(
        unsafe { std::ffi::CStr::from_ptr(entry.pw_name) } == username.as_c_str(),
        "account alias is not allowed"
    );
    Ok(())
}

fn request(username: &str, password: &str) -> Result<Zeroizing<Vec<u8>>> {
    ensure!(
        !username.is_empty() && username.len() <= 256 && !username.contains('\0'),
        "invalid account name"
    );
    ensure!(
        !password.is_empty() && password.len() <= 4096 && !password.contains('\0'),
        "invalid account password"
    );
    let mut frame = Zeroizing::new(Vec::with_capacity(16 + username.len() + password.len()));
    frame.extend_from_slice(b"TPAM0001");
    frame.extend_from_slice(&(username.len() as u32).to_be_bytes());
    frame.extend_from_slice(&(password.len() as u32).to_be_bytes());
    frame.extend_from_slice(username.as_bytes());
    frame.extend_from_slice(password.as_bytes());
    Ok(frame)
}

/// Authenticate only this host's existing account. Passwords never enter argv,
/// environment, logs or files. The caller must throttle before invoking this.
pub async fn authenticate(helper: &Path, username: &str, password: &str) -> Result<()> {
    validate_self_user(username)?;
    let helper = validate_helper(helper)?;
    run_worker(&helper, request(username, password)?, WORKER_TIMEOUT).await
}

pub fn validate_helper(helper: &Path) -> Result<std::path::PathBuf> {
    ensure!(helper.is_absolute(), "PAM helper path must be absolute");
    let helper = helper
        .canonicalize()
        .context("PAM helper is not installed")?;
    let metadata = helper.metadata()?;
    ensure!(
        metadata.is_file()
            && metadata.uid() == 0
            && metadata.mode() & 0o6022 == 0
            && metadata.mode() & 0o111 != 0,
        "PAM helper must be root-owned, executable, non-setuid and not writable by group/others"
    );
    // Reject replaceable parent directories as well as a writable helper.
    for directory in helper.ancestors().skip(1) {
        let metadata = directory.metadata()?;
        ensure!(
            trusted_directory(metadata.uid(), metadata.mode()),
            "PAM helper directory must be root-owned and not writable by group/others"
        );
    }
    Ok(helper)
}

fn trusted_directory(uid: u32, mode: u32) -> bool {
    // Nix's store is root:nixbld 1775. Sticky directories do not permit
    // another user to replace the root-owned next component, whose ownership
    // is checked separately above. Non-sticky writable ancestors are unsafe.
    uid == 0 && (mode & 0o022 == 0 || mode & 0o1000 != 0)
}

async fn run_worker(helper: &Path, frame: Zeroizing<Vec<u8>>, timeout: Duration) -> Result<()> {
    let mut child = tokio::process::Command::new(helper)
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("LANG", "C")
        .env("LC_ALL", "C")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .context("could not start PAM helper")?;
    let outcome = tokio::time::timeout(timeout, async {
        let mut stdin = child.stdin.take().context("missing PAM input pipe")?;
        stdin.write_all(&frame).await?;
        drop(frame);
        stdin.shutdown().await?;
        drop(stdin);
        let mut output = Vec::with_capacity(4);
        child
            .stdout
            .take()
            .context("missing PAM output pipe")?
            .take(4)
            .read_to_end(&mut output)
            .await?;
        ensure!(output == b"OK\n", "system-account authentication failed");
        ensure!(
            child.wait().await?.success(),
            "system-account authentication failed"
        );
        Ok(())
    })
    .await;
    match outcome {
        Ok(Ok(())) => Ok(()),
        Ok(Err(error)) => {
            let _ = child.kill().await;
            Err(error)
        }
        Err(_) => {
            let _ = child.kill().await;
            bail!("system-account authentication timed out")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn root_owned_sticky_store_ancestors_are_safe() {
        assert!(trusted_directory(0, 0o1775));
        assert!(trusted_directory(0, 0o755));
        assert!(!trusted_directory(0, 0o775));
        assert!(!trusted_directory(1000, 0o1755));
        assert!(!trusted_directory(1000, 0o755));
    }
    #[test]
    fn rejects_invalid_credentials_and_encodes_lengths() {
        assert!(request("ej", "").is_err());
        assert!(request("root\0ej", "secret").is_err());
        assert!(request("ej", "pass\0word").is_err());
        assert!(request("ej", &"a".repeat(4097)).is_err());
        let frame = request("ej", "example").unwrap();
        assert_eq!(&frame[..16], b"TPAM0001\0\0\0\x02\0\0\0\x07");
        assert_eq!(&frame[16..], b"ejexample");
    }
    #[test]
    fn rejects_root_and_unknown_account_without_pam() {
        assert!(validate_self_user("root").is_err());
        assert!(validate_self_user("teleport-account-that-must-not-exist-9842983").is_err());
    }
    #[test]
    fn helper_must_be_administrator_installed() {
        assert!(validate_helper(Path::new("teleport-pam")).is_err());
        let directory = tempfile::tempdir().unwrap();
        let helper = directory.path().join("teleport-pam");
        std::fs::write(&helper, b"fixture").unwrap();
        assert!(validate_helper(&helper).is_err());
    }
    #[tokio::test]
    async fn worker_failure_and_timeout_are_closed() {
        use std::os::unix::fs::PermissionsExt;
        let directory = tempfile::tempdir().unwrap();
        let helper = directory.path().join("fixture");
        let shell = if Path::new("/bin/sh").exists() {
            "/bin/sh".to_owned()
        } else {
            std::env::var("SHELL").expect("Nix sandbox must provide its build shell")
        };
        let prelude = format!("#!{shell}\nwhile IFS= read -r line; do :; done\n");
        // Synthetic process only: no PAM library or real account attempts.
        std::fs::write(&helper, format!("{prelude}printf 'NO\\n'\n")).unwrap();
        std::fs::set_permissions(&helper, std::fs::Permissions::from_mode(0o700)).unwrap();
        assert!(
            run_worker(
                &helper,
                request("fixture", "fake").unwrap(),
                Duration::from_secs(2)
            )
            .await
            .is_err()
        );
        std::fs::write(&helper, format!("{prelude}printf 'OK\\n'\n")).unwrap();
        assert!(
            run_worker(
                &helper,
                request("fixture", "fake").unwrap(),
                Duration::from_secs(2)
            )
            .await
            .is_ok()
        );
        std::fs::write(&helper, format!("#!{shell}\nwhile :; do :; done\n")).unwrap();
        assert!(
            run_worker(
                &helper,
                request("fixture", "fake").unwrap(),
                Duration::from_millis(50)
            )
            .await
            .is_err()
        );
    }
}
