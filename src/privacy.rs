//! Privacy mode: while a client session lasts, the host's physical monitors
//! are off and the session streams a virtual output (KDE). See
//! docs/virtual-display.md.
//!
//! Restoring never depends on the host staying healthy:
//! - the outputs to restore are saved to a private state file *before* any
//!   is turned off;
//! - a guard process (`teleport privacy-guard`) restores them when the host
//!   exits, crashes, is killed or stops sending heartbeats, and on SIGTERM;
//! - `teleport privacy-restore` (also the NixOS unit's `ExecStopPost`, and
//!   run at host start) restores from the state file;
//! - KWin itself turns them back on when the virtual output disappears,
//!   since it applies its stored layout for the remaining outputs.
use crate::virtual_display::{self, Output, Saved};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{
    io::{Read, Write},
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::{Path, PathBuf},
    time::Duration,
};
use tokio::io::AsyncWriteExt;

/// `--privacy-mode`.
#[derive(clap::ValueEnum, Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum PrivacyMode {
    /// Never turn the host's monitors off.
    #[default]
    Off,
    /// For sessions whose client asks (`teleport client --privacy`).
    Request,
    /// For every session.
    Always,
}

/// The guard restores the monitors when heartbeats stop for this long (a
/// hung host). Longer than the host's slowest portal recovery.
pub const GUARD_TIMEOUT: Duration = Duration::from_secs(45);
const HEARTBEAT: Duration = Duration::from_secs(1);

#[derive(Debug, Deserialize, Serialize)]
struct State {
    version: u32,
    outputs: Vec<Saved>,
}

/// The host's state file: in its identity directory, else the runtime
/// directory (per listen port, so test hosts never share it).
pub fn state_path(identity_dir: Option<&Path>, listen: &str) -> PathBuf {
    match identity_dir {
        Some(directory) => directory.join("privacy-restore.json"),
        None => {
            let port = listen.rsplit(':').next().unwrap_or("0");
            let port: String = port.chars().filter(char::is_ascii_digit).collect();
            std::env::var_os("XDG_RUNTIME_DIR")
                .map(PathBuf::from)
                .unwrap_or_else(std::env::temp_dir)
                .join(format!("teleport-privacy-{port}.json"))
        }
    }
}

/// The NixOS module's identity directory, for `teleport privacy-restore`.
pub fn default_state_path() -> PathBuf {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_default();
    home.join(".local/state/teleport/host/privacy-restore.json")
}

