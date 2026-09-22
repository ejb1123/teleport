use crate::protocol::{Event, Monitor};
use anyhow::{Context, Result, ensure};
use ashpd::desktop::{
    PersistMode, Session,
    remote_desktop::{Axis, DeviceType, KeyState, RemoteDesktop},
    screencast::{CursorMode, Screencast, SourceType},
};
use std::{
    collections::BTreeSet,
    io::{Read, Write},
    os::fd::{AsRawFd, OwnedFd},
    os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
    path::Path,
};
use x11rb::{
    connection::Connection,
    protocol::{randr::ConnectionExt as _, xproto, xtest::ConnectionExt},
};

pub struct Capture {
    pub width: u32,
    pub height: u32,
    pub name: &'static str,
    pub monitors: Vec<Monitor>,
    pub active_monitor: usize,
    backend: Backend,
    keys: BTreeSet<u16>,
    buttons: BTreeSet<u8>,
}

enum Backend {
    Portal {
        proxy: RemoteDesktop<'static>,
        session: Session<'static, RemoteDesktop<'static>>,
        fd: OwnedFd,
        nodes: Vec<u32>,
    },
    X11 {
        connection: Box<x11rb::rust_connection::RustConnection>,
        root: u32,
        origins: Vec<(i16, i16)>,
    },
    Test,
}

impl Capture {
    #[cfg(test)]
    pub async fn open(source: &str) -> Result<Self> {
        Self::open_with_options(source, None, 0).await
    }

    pub async fn open_with_options(
        source: &str,
        restore_token_path: Option<&Path>,
        monitor: usize,
    ) -> Result<Self> {
        let mut result = Self {
            width: 1280,
            height: 720,
            name: "test",
            monitors: vec![
                Monitor {
                    id: 0,
                    name: "Test landscape".into(),
                    width: 1280,
                    height: 720,
                },
                Monitor {
                    id: 1,
                    name: "Test portrait".into(),
                    width: 720,
                    height: 1280,
                },
            ],
            active_monitor: 0,
            backend: Backend::Test,
            keys: BTreeSet::new(),
            buttons: BTreeSet::new(),
        };
        let source = match source {
            "auto"
                if std::env::var("XDG_SESSION_TYPE").as_deref() == Ok("wayland")
                    || std::env::var_os("WAYLAND_DISPLAY").is_some() =>
            {
                "portal"
            }
            "auto" => "x11",
            value => value,
        };
        match source {
            "test" => (),
            "portal" => {
                let proxy = RemoteDesktop::new().await.context(
                    "RemoteDesktop portal unavailable; run inside your graphical session",
                )?;
                let cast = Screencast::new().await?;
                let session = proxy.create_session().await?;
                let restore_token = restore_token_path
                    .map(read_restore_token)
                    .transpose()?
                    .flatten();
                proxy
                    .select_devices(
                        &session,
                        DeviceType::Keyboard | DeviceType::Pointer,
                        restore_token.as_deref(),
                        if restore_token_path.is_some() {
                            PersistMode::ExplicitlyRevoked
                        } else {
                            PersistMode::DoNot
                        },
                    )
                    .await?
                    .response()?;
                cast.select_sources(
                    &session,
                    CursorMode::Embedded,
                    SourceType::Monitor.into(),
                    true,
                    None,
                    PersistMode::DoNot,
                )
                .await?
                .response()?;
                tracing::info!(
                    "Select monitors and approve desktop control in the local portal dialog; persistence depends on your compositor"
                );
                let response = proxy.start(&session, None).await?.response()?;
                ensure!(
                    response.devices().contains(DeviceType::Keyboard)
                        && response.devices().contains(DeviceType::Pointer),
                    "keyboard and pointer permission are required"
                );
                let streams = response.streams().context("portal returned no monitor")?;
                ensure!(!streams.is_empty(), "portal returned no monitor");
                result.monitors = streams
                    .iter()
                    .enumerate()
                    .map(|(id, stream)| {
                        let (width, height) = stream
                            .size()
                            .context("portal did not report logical monitor size")?;
                        ensure!(width > 0 && height > 0, "invalid monitor size");
                        Ok(Monitor {
                            id,
                            name: stream
                                .id()
                                .map(str::to_owned)
                                .unwrap_or_else(|| format!("Monitor {}", id + 1)),
                            width: width as u32,
                            height: height as u32,
                        })
                    })
                    .collect::<Result<Vec<_>>>()?;
                let nodes = streams
                    .iter()
                    .map(|stream| stream.pipe_wire_node_id())
                    .collect();
                if let Some(path) = restore_token_path {
                    if let Some(token) = response.restore_token() {
                        write_restore_token(path, token)?;
                    } else {
                        tracing::warn!(
                            "Portal did not grant persistent access; future starts may require local approval"
                        );
                    }
                }
                let fd = cast.open_pipe_wire_remote(&session).await?;
                result.name = "Wayland portal";
                result.backend = Backend::Portal {
                    proxy,
                    session,
                    fd,
                    nodes,
                };
            }
            "x11" => {
                ensure!(
                    std::env::var("XDG_SESSION_TYPE").as_deref() != Ok("wayland")
                        && std::env::var_os("WAYLAND_DISPLAY").is_none(),
                    "X11 capture cannot share a Wayland session; use --source portal"
                );
                let (connection, screen) = x11rb::connect(None)?;
                connection.xtest_get_version(2, 2)?.reply()?;
                let screen = &connection.setup().roots[screen];
                result.width = screen.width_in_pixels as u32;
                result.height = screen.height_in_pixels as u32;
                let root = screen.root;
                let (monitors, origins) =
                    x11_monitors(&connection, root, result.width, result.height);
                result.monitors = monitors;
                result.backend = Backend::X11 {
                    connection: Box::new(connection),
                    root,
                    origins,
                };
                result.name = "X11";
            }
            _ => anyhow::bail!("unknown capture source"),
        }
        result.select_monitor(monitor).await?;
        Ok(result)
    }

