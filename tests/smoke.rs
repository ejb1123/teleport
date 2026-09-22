#![cfg(target_os = "linux")]

use std::{
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

struct Process(Child);
impl Drop for Process {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
#[ignore = "requires local UDP sockets and GStreamer runtime plugins"]
fn negotiated_resolution_and_native_portrait() {
    let temp = tempfile::tempdir().unwrap();
    let pairing = temp.path().join("pairing.json");
    let socket = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    let address = socket.local_addr().unwrap().to_string();
    drop(socket);
    let mut host = Process(
        teleport()
            .args([
                "host",
                "--source",
                "test",
                "--encoder",
                "software",
                "--listen",
                &address,
                "--width",
                "640",
                "--pairing-file",
            ])
            .arg(&pairing)
            .stdout(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let deadline = Instant::now() + Duration::from_secs(15);
    while std::fs::read(&pairing)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
        .is_none()
    {
        assert!(host.0.try_wait().unwrap().is_none(), "test host exited");
        assert!(Instant::now() < deadline, "test host did not become ready");
        std::thread::sleep(Duration::from_millis(30));
    }
    for (codec, width, monitor, expected_width, expected_height, software) in [
        ("h264", "1920", "0", 1920, 1080, true),
        ("h264", "0", "1", 720, 1280, true),
        ("h265", "1920", "0", 1920, 1080, true),
        ("h265", "0", "1", 720, 1280, true),
        ("h265", "640", "0", 640, 360, false),
    ] {
        let mut command = teleport();
        command
            .args(["client", &address, "--pairing-file"])
            .arg(&pairing)
            .args([
                "--headless-frames",
                "15",
                "--codec",
                codec,
                "--width",
                width,
                "--fps",
                "30",
                "--bitrate",
                "4000",
                "--monitor",
                monitor,
            ]);
        if software {
            command.arg("--software-decoder");
        }
        let output = command.output().unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            output.status.success(),
            "negotiation failed: {stdout} {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            stdout.contains("SMOKE PASS"),
            "video did not decode: {stdout}"
        );
        assert!(
            stdout.contains(&format!("width={expected_width}")),
            "wrong negotiated width: {stdout}"
        );
        assert!(
            stdout.contains(&format!("height={expected_height}")),
            "wrong negotiated height: {stdout}"
        );
        assert!(
            stdout.contains("receive_to_decode_us="),
            "pipeline diagnostics missing: {stdout}"
        );
    }
}

#[test]
#[ignore = "requires local TCP/UDP sockets and GStreamer plugins"]
fn one_time_code_saves_trust_and_connects_without_file_transfer() {
    use std::io::{BufRead, Write};
    let temp = tempfile::tempdir().unwrap();
    let identity = temp.path().join("identity");
    let config = temp.path().join("client-config");
    let socket = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    let address = socket.local_addr().unwrap().to_string();
    drop(socket);
    let mut host = Process(
        teleport()
            .args([
                "host",
                "--source",
                "test",
                "--encoder",
                "software",
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
                let code = rest.split_whitespace().next().unwrap().to_owned();
                let _ = sender.send(code);
            }
        }
    });
    let code = receiver
        .recv_timeout(Duration::from_secs(15))
        .expect("host did not display a code");
    let enroll = |code: &str| {
        let mut child = teleport()
            .args(["pair", &address])
            .env("XDG_CONFIG_HOME", &config)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        writeln!(child.stdin.take().unwrap(), "{code}").unwrap();
        child.wait_with_output().unwrap()
    };
    let wrong = if code == "000-000" {
        "111111"
    } else {
        "000000"
    };
    assert!(!enroll(wrong).status.success(), "wrong code accepted");
    assert!(!config.join("teleport/profiles.json").exists());
    let output = enroll(&code);
    assert!(
        output.status.success(),
        "pairing failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        !String::from_utf8_lossy(&output.stdout).contains(&code),
        "client echoed secret code"
    );
    let profiles: serde_json::Value =
        serde_json::from_slice(&std::fs::read(config.join("teleport/profiles.json")).unwrap())
            .unwrap();
    let credential = profiles[0]["pairing_file"].as_str().unwrap();
    let saved = std::fs::read(credential).unwrap();
    let host_credential: serde_json::Value =
        serde_json::from_slice(&std::fs::read(identity.join("pairing.json")).unwrap()).unwrap();
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&saved).unwrap(),
        host_credential
    );
    assert!(!enroll(&code).status.success(), "used code accepted again");
    assert_eq!(
        std::fs::read(credential).unwrap(),
        saved,
        "failed pairing modified saved trust"
    );
    for _ in 0..2 {
        let output = teleport()
            .args([
                "client",
                &address,
                "--headless-frames",
                "15",
                "--pairing-file",
                credential,
            ])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "saved-pairing desktop connection failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

/// Strip development-shell discovery paths when exercising a packaged binary.
fn teleport() -> Command {
    if let Some(binary) = std::env::var_os("TELEPORT_TEST_BINARY") {
        let mut command = Command::new(binary);
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
        command
    } else {
        Command::new(env!("CARGO_BIN_EXE_teleport"))
    }
}

/// Real TLS/QUIC/MoQ, H.264 encode/decode, and control heartbeat, with no display.
/// Run outside the Nix build sandbox: cargo test --test smoke -- --ignored --nocapture
#[test]
#[ignore = "requires local UDP sockets and GStreamer runtime plugins"]
fn native_moq_video() {
    let temp = tempfile::tempdir().unwrap();
    let pairing = temp.path().join("pairing.json");
    let socket = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    let address = socket.local_addr().unwrap().to_string();
    drop(socket);
    let mut host = Process(
        teleport()
            .args([
                "host",
                "--source",
                "test",
                "--listen",
                &address,
                "--width",
                "640",
                "--fps",
                "30",
                "--pairing-file",
            ])
            .arg(&pairing)
            .stdout(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let deadline = Instant::now() + Duration::from_secs(15);
    while !pairing.exists() {
        assert!(
            host.0.try_wait().unwrap().is_none(),
            "host exited before pairing"
        );
        assert!(Instant::now() < deadline, "host startup timed out");
        std::thread::sleep(Duration::from_millis(50));
    }
    // A complete JSON file indicates the host finished publishing credentials.
    loop {
        if serde_json::from_slice::<serde_json::Value>(&std::fs::read(&pairing).unwrap()).is_ok() {
            break;
        }
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(10));
    }
    let output = teleport()
        .args([
            "client",
            &address,
            "--headless-frames",
            "45",
            "--pairing-file",
        ])
        .arg(&pairing)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "client failed:\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("SMOKE PASS"));
    // Same authenticated host, explicitly selected HEVC and automatic hardware
    // decoding. Codec choice must not change or rotate trust.
    let output = teleport()
        .args([
            "client",
            &address,
            "--codec",
            "h265",
            "--headless-frames",
            "30",
            "--pairing-file",
        ])
        .arg(&pairing)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "HEVC hardware-auto stream failed: {} {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("SMOKE PASS"));
    // Explicit HDR on the synthetic source: authenticated transport must retain
    // Main 10/PQ metadata and the decoder's high-precision planes. This is not
    // a desktop-capture or physical HDR-display acceptance test.
    let output = teleport()
        .args([
            "client",
            &address,
            "--codec",
            "h265",
            "--dynamic-range",
            "hdr10",
            "--headless-frames",
            "15",
            "--pairing-file",
        ])
        .arg(&pairing)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "HDR transport failed: {} {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let hdr_output = String::from_utf8_lossy(&output.stdout);
    assert!(
        hdr_output.contains("SMOKE PASS") && hdr_output.contains("HDR10"),
        "{hdr_output}"
    );
    // Reconnect to SDR on the same host: tear down the old pipeline and controls.
    let output = teleport()
        .args([
            "client",
            &address,
            "--headless-frames",
            "15",
            "--pairing-file",
        ])
        .arg(&pairing)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "reconnect failed: {} {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let original: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&pairing).unwrap()).unwrap();
    for field in ["token", "fingerprint"] {
        let mut bad = original.clone();
        bad[field] = "0".repeat(64).into();
        let path = temp.path().join(format!("bad-{field}.json"));
        std::fs::write(&path, serde_json::to_vec(&bad).unwrap()).unwrap();
        let output = teleport()
            .args([
                "client",
                &address,
                "--headless-frames",
                "1",
                "--pairing-file",
            ])
            .arg(path)
            .output()
            .unwrap();
        assert!(!output.status.success(), "invalid {field} was accepted");
    }
}

/// Saved client credentials must survive a real server restart, and monitor
/// switching must renegotiate dimensions instead of stretching stale frames.
#[test]
#[ignore = "requires local UDP sockets and GStreamer runtime plugins"]
fn persistent_identity_and_monitor_switch() {
    let temp = tempfile::tempdir().unwrap();
    let identity = temp.path().join("identity");
    let pairing = identity.join("pairing.json");
    let saved_pairing = temp.path().join("client-pairing.json");
    let socket = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    let address = socket.local_addr().unwrap().to_string();
    drop(socket);

    for attempt in 0..2 {
        let mut host = Process(
            teleport()
                .args([
                    "host",
                    "--source",
                    "test",
                    "--listen",
                    &address,
                    "--width",
                    "640",
                    "--fps",
                    "30",
                    "--audio-source",
                    "test",
                    "--identity-dir",
                ])
                .arg(&identity)
                .stdout(Stdio::null())
                .spawn()
                .unwrap(),
        );
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            assert!(
                host.0.try_wait().unwrap().is_none(),
                "host exited during startup"
            );
            if std::fs::read(&pairing)
                .ok()
                .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
                .is_some()
            {
                break;
            }
            assert!(Instant::now() < deadline, "host startup timed out");
            std::thread::sleep(Duration::from_millis(50));
        }
        if attempt == 0 {
            std::fs::copy(&pairing, &saved_pairing).unwrap();
        }
        // On restart the pairing already exists before bind; allow startup to
        // finish without modifying the client copy or trusting a new fingerprint.
        std::thread::sleep(Duration::from_millis(500));
        let output = teleport()
            .args([
                "client",
                &address,
                "--headless-frames",
                "45",
                "--smoke-switch-monitor",
                "--smoke-audio",
                "--pairing-file",
            ])
            .arg(&saved_pairing)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "persistent/switch client attempt {attempt} failed: {} {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(String::from_utf8_lossy(&output.stdout).contains("SMOKE PASS"));
        assert_eq!(
            std::fs::read(&pairing).unwrap(),
            std::fs::read(&saved_pairing).unwrap(),
            "host identity changed across restart"
        );
        // Drop kills and reaps this host before the same port/identity is reused.
        drop(host);
    }
}

#[test]
#[ignore = "requires Xvfb, local UDP sockets and GStreamer runtime plugins"]
fn x11_capture_input_and_native_window() {
    use std::io::BufRead;
    use x11rb::{connection::Connection, protocol::xproto::ConnectionExt};
    let mut display = Process(
        Command::new("Xvfb")
            .args([
                "-displayfd",
                "1",
                "-screen",
                "0",
                "800x600x24",
                "-nolisten",
                "tcp",
            ])
            .stdout(Stdio::piped())
            .spawn()
            .expect("Xvfb must be installed"),
    );
    let mut number = String::new();
    std::io::BufReader::new(display.0.stdout.take().unwrap())
        .read_line(&mut number)
        .unwrap();
    let display_name = format!(":{}", number.trim());
    let (connection, screen) = x11rb::connect(Some(&display_name)).unwrap();
    let root = connection.setup().roots[screen].root;
    // Focus an isolated test window so we can observe XTest key transitions.
    let window = connection.generate_id().unwrap();
    use x11rb::protocol::xproto::{CreateWindowAux, EventMask, InputFocus, WindowClass};
    connection
        .create_window(
            0,
            window,
            root,
            0,
            0,
            800,
            600,
            0,
            WindowClass::INPUT_OUTPUT,
            0,
            &CreateWindowAux::new().event_mask(EventMask::KEY_PRESS | EventMask::KEY_RELEASE),
        )
        .unwrap()
        .check()
        .unwrap();
    connection.map_window(window).unwrap().check().unwrap();
    connection
        .set_input_focus(InputFocus::PARENT, window, 0u32)
        .unwrap()
        .check()
        .unwrap();
    let temp = tempfile::tempdir().unwrap();
    let pairing = temp.path().join("pairing.json");
    let socket = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    let address = socket.local_addr().unwrap().to_string();
    drop(socket);
    let mut host = Process(
        teleport()
            .env_remove("WAYLAND_DISPLAY")
            .env("XDG_SESSION_TYPE", "x11")
            .env("DISPLAY", &display_name)
            .args([
                "host",
                "--clipboard",
                "--source",
                "x11",
                "--listen",
                &address,
                "--width",
                "640",
                "--fps",
                "30",
                "--pairing-file",
            ])
            .arg(&pairing)
            .stdout(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let deadline = Instant::now() + Duration::from_secs(15);
    while std::fs::read(&pairing)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
        .is_none()
    {
        assert!(host.0.try_wait().unwrap().is_none());
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(50));
    }
    let output = teleport()
        .args([
            "client",
            &address,
            "--headless-frames",
            "30",
            "--smoke-input",
            "--clipboard",
            "--smoke-clipboard",
            "--pairing-file",
        ])
        .arg(&pairing)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "input client failed: {} {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let mut transitions = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        while let Some(event) = connection.poll_for_event().unwrap() {
            match event {
                x11rb::protocol::Event::KeyPress(e) => transitions.push((e.detail, true)),
                x11rb::protocol::Event::KeyRelease(e) => transitions.push((e.detail, false)),
                _ => (),
            }
        }
        if transitions.contains(&(37, false)) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "disconnect did not release Ctrl; events: {transitions:?}"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(
        transitions.contains(&(38, true)) && transitions.contains(&(38, false)),
        "A press/release missing: {transitions:?}"
    );
    assert!(
        connection
            .query_keymap()
            .unwrap()
            .reply()
            .unwrap()
            .keys
            .iter()
            .all(|byte| *byte == 0)
    );
    let pointer = connection.query_pointer(root).unwrap().reply().unwrap();
    assert!((pointer.root_x - 399).abs() <= 1 && (pointer.root_y - 299).abs() <= 1);

    // Suspend only our isolated test client, simulating sleep/network loss while
    // Ctrl is held. The host must release it without a clean client disconnect.
    let mut sleeping_client = Process(
        teleport()
            .args([
                "client",
                &address,
                "--headless-frames",
                "10000",
                "--smoke-input",
                "--pairing-file",
            ])
            .arg(&pairing)
            .stdout(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let keys = connection.query_keymap().unwrap().reply().unwrap().keys;
        if keys[37 / 8] & (1 << (37 % 8)) != 0 {
            break;
        }
        assert!(
            sleeping_client.0.try_wait().unwrap().is_none(),
            "sleep test client exited before holding Ctrl"
        );
        assert!(Instant::now() < deadline, "sleep test never held Ctrl");
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(
        Command::new("kill")
            .args(["-STOP", &sleeping_client.0.id().to_string()])
            .status()
            .unwrap()
            .success()
    );
    let deadline = Instant::now() + Duration::from_secs(8);
    loop {
        let keys = connection.query_keymap().unwrap().reply().unwrap().keys;
        if keys.iter().all(|byte| *byte == 0) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "heartbeat timeout did not release held Ctrl"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    drop(sleeping_client);

    let output = teleport()
        .env_remove("WAYLAND_DISPLAY")
        .env("SDL_VIDEODRIVER", "x11")
        .env("DISPLAY", &display_name)
        .args([
            "client",
            &address,
            "--software-renderer",
            "--exit-after-frames",
            "30",
            "--pairing-file",
        ])
        .arg(&pairing)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "native window failed: {} {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    // A systemd service stop sends SIGTERM, not Ctrl+C. Terminate only our own
    // host while its client holds Ctrl and verify graceful input cleanup.
    let mut stopping_client = Process(
        teleport()
            .args([
                "client",
                &address,
                "--headless-frames",
                "10000",
                "--smoke-input",
                "--pairing-file",
            ])
            .arg(&pairing)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let keys = connection.query_keymap().unwrap().reply().unwrap().keys;
        if keys[37 / 8] & (1 << (37 % 8)) != 0 {
            break;
        }
        assert!(
            stopping_client.0.try_wait().unwrap().is_none(),
            "service-stop test client exited before holding Ctrl"
        );
        assert!(
            Instant::now() < deadline,
            "service-stop test never held Ctrl"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(
        Command::new("kill")
            .args(["-TERM", &host.0.id().to_string()])
            .status()
            .unwrap()
            .success()
    );
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(status) = host.0.try_wait().unwrap() {
            assert!(status.success(), "SIGTERM host shutdown failed: {status}");
            break;
        }
        assert!(Instant::now() < deadline, "SIGTERM host shutdown timed out");
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(
        connection
            .query_keymap()
            .unwrap()
            .reply()
            .unwrap()
            .keys
            .iter()
            .all(|byte| *byte == 0),
        "SIGTERM did not release held Ctrl"
    );
}
