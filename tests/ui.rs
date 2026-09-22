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

fn rendered_pixel(connection: &RustConnection, window: u32, x: i16, y: i16, rgb: u32) -> bool {
    let Ok(cookie) =
        connection.get_image(xproto::ImageFormat::Z_PIXMAP, window, x, y, 1, 1, u32::MAX)
    else {
        return false;
    };
    let Ok(image) = cookie.reply() else {
        return false;
    };
    let Ok(bytes) = <[u8; 4]>::try_from(image.data.as_slice()) else {
        return false;
    };
    let pixel = if connection.setup().image_byte_order == xproto::ImageOrder::LSB_FIRST {
        u32::from_le_bytes(bytes)
    } else {
        u32::from_be_bytes(bytes)
    };
    pixel & 0xffffff == rgb
}

fn wait_field_focus(connection: &RustConnection, window: u32, y: i16) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !rendered_pixel(connection, window, 304, y, 0x5bdfc9) {
        assert!(
            Instant::now() < deadline,
            "launcher never rendered focused field"
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
    click(&connection, root, window, 400, 200);
    wait_field_focus(&connection, window, 180);
    type_ascii(&connection, root, window, &address);
    std::thread::sleep(Duration::from_millis(100));
    if password_mode {
        click(&connection, root, window, 400, 560);
        type_ascii(&connection, root, window, "work");
    } else {
        // Password login is now the default; exercise the optional legacy code tab.
        click(&connection, root, window, 650, 645);
    }
    click(&connection, root, window, 400, 603);
    wait_field_focus(&connection, window, 585);
    type_ascii(
        &connection,
        root,
        window,
        if password_mode { PASSWORD } else { &code },
    );
    std::thread::sleep(Duration::from_millis(100));
    click(&connection, root, window, 870, 603);
    let index = config.join("teleport/profiles.json");
    let deadline = Instant::now() + Duration::from_secs(15);
    let profiles: serde_json::Value = loop {
        if let Some(profiles) = std::fs::read(&index)
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        {
            break profiles;
        }
        assert!(
            launcher.0.try_wait().unwrap().is_none(),
            "launcher exited during pairing"
        );
        assert!(
            Instant::now() < deadline,
            "pairing button did not save a trusted host"
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
    click(&connection, root, window, 500, 294);
    let desktop = desktop_window(&connection, root, &mut launcher, "display 1/2", None);
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
        // An explicit address alias reuses the pinned identity, not network trust.
        let alias = format!("127.0.0.1:0{}", address.rsplit(':').next().unwrap());
        click(&connection, root, window, 400, 200);
        // Xvfb has no window manager to restore keyboard focus after the
        // streaming child closes. Focus before pressing the modifier too.
        connection
            .set_input_focus(xproto::InputFocus::PARENT, window, x11rb::CURRENT_TIME)
            .unwrap()
            .check()
            .unwrap();
        connection
            .xtest_fake_input(xproto::KEY_PRESS_EVENT, 37, 0, root, 0, 0, 0)
            .unwrap();
        type_ascii(&connection, root, window, "a");
        connection
            .xtest_fake_input(xproto::KEY_RELEASE_EVENT, 37, 0, root, 0, 0, 0)
            .unwrap();
        connection.flush().unwrap();
        type_ascii(&connection, root, window, &alias);
        click(&connection, root, window, 870, 645);
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let profiles: serde_json::Value =
                serde_json::from_slice(&std::fs::read(&index).unwrap()).unwrap();
            if profiles.as_array().unwrap().len() == 2 {
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
                "saved identity alias was not created"
            );
            std::thread::sleep(Duration::from_millis(30));
        }
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
            .args([
                "client",
                &address,
                "--software-renderer",
                "--software-decoder",
                "--pairing-file",
            ])
            .arg(&pairing)
            .spawn()
            .unwrap(),
    );
    let first = desktop_window(&connection, root, &mut client, "display 1/2", None);
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
    click(&connection, root, first, 56, 28);
    assert_eq!(
        desktop_window(&connection, root, &mut client, "display 2/2", None),
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
    let second = desktop_window(&connection, root, &mut client, "display", None);
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
