#![cfg(target_os = "linux")]

use serde_json::{Value, json};
use std::{
    io::{BufRead, Read, Write},
    path::{Path, PathBuf},
    process::{Child, Command, ExitStatus, Stdio},
    sync::mpsc,
    time::{Duration, Instant},
};

struct Process(Child);
impl Drop for Process {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
fn teleport() -> Command {
    let mut command = Command::new(
        std::env::var_os("TELEPORT_TEST_BINARY")
            .unwrap_or_else(|| env!("CARGO_BIN_EXE_teleport").into()),
    );
    if std::env::var_os("TELEPORT_TEST_BINARY").is_some() {
        for name in [
            "GST_PLUGIN_PATH",
            "GST_PLUGIN_PATH_1_0",
            "GST_PLUGIN_SYSTEM_PATH",
            "GST_PLUGIN_SYSTEM_PATH_1_0",
            "GST_PLUGIN_SCANNER",
            "GST_PLUGIN_SCANNER_1_0",
        ] {
            command.env_remove(name);
        }
    }
    command
}
fn finish(process: &mut Process, timeout: Duration) -> (ExitStatus, String, String) {
    let deadline = Instant::now() + timeout;
    let status = loop {
        if let Some(status) = process.0.try_wait().unwrap() {
            break status;
        }
        assert!(
            Instant::now() < deadline,
            "isolated access-test process did not exit within {timeout:?}"
        );
        std::thread::sleep(Duration::from_millis(20));
    };
    let mut stdout = String::new();
    let mut stderr = String::new();
    if let Some(mut pipe) = process.0.stdout.take() {
        pipe.read_to_string(&mut stdout).unwrap();
    }
    if let Some(mut pipe) = process.0.stderr.take() {
        pipe.read_to_string(&mut stderr).unwrap();
    }
    (status, stdout, stderr)
}
fn admin(directory: &Path, request: Value) -> Value {
    let mut socket = std::os::unix::net::UnixStream::connect(directory.join("admin.sock")).unwrap();
    socket
        .set_read_timeout(Some(Duration::from_secs(20)))
        .unwrap();
    socket
        .set_write_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let bytes = serde_json::to_vec(&request).unwrap();
    socket
        .write_all(&(bytes.len() as u32).to_be_bytes())
        .unwrap();
    socket.write_all(&bytes).unwrap();
    let mut length = [0; 4];
    socket.read_exact(&mut length).unwrap();
    let length = u32::from_be_bytes(length) as usize;
    assert!(length <= 65536);
    let mut bytes = vec![0; length];
    socket.read_exact(&mut bytes).unwrap();
    let response: Value = serde_json::from_slice(&bytes).unwrap();
    assert!(
        response["error"].is_null(),
        "isolated admin operation failed"
    );
    response
}
fn enroll(directory: &Path, address: &str, config: &Path) -> PathBuf {
    let response = admin(directory, json!("OpenPairing"));
    let code = response["code"].as_str().unwrap();
    let mut child = Process(
        teleport()
            .args(["pair", address])
            .env("XDG_CONFIG_HOME", config)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap(),
    );
    writeln!(child.0.stdin.take().unwrap(), "{code}").unwrap();
    let (status, stdout, stderr) = finish(&mut child, Duration::from_secs(15));
    assert!(status.success(), "test code enrollment failed: {stderr}");
    assert!(!stdout.contains(code), "client echoed enrollment code");
    let profiles: Value =
        serde_json::from_slice(&std::fs::read(config.join("teleport/profiles.json")).unwrap())
            .unwrap();
    PathBuf::from(profiles[0]["pairing_file"].as_str().unwrap())
}
fn client(address: &str, credential: &Path, frames: &str) -> Process {
    Process(
        teleport()
            .args(["client", address, "--pairing-file"])
            .arg(credential)
            .args(["--headless-frames", frames, "--software-decoder"])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap(),
    )
}
fn assert_connects(address: &str, credential: &Path) {
    let mut process = client(address, credential, "12");
    let (status, stdout, stderr) = finish(&mut process, Duration::from_secs(20));
    assert!(
        status.success() && stdout.contains("SMOKE PASS"),
        "trusted test client failed: {stdout} {stderr}"
    );
}
fn expect_log(receiver: &mpsc::Receiver<String>, message: &str) {
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        let line = receiver
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .unwrap_or_else(|_| panic!("synthetic host did not report: {message}"));
        if line.contains(message) {
            return;
        }
    }
}

