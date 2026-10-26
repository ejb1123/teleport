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
    let relay_args: Vec<&str> = versions
        .iter()
        .flat_map(|version| ["--version", version])
        .collect();
    setup_relay(&relay_args, host_args)
}

/// A relay that behaves like Cloudflare's: moq-transport only, and no
/// namespace discovery, so peers announce to each other (src/discovery.rs).
const PROVIDER: &[&str] = &["--version", "moq-transport-14", "--no-namespace-discovery"];

fn setup_relay(relay_args: &[&str], host_args: &[&str]) -> Setup {
    let temp = tempfile::tempdir().unwrap();
    let relay_address = free_udp();
    let mut relay = Process(
        teleport()
            .args(["relay", "--listen", &relay_address, "--key", RELAY_KEY])
            .args(relay_args)
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
    let host_address = free_udp();
    let log = std::fs::File::create(temp.path().join("host.log")).unwrap();
    let host = Process(
        teleport()
            .args(["host", "--source", "test", "--encoder", "software"])
            .args(["--listen", &host_address, "--width", "640"])
            // Peer-to-peer stays on loopback, where it has no usable
            // candidate: signalling runs, then sessions use the relay.
            // tests/p2p.rs covers hole punching between NATed networks.
            .args(["--p2p-listen", "127.0.0.1:0"])
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

fn host_log(setup: &Setup) -> String {
    std::fs::read_to_string(setup.temp.path().join("host.log")).unwrap_or_default()
}

/// The pairing link a host started with `--relay --pair` prints.
fn relay_pairing_link(setup: &Setup) -> String {
    let deadline = Instant::now() + Duration::from_secs(20);
    while !host_log(setup).contains("relay pairing open") {
        assert!(
            Instant::now() < deadline,
            "host did not open relay pairing:\n{}",
            host_log(setup)
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    host_log(setup)
        .lines()
        .find_map(|line| line.strip_prefix("Relay pairing link: "))
        .and_then(|rest| rest.split_whitespace().next())
        .expect("host did not print a relay pairing link")
        .to_owned()
}

/// `teleport pair` through the relay only: the direct address is
/// unreachable and only names the saved profile.
fn pair_via_relay(setup: &Setup, config: &std::path::Path, link: &str) -> Output {
    use std::io::Write;
    let mut child = teleport()
        .args(["pair", "127.0.0.1:9", "--relay", &setup.relay_url])
        .args(["--relay-fingerprint", &setup.relay_fingerprint])
        .args(["--relay-key", RELAY_KEY])
        .env("XDG_CONFIG_HOME", config)
        .env_remove("TELEPORT_RELAY_KEY")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    writeln!(child.stdin.take().unwrap(), "{link}").unwrap();
    child.wait_with_output().unwrap()
}

/// Launched as the launcher does for a saved desktop with relay settings:
/// relay URL and key in the environment, `--route` on the command line.
fn auto_client(setup: &Setup, address: &str) -> Output {
    teleport()
        .args(["client", address, "--pairing-file"])
        .arg(setup.temp.path().join("identity/pairing.json"))
        .args(["--route", "auto"])
        .args(["--relay-fingerprint", &setup.relay_fingerprint])
        .args(["--headless-frames", "30", "--software-decoder"])
        .env("TELEPORT_RELAY_URL", &setup.relay_url)
        .env("TELEPORT_RELAY_KEY", RELAY_KEY)
        .output()
        .unwrap()
}

fn host_fingerprint(setup: &Setup) -> String {
    let pairing: serde_json::Value = serde_json::from_slice(
        &std::fs::read(setup.temp.path().join("identity/pairing.json")).unwrap(),
    )
    .unwrap();
    pairing["fingerprint"].as_str().unwrap().to_owned()
}

fn presence(setup: &Setup, fingerprint: &str) -> String {
    let output = teleport()
        .args(["presence", "--relay", &setup.relay_url])
        .args(["--relay-fingerprint", &setup.relay_fingerprint])
        .args(["--relay-key", RELAY_KEY])
        .args(["--host-fingerprint", fingerprint, "--wait", "15"])
        .env_remove("TELEPORT_RELAY_URL")
        .env_remove("TELEPORT_RELAY_KEY")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "presence check failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    // Logs share stdout; the verdict is a line of its own.
    let stdout = String::from_utf8_lossy(&output.stdout);
    stdout
        .lines()
        .map(str::trim)
        .find(|line| ["ONLINE", "OFFLINE", "UNKNOWN"].contains(line))
        .unwrap_or_else(|| panic!("no presence verdict: {stdout}"))
        .to_owned()
}

#[test]
#[ignore = "requires local UDP sockets and GStreamer plugins"]
fn first_pairing_and_session_work_through_the_relay_alone() {
    first_pairing_and_session(&[]);
}

#[test]
#[ignore = "requires local UDP sockets and GStreamer plugins"]
fn first_pairing_and_session_work_without_namespace_discovery() {
    first_pairing_and_session(PROVIDER);
}

fn first_pairing_and_session(relay: &[&str]) {
    let setup = setup_relay(relay, &["--pair"]);
    let link = relay_pairing_link(&setup);
    let (prefix, code) = link.rsplit_once('/').unwrap();
    let config = setup.temp.path().join("config");
    // A wrong code reveals nothing and saves nothing.
    let wrong = if code == "000-000" {
        "000-001"
    } else {
        "000-000"
    };
    let output = pair_via_relay(&setup, &config, &format!("{prefix}/{wrong}"));
    assert!(
        !output.status.success(),
        "wrong relay pairing code accepted"
    );
    assert!(!config.join("teleport/profiles.json").exists());
    // The right link pairs, purely through the relay.
    let output = pair_via_relay(&setup, &config, &link);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "relay pairing failed: {stdout}{}\nhost log:\n{}",
        String::from_utf8_lossy(&output.stderr),
        host_log(&setup)
    );
    assert!(!stdout.contains(code), "client echoed the pairing code");
    let profiles: serde_json::Value =
        serde_json::from_slice(&std::fs::read(config.join("teleport/profiles.json")).unwrap())
            .unwrap();
    let credential = std::path::PathBuf::from(profiles[0]["pairing_file"].as_str().unwrap());
    let issued: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&credential).unwrap()).unwrap();
    let host: serde_json::Value = serde_json::from_slice(
        &std::fs::read(setup.temp.path().join("identity/pairing.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(issued["fingerprint"], host["fingerprint"]);
    assert_ne!(
        issued["token"], host["token"],
        "expected a per-device credential"
    );
    // The code is single use.
    let output = pair_via_relay(&setup, &setup.temp.path().join("again"), &link);
    assert!(!output.status.success(), "relay pairing code reused");
    // The saved credential opens a normal relayed session.
    let output = client(&setup, &credential, &setup.relay_fingerprint);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success() && stdout.contains("SMOKE PASS"),
        "relayed session after relay pairing failed: {stdout}{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(stdout.contains("end-to-end encrypted relay session established"));
}

#[test]
#[ignore = "requires local UDP sockets and GStreamer plugins"]
fn auto_route_falls_back_to_the_relay_and_presence_follows_the_host() {
    auto_route_and_presence(&[]);
}

/// Peer-to-peer signalling, sessions and presence where the relay announces
/// nothing, as on Cloudflare's.
#[test]
#[ignore = "requires local UDP sockets and GStreamer plugins"]
fn auto_route_and_presence_work_without_namespace_discovery() {
    auto_route_and_presence(PROVIDER);
}

fn auto_route_and_presence(relay: &[&str]) {
    let mut setup = setup_relay(relay, &[]);
    let fingerprint = host_fingerprint(&setup);
    assert_eq!(presence(&setup, &fingerprint), "ONLINE");
    // Nothing vouches for another host identity in this namespace.
    assert_ne!(presence(&setup, &"ab".repeat(32)), "ONLINE");

    // The direct address answers nothing: auto must choose the relay.
    let started = Instant::now();
    let output = auto_client(&setup, "127.0.0.1:9");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success() && stdout.contains("SMOKE PASS"),
        "auto route failed: {stdout}{}\nhost log:\n{}",
        String::from_utf8_lossy(&output.stderr),
        std::fs::read_to_string(setup.temp.path().join("host.log")).unwrap_or_default()
    );
    assert!(
        stdout.contains("direct path unavailable; trying the relay"),
        "{stdout}"
    );
    // The host answered peer-to-peer signalling, with nothing usable.
    assert!(
        stdout.contains(
            "peer-to-peer path unavailable error=\"peer-to-peer path unavailable: the host offered no usable peer-to-peer candidates\""
        ),
        "{stdout}"
    );
    assert!(stdout.contains("connection path chosen: relay"), "{stdout}");
    assert!(stdout.contains("end-to-end encrypted relay session established"));
    assert!(started.elapsed() < Duration::from_secs(40));

    // A reachable host is never reached through the relay. The direct
    // address and peer-to-peer are tried at once, and on loopback either may
    // connect first; both are direct UDP paths.
    let output = auto_client(&setup, &setup.host_address);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success() && stdout.contains("SMOKE PASS"),
        "auto route (direct) failed: {stdout}{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        stdout.contains("connection path chosen: direct")
            || stdout.contains("connection path chosen: p2p"),
        "{stdout}"
    );
    assert!(!stdout.contains("relay session established"), "{stdout}");

    // Stop the host: the relay notices the lost connection and the host is
    // reported offline.
    setup._host.0.kill().unwrap();
    setup._host.0.wait().unwrap();
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let state = presence(&setup, &fingerprint);
        if state == "OFFLINE" {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "host still reported {state} after it stopped"
        );
        std::thread::sleep(Duration::from_secs(1));
    }
}

/// One request on the host's admin socket (docs/guests.md).
fn admin(setup: &Setup, request: serde_json::Value) -> serde_json::Value {
    use std::io::{Read, Write};
    let identity = setup.temp.path().join("identity");
    let mut socket = std::os::unix::net::UnixStream::connect(identity.join("admin.sock")).unwrap();
    socket
        .set_read_timeout(Some(Duration::from_secs(20)))
        .unwrap();
    let bytes = serde_json::to_vec(&request).unwrap();
    socket
        .write_all(&(bytes.len() as u32).to_be_bytes())
        .unwrap();
    socket.write_all(&bytes).unwrap();
    let mut length = [0; 4];
    socket.read_exact(&mut length).unwrap();
    let mut response = vec![0; u32::from_be_bytes(length) as usize];
    socket.read_exact(&mut response).unwrap();
    let response: serde_json::Value = serde_json::from_slice(&response).unwrap();
    assert!(response["error"].is_null(), "admin {request}: {response}");
    response
}

/// A relayed headless client writing to `log`.
fn relayed_client(
    setup: &Setup,
    pairing: &std::path::Path,
    frames: &str,
    args: &[&str],
    log: &std::path::Path,
) -> Process {
    let file = std::fs::File::create(log).unwrap();
    Process(
        teleport()
            .args(["client", "127.0.0.1:9", "--pairing-file"])
            .arg(pairing)
            .args(["--relay", &setup.relay_url])
            .args(["--relay-fingerprint", &setup.relay_fingerprint])
            .args(["--relay-key", RELAY_KEY])
            .args(["--headless-frames", frames, "--software-decoder"])
            .args(args)
            .env_remove("TELEPORT_RELAY_KEY")
            .stdout(file.try_clone().unwrap())
            .stderr(file)
            .spawn()
            .unwrap(),
    )
}

/// G15/G16 through the relay: a guest enrolls with an invite link through
/// the relay alone, waits for approval, shares the owner's stream and gets
/// only what it was granted, exactly as on a direct connection.
#[test]
#[ignore = "requires local UDP sockets and GStreamer plugins"]
fn guest_joins_through_the_relay_with_approval_and_permissions() {
    guest_joins(&[]);
}

#[test]
#[ignore = "requires local UDP sockets and GStreamer plugins"]
fn guest_joins_without_namespace_discovery() {
    guest_joins(PROVIDER);
}

fn guest_joins(relay: &[&str]) {
    let setup = setup_relay(relay, &[]);
    let wait_log = |needles: &[&str]| {
        let deadline = Instant::now() + Duration::from_secs(30);
        while !host_log(&setup)
            .lines()
            .any(|line| needles.iter().all(|needle| line.contains(needle)))
        {
            assert!(
                Instant::now() < deadline,
                "host did not log {needles:?}:\n{}",
                host_log(&setup)
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    };
    let invite = admin(
        &setup,
        serde_json::json!({ "CreateInvite": {
            "name": "Remote", "uses": 1, "seconds": 600,
            "permissions": { "gamepad": true }
        }}),
    )["invite"]
        .as_str()
        .unwrap()
        .to_owned();
    // The link names the host's relay id: enrollment needs no reachable port.
    let relay_host = admin(&setup, serde_json::json!("Status"))["relay_host"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(invite.ends_with(&relay_host), "{invite}");
    let config = setup.temp.path().join("guest-config");
    let output = pair_via_relay(&setup, &config, &invite);
    assert!(
        output.status.success(),
        "invite through the relay failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let profiles: serde_json::Value =
        serde_json::from_slice(&std::fs::read(config.join("teleport/profiles.json")).unwrap())
            .unwrap();
    let guest = std::path::PathBuf::from(profiles[0]["pairing_file"].as_str().unwrap());
    // Used up: the same link again gets nothing.
    let again = pair_via_relay(&setup, &setup.temp.path().join("again"), &invite);
    assert!(!again.status.success());
    let owner_log = setup.temp.path().join("owner.log");
    let mut owner = relayed_client(
        &setup,
        &setup.temp.path().join("identity/pairing.json"),
        "900",
        &[],
        &owner_log,
    );
    wait_log(&["session started the desktop"]);
    let guest_log = setup.temp.path().join("guest.log");
    let mut guest_client =
        relayed_client(&setup, &guest, "60", &["--smoke-guest-input"], &guest_log);
    wait_log(&["guest asks to join", "Remote"]);
    let deadline = Instant::now() + Duration::from_secs(20);
    let id = loop {
        if let Some(id) =
            admin(&setup, serde_json::json!("Sessions"))["approvals"][0]["id"].as_u64()
        {
            break id;
        }
        assert!(Instant::now() < deadline, "no guest waiting");
        std::thread::sleep(Duration::from_millis(50));
    };
    admin(
        &setup,
        serde_json::json!({ "AnswerGuest": { "id": id, "allow": true } }),
    );
    let deadline = Instant::now() + Duration::from_secs(90);
    let status = loop {
        if let Some(status) = guest_client.0.try_wait().unwrap() {
            break status;
        }
        assert!(Instant::now() < deadline, "guest did not finish");
        std::thread::sleep(Duration::from_millis(50));
    };
    let log = std::fs::read_to_string(&guest_log).unwrap();
    assert!(
        status.success() && log.contains("SMOKE PASS"),
        "relayed guest failed: {log}\nhost log:\n{}",
        host_log(&setup)
    );
    assert!(log.contains("end-to-end encrypted relay session established"));
    let sessions = admin(&setup, serde_json::json!("Sessions"));
    assert!(
        sessions["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .all(|session| session["transport"] == "relay"),
        "{sessions}"
    );
    wait_log(&["refused input from session", "Remote", "keyboard and mouse"]);
    wait_log(&["gamepad connected", "pad=4"]);
    assert!(owner.0.try_wait().unwrap().is_none(), "owner session ended");
}

/// Live, through Cloudflare's public relay (draft-14, no account needed):
/// pairing through the relay, an end-to-end encrypted session, and presence.
/// Opt in with `TELEPORT_TEST_CLOUDFLARE=1`; `TELEPORT_TEST_CLOUDFLARE_URL`
/// picks another relay (e.g. a provisioned draft-16 relay, its token in the
/// path). The host shows the synthetic test source only.
#[test]
#[ignore = "uses the internet: set TELEPORT_TEST_CLOUDFLARE=1"]
fn cloudflare_relay_pairs_streams_and_reports_presence() {
    if std::env::var_os("TELEPORT_TEST_CLOUDFLARE").is_none() {
        eprintln!("skipped: set TELEPORT_TEST_CLOUDFLARE=1 to test through Cloudflare");
        return;
    }
    let url = std::env::var("TELEPORT_TEST_CLOUDFLARE_URL")
        .unwrap_or_else(|_| "https://draft-14.cloudflare.mediaoverquic.com/".into());
    // A fresh identity is a fresh namespace on the shared relay.
    let temp = tempfile::tempdir().unwrap();
    let host_log = temp.path().join("host.log");
    let log = std::fs::File::create(&host_log).unwrap();
    let mut host = Process(
        teleport()
            .args(["host", "--source", "test", "--encoder", "software"])
            .args(["--listen", &free_udp(), "--width", "640", "--pair"])
            .arg("--identity-dir")
            .arg(temp.path().join("identity"))
            .args(["--relay", &url])
            .env_remove("TELEPORT_RELAY_KEY")
            .stdout(log.try_clone().unwrap())
            .stderr(log)
            .spawn()
            .unwrap(),
    );
    let read_log = || std::fs::read_to_string(&host_log).unwrap_or_default();
    let deadline = Instant::now() + Duration::from_secs(30);
    while !read_log().contains("relay pairing open") {
        assert!(
            Instant::now() < deadline,
            "no relay pairing:\n{}",
            read_log()
        );
        std::thread::sleep(Duration::from_millis(100));
    }
    let link = read_log()
        .lines()
        .find_map(|line| line.strip_prefix("Relay pairing link: "))
        .and_then(|rest| rest.split_whitespace().next())
        .expect("no relay pairing link")
        .to_owned();
    let config = temp.path().join("config");
    let mut pair = teleport()
        .args(["pair", "127.0.0.1:9", "--relay", &url])
        .env("XDG_CONFIG_HOME", &config)
        .env_remove("TELEPORT_RELAY_KEY")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    {
        use std::io::Write;
        writeln!(pair.stdin.take().unwrap(), "{link}").unwrap();
    }
    let output = pair.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "pairing through Cloudflare failed: {}\nhost log:\n{}",
        String::from_utf8_lossy(&output.stderr),
        read_log()
    );
    let profiles: serde_json::Value =
        serde_json::from_slice(&std::fs::read(config.join("teleport/profiles.json")).unwrap())
            .unwrap();
    let credential = profiles[0]["pairing_file"].as_str().unwrap().to_owned();
    let output = teleport()
        .args(["client", "127.0.0.1:9", "--pairing-file", &credential])
        .args(["--relay", &url, "--route", "relay"])
        .args(["--headless-frames", "60", "--software-decoder"])
        .arg("--smoke-input-latency")
        .env_remove("TELEPORT_RELAY_KEY")
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success() && stdout.contains("SMOKE PASS"),
        "session through Cloudflare failed: {stdout}{}\nhost log:\n{}",
        String::from_utf8_lossy(&output.stderr),
        read_log()
    );
    assert!(stdout.contains("end-to-end encrypted relay session established"));
    assert!(
        !stdout.contains("probes=0 "),
        "no input round trips: {stdout}"
    );
    let pairing: serde_json::Value =
        serde_json::from_slice(&std::fs::read(temp.path().join("identity/pairing.json")).unwrap())
            .unwrap();
    let fingerprint = pairing["fingerprint"].as_str().unwrap().to_owned();
    let presence = || {
        let output = teleport()
            .args(["presence", "--relay", &url])
            .args(["--host-fingerprint", &fingerprint, "--wait", "15"])
            .env_remove("TELEPORT_RELAY_URL")
            .env_remove("TELEPORT_RELAY_KEY")
            .output()
            .unwrap();
        String::from_utf8_lossy(&output.stdout)
            .lines()
            .map(str::trim)
            .find(|line| ["ONLINE", "OFFLINE", "UNKNOWN"].contains(line))
            .unwrap_or("none")
            .to_owned()
    };
    assert_eq!(presence(), "ONLINE");
    host.0.kill().unwrap();
    host.0.wait().unwrap();
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let state = presence();
        if state == "OFFLINE" {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "still {state} after the host stopped"
        );
        std::thread::sleep(Duration::from_secs(2));
    }
}

/// A self-signed certificate and key as PEM, and its SHA-256 fingerprint.
fn pem_identity() -> (String, String, String) {
    let certified = rcgen::generate_simple_self_signed(vec!["relay.test".into()]).unwrap();
    let der = certified.cert.der().to_vec();
    let fingerprint = ring::digest::digest(&ring::digest::SHA256, &der)
        .as_ref()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    let pem = |label: &str, der: &[u8]| {
        use base64::Engine;
        let text = base64::engine::general_purpose::STANDARD.encode(der);
        let lines: Vec<&str> = text
            .as_bytes()
            .chunks(64)
            .map(|line| std::str::from_utf8(line).unwrap())
            .collect();
        format!(
            "-----BEGIN {label}-----\n{}\n-----END {label}-----\n",
            lines.join("\n")
        )
    };
    (
        pem("CERTIFICATE", &der),
        pem("PRIVATE KEY", &certified.signing_key.serialize_der()),
        fingerprint,
    )
}

/// What `teleport presence` says through a relay pinned at `pin`: OFFLINE
/// when the relay answered with that certificate (no host is there),
/// UNKNOWN when the pin did not match.
fn pinned_verdict(relay_url: &str, pin: &str) -> String {
    let output = teleport()
        .args(["presence", "--relay", relay_url, "--relay-fingerprint", pin])
        .args([
            "--relay-key",
            RELAY_KEY,
            "--host-fingerprint",
            &"ab".repeat(32),
        ])
        .args(["--wait", "10"])
        .env_remove("TELEPORT_RELAY_URL")
        .env_remove("TELEPORT_RELAY_KEY")
        .output()
        .unwrap();
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::trim)
        .find(|line| ["ONLINE", "OFFLINE", "UNKNOWN"].contains(line))
        .unwrap_or("NONE")
        .to_owned()
}

/// `teleport relay --tls-cert --tls-key` serves the given certificate (as a
/// CA-issued one for browsers would be), prints its fingerprint, and picks
/// up a renewed certificate written over the same files without a restart.
#[test]
#[ignore = "requires local UDP sockets"]
fn relay_serves_and_reloads_a_certificate_from_files() {
    let temp = tempfile::tempdir().unwrap();
    let (cert, key) = (temp.path().join("cert.pem"), temp.path().join("key.pem"));
    let (cert_a, key_a, pin_a) = pem_identity();
    let (cert_b, key_b, pin_b) = pem_identity();
    std::fs::write(&cert, &cert_a).unwrap();
    std::fs::write(&key, &key_a).unwrap();
    let address = free_udp();
    let mut relay = Process(
        teleport()
            .args([
                "relay",
                "--listen",
                &address,
                "--key",
                RELAY_KEY,
                "--stun-listen",
                "off",
            ])
            .arg("--tls-cert")
            .arg(&cert)
            .arg("--tls-key")
            .arg(&key)
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let printed = BufReader::new(relay.0.stdout.take().unwrap())
        .lines()
        .map_while(Result::ok)
        .find_map(|line| line.strip_prefix("RELAY FINGERPRINT ").map(str::to_owned))
        .expect("relay did not print its fingerprint");
    assert_eq!(printed, pin_a);
    let url = format!("moqt://{address}");
    assert_eq!(pinned_verdict(&url, &pin_a), "OFFLINE");
    assert_eq!(pinned_verdict(&url, &pin_b), "UNKNOWN");

    // Renewal: the key first, then the certificate, each by rename, as
    // certificate managers write them.
    for (path, contents) in [(&key, &key_b), (&cert, &cert_b)] {
        let staged = path.with_extension("new");
        std::fs::write(&staged, contents).unwrap();
        std::fs::rename(&staged, path).unwrap();
    }
    let deadline = Instant::now() + Duration::from_secs(20);
    while pinned_verdict(&url, &pin_b) != "OFFLINE" {
        assert!(
            Instant::now() < deadline,
            "the relay never served the renewed certificate"
        );
        std::thread::sleep(Duration::from_millis(200));
    }
    assert_eq!(pinned_verdict(&url, &pin_a), "UNKNOWN");
}

/// `teleport ticket <command> <file>`: the public key it prints.
fn ticket_keygen(command: &str, path: &std::path::Path) -> String {
    let output = teleport()
        .args(["ticket", command])
        .arg(path)
        .output()
        .unwrap();
    assert!(output.status.success(), "{command} failed");
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

fn sign_ticket(issuer_seed: &std::path::Path, fingerprint: &str, client_key: &str) -> String {
    let output = teleport()
        .args(["ticket", "sign", "--issuer-key-file"])
        .arg(issuer_seed)
        .args([
            "--host-fingerprint",
            fingerprint,
            "--client-key",
            client_key,
        ])
        .args(["--name", "Support (Integration)", "--session-id", "it-1"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "ticket sign failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

/// `teleport login-ticket` through the relay only, saving the credential.
fn redeem_ticket(setup: &Setup, ticket: &str, key: &std::path::Path, name: &str) -> Output {
    let ticket_file = setup.temp.path().join(format!("{name}.ticket"));
    std::fs::write(&ticket_file, ticket).unwrap();
    teleport()
        .args([
            "login-ticket",
            "127.0.0.1:9",
            "--fingerprint",
            &host_fingerprint(setup),
        ])
        .arg("--ticket-file")
        .arg(&ticket_file)
        .arg("--key-file")
        .arg(key)
        .arg("--save-credential")
        .arg(setup.temp.path().join(format!("{name}.json")))
        .args(["--relay", &setup.relay_url])
        .args(["--relay-fingerprint", &setup.relay_fingerprint])
        .args(["--relay-key", RELAY_KEY])
        .env_remove("TELEPORT_RELAY_KEY")
        .output()
        .unwrap()
}

/// A host that trusts an issuer opens one relayed, end-to-end encrypted
/// guest session for a ticket that issuer signed, with no pairing and no
/// approval prompt (`--ticket-approval issuer`); the same ticket is refused a
/// second time, and so is a ticket from any other issuer.
#[test]
#[ignore = "requires local UDP sockets and GStreamer plugins"]
fn issuer_signed_ticket_opens_one_relayed_session() {
    let keys = tempfile::tempdir().unwrap();
    let issuer = ticket_keygen("issuer-keygen", &keys.path().join("issuer"));
    let client_key = ticket_keygen("client-keygen", &keys.path().join("client"));
    ticket_keygen("issuer-keygen", &keys.path().join("stranger"));
    let setup = setup_with(
        &[],
        &[
            "--ticket-issuer-key",
            &issuer,
            "--ticket-approval",
            "issuer",
        ],
    );
    let fingerprint = host_fingerprint(&setup);
    let ticket = sign_ticket(&keys.path().join("issuer"), &fingerprint, &client_key);

    let redeemed = redeem_ticket(&setup, &ticket, &keys.path().join("client"), "first");
    assert!(
        redeemed.status.success(),
        "ticket redemption failed: {}\nhost log:\n{}",
        String::from_utf8_lossy(&redeemed.stderr),
        host_log(&setup)
    );
    let output = client(
        &setup,
        &setup.temp.path().join("first.json"),
        &setup.relay_fingerprint,
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success() && stdout.contains("SMOKE PASS"),
        "ticket session failed: {stdout}{}\nhost log:\n{}",
        String::from_utf8_lossy(&output.stderr),
        host_log(&setup)
    );
    assert!(stdout.contains("end-to-end encrypted relay session established"));
    let log = host_log(&setup);
    assert!(log.contains("session ticket redeemed"), "{log}");
    assert!(!log.contains("waiting for approval"), "{log}");

    // The credential was for one session: it is spent.
    let again = client(
        &setup,
        &setup.temp.path().join("first.json"),
        &setup.relay_fingerprint,
    );
    assert!(!String::from_utf8_lossy(&again.stdout).contains("SMOKE PASS"));
    // The ticket itself was for one redemption.
    assert!(
        !redeem_ticket(&setup, &ticket, &keys.path().join("client"), "replay")
            .status
            .success()
    );
    // A ticket from an issuer the host does not trust.
    let foreign = sign_ticket(&keys.path().join("stranger"), &fingerprint, &client_key);
    assert!(
        !redeem_ticket(&setup, &foreign, &keys.path().join("client"), "foreign")
            .status
            .success()
    );
    // A good ticket presented with another client's key.
    ticket_keygen("client-keygen", &keys.path().join("thief"));
    let fresh = sign_ticket(&keys.path().join("issuer"), &fingerprint, &client_key);
    assert!(
        !redeem_ticket(&setup, &fresh, &keys.path().join("thief"), "thief")
            .status
            .success()
    );
    assert!(
        redeem_ticket(&setup, &fresh, &keys.path().join("client"), "owner")
            .status
            .success()
    );
}

/// A one-shot issuer on loopback, as a web site's redeem URL behaves: the
/// first POST gets a ticket for the key it carries, every later one 410.
fn issuer_site(
    issuer_seed: std::path::PathBuf,
    fingerprint: String,
    relay_url: String,
    relay_fingerprint: String,
) -> String {
    use std::io::{Read, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!(
        "http://{}/redeem/one-time-secret",
        listener.local_addr().unwrap()
    );
    std::thread::spawn(move || {
        let mut spent = false;
        for stream in listener.incoming() {
            let mut stream = stream.unwrap();
            let mut request = Vec::new();
            let mut buffer = [0; 4096];
            let body = loop {
                let n = stream.read(&mut buffer).unwrap();
                request.extend_from_slice(&buffer[..n]);
                if let Some(split) = request.windows(4).position(|w| w == b"\r\n\r\n") {
                    let head = String::from_utf8_lossy(&request[..split]).to_lowercase();
                    let length: usize = head
                        .lines()
                        .find_map(|l| l.strip_prefix("content-length:"))
                        .unwrap()
                        .trim()
                        .parse()
                        .unwrap();
                    if request.len() >= split + 4 + length {
                        break request[split + 4..split + 4 + length].to_vec();
                    }
                }
            };
            let (status, answer) = if spent {
                (
                    "410 Gone",
                    serde_json::json!({"error": "this session link is no longer valid"}),
                )
            } else {
                spent = true;
                let posted: serde_json::Value = serde_json::from_slice(&body).unwrap();
                let ticket = sign_ticket(
                    &issuer_seed,
                    &fingerprint,
                    posted["client_key"].as_str().unwrap(),
                );
                (
                    "200 OK",
                    serde_json::json!({
                        "ticket": ticket,
                        "name": "Integration box",
                        "host_fingerprint": fingerprint,
                        "relay": {"url": relay_url, "fingerprint": relay_fingerprint, "key": RELAY_KEY},
                    }),
                )
            };
            let answer = answer.to_string();
            let _ = write!(
                stream,
                "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{answer}",
                answer.len()
            );
        }
    });
    url
}

/// `teleport open` with a web page's link: the client makes its own key,
/// gets a ticket for it from the single-use URL, and opens the session
/// through the relay the issuer named. The link works once.
#[test]
#[ignore = "requires local UDP/TCP sockets and GStreamer plugins"]
fn a_redeem_link_opens_one_relayed_session() {
    let keys = tempfile::tempdir().unwrap();
    let issuer = ticket_keygen("issuer-keygen", &keys.path().join("issuer"));
    let setup = setup_with(
        &[],
        &[
            "--ticket-issuer-key",
            &issuer,
            "--ticket-approval",
            "issuer",
        ],
    );
    let url = issuer_site(
        keys.path().join("issuer"),
        host_fingerprint(&setup),
        setup.relay_url.clone(),
        setup.relay_fingerprint.clone(),
    );
    let link = format!(
        "teleport://redeem?url={}",
        url::form_urlencoded::byte_serialize(url.as_bytes()).collect::<String>()
    );
    let open = |name: &str| {
        teleport()
            .args(["open", &link, "--save-credential"])
            .arg(setup.temp.path().join(name))
            .env_remove("TELEPORT_RELAY_KEY")
            .output()
            .unwrap()
    };
    let first = open("link.json");
    assert!(
        first.status.success(),
        "open failed: {}{}\nhost log:\n{}",
        String::from_utf8_lossy(&first.stdout),
        String::from_utf8_lossy(&first.stderr),
        host_log(&setup)
    );
    assert!(String::from_utf8_lossy(&first.stdout).contains("Integration box"));
    let output = client(
        &setup,
        &setup.temp.path().join("link.json"),
        &setup.relay_fingerprint,
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success() && stdout.contains("SMOKE PASS"),
        "session from a link failed: {stdout}\nhost log:\n{}",
        host_log(&setup)
    );
    let again = open("again.json");
    assert!(!again.status.success());
    assert!(
        String::from_utf8_lossy(&again.stderr).contains("no longer valid"),
        "{}",
        String::from_utf8_lossy(&again.stderr)
    );
}
