use crate::protocol::Event;
use anyhow::{Context, Result, ensure};
use ashpd::desktop::{
    PersistMode, Session,
    remote_desktop::{Axis, DeviceType, KeyState, RemoteDesktop},
    screencast::{CursorMode, Screencast, SourceType},
};
use std::{
    collections::BTreeSet,
    os::fd::{AsRawFd, OwnedFd},
};
use x11rb::{
    connection::Connection,
    protocol::{xproto, xtest::ConnectionExt},
};

pub struct Capture {
    pub width: u32,
    pub height: u32,
    pub name: &'static str,
    backend: Backend,
    keys: BTreeSet<u16>,
    buttons: BTreeSet<u8>,
}

enum Backend {
    Portal {
        proxy: RemoteDesktop<'static>,
        session: Session<'static, RemoteDesktop<'static>>,
        fd: OwnedFd,
        node: u32,
    },
    X11 {
        connection: Box<x11rb::rust_connection::RustConnection>,
        root: u32,
    },
    Test,
}

impl Capture {
    pub async fn open(source: &str) -> Result<Self> {
        let mut result = Self {
            width: 1280,
            height: 720,
            name: "test",
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
                proxy
                    .select_devices(
                        &session,
                        DeviceType::Keyboard | DeviceType::Pointer,
                        None,
                        PersistMode::DoNot,
                    )
                    .await?
                    .response()?;
                cast.select_sources(
                    &session,
                    CursorMode::Embedded,
                    SourceType::Monitor.into(),
                    false,
                    None,
                    PersistMode::DoNot,
                )
                .await?
                .response()?;
                tracing::info!(
                    "Select one monitor and approve desktop control in the local portal dialog"
                );
                let response = proxy.start(&session, None).await?.response()?;
                ensure!(
                    response.devices().contains(DeviceType::Keyboard)
                        && response.devices().contains(DeviceType::Pointer),
                    "keyboard and pointer permission are required"
                );
                let stream = response
                    .streams()
                    .and_then(|streams| streams.first())
                    .context("portal returned no monitor")?;
                let (width, height) = stream
                    .size()
                    .context("portal did not report logical monitor size")?;
                ensure!(width > 0 && height > 0, "invalid monitor size");
                result.width = width as u32;
                result.height = height as u32;
                let node = stream.pipe_wire_node_id();
                let fd = cast.open_pipe_wire_remote(&session).await?;
                result.name = "Wayland portal";
                result.backend = Backend::Portal {
                    proxy,
                    session,
                    fd,
                    node,
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
                result.backend = Backend::X11 {
                    connection: Box::new(connection),
                    root,
                };
                result.name = "X11";
            }
            _ => anyhow::bail!("unknown capture source"),
        }
        Ok(result)
    }

    pub fn pipeline_source(&self) -> String {
        match &self.backend {
            Backend::Test => "videotestsrc is-live=true pattern=ball".into(),
            Backend::Portal { fd, node, .. } => format!(
                "pipewiresrc fd={} path={node} do-timestamp=true ! video/x-raw",
                fd.as_raw_fd()
            ),
            Backend::X11 { .. } => "ximagesrc use-damage=false show-pointer=true".into(),
        }
    }

    async fn key(&self, code: u16, down: bool) -> Result<()> {
        match &self.backend {
            Backend::Portal { proxy, session, .. } => {
                proxy
                    .notify_keyboard_keycode(session, code as i32, state(down))
                    .await?
            }
            Backend::X11 { connection, root } => {
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
            Backend::X11 { connection, root } => {
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
                        node,
                        ..
                    } => {
                        proxy
                            .notify_pointer_motion_absolute(session, *node, x, y)
                            .await?
                    }
                    Backend::X11 { connection, root } => {
                        connection
                            .xtest_fake_input(
                                xproto::MOTION_NOTIFY_EVENT,
                                0,
                                0,
                                *root,
                                x as i16,
                                y as i16,
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
                Backend::X11 { connection, root } => {
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