    /// Switching updates capture and input coordinates; callers must rebuild the video pipeline.
    pub async fn select_monitor(&mut self, index: usize) -> Result<()> {
        let monitor = self
            .monitors
            .get(index)
            .context("monitor index is out of range")?;
        let (width, height) = (monitor.width, monitor.height);
        self.release_all().await;
        // A new GStreamer pipeline creates a new PipeWire core. Obtain a fresh
        // authorized socket rather than replaying the core handshake on a socket
        // whose previous core has already disconnected.
        if let Backend::Portal { session, fd, .. } = &mut self.backend {
            *fd = Screencast::new()
                .await?
                .open_pipe_wire_remote(session)
                .await?;
        }
        self.active_monitor = index;
        self.width = width;
        self.height = height;
        Ok(())
    }

    pub fn pipeline_source(&self) -> String {
        match &self.backend {
            Backend::Test => format!(
                "videotestsrc is-live=true pattern={}",
                if self.active_monitor == 0 {
                    "ball"
                } else {
                    "smpte"
                }
            ),
            Backend::Portal { fd, nodes, .. } => format!(
                "pipewiresrc fd={} path={} do-timestamp=true ! video/x-raw",
                fd.as_raw_fd(),
                nodes[self.active_monitor]
            ),
            Backend::X11 { origins, .. } => {
                let (x, y) = origins[self.active_monitor];
                format!(
                    "ximagesrc use-damage=false show-pointer=true startx={x} starty={y} endx={} endy={}",
                    u32::from(x as u16) + self.width - 1,
                    u32::from(y as u16) + self.height - 1
                )
            }
        }
    }

    pub fn pipeline_source_for_range(
        &self,
        range: crate::protocol::DynamicRange,
    ) -> Result<String> {
        if range == crate::protocol::DynamicRange::Sdr {
            return Ok(self.pipeline_source());
        }
        ensure!(
            !matches!(self.backend, Backend::X11 { .. }),
            "HDR capture requires a compatible Wayland compositor; X11 capture is SDR"
        );
        // Require real high-precision PQ RGB, never relabel an 8-bit source.
        // Test mode generates synthetic PQ code values, not desktop capture.
        Ok(format!(
            "{} ! capsfilter name=hdr_source caps=video/x-raw,format=RGB10A2_LE,colorimetry=1:1:14:7",
            self.pipeline_source()
        ))
    }

    async fn key(&self, code: u16, down: bool) -> Result<()> {
        match &self.backend {
            Backend::Portal { proxy, session, .. } => {
                proxy
                    .notify_keyboard_keycode(session, code as i32, state(down))
                    .await?
            }
            Backend::X11 {
                connection, root, ..
            } => {
                connection
                    .xtest_fake_input(
                        if down {
                            xproto::KEY_PRESS_EVENT
                        } else {
                            xproto::KEY_RELEASE_EVENT
                        },
                        (code + 8) as u8,
                        0,
                        *root,
                        0,
                        0,
                        0,
                    )?
                    .check()?;
            }
            Backend::Test => (),
        }
        Ok(())
    }