fn write_state(path: &Path, state: &State) -> Result<()> {
    let parent = path
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let temporary = parent.join(format!(".teleport-privacy-{:032x}", rand::random::<u128>()));
    let result = (|| -> Result<()> {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temporary)?;
        file.write_all(&serde_json::to_vec_pretty(state)?)?;
        file.sync_all()?;
        std::fs::rename(&temporary, path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    result.context("saving the privacy-mode restore state")
}

fn read_state(path: &Path) -> Result<Option<State>> {
    let file = match std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
    {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).context("opening the privacy-mode restore state"),
    };
    let metadata = file.metadata()?;
    ensure!(
        metadata.is_file() && metadata.uid() == unsafe { libc::geteuid() },
        "privacy-mode restore state must be a regular file owned by this user"
    );
    let mut data = Vec::new();
    file.take(1 << 20).read_to_end(&mut data)?;
    Ok(Some(
        serde_json::from_slice(&data).context("parsing the privacy-mode restore state")?,
    ))
}

/// Turn the outputs in the state file back on and delete the file. Without
/// a state file there is nothing to do (`Ok(false)`). Idempotent; the file
/// stays if an output could not be restored, so a later run can retry.
pub fn restore(path: &Path) -> Result<bool> {
    let Some(state) = read_state(path)? else {
        return Ok(false);
    };
    let current = virtual_display::outputs()?;
    virtual_display::apply(&virtual_display::restore_args(&state.outputs, &current))?;
    let after = virtual_display::outputs()?;
    let still_off: Vec<_> = state
        .outputs
        .iter()
        .filter(|saved| {
            after
                .iter()
                .any(|output| output.name == saved.name && output.connected && !output.enabled)
        })
        .map(|saved| saved.name.as_str())
        .collect();
    ensure!(
        still_off.is_empty(),
        "monitors still off: {}",
        still_off.join(", ")
    );
    match std::fs::remove_file(path) {
        Err(error) if error.kind() != std::io::ErrorKind::NotFound => {
            return Err(error).context("removing the privacy-mode restore state");
        }
        _ => {}
    }
    tracing::info!(
        monitors = state.outputs.len(),
        "restored the host's monitors after privacy mode"
    );
    Ok(true)
}

/// `teleport privacy-restore --all`: turn on every connected physical output
/// that is off (an escape hatch when no state file survived).
pub fn enable_all() -> Result<usize> {
    let outputs = virtual_display::outputs()?;
    let args: Vec<_> = outputs
        .iter()
        .filter(|output| output.is_physical() && !output.enabled)
        .map(|output| format!("output.{}.enable", output.id))
        .collect();
    virtual_display::apply(&args)?;
    Ok(args.len())
}

/// One session's privacy mode. Restores the monitors when dropped, however
/// the session ends.
pub struct Privacy {
    path: PathBuf,
    saved: Vec<Saved>,
    guard: Option<tokio::process::Child>,
    heartbeat: Option<tokio::task::JoinHandle<()>>,
    restored: bool,
}

impl Privacy {
    /// Save the physical outputs and start the guard; nothing is off yet.
    pub async fn begin(path: PathBuf) -> Result<Self> {
        let outputs = blocking(virtual_display::outputs).await?;
        let saved: Vec<_> = virtual_display::privacy_targets(&outputs)
            .into_iter()
            .map(Saved::of)
            .collect();
        write_state(
            &path,
            &State {
                version: 1,
                outputs: saved.clone(),
            },
        )?;
        let mut privacy = Self {
            path,
            saved,
            guard: None,
            heartbeat: None,
            restored: false,
        };
        let mut guard = tokio::process::Command::new(std::env::current_exe()?)
            .arg("privacy-guard")
            .arg("--state")
            .arg(&privacy.path)
            .arg("--timeout-secs")
            // Tests shorten it to see a hung host restored.
            .arg(
                std::env::var("TELEPORT_PRIVACY_GUARD_SECS")
                    .ok()
                    .and_then(|value| value.parse::<u64>().ok())
                    .unwrap_or(GUARD_TIMEOUT.as_secs())
                    .to_string(),
            )
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .spawn()
            .context("starting the privacy-mode guard")?;
        let mut stdin = guard.stdin.take().context("guard stdin")?;
        privacy.guard = Some(guard);
        privacy.heartbeat = Some(tokio::spawn(async move {
            let mut tick = tokio::time::interval(HEARTBEAT);
            loop {
                tick.tick().await;
                if stdin.write_all(b".").await.is_err() {
                    break;
                }
            }
        }));
        Ok(privacy)
    }

    /// Turn off every enabled physical output, including ones that appeared
    /// since (a monitor plugged in or waking up), remembering how they were.
    /// Call while the virtual output streams. Returns how many were turned off.
    pub async fn enforce(&mut self) -> Result<usize> {
        let outputs = blocking(virtual_display::outputs).await?;
        let targets = virtual_display::privacy_targets(&outputs);
        if targets.is_empty() {
            return Ok(0);
        }
        let mut changed = false;
        for output in &targets {
            if !self.saved.iter().any(|saved| saved.name == output.name) {
                self.saved.push(Saved::of(output));
                changed = true;
            }
        }
        if changed {
            write_state(
                &self.path,
                &State {
                    version: 1,
                    outputs: self.saved.clone(),
                },
            )?;
        }
        let args = virtual_display::disable_args(&targets);
        let count = targets.len();
        blocking(move || virtual_display::apply(&args)).await?;
        let after = blocking(virtual_display::outputs).await?;
        let on: Vec<_> = virtual_display::privacy_targets(&after)
            .into_iter()
            .map(|output: &Output| output.name.clone())
            .collect();
        ensure!(
            on.is_empty(),
            "the compositor kept monitors on: {}",
            on.join(", ")
        );
        Ok(count)
    }

    /// The outputs privacy mode turned off (for logs).
    pub fn monitors(&self) -> usize {
        self.saved.len()
    }

    /// Turn the monitors back on now (the virtual output should still exist,
    /// so KWin stores the layout with them on).
    pub async fn end(mut self) -> Result<()> {
        let path = self.path.clone();
        let result = blocking(move || restore(&path)).await;
        self.finish();
        result.map(drop)
    }

    /// Stop the heartbeats: the guard sees EOF and runs a restore, which is
    /// a no-op once the state file is gone, or retries a failed one.
    fn finish(&mut self) {
        self.restored = true;
        if let Some(heartbeat) = self.heartbeat.take() {
            heartbeat.abort();
        }
        // Never killed or waited for: it must outlive a failed restore.
        drop(self.guard.take());
    }
}

impl Drop for Privacy {
    fn drop(&mut self) {
        if self.restored {
            return;
        }
        // A cancelled session (shutdown, revoked access): restore inline.
        if let Err(error) = restore(&self.path) {
            tracing::error!(
                error = format!("{error:#}"),
                "could not restore the host's monitors; the privacy guard retries"
            );
        }
        self.finish();
    }
}

async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> Result<T> + Send + 'static,
) -> Result<T> {
    tokio::task::spawn_blocking(work)
        .await
        .context("output control task failed")?
}

