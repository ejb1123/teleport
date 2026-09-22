//! Small native SDL connection manager; streaming runs in a separate native process.
use anyhow::{Context, Result};
use sdl2::{event::Event, keyboard::Keycode, pixels::Color, rect::Rect};
use std::{
    io::{BufRead, BufReader},
    path::PathBuf,
    process::{Child, Command, Stdio},
    time::Duration,
};

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

pub fn run() -> Result<()> {
    let sdl = sdl2::init().map_err(anyhow::Error::msg)?;
    let video = sdl.video().map_err(anyhow::Error::msg)?;
    let ttf = sdl2::ttf::init().map_err(anyhow::Error::msg)?;
    let font = ttf
        .load_font(font_path()?, 18)
        .map_err(anyhow::Error::msg)?;
    let window = video
        .window("Teleport — Connect", 800, 550)
        .position_centered()
        .allow_highdpi()
        .build()?;
    let mut canvas = window.into_canvas().software().build()?;
    canvas.set_logical_size(800, 550)?;
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
    let mut focused = 0;
    let mut status = "Import a pairing file received securely from your Linux host.".to_owned();
    let mut child: Option<Child> = None;
    let mut progress = None;
    let mut clipboard = false;
    let mut mute = false;
    let mut profile_index = profiles.len().saturating_sub(1);
    'ui: loop {
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
        }
        for event in events.poll_iter() {
            let mut action = None;
            match event {
                Event::Quit { .. } => break 'ui,
                Event::TextInput { text, .. } => {
                    let field = if focused == 0 {
                        &mut address
                    } else {
                        &mut pairing_path
                    };
                    if field.len() + text.len() <= 2048 {
                        field.push_str(&text);
                    }
                }
                Event::DropFile { filename, .. } => {
                    pairing_path = filename;
                    focused = 1;
                }
                Event::KeyDown {
                    keycode: Some(Keycode::Tab),
                    ..
                } => focused = 1 - focused,
                Event::KeyDown {
                    keycode: Some(Keycode::Backspace),
                    ..
                } => {
                    if focused == 0 {
                        address.pop();
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
                        let field = if focused == 0 {
                            &mut address
                        } else {
                            &mut pairing_path
                        };
                        if text.len() <= 2048 {
                            *field = text.trim().to_owned();
                        }
                    }
                }
                Event::MouseButtonDown { x, y, .. } => {
                    if (90..132).contains(&y) {
                        focused = 0;
                    }
                    if (170..212).contains(&y) {
                        focused = 1;
                    }
                    if (230..275).contains(&y) {
                        action = Some(if x < 400 { 0 } else { 1 });
                    }
                    if (300..340).contains(&y) {
                        if x < 400 {
                            clipboard = !clipboard;
                        } else {
                            mute = !mute;
                        }
                    }
                    if (365..412).contains(&y) {
                        action = Some(if x < 400 { 2 } else { 3 });
                    }
                }
                _ => {}
            }
            match action {
                Some(0) => match crate::profiles::import(
                    address.trim(),
                    &PathBuf::from(pairing_path.trim()),
                    &mut profiles,
                ) {
                    Ok(profile) => {
                        selected = Some(profile);
                        profile_index = profiles.len() - 1;
                        status = "Pairing imported and trusted. Ready to connect.".into();
                    }
                    Err(error) => status = format!("Import failed: {error}"),
                },
                Some(1) if !profiles.is_empty() => {
                    profile_index = (profile_index + 1) % profiles.len();
                    selected = Some(profiles[profile_index].clone());
                    address = profiles[profile_index].address.clone();
                    status = "Saved host selected. Connect uses its trusted pairing.".into();
                }
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
                            .arg("--reconnect");
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
                        status = "Import and trust a pairing file for this address first.".into();
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
        canvas.set_draw_color(Color::RGB(22, 27, 36));
        canvas.clear();
        for (index, y) in [90, 170].into_iter().enumerate() {
            canvas.set_draw_color(if focused == index {
                Color::RGB(50, 75, 104)
            } else {
                Color::RGB(39, 46, 58)
            });
            canvas
                .fill_rect(Rect::new(20, y, 760, 42))
                .map_err(anyhow::Error::msg)?;
        }
        for y in [230, 365] {
            for x in [20, 410] {
                canvas.set_draw_color(Color::RGB(44, 82, 110));
                canvas
                    .fill_rect(Rect::new(x, y, 370, 45))
                    .map_err(anyhow::Error::msg)?;
            }
        }
        let labels = [
            (20, 20, "Teleport • Native remote desktop".into()),
            (20, 65, "Host address (hostname:port)".into()), (30, 100, address.clone()),
            (20, 145, "Pairing file path — paste a path or drop the file here".into()), (30, 180, pairing_path.clone()),
            (35, 242, "Import & trust pairing".into()), (425, 242, "Next saved host".into()),
            (20, 308, format!("[{}] Share text clipboard", if clipboard { "x" } else { " " })),
            (410, 308, format!("[{}] Mute audio", if mute { "x" } else { " " })),
            (35, 378, if child.is_some() { "Session running" } else { "Connect / reconnect" }.into()),
            (425, 378, "Disconnect".into()),
            (20, 480, "Only trust pairing files obtained from your host securely.".into()),
            (20, 510, "Tab switches fields • Ctrl/Cmd+V pastes • Closing this window keeps the session open".into()),
        ];
        for (x, y, text) in labels
            .into_iter()
            .chain(std::iter::once((20, 435, status.clone())))
        {
            if text.is_empty() {
                continue;
            }
            let surface = font.render(&text).blended(Color::RGB(231, 237, 245))?;
            let texture = textures.create_texture_from_surface(&surface)?;
            let width = surface.width().min((780 - x) as u32);
            canvas
                .copy(
                    &texture,
                    Some(Rect::new(0, 0, width, surface.height())),
                    Rect::new(x, y, width, surface.height()),
                )
                .map_err(anyhow::Error::msg)?;
        }
        canvas.present();
        std::thread::sleep(Duration::from_millis(20));
    }
    Ok(())
}
