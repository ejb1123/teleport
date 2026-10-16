#![cfg(target_os = "linux")]
//! Real relay, host and client processes: the host is reachable only through
//! `teleport relay`, and every session is end-to-end encrypted.

use std::{
    io::{BufRead, BufReader},
    process::{Child, Command, Output, Stdio},
    time::{Duration, Instant},
};

const RELAY_KEY: &str = "integration-test-relay-key";

struct Process(Child);
impl Drop for Process {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn teleport() -> Command {
    Command::new(
        std::env::var_os("TELEPORT_TEST_BINARY")
            .unwrap_or_else(|| env!("CARGO_BIN_EXE_teleport").into()),
    )
}

fn free_udp() -> String {
    let socket = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    socket.local_addr().unwrap().to_string()
}

struct Setup {
    _relay: Process,
    _host: Process,
    relay_url: String,
    relay_fingerprint: String,
    temp: tempfile::TempDir,
    /// The host's direct address (UDP sessions, TCP code pairing).
    host_address: String,
}

fn setup() -> Setup {
    setup_with_versions(&[])
}

/// `versions` restricts the relay (e.g. IETF-only, as provider relays are).
fn setup_with_versions(versions: &[&str]) -> Setup {
    setup_with(versions, &[])
}

fn setup_with(versions: &[&str], host_args: &[&str]) -> Setup {
    let temp = tempfile::tempdir().unwrap();
    let host_address = free_udp();
    let relay_address = free_udp();
    let mut relay = Process(
        teleport()
            .args(["relay", "--listen", &relay_address, "--key", RELAY_KEY])
            .args(versions.iter().flat_map(|version| ["--version", version]))
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let mut lines = BufReader::new(relay.0.stdout.take().unwrap()).lines();
    let relay_fingerprint = lines
        .by_ref()
        .map_while(Result::ok)
        .find_map(|line| line.strip_prefix("RELAY FINGERPRINT ").map(str::to_owned))
        .expect("relay did not print its fingerprint");
    // Keep draining: a relay blocked on a full stdout pipe stops forwarding.
    let relay_log = temp.path().join("relay.log");
    std::thread::spawn(move || {
        use std::io::Write;
        let mut file = std::fs::File::create(relay_log).unwrap();
        for line in lines.map_while(Result::ok) {
            let _ = writeln!(file, "{line}");
        }
    });
    let relay_url = format!("moqt://{relay_address}");
    let log = std::fs::File::create(temp.path().join("host.log")).unwrap();
    let host = Process(
        teleport()
            .args(["host", "--source", "test", "--encoder", "software"])
            .args(["--listen", &host_address, "--width", "640"])
            .args(host_args)
            .arg("--identity-dir")
            .arg(temp.path().join("identity"))
            .args(["--relay-key", RELAY_KEY])
            .args([
                "--relay",
                &relay_url,
                "--relay-fingerprint",
                &relay_fingerprint,
            ])
            .stdout(log.try_clone().unwrap())
            .stderr(log)
            .spawn()
            .unwrap(),
    );
    let deadline = Instant::now() + Duration::from_secs(20);
    while !std::fs::read_to_string(temp.path().join("host.log"))
        .unwrap_or_default()
        .contains("connected to relay")
    {
        assert!(Instant::now() < deadline, "host did not connect to relay");
        std::thread::sleep(Duration::from_millis(50));
    }
    Setup {
        _relay: relay,
        _host: host,
        relay_url,
        relay_fingerprint,
        temp,
        host_address,
    }
}

fn client(setup: &Setup, pairing: &std::path::Path, relay_fingerprint: &str) -> Output {
    client_with_key(setup, pairing, relay_fingerprint, Some(RELAY_KEY))
}

fn client_with_key(
    setup: &Setup,
    pairing: &std::path::Path,
    relay_fingerprint: &str,
    key: Option<&str>,
) -> Output {
    let mut command = teleport();
    command
        // The direct address is deliberately unreachable: only the relay works.
        .args(["client", "127.0.0.1:9", "--pairing-file"])
        .arg(pairing)
        .args([
            "--relay",
            &setup.relay_url,
            "--relay-fingerprint",
            relay_fingerprint,
        ])
        .args(["--headless-frames", "30", "--software-decoder"])
        .arg("--smoke-input-latency");
    if let Some(key) = key {
        command.args(["--relay-key", key]);
    }
    // Never inherit a developer's key into tests that expect its absence.
    command.env_remove("TELEPORT_RELAY_KEY").output().unwrap()
}

#[test]
#[ignore = "requires local UDP sockets and GStreamer plugins"]
fn relayed_session_streams_video_and_input_end_to_end() {
    let setup = setup();
    let output = client(
        &setup,
        &setup.temp.path().join("identity/pairing.json"),
        &setup.relay_fingerprint,
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success() && stdout.contains("SMOKE PASS"),
        "relayed session failed: {stdout}{}\nhost log:\n{}",
        String::from_utf8_lossy(&output.stderr),
        std::fs::read_to_string(setup.temp.path().join("host.log")).unwrap_or_default()
            + "\nrelay log:\n"
            + &std::fs::read_to_string(setup.temp.path().join("relay.log")).unwrap_or_default()
    );
    assert!(stdout.contains("end-to-end encrypted relay session established"));
    // Probe replies prove the client→host input path works through the bridge.
    assert!(
        !stdout.contains("probes=0 "),
        "no input round trips: {stdout}"
    );
}

#[test]
#[ignore = "requires local UDP sockets and GStreamer plugins"]
fn relayed_host_ignores_unknown_credentials() {
    let setup = setup();
    let real: serde_json::Value = serde_json::from_slice(
        &std::fs::read(setup.temp.path().join("identity/pairing.json")).unwrap(),
    )
    .unwrap();
    let forged = setup.temp.path().join("forged.json");
    std::fs::write(
        &forged,
        serde_json::to_vec(&serde_json::json!({
            "token": "f".repeat(64),
            "fingerprint": real["fingerprint"],
        }))
        .unwrap(),
    )
    .unwrap();
    let output = client(&setup, &forged, &setup.relay_fingerprint);
    assert!(!output.status.success(), "forged credential was accepted");
    let log = std::fs::read_to_string(setup.temp.path().join("host.log")).unwrap();
    assert!(
        log.contains("rejected unauthorized relayed client"),
        "{log}"
    );
}

#[test]
#[ignore = "requires local UDP sockets and GStreamer plugins"]
fn client_refuses_an_unpinned_relay() {
    let setup = setup();
    let output = client(
        &setup,
        &setup.temp.path().join("identity/pairing.json"),
        &"0".repeat(64),
    );
    assert!(!output.status.success(), "connected to an unpinned relay");
}

#[test]
#[ignore = "requires local UDP sockets and GStreamer plugins"]
fn relay_refuses_clients_without_its_key() {
    let setup = setup();
    let pairing = setup.temp.path().join("identity/pairing.json");
    for key in [None, Some("wrong key")] {
        let output = client_with_key(&setup, &pairing, &setup.relay_fingerprint, key);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(!output.status.success(), "relay admitted key {key:?}");
        assert!(
            stderr.contains("check --relay-key"),
            "failed for another reason: {stderr}"
        );
    }
}

#[test]
#[ignore = "requires local UDP sockets and GStreamer plugins"]
fn relayed_session_works_over_ietf_moq_transport_like_provider_relays() {
    // Cloudflare's relay speaks IETF drafts 14/16 only. No datagrams there:
    // pointer motion must fall back to the ordered input track.
    for version in ["moq-transport-16", "moq-transport-14"] {
        let setup = setup_with_versions(&[version]);
        let output = client(
            &setup,
            &setup.temp.path().join("identity/pairing.json"),
            &setup.relay_fingerprint,
        );
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            output.status.success() && stdout.contains("SMOKE PASS"),
            "{version}: {stdout}{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            !stdout.contains("probes=0 "),
            "{version}: no input: {stdout}"
        );
    }
}

/// Enroll a device by code pairing through the host's admin socket, as a
/// current client does (it registers a device key); returns the pairing file.
fn enroll(setup: &Setup) -> std::path::PathBuf {
    use std::io::{Read, Write};
    let identity = setup.temp.path().join("identity");
    let mut socket = std::os::unix::net::UnixStream::connect(identity.join("admin.sock")).unwrap();
    socket
        .set_read_timeout(Some(Duration::from_secs(20)))
        .unwrap();
    let request = serde_json::to_vec(&serde_json::json!("OpenPairing")).unwrap();
    socket
        .write_all(&(request.len() as u32).to_be_bytes())
        .unwrap();
    socket.write_all(&request).unwrap();
    let mut length = [0; 4];
    socket.read_exact(&mut length).unwrap();
    let mut response = vec![0; u32::from_be_bytes(length) as usize];
    socket.read_exact(&mut response).unwrap();
    let response: serde_json::Value = serde_json::from_slice(&response).unwrap();
    let code = response["code"].as_str().expect("pairing code").to_owned();
    let config = setup.temp.path().join("config");
    let mut child = teleport()
        .args(["pair", &setup.host_address])
        .env("XDG_CONFIG_HOME", &config)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    writeln!(child.stdin.take().unwrap(), "{code}").unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "code enrollment failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let profiles: serde_json::Value =
        serde_json::from_slice(&std::fs::read(config.join("teleport/profiles.json")).unwrap())
            .unwrap();
    std::path::PathBuf::from(profiles[0]["pairing_file"].as_str().unwrap())
}

/// Run a client that the host must refuse, until the host logs `needle`.
fn refused(setup: &Setup, pairing: &std::path::Path, needle: &str) {
    let log_path = setup.temp.path().join("host.log");
    let before = std::fs::read_to_string(&log_path).unwrap_or_default().len();
    let mut child = Process(
        teleport()
            .args(["client", "127.0.0.1:9", "--pairing-file"])
            .arg(pairing)
            .args([
                "--relay",
                &setup.relay_url,
                "--relay-fingerprint",
                &setup.relay_fingerprint,
                "--relay-key",
                RELAY_KEY,
            ])
            .args(["--headless-frames", "30", "--software-decoder"])
            .env_remove("TELEPORT_RELAY_KEY")
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let deadline = Instant::now() + Duration::from_secs(25);
    loop {
        let log = std::fs::read_to_string(&log_path).unwrap_or_default();
        if log[before..].contains(needle) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "host never logged {needle:?}:\n{}",
            &log[before..]
        );
        std::thread::sleep(Duration::from_millis(100));
    }
    std::thread::sleep(Duration::from_millis(500));
    let _ = child.0.kill();
    let mut stdout = String::new();
    if let Some(mut pipe) = child.0.stdout.take() {
        use std::io::Read;
        let _ = pipe.read_to_string(&mut stdout);
    }
    assert!(
        !stdout.contains("SMOKE PASS"),
        "refused client streamed anyway"
    );
}

#[test]
#[ignore = "requires local UDP/TCP sockets and GStreamer plugins"]
fn relay_requires_device_keys_when_asked() {
    let setup = setup_with(&[], &["--relay-require-device-key"]);
    // A code-paired device registers a key and streams through the relay.
    let keyed = enroll(&setup);
    let credential: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&keyed).unwrap()).unwrap();
    let seed = credential["device_key"]
        .as_str()
        .expect("no device key saved");
    let access = std::fs::read_to_string(setup.temp.path().join("identity/access.json")).unwrap();
    assert!(
        access.contains("public_key") && !access.contains(seed),
        "host must store the public key only"
    );
    let output = client(&setup, &keyed, &setup.relay_fingerprint);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success() && stdout.contains("SMOKE PASS"),
        "keyed relayed session failed: {stdout}{}\nhost log:\n{}",
        String::from_utf8_lossy(&output.stderr),
        std::fs::read_to_string(setup.temp.path().join("host.log")).unwrap_or_default()
    );
    // The same credential without its device key, which is all the host's
    // records allow: the handshake is answered but never accepted.
    let stripped = setup.temp.path().join("stripped.json");
    std::fs::write(
        &stripped,
        serde_json::to_vec(&serde_json::json!({
            "token": credential["token"],
            "fingerprint": credential["fingerprint"],
        }))
        .unwrap(),
    )
    .unwrap();
    refused(&setup, &stripped, "device did not sign the session");
    // The host's own pairing token is hash-only: refused outright.
    refused(
        &setup,
        &setup.temp.path().join("identity/pairing.json"),
        "refused relayed credential without a device key",
    );
}