/// `teleport privacy-guard`: wait for the host's heartbeats on stdin; on
/// EOF (host gone), silence, SIGTERM, SIGINT or SIGHUP restore the monitors
/// from `state`.
pub fn guard(state: &Path, timeout: Duration) -> Result<()> {
    guard_from(state, timeout, std::io::stdin(), true)
}

fn guard_from(
    state: &Path,
    timeout: Duration,
    mut input: impl Read + Send + 'static,
    signals: bool,
) -> Result<()> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let reason = runtime.block_on(async {
        use tokio::signal::unix::{SignalKind, signal};
        let (sender, mut beats) = tokio::sync::mpsc::unbounded_channel();
        std::thread::spawn(move || {
            let mut buffer = [0u8; 64];
            loop {
                let alive = matches!(input.read(&mut buffer), Ok(count) if count > 0);
                if sender.send(alive).is_err() || !alive {
                    break;
                }
            }
        });
        let mut handlers = Vec::new();
        if signals {
            for kind in [
                SignalKind::terminate(),
                SignalKind::interrupt(),
                SignalKind::hangup(),
            ] {
                handlers.push(signal(kind)?);
            }
        }
        let signalled = async {
            match handlers.as_mut_slice() {
                [terminate, interrupt, hangup] => {
                    tokio::select! {
                        _ = terminate.recv() => "terminated",
                        _ = interrupt.recv() => "interrupted",
                        _ = hangup.recv() => "hangup",
                    }
                }
                _ => std::future::pending().await,
            }
        };
        tokio::pin!(signalled);
        Ok::<_, anyhow::Error>(loop {
            tokio::select! {
                beat = tokio::time::timeout(timeout, beats.recv()) => match beat {
                    Err(_) => break "no heartbeat from the host",
                    Ok(Some(true)) => {}
                    Ok(Some(false) | None) => break "host exited",
                },
                reason = &mut signalled => break reason,
            }
        })
    })?;
    drop(runtime);
    if signals {
        // Finish restoring whatever else arrives (systemd stops the unit).
        unsafe {
            libc::signal(libc::SIGTERM, libc::SIG_IGN);
            libc::signal(libc::SIGINT, libc::SIG_IGN);
            libc::signal(libc::SIGHUP, libc::SIG_IGN);
        }
    }
    let mut attempt = 0;
    loop {
        attempt += 1;
        match restore(state) {
            Ok(true) => {
                tracing::warn!(reason, "privacy guard restored the host's monitors");
                return Ok(());
            }
            Ok(false) => return Ok(()),
            Err(error) if attempt < 10 => {
                tracing::warn!(
                    reason,
                    attempt,
                    error = format!("{error:#}"),
                    "privacy guard could not restore the monitors yet"
                );
                std::thread::sleep(Duration::from_secs(1));
            }
            Err(error) => {
                return Err(error.context("privacy guard could not restore the monitors"));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn state_is_private_and_rejects_symlinks() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("privacy-restore.json");
        assert!(read_state(&path).unwrap().is_none());
        let saved = Saved {
            name: "DP-1".into(),
            pos: virtual_display::Point { x: 0, y: 0 },
            mode: Some("2560x1440@144".into()),
            scale: 1.0,
            priority: 1,
        };
        write_state(
            &path,
            &State {
                version: 1,
                outputs: vec![saved.clone()],
            },
        )
        .unwrap();
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(read_state(&path).unwrap().unwrap().outputs, [saved]);
        let link = directory.path().join("link.json");
        std::os::unix::fs::symlink(&path, &link).unwrap();
        assert!(read_state(&link).is_err());
    }

    #[test]
    fn state_paths_follow_the_identity_directory_or_the_listen_port() {
        assert_eq!(
            state_path(Some(Path::new("/id")), "0.0.0.0:4443"),
            Path::new("/id/privacy-restore.json")
        );
        let path = state_path(None, "127.0.0.1:47123");
        assert!(
            path.ends_with("teleport-privacy-47123.json"),
            "{}",
            path.display()
        );
        let path = state_path(None, "[::1]:5000");
        assert!(
            path.ends_with("teleport-privacy-5000.json"),
            "{}",
            path.display()
        );
    }

    /// A fake `kscreen-doctor` over a JSON file (one output per line), so
    /// the restore paths run without a compositor.
    fn fake_kscreen(directory: &Path, outputs: &str) -> PathBuf {
        let state = directory.join("outputs.json");
        std::fs::write(&state, outputs).unwrap();
        let script = directory.join("kscreen-doctor");
        let body = r#"#!/bin/sh
state='@STATE@'
if [ "$1" = --json ]; then cat "$state"; exit 0; fi
if [ "$1" = --outputs ]; then exit 0; fi
echo "$@" >> '@CALLS@'
for arg in "$@"; do
  case "$arg" in
    output.*.enable) id=${arg#output.}; id=${id%.enable}; from=false; to=true;;
    output.*.disable) id=${arg#output.}; id=${id%.disable}; from=true; to=false;;
    *) continue;;
  esac
  sed -i "s/\"id\":$id,\"name\":\([^,]*\),\"enabled\":$from/\"id\":$id,\"name\":\1,\"enabled\":$to/" "$state"
done
"#
        .replace("@STATE@", &state.display().to_string())
        .replace("@CALLS@", &directory.join("calls").display().to_string());
        std::fs::write(&script, body).unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        script
    }

    const OUTPUTS: &str = r#"{"outputs":[
{"id":1,"name":"DP-1","enabled":true,"connected":true,"pos":{"x":0,"y":0},"currentModeId":"1","modes":[{"id":"1","name":"2560x1440@144","refreshRate":144,"size":{"width":2560,"height":1440}}],"scale":1,"priority":1},
{"id":2,"name":"Virtual-virtual-xdp-kde-dev.teleport.Host","enabled":true,"connected":true,"pos":{"x":2560,"y":0},"currentModeId":"2","modes":[],"scale":1,"priority":2}
]}"#;

    fn session(state: &Path) -> Privacy {
        Privacy {
            path: state.to_owned(),
            saved: Vec::new(),
            guard: None,
            heartbeat: None,
            restored: false,
        }
    }

    /// As if the host died with the monitors off: only the state file is left.
    async fn crashed(state: &Path) {
        let mut privacy = session(state);
        assert_eq!(privacy.enforce().await.unwrap(), 1);
        privacy.restored = true;
        drop(privacy);
        assert!(state.exists());
    }

    // The only test using TELEPORT_KSCREEN_DOCTOR (process-wide).
    #[tokio::test(flavor = "multi_thread")]
    async fn privacy_turns_monitors_off_and_every_exit_path_restores_them() {
        let directory = tempfile::tempdir().unwrap();
        let fake = fake_kscreen(directory.path(), OUTPUTS);
        // SAFETY: no other test reads or writes this variable.
        unsafe { std::env::set_var("TELEPORT_KSCREEN_DOCTOR", &fake) };
        let state = directory.path().join("privacy-restore.json");
        let enabled = || {
            let outputs = virtual_display::outputs().unwrap();
            (outputs[0].enabled, outputs[1].enabled)
        };

        // A normal end: off for the session, on afterwards, state removed.
        let mut privacy = session(&state);
        assert_eq!(privacy.enforce().await.unwrap(), 1);
        assert_eq!(enabled(), (false, true), "the virtual output stays on");
        assert!(state.exists(), "saved before anything is turned off");
        assert_eq!(privacy.enforce().await.unwrap(), 0, "nothing left on");
        privacy.end().await.unwrap();
        assert_eq!(enabled(), (true, true));
        assert!(!state.exists());
        let calls = std::fs::read_to_string(directory.path().join("calls")).unwrap();
        assert!(calls.contains("output.1.disable"), "{calls}");
        assert!(
            calls.contains("output.1.enable output.1.position.0,0 output.1.mode.1 output.1.scale.1 output.1.priority.1"),
            "{calls}"
        );

        // A dropped (cancelled) session restores inline.
        let mut privacy = session(&state);
        privacy.enforce().await.unwrap();
        tokio::task::block_in_place(|| drop(privacy));
        assert_eq!(enabled(), (true, true));
        assert!(!state.exists());

        // A crashed host: `privacy-restore` (also run at the next host start
        // and by the unit's ExecStopPost) turns them back on, once.
        crashed(&state).await;
        assert_eq!(enabled(), (false, true));
        assert!(restore(&state).unwrap());
        assert_eq!(enabled(), (true, true));
        assert!(!restore(&state).unwrap(), "idempotent");

        // The guard restores when the host's pipe closes...
        crashed(&state).await;
        let guarded = state.clone();
        tokio::task::spawn_blocking(move || {
            guard_from(&guarded, Duration::from_secs(5), std::io::empty(), false)
        })
        .await
        .unwrap()
        .unwrap();
        assert_eq!(enabled(), (true, true));
        assert!(!state.exists());

        // ... and when heartbeats stop (a hung host).
        crashed(&state).await;
        let (reader, writer) = std::os::unix::net::UnixStream::pair().unwrap();
        let started = std::time::Instant::now();
        let guarded = state.clone();
        tokio::task::spawn_blocking(move || {
            guard_from(&guarded, Duration::from_millis(300), reader, false)
        })
        .await
        .unwrap()
        .unwrap();
        assert!(started.elapsed() >= Duration::from_millis(300));
        assert_eq!(enabled(), (true, true));
        drop(writer);

        // Heartbeats keep it waiting; the host's own restore leaves the
        // guard nothing to do when the pipe then closes.
        crashed(&state).await;
        let (reader, mut writer) = std::os::unix::net::UnixStream::pair().unwrap();
        let guarded = state.clone();
        let guard = tokio::task::spawn_blocking(move || {
            guard_from(&guarded, Duration::from_millis(400), reader, false)
        });
        for _ in 0..4 {
            tokio::time::sleep(Duration::from_millis(200)).await;
            writer.write_all(b".").unwrap();
            assert_eq!(enabled(), (false, true), "kept off while the host lives");
        }
        assert!(restore(&state).unwrap());
        drop(writer);
        guard.await.unwrap().unwrap();
        assert_eq!(enabled(), (true, true));
        // SAFETY: as above.
        unsafe { std::env::remove_var("TELEPORT_KSCREEN_DOCTOR") };
    }
}
