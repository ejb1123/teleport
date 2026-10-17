use crate::{
    protocol::{Event, Monitor},
    virtual_display::{MonitorFailure, Plan, Size, VirtualDisplay},
};
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

/// PipeWire capture is damage-driven. Re-send the last frame on a static
/// desktop so a requested keyframe or recovery always has a frame to encode,
/// and a change dropped by the FPS cap is repeated rather than lost.
const KEEPALIVE_MS: u32 = 250;

/// Caps every PipeWire source description ends with (` ! video/x-raw`):
/// CPU-mapped frames. The direct FFmpeg encoder replaces it to negotiate
/// DMA-BUFs for zero-copy capture.
pub const PIPEWIRE_SYSTEM_MEMORY: &str = "video/x-raw";

/// `--source test` pattern. `TELEPORT_TEST_PATTERN` (e.g. `snow`) lets network
/// tests use content that fills the configured bitrate; the default ball
/// compresses to almost nothing and would never stress a link.
fn test_pattern(monitor: usize) -> String {
    std::env::var("TELEPORT_TEST_PATTERN")
        .ok()
        .filter(|name| {
            !name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
        })
        .unwrap_or_else(|| if monitor == 0 { "ball" } else { "smpte" }.into())
}

pub struct Capture {
    pub width: u32,
    pub height: u32,
    pub name: &'static str,
    pub monitors: Vec<Monitor>,
    pub active_monitor: usize,
    backend: Backend,
    keys: BTreeSet<u16>,
    buttons: BTreeSet<u8>,
    /// Virtual gamepads for this session (no-ops on the test backend).
    gamepads: crate::gamepad::Gamepads,
    /// How this capture was opened, so a lost portal session can be recreated.
    source: String,
    restore_token_path: Option<std::path::PathBuf>,
    /// The session was closed and not yet successfully recreated.
    lost: bool,
    /// The client draws the pointer itself; leave it out of X11 video.
    pub local_cursor: bool,
    /// Portal: the host allows sending the pointer as metadata (the default).
    allow_cursor_metadata: bool,
    /// Portal: the compositor offers `CursorMode::Metadata`.
    cursor_metadata_available: bool,
    /// Portal: the open session sends the pointer as PipeWire cursor metadata
    /// (read by `crate::pw_cursor`) instead of drawing it into the video.
    cursor_metadata: bool,
    /// Host `--virtual-display`.
    virtual_mode: VirtualDisplay,
    /// Portal: the compositor offers virtual outputs (ScreenCast VIRTUAL).
    virtual_available: bool,
    /// Privacy mode: the primary session may only stream a virtual output.
    privacy: bool,
    /// Mode for a primary virtual output (the client's display).
    virtual_size: Option<Size>,
    /// Portal: the primary session streams a virtual output (monitor 0).
    primary_virtual: Option<VirtualOutput>,
    /// A virtual monitor added for the client, in a portal session of its
    /// own; always the last entry of `monitors`.
    extra: Option<ExtraMonitor>,
}

/// A portal virtual output: its compositor output name (KDE; the stream's
/// `mapping_id`) and mode.
#[derive(Clone, Debug)]
struct VirtualOutput {
    name: Option<String>,
    size: Size,
    /// Its mode can be changed (KDE output control works for it).
    resizable: bool,
}

struct ExtraMonitor {
    proxy: RemoteDesktop<'static>,
    session: Session<'static, RemoteDesktop<'static>>,
    node: u32,
    output: VirtualOutput,
}

/// The portal shared no monitor (or only virtual outputs).
#[derive(Debug)]
struct NoMonitor;

impl std::fmt::Display for NoMonitor {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("the desktop portal shared no monitor")
    }
}

impl std::error::Error for NoMonitor {}

fn monitor_failure(error: &anyhow::Error) -> MonitorFailure {
    use ashpd::desktop::ResponseError;
    if error.downcast_ref::<NoMonitor>().is_some() {
        return MonitorFailure::NoMonitor;
    }
    match error.downcast_ref::<ashpd::Error>() {
        Some(ashpd::Error::Response(ResponseError::Cancelled)) => MonitorFailure::Cancelled,
        Some(ashpd::Error::Response(ResponseError::Other)) => MonitorFailure::PortalError,
        _ => MonitorFailure::Other,
    }
}

/// An open RemoteDesktop + ScreenCast portal session.
struct PortalSession {
    proxy: RemoteDesktop<'static>,
    session: Session<'static, RemoteDesktop<'static>>,
    monitors: Vec<Monitor>,
    nodes: Vec<u32>,
    /// Compositor output names (`mapping_id`, KDE).
    outputs: Vec<Option<String>>,
    cursor_metadata_available: bool,
    cursor_metadata: bool,
}