/// Exercises revocation through the actual persistent host and pinned QUIC
/// desktop transport, not only an access-record authorization predicate.
#[test]
#[ignore = "requires local TCP/UDP sockets and GStreamer runtime plugins"]
fn managed_active_session_revokes_while_legacy_trust_survives() {
    let temporary = tempfile::tempdir_in("/tmp").unwrap();
    let directory = temporary.path().join("host");
    let reservation = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    let address = reservation.local_addr().unwrap().to_string();
    drop(reservation);
    let mut host = Process(
        teleport()
            .args([
                "host",
                "--source",
                "test",
                "--encoder",
                "software",
                "--width",
                "320",
                "--fps",
                "30",
                "--listen",
                &address,
                "--identity-dir",
            ])
            .arg(&directory)
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap(),
    );
    let (sender, receiver) = mpsc::channel();
    let stdout = host.0.stdout.take().unwrap();
    std::thread::spawn(move || {
        for line in std::io::BufReader::new(stdout)
            .lines()
            .map_while(Result::ok)
        {
            let _ = sender.send(line);
        }
    });
    let ready_deadline = Instant::now() + Duration::from_secs(15);
    while !directory.join("admin.sock").exists() {
        assert!(
            host.0.try_wait().unwrap().is_none(),
            "synthetic test host exited"
        );
        assert!(
            Instant::now() < ready_deadline,
            "synthetic host did not become ready"
        );
        std::thread::sleep(Duration::from_millis(30));
    }
    let legacy = directory.join("pairing.json");
    let legacy_before = std::fs::read(&legacy).unwrap();
    let first = enroll(&directory, &address, &temporary.path().join("first-client"));
    let first_record: Value = serde_json::from_slice(&std::fs::read(&first).unwrap()).unwrap();
    let legacy_record: Value = serde_json::from_slice(&legacy_before).unwrap();
    assert_eq!(first_record["fingerprint"], legacy_record["fingerprint"]);
    assert_ne!(first_record["token"], legacy_record["token"]);
    let first_id = admin(&directory, json!("Status"))["devices"][0]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let second = enroll(
        &directory,
        &address,
        &temporary.path().join("second-client"),
    );
    assert_eq!(
        admin(&directory, json!("Status"))["devices"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    let mut active = client(&address, &first, "100000");
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        assert!(
            active.0.try_wait().unwrap().is_none(),
            "managed client failed before revocation"
        );
        let line = receiver
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .expect("host did not establish managed desktop session");
        if line.contains("native client connected") {
            break;
        }
    }
    std::thread::sleep(Duration::from_millis(250));
    assert!(
        active.0.try_wait().unwrap().is_none(),
        "managed session failed before administrative revocation"
    );
    let revoked_at = Instant::now();
    admin(&directory, json!({"RevokeDevice": {"id": first_id}}));
    let (status, stdout, stderr) = finish(&mut active, Duration::from_secs(4));
    assert!(
        !status.success(),
        "revoked active session continued successfully"
    );
    assert!(revoked_at.elapsed() < Duration::from_secs(4));
    assert!(
        !stdout.contains("SMOKE PASS") && !stderr.contains("timed out decoding"),
        "client exited from unrelated frame timeout"
    );
    expect_log(&receiver, "device access revoked");
    assert_connects(&address, &second);
    assert_connects(&address, &legacy);

    // Account configuration must not rotate already trusted code-enrolled
    // devices or legacy credentials. Password-issued behavior has separate
    // access backend tests; this exercises the live admin/host interaction.
    admin(
        &directory,
        json!({"SetPassword": {"username": "access_test", "password": "public integration test password 123"}}),
    );
    assert_connects(&address, &second);
    admin(
        &directory,
        json!({"SetPassword": {"username": "access_test", "password": "different public integration password 456"}}),
    );
    admin(&directory, json!("DisablePassword"));
    assert_connects(&address, &second);
    admin(&directory, json!("RevokeAllDevices"));
    assert!(
        admin(&directory, json!("Status"))["devices"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    for credential in [&first, &second] {
        for _ in receiver.try_iter() {}
        let mut revoked = client(&address, credential, "12");
        assert!(
            !finish(&mut revoked, Duration::from_secs(20)).0.success(),
            "revoked credential reconnected"
        );
        expect_log(&receiver, "rejected unauthorized client");
    }
    assert_connects(&address, &legacy);
    assert_eq!(
        std::fs::read(&legacy).unwrap(),
        legacy_before,
        "administration rotated legacy host identity"
    );
    assert!(
        host.0.try_wait().unwrap().is_none(),
        "revocation stopped the host"
    );
    // Synthetic capture has no real input device. This test intentionally makes
    // no assertion about physical held-key release or compositor behavior.
}
