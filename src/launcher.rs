//! Small native SDL connection manager; streaming runs in a separate native process.
use anyhow::{Context, Result};
use sdl2::{event::Event, keyboard::Keycode, pixels::Color, rect::Rect};
use std::{
    io::{BufRead, BufReader},
    path::PathBuf,
    process::{Child, Command, Stdio},
    time::Duration,
};
use zeroize::{Zeroize, Zeroizing};

fn session_profile(
    address: &str,
    pairing: &crate::protocol::Pairing,
) -> Result<(crate::profiles::Profile, tempfile::TempDir)> {
    use std::io::Write;
    pairing.validate()?;
    let directory = tempfile::Builder::new()
        .prefix("teleport-session-")
        .tempdir()?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700))?;
    }
    let path = directory.path().join("session.json");
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let serialized = Zeroizing::new(serde_json::to_vec(pairing)?);
    options.open(&path)?.write_all(&serialized)?;
    Ok((
        crate::profiles::Profile {
            name: String::new(),
            address: address.into(),
            pairing_file: Some(path),
            fingerprint: Some(pairing.fingerprint.clone()),
        },
        directory,
    ))
}

#[cfg(test)]
fn system_fingerprint(
    address: &str,
    explicit: &str,
    profiles: &[crate::profiles::Profile],
) -> Result<String> {
    let fingerprint = if explicit.trim().is_empty() {
        let profile = profiles
            .iter()
            .find(|profile| profile.address == address)
            .context("a trusted host fingerprint is required")?;
        profile
            .trusted_fingerprint()?
            .context("approve this desktop's fingerprint first")?
    } else {
        explicit.trim().to_ascii_lowercase()
    };
    anyhow::ensure!(
        fingerprint.len() == 64 && fingerprint.bytes().all(|byte| byte.is_ascii_hexdigit()),
        "expected a SHA-256 fingerprint"
    );
    Ok(fingerprint.to_ascii_lowercase())
}