/// Open a portal session sharing every monitor (`virtual_output` false) or a
/// new virtual output. The restore token, when given, avoids the local
/// dialog (only one live session may use it: tokens rotate).
async fn open_portal(
    virtual_output: bool,
    restore_token_path: Option<&Path>,
    cursor_metadata: bool,
) -> Result<PortalSession> {
    let proxy = RemoteDesktop::new()
        .await
        .context("RemoteDesktop portal unavailable; run inside your graphical session")?;
    let cast = Screencast::new().await?;
    let session = proxy.create_session().await?;
    match start_portal(
        &proxy,
        &cast,
        &session,
        virtual_output,
        restore_token_path,
        cursor_metadata,
    )
    .await
    {
        Ok(started) => Ok(PortalSession {
            proxy,
            session,
            monitors: started.monitors,
            nodes: started.nodes,
            outputs: started.outputs,
            cursor_metadata_available: started.cursor_metadata_available,
            cursor_metadata: started.cursor_metadata,
        }),
        Err(error) => {
            // Never leave a session (and its virtual output) behind.
            let _ = session.close().await;
            Err(error)
        }
    }
}

struct Started {
    monitors: Vec<Monitor>,
    nodes: Vec<u32>,
    outputs: Vec<Option<String>>,
    cursor_metadata_available: bool,
    cursor_metadata: bool,
}

