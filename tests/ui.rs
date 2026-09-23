#![cfg(target_os = "linux")]

use std::{
    io::BufRead,
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};
use x11rb::{
    connection::Connection,
    protocol::{
        xproto::{self, ConnectionExt},
        xtest::ConnectionExt as _,
    },
    rust_connection::RustConnection,
};

struct Process(Child);
impl Drop for Process {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn teleport(display: &str) -> Command {
    let binary = std::env::var_os("TELEPORT_TEST_BINARY")
        .unwrap_or_else(|| env!("CARGO_BIN_EXE_teleport").into());
    let mut command = Command::new(binary);
    if std::env::var_os("TELEPORT_TEST_BINARY").is_some() {
        for key in [
            "GST_PLUGIN_PATH",
            "GST_PLUGIN_PATH_1_0",
            "GST_PLUGIN_SYSTEM_PATH",
            "GST_PLUGIN_SYSTEM_PATH_1_0",
            "GST_PLUGIN_SCANNER",
            "GST_PLUGIN_SCANNER_1_0",
        ] {
            command.env_remove(key);
        }
    }
    command
        .env_remove("WAYLAND_DISPLAY")
        .env("SDL_VIDEODRIVER", "x11")
        .env("DISPLAY", display)
        .stdout(Stdio::null());
    command
}

fn desktop_window(
    connection: &RustConnection,
    root: u32,
    process: &mut Process,
    title_fragment: &str,
    old: Option<u32>,
) -> u32 {
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut observed = std::collections::BTreeSet::new();
    let name = connection
        .intern_atom(false, b"_NET_WM_NAME")
        .unwrap()
        .reply()
        .unwrap()
        .atom;
    loop {
        assert!(
            process.0.try_wait().unwrap().is_none(),
            "native process exited before expected window: {title_fragment}"
        );
        for window in connection
            .query_tree(root)
            .unwrap()
            .reply()
            .unwrap()
            .children
        {
            if old == Some(window) {
                continue;
            }
            for atom in [name, xproto::AtomEnum::WM_NAME.into()] {
                if let Ok(property) = connection
                    .get_property(false, window, atom, xproto::AtomEnum::ANY, 0, 1024)
                    .unwrap()
                    .reply()
                {
                    let title = String::from_utf8_lossy(&property.value);
                    if title.contains(title_fragment) {
                        // Stable titles appear before SDL's first paint (and
                        // before a failed accelerated window is recreated).
                        if title_fragment == "Teleport - Remote Desktop"
                            && !rendered_pixel(connection, window, 5, 5, 0x151b26)
                        {
                            continue;
                        }
                        return window;
                    }
                    if !title.is_empty() {
                        observed.insert(title.into_owned());
                    }
                }
            }
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for native window: {title_fragment}; observed titles: {observed:?}"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn click(connection: &RustConnection, root: u32, window: u32, x: i16, y: i16) {
    let position = connection
        .translate_coordinates(window, root, x, y)
        .unwrap()
        .reply()
        .unwrap();
    connection
        .xtest_fake_input(
            xproto::MOTION_NOTIFY_EVENT,
            0,
            0,
            root,
            position.dst_x,
            position.dst_y,
            0,
        )
        .unwrap()
        .check()
        .unwrap();
    connection
        .xtest_fake_input(xproto::BUTTON_PRESS_EVENT, 1, 0, root, 0, 0, 0)
        .unwrap()
        .check()
        .unwrap();
    connection
        .xtest_fake_input(xproto::BUTTON_RELEASE_EVENT, 1, 0, root, 0, 0, 0)
        .unwrap()
        .check()
        .unwrap();
    connection.flush().unwrap();
}

fn wait_exit(process: &mut Process) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(status) = process.0.try_wait().unwrap() {
            assert!(status.success(), "native UI failed: {status}");
            break;
        }
        assert!(Instant::now() < deadline, "native UI did not close");
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn type_ascii(connection: &RustConnection, root: u32, window: u32, text: &str) {
    connection
        .set_input_focus(xproto::InputFocus::PARENT, window, x11rb::CURRENT_TIME)
        .unwrap()
        .check()
        .unwrap();
    let setup = connection.setup();
    let mapping = connection
        .get_keyboard_mapping(setup.min_keycode, setup.max_keycode - setup.min_keycode + 1)
        .unwrap()
        .reply()
        .unwrap();
    let keys: Vec<&[u32]> = mapping
        .keysyms
        .chunks(usize::from(mapping.keysyms_per_keycode))
        .collect();
    let shift = setup.min_keycode
        + keys
            .iter()
            .position(|symbols| symbols[0] == 0xffe1)
            .unwrap() as u8;
    let key_event = |kind, code| {
        connection
            .xtest_fake_input(kind, code, 0, root, 0, 0, 0)
            .unwrap()
            .check()
            .unwrap();
    };
    for byte in text.bytes() {
        assert!(byte.is_ascii());
        let (index, shifted) = keys
            .iter()
            .enumerate()
            .find_map(|(index, symbols)| {
                symbols
                    .iter()
                    .take(2)
                    .position(|symbol| *symbol == u32::from(byte))
                    .map(|column| (index, column == 1))
            })
            .expect("test character is absent from Xvfb keymap");
        let code = setup.min_keycode + index as u8;
        if shifted {
            key_event(xproto::KEY_PRESS_EVENT, shift);
        }
        key_event(xproto::KEY_PRESS_EVENT, code);
        key_event(xproto::KEY_RELEASE_EVENT, code);
        if shifted {
            key_event(xproto::KEY_RELEASE_EVENT, shift);
        }
        // SDL's text-input events are generated while pumping X events. Pace
        // input like a user, rather than flooding multiple fields in one frame.
        std::thread::sleep(Duration::from_millis(30));
    }
    connection.flush().unwrap();
}

fn rendered_rgb(connection: &RustConnection, window: u32, x: i16, y: i16) -> Option<u32> {
    let Ok(cookie) =
        connection.get_image(xproto::ImageFormat::Z_PIXMAP, window, x, y, 1, 1, u32::MAX)
    else {
        return None;
    };
    let Ok(image) = cookie.reply() else {
        return None;
    };
    let Ok(bytes) = <[u8; 4]>::try_from(image.data.as_slice()) else {
        return None;
    };
    let pixel = if connection.setup().image_byte_order == xproto::ImageOrder::LSB_FIRST {
        u32::from_le_bytes(bytes)
    } else {
        u32::from_be_bytes(bytes)
    };
    Some(pixel & 0xffffff)
}

fn rendered_pixel(connection: &RustConnection, window: u32, x: i16, y: i16, rgb: u32) -> bool {
    rendered_rgb(connection, window, x, y) == Some(rgb)
}

fn wait_field_focus(connection: &RustConnection, window: u32, y: i16) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !rendered_pixel(connection, window, 304, y, 0x5bdfc9) {
        if Instant::now() >= deadline {
            dump_window(connection, window);
        }
        assert!(
            Instant::now() < deadline,
            "launcher never rendered focused field at y={y}"
        );
        std::thread::sleep(Duration::from_millis(30));
    }
}

fn dump_window(connection: &RustConnection, window: u32) {
    use std::io::Write;
    let geometry = connection.get_geometry(window).unwrap().reply().unwrap();
    let image = connection
        .get_image(
            xproto::ImageFormat::Z_PIXMAP,
            window,
            0,
            0,
            geometry.width,
            geometry.height,
            u32::MAX,
        )
        .unwrap()
        .reply()
        .unwrap();
    let mut file = tempfile::Builder::new()
        .prefix("teleport-ui-failure-")
        .suffix(".ppm")
        .tempfile_in("/tmp")
        .unwrap();
    writeln!(file, "P6\n{} {}\n255", geometry.width, geometry.height).unwrap();
    for bytes in image.data.chunks_exact(4) {
        let pixel = if connection.setup().image_byte_order == xproto::ImageOrder::LSB_FIRST {
            u32::from_le_bytes(bytes.try_into().unwrap())
        } else {
            u32::from_be_bytes(bytes.try_into().unwrap())
        };
        file.write_all(&[(pixel >> 16) as u8, (pixel >> 8) as u8, pixel as u8])
            .unwrap();
    }
    let (_, path) = file.keep().unwrap();
    eprintln!("UI failure screenshot: {}", path.display());
}

fn add_desktop(connection: &RustConnection, root: u32, window: u32, name: &str, address: &str) {
    click(connection, root, window, 100, 615);
    wait_field_focus(connection, window, 220);
    type_ascii(connection, root, window, name);
    click(connection, root, window, 400, 320);
    wait_field_focus(connection, window, 300);
    type_ascii(connection, root, window, address);
    click(connection, root, window, 500, 410);
}

fn wait_host_approval(connection: &RustConnection, window: u32) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while !(rendered_pixel(connection, window, 304, 460, 0x141e2b)
        && (rendered_pixel(connection, window, 304, 560, 0x5bdfc9)
            || rendered_pixel(connection, window, 304, 560, 0x6defd9)))
    {
        if Instant::now() >= deadline {
            dump_window(connection, window);
        }
        assert!(
            Instant::now() < deadline,
            "first-contact fingerprint approval did not appear"
        );
        std::thread::sleep(Duration::from_millis(30));
    }
}

fn wait_authentication_form(connection: &RustConnection, window: u32) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !(rendered_pixel(connection, window, 304, 340, 0x5bdfc9)
        && rendered_pixel(connection, window, 304, 460, 0x5bdfc9))
    {
        if Instant::now() >= deadline {
            dump_window(connection, window);
        }
        assert!(
            Instant::now() < deadline,
            "authentication form did not appear"
        );
        std::thread::sleep(Duration::from_millis(30));
    }
}