pub fn font_path() -> Result<PathBuf> {
    let candidates = [
        std::env::var_os("TELEPORT_FONT").map(PathBuf::from),
        Some(PathBuf::from(
            "/System/Library/Fonts/Supplemental/Arial.ttf",
        )),
        Some(PathBuf::from(
            "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",
        )),
        Some(PathBuf::from("/usr/share/fonts/TTF/DejaVuSans.ttf")),
    ];
    candidates
        .into_iter()
        .flatten()
        .find(|path| path.is_file())
        .context("No UI font found; run through Nix or set TELEPORT_FONT to a .ttf font")
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Page {
    Home,
    Add,
    Auth,
}
#[derive(Clone, Copy, PartialEq, Eq)]
enum AuthMode {
    Linux,
    Teleport,
    Code,
    File,
}
struct Approval {
    address: String,
    fingerprint: String,
    pairing: Option<crate::protocol::Pairing>,
    presented: bool,
}

impl Approval {
    fn accepts_click(&self, clicks: u8) -> bool {
        self.presented && clicks == 1
    }
}

fn approval_confirm_rect() -> Rect {
    Rect::new(304, 560, 448, 44)
}

#[derive(Default)]
struct AgentConsent {
    address: String,
    enabled: bool,
    pending: Option<std::time::Instant>,
}

impl AgentConsent {
    fn clear(&mut self) {
        *self = Self::default();
    }
    fn select(&mut self, address: &str) {
        if self.address != address {
            self.clear();
            self.address = address.into();
        }
    }
    fn enabled_for(&self, address: &str) -> bool {
        self.enabled && !address.is_empty() && self.address == address
    }
    fn awaiting_for(&self, address: &str, now: std::time::Instant) -> bool {
        self.address == address
            && self
                .pending
                .is_some_and(|at| now.saturating_duration_since(at) < Duration::from_secs(10))
    }
    fn click(&mut self, address: &str, now: std::time::Instant) {
        self.select(address);
        if self.enabled {
            self.clear();
        } else if !address.is_empty() && self.awaiting_for(address, now) {
            self.enabled = true;
            self.pending = None;
        } else if !address.is_empty() {
            self.pending = Some(now);
        }
    }
}
impl Drop for Approval {
    fn drop(&mut self) {
        if let Some(pairing) = &mut self.pairing {
            pairing.token.zeroize();
        }
    }
}
enum WorkerResult {
    Observed(Result<String>),
    Authenticated(bool, Result<crate::protocol::Pairing>),
}
type WorkerReceiver = std::sync::mpsc::Receiver<(String, WorkerResult)>;

fn authenticated_profile(
    address: &str,
    pairing: &crate::protocol::Pairing,
    session_only: bool,
    profiles: &mut Vec<crate::profiles::Profile>,
    ephemeral: &mut Option<tempfile::TempDir>,
) -> Result<crate::profiles::Profile> {
    let trusted = profiles
        .iter()
        .find(|p| p.address == address)
        .context("desktop is no longer saved")?
        .trusted_fingerprint()?
        .context("approve this desktop's fingerprint first")?;
    anyhow::ensure!(
        moq_native::tls::parse_fingerprint(&trusted)?
            == moq_native::tls::parse_fingerprint(&pairing.fingerprint)?,
        "HOST IDENTITY CHANGED: connection blocked; saved trust was not replaced"
    );
    if session_only {
        let (mut profile, directory) = session_profile(address, pairing)?;
        profile.name = profiles
            .iter()
            .find(|p| p.address == address)
            .map_or_else(String::new, |p| p.name.clone());
        *ephemeral = Some(directory);
        Ok(profile)
    } else {
        *ephemeral = None;
        crate::profiles::save_pairing(address, pairing, profiles)
    }
}

#[allow(non_snake_case)]
pub fn run() -> Result<()> {
    let CONNECT = Rect::new(304, 272, 448, 44);
    let DISCONNECT = Rect::new(764, 272, 228, 44);
    let NEW_HOST = Rect::new(16, 592, 224, 44);
    let HOST_SETTINGS = Rect::new(16, 704, 224, 40);
    let SETTINGS = Rect::new(304, 342, 220, 36);
    let SIGN_IN = Rect::new(540, 342, 220, 36);
    let NAME = Rect::new(304, 220, 688, 44);
    let ADDRESS = Rect::new(304, 300, 688, 44);
    let SAVE = Rect::new(304, 390, 448, 44);
    let CANCEL_ADD = Rect::new(764, 390, 228, 44);
    let USERNAME = Rect::new(304, 300, 688, 44);
    let SECRET = Rect::new(304, 380, 688, 44);
    let AUTH = Rect::new(304, 460, 448, 44);
    let CANCEL_AUTH = Rect::new(764, 460, 228, 44);
    let APPROVE = approval_confirm_rect();
    let REJECT = Rect::new(764, 560, 228, 44);
    debug_assert!(!APPROVE.has_intersection(AUTH));
    let mode_rect = |index: usize| Rect::new(304 + index as i32 * 174, 220, 166, 38);
    let pref_rect = |index: usize| {
        Rect::new(
            304 + (index % 3) as i32 * 232,
            456 + (index / 3) as i32 * 54,
            220,
            38,
        )
    };
    let (sdl, video) = crate::windowing::init()?;
    let ttf = sdl2::ttf::init().map_err(anyhow::Error::msg)?;
    let font = ttf
        .load_font(font_path()?, 16)
        .map_err(anyhow::Error::msg)?;
    let title_font = ttf
        .load_font(font_path()?, 30)
        .map_err(anyhow::Error::msg)?;
    let small_font = ttf
        .load_font(font_path()?, 13)
        .map_err(anyhow::Error::msg)?;
    let window = video
        .window("Teleport", 1040, 832)
        .position_centered()
        .allow_highdpi()
        .resizable()
        .build()?;
    let mut canvas = crate::windowing::software(window)?;
    canvas.set_logical_size(1040, 832)?;
    let textures = canvas.texture_creator();
    let mut events = sdl.event_pump().map_err(anyhow::Error::msg)?;
    video.text_input().start();
    let mut profiles = crate::profiles::load()?;
    let mut selected = profiles.last().cloned();
    let mut page = Page::Home;
    let mut mode = AuthMode::Linux;
    let mut name = String::new();
    let mut address = String::new();
    let mut username = String::new();
    let mut secret = Zeroizing::new(String::new());
    let mut focused = 0usize;
    let mut select_all = false;
    let mut host_offset = profiles.len().saturating_sub(7);
    let mut status =
        "Add a desktop with its name and address. Sign in when you connect.".to_owned();
    let mut worker: Option<WorkerReceiver> = None;
    let mut approval: Option<Approval> = None;
    let mut ephemeral: Option<tempfile::TempDir> = None;
    let mut child: Option<Child> = None;
    let mut progress: Option<std::sync::mpsc::Receiver<String>> = None;
    let mut connect_requested = false;
    let mut auth_requested = false;
    let mut settings = false;
    let mut resolution = 0usize;
    let mut frame_rate = 1usize;
    let mut quality = 1usize;
    let mut codec = 0usize;
    let mut hdr = false;
    let mut clipboard = false;
    let mut mute = false;
    let mut agent_consent = AgentConsent::default();
    let started = std::time::Instant::now();

    'ui: loop {
        agent_consent.select(
            selected
                .as_ref()
                .map_or("", |profile| profile.address.as_str()),
        );
        if let Some(receiver) = &worker {
            match receiver.try_recv() {
                Ok((host, outcome)) => {
                    worker = None;
                    let result: Result<()> = (|| {
                        match outcome {
                            WorkerResult::Observed(result) => {
                                let fingerprint = result?;
                                if let Some(pin) = profiles
                                    .iter()
                                    .find(|p| p.address == host)
                                    .context("desktop missing")?
                                    .trusted_fingerprint()?
                                {
                                    anyhow::ensure!(
                                        pin.eq_ignore_ascii_case(&fingerprint),
                                        "HOST IDENTITY CHANGED: saved trust was not replaced"
                                    );
                                    auth_requested = true;
                                } else {
                                    status = "Verify the host fingerprint, then approve or cancel."
                                        .into();
                                    approval = Some(Approval {
                                        address: host,
                                        fingerprint,
                                        pairing: None,
                                        presented: false,
                                    });
                                }
                            }
                            WorkerResult::Authenticated(session, result) => {
                                secret.zeroize();
                                let mut pairing = result?;
                                pairing.validate()?;
                                let pin = profiles
                                    .iter()
                                    .find(|p| p.address == host)
                                    .context("desktop missing")?
                                    .trusted_fingerprint()?;
                                if let Some(pin) = pin {
                                    if !pin.eq_ignore_ascii_case(&pairing.fingerprint) {
                                        pairing.token.zeroize();
                                        anyhow::bail!(
                                            "HOST IDENTITY CHANGED: saved trust was not replaced"
                                        );
                                    }
                                    selected = Some(authenticated_profile(
                                        &host,
                                        &pairing,
                                        session,
                                        &mut profiles,
                                        &mut ephemeral,
                                    )?);
                                    pairing.token.zeroize();
                                    page = Page::Home;
                                    connect_requested = true;
                                } else {
                                    anyhow::ensure!(
                                        !session,
                                        "Linux password login requires prior host approval"
                                    );
                                    status = "Verify the host fingerprint, then approve or cancel."
                                        .into();
                                    approval = Some(Approval {
                                        address: host,
                                        fingerprint: pairing.fingerprint.clone(),
                                        pairing: Some(pairing),
                                        presented: false,
                                    });
                                }
                            }
                        }
                        Ok(())
                    })();
                    if let Err(error) = result {
                        secret.zeroize();
                        status = format!("Connection blocked: {error}");
                    }
                }
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    worker = None;
                    secret.zeroize();
                    status = "Connection worker stopped. Try again.".into();
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => {}
            }
        }
        if let Some(receiver) = &progress {
            while let Ok(message) = receiver.try_recv() {
                status = message;
            }
        }
        if let Some(process) = child.as_mut()
            && let Some(exit) = process.try_wait()?
        {
            child = None;
            progress = None;
            if ephemeral.take().is_some() {
                selected = profiles
                    .iter()
                    .find(|p| selected.as_ref().is_some_and(|s| s.address == p.address))
                    .cloned();
            }
            agent_consent.clear();
            if exit.success() {
                status = "Disconnected. Your desktop is saved for next time.".into();
            } else {
                page = Page::Auth;
                status = "Connection failed. Check the network, or sign in again. Saved identity remains pinned.".into();
            }
        }
        for event in events.poll_iter() {
            if matches!(event, Event::Quit { .. }) {
                break 'ui;
            }
            if let Some(pending) = approval.as_ref() {
                if let Event::MouseButtonDown {
                    x,
                    y,
                    mouse_btn: sdl2::mouse::MouseButton::Left,
                    clicks,
                    ..
                } = event
                {
                    if !pending.accepts_click(clicks) {
                        continue;
                    }
                    if hit(REJECT, x, y) {
                        approval = None;
                        secret.zeroize();
                        status = "Host approval cancelled. No new trust was saved.".into();
                    } else if hit(APPROVE, x, y) {
                        let host = pending.address.clone();
                        let pin = pending.fingerprint.clone();
                        match crate::profiles::approve_fingerprint(&host, &pin, &mut profiles) {
                            Ok(profile) => {
                                selected = Some(profile);
                                let mut pending = approval.take().unwrap();
                                if let Some(mut pairing) = pending.pairing.take() {
                                    match authenticated_profile(
                                        &host,
                                        &pairing,
                                        false,
                                        &mut profiles,
                                        &mut ephemeral,
                                    ) {
                                        Ok(profile) => {
                                            selected = Some(profile);
                                            page = Page::Home;
                                            connect_requested = true;
                                        }
                                        Err(error) => {
                                            status = format!("Could not save access: {error}")
                                        }
                                    }
                                    pairing.token.zeroize();
                                } else {
                                    auth_requested = true;
                                }
                            }
                            Err(error) => {
                                approval = None;
                                secret.zeroize();
                                status = format!("Connection blocked: {error}");
                            }
                        }
                    }
                }
                continue;
            }
            if worker.is_some() {
                continue;
            }
            match event {
                Event::KeyDown {
                    keycode: Some(Keycode::Escape),
                    ..
                } => {
                    page = Page::Home;
                    secret.zeroize();
                }
                Event::KeyDown {
                    keycode: Some(Keycode::Tab),
                    ..
                } if page != Page::Home => {
                    focused = 1 - focused.min(1);
                    select_all = false;
                }
                Event::KeyDown {
                    keycode: Some(Keycode::A),
                    keymod,
                    ..
                } if shortcut(keymod) => select_all = true,
                Event::TextInput { text, .. } if page != Page::Home => {
                    let field: &mut String = match (page, focused) {
                        (Page::Add, 0) => &mut name,
                        (Page::Add, _) => &mut address,
                        (_, 0) if matches!(mode, AuthMode::Linux | AuthMode::Teleport) => {
                            &mut username
                        }
                        _ => &mut secret,
                    };
                    if select_all {
                        field.zeroize();
                        select_all = false;
                    }
                    if page == Page::Auth && mode == AuthMode::Code {
                        append_code(field, &text);
                    } else if field.len() + text.len() <= 2048 {
                        field.push_str(&text);
                    }
                }
                Event::KeyDown {
                    keycode: Some(Keycode::Backspace),
                    ..
                } if page != Page::Home => {
                    let field: &mut String = match (page, focused) {
                        (Page::Add, 0) => &mut name,
                        (Page::Add, _) => &mut address,
                        (_, 0) if matches!(mode, AuthMode::Linux | AuthMode::Teleport) => {
                            &mut username
                        }
                        _ => &mut secret,
                    };
                    if select_all {
                        field.zeroize();
                        select_all = false;
                    } else {
                        field.pop();
                    }
                }
                Event::KeyDown {
                    keycode: Some(Keycode::V),
                    keymod,
                    ..
                } if shortcut(keymod) && page != Page::Home => {
                    if let Ok(text) = video.clipboard().clipboard_text() {
                        let field: &mut String = match (page, focused) {
                            (Page::Add, 0) => &mut name,
                            (Page::Add, _) => &mut address,
                            (_, 0) if matches!(mode, AuthMode::Linux | AuthMode::Teleport) => {
                                &mut username
                            }
                            _ => &mut secret,
                        };
                        if text.len() <= 2048 {
                            field.zeroize();
                            if page == Page::Auth && mode == AuthMode::Code {
                                append_code(field, &text);
                            } else {
                                field.push_str(&text);
                            }
                            select_all = false;
                        }
                    }
                }
                Event::DropFile { filename, .. } if page == Page::Auth => {
                    mode = AuthMode::File;
                    *secret = filename;
                    focused = 1;
                }
                Event::KeyDown {
                    keycode: Some(Keycode::Return | Keycode::KpEnter),
                    ..
                } => match page {
                    Page::Home => connect_requested = true,
                    Page::Auth => auth_requested = true,
                    Page::Add => match crate::profiles::save_desktop(
                        name.trim(),
                        address.trim(),
                        &mut profiles,
                    ) {
                        Ok(profile) => {
                            agent_consent.select(&profile.address);
                            selected = Some(profile);
                            page = Page::Home;
                            host_offset = profiles.len().saturating_sub(7);
                            status = "Desktop saved. Click Connect to sign in.".into();
                        }
                        Err(error) => status = format!("Could not add desktop: {error}"),
                    },
                },
                Event::MouseWheel { y, .. } if page == Page::Home => {
                    if y < 0 {
                        host_offset = (host_offset + 1).min(profiles.len().saturating_sub(7));
                    } else {
                        host_offset = host_offset.saturating_sub(1);
                    }
                }
                Event::MouseButtonDown {
                    x,
                    y,
                    mouse_btn: sdl2::mouse::MouseButton::Left,
                    clicks: 1,
                    ..
                } => {
                    select_all = false;
                    match page {
                        Page::Add => {
                            if hit(NAME, x, y) {
                                focused = 0;
                            }
                            if hit(ADDRESS, x, y) {
                                focused = 1;
                            }
                            if hit(CANCEL_ADD, x, y) {
                                page = Page::Home;
                            }
                            if hit(SAVE, x, y) {
                                match crate::profiles::save_desktop(
                                    name.trim(),
                                    address.trim(),
                                    &mut profiles,
                                ) {
                                    Ok(profile) => {
                                        agent_consent.select(&profile.address);
                                        selected = Some(profile);
                                        page = Page::Home;
                                        host_offset = profiles.len().saturating_sub(7);
                                        status = "Desktop saved. Click Connect to sign in.".into();
                                    }
                                    Err(error) => {
                                        status = format!("Could not add desktop: {error}")
                                    }
                                }
                            }
                        }
                        Page::Auth => {
                            if hit(USERNAME, x, y) {
                                focused = 0;
                            }
                            if hit(SECRET, x, y) {
                                focused = 1;
                            }
                            if hit(AUTH, x, y) {
                                auth_requested = true;
                            }
                            if hit(CANCEL_AUTH, x, y) {
                                page = Page::Home;
                                secret.zeroize();
                            }
                            for (index, value) in [
                                AuthMode::Linux,
                                AuthMode::Teleport,
                                AuthMode::Code,
                                AuthMode::File,
                            ]
                            .into_iter()
                            .enumerate()
                            {
                                if hit(mode_rect(index), x, y) {
                                    mode = value;
                                    secret.zeroize();
                                    focused =
                                        if matches!(mode, AuthMode::Linux | AuthMode::Teleport) {
                                            0
                                        } else {
                                            1
                                        };
                                }
                            }
                        }
                        Page::Home => {
                            if hit(NEW_HOST, x, y) && child.is_none() {
                                agent_consent.clear();
                                page = Page::Add;
                                name.clear();
                                address.clear();
                                focused = 0;
                            }
                            for (index, profile) in
                                profiles.iter().enumerate().skip(host_offset).take(7)
                            {
                                if hit(
                                    Rect::new(16, 146 + (index - host_offset) as i32 * 60, 224, 52),
                                    x,
                                    y,
                                ) && child.is_none()
                                {
                                    selected = Some(profile.clone());
                                    ephemeral = None;
                                    agent_consent.clear();
                                }
                            }
                            if hit(CONNECT, x, y) && child.is_none() {
                                connect_requested = true;
                            }
                            if hit(SIGN_IN, x, y) && child.is_none() && selected.is_some() {
                                page = Page::Auth;
                                secret.zeroize();
                                focused = 0;
                            }
                            if hit(SETTINGS, x, y) {
                                settings = !settings;
                            }
                            if hit(DISCONNECT, x, y)
                                && let Some(mut process) = child.take()
                            {
                                let _ = process.kill();
                                let _ = process.wait();
                                progress = None;
                                if ephemeral.take().is_some() {
                                    selected = profiles
                                        .iter()
                                        .find(|p| {
                                            selected
                                                .as_ref()
                                                .is_some_and(|s| s.address == p.address)
                                        })
                                        .cloned();
                                }
                                agent_consent.clear();
                                status = "Disconnected.".into();
                            }
                            if settings && child.is_none() {
                                if hit(pref_rect(0), x, y) {
                                    resolution = (resolution + 1) % 5;
                                }
                                if hit(pref_rect(1), x, y) {
                                    frame_rate = (frame_rate + 1) % 3;
                                }
                                if hit(pref_rect(2), x, y) {
                                    quality = (quality + 1) % 3;
                                }
                                if hit(pref_rect(3), x, y) {
                                    codec = 1 - codec;
                                    if codec == 0 {
                                        hdr = false;
                                    }
                                }
                                if hit(pref_rect(4), x, y) && cfg!(target_os = "macos") {
                                    hdr = !hdr;
                                    if hdr {
                                        codec = 1;
                                    }
                                }
                                if hit(pref_rect(5), x, y) {
                                    clipboard = !clipboard;
                                }
                                if hit(pref_rect(6), x, y) {
                                    mute = !mute;
                                }
                                if hit(pref_rect(7), x, y) {
                                    let target = selected
                                        .as_ref()
                                        .map_or("", |profile| profile.address.as_str());
                                    agent_consent.click(target, std::time::Instant::now());
                                    if agent_consent.awaiting_for(target, std::time::Instant::now())
                                    {
                                        status = "This host can request signatures with your SSH keys. Click Confirm SSH again to allow this session.".into();
                                    }
                                }
                            }
                            if hit(HOST_SETTINGS, x, y) && cfg!(target_os = "linux") {
                                match Command::new(std::env::current_exe()?)
                                    .arg("host-manager")
                                    .spawn()
                                {
                                    Ok(mut process) => {
                                        std::thread::spawn(move || {
                                            let _ = process.wait();
                                        });
                                    }
                                    Err(error) => {
                                        status = format!("Could not open host settings: {error}")
                                    }
                                }
                            }
                        }
                    }
                }
                _ => {}
            }
        }

        if auth_requested && worker.is_none() && approval.is_none() && child.is_none() {
            auth_requested = false;
            if let Some(profile) = selected.clone() {
                let host = profile.address.clone();
                let result: Result<()> = (|| {
                    let pin = profile.trusted_fingerprint()?;
                    if mode == AuthMode::Linux && pin.is_none() {
                        let (sender, receiver) = std::sync::mpsc::channel();
                        std::thread::spawn(move || {
                            let result = tokio::runtime::Builder::new_multi_thread()
                                .enable_all()
                                .build()
                                .map_err(anyhow::Error::from)
                                .and_then(|runtime| {
                                    runtime
                                        .block_on(crate::system_login::observe_fingerprint(&host))
                                });
                            let _ = sender.send((host, WorkerResult::Observed(result)));
                        });
                        worker = Some(receiver);
                        status =
                            "Reading the host certificate. No password is sent before approval."
                                .into();
                        return Ok(());
                    }
                    anyhow::ensure!(
                        !secret.is_empty(),
                        "enter a password, pairing code, or file path"
                    );
                    if matches!(mode, AuthMode::Linux | AuthMode::Teleport) {
                        anyhow::ensure!(!username.trim().is_empty(), "enter your username");
                    }
                    let user = username.trim().to_owned();
                    let credential = Zeroizing::new(std::mem::take(&mut *secret));
                    let auth_mode = mode;
                    let (sender, receiver) = std::sync::mpsc::channel();
                    std::thread::spawn(move || {
                        let result = tokio::runtime::Builder::new_multi_thread()
                            .enable_all()
                            .build()
                            .map_err(anyhow::Error::from)
                            .and_then(|runtime| match auth_mode {
                                AuthMode::Linux => runtime.block_on(crate::system_login::login(
                                    &host,
                                    &user,
                                    &credential,
                                    pin.as_deref().unwrap(),
                                )),
                                AuthMode::Teleport => runtime.block_on(crate::access::login(
                                    &host,
                                    &user,
                                    &credential,
                                    &format!("{} client", std::env::consts::OS),
                                )),
                                AuthMode::Code => {
                                    runtime.block_on(crate::pairing::pair(&host, &credential))
                                }
                                AuthMode::File => {
                                    let path = std::path::Path::new(credential.trim());
                                    crate::profiles::check_private_file(path)?;
                                    crate::protocol::Pairing::read(path)
                                }
                            });
                        let _ = sender.send((
                            host,
                            WorkerResult::Authenticated(auth_mode == AuthMode::Linux, result),
                        ));
                    });
                    worker = Some(receiver);
                    status =
                        "Authenticating. Touch your security key if this host requires it.".into();
                    Ok(())
                })();
                if let Err(error) = result {
                    secret.zeroize();
                    status = format!("Sign-in blocked: {error}");
                }
            }
        }
        if connect_requested && worker.is_none() && approval.is_none() && child.is_none() {
            connect_requested = false;
            if let Some(profile) = selected.as_ref() {
                if let Some(credential) = &profile.pairing_file {
                    let result: Result<Child> = (|| {
                        profile
                            .trusted_fingerprint()?
                            .context("host identity is not approved")?;
                        crate::profiles::check_private_file(credential)?;
                        let mut command = Command::new(std::env::current_exe()?);
                        command
                            .arg("client")
                            .arg(&profile.address)
                            .arg("--pairing-file")
                            .arg(credential)
                            .arg("--width")
                            .arg([0, 1920, 2560, 1280, 3840][resolution].to_string())
                            .arg("--fps")
                            .arg([30, 60, 120][frame_rate].to_string())
                            .arg("--bitrate")
                            .arg([8000, 20000, 40000][quality].to_string())
                            .arg("--codec")
                            .arg(["h264", "h265"][codec])
                            .arg("--dynamic-range")
                            .arg(if hdr { "hdr10" } else { "sdr" });
                        if clipboard {
                            command.arg("--clipboard");
                        }
                        if mute {
                            command.arg("--mute");
                        }
                        if agent_consent.enabled_for(&profile.address) {
                            command.arg("--forward-ssh-agent");
                        }
                        command.stdin(Stdio::null()).stdout(Stdio::piped());
                        Ok(command.spawn()?)
                    })();
                    match result {
                        Ok(mut process) => {
                            let output = process.stdout.take().context("missing client output")?;
                            let (sender, receiver) = std::sync::mpsc::sync_channel(16);
                            std::thread::spawn(move || {
                                for line in BufReader::new(output).lines().map_while(Result::ok) {
                                    println!("{line}");
                                    if line.contains("desktop connected") {
                                        let _ = sender.try_send(
                                            "Connected. Your native desktop is ready.".to_owned(),
                                        );
                                    }
                                }
                            });
                            progress = Some(receiver);
                            child = Some(process);
                            page = Page::Home;
                            status = "Connecting with the saved host identity...".into();
                        }
                        Err(error) => {
                            status = format!("Connection blocked: {error}");
                            page = Page::Auth;
                        }
                    }
                } else {
                    page = Page::Auth;
                    focused = 0;
                    status = "Choose how to sign in. Passwords are never saved.".into();
                }
            } else {
                status = "Add or select a desktop first.".into();
            }
        }

        canvas.set_draw_color(Color::RGB(12, 18, 27));
        canvas.clear();
        let mouse = events.mouse_state();
        let (w, h) = canvas.window().size();
        let scale = (w as f32 / 1040.0).min(h as f32 / 832.0).max(0.001);
        let pointer = (
            ((mouse.x() as f32 - (w as f32 - 1040.0 * scale) / 2.0) / scale) as i32,
            ((mouse.y() as f32 - (h as f32 - 832.0 * scale) / 2.0) / scale) as i32,
        );
        let mut ui = Painter {
            canvas: &mut canvas,
            textures: &textures,
            font: &font,
            pointer,
        };
        ui.panel(Rect::new(0, 0, 256, 832), Color::RGB(17, 25, 36))?;
        ui.text(&title_font, "teleport", 24, 30, 210, INK)?;
        ui.text(&small_font, "YOUR DESKTOPS", 24, 118, 210, MUTED)?;
        for (index, profile) in profiles.iter().enumerate().skip(host_offset).take(7) {
            let rect = Rect::new(16, 146 + (index - host_offset) as i32 * 60, 224, 52);
            ui.button(
                rect,
                "",
                page == Page::Home && worker.is_none() && approval.is_none(),
                false,
            )?;
            let active = selected
                .as_ref()
                .is_some_and(|p| p.address == profile.address);
            if active {
                ui.panel(Rect::new(rect.x(), rect.y() + 6, 3, 40), ACCENT)?;
            }
            ui.text(&font, profile.label(), 28, rect.y() + 6, 194, INK)?;
            ui.text(&small_font, &profile.address, 28, rect.y() + 30, 194, MUTED)?;
        }
        if profiles.is_empty() {
            ui.text(&font, "No desktops yet", 24, 160, 210, INK)?;
        }
        ui.button(
            NEW_HOST,
            "+ Add desktop",
            page == Page::Home && child.is_none(),
            true,
        )?;
        if cfg!(target_os = "linux") {
            ui.button(HOST_SETTINGS, "Host settings", page == Page::Home, false)?;
        }
        ui.text(&small_font, "Native. Encrypted.", 24, 780, 210, MUTED)?;
        ui.text(&title_font, "Your desktops", 284, 30, 700, INK)?;
        ui.text(
            &font,
            "Save an address now. Sign in when you connect.",
            284,
            78,
            700,
            MUTED,
        )?;
        ui.panel(Rect::new(280, 124, 736, 580), Color::RGB(20, 30, 43))?;
        let caret = started.elapsed().as_millis() % 1000 < 500;
        if let Some(pending) = &approval {
            ui.text(&title_font, "Approve this desktop?", 304, 164, 688, INK)?;
            ui.text(&font, &pending.address, 304, 220, 688, INK)?;
            ui.text(
                &small_font,
                "FIRST CONTACT - SHA-256 HOST FINGERPRINT",
                304,
                276,
                688,
                ACCENT,
            )?;
            ui.text(&font, &pending.fingerprint[..32], 304, 312, 688, INK)?;
            ui.text(&font, &pending.fingerprint[32..], 304, 344, 688, INK)?;
            ui.text(
                &font,
                "Compare this fingerprint using trusted SSH or the host's screen.",
                304,
                400,
                688,
                INK,
            )?;
            ui.text(
                &small_font,
                "An observed certificate alone does not prove who owns this desktop.",
                304,
                430,
                688,
                MUTED,
            )?;
            ui.button(APPROVE, "Approve fingerprint and continue", true, true)?;
            ui.button(REJECT, "Cancel", true, false)?;
        } else {
            match page {
                Page::Add => {
                    ui.text(&title_font, "Add desktop", 304, 148, 688, INK)?;
                    ui.text(&small_font, "NAME", 304, 194, 688, MUTED)?;
                    ui.field(
                        NAME,
                        &name,
                        "My Linux desktop",
                        focused == 0,
                        select_all && focused == 0,
                        caret,
                    )?;
                    ui.text(&small_font, "ADDRESS", 304, 274, 688, MUTED)?;
                    ui.field(
                        ADDRESS,
                        &address,
                        "desktop.local:4443",
                        focused == 1,
                        select_all && focused == 1,
                        caret,
                    )?;
                    ui.button(SAVE, "Save desktop", true, true)?;
                    ui.button(CANCEL_ADD, "Cancel", true, false)?;
                    ui.text(
                        &font,
                        "No password or pairing code is needed to save a desktop.",
                        304,
                        470,
                        688,
                        MUTED,
                    )?;
                }
                Page::Auth => {
                    ui.text(&title_font, "Sign in to your desktop", 304, 148, 688, INK)?;
                    if let Some(profile) = &selected {
                        ui.text(&small_font, &profile.address, 304, 192, 688, MUTED)?;
                    }
                    for (index, (value, label)) in [
                        (AuthMode::Linux, "Linux account"),
                        (AuthMode::Teleport, "Teleport account"),
                        (AuthMode::Code, "Pairing code"),
                        (AuthMode::File, "Pairing file"),
                    ]
                    .into_iter()
                    .enumerate()
                    {
                        ui.button(mode_rect(index), label, worker.is_none(), mode == value)?;
                    }
                    if matches!(mode, AuthMode::Linux | AuthMode::Teleport) {
                        ui.text(&small_font, "USERNAME", 304, 274, 688, MUTED)?;
                        ui.field(
                            USERNAME,
                            &username,
                            if mode == AuthMode::Linux {
                                "Linux username, for example ej"
                            } else {
                                "Teleport username"
                            },
                            focused == 0,
                            select_all && focused == 0,
                            caret,
                        )?;
                    } else {
                        ui.text(
                            &font,
                            if mode == AuthMode::Code {
                                "Enter the six-digit code displayed by the host."
                            } else {
                                "Choose a private pairing file from a trusted source."
                            },
                            304,
                            310,
                            688,
                            MUTED,
                        )?;
                    }
                    let masked = "*".repeat(secret.chars().count().min(256));
                    ui.field(
                        SECRET,
                        if matches!(mode, AuthMode::Linux | AuthMode::Teleport) {
                            &masked
                        } else {
                            &secret
                        },
                        match mode {
                            AuthMode::Linux => "Current Linux password",
                            AuthMode::Teleport => "Teleport password",
                            AuthMode::Code => "Six-digit pairing code",
                            AuthMode::File => "Path to pairing file",
                        },
                        focused == 1,
                        select_all && focused == 1,
                        caret,
                    )?;
                    ui.button(
                        AUTH,
                        if worker.is_some() {
                            "Please wait..."
                        } else {
                            "Continue"
                        },
                        worker.is_none(),
                        true,
                    )?;
                    ui.button(CANCEL_AUTH, "Cancel", worker.is_none(), false)?;
                    ui.text(&small_font,if mode==AuthMode::Linux{"Linux login is session-only. We remember the desktop, never the password."}else{"Successful pairing saves device access. Your password is never stored."},304,550,688,MUTED)?;
                }
                Page::Home => {
                    ui.text(
                        &title_font,
                        selected
                            .as_ref()
                            .map_or("Ready when you are", |p| p.label()),
                        304,
                        156,
                        688,
                        INK,
                    )?;
                    ui.text(
                        &font,
                        selected
                            .as_ref()
                            .map_or("Add your first desktop to get started.", |p| {
                                p.address.as_str()
                            }),
                        304,
                        212,
                        688,
                        MUTED,
                    )?;
                    ui.button(
                        CONNECT,
                        if child.is_some() {
                            "Session running"
                        } else {
                            "Connect"
                        },
                        selected.is_some() && child.is_none() && worker.is_none(),
                        true,
                    )?;
                    ui.button(DISCONNECT, "Disconnect", child.is_some(), false)?;
                    ui.button(
                        SETTINGS,
                        if settings {
                            "Hide settings"
                        } else {
                            "Connection settings"
                        },
                        true,
                        false,
                    )?;
                    ui.button(
                        SIGN_IN,
                        "Sign in again",
                        selected.is_some() && child.is_none(),
                        false,
                    )?;
                    if settings {
                        ui.text(&small_font, "STREAM SETTINGS", 304, 420, 688, MUTED)?;
                        let labels = [
                            [
                                "Native resolution",
                                "1080p width",
                                "1440p width",
                                "720p width",
                                "4K width",
                            ][resolution],
                            ["30 fps", "60 fps", "120 fps"][frame_rate],
                            ["8 Mbps", "20 Mbps", "40 Mbps"][quality],
                            ["H.264", "H.265"][codec],
                            if hdr { "HDR preview" } else { "SDR" },
                            if clipboard {
                                "Clipboard on"
                            } else {
                                "Clipboard off"
                            },
                            if mute { "Audio muted" } else { "Audio on" },
                            if agent_consent.enabled_for(
                                selected
                                    .as_ref()
                                    .map_or("", |profile| profile.address.as_str()),
                            ) {
                                "SSH agent on"
                            } else if agent_consent.awaiting_for(
                                selected
                                    .as_ref()
                                    .map_or("", |profile| profile.address.as_str()),
                                std::time::Instant::now(),
                            ) {
                                "Confirm SSH"
                            } else {
                                "SSH agent off"
                            },
                        ];
                        for (index, label) in labels.into_iter().enumerate() {
                            ui.button(
                                pref_rect(index),
                                label,
                                child.is_none() && (index != 4 || cfg!(target_os = "macos")),
                                false,
                            )?;
                        }
                    } else {
                        ui.text(
                            &font,
                            "Your connection stays native and encrypted.",
                            304,
                            438,
                            688,
                            MUTED,
                        )?;
                        ui.text(&small_font,"Resolution, video quality, clipboard and SSH sharing are in Connection settings.",304,474,688,MUTED)?;
                    }
                }
            }
        }
        ui.panel(Rect::new(280, 716, 736, 70), Color::RGB(17, 37, 44))?;
        let surface = small_font.render(&status).blended_wrapped(INK, 688)?;
        let texture = textures.create_texture_from_surface(&surface)?;
        let height = surface.height().min(54);
        ui.canvas
            .copy(
                &texture,
                Some(Rect::new(0, 0, surface.width(), height)),
                Rect::new(304, 726, surface.width(), height),
            )
            .map_err(anyhow::Error::msg)?;
        canvas.present();
        // Queued input preceding the first actual presentation cannot consent.
        if let Some(pending) = approval.as_mut() {
            pending.presented = true;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    secret.zeroize();
    if ephemeral.is_some()
        && let Some(mut process) = child.take()
    {
        let _ = process.kill();
        let _ = process.wait();
    }
    Ok(())
}
const INK: Color = Color::RGB(231, 240, 246);
const MUTED: Color = Color::RGB(146, 166, 185);
const ACCENT: Color = Color::RGB(91, 223, 201);

fn hit(rect: Rect, x: i32, y: i32) -> bool {
    rect.contains_point((x, y))
}
fn shortcut(modifiers: sdl2::keyboard::Mod) -> bool {
    modifiers.intersects(
        sdl2::keyboard::Mod::LCTRLMOD
            | sdl2::keyboard::Mod::RCTRLMOD
            | sdl2::keyboard::Mod::LGUIMOD
            | sdl2::keyboard::Mod::RGUIMOD,
    )
}

struct Painter<'a, 'ttf> {
    canvas: &'a mut sdl2::render::Canvas<sdl2::video::Window>,
    textures: &'a sdl2::render::TextureCreator<sdl2::video::WindowContext>,
    font: &'a sdl2::ttf::Font<'ttf, 'static>,
    pointer: (i32, i32),
}

impl Painter<'_, '_> {
    fn panel(&mut self, rect: Rect, color: Color) -> Result<()> {
        self.canvas.set_draw_color(color);
        self.canvas.fill_rect(rect).map_err(anyhow::Error::msg)
    }

    fn text(
        &mut self,
        font: &sdl2::ttf::Font<'_, '_>,
        text: &str,
        x: i32,
        y: i32,
        max_width: u32,
        color: Color,
    ) -> Result<()> {
        if text.is_empty() {
            return Ok(());
        }
        let mut visible = text.to_owned();
        if font.size_of(&visible)?.0 > max_width {
            while !visible.is_empty() && font.size_of(&format!("{visible}…"))?.0 > max_width {
                visible.pop();
            }
            visible.push('…');
        }
        let surface = font.render(&visible).blended(color)?;
        let texture = self.textures.create_texture_from_surface(&surface)?;
        self.canvas
            .copy(
                &texture,
                None,
                Rect::new(x, y, surface.width(), surface.height()),
            )
            .map_err(anyhow::Error::msg)
    }

    fn button(&mut self, rect: Rect, text: &str, enabled: bool, primary: bool) -> Result<()> {
        let hovered = enabled && rect.contains_point(self.pointer);
        let color = if !enabled {
            Color::RGB(30, 40, 52)
        } else if primary {
            if hovered {
                Color::RGB(109, 239, 217)
            } else {
                ACCENT
            }
        } else if hovered {
            Color::RGB(51, 70, 89)
        } else {
            Color::RGB(34, 49, 66)
        };
        self.panel(rect, color)?;
        let ink = if !enabled {
            MUTED
        } else if primary {
            Color::RGB(12, 36, 39)
        } else {
            INK
        };
        let width = self
            .font
            .size_of(text)?
            .0
            .min(rect.width().saturating_sub(24));
        self.text(
            self.font,
            text,
            rect.x() + ((rect.width() - width) / 2) as i32,
            rect.y() + ((rect.height() as i32 - self.font.height()) / 2),
            rect.width().saturating_sub(24),
            ink,
        )
    }

    fn field(
        &mut self,
        rect: Rect,
        value: &str,
        placeholder: &str,
        focused: bool,
        selected: bool,
        caret: bool,
    ) -> Result<()> {
        self.panel(rect, Color::RGB(10, 19, 29))?;
        self.canvas.set_draw_color(if focused {
            ACCENT
        } else {
            Color::RGB(51, 68, 85)
        });
        self.canvas.draw_rect(rect).map_err(anyhow::Error::msg)?;
        if selected {
            self.panel(
                Rect::new(
                    rect.x() + 10,
                    rect.y() + 8,
                    rect.width() - 20,
                    rect.height() - 16,
                ),
                Color::RGB(37, 85, 105),
            )?;
        }
        let mut visible = value.to_owned();
        while !visible.is_empty() && self.font.size_of(&visible)?.0 > rect.width() - 32 {
            visible.remove(0);
        }
        self.text(
            self.font,
            if value.is_empty() {
                placeholder
            } else {
                &visible
            },
            rect.x() + 12,
            rect.y() + ((rect.height() as i32 - self.font.height()) / 2),
            rect.width() - 24,
            if value.is_empty() { MUTED } else { INK },
        )?;
        if focused && caret {
            let width = self.font.size_of(&visible)?.0;
            self.panel(
                Rect::new(
                    rect.x() + 12 + width as i32,
                    rect.y() + 10,
                    2,
                    rect.height() - 20,
                ),
                ACCENT,
            )?;
        }
        Ok(())
    }
}

fn append_code(code: &mut String, text: &str) {
    // Do not silently convert arbitrary clipboard text into a valid code.
    if text
        .bytes()
        .all(|b| b.is_ascii_digit() || b.is_ascii_whitespace() || b == b'-')
    {
        let digits: String = text.chars().filter(char::is_ascii_digit).collect();
        if code.len() + digits.len() <= 6 {
            code.push_str(&digits);
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn host_approval_requires_presented_prompt_and_single_deliberate_click() {
        let mut approval = super::Approval {
            address: "desktop:4443".into(),
            fingerprint: "aa".repeat(32),
            pairing: None,
            presented: false,
        };
        assert!(!approval.accepts_click(1));
        assert!(!approval.accepts_click(2));
        approval.presented = true;
        assert!(approval.accepts_click(1));
        assert!(!approval.accepts_click(2));
        assert!(!approval.accepts_click(3));
        let submit = sdl2::rect::Rect::new(304, 460, 448, 44);
        assert!(!super::approval_confirm_rect().has_intersection(submit));
        assert!(!super::approval_confirm_rect().contains_point((500, 480)));
    }

    #[test]
    fn ssh_consent_and_pending_confirmation_are_bound_to_exact_address() {
        let mut consent = super::AgentConsent::default();
        let now = std::time::Instant::now();
        consent.click("first:4443", now);
        assert!(consent.awaiting_for("first:4443", now));
        assert!(!consent.awaiting_for("second:4443", now));
        consent.click("second:4443", now);
        assert!(!consent.enabled_for("second:4443"));
        consent.click("second:4443", now);
        assert!(consent.enabled_for("second:4443"));
        // Even before the UI's next select/clear, spawning another address is forbidden.
        assert!(!consent.enabled_for("first:4443"));
        consent.select("first:4443");
        assert!(!consent.enabled_for("second:4443"));
        assert!(!consent.awaiting_for("second:4443", now));
        consent.click("first:4443", now);
        consent.clear(); // Add desktop, disconnect, or session exit.
        consent.click("first:4443", now);
        assert!(!consent.enabled_for("first:4443"));
        assert!(consent.awaiting_for("first:4443", now));
        consent.click("first:4443", now + std::time::Duration::from_secs(11));
        assert!(!consent.enabled_for("first:4443"));
    }

    #[test]
    fn linux_login_pin_requires_explicit_trust_and_session_file_is_temporary() -> anyhow::Result<()>
    {
        let pairing = crate::protocol::Pairing {
            token: "a".repeat(64),
            fingerprint: "b".repeat(64),
        };
        let (profile, directory) = super::session_profile("desktop:4443", &pairing)?;
        crate::profiles::check_private_file(profile.pairing_file.as_ref().unwrap())?;
        assert_eq!(
            crate::protocol::Pairing::read(profile.pairing_file.as_ref().unwrap())?.token,
            pairing.token
        );
        assert!(super::system_fingerprint("desktop:4443", "", &[]).is_err());
        assert!(
            super::system_fingerprint("alias:4443", "", std::slice::from_ref(&profile)).is_err()
        );
        assert_eq!(
            super::system_fingerprint("desktop:4443", "", std::slice::from_ref(&profile))?,
            pairing.fingerprint
        );
        assert_eq!(
            super::system_fingerprint("alias:4443", &"C".repeat(64), &[])?,
            "c".repeat(64)
        );
        assert!(
            super::system_fingerprint("desktop:4443", "bad", std::slice::from_ref(&profile))
                .is_err()
        );
        let path = profile.pairing_file.clone().unwrap();
        drop(directory);
        assert!(!path.exists());
        Ok(())
    }
    #[test]
    fn hit_testing_excludes_margins_and_adjacent_controls() {
        let button = sdl2::rect::Rect::new(304, 272, 448, 44);
        assert!(super::hit(button, 304, 272));
        assert!(super::hit(button, 751, 315));
        assert!(!super::hit(button, 303, 272));
        assert!(!super::hit(button, 752, 272));
        assert!(!super::hit(button, 304, 316));
    }

    #[test]
    fn code_entry_is_bounded_and_normalizes_separators() {
        let mut code = String::new();
        super::append_code(&mut code, "123-456\n");
        assert_eq!(code, "123456");
        super::append_code(&mut code, "7");
        assert_eq!(code, "123456");
        code.clear();
        super::append_code(&mut code, "code 123456");
        assert!(code.is_empty());
        super::append_code(&mut code, "1234567");
        assert!(code.is_empty());
    }
}
