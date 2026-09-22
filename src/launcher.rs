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

type PairingReceiver = std::sync::mpsc::Receiver<(String, Result<crate::protocol::Pairing>)>;

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

#[allow(non_snake_case)]
pub fn run() -> Result<()> {
    let ADDRESS = Rect::new(304, 178, 688, 48);
    let CONNECT = Rect::new(304, 272, 448, 44);
    let DISCONNECT = Rect::new(764, 272, 228, 44);
    let CLIPBOARD = Rect::new(304, 326, 228, 28);
    let MUTE = Rect::new(544, 326, 176, 28);
    let SSH_AGENT = Rect::new(732, 326, 260, 28);
    let PAIR_FIELD = Rect::new(304, 582, 448, 44);
    let PAIR_BUTTON = Rect::new(764, 582, 228, 44);
    let FILE_MODE = Rect::new(304, 632, 220, 26);
    let PASSWORD_MODE = Rect::new(540, 632, 220, 26);
    let USERNAME = Rect::new(304, 546, 448, 30);
    let HOST_SETTINGS = Rect::new(16, 704, 224, 40);
    let TRUST_ALIAS = Rect::new(764, 632, 228, 26);
    let NEW_HOST = Rect::new(16, 592, 224, 44);
    let RESOLUTION = Rect::new(304, 428, 180, 38);
    let FPS = Rect::new(496, 428, 88, 38);
    let QUALITY = Rect::new(596, 428, 106, 38);
    let CODEC = Rect::new(714, 428, 110, 38);
    let RANGE = Rect::new(836, 428, 156, 38);
    let sdl = sdl2::init().map_err(anyhow::Error::msg)?;
    let video = sdl.video().map_err(anyhow::Error::msg)?;
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
    let mut canvas = window.into_canvas().software().build()?;
    canvas.set_logical_size(1040, 832)?;
    let textures = canvas.texture_creator();
    let mut events = sdl.event_pump().map_err(anyhow::Error::msg)?;
    video.text_input().start();
    let mut profiles = crate::profiles::load()?;
    let mut selected = profiles.last().cloned();
    let mut address = selected
        .as_ref()
        .map(|p| p.address.clone())
        .unwrap_or_default();
    let mut pairing_path = String::new();
    let mut pairing_code = String::new();
    let mut username = String::new();
    let mut password = Zeroizing::new(String::new());
    let mut use_password = true;
    let mut use_file = false;
    let mut pending_pairing: Option<PairingReceiver> = None;
    let mut focused = 0;
    let mut select_all = false;
    let mut host_offset = profiles.len().saturating_sub(7);
    let started = std::time::Instant::now();
    let mut status = if selected.is_some() {
        "Saved host ready. Click Connect; no pairing code needed."
    } else {
        "Log in with your Teleport account, or choose a one-time pairing code."
    }
    .to_owned();
    let mut child: Option<Child> = None;
    let mut progress = None;
    let mut clipboard = false;
    let mut mute = false;
    let mut forward_agent = false;
    let mut agent_confirmation: Option<std::time::Instant> = None;
    let mut agent_target = String::new();
    let mut resolution = 0;
    let mut frame_rate = 1;
    let mut quality = 1;
    let mut codec = 0;
    let mut hdr = false;
    'ui: loop {
        if let Some(receiver) = &pending_pairing {
            match receiver.try_recv() {
                Ok((host, result)) => {
                    pending_pairing = None;
                    pairing_code.clear();
                    password.zeroize();
                    match result.and_then(|pairing| {
                        crate::profiles::save_pairing(&host, &pairing, &mut profiles)
                    }) {
                        Ok(profile) => {
                            forward_agent = false;
                            agent_confirmation = None;
                            address = host;
                            selected = Some(profile);
                            host_offset = profiles.len().saturating_sub(7);
                            status =
                                "Host verified and saved. Click Connect; no code needed next time."
                                    .into();
                        }
                        Err(_) => {
                            status = if use_password {
                                "Login failed. Check your Teleport account, host address, and TCP access."
                            } else {
                                "Pairing failed. Check host/code, TCP port, or open a new code in Host settings."
                            }.into();
                        }
                    }
                }
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    pending_pairing = None;
                    pairing_code.clear();
                    password.zeroize();
                    status = "Pairing worker stopped. Start a new pairing attempt.".into();
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => {}
            }
        }
        if let Some(receiver) = &progress {
            while let Ok(message) = std::sync::mpsc::Receiver::<String>::try_recv(receiver) {
                status = message;
            }
        }
        if let Some(process) = child.as_mut()
            && let Some(exit) = process.try_wait()?
        {
            status = if exit.success() {
                "Session ended. Connect to start again.".into()
            } else {
                format!(
                    "Connection exited ({exit}). Check host, pairing and UDP 4443; see terminal for details."
                )
            };
            child = None;
            progress = None;
            forward_agent = false;
            agent_confirmation = None;
        }
        for event in events.poll_iter() {
            if agent_target != address {
                forward_agent = false;
                agent_confirmation = None;
            }
            let mut action = None;
            match event {
                Event::Quit { .. } => break 'ui,
                Event::TextInput { text, .. } => {
                    let field = if focused == 0 {
                        &mut address
                    } else if focused == 2 {
                        &mut username
                    } else if use_password {
                        &mut *password
                    } else if !use_file {
                        &mut pairing_code
                    } else {
                        &mut pairing_path
                    };
                    if select_all {
                        field.zeroize();
                        select_all = false;
                    }
                    if focused == 1 && !use_file && !use_password {
                        append_code(field, &text);
                    } else if field.len() + text.len() <= 2048 {
                        field.push_str(&text);
                    }
                }
                Event::DropFile { filename, .. } => {
                    pairing_path = filename;
                    use_file = true;
                    use_password = false;
                    password.zeroize();
                    focused = 1;
                }
                Event::KeyDown {
                    keycode: Some(Keycode::Tab),
                    ..
                } => {
                    focused = if use_password {
                        match focused {
                            0 => 2,
                            2 => 1,
                            _ => 0,
                        }
                    } else {
                        1 - focused.min(1)
                    };
                    select_all = false;
                }
                Event::KeyDown {
                    keycode: Some(Keycode::Return | Keycode::KpEnter),
                    ..
                } => {
                    action = Some(
                        if selected
                            .as_ref()
                            .is_some_and(|p| p.address == address.trim())
                            && focused == 0
                        {
                            2
                        } else {
                            0
                        },
                    );
                }
                Event::KeyDown {
                    keycode: Some(Keycode::A),
                    keymod,
                    ..
                } if shortcut(keymod) => select_all = true,
                Event::KeyDown {
                    keycode: Some(Keycode::Backspace),
                    ..
                } => {
                    if select_all {
                        if focused == 0 {
                            address.clear();
                        } else if focused == 2 {
                            username.clear();
                        } else if use_password {
                            password.zeroize();
                        } else if use_file {
                            pairing_path.clear();
                        } else {
                            pairing_code.clear();
                        }
                        select_all = false;
                        continue;
                    }
                    if focused == 0 {
                        address.pop();
                    } else if focused == 2 {
                        username.pop();
                    } else if use_password {
                        password.pop();
                    } else if !use_file {
                        pairing_code.pop();
                    } else {
                        pairing_path.pop();
                    }
                }
                Event::KeyDown {
                    keycode: Some(Keycode::V),
                    keymod,
                    ..
                } if keymod.intersects(
                    sdl2::keyboard::Mod::LCTRLMOD
                        | sdl2::keyboard::Mod::RCTRLMOD
                        | sdl2::keyboard::Mod::LGUIMOD
                        | sdl2::keyboard::Mod::RGUIMOD,
                ) =>
                {
                    if let Ok(text) = video.clipboard().clipboard_text() {
                        select_all = false;
                        let field = if focused == 0 {
                            &mut address
                        } else if focused == 2 {
                            &mut username
                        } else if use_password {
                            &mut *password
                        } else if !use_file {
                            &mut pairing_code
                        } else {
                            &mut pairing_path
                        };
                        if focused == 1 && !use_file && !use_password {
                            field.clear();
                            append_code(field, &text);
                        } else if text.len() <= 2048 {
                            field.zeroize();
                            *field = if focused == 1 && use_password {
                                text
                            } else {
                                text.trim().to_owned()
                            };
                        }
                    }
                }
                Event::MouseWheel { y, .. } => {
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
                    ..
                } => {
                    select_all = false;
                    if child.is_none() {
                        if hit(RESOLUTION, x, y) {
                            resolution = (resolution + 1) % 5;
                        }
                        if hit(FPS, x, y) {
                            frame_rate = (frame_rate + 1) % 3;
                        }
                        if hit(QUALITY, x, y) {
                            quality = (quality + 1) % 3;
                        }
                        if hit(CODEC, x, y) {
                            codec = (codec + 1) % 2;
                            if codec == 0 {
                                hdr = false;
                            }
                        }
                        if hit(RANGE, x, y) && cfg!(target_os = "macos") {
                            hdr = !hdr;
                            if hdr {
                                codec = 1;
                                status = "Experimental HDR requires patched KWin and an HDR-capable Mac display.".into();
                            }
                        }
                    }
                    if hit(NEW_HOST, x, y) {
                        forward_agent = false;
                        agent_confirmation = None;
                        selected = None;
                        address.clear();
                        pairing_code.clear();
                        password.zeroize();
                        focused = 0;
                        status =
                            "Enter your host address, then log in or use a one-time code.".into();
                    }
                    for (index, profile) in profiles.iter().enumerate().skip(host_offset).take(7) {
                        if hit(
                            Rect::new(16, 146 + ((index - host_offset) as i32) * 60, 224, 52),
                            x,
                            y,
                        ) {
                            selected = Some(profile.clone());
                            forward_agent = false;
                            agent_confirmation = None;
                            address = profile.address.clone();
                            focused = 0;
                            status =
                                "Saved identity selected. Ready for a secure connection.".into();
                        }
                    }
                    if hit(FILE_MODE, x, y) && pending_pairing.is_none() {
                        use_file = !use_file;
                        use_password = false;
                        password.zeroize();
                        focused = 1;
                    }
                    if hit(PASSWORD_MODE, x, y) && pending_pairing.is_none() {
                        use_password = !use_password;
                        use_file = false;
                        password.zeroize();
                        focused = if use_password { 2 } else { 1 };
                    }
                    if hit(USERNAME, x, y) && use_password {
                        focused = 2;
                    }
                    if hit(TRUST_ALIAS, x, y)
                        && child.is_none()
                        && pending_pairing.is_none()
                        && let Some(profile) =
                            selected.as_ref().filter(|p| p.address != address.trim())
                    {
                        let credential = profile.pairing_file.clone();
                        match crate::profiles::import(address.trim(), &credential, &mut profiles) {
                            Ok(profile) => {
                                selected = Some(profile);
                                host_offset = profiles.len().saturating_sub(7);
                                status = "Saved this address with the selected host's identity. TLS identity must still match.".into();
                            }
                            Err(error) => status = format!("Could not save address: {error}"),
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
                            Err(error) => status = format!("Could not open host settings: {error}"),
                        }
                    }
                    if hit(ADDRESS, x, y) {
                        focused = 0;
                    }
                    if hit(PAIR_FIELD, x, y) {
                        focused = 1;
                    }
                    if hit(PAIR_BUTTON, x, y) && pending_pairing.is_none() {
                        action = Some(0);
                    }
                    if hit(CLIPBOARD, x, y) && child.is_none() {
                        clipboard = !clipboard;
                    }
                    if hit(MUTE, x, y) && child.is_none() {
                        mute = !mute;
                    }
                    if hit(SSH_AGENT, x, y) && child.is_none() {
                        if forward_agent {
                            forward_agent = false;
                            agent_confirmation = None;
                            status = "SSH agent forwarding disabled.".into();
                        } else if agent_confirmation
                            .is_some_and(|at| at.elapsed() < Duration::from_secs(10))
                        {
                            forward_agent = true;
                            agent_confirmation = None;
                            status = "SSH agent enabled for the next session only. Disconnect ends forwarding. Host must also allow it.".into();
                        } else {
                            agent_target = address.clone();
                            agent_confirmation = Some(std::time::Instant::now());
                            status = "Trust this host: it can request SSH authentication with your loaded keys. Click Confirm SSH agent within 10 seconds to enable.".into();
                        }
                    }
                    if hit(CONNECT, x, y)
                        && child.is_none()
                        && selected
                            .as_ref()
                            .is_some_and(|p| p.address == address.trim())
                    {
                        action = Some(2);
                    }
                    if hit(DISCONNECT, x, y) && child.is_some() {
                        action = Some(3);
                    }
                }
                _ => {}
            }
            match action {
                Some(0) if pending_pairing.is_some() => {}
                Some(0) if use_password => {
                    let host = address.trim().to_owned();
                    if let Err(error) = crate::profiles::validate_address(&host) {
                        status = format!("Invalid address: {error}");
                    } else if username.trim().is_empty() || password.is_empty() {
                        status =
                            "Enter your Teleport username and password (not your Linux login)."
                                .into();
                    } else {
                        let username = username.trim().to_owned();
                        let secret = Zeroizing::new(std::mem::take(&mut *password));
                        let (sender, receiver) = std::sync::mpsc::channel();
                        std::thread::spawn(move || {
                            let result = tokio::runtime::Builder::new_current_thread()
                                .enable_all()
                                .build()
                                .map_err(anyhow::Error::from)
                                .and_then(|runtime| {
                                    runtime.block_on(crate::access::login(
                                        &host,
                                        &username,
                                        &secret,
                                        &format!("{} client", std::env::consts::OS),
                                    ))
                                });
                            let _ = sender.send((host, result));
                        });
                        pending_pairing = Some(receiver);
                        status = "Authenticating account and verifying host…".into();
                    }
                }
                Some(0) if !use_file => {
                    let host = address.trim().to_owned();
                    if let Err(error) = crate::profiles::validate_address(&host) {
                        status = format!("Invalid address: {error}");
                    } else if pairing_code.len() != 6 {
                        status = "Enter the six-digit code shown by your Linux host.".into();
                    } else {
                        let code = std::mem::take(&mut pairing_code);
                        let (sender, receiver) = std::sync::mpsc::channel();
                        std::thread::spawn(move || {
                            let result = tokio::runtime::Builder::new_current_thread()
                                .enable_all()
                                .build()
                                .map_err(anyhow::Error::from)
                                .and_then(|runtime| {
                                    runtime.block_on(crate::pairing::pair(&host, &code))
                                });
                            let _ = sender.send((host, result));
                        });
                        pending_pairing = Some(receiver);
                        status = "Verifying host with your code…".into();
                    }
                }
                Some(0) => match crate::profiles::import(
                    address.trim(),
                    &PathBuf::from(pairing_path.trim()),
                    &mut profiles,
                ) {
                    Ok(profile) => {
                        selected = Some(profile);
                        host_offset = profiles.len().saturating_sub(7);
                        status = "Pairing imported and trusted. Ready to connect.".into();
                        forward_agent = false;
                        agent_confirmation = None;
                    }
                    Err(error) => status = format!("Import failed: {error}"),
                },
                Some(2) if child.is_none() => {
                    if let Some(profile) = selected.as_ref().filter(|p| p.address == address.trim())
                    {
                        if let Err(error) =
                            crate::profiles::check_private_file(&profile.pairing_file)
                        {
                            status = format!("Pairing unavailable: {error}");
                            continue;
                        }
                        let mut command = Command::new(std::env::current_exe()?);
                        command
                            .arg("client")
                            .arg(&profile.address)
                            .arg("--pairing-file")
                            .arg(&profile.pairing_file)
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
                        if forward_agent {
                            command.arg("--forward-ssh-agent");
                        } else {
                            command.arg("--reconnect");
                        }
                        if clipboard {
                            command.arg("--clipboard");
                        }
                        if mute {
                            command.arg("--mute");
                        }
                        command.stdin(Stdio::null()).stdout(Stdio::piped());
                        match command.spawn() {
                            Ok(mut process) => {
                                let output = process
                                    .stdout
                                    .take()
                                    .context("client output pipe missing")?;
                                let (sender, receiver) = std::sync::mpsc::sync_channel(16);
                                std::thread::spawn(move || {
                                    for line in BufReader::new(output).lines().map_while(Result::ok)
                                    {
                                        // Only fixed, non-sensitive statuses reach the UI.
                                        println!("{line}");
                                        let message = if line.contains("desktop connected") {
                                            Some("Connected — native desktop session is ready.")
                                        } else if line.contains("connecting with pinned TLS") {
                                            Some("Connecting with the host's trusted certificate…")
                                        } else if line.contains("reconnect")
                                            || line.contains("connection failed")
                                        {
                                            Some(
                                                "Connection interrupted; retrying. Check host/network or disconnect.",
                                            )
                                        } else {
                                            None
                                        };
                                        if let Some(message) = message {
                                            let _ = sender.try_send(message.to_owned());
                                        }
                                    }
                                });
                                progress = Some(receiver);
                                child = Some(process);
                                status =
                                    "Connecting… session opens in a separate native window.".into();
                            }
                            Err(error) => status = format!("Could not launch client: {error}"),
                        }
                    } else {
                        status =
                            "Log in to this host, use its one-time code, or import a pairing file."
                                .into();
                    }
                }
                Some(3) => {
                    if let Some(mut process) = child.take() {
                        let _ = process.kill();
                        let _ = process.wait();
                    }
                    status = "Disconnected. Connect to start again.".into();
                    progress = None;
                }
                _ => {}
            }
        }
        canvas.set_draw_color(Color::RGB(12, 18, 27));
        canvas.clear();
        let mouse = events.mouse_state();
        // SDL transforms mouse events to logical coordinates, but MouseState
        // remains in window coordinates. Apply the same letterbox transform
        // for hover feedback after a resize (including Retina windows).
        let (window_width, window_height) = canvas.window().size();
        let scale = (window_width as f32 / 1040.0)
            .min(window_height as f32 / 832.0)
            .max(0.001);
        let pointer = (
            ((mouse.x() as f32 - (window_width as f32 - 1040.0 * scale) / 2.0) / scale) as i32,
            ((mouse.y() as f32 - (window_height as f32 - 832.0 * scale) / 2.0) / scale) as i32,
        );
        let mut ui = Painter {
            canvas: &mut canvas,
            textures: &textures,
            font: &font,
            pointer,
        };
        ui.panel(Rect::new(0, 0, 256, 832), Color::RGB(17, 25, 36))?;
        ui.panel(Rect::new(280, 124, 736, 240), Color::RGB(20, 30, 43))?;
        ui.panel(Rect::new(280, 388, 736, 92), Color::RGB(20, 30, 43))?;
        ui.panel(Rect::new(280, 500, 736, 194), Color::RGB(20, 30, 43))?;
        ui.text(
            &small_font,
            "STREAM PREFERENCES · APPLIED ON CONNECTION",
            304,
            402,
            680,
            MUTED,
        )?;
        ui.button(
            RESOLUTION,
            [
                "Native resolution",
                "1080p width",
                "1440p width",
                "720p width",
                "4K width",
            ][resolution],
            child.is_none(),
            false,
        )?;
        ui.button(
            FPS,
            ["30 fps", "60 fps", "120 fps"][frame_rate],
            child.is_none(),
            false,
        )?;
        ui.button(
            QUALITY,
            ["8 Mbps", "20 Mbps", "40 Mbps"][quality],
            child.is_none(),
            false,
        )?;
        ui.button(CODEC, ["H.264", "H.265"][codec], child.is_none(), false)?;
        ui.button(
            RANGE,
            if hdr { "HDR · preview" } else { "SDR" },
            child.is_none() && cfg!(target_os = "macos"),
            hdr,
        )?;
        ui.text(&title_font, "teleport", 24, 30, 210, INK)?;
        ui.text(&small_font, "YOUR DESKTOP, ANYWHERE", 24, 74, 220, MUTED)?;
        ui.text(&small_font, "SAVED HOSTS", 24, 118, 210, MUTED)?;
        if profiles.is_empty() {
            ui.text(&font, "No hosts yet", 24, 160, 205, INK)?;
            ui.text(
                &small_font,
                "Pair once. Connect anytime.",
                24,
                188,
                210,
                MUTED,
            )?;
        }
        for (index, profile) in profiles.iter().enumerate().skip(host_offset).take(7) {
            let rect = Rect::new(16, 146 + ((index - host_offset) as i32) * 60, 224, 52);
            let active = selected
                .as_ref()
                .is_some_and(|p| p.address == profile.address);
            ui.button(rect, "", true, false)?;
            if active {
                ui.panel(
                    Rect::new(rect.x(), rect.y() + 6, 3, rect.height() - 12),
                    ACCENT,
                )?;
            }
            ui.text(&font, &profile.address, 28, rect.y() + 6, 194, INK)?;
            ui.text(
                &small_font,
                if active {
                    "Selected · verified identity"
                } else {
                    "Verified identity"
                },
                28,
                rect.y() + 30,
                194,
                if active { ACCENT } else { MUTED },
            )?;
        }
        ui.button(NEW_HOST, "+  Add a host", true, false)?;
        ui.text(&small_font, "Native · Encrypted", 24, 678, 220, MUTED)?;
        ui.text(&title_font, "Your next connection", 284, 30, 700, INK)?;
        ui.text(
            &font,
            "Pick a saved desktop, or securely pair a new one.",
            284,
            78,
            700,
            MUTED,
        )?;
        ui.text(&small_font, "HOST ADDRESS", 304, 148, 500, MUTED)?;
        ui.field(
            ADDRESS,
            &address,
            "desktop.local:4443",
            focused == 0,
            select_all && focused == 0,
            started.elapsed().as_millis() % 1000 < 500,
        )?;
        ui.text(
            &small_font,
            if selected
                .as_ref()
                .is_some_and(|p| p.address == address.trim())
            {
                "Verified host · identity checked on every connection"
            } else {
                "New address · log in below, or explicitly reuse the selected host's identity"
            },
            304,
            238,
            680,
            MUTED,
        )?;
        let ready = selected
            .as_ref()
            .is_some_and(|p| p.address == address.trim());
        ui.button(
            CONNECT,
            if child.is_some() {
                "Session running"
            } else {
                "Connect to desktop"
            },
            ready && child.is_none(),
            true,
        )?;
        ui.button(DISCONNECT, "Disconnect", child.is_some(), false)?;
        ui.button(
            SSH_AGENT,
            if forward_agent {
                "[x] SSH agent (session)"
            } else if agent_confirmation.is_some_and(|at| at.elapsed() < Duration::from_secs(10)) {
                "Confirm SSH agent"
            } else {
                "[ ] SSH agent"
            },
            child.is_none(),
            forward_agent,
        )?;
        ui.button(
            CLIPBOARD,
            if clipboard {
                "[x] Share clipboard"
            } else {
                "[ ] Share clipboard"
            },
            child.is_none(),
            false,
        )?;
        ui.button(
            MUTE,
            if mute {
                "[x] Mute audio"
            } else {
                "[ ] Mute audio"
            },
            child.is_none(),
            false,
        )?;
        ui.text(
            &font,
            if use_password {
                "Sign in to your host"
            } else {
                "Pair a new host"
            },
            304,
            520,
            400,
            INK,
        )?;
        if use_password {
            ui.field(
                USERNAME,
                &username,
                "Teleport username",
                focused == 2,
                select_all && focused == 2,
                started.elapsed().as_millis() % 1000 < 500,
            )?;
        } else {
            ui.text(
                &small_font,
                "Run teleport host --pair on Linux. Enter the code shown there.",
                304,
                550,
                680,
                MUTED,
            )?;
        }
        let masked = "•".repeat(password.chars().count().min(256));
        ui.field(
            PAIR_FIELD,
            if use_password {
                &masked
            } else if use_file {
                &pairing_path
            } else {
                &pairing_code
            },
            if use_password {
                "Persistent password / passphrase"
            } else if use_file {
                "Paste or drop a pairing file"
            } else {
                "Six-digit code"
            },
            focused == 1,
            select_all && focused == 1,
            started.elapsed().as_millis() % 1000 < 500,
        )?;
        ui.button(
            PAIR_BUTTON,
            if pending_pairing.is_some() {
                "Verifying…"
            } else if use_password {
                "Log in & save"
            } else if use_file {
                "Import & trust"
            } else {
                "Pair & save"
            },
            pending_pairing.is_none(),
            false,
        )?;
        ui.button(
            FILE_MODE,
            if use_file {
                "Use one-time code"
            } else {
                "Import pairing file"
            },
            pending_pairing.is_none(),
            false,
        )?;
        ui.button(
            PASSWORD_MODE,
            if use_password {
                "Use one-time code"
            } else {
                "Use password"
            },
            pending_pairing.is_none(),
            false,
        )?;
        if selected
            .as_ref()
            .is_some_and(|p| p.address != address.trim())
        {
            ui.button(
                TRUST_ALIAS,
                "Use saved identity",
                child.is_none() && pending_pairing.is_none(),
                false,
            )?;
        }
        if cfg!(target_os = "linux") {
            ui.button(HOST_SETTINGS, "Host settings", true, false)?;
        }
        ui.text(
            &small_font,
            "Saved device credentials. Your password is not saved on this client.",
            304,
            664,
            680,
            MUTED,
        )?;
        ui.panel(Rect::new(280, 716, 736, 60), Color::RGB(17, 37, 44))?;
        let surface = small_font.render(&status).blended_wrapped(INK, 688)?;
        let texture = textures.create_texture_from_surface(&surface)?;
        let height = surface.height().min(44);
        ui.canvas
            .copy(
                &texture,
                Some(Rect::new(0, 0, surface.width(), height)),
                Rect::new(304, 726, surface.width(), height),
            )
            .map_err(anyhow::Error::msg)?;
        ui.text(
            &small_font,
            "Enter to connect / pair · Tab to switch fields · Closing keeps your session open",
            284,
            796,
            724,
            MUTED,
        )?;
        canvas.present();
        std::thread::sleep(Duration::from_millis(20));
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