/// Exercise the actual launcher fields and pairing button without a copied file.
#[test]
#[ignore = "requires Xvfb, TCP/UDP and GStreamer runtime plugins"]
fn native_launcher_pairs_with_code_and_connects_saved_host() {
    launcher_authentication(false);
}

#[test]
#[ignore = "requires Xvfb, TCP/UDP and GStreamer runtime plugins"]
fn native_launcher_password_login_and_saved_identity_alias() {
    launcher_authentication(true);
}

/// No user SDL flags: a Wayland session with XWayland must have a CPU-only
/// local UI path even when the Nix application cannot load the host GPU stack.
#[test]
#[ignore = "requires Xvfb and GStreamer runtime plugins"]
fn local_windows_start_without_gpu_or_sdl_workarounds() {
    let temp = tempfile::tempdir_in("/tmp").unwrap();
    let mut display = Process(
        Command::new("Xvfb")
            .args([
                "-displayfd",
                "1",
                "-screen",
                "0",
                "1600x1000x24",
                "-nolisten",
                "tcp",
            ])
            .stdout(Stdio::piped())
            .spawn()
            .unwrap(),
    );
    let mut number = String::new();
    std::io::BufReader::new(display.0.stdout.take().unwrap())
        .read_line(&mut number)
        .unwrap();
    let display_name = format!(":{}", number.trim());
    let (connection, screen) = x11rb::connect(Some(&display_name)).unwrap();
    let root = connection.setup().roots[screen].root;
    for (command, title) in [
        ("launcher", "Teleport"),
        ("host-manager", "Teleport Host Settings"),
    ] {
        let mut process = Process(
            teleport(&display_name)
                .arg(command)
                .env("XDG_CONFIG_HOME", temp.path().join("config"))
                .env("XDG_STATE_HOME", temp.path().join("state"))
                .env("WAYLAND_DISPLAY", "wayland-unavailable-test")
                .env_remove("SDL_VIDEODRIVER")
                .env_remove("SDL_VIDEO_DRIVER")
                .env_remove("SDL_RENDER_DRIVER")
                .env_remove("SDL_FRAMEBUFFER_ACCELERATION")
                .env("SDL_OPENGL_LIBRARY", "/nonexistent/libGL.so")
                .env("SDL_VIDEO_EGL_DRIVER", "/nonexistent/libEGL.so")
                .env("SDL_VULKAN_LIBRARY", "/nonexistent/libvulkan.so")
                .spawn()
                .unwrap(),
        );
        let window = desktop_window(&connection, root, &mut process, title, None);
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            assert!(
                process.0.try_wait().unwrap().is_none(),
                "UI exited after window creation"
            );
            // Both windows paint a navy background away from controls.
            if rendered_pixel(&connection, window, 900, 5, 0x0c121b) {
                break;
            }
            assert!(Instant::now() < deadline, "UI never painted its background");
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

fn launcher_authentication(password_mode: bool) {
    use std::os::unix::fs::PermissionsExt;
    // A pathname Unix socket must fit sockaddr_un even inside a deeply nested
    // Nix development shell's TMPDIR.
    let temp = tempfile::tempdir_in("/tmp").unwrap();
    let config = temp.path().join("config");
    let identity = temp.path().join("identity");
    let mut display = Process(
        Command::new("Xvfb")
            .args([
                "-displayfd",
                "1",
                "-screen",
                "0",
                "1600x1000x24",
                "-nolisten",
                "tcp",
            ])
            .stdout(Stdio::piped())
            .spawn()
            .unwrap(),
    );
    let mut number = String::new();
    std::io::BufReader::new(display.0.stdout.take().unwrap())
        .read_line(&mut number)
        .unwrap();
    let display_name = format!(":{}", number.trim());
    let (connection, screen) = x11rb::connect(Some(&display_name)).unwrap();
    let root = connection.setup().roots[screen].root;
    let socket = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    let address = socket.local_addr().unwrap().to_string();
    drop(socket);
    let mut host = Process(
        teleport(&display_name)
            .args([
                "host",
                "--source",
                "test",
                "--encoder",
                "software",
                "--width",
                "640",
                "--fps",
                "30",
                "--pair",
                "--listen",
                &address,
                "--identity-dir",
            ])
            .arg(&identity)
            .stdout(Stdio::piped())
            .spawn()
            .unwrap(),
    );
    let stdout = host.0.stdout.take().unwrap();
    let (sender, receiver) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        for line in std::io::BufReader::new(stdout)
            .lines()
            .map_while(Result::ok)
        {
            if let Some(rest) = line.strip_prefix("Pairing code: ") {
                let _ = sender.send(rest.split_whitespace().next().unwrap().to_owned());
            }
        }
    });
    let code = receiver
        .recv_timeout(Duration::from_secs(15))
        .expect("host did not display a code");
    const PASSWORD: &str = "Work test passphrase 2468";
    if password_mode {
        use std::io::{Read, Write};
        let mut admin =
            std::os::unix::net::UnixStream::connect(identity.join("admin.sock")).unwrap();
        admin
            .set_read_timeout(Some(Duration::from_secs(15)))
            .unwrap();
        let request = serde_json::to_vec(
            &serde_json::json!({"SetPassword": {"username": "work", "password": PASSWORD}}),
        )
        .unwrap();
        admin
            .write_all(&(request.len() as u32).to_be_bytes())
            .unwrap();
        admin.write_all(&request).unwrap();
        let mut length = [0; 4];
        admin.read_exact(&mut length).unwrap();
        let length = u32::from_be_bytes(length) as usize;
        assert!(length <= 65536);
        let mut response = vec![0; length];
        admin.read_exact(&mut response).unwrap();
        let response: serde_json::Value = serde_json::from_slice(&response).unwrap();
        assert!(response["error"].is_null(), "host password setup failed");
    }
    let mut launcher = Process(
        teleport(&display_name)
            .arg("launcher")
            .env("XDG_CONFIG_HOME", &config)
            .env("SDL_RENDER_DRIVER", "software")
            .env("SDL_IM_MODULE", "none")
            .env("XMODIFIERS", "@im=none")
            .spawn()
            .unwrap(),
    );
    // WM_NAME appears before SDL has initialized its renderer/text input.
    // Wait for a real UI frame; SDL may replace the early X window.
    let deadline = Instant::now() + Duration::from_secs(15);
    let window = loop {
        let window = desktop_window(&connection, root, &mut launcher, "Teleport", None);
        if rendered_pixel(&connection, window, 1030, 820, 0x0c121b) {
            break window;
        }
        assert!(
            Instant::now() < deadline,
            "launcher did not draw its first frame"
        );
        std::thread::sleep(Duration::from_millis(30));
    };
    add_desktop(&connection, root, window, "Work desktop", &address);
    let index = config.join("teleport/profiles.json");
    let deadline = Instant::now() + Duration::from_secs(10);
    while !index.exists() {
        assert!(Instant::now() < deadline, "desktop draft was not saved");
        std::thread::sleep(Duration::from_millis(30));
    }
    let draft: serde_json::Value = serde_json::from_slice(&std::fs::read(&index).unwrap()).unwrap();
    assert_eq!(draft[0]["name"], "Work desktop");
    assert!(draft[0]["pairing_file"].is_null());
    assert!(draft[0]["fingerprint"].is_null());
    // Drafts survive an actual application restart without requiring auth.
    launcher.0.kill().unwrap();
    launcher.0.wait().unwrap();
    launcher = Process(
        teleport(&display_name)
            .arg("launcher")
            .env("XDG_CONFIG_HOME", &config)
            .env("SDL_RENDER_DRIVER", "software")
            .env("SDL_IM_MODULE", "none")
            .env("XMODIFIERS", "@im=none")
            .spawn()
            .unwrap(),
    );
    let window = desktop_window(&connection, root, &mut launcher, "Teleport", None);
    let deadline = Instant::now() + Duration::from_secs(10);
    while !rendered_pixel(&connection, window, 304, 272, 0x5bdfc9) {
        assert!(
            Instant::now() < deadline,
            "saved desktop was not selectable after restart"
        );
        std::thread::sleep(Duration::from_millis(30));
    }
    click(&connection, root, window, 500, 294);
    wait_authentication_form(&connection, window);
    let sign_in = || {
        click(
            &connection,
            root,
            window,
            if password_mode { 560 } else { 740 },
            238,
        );
        if password_mode {
            click(&connection, root, window, 400, 320);
            wait_field_focus(&connection, window, 300);
            type_ascii(&connection, root, window, "work");
        }
        click(&connection, root, window, 400, 400);
        wait_field_focus(&connection, window, 380);
        type_ascii(
            &connection,
            root,
            window,
            if password_mode { PASSWORD } else { &code },
        );
        click(&connection, root, window, 500, 480);
    };
    sign_in();
    wait_host_approval(&connection, window);
    // A queued/repeated click at the previous Continue position cannot approve
    // a certificate, even if authentication completes between click edges.
    click(&connection, root, window, 500, 480);
    click(&connection, root, window, 500, 480);
    std::thread::sleep(Duration::from_millis(100));
    let before_approval: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&index).unwrap()).unwrap();
    assert!(before_approval[0]["pairing_file"].is_null());
    assert!(before_approval[0]["fingerprint"].is_null());
    if password_mode {
        // Rejecting first contact never writes a pin or returned credential.
        click(&connection, root, window, 870, 580);
        std::thread::sleep(Duration::from_millis(100));
        let rejected: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&index).unwrap()).unwrap();
        assert!(rejected[0]["pairing_file"].is_null());
        assert!(rejected[0]["fingerprint"].is_null());
        // Username is retained, but the password was cleared on cancellation.
        click(&connection, root, window, 400, 400);
        wait_field_focus(&connection, window, 380);
        type_ascii(&connection, root, window, PASSWORD);
        click(&connection, root, window, 500, 480);
        wait_host_approval(&connection, window);
    }
    click(&connection, root, window, 500, 580);
    let deadline = Instant::now() + Duration::from_secs(15);
    let profiles: serde_json::Value = loop {
        let profiles: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&index).unwrap()).unwrap();
        if profiles[0]["pairing_file"].is_string() {
            break profiles;
        }
        assert!(
            launcher.0.try_wait().unwrap().is_none(),
            "launcher exited during pairing"
        );
        assert!(
            Instant::now() < deadline,
            "approved host access was not saved"
        );
        std::thread::sleep(Duration::from_millis(30));
    };
    assert_eq!(profiles[0]["address"], address);
    let saved = profiles[0]["pairing_file"].as_str().unwrap();
    for file in [index.as_path(), std::path::Path::new(saved)] {
        assert_eq!(
            std::fs::metadata(file).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
    let saved_pairing: serde_json::Value =
        serde_json::from_slice(&std::fs::read(saved).unwrap()).unwrap();
    let legacy: serde_json::Value =
        serde_json::from_slice(&std::fs::read(identity.join("pairing.json")).unwrap()).unwrap();
    assert_eq!(saved_pairing["fingerprint"], legacy["fingerprint"]);
    // Persistent hosts issue separately revocable credentials after code proof.
    assert_ne!(saved_pairing["token"], legacy["token"]);
    let window = desktop_window(&connection, root, &mut launcher, "Teleport", None);
    let desktop = desktop_window(
        &connection,
        root,
        &mut launcher,
        "Teleport - Remote Desktop",
        None,
    );
    assert_ne!(desktop, window);
    // The launcher's disconnect control owns and reaps the streaming child.
    connection
        .configure_window(
            window,
            &xproto::ConfigureWindowAux::new().stack_mode(xproto::StackMode::ABOVE),
        )
        .unwrap()
        .check()
        .unwrap();
    click(&connection, root, window, 870, 294);
    let deadline = Instant::now() + Duration::from_secs(10);
    while connection
        .query_tree(root)
        .unwrap()
        .reply()
        .unwrap()
        .children
        .contains(&desktop)
    {
        assert!(
            Instant::now() < deadline,
            "launcher did not disconnect streaming child"
        );
        std::thread::sleep(Duration::from_millis(30));
    }
    if password_mode {
        // An alias is an ordinary saved desktop; explicitly importing an
        // already trusted private file still shows first-contact approval.
        let alias = format!("127.0.0.1:0{}", address.rsplit(':').next().unwrap());
        add_desktop(&connection, root, window, "Work alias", &alias);
        click(&connection, root, window, 500, 294);
        wait_authentication_form(&connection, window);
        click(&connection, root, window, 915, 238);
        click(&connection, root, window, 400, 400);
        wait_field_focus(&connection, window, 380);
        type_ascii(&connection, root, window, saved);
        click(&connection, root, window, 500, 480);
        wait_host_approval(&connection, window);
        click(&connection, root, window, 500, 580);
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let profiles: serde_json::Value =
                serde_json::from_slice(&std::fs::read(&index).unwrap()).unwrap();
            if profiles.as_array().unwrap().len() == 2 && profiles[1]["pairing_file"].is_string() {
                assert_eq!(profiles[1]["address"], alias);
                let aliased: serde_json::Value = serde_json::from_slice(
                    &std::fs::read(profiles[1]["pairing_file"].as_str().unwrap()).unwrap(),
                )
                .unwrap();
                assert_eq!(aliased, saved_pairing);
                break;
            }
            assert!(
                Instant::now() < deadline,
                "approved identity alias was not saved"
            );
            std::thread::sleep(Duration::from_millis(30));
        }
        let desktop = desktop_window(
            &connection,
            root,
            &mut launcher,
            "Teleport - Remote Desktop",
            None,
        );
        assert_ne!(desktop, window);
        connection
            .configure_window(
                window,
                &xproto::ConfigureWindowAux::new().stack_mode(xproto::StackMode::ABOVE),
            )
            .unwrap()
            .check()
            .unwrap();
        click(&connection, root, window, 870, 294);
    }
}