async fn start_portal(
    proxy: &RemoteDesktop<'static>,
    cast: &Screencast<'static>,
    session: &Session<'static, RemoteDesktop<'static>>,
    virtual_output: bool,
    restore_token_path: Option<&Path>,
    cursor_metadata: bool,
) -> Result<Started> {
    let restore_token = restore_token_path
        .map(read_restore_token)
        .transpose()?
        .flatten();
    proxy
        .select_devices(
            session,
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
    // Portals before version 2 have no AvailableCursorModes and
    // keep the pointer embedded.
    let available = cast
        .available_cursor_modes()
        .await
        .is_ok_and(|modes| modes.contains(CursorMode::Metadata));
    let metadata = cursor_metadata && available;
    cast.select_sources(
        session,
        if metadata {
            CursorMode::Metadata
        } else {
            CursorMode::Embedded
        },
        if virtual_output {
            SourceType::Virtual.into()
        } else {
            SourceType::Monitor.into()
        },
        !virtual_output,
        None,
        PersistMode::DoNot,
    )
    .await?
    .response()?;
    if !virtual_output {
        tracing::info!(
            "Select monitors and approve desktop control in the local portal dialog; persistence depends on your compositor"
        );
    }
    let response = proxy.start(session, None).await?.response()?;
    ensure!(
        response.devices().contains(DeviceType::Keyboard)
            && response.devices().contains(DeviceType::Pointer),
        "keyboard and pointer permission are required"
    );
    let streams: Vec<_> = response
        .streams()
        .unwrap_or_default()
        .iter()
        // A monitor session shares every output, including virtual ones
        // (ours or another app's); those are not monitors.
        .filter(|stream| {
            virtual_output
                || !stream
                    .mapping_id()
                    .is_some_and(crate::virtual_display::is_portal_virtual)
        })
        .collect();
    if streams.is_empty() {
        return Err(NoMonitor.into());
    }
    let monitors = streams
        .iter()
        .enumerate()
        .map(|(id, stream)| {
            let (width, height) = stream
                .size()
                .context("portal did not report logical monitor size")?;
            ensure!(width > 0 && height > 0, "invalid monitor size");
            Ok(Monitor {
                id,
                name: if virtual_output {
                    "Virtual display".into()
                } else {
                    stream
                        .id()
                        .map(str::to_owned)
                        .unwrap_or_else(|| format!("Monitor {}", id + 1))
                },
                width: width as u32,
                height: height as u32,
                // The portal reports no refresh rate.
                refresh_mhz: 0,
                virtual_display: virtual_output,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    let nodes = streams
        .iter()
        .map(|stream| stream.pipe_wire_node_id())
        .collect();
    let outputs = streams
        .iter()
        .map(|stream| stream.mapping_id().map(str::to_owned))
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
    Ok(Started {
        monitors,
        nodes,
        outputs,
        cursor_metadata_available: available,
        cursor_metadata: metadata,
    })
}

/// Open a virtual-output session and give the output `size` (KDE). Unless
/// for privacy mode, physical monitors that were on stay on.
async fn open_virtual(
    restore_token_path: Option<&Path>,
    cursor_metadata: bool,
    size: Size,
    privacy: bool,
) -> Result<(PortalSession, VirtualOutput)> {
    let before = if privacy {
        Vec::new()
    } else {
        tokio::task::spawn_blocking(crate::virtual_display::enabled_physical)
            .await
            .unwrap_or_default()
    };
    let mut session = open_portal(true, restore_token_path, cursor_metadata).await?;
    // Elsewhere than KDE (GNOME) there is no output name or
    // `kscreen-doctor`, and the portal's size stays.
    let mut output = VirtualOutput {
        resizable: session.outputs[0].is_some(),
        name: session.outputs[0].clone(),
        size: Size {
            width: session.monitors[0].width,
            height: session.monitors[0].height,
            refresh_mhz: crate::virtual_display::Size::DEFAULT.refresh_mhz,
        },
    };
    resize_output(&mut output, size).await;
    session.monitors[0].width = output.size.width;
    session.monitors[0].height = output.size.height;
    session.monitors[0].refresh_mhz = output.size.refresh_mhz;
    if !before.is_empty()
        && let Err(error) =
            tokio::task::spawn_blocking(move || crate::virtual_display::keep_enabled(&before))
                .await
                .map_err(anyhow::Error::from)
                .and_then(|result| result)
    {
        tracing::warn!(
            error = format!("{error:#}"),
            "could not check the physical monitors"
        );
    }
    tracing::info!(
        output = output.name.as_deref().unwrap_or("unknown"),
        width = output.size.width,
        height = output.size.height,
        refresh_mhz = output.size.refresh_mhz,
        "virtual display created"
    );
    Ok((session, output))
}

/// Give a virtual output `size` through KDE's output management. Elsewhere
/// (GNOME: no `kscreen-doctor`, no output name) it keeps the portal's size.
async fn resize_output(output: &mut VirtualOutput, size: Size) {
    let Some(name) = output.name.clone().filter(|_| output.resizable) else {
        return;
    };
    if output.size == size {
        return;
    }
    match tokio::task::spawn_blocking(move || crate::virtual_display::resize(&name, size)).await {
        Ok(Ok(size)) => output.size = size,
        result => {
            // Not again for this output (no KDE output control, or refused).
            output.resizable = false;
            let error = match result {
                Ok(Err(error)) => format!("{error:#}"),
                Err(error) => error.to_string(),
                Ok(Ok(_)) => unreachable!(),
            };
            tracing::warn!(
                error,
                "could not resize the virtual display; keeping its size"
            );
        }
    }
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

/// A portal remote socket must outlive the GStreamer source using it. Each
/// simultaneous source needs its own core connection, never a replayed fd.
pub struct MonitorSource {
    pub pipeline: String,
    _fd: Option<OwnedFd>,
}

impl Capture {
    pub async fn monitor_source(
        &self,
        index: usize,
        range: crate::protocol::DynamicRange,
    ) -> Result<MonitorSource> {
        let monitor = self
            .monitors
            .get(index)
            .context("monitor index is out of range")?;
        let mut fd = None;
        let mut pipeline = match &self.backend {
            Backend::Test => format!("videotestsrc is-live=true pattern={}", test_pattern(index)),
            Backend::Portal { .. } => {
                // The socket must come from the session owning the node.
                let (_, session, node) = self
                    .portal_target(index)
                    .context("monitor index is out of range")?;
                let remote = Screencast::new()
                    .await?
                    .open_pipe_wire_remote(session)
                    .await?;
                let source = format!(
                    "pipewiresrc name={} fd={} path={} do-timestamp=true keepalive-time={KEEPALIVE_MS} ! {PIPEWIRE_SYSTEM_MEMORY}",
                    crate::pw_cursor::source_name(index),
                    remote.as_raw_fd(),
                    node
                );
                fd = Some(remote);
                source
            }
            Backend::X11 { origins, .. } => {
                ensure!(
                    range == crate::protocol::DynamicRange::Sdr,
                    "X11 capture is SDR"
                );
                let (x, y) = origins[index];
                format!(
                    "ximagesrc use-damage=false show-pointer={} startx={x} starty={y} endx={} endy={}",
                    !self.local_cursor,
                    u32::from(x as u16) + monitor.width - 1,
                    u32::from(y as u16) + monitor.height - 1
                )
            }
        };
        if range == crate::protocol::DynamicRange::Hdr10 {
            pipeline.push_str(" ! capsfilter name=hdr_source caps=video/x-raw,format=RGB10A2_LE,colorimetry=1:1:14:7");
        }
        Ok(MonitorSource { pipeline, _fd: fd })
    }

    pub async fn monitor_motion(&self, index: usize, x: f64, y: f64) -> Result<()> {
        Event::MonitorMotion { index, x, y }.validate()?;
        let monitor = self
            .monitors
            .get(index)
            .context("monitor index is out of range")?;
        let x = x * (monitor.width - 1) as f64;
        let y = y * (monitor.height - 1) as f64;
        match &self.backend {
            Backend::Portal { .. } => {
                let (proxy, session, node) = self
                    .portal_target(index)
                    .context("monitor index is out of range")?;
                proxy
                    .notify_pointer_motion_absolute(session, node, x, y)
                    .await?
            }
            Backend::X11 {
                connection,
                root,
                origins,
            } => {
                let (ox, oy) = origins[index];
                connection
                    .xtest_fake_input(
                        xproto::MOTION_NOTIFY_EVENT,
                        0,
                        0,
                        *root,
                        (x as i32 + i32::from(ox)) as i16,
                        (y as i32 + i32::from(oy)) as i16,
                        0,
                    )?
                    .check()?;
            }
            Backend::Test => (),
        }
        Ok(())
    }
    #[cfg(test)]
    pub async fn open(source: &str) -> Result<Self> {
        Self::open_with_options(source, None, 0, false).await
    }

    /// `cursor_metadata`: open a portal session with the pointer sent as
    /// PipeWire metadata when the compositor offers it (the client draws it,
    /// docs/cursor.md); otherwise the pointer is drawn into the video.
    /// Physical monitors only (no virtual outputs).
    #[cfg(test)]
    pub async fn open_with_options(
        source: &str,
        restore_token_path: Option<&Path>,
        monitor: usize,
        cursor_metadata: bool,
    ) -> Result<Self> {
        Self::open_host(
            source,
            restore_token_path,
            monitor,
            cursor_metadata,
            VirtualDisplay::Never,
        )
        .await
    }

    /// As `open_with_options`, with the host's `--virtual-display` policy:
    /// a virtual output when there is no monitor, or always.
    pub async fn open_host(
        source: &str,
        restore_token_path: Option<&Path>,
        monitor: usize,
        cursor_metadata: bool,
        virtual_mode: VirtualDisplay,
    ) -> Result<Self> {
        Self::open_inner(
            source,
            restore_token_path,
            monitor,
            cursor_metadata,
            virtual_mode,
            false,
            None,
        )
        .await
    }

    async fn open_inner(
        source: &str,
        restore_token_path: Option<&Path>,
        monitor: usize,
        cursor_metadata: bool,
        virtual_mode: VirtualDisplay,
        privacy: bool,
        virtual_size: Option<Size>,
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
                    refresh_mhz: 0,
                    virtual_display: false,
                },
                Monitor {
                    id: 1,
                    name: "Test portrait".into(),
                    width: 720,
                    height: 1280,
                    refresh_mhz: 0,
                    virtual_display: false,
                },
            ],
            active_monitor: 0,
            backend: Backend::Test,
            source: source.to_owned(),
            restore_token_path: restore_token_path.map(Path::to_path_buf),
            lost: false,
            local_cursor: false,
            allow_cursor_metadata: cursor_metadata,
            cursor_metadata_available: false,
            cursor_metadata: false,
            keys: BTreeSet::new(),
            buttons: BTreeSet::new(),
            gamepads: crate::gamepad::Gamepads::new(false),
            virtual_mode,
            virtual_available: false,
            privacy,
            virtual_size,
            primary_virtual: None,
            extra: None,
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
            "test" => ensure!(!privacy, "privacy mode needs a desktop portal"),
            "portal" => {
                register_portal_app().await;
                let virtual_available = Screencast::new()
                    .await?
                    .available_source_types()
                    .await
                    .is_ok_and(|types| types.contains(SourceType::Virtual));
                let plan = crate::virtual_display::plan(virtual_mode, virtual_available, privacy)?;
                let size = virtual_size.unwrap_or(Size::DEFAULT);
                let (opened, primary_virtual) = match plan {
                    Plan::Monitors => (
                        open_portal(false, restore_token_path, cursor_metadata).await?,
                        None,
                    ),
                    Plan::Virtual => {
                        let (opened, output) =
                            open_virtual(restore_token_path, cursor_metadata, size, privacy)
                                .await?;
                        (opened, Some(output))
                    }
                    Plan::MonitorsOrVirtual => {
                        match open_portal(false, restore_token_path, cursor_metadata).await {
                            Ok(opened) => (opened, None),
                            Err(error)
                                if crate::virtual_display::falls_back(monitor_failure(&error)) =>
                            {
                                tracing::warn!(
                                    error = format!("{error:#}"),
                                    "no monitor to share; streaming a virtual display instead"
                                );
                                let (opened, output) =
                                    open_virtual(restore_token_path, cursor_metadata, size, false)
                                        .await?;
                                (opened, Some(output))
                            }
                            Err(error) => return Err(error),
                        }
                    }
                };
                let fd = Screencast::new()
                    .await?
                    .open_pipe_wire_remote(&opened.session)
                    .await?;
                result.monitors = opened.monitors;
                result.cursor_metadata_available = opened.cursor_metadata_available;
                result.cursor_metadata = opened.cursor_metadata;
                result.virtual_available = virtual_available;
                tracing::info!(
                    cursor = if opened.cursor_metadata {
                        "metadata"
                    } else {
                        "embedded"
                    },
                    metadata_available = opened.cursor_metadata_available,
                    virtual_display = primary_virtual.is_some(),
                    virtual_available,
                    "desktop portal session started"
                );
                result.name = if primary_virtual.is_some() {
                    "Wayland portal (virtual display)"
                } else {
                    "Wayland portal"
                };
                result.primary_virtual = primary_virtual;
                result.backend = Backend::Portal {
                    proxy: opened.proxy,
                    session: opened.session,
                    fd,
                    nodes: opened.nodes,
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
        result.gamepads = crate::gamepad::Gamepads::new(!matches!(result.backend, Backend::Test));
        if let Err(error) = result.select_monitor(monitor).await {
            // Never leave a portal session (and a virtual output) behind.
            result.close().await;
            return Err(error);
        }
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
        if let Some((_, session, _)) = self.portal_target(index) {
            let remote = Screencast::new()
                .await?
                .open_pipe_wire_remote(session)
                .await?;
            if let Backend::Portal { fd, .. } = &mut self.backend {
                *fd = remote;
            }
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
                test_pattern(self.active_monitor)
            ),
            Backend::Portal { fd, .. } => format!(
                "pipewiresrc name={} fd={} path={} do-timestamp=true keepalive-time={KEEPALIVE_MS} ! {PIPEWIRE_SYSTEM_MEMORY}",
                crate::pw_cursor::source_name(self.active_monitor),
                fd.as_raw_fd(),
                self.portal_target(self.active_monitor)
                    .map_or(0, |(_, _, node)| node)
            ),
            Backend::X11 { origins, .. } => {
                let (x, y) = origins[self.active_monitor];
                format!(
                    "ximagesrc use-damage=false show-pointer={} startx={x} starty={y} endx={} endy={}",
                    !self.local_cursor,
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
                let code = evdev_button(button).context("unsupported button")?;
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
                    Backend::Portal { .. } => {
                        let (proxy, session, node) = self
                            .portal_target(self.active_monitor)
                            .context("monitor index is out of range")?;
                        proxy
                            .notify_pointer_motion_absolute(session, node, x, y)
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
            Event::RelativeMotion { dx, dy } => match &self.backend {
                Backend::Portal { proxy, session, .. } => {
                    proxy
                        .notify_pointer_motion(session, f64::from(dx), f64::from(dy))
                        .await?
                }
                Backend::X11 {
                    connection, root, ..
                } => {
                    // XTest motion with detail = 1 is relative to the current position.
                    connection
                        .xtest_fake_input(
                            xproto::MOTION_NOTIFY_EVENT,
                            1,
                            0,
                            *root,
                            dx.clamp(i16::MIN.into(), i16::MAX.into()) as i16,
                            dy.clamp(i16::MIN.into(), i16::MAX.into()) as i16,
                            0,
                        )?
                        .check()?;
                }
                Backend::Test => (),
            },
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
            Event::GamepadConnected { .. }
            | Event::GamepadDisconnected { .. }
            | Event::GamepadButton { .. }
            | Event::GamepadAxis { .. }
            | Event::GamepadTouch { .. }
            | Event::GamepadMotion { .. } => self.gamepads.input(&event)?,
            Event::Ping => (),
            // Host-level controls are handled before injection.
            Event::SelectMonitor { .. }
            | Event::MonitorStream { .. }
            | Event::MonitorMotion { .. }
            | Event::MonitorFeedback { .. }
            | Event::Probe { .. }
            | Event::ConfigureVideo { .. }
            | Event::Feedback { .. }
            | Event::NetworkFeedback { .. }
            | Event::Clipboard { .. }
            | Event::ClipboardRequest
            | Event::RequestKeyframe { .. }
            | Event::ClientFeatures { .. }
            | Event::SelectCodec { .. }
            | Event::TakeControl { .. }
            | Event::ApproveGuest { .. }
            | Event::ClientDisplay(_) => (),
        }
        Ok(())
    }

    /// A `Gamepad*` event from the session owning pad `block` (docs/guests.md).
    pub fn gamepad_input(&mut self, block: u8, event: &Event) -> Result<()> {
        self.gamepads.input_block(block, event)
    }

    /// One session's release: its pads only.
    pub fn neutralise_pads(&mut self, block: u8) {
        self.gamepads.neutralise_block(block);
    }

    /// One session left: remove its pads only.
    pub fn destroy_pads(&mut self, block: u8) {
        self.gamepads.destroy_block(block);
    }

    /// Release held keys and mouse buttons (they belong to the session that
    /// held keyboard and mouse control), not gamepads.
    pub async fn release_keyboard_mouse(&mut self) {
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
        // Pads stay attached (a game would see an unplug on every focus loss
        // or toolbar click); every button is released and every axis centred.
        self.gamepads.neutralise();
    }

    /// The client session ended: release everything and remove its pads and
    /// its virtual monitor.
    pub async fn end_session(&mut self) {
        self.release_all().await;
        self.gamepads.destroy_all();
        self.remove_virtual_monitor().await;
    }

    /// The portal session, proxy and PipeWire node behind monitor `index`.
    fn portal_target(
        &self,
        index: usize,
    ) -> Option<(
        &RemoteDesktop<'static>,
        &Session<'static, RemoteDesktop<'static>>,
        u32,
    )> {
        let Backend::Portal {
            proxy,
            session,
            nodes,
            ..
        } = &self.backend
        else {
            return None;
        };
        if let Some(&node) = nodes.get(index) {
            return Some((proxy, session, node));
        }
        self.extra
            .as_ref()
            .filter(|_| index == nodes.len())
            .map(|extra| (&extra.proxy, &extra.session, extra.node))
    }

    /// Whether a client may add a virtual monitor here: next to physical
    /// monitors only. KDE's portal names every virtual output of an app the
    /// same, so a second one could not be told from (and sized apart from)
    /// a virtual primary display.
    pub fn virtual_monitors_available(&self) -> bool {
        self.is_portal()
            && self.virtual_available
            && self.virtual_mode != VirtualDisplay::Never
            && self.primary_virtual.is_none()
    }

    /// Whether the compositor can create virtual outputs (privacy mode).
    pub fn virtual_available(&self) -> bool {
        self.is_portal() && self.virtual_available
    }

    /// The primary session streams a virtual output (monitor 0) that can
    /// follow the client's display: its size.
    pub fn primary_virtual(&self) -> Option<Size> {
        self.primary_virtual
            .as_ref()
            .filter(|output| output.resizable)
            .map(|output| output.size)
    }

    /// The client's virtual monitor can follow its display.
    pub fn virtual_monitor_resizable(&self) -> bool {
        self.extra
            .as_ref()
            .is_some_and(|extra| extra.output.resizable)
    }

    /// Privacy mode: the primary session streams a virtual output only.
    pub fn privacy(&self) -> bool {
        self.privacy
    }

    /// The client's virtual monitor, if added: its index.
    pub fn virtual_monitor(&self) -> Option<usize> {
        self.extra.as_ref().map(|_| self.monitors.len() - 1)
    }

    /// Add a virtual monitor of `size` in a portal session of its own, as the
    /// last monitor. Physical monitors stay as they are.
    pub async fn add_virtual_monitor(&mut self, size: Size) -> Result<usize> {
        ensure!(
            self.virtual_monitors_available(),
            "this host cannot create virtual displays"
        );
        ensure!(self.extra.is_none(), "a virtual monitor already exists");
        let (opened, output) = open_virtual(None, self.cursor_metadata, size, false).await?;
        let index = self.monitors.len();
        let mut monitor = opened.monitors[0].clone();
        monitor.id = index;
        monitor.name = "Virtual monitor".into();
        self.monitors.push(monitor);
        self.extra = Some(ExtraMonitor {
            proxy: opened.proxy,
            session: opened.session,
            node: opened.nodes[0],
            output,
        });
        Ok(index)
    }

    /// Close the virtual monitor's session: the compositor removes the output.
    pub async fn remove_virtual_monitor(&mut self) {
        let Some(extra) = self.extra.take() else {
            return;
        };
        let _ = extra.session.close().await;
        self.monitors.pop();
        tracing::info!("virtual monitor removed");
        if self.active_monitor >= self.monitors.len() && !self.monitors.is_empty() {
            let _ = self.select_monitor(0).await;
        }
    }

    /// Resize the virtual output behind monitor `index` (KDE). Returns
    /// whether its size changed; callers then restart its pipeline.
    pub async fn resize_virtual(&mut self, index: usize, size: Size) -> Result<bool> {
        let extra_index = self.virtual_monitor();
        let output = if extra_index == Some(index) {
            &mut self.extra.as_mut().context("virtual monitor gone")?.output
        } else if index == 0 && self.primary_virtual.is_some() {
            self.primary_virtual
                .as_mut()
                .context("virtual display gone")?
        } else {
            anyhow::bail!("monitor {index} is not a virtual display");
        };
        let before = output.size;
        resize_output(output, size).await;
        let after = output.size;
        if after == before {
            return Ok(false);
        }
        let monitor = &mut self.monitors[index];
        (monitor.width, monitor.height, monitor.refresh_mhz) =
            (after.width, after.height, after.refresh_mhz);
        if self.active_monitor == index {
            (self.width, self.height) = (after.width, after.height);
        }
        Ok(true)
    }

    /// Privacy mode on: the primary session becomes a virtual output of
    /// `size` (the physical monitors are turned off by `crate::privacy`
    /// afterwards). Off: back to the physical monitors. Recreates the
    /// session; callers rebuild every capture pipeline first.
    pub async fn set_privacy(&mut self, privacy: bool, size: Option<Size>) -> Result<()> {
        let reopen = crate::virtual_display::privacy_reopens(
            self.virtual_mode,
            self.primary_virtual.is_some(),
            self.privacy,
            privacy,
        );
        self.privacy = privacy;
        if size.is_some() {
            self.virtual_size = size;
        }
        if !reopen && !self.lost {
            return Ok(());
        }
        self.reopen().await
    }

    /// The primary session streams a virtual output only because no
    /// monitor was there (`--virtual-display fallback`).
    pub fn fallback_virtual(&self) -> bool {
        self.primary_virtual.is_some()
            && !self.privacy
            && self.virtual_mode == VirtualDisplay::Fallback
    }

    /// Between sessions, give up a fallback virtual output: at the desk it
    /// would be an invisible screen beside the real ones once they return.
    /// The next client reopens capture (`is_lost`).
    pub async fn release_fallback(&mut self) {
        if self.fallback_virtual() {
            self.close().await;
            self.primary_virtual = None;
            self.lost = true;
            tracing::info!("virtual display released until the next client");
        }
    }

    /// The client plays pad feedback (rumble and/or PlayStation lights and
    /// trigger effects), reported to `sender`.
    pub fn enable_pad_feedback(
        &mut self,
        sender: crate::gamepad::FeedbackSender,
        rumble: bool,
        lights: bool,
    ) {
        self.gamepads.enable_feedback(sender, rumble, lights);
    }

    /// Whether `Gamepad*` events can be backed by virtual pads.
    pub fn gamepads_available(&self) -> bool {
        self.gamepads.available()
    }

    /// Pad types the client may ask for (`Desktop::gamepad_types`).
    pub fn gamepad_types(&self) -> Vec<crate::protocol::GamepadType> {
        self.gamepads.types()
    }

    /// A previous recreation failed: the capture must be reopened before use.
    pub fn is_lost(&self) -> bool {
        self.lost
    }

    /// Whether input and capture go through the desktop portal (Wayland).
    pub fn is_portal(&self) -> bool {
        matches!(self.backend, Backend::Portal { .. })
    }

    /// Whether the pointer can be left out of the video and sent as shapes
    /// instead: X11 (XFixes), or the desktop portal when the compositor
    /// offers cursor metadata and the host allows it.
    pub fn supports_local_cursor(&self) -> bool {
        match self.backend {
            Backend::X11 { .. } => true,
            Backend::Portal { .. } => self.allow_cursor_metadata && self.cursor_metadata_available,
            Backend::Test => false,
        }
    }

    /// Portal: the pointer is left out of the video and sent as PipeWire
    /// cursor metadata (`crate::pw_cursor`).
    pub fn cursor_metadata(&self) -> bool {
        self.cursor_metadata
    }

    /// Portal: switch between cursor metadata (the client draws the pointer)
    /// and a pointer drawn into the video. The cursor mode belongs to the
    /// ScreenCast session, so this recreates the session as for a lost one
    /// (the saved token or the compositor's pre-authorization avoids a
    /// dialog). Returns whether the session changed; callers then rebuild
    /// every capture pipeline.
    pub async fn set_cursor_metadata(&mut self, metadata: bool) -> Result<bool> {
        if metadata == self.cursor_metadata || (metadata && !self.supports_local_cursor()) {
            return Ok(false);
        }
        tracing::info!(
            metadata,
            "recreating the desktop portal session for the client's cursor"
        );
        self.reopen_with(metadata).await?;
        Ok(true)
    }

    /// Replace a lost portal session with a new one, restored from the saved
    /// token (no local dialog when the compositor honours it). Keeps the
    /// active monitor when it still exists, and the cursor mode.
    pub async fn reopen(&mut self) -> Result<()> {
        self.reopen_with(self.cursor_metadata).await
    }

    async fn reopen_with(&mut self, metadata: bool) -> Result<()> {
        let monitor = self.active_monitor;
        // A portal recreation mid-session keeps the session's (neutralised)
        // pads: uinput does not depend on the portal.
        let gamepads = std::mem::replace(&mut self.gamepads, crate::gamepad::Gamepads::new(false));
        self.close().await;
        self.gamepads = gamepads;
        self.lost = true;
        // A primary virtual output keeps its size (the client's display).
        let size = self
            .primary_virtual
            .as_ref()
            .map(|output| output.size)
            .or(self.virtual_size);
        let open = |monitor| {
            Self::open_inner(
                &self.source,
                self.restore_token_path.as_deref(),
                monitor,
                metadata,
                self.virtual_mode,
                self.privacy,
                size,
            )
        };
        let mut fresh = match open(monitor).await {
            Ok(capture) => capture,
            // The monitor may be gone after an output change; start on the first.
            Err(error) if monitor != 0 => {
                tracing::warn!(%error, monitor, "previous monitor unavailable; reopening on the first");
                open(0).await?
            }
            Err(error) => return Err(error),
        };
        fresh.local_cursor = self.local_cursor;
        fresh.allow_cursor_metadata = self.allow_cursor_metadata;
        std::mem::swap(self, &mut fresh);
        std::mem::swap(&mut self.gamepads, &mut fresh.gamepads);
        Ok(())
    }

    pub async fn close(&mut self) {
        self.end_session().await;
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

/// Wire button (X11 numbering) → the evdev code the portal's
/// `NotifyPointerButton` takes (`linux/input-event-codes.h`).
fn evdev_button(button: u8) -> Option<i32> {
    Some(match button {
        1 => 0x110,                               // BTN_LEFT
        2 => 0x112,                               // BTN_MIDDLE
        3 => 0x111,                               // BTN_RIGHT
        crate::protocol::BUTTON_BACK => 0x113,    // BTN_SIDE
        crate::protocol::BUTTON_FORWARD => 0x114, // BTN_EXTRA
        _ => return None,
    })
}

/// Refresh rate of the first active output of a RandR monitor, in
/// millihertz; 0 when unknown (Xvfb modes have no dot clock).
fn x11_refresh_mhz(
    connection: &x11rb::rust_connection::RustConnection,
    outputs: &[x11rb::protocol::randr::Output],
) -> u32 {
    (|| -> Result<u32> {
        for &output in outputs {
            let info = connection
                .randr_get_output_info(output, x11rb::CURRENT_TIME)?
                .reply()?;
            if info.crtc == 0 {
                continue;
            }
            let crtc = connection
                .randr_get_crtc_info(info.crtc, x11rb::CURRENT_TIME)?
                .reply()?;
            let resources = connection
                .randr_get_screen_resources_current(connection.setup().roots[0].root)?
                .reply()?;
            if let Some(refresh) = resources
                .modes
                .iter()
                .find(|mode| mode.id == crtc.mode)
                .and_then(|mode| {
                    (u64::from(mode.dot_clock) * 1000)
                        .checked_div(u64::from(mode.htotal) * u64::from(mode.vtotal))
                })
            {
                return Ok(refresh as u32);
            }
        }
        Ok(0)
    })()
    .unwrap_or(0)
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
                refresh_mhz: x11_refresh_mhz(connection, &item.outputs),
                virtual_display: false,
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
                refresh_mhz: 0,
                virtual_display: false,
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

/// Application ID the host registers with desktop portals (needs a matching
/// `dev.teleport.Host.desktop`, which the NixOS module installs). Lets KDE
/// honour a pre-authorization instead of asking through a local dialog.
pub const PORTAL_APP_ID: &str = "dev.teleport.Host";

/// Register the host's application ID on the portal connection, once per
/// process, before the first portal request. Optional: older portals lack the
/// registry and the host then stays anonymous (local dialogs as before).
async fn register_portal_app() {
    static REGISTERED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    if REGISTERED.swap(true, std::sync::atomic::Ordering::SeqCst) {
        return;
    }
    let result = match PORTAL_APP_ID.parse() {
        Ok(app_id) => ashpd::register_host_app(app_id)
            .await
            .map_err(anyhow::Error::from),
        Err(error) => Err(anyhow::anyhow!("{error:?}")),
    };
    match result {
        Ok(()) => tracing::info!(app_id = PORTAL_APP_ID, "registered with the desktop portal"),
        Err(error) => tracing::warn!(
            app_id = PORTAL_APP_ID,
            error = format!("{error:#}"),
            "portal app registration unavailable; local approval may be required"
        ),
    }
}

/// Whether a portal error means our RemoteDesktop session is gone or no
/// longer allowed to inject (KDE closes it when its outputs disappear, e.g.
/// when sleeping monitors wake). Such a session never recovers by retrying.
pub fn portal_session_lost(error: &anyhow::Error) -> bool {
    let text = format!("{error:#}");
    [
        "Invalid session",
        "non-existing session",
        "is not allowed to call",
    ]
    .iter()
    .any(|pattern| text.contains(pattern))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn mouse_buttons_map_to_evdev_codes() {
        // Unchanged for left, middle and right (272, 274, 273 before).
        assert_eq!(evdev_button(1), Some(272));
        assert_eq!(evdev_button(2), Some(274));
        assert_eq!(evdev_button(3), Some(273));
        assert_eq!(evdev_button(crate::protocol::BUTTON_BACK), Some(275));
        assert_eq!(evdev_button(crate::protocol::BUTTON_FORWARD), Some(276));
        assert_eq!(evdev_button(4), None);
    }
    #[test]
    fn missing_monitors_are_told_from_refusals() {
        use ashpd::desktop::ResponseError;
        let failure = |error: anyhow::Error| monitor_failure(&error.context("opening the portal"));
        assert_eq!(failure(NoMonitor.into()), MonitorFailure::NoMonitor);
        // KDE answers "Other" when it has no screen at all.
        assert_eq!(
            failure(ashpd::Error::Response(ResponseError::Other).into()),
            MonitorFailure::PortalError
        );
        assert_eq!(
            failure(ashpd::Error::Response(ResponseError::Cancelled).into()),
            MonitorFailure::Cancelled
        );
        assert_eq!(
            failure(anyhow::anyhow!(
                "keyboard and pointer permission are required"
            )),
            MonitorFailure::Other
        );
    }

    #[tokio::test]
    async fn the_test_source_has_no_virtual_displays() {
        let mut capture = Capture::open("test").await.unwrap();
        assert!(!capture.virtual_monitors_available());
        assert!(capture.add_virtual_monitor(Size::DEFAULT).await.is_err());
        assert!(capture.resize_virtual(0, Size::DEFAULT).await.is_err());
        assert_eq!(capture.monitors.len(), 2);
        assert!(
            Capture::open_inner("test", None, 0, false, VirtualDisplay::Fallback, true, None)
                .await
                .is_err(),
            "privacy needs a portal"
        );
    }

    #[test]
    fn recognises_kde_lost_session_errors_only() {
        for text in [
            "Portal request failed: org.freedesktop.zbus.Error: Invalid session",
            "Portal request failed: org.freedesktop.zbus.Error: Session is not allowed to call NotifyPointer methods",
        ] {
            assert!(portal_session_lost(&anyhow::anyhow!("{text}")), "{text}");
        }
        assert!(!portal_session_lost(&anyhow::anyhow!(
            "Portal request failed: timeout"
        )));
        assert!(!portal_session_lost(&anyhow::anyhow!("unsupported key")));
    }

    #[tokio::test]
    async fn independent_monitor_source_and_motion_preserve_primary_and_held_keys() {
        let mut capture = Capture::open("test").await.unwrap();
        capture
            .input(Event::Key {
                code: 42,
                down: true,
            })
            .await
            .unwrap();
        let source = capture
            .monitor_source(1, crate::protocol::DynamicRange::Sdr)
            .await
            .unwrap();
        assert!(source.pipeline.contains("smpte"));
        capture.monitor_motion(1, 1.0, 0.5).await.unwrap();
        assert_eq!(
            (capture.active_monitor, capture.width, capture.height),
            (0, 1280, 720)
        );
        assert!(capture.keys.contains(&42));
        assert!(
            capture
                .monitor_source(2, crate::protocol::DynamicRange::Sdr)
                .await
                .is_err()
        );
        assert!(capture.monitor_motion(2, 0.5, 0.5).await.is_err());
        assert!(capture.monitor_motion(1, f64::NAN, 0.5).await.is_err());
        capture.release_all().await;
        assert!(capture.keys.is_empty());
    }

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
