//! The host user's graphical login session through logind (`loginctl`):
//! lock state, locking and unlocking (G24, docs/wake-and-unattended.md).
//!
//! * `teleport lock-session` locks the session right after an automatic
//!   login (NixOS `unattended`), so a machine that boots into the desktop
//!   is never left open.
//! * Secure attention (the client's Ctrl+Alt+End) locks an unlocked
//!   session, or wakes the lock screen's password prompt.
//! * `--unlock-on-system-login` unlocks after PAM has just verified the
//!   account password for a system login; the password is never stored.
//!
//! logind lets a user lock and unlock their own sessions without further
//! authorization; KDE's screen locker follows its Lock and Unlock signals.

use anyhow::{Context, Result, ensure};
use std::collections::HashMap;
use std::time::{Duration, Instant};

/// A logind session id (`2`, `c1`, …).
fn valid_session(id: &str) -> bool {
    id.as_bytes().first().is_some_and(u8::is_ascii_alphanumeric)
        && id.len() <= 64
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

fn loginctl(args: &[&str]) -> Result<String> {
    let output = std::process::Command::new("loginctl")
        .args(args)
        .stdin(std::process::Stdio::null())
        .output()
        .context("loginctl is unavailable")?;
    ensure!(
        output.status.success(),
        "loginctl {}: {}",
        args.first().copied().unwrap_or_default(),
        String::from_utf8_lossy(&output.stderr).trim()
    );
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

/// The session to act on: the user's display session as logind reports it
/// (`show-user -p Display`), else this process's own session.
pub fn choose_session(display: &str, own: Option<&str>) -> Option<String> {
    [Some(display.trim()), own.map(str::trim)]
        .into_iter()
        .flatten()
        .find(|id| valid_session(id))
        .map(str::to_owned)
}

/// The user's graphical session id.
pub fn display_session() -> Result<String> {
    let uid = unsafe { libc::getuid() }.to_string();
    let display =
        loginctl(&["show-user", &uid, "--property=Display", "--value"]).unwrap_or_default();
    choose_session(&display, std::env::var("XDG_SESSION_ID").ok().as_deref())
        .context("no graphical login session for this user")
}

/// `LockedHint` of `session`.
pub fn locked(session: &str) -> Result<bool> {
    ensure!(valid_session(session), "invalid session id");
    parse_locked_hint(&loginctl(&[
        "show-session",
        session,
        "--property=LockedHint",
        "--value",
    ])?)
}

pub fn parse_locked_hint(value: &str) -> Result<bool> {
    match value.trim() {
        "yes" => Ok(true),
        "no" => Ok(false),
        other => anyhow::bail!("unexpected LockedHint {other:?}"),
    }
}

pub fn lock(session: &str) -> Result<()> {
    ensure!(valid_session(session), "invalid session id");
    loginctl(&["lock-session", session]).map(drop)
}

pub fn unlock(session: &str) -> Result<()> {
    ensure!(valid_session(session), "invalid session id");
    loginctl(&["unlock-session", session]).map(drop)
}

/// Whether this host can report and change its session's lock state.
pub fn available() -> bool {
    display_session()
        .and_then(|session| locked(&session))
        .is_ok()
}

/// What secure attention does.
#[derive(Debug, PartialEq, Eq)]
pub enum Attention {
    /// The session was unlocked: lock it (Windows' Ctrl+Alt+Del menu has
    /// "Lock" first; signing out or switching users is not offered).
    Lock,
    /// The session is locked: show the password prompt (the client then
    /// types the password, as at the machine).
    Prompt,
}

pub fn attention_for(locked: bool) -> Attention {
    if locked {
        Attention::Prompt
    } else {
        Attention::Lock
    }
}

/// Secure attention: lock the session if it is unlocked. Returns what the
/// caller must still do (show the prompt by a harmless key press).
pub fn secure_attention() -> Result<Attention> {
    let session = display_session()?;
    let attention = attention_for(locked(&session)?);
    if attention == Attention::Lock {
        lock(&session)?;
    }
    Ok(attention)
}

/// One step of locking after an automatic login.
#[derive(Debug, PartialEq, Eq)]
pub enum LockStep {
    Done,
    /// Ask logind to lock (again: the locker may not have been up yet).
    Lock,
    /// No session yet, or its state is unknown: wait.
    Wait,
    /// Out of time while still unlocked.
    Fail,
}

/// `session`: the user's session and its LockedHint, if known.
pub fn lock_step(
    session: Option<bool>,
    since_request: Option<Duration>,
    expired: bool,
) -> LockStep {
    match session {
        Some(true) => LockStep::Done,
        _ if expired => LockStep::Fail,
        // Re-request every 2 s until the screen locker reports locked.
        Some(false) if since_request.is_none_or(|elapsed| elapsed >= Duration::from_secs(2)) => {
            LockStep::Lock
        }
        _ => LockStep::Wait,
    }
}

/// The PAM service logind recorded for `session` (`sddm-autologin`, …).
pub fn service(session: &str) -> Result<String> {
    ensure!(valid_session(session), "invalid session id");
    loginctl(&["show-session", session, "--property=Service", "--value"])
}

/// Whether a session's PAM service is a display manager's automatic login
/// (`sddm-autologin`, `gdm-autologin`, `lightdm-autologin`).
pub fn automatic_login(service: &str) -> bool {
    service.trim().ends_with("-autologin")
}

/// KDE's portal permission table and entry that let an application start
/// remote desktop without the local consent dialog (KRdp's mechanism; the
/// NixOS module's `preauthorizeRemoteDesktop`).
const KDE_PERMISSION_TABLE: &str = "kde-authorized";
const KDE_PERMISSION_ID: &str = "remote-desktop";

/// Whether a permission-store entry (application → permissions) grants
/// `app` remote desktop: KDE checks for a first permission of `yes`.
pub fn preauthorized(permissions: &HashMap<String, Vec<String>>, app: &str) -> bool {
    permissions
        .get(app)
        .and_then(|values| values.first())
        .is_some_and(|value| value == "yes")
}

/// Whether KDE pre-authorizes the host (`capture::PORTAL_APP_ID`) for
/// remote desktop; `None` when the permission store cannot be read.
async fn kde_preauthorized() -> Option<bool> {
    use ashpd::zbus;
    let connection = zbus::Connection::session().await.ok()?;
    let reply = connection
        .call_method(
            Some("org.freedesktop.impl.portal.PermissionStore"),
            "/org/freedesktop/impl/portal/PermissionStore",
            Some("org.freedesktop.impl.portal.PermissionStore"),
            "Lookup",
            &(KDE_PERMISSION_TABLE, KDE_PERMISSION_ID),
        )
        .await;
    match reply {
        Ok(message) => {
            let (permissions, _data): (HashMap<String, Vec<String>>, zbus::zvariant::OwnedValue) =
                message.body().deserialize().ok()?;
            Some(preauthorized(&permissions, crate::capture::PORTAL_APP_ID))
        }
        // No such table or entry: nothing was ever pre-authorized.
        Err(zbus::Error::MethodError(name, _, _))
            if name.as_str() == "org.freedesktop.portal.Error.NotFound" =>
        {
            Some(false)
        }
        Err(_) => None,
    }
}

/// Unattended start (G24) needs KDE's pre-authorization: on a locked screen
/// nobody can answer the portal's consent dialog, so the host would wait
/// for it until someone unlocks at the machine. Warns when the host starts
/// on KDE in a session that is locked or was logged in automatically and
/// the host is not pre-authorized. Silent when anything cannot be read.
pub async fn warn_unless_preauthorized() {
    let desktop = std::env::var("XDG_CURRENT_DESKTOP").unwrap_or_default();
    if !desktop
        .split(':')
        .any(|name| name.eq_ignore_ascii_case("KDE"))
    {
        return;
    }
    let unattended = tokio::task::spawn_blocking(|| {
        let session = display_session().ok()?;
        let automatic = service(&session).is_ok_and(|service| automatic_login(&service));
        Some(automatic || locked(&session).unwrap_or(false))
    })
    .await
    .ok()
    .flatten()
    .unwrap_or(false);
    if !unattended {
        return;
    }
    let authorized = tokio::time::timeout(Duration::from_secs(5), kde_preauthorized())
        .await
        .ok()
        .flatten();
    if authorized == Some(false) {
        let app = crate::capture::PORTAL_APP_ID;
        tracing::warn!(
            "this session is locked or was logged in automatically, but KDE has not pre-authorized {app} for remote desktop: nobody can approve its consent dialog on a locked screen. Run once as this user: flatpak permission-set {KDE_PERMISSION_TABLE} {KDE_PERMISSION_ID} {app} yes (see {})",
            crate::docs::page("wake-and-unattended.md")
        );
    }
}

/// `teleport lock-session`: lock the user's graphical session and wait until
/// logind reports it locked (at most `timeout`). With `only_automatic`, a
/// session someone logged in to with a password is left alone.
pub fn lock_and_wait(timeout: Duration, only_automatic: bool) -> Result<()> {
    let deadline = Instant::now() + timeout;
    if only_automatic {
        // logind knows the session as soon as this user service runs in it.
        let session = display_session()?;
        let service = service(&session)?;
        if !automatic_login(&service) {
            println!("Not an automatic login ({service}); leaving the session as it is");
            return Ok(());
        }
    }
    let mut requested: Option<Instant> = None;
    loop {
        let session = display_session().ok();
        let state = session.as_deref().and_then(|id| locked(id).ok());
        match lock_step(
            state,
            requested.map(|at| at.elapsed()),
            Instant::now() >= deadline,
        ) {
            LockStep::Done => {
                println!("Session locked");
                return Ok(());
            }
            LockStep::Fail => anyhow::bail!("the session did not lock within {timeout:?}"),
            LockStep::Lock => {
                if let Some(id) = &session
                    && let Err(error) = lock(id)
                {
                    tracing::warn!(
                        error = format!("{error:#}"),
                        "lock request failed; retrying"
                    );
                }
                requested = Some(Instant::now());
            }
            LockStep::Wait => (),
        }
        std::thread::sleep(Duration::from_millis(250));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_ids_come_from_logind_and_are_checked() {
        assert_eq!(choose_session("2\n", Some("5")).as_deref(), Some("2"));
        assert_eq!(choose_session("", Some("c1")).as_deref(), Some("c1"));
        assert_eq!(choose_session("", None), None);
        assert_eq!(choose_session("1; rm -rf /", None), None);
        // Never something loginctl would read as an option.
        assert_eq!(choose_session("--all", Some("3")).as_deref(), Some("3"));
        assert!(!valid_session("a b") && !valid_session("") && valid_session("c_1-2"));
        assert!(lock("--all").is_err() && unlock("-h").is_err());
        assert!(parse_locked_hint("yes\n").unwrap());
        assert!(!parse_locked_hint("no").unwrap());
        assert!(parse_locked_hint("").is_err());
    }

    #[test]
    fn kde_preauthorization_needs_yes_for_the_host() {
        let app = "dev.teleport.Host";
        let entry = |pairs: &[(&str, &[&str])]| -> HashMap<String, Vec<String>> {
            pairs
                .iter()
                .map(|(id, values)| {
                    (
                        (*id).to_owned(),
                        values.iter().map(|value| (*value).to_owned()).collect(),
                    )
                })
                .collect()
        };
        assert!(preauthorized(&entry(&[(app, &["yes"])]), app));
        assert!(!preauthorized(&entry(&[(app, &["no"])]), app));
        assert!(!preauthorized(&entry(&[(app, &[])]), app));
        assert!(!preauthorized(
            &entry(&[("org.kde.krdpserver", &["yes"])]),
            app
        ));
        assert!(!preauthorized(&entry(&[]), app));
    }

    #[test]
    fn only_automatic_logins_are_locked_on_request() {
        for service in ["sddm-autologin", "gdm-autologin\n", "lightdm-autologin"] {
            assert!(automatic_login(service), "{service}");
        }
        for service in ["sddm", "login", "sshd", "", "autologin-helper"] {
            assert!(!automatic_login(service), "{service}");
        }
    }

    #[test]
    fn secure_attention_locks_or_prompts() {
        assert_eq!(attention_for(false), Attention::Lock);
        assert_eq!(attention_for(true), Attention::Prompt);
    }

    #[test]
    fn lock_after_login_retries_until_locked_or_out_of_time() {
        let second = Some(Duration::from_secs(1));
        let later = Some(Duration::from_secs(3));
        // No session yet (logind or the desktop still starting): wait.
        assert_eq!(lock_step(None, None, false), LockStep::Wait);
        // Unlocked: ask, then wait for the locker, then ask again.
        assert_eq!(lock_step(Some(false), None, false), LockStep::Lock);
        assert_eq!(lock_step(Some(false), second, false), LockStep::Wait);
        assert_eq!(lock_step(Some(false), later, false), LockStep::Lock);
        assert_eq!(lock_step(Some(true), second, false), LockStep::Done);
        // Locked is success even at the deadline; unlocked then fails.
        assert_eq!(lock_step(Some(true), None, true), LockStep::Done);
        assert_eq!(lock_step(Some(false), later, true), LockStep::Fail);
        assert_eq!(lock_step(None, None, true), LockStep::Fail);
    }
}
