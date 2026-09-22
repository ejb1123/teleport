//! Native local host settings. Passwords never enter process arguments or logs.
use crate::host_admin::{self, Request, Response};
use anyhow::{Context, Result};
use sdl2::{event::Event, keyboard::Keycode, pixels::Color, rect::Rect};
use std::{
    sync::mpsc,
    time::{Duration, Instant},
};
use zeroize::Zeroize;

enum Action {
    Admin(Request),
    Service(&'static str),
}
pub fn run() -> Result<()> {
    let directory = host_admin::default_directory()?;
    let sdl = sdl2::init().map_err(anyhow::Error::msg)?;
    let video = sdl.video().map_err(anyhow::Error::msg)?;
    let ttf = sdl2::ttf::init().map_err(anyhow::Error::msg)?;
    let font = ttf
        .load_font(crate::launcher::font_path()?, 16)
        .map_err(anyhow::Error::msg)?;
    let window = video
        .window("Teleport Host Settings", 920, 800)
        .position_centered()
        .allow_highdpi()
        .resizable()
        .build()?;
    let mut canvas = window.into_canvas().software().build()?;
    canvas.set_logical_size(920, 800)?;
    let textures = canvas.texture_creator();
    let mut events = sdl.event_pump().map_err(anyhow::Error::msg)?;
    video.text_input().start();
    let fields = [
        Rect::new(28, 274, 260, 42),
        Rect::new(302, 274, 278, 42),
        Rect::new(594, 274, 298, 42),
    ];
    let buttons = [
        (Rect::new(28, 118, 164, 38), "Start service"),
        (Rect::new(204, 118, 164, 38), "Stop service"),
        (Rect::new(28, 330, 206, 40), "Save password"),
        (Rect::new(246, 330, 206, 40), "Disable login"),
        (Rect::new(28, 422, 206, 40), "Open pairing"),
        (Rect::new(246, 422, 206, 40), "Close pairing"),
        (Rect::new(28, 690, 206, 40), "Revoke selected"),
        (Rect::new(246, 690, 206, 40), "Revoke all managed"),
    ];
    let mut values = [String::new(), String::new(), String::new()];
    let mut focused = 0;
    let mut status: Option<Response> = None;
    let mut message = String::from("Checking the local host service…");
    let mut service_state = String::from("unknown");
    let mut pairing_code: Option<(String, Instant)> = None;
    let mut selected: Option<String> = None;
    let mut confirm_action: Option<(usize, Instant)> = None;
    let mut device_offset = 0usize;
    let mut pending = None;
    let mut last_refresh = Instant::now() - Duration::from_secs(5);
    'running: loop {
        if confirm_action.is_some_and(|(_, when)| when.elapsed() > Duration::from_secs(10)) {
            confirm_action = None;
            message = "Confirmation expired; no change made.".into();
        }
        device_offset = device_offset.min(
            status
                .as_ref()
                .map_or(0, |s| s.devices.len().saturating_sub(6)),
        );
        if let Some(receiver) = &pending {
            let receiver: &mpsc::Receiver<(Result<Response>, String)> = receiver;
            if let Ok((result, state)) = receiver.try_recv() {
                pending = None;
                service_state = state;
                match result {
                    Ok(mut response) => {
                        if let Some(code) = response.code.take() {
                            pairing_code = Some((code, Instant::now()));
                        }
                        if !response.pairing_open {
                            pairing_code = None;
                        }
                        message = "Host settings updated. Existing trusted devices stay connected unless revoked.".into();
                        status = Some(response);
                    }
                    Err(error) => {
                        message = error.to_string();
                        status = None;
                    }
                }
            }
        }
        let mut action = None;
        for event in events.poll_iter() {
            match event {
                Event::Quit { .. }
                | Event::KeyDown {
                    keycode: Some(Keycode::Escape),
                    ..
                } => break 'running,
                Event::TextInput { text, .. }
                    if pending.is_none()
                        && values[focused].len() + text.len() <= 256
                        && !text.chars().any(char::is_control) =>
                {
                    values[focused].push_str(&text);
                    confirm_action = None;
                }
                Event::MouseWheel { y, .. } => {
                    device_offset = device_offset.saturating_add_signed(-(y as isize)).min(
                        status
                            .as_ref()
                            .map_or(0, |s| s.devices.len().saturating_sub(6)),
                    );
                    confirm_action = None;
                }
                Event::KeyDown {
                    keycode: Some(Keycode::PageDown),
                    ..
                } => {
                    device_offset = device_offset.saturating_add(6).min(
                        status
                            .as_ref()
                            .map_or(0, |s| s.devices.len().saturating_sub(6)),
                    );
                }
                Event::KeyDown {
                    keycode: Some(Keycode::PageUp),
                    ..
                } => {
                    device_offset = device_offset.saturating_sub(6);
                }
                Event::KeyDown {
                    keycode: Some(Keycode::Backspace),
                    ..
                } => {
                    values[focused].pop();
                }
                Event::KeyDown {
                    keycode: Some(Keycode::Tab),
                    ..
                } => {
                    focused = (focused + 1) % 3;
                }
                Event::KeyDown {
                    keycode: Some(Keycode::V),
                    keymod,
                    ..
                } if keymod
                    .intersects(sdl2::keyboard::Mod::LCTRLMOD | sdl2::keyboard::Mod::RCTRLMOD) =>
                {
                    if let Ok(text) = video.clipboard().clipboard_text()
                        && text.len() <= 256
                        && !text.chars().any(char::is_control)
                    {
                        values[focused] = text;
                    }
                }
                Event::MouseButtonDown {
                    x,
                    y,
                    mouse_btn: sdl2::mouse::MouseButton::Left,
                    ..
                } if pending.is_none() => {
                    for (index, field) in fields.iter().enumerate() {
                        if field.contains_point((x, y)) {
                            focused = index;
                            confirm_action = None;
                        }
                    }
                    if let Some(status) = &status {
                        for (index, device) in status
                            .devices
                            .iter()
                            .skip(device_offset)
                            .take(6)
                            .enumerate()
                        {
                            if Rect::new(28, 510 + index as i32 * 28, 864, 26)
                                .contains_point((x, y))
                            {
                                selected = Some(device.id.clone());
                                confirm_action = None;
                            }
                        }
                    }
                    for (index, (rect, _)) in buttons.iter().enumerate() {
                        if !rect.contains_point((x, y)) {
                            continue;
                        }
                        let needs_confirmation = [1, 3, 6, 7].contains(&index)
                            || (index == 2
                                && status.as_ref().is_some_and(|s| s.username.is_some()));
                        if needs_confirmation
                            && confirm_action.is_none_or(|(button, _)| button != index)
                        {
                            confirm_action = Some((index, Instant::now()));
                            message = "Click that button again to confirm. Revocation disconnects affected devices.".into();
                            continue;
                        }
                        confirm_action = None;
                        action = match index {
                            0 => Some(Action::Service("start")),
                            1 => Some(Action::Service("stop")),
                            2 => {
                                if values[1] != values[2] {
                                    message = "Passwords do not match.".into();
                                    None
                                } else {
                                    let password = std::mem::take(&mut values[1]);
                                    values[2].zeroize();
                                    Some(Action::Admin(Request::SetPassword {
                                        username: values[0].clone(),
                                        password,
                                    }))
                                }
                            }
                            3 => Some(Action::Admin(Request::DisablePassword)),
                            4 => Some(Action::Admin(Request::OpenPairing)),
                            5 => {
                                pairing_code = None;
                                Some(Action::Admin(Request::ClosePairing))
                            }
                            6 => selected
                                .clone()
                                .map(|id| Action::Admin(Request::RevokeDevice { id })),
                            7 => Some(Action::Admin(Request::RevokeAllDevices)),
                            _ => None,
                        };
                    }
                }
                _ => (),
            }
        }
        if pending.is_none()
            && (action.is_some()
                || (confirm_action.is_none() && last_refresh.elapsed() >= Duration::from_secs(5)))
        {
            let action = action.unwrap_or(Action::Admin(Request::Status));
            let directory = directory.clone();
            let (sender, receiver) = mpsc::channel();
            pending = Some(receiver);
            last_refresh = Instant::now();
            std::thread::spawn(move || {
                let result = (|| {
                    let mut request = match action {
                        Action::Admin(request) => request,
                        Action::Service(verb) => {
                            let output = std::process::Command::new("systemctl")
                                .args(["--user", verb, "teleport-desktop.service"])
                                .output()
                                .context("run user service control")?;
                            anyhow::ensure!(
                                output.status.success(),
                                "service operation failed: {}",
                                String::from_utf8_lossy(&output.stderr)
                            );
                            Request::Status
                        }
                    };
                    let result = host_admin::call(&directory, &request);
                    request.clear_password();
                    result
                })();
                let state = std::process::Command::new("systemctl")
                    .args(["--user", "is-active", "teleport-desktop.service"])
                    .output()
                    .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned())
                    .unwrap_or_else(|_| "unavailable".into());
                let _ = sender.send((result, state));
            });
        }
        canvas.set_draw_color(Color::RGB(12, 18, 27));
        canvas.clear();
        let mut text = |label: &str, x: i32, y: i32, width: u32| -> Result<()> {
            if label.is_empty() {
                return Ok(());
            }
            let surface = font
                .render(label)
                .blended(Color::RGB(220, 230, 238))
                .map_err(anyhow::Error::msg)?;
            let texture = textures.create_texture_from_surface(&surface)?;
            let source = Rect::new(0, 0, surface.width().min(width), surface.height());
            canvas
                .copy(
                    &texture,
                    source,
                    Rect::new(x, y, source.width(), source.height()),
                )
                .map_err(anyhow::Error::msg)?;
            Ok(())
        };
        text("TELEPORT / HOST SETTINGS", 28, 26, 864)?;
        text(
            &format!("User service: {service_state}    •    Local controls only"),
            28,
            65,
            864,
        )?;
        text(
            &status
                .as_ref()
                .map(|s| {
                    format!(
                        "Listen: {}   Login: {}",
                        s.listen,
                        s.username.as_deref().unwrap_or("disabled")
                    )
                })
                .unwrap_or_else(|| {
                    "Host control unavailable. Start the updated persistent host service.".into()
                }),
            28,
            92,
            864,
        )?;
        text(
            "HOST IDENTITY / verify this fingerprint on new clients",
            28,
            180,
            864,
        )?;
        text(
            &status
                .as_ref()
                .map(|s| s.fingerprint.clone())
                .unwrap_or_default(),
            28,
            208,
            864,
        )?;
        text("Username", 28, 248, 260)?;
        text("Password (12+ characters)", 302, 248, 278)?;
        text("Confirm password", 594, 248, 298)?;
        text(
            "OPTIONAL ONE-TIME PAIRING / expires in 5 minutes",
            28,
            392,
            864,
        )?;
        if let Some((code, when)) = &pairing_code
            && when.elapsed() < Duration::from_secs(300)
        {
            text(&format!("Code: {code}"), 482, 434, 360)?;
        }
        text(
            &format!(
                "MANAGED DEVICES ({}) / legacy imported credentials are preserved",
                status.as_ref().map_or(0, |s| s.devices.len())
            ),
            28,
            482,
            864,
        )?;
        if let Some(status) = &status {
            for (index, device) in status
                .devices
                .iter()
                .skip(device_offset)
                .take(6)
                .enumerate()
            {
                text(
                    &format!(
                        "{} {}   {}",
                        if selected.as_ref() == Some(&device.id) {
                            "›"
                        } else {
                            " "
                        },
                        device.name,
                        device.id
                    ),
                    28,
                    512 + index as i32 * 28,
                    864,
                )?;
            }
        }
        text("Scroll / PgUp / PgDn", 684, 696, 208)?;
        text(&message, 28, 752, 864)?;
        for (index, field) in fields.iter().enumerate() {
            canvas.set_draw_color(if index == focused {
                Color::RGB(91, 223, 201)
            } else {
                Color::RGB(58, 77, 91)
            });
            canvas.draw_rect(*field).map_err(anyhow::Error::msg)?;
            let label = if index == 0 {
                values[index].clone()
            } else {
                "•".repeat(values[index].chars().count())
            };
            if !label.is_empty() {
                let surface = font
                    .render(&label)
                    .blended(Color::RGB(220, 230, 238))
                    .map_err(anyhow::Error::msg)?;
                let texture = textures.create_texture_from_surface(&surface)?;
                let width = surface.width().min(field.width() - 16);
                canvas
                    .copy(
                        &texture,
                        Rect::new(0, 0, width, surface.height()),
                        Rect::new(field.x() + 8, field.y() + 10, width, surface.height()),
                    )
                    .map_err(anyhow::Error::msg)?;
            }
        }
        for (rect, label) in buttons {
            canvas.set_draw_color(if pending.is_some() {
                Color::RGB(32, 43, 53)
            } else {
                Color::RGB(36, 67, 75)
            });
            canvas.fill_rect(rect).map_err(anyhow::Error::msg)?;
            let surface = font
                .render(label)
                .blended(Color::RGB(220, 230, 238))
                .map_err(anyhow::Error::msg)?;
            let texture = textures.create_texture_from_surface(&surface)?;
            canvas
                .copy(
                    &texture,
                    None,
                    Rect::new(
                        rect.x() + 12,
                        rect.y() + 10,
                        surface.width(),
                        surface.height(),
                    ),
                )
                .map_err(anyhow::Error::msg)?;
        }
        canvas.present();
        std::thread::sleep(Duration::from_millis(16));
    }
    values[1].zeroize();
    values[2].zeroize();
    Ok(())
}
