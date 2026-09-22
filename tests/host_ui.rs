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
};

struct Process(Child);
impl Drop for Process {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
fn until(mut predicate: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(30);
    while !predicate() {
        assert!(Instant::now() < deadline, "host manager test timed out");
        std::thread::sleep(Duration::from_millis(50));
    }
}
#[test]
#[ignore = "requires Xvfb and GStreamer runtime plugins"]
fn native_host_manager_configures_password_and_pairing() {
    let binary = std::env::var_os("TELEPORT_TEST_BINARY")
        .unwrap_or_else(|| env!("CARGO_BIN_EXE_teleport").into());
    let mut screen = Process(
        Command::new("Xvfb")
            .args([
                "-displayfd",
                "1",
                "-screen",
                "0",
                "1280x900x24",
                "-nolisten",
                "tcp",
            ])
            .stdout(Stdio::piped())
            .spawn()
            .unwrap(),
    );
    let mut number = String::new();
    std::io::BufReader::new(screen.0.stdout.take().unwrap())
        .read_line(&mut number)
        .unwrap();
    let display = format!(":{}", number.trim());
    let home = tempfile::tempdir_in("/tmp").unwrap();
    let directory = home.path().join(".local/state/teleport/host");
    let reservation = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    let address = reservation.local_addr().unwrap().to_string();
    drop(reservation);
    let mut host = Process(
        Command::new(&binary)
            .args([
                "host",
                "--listen",
                &address,
                "--source",
                "test",
                "--encoder",
                "software",
                "--width",
                "320",
                "--fps",
                "30",
                "--identity-dir",
            ])
            .arg(&directory)
            .stdout(Stdio::null())
            .spawn()
            .unwrap(),
    );
    until(|| {
        assert!(
            host.0.try_wait().unwrap().is_none(),
            "synthetic host exited"
        );
        directory.join("admin.sock").exists()
    });
    let mut manager = Process(
        Command::new(&binary)
            .arg("host-manager")
            .env("HOME", home.path())
            .env("DISPLAY", &display)
            .env("SDL_VIDEODRIVER", "x11")
            .env_remove("WAYLAND_DISPLAY")
            .spawn()
            .unwrap(),
    );
    let (connection, screen_number) = x11rb::connect(Some(&display)).unwrap();
    let root = connection.setup().roots[screen_number].root;
    let mut window = 0;
    until(|| {
        assert!(
            manager.0.try_wait().unwrap().is_none(),
            "host manager exited"
        );
        for candidate in connection
            .query_tree(root)
            .unwrap()
            .reply()
            .unwrap()
            .children
        {
            if let Ok(property) = connection
                .get_property(
                    false,
                    candidate,
                    xproto::AtomEnum::WM_NAME,
                    xproto::AtomEnum::ANY,
                    0,
                    1024,
                )
                .unwrap()
                .reply()
                && String::from_utf8_lossy(&property.value).contains("Teleport Host Settings")
                && painted_background(&connection, candidate)
            {
                window = candidate;
                return true;
            }
        }
        false
    });
    let mut click = |x, y| {
        until(|| {
            assert!(
                manager.0.try_wait().unwrap().is_none(),
                "host manager exited before click at {x},{y}"
            );
            painted_pixel(&connection, window, 29, 119, 0x24434b)
        });
        let position = connection
            .translate_coordinates(window, root, x, y)
            .unwrap()
            .reply()
            .unwrap();
        for (kind, detail, px, py) in [
            (
                xproto::MOTION_NOTIFY_EVENT,
                0,
                position.dst_x,
                position.dst_y,
            ),
            (xproto::BUTTON_PRESS_EVENT, 1, 0, 0),
            (xproto::BUTTON_RELEASE_EVENT, 1, 0, 0),
        ] {
            connection
                .xtest_fake_input(kind, detail, 0, root, px, py, 0)
                .unwrap()
                .check()
                .unwrap();
        }
        connection.flush().unwrap();
        std::thread::sleep(Duration::from_millis(100));
    };
    let setup = connection.setup();
    let mapping = connection
        .get_keyboard_mapping(setup.min_keycode, setup.max_keycode - setup.min_keycode + 1)
        .unwrap()
        .reply()
        .unwrap();
    let key = |symbol: u32| {
        let index = mapping
            .keysyms
            .chunks(usize::from(mapping.keysyms_per_keycode))
            .position(|symbols| symbols[0] == symbol)
            .expect("test key missing");
        for kind in [xproto::KEY_PRESS_EVENT, xproto::KEY_RELEASE_EVENT] {
            connection
                .xtest_fake_input(kind, setup.min_keycode + index as u8, 0, root, 0, 0, 0)
                .unwrap()
                .check()
                .unwrap();
        }
        connection.flush().unwrap();
        std::thread::sleep(Duration::from_millis(30));
    };
    connection
        .set_input_focus(xproto::InputFocus::PARENT, window, x11rb::CURRENT_TIME)
        .unwrap()
        .check()
        .unwrap();
    click(80, 292);
    for byte in b"testuser" {
        key(u32::from(*byte));
    }
    click(350, 292);
    for byte in b"publictestpassword123" {
        key(u32::from(*byte));
    }
    click(650, 292);
    for byte in b"publictestpassword123" {
        key(u32::from(*byte));
    }
    click(100, 350);
    let status = || {
        let output = Command::new(&binary)
            .args(["host-admin", "--identity-dir"])
            .arg(&directory)
            .arg("status")
            .output()
            .unwrap();
        assert!(output.status.success(), "local admin status failed");
        serde_json::from_slice::<serde_json::Value>(&output.stdout).unwrap()
    };
    until(|| status()["username"] == "testuser");
    click(100, 442);
    until(|| status()["pairing_open"] == true);
    click(320, 442);
    until(|| status()["pairing_open"] == false);
    click(320, 350);
    assert_eq!(
        status()["username"],
        "testuser",
        "disable needs confirmation"
    );
    click(320, 350);
    until(|| status()["username"].is_null());
    key(0xff1b);
    until(|| {
        manager.0.try_wait().unwrap().is_some_and(|s| {
            assert!(s.success());
            true
        })
    });
}

// SDL can replace its initial X window while constructing a software canvas.
// Discover the rendered window, not merely the first transient matching title.
fn painted_background(connection: &x11rb::rust_connection::RustConnection, window: u32) -> bool {
    painted_pixel(connection, window, 900, 780, 0x0c121b)
}
fn painted_pixel(
    connection: &x11rb::rust_connection::RustConnection,
    window: u32,
    x: i16,
    y: i16,
    color: u32,
) -> bool {
    let Ok(cookie) =
        connection.get_image(xproto::ImageFormat::Z_PIXMAP, window, x, y, 1, 1, u32::MAX)
    else {
        return false;
    };
    let Ok(reply) = cookie.reply() else {
        return false;
    };
    let Ok(bytes) = <[u8; 4]>::try_from(reply.data.as_slice()) else {
        return false;
    };
    let pixel = if connection.setup().image_byte_order == xproto::ImageOrder::LSB_FIRST {
        u32::from_le_bytes(bytes)
    } else {
        u32::from_be_bytes(bytes)
    };
    pixel & 0xffffff == color
}