    async fn button(&self, button: u8, down: bool) -> Result<()> {
        match &self.backend {
            Backend::Portal { proxy, session, .. } => {
                let code = match button {
                    1 => 272,
                    2 => 274,
                    _ => 273,
                };
                proxy
                    .notify_pointer_button(session, code, state(down))
                    .await?;
            }
            Backend::X11 {
                connection, root, ..
            } => {
                connection
                    .xtest_fake_input(
                        if down {
                            xproto::BUTTON_PRESS_EVENT
                        } else {
                            xproto::BUTTON_RELEASE_EVENT
                        },
                        button,
                        0,
                        *root,
                        0,
                        0,
                        0,
                    )?
                    .check()?;
            }
            Backend::Test => (),
        }
        Ok(())
    }

    pub async fn input(&mut self, event: Event) -> Result<()> {
        event.validate()?;
        match event {
            Event::Key { code, down } => {
                if down {
                    if self.keys.insert(code) {
                        self.key(code, true).await?;
                    }
                } else if self.keys.contains(&code) {
                    self.key(code, false).await?;
                    self.keys.remove(&code);
                }
            }
            Event::Button { button, down } => {
                if down {
                    if self.buttons.insert(button) {
                        self.button(button, true).await?;
                    }
                } else if self.buttons.contains(&button) {
                    self.button(button, false).await?;
                    self.buttons.remove(&button);
                }
            }
            Event::Motion { x, y } => {
                let x = x * (self.width - 1) as f64;
                let y = y * (self.height - 1) as f64;
                match &self.backend {
                    Backend::Portal {
                        proxy,
                        session,
                        nodes,
                        ..
                    } => {
                        proxy
                            .notify_pointer_motion_absolute(
                                session,
                                nodes[self.active_monitor],
                                x,
                                y,
                            )
                            .await?
                    }
                    Backend::X11 {
                        connection,
                        root,
                        origins,
                    } => {
                        let (origin_x, origin_y) = origins[self.active_monitor];
                        connection
                            .xtest_fake_input(
                                xproto::MOTION_NOTIFY_EVENT,
                                0,
                                0,
                                *root,
                                (x as i32 + i32::from(origin_x)) as i16,
                                (y as i32 + i32::from(origin_y)) as i16,
                                0,
                            )?
                            .check()?;
                    }
                    Backend::Test => (),
                }
            }
            Event::Scroll { x, y } => match &self.backend {
                Backend::Portal { proxy, session, .. } => {
                    if y != 0 {
                        proxy
                            .notify_pointer_axis_discrete(session, Axis::Vertical, -y)
                            .await?;
                    }
                    if x != 0 {
                        proxy
                            .notify_pointer_axis_discrete(session, Axis::Horizontal, x)
                            .await?;
                    }
                }
                Backend::X11 {
                    connection, root, ..
                } => {
                    for (count, button) in [
                        (y.abs(), if y > 0 { 4 } else { 5 }),
                        (x.abs(), if x > 0 { 7 } else { 6 }),
                    ] {
                        for _ in 0..count {
                            connection
                                .xtest_fake_input(
                                    xproto::BUTTON_PRESS_EVENT,
                                    button,
                                    0,
                                    *root,
                                    0,
                                    0,
                                    0,
                                )?
                                .check()?;
                            connection
                                .xtest_fake_input(
                                    xproto::BUTTON_RELEASE_EVENT,
                                    button,
                                    0,
                                    *root,
                                    0,
                                    0,
                                    0,
                                )?
                                .check()?;
                        }
                    }
                }
                Backend::Test => (),
            },
            Event::ReleaseAll => self.release_all().await,
            Event::Ping => (),
            // Host-level controls are handled before injection.
            Event::SelectMonitor { .. }
            | Event::Probe { .. }
            | Event::ConfigureVideo { .. }
            | Event::Feedback { .. }
            | Event::Clipboard { .. }
            | Event::ClipboardRequest => (),
        }
        Ok(())
    }

    pub async fn release_all(&mut self) {
        for code in std::mem::take(&mut self.keys) {
            if let Err(error) = self.key(code, false).await {
                tracing::warn!(%error, "key release failed");
            }
        }
        for button in std::mem::take(&mut self.buttons) {
            if let Err(error) = self.button(button, false).await {
                tracing::warn!(%error, "button release failed");
            }
        }
    }

    pub async fn close(&mut self) {
        self.release_all().await;
        if let Backend::Portal { session, .. } = &self.backend {
            let _ = session.close().await;
        }
    }
}