/// Click actual SDL toolbar controls on an isolated X server. No real desktop input.
#[test]
#[ignore = "requires Xvfb, UDP and GStreamer runtime plugins"]
fn native_toolbar_monitor_reconnect_disconnect_and_launcher() {
    let temp = tempfile::tempdir().unwrap();
    let mut display = Process(
        Command::new("Xvfb")
            .args([
                "-displayfd",
                "1",
                "-screen",
                "0",
                "1600x1000x24",
                "-nolisten",
                "tcp",
            ])
            .stdout(Stdio::piped())
            .spawn()
            .unwrap(),
    );
    let mut number = String::new();
    std::io::BufReader::new(display.0.stdout.take().unwrap())
        .read_line(&mut number)
        .unwrap();
    let display_name = format!(":{}", number.trim());
    let (connection, screen) = x11rb::connect(Some(&display_name)).unwrap();
    let root = connection.setup().roots[screen].root;
    let socket = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    let address = socket.local_addr().unwrap().to_string();
    drop(socket);
    let pairing = temp.path().join("pairing.json");
    let mut host = Process(
        teleport(&display_name)
            .args([
                "host",
                "--source",
                "test",
                "--encoder",
                "software",
                "--width",
                "640",
                "--fps",
                "30",
                "--listen",
                &address,
                "--pairing-file",
            ])
            .arg(&pairing)
            .spawn()
            .unwrap(),
    );
    let deadline = Instant::now() + Duration::from_secs(15);
    while std::fs::read(&pairing)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
        .is_none()
    {
        assert!(host.0.try_wait().unwrap().is_none(), "host exited");
        assert!(Instant::now() < deadline, "host never published pairing");
        std::thread::sleep(Duration::from_millis(30));
    }
    let mut client = Process(
        teleport(&display_name)
            .args(["client", &address, "--software-decoder", "--pairing-file"])
            .arg(&pairing)
            // Force accelerated creation to fail. The connecting UI must
            // ignore this hint; the stream must retry software presentation.
            .env("SDL_RENDER_DRIVER", "opengl")
            .env("SDL_OPENGL_LIBRARY", "/nonexistent/libGL.so")
            .spawn()
            .unwrap(),
    );
    let first = desktop_window(
        &connection,
        root,
        &mut client,
        "Teleport - Remote Desktop",
        None,
    );
    // Stats is rendered locally; opening it must not disrupt the session.
    click(&connection, root, first, 955, 28);
    let stats_deadline = Instant::now() + Duration::from_secs(10);
    while !rendered_pixel(&connection, first, 760, 100, 0x101721) {
        assert!(
            Instant::now() < stats_deadline,
            "stats overlay did not render"
        );
        std::thread::sleep(Duration::from_millis(30));
    }
    click(&connection, root, first, 955, 28);
    // Fullscreen fills the local display. Chrome overlays the video and hides
    // after the pointer leaves it; returning to the top reveals local controls.
    let original = connection.get_geometry(first).unwrap().reply().unwrap();
    click(&connection, root, first, 1030, 28);
    let deadline = Instant::now() + Duration::from_secs(10);
    let fullscreen_width = loop {
        let geometry = connection.get_geometry(first).unwrap().reply().unwrap();
        if geometry.width == 1600 && geometry.height == 1000 {
            break geometry.width;
        }
        assert!(
            Instant::now() < deadline,
            "fullscreen did not cover the display"
        );
        std::thread::sleep(Duration::from_millis(30));
    };
    let move_pointer = |x, y| {
        let position = connection
            .translate_coordinates(first, root, x, y)
            .unwrap()
            .reply()
            .unwrap();
        connection
            .xtest_fake_input(
                xproto::MOTION_NOTIFY_EVENT,
                0,
                0,
                root,
                position.dst_x,
                position.dst_y,
                0,
            )
            .unwrap()
            .check()
            .unwrap();
        connection.flush().unwrap();
    };
    move_pointer(500, 500);
    let deadline = Instant::now() + Duration::from_secs(10);
    while rendered_pixel(&connection, first, 5, 5, 0x151b26) {
        assert!(
            Instant::now() < deadline,
            "fullscreen toolbar did not collapse"
        );
        std::thread::sleep(Duration::from_millis(30));
    }
    move_pointer(800, 1);
    let deadline = Instant::now() + Duration::from_secs(10);
    while !rendered_pixel(&connection, first, 5, 5, 0x151b26) {
        assert!(Instant::now() < deadline, "top edge did not reveal toolbar");
        std::thread::sleep(Duration::from_millis(30));
    }
    click(&connection, root, first, fullscreen_width as i16 - 250, 28);
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let geometry = connection.get_geometry(first).unwrap().reply().unwrap();
        if geometry.width == original.width && geometry.height == original.height {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "fullscreen did not restore window size"
        );
        std::thread::sleep(Duration::from_millis(30));
    }
    click(&connection, root, first, 56, 28);
    // Monitor 1 is a grayscale ball; monitor 2 is SMPTE color bars. Validate
    // actual switched video, not a changing window title. The center top bar
    // is green; tolerate codec/color-conversion rounding rather than exact RGB.
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let switched = rendered_rgb(&connection, first, 640, 200).is_some_and(|rgb| {
            ((rgb >> 8) & 255) > 128 && ((rgb >> 16) & 255) < 96 && (rgb & 255) < 96
        });
        if switched {
            break;
        }
        assert!(
            client.0.try_wait().unwrap().is_none(),
            "client exited during monitor switch"
        );
        assert!(
            Instant::now() < deadline,
            "monitor switch did not present SMPTE green video"
        );
        std::thread::sleep(Duration::from_millis(30));
    }
    assert_eq!(
        desktop_window(
            &connection,
            root,
            &mut client,
            "Teleport - Remote Desktop",
            None
        ),
        first
    );
    connection
        .change_window_attributes(
            root,
            &xproto::ChangeWindowAttributesAux::new()
                .event_mask(xproto::EventMask::SUBSTRUCTURE_NOTIFY),
        )
        .unwrap()
        .check()
        .unwrap();
    click(&connection, root, first, 1122, 28);
    // SDL may reuse the same X resource ID after rebuilding its video context.
    // Observe actual destruction, then wait for a newly connected desktop title.
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        if let Some(x11rb::protocol::Event::DestroyNotify(event)) =
            connection.poll_for_event().unwrap()
            && event.window == first
        {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "reconnect did not destroy the old session window"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    let second = desktop_window(
        &connection,
        root,
        &mut client,
        "Teleport - Remote Desktop",
        None,
    );
    click(&connection, root, second, 1219, 28);
    wait_exit(&mut client);

    let mut launcher = Process(
        teleport(&display_name)
            .arg("launcher")
            .env("XDG_CONFIG_HOME", temp.path().join("config"))
            .spawn()
            .unwrap(),
    );
    let window = desktop_window(&connection, root, &mut launcher, "Teleport", None);
    let protocols = connection
        .intern_atom(false, b"WM_PROTOCOLS")
        .unwrap()
        .reply()
        .unwrap()
        .atom;
    let delete = connection
        .intern_atom(false, b"WM_DELETE_WINDOW")
        .unwrap()
        .reply()
        .unwrap()
        .atom;
    let event = xproto::ClientMessageEvent::new(32, window, protocols, [delete, 0, 0, 0, 0]);
    connection
        .send_event(false, window, xproto::EventMask::NO_EVENT, event)
        .unwrap()
        .check()
        .unwrap();
    wait_exit(&mut launcher);
}
