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

/// What the sessions column has selected.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Chosen {
    Approval(u64),
    Session(u64),
}

/// Rows of the sessions column: guests waiting for approval first.
fn session_rows(status: &Response) -> Vec<(Chosen, String)> {
    let mut rows: Vec<(Chosen, String)> = status
        .approvals
        .iter()
        .map(|request| {
            (
                Chosen::Approval(request.id),
                format!("WAITING {} s  {}", request.remaining_s, request.name),
            )
        })
        .collect();
    rows.extend(status.sessions.iter().map(|session| {
        (
            Chosen::Session(session.id),
            format!(
                "{}{} {}  {}",
                if session.control { "⌨ " } else { "" },
                session.role.label(),
                session.name,
                session.transport
            ),
        )
    }));
    rows
}

/// A guest's permissions toggled in the sessions column.
fn toggled(
    status: &Response,
    chosen: Option<Chosen>,
    change: impl FnOnce(&mut crate::protocol::Permissions, &mut bool),
) -> Option<Request> {
    let Some(Chosen::Session(id)) = chosen else {
        return None;
    };
    let session = status.sessions.iter().find(|s| s.id == id)?;
    let device = session.device.clone()?;
    let grant = status
        .devices
        .iter()
        .find(|d| d.id == device)
        .and_then(|d| d.guest)?;
    let (mut permissions, mut trusted) = (grant.permissions, grant.trusted);
    change(&mut permissions, &mut trusted);
    Some(Request::SetGuest {
        id: device,
        permissions: Some(permissions),
        trusted: Some(trusted),
    })
}
pub fn run() -> Result<()> {
    let directory = host_admin::default_directory()?;
    let (sdl, video) = crate::windowing::init()?;
    let ttf = sdl2::ttf::init().map_err(anyhow::Error::msg)?;
    let font = ttf
        .load_font(crate::launcher::font_path()?, 16)
        .map_err(anyhow::Error::msg)?;
    let window = video
        .window("Teleport Host Settings", 1260, 800)
        .position_centered()
        .allow_highdpi()
        .resizable()
        .build()?;
    let mut canvas = crate::windowing::software(window)?;
    canvas.set_logical_size(1260, 800)?;
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
        // Sessions and guests (docs/guests.md), right column.
        (Rect::new(940, 392, 146, 36), "Allow"),
        (Rect::new(1098, 392, 146, 36), "Deny"),
        (Rect::new(940, 436, 146, 36), "Kick"),
        (Rect::new(1098, 436, 146, 36), "Take control"),
        (Rect::new(940, 480, 146, 36), "Release control"),
        (Rect::new(1098, 480, 146, 36), "Invite guest"),
        (Rect::new(940, 524, 146, 36), "Gamepad on/off"),
        (Rect::new(1098, 524, 146, 36), "Keys+mouse"),
        (Rect::new(940, 568, 146, 36), "Clipboard"),
        (Rect::new(1098, 568, 146, 36), "Trusted"),
        (Rect::new(940, 612, 146, 34), "Microphone"),
    ];
    let mut values = [String::new(), String::new(), String::new()];
    let mut focused = 0;
    let mut status: Option<Response> = None;
    let mut message = String::from("Checking the local host service…");
    let mut service_state = String::from("unknown");
    let mut pairing_code: Option<(String, Instant)> = None;
    let mut relay_pairing: Option<String> = None;
    let mut selected: Option<String> = None;
    let mut confirm_action: Option<(usize, Instant)> = None;
    let mut device_offset = 0usize;
    // A guest waiting for approval or a connected session (right column).
    let mut chosen: Option<Chosen> = None;
    let mut invite_link: Option<(String, Instant)> = None;
    let mut last_service_check = Instant::now() - Duration::from_secs(10);
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
            let receiver: &mpsc::Receiver<(Result<Response>, Option<String>, bool)> = receiver;
            if let Ok((result, state, background)) = receiver.try_recv() {
                pending = None;
                if let Some(state) = state {
                    service_state = state;
                }
                match result {
                    Ok(mut response) => {
                        if let Some(code) = response.code.take() {
                            pairing_code = Some((code, Instant::now()));
                            relay_pairing = response.relay_pairing.take();
                        }
                        if !response.pairing_open {
                            pairing_code = None;
                            relay_pairing = None;
                        }
                        if let Some(link) = response.invite.take() {
                            invite_link = Some((link, Instant::now()));
                        }
                        if !background {
                            message = "Host settings updated. Existing trusted devices stay connected unless revoked.".into();
                            if invite_link
                                .as_ref()
                                .is_some_and(|(_, when)| when.elapsed() < Duration::from_secs(1))
                                && response
                                    .sessions
                                    .iter()
                                    .any(|s| s.role == crate::protocol::Role::Guest)
                            {
                                message = "Invite created; its link stays hidden while a guest is connected (or run host-admin create-invite privately).".into();
                            }
                        }
                        // A guest now waiting: select it so Allow/Deny act on it.
                        if chosen.is_none()
                            && let Some(request) = response.approvals.first()
                        {
                            chosen = Some(Chosen::Approval(request.id));
                        }
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
                        for (index, (row, _)) in
                            session_rows(status).into_iter().take(11).enumerate()
                        {
                            if Rect::new(940, 64 + index as i32 * 28, 304, 26)
                                .contains_point((x, y))
                            {
                                chosen = Some(row);
                                confirm_action = None;
                            }
                        }
                    }
                    for (index, (rect, _)) in buttons.iter().enumerate() {
                        if !rect.contains_point((x, y)) {
                            continue;
                        }
                        let needs_confirmation = [1, 3, 6, 7, 15, 18].contains(&index)
                            || (index == 2
                                && status.as_ref().is_some_and(|s| s.username.is_some()));
                        if needs_confirmation
                            && confirm_action.is_none_or(|(button, _)| button != index)
                        {
                            confirm_action = Some((index, Instant::now()));
                            message = if index == 15 {
                                "Keyboard and mouse let a guest act as you on this computer (run programs, open these settings). Click again to confirm."
                            } else if index == 18 {
                                "A guest's microphone plays as this computer's \"Teleport Microphone\": voice apps here hear the guest as you. Click again to confirm."
                            } else {
                                "Click that button again to confirm. Revocation disconnects affected devices."
                            }
                            .into();
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
                                relay_pairing = None;
                                Some(Action::Admin(Request::ClosePairing))
                            }
                            6 => selected
                                .clone()
                                .map(|id| Action::Admin(Request::RevokeDevice { id })),
                            7 => Some(Action::Admin(Request::RevokeAllDevices)),
                            8 | 9 => match chosen {
                                Some(Chosen::Approval(id)) => {
                                    Some(Action::Admin(Request::AnswerGuest {
                                        id,
                                        allow: index == 8,
                                    }))
                                }
                                _ => {
                                    message = "Select a waiting guest first.".into();
                                    None
                                }
                            },
                            10 => match chosen {
                                Some(Chosen::Session(id)) => {
                                    Some(Action::Admin(Request::Kick { id }))
                                }
                                _ => {
                                    message = "Select a connected session first.".into();
                                    None
                                }
                            },
                            11 => Some(Action::Admin(Request::TakeControl)),
                            12 => Some(Action::Admin(Request::ReleaseControl)),
                            13 => Some(Action::Admin(Request::CreateInvite {
                                name: "Guest".into(),
                                uses: 1,
                                seconds: 3600,
                                permissions: crate::protocol::Permissions::GUEST_DEFAULT,
                            })),
                            14..=18 => {
                                let request = status.as_ref().and_then(|status| {
                                    toggled(status, chosen, |permissions, trusted| match index {
                                        14 => permissions.gamepad = !permissions.gamepad,
                                        15 => {
                                            permissions.keyboard_mouse = !permissions.keyboard_mouse
                                        }
                                        16 => permissions.clipboard = !permissions.clipboard,
                                        17 => *trusted = !*trusted,
                                        _ => permissions.microphone = !permissions.microphone,
                                    })
                                });
                                if request.is_none() {
                                    message = "Select a connected guest first (owners always have full control).".into();
                                }
                                request.map(Action::Admin)
                            }
                            _ => None,
                        };
                    }
                }
                _ => (),
            }
        }
        if pending.is_none()
            && (action.is_some()
                || (confirm_action.is_none() && last_refresh.elapsed() >= Duration::from_secs(1)))
        {
            // Background refreshes every second (guests waiting for
            // approval); the service state only every five.
            let background = action.is_none();
            let check_service =
                !background || last_service_check.elapsed() >= Duration::from_secs(5);
            if check_service {
                last_service_check = Instant::now();
            }
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
                let state = check_service.then(|| {
                    std::process::Command::new("systemctl")
                        .args(["--user", "is-active", "teleport-desktop.service"])
                        .output()
                        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned())
                        .unwrap_or_else(|_| "unavailable".into())
                });
                let _ = sender.send((result, state, background));
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
                        "Listen: {}   Teleport login: {}{}",
                        s.listen,
                        s.username.as_deref().unwrap_or("disabled"),
                        if s.u2f_required {
                            " + U2F touch (new devices)"
                        } else {
                            ""
                        }
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
            if status.as_ref().is_some_and(|s| s.system_login) {
                "Linux account login: enabled (desktop owner, one session)"
            } else {
                "Linux login: disabled; install PAM helper and enable it in service configuration"
            },
            28,
            158,
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
            "Key setup: teleport host-admin require-u2f FILE (password enrollment only)",
            28,
            372,
            864,
        )?;
        text(
            "OPTIONAL ONE-TIME PAIRING / expires in 5 minutes",
            28,
            392,
            864,
        )?;
        // Guests watch this screen: secrets stay hidden while one is connected.
        let guest_watching = status.as_ref().is_some_and(|s| {
            s.sessions
                .iter()
                .any(|session| session.role == crate::protocol::Role::Guest)
        });
        if let Some((code, when)) = &pairing_code
            && when.elapsed() < Duration::from_secs(300)
        {
            if guest_watching {
                text("Code: hidden while a guest is connected", 482, 434, 400)?;
            } else {
                text(&format!("Code: {code}"), 482, 434, 360)?;
                if let Some(link) = &relay_pairing {
                    text(&format!("Relay: {link}"), 28, 458, 864)?;
                }
            }
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
        // Sessions and guests (docs/guests.md).
        text("SESSIONS / GUESTS", 940, 26, 304)?;
        match &status {
            Some(status) => {
                let rows = session_rows(status);
                if rows.is_empty() {
                    text("No session connected", 940, 64, 304)?;
                }
                for (index, (row, label)) in rows.iter().take(11).enumerate() {
                    text(
                        &format!("{} {label}", if chosen == Some(*row) { "›" } else { " " }),
                        940,
                        66 + index as i32 * 28,
                        304,
                    )?;
                }
                // What the selected guest may do.
                let detail = match chosen {
                    Some(Chosen::Session(id)) => {
                        status.sessions.iter().find(|s| s.id == id).map(|s| {
                            let trusted = s
                                .device
                                .as_ref()
                                .and_then(|device| status.devices.iter().find(|d| &d.id == device))
                                .and_then(|d| d.guest)
                                .is_some_and(|grant| grant.trusted);
                            format!(
                                "{}{}",
                                s.permissions.label(),
                                if trusted { ", trusted" } else { "" }
                            )
                        })
                    }
                    Some(Chosen::Approval(_)) => Some("Allow or deny this guest".into()),
                    None => None,
                };
                text(detail.as_deref().unwrap_or(""), 940, 372, 304)?;
                text(
                    &format!("Open invites: {}", status.invites.len()),
                    940,
                    616,
                    304,
                )?;
            }
            None => text("Host control unavailable", 940, 64, 304)?,
        }
        if let Some((link, _)) = invite_link
            .as_ref()
            .filter(|(_, when)| when.elapsed() < Duration::from_secs(120) && !guest_watching)
        {
            // As secret as a pairing code: shown here only, never logged.
            text(
                "Invite link (one use, 1 hour; give it to the guest):",
                940,
                650,
                304,
            )?;
            for (index, chunk) in link.as_bytes().chunks(34).enumerate().take(4) {
                text(
                    &String::from_utf8_lossy(chunk),
                    940,
                    676 + index as i32 * 22,
                    304,
                )?;
            }
        }
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