fn state(down: bool) -> KeyState {
    if down {
        KeyState::Pressed
    } else {
        KeyState::Released
    }
}

fn x11_monitors(
    connection: &x11rb::rust_connection::RustConnection,
    root: u32,
    width: u32,
    height: u32,
) -> (Vec<Monitor>, Vec<(i16, i16)>) {
    use x11rb::protocol::xproto::ConnectionExt as _;
    let detected = (|| -> Result<_> {
        let version = connection.randr_query_version(1, 5)?.reply()?;
        ensure!(
            (version.major_version, version.minor_version) >= (1, 5),
            "RandR 1.5 unavailable"
        );
        let reply = connection.randr_get_monitors(root, true)?.reply()?;
        let mut monitors = Vec::new();
        let mut origins = Vec::new();
        for item in reply.monitors {
            // XImage regions and XTest absolute positions must fit root coordinates.
            if item.width == 0
                || item.height == 0
                || item.x < 0
                || item.y < 0
                || u32::from(item.x as u16) + u32::from(item.width) > width
                || u32::from(item.y as u16) + u32::from(item.height) > height
            {
                continue;
            }
            let id = monitors.len();
            let atom = connection.get_atom_name(item.name)?.reply()?;
            monitors.push(Monitor {
                id,
                name: String::from_utf8_lossy(&atom.name).into_owned(),
                width: item.width.into(),
                height: item.height.into(),
            });
            origins.push((item.x, item.y));
        }
        ensure!(!monitors.is_empty(), "RandR returned no valid monitors");
        Ok((monitors, origins))
    })();
    detected.unwrap_or_else(|error| {
        tracing::debug!(%error, "Using whole X11 desktop as the single capture monitor");
        (
            vec![Monitor {
                id: 0,
                name: "X11 desktop".into(),
                width,
                height,
            }],
            vec![(0, 0)],
        )
    })
}

// Linux-only module: O_NOFOLLOW prevents following a symlink to somebody else's token.
fn read_restore_token(path: &Path) -> Result<Option<String>> {
    let file = match std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
    {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).context("opening portal restore token"),
    };
    let metadata = file.metadata()?;
    ensure!(
        metadata.uid() == unsafe { libc::geteuid() },
        "portal restore token must be owned by the current user"
    );
    ensure!(
        metadata.is_file(),
        "portal restore token must be a regular file"
    );
    ensure!(
        metadata.permissions().mode() & 0o077 == 0,
        "portal restore token must be private (chmod 600)"
    );
    ensure!(metadata.len() <= 16_384, "portal restore token too large");
    let mut token = String::new();
    file.take(16_385).read_to_string(&mut token)?;
    ensure!(
        !token.is_empty() && token.len() <= 16_384 && !token.contains('\0'),
        "invalid portal restore token"
    );
    Ok(Some(token))
}

fn write_restore_token(path: &Path, token: &str) -> Result<()> {
    let parent = path
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let temporary = parent.join(format!(".teleport-restore-{:032x}", rand::random::<u128>()));
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&temporary)
        .context("creating private portal restore token; parent directory must exist")?;
    let result = (|| -> Result<()> {
        file.write_all(token.as_bytes())?;
        file.sync_all()?;
        std::fs::rename(&temporary, path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    result.context("saving portal restore token")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn monitor_selection_validates_and_updates_dimensions() {
        let mut capture = Capture::open("test").await.unwrap();
        capture.select_monitor(1).await.unwrap();
        assert_eq!(
            (capture.width, capture.height, capture.active_monitor),
            (720, 1280, 1)
        );
        assert!(capture.select_monitor(2).await.is_err());
        assert_eq!(capture.active_monitor, 1);
    }

    #[test]
    fn restore_token_is_private_atomic_and_rejects_symlinks() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("restore");
        assert!(read_restore_token(&path).unwrap().is_none());
        write_restore_token(&path, "first").unwrap();
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        write_restore_token(&path, "rotated").unwrap();
        assert_eq!(
            read_restore_token(&path).unwrap().as_deref(),
            Some("rotated")
        );
        let link = directory.path().join("symlink");
        std::os::unix::fs::symlink(&path, &link).unwrap();
        assert!(read_restore_token(&link).is_err());
        write_restore_token(&link, "replacement").unwrap();
        assert_eq!(
            read_restore_token(&path).unwrap().as_deref(),
            Some("rotated")
        );
        assert_eq!(
            read_restore_token(&link).unwrap().as_deref(),
            Some("replacement")
        );
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(read_restore_token(&path).is_err());
    }
}
