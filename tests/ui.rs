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
    let first = desktop_window(&connection, root, &mut client, "monitor 1/2", None);
    click(&connection, root, first, 91, 22);
    assert_eq!(
        desktop_window(&connection, root, &mut client, "monitor 2/2", None),
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
    click(&connection, root, first, 1005, 22);
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
    let second = desktop_window(&connection, root, &mut client, "monitor", None);
    click(&connection, root, second, 1188, 22);
    wait_exit(&mut client);

    let mut launcher = Process(
        teleport(&display_name)
            .arg("launcher")
            .env("XDG_CONFIG_HOME", temp.path().join("config"))
            .spawn()
            .unwrap(),
    );
    let window = desktop_window(&connection, root, &mut launcher, "Connect", None);
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
