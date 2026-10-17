#![cfg(target_os = "linux")]
//! Shared sessions (docs/guests.md) with real processes: a test host, its
//! owner's clients and guests enrolled through invites. Approval, what a
//! guest may inject, keyboard/mouse control, kick, revocation and invite
//! limits. The relayed guest is in tests/relay.rs.

use serde_json::{Value, json};
use std::{
    io::{Read, Write},
    path::{Path, PathBuf},
    process::{Child, Command, ExitStatus, Stdio},
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
    Command::new(
        std::env::var_os("TELEPORT_TEST_BINARY")
            .unwrap_or_else(|| env!("CARGO_BIN_EXE_teleport").into()),
    )
}

fn free_udp() -> String {
    let socket = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    socket.local_addr().unwrap().to_string()
}

/// A headless client whose output goes to a file.
struct Client {
    process: Process,
    log: PathBuf,
}

impl Client {
    fn log(&self) -> String {
        std::fs::read_to_string(&self.log).unwrap_or_default()
    }

    fn running(&mut self) -> bool {
        self.process.0.try_wait().unwrap().is_none()
    }

    fn wait(&mut self, timeout: Duration) -> (ExitStatus, String) {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(status) = self.process.0.try_wait().unwrap() {
                return (status, self.log());
            }
            assert!(
                Instant::now() < deadline,
                "client did not exit within {timeout:?}:\n{}",
                self.log()
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

struct Host {
    _process: Process,
    temp: tempfile::TempDir,
    directory: PathBuf,
    address: String,
}

impl Host {
    fn start(args: &[&str]) -> Self {
        let temp = tempfile::tempdir_in("/tmp").unwrap();
        let directory = temp.path().join("host");
        let address = free_udp();
        let log = std::fs::File::create(temp.path().join("host.log")).unwrap();
        let mut process = Process(
            teleport()
                .args(["host", "--source", "test", "--encoder", "software"])
                .args(["--width", "320", "--fps", "30", "--listen", &address])
                .args(args)
                .arg("--identity-dir")
                .arg(&directory)
                .env("RUST_LOG", "teleport=info")
                .stdout(log.try_clone().unwrap())
                .stderr(log)
                .spawn()
                .unwrap(),
        );
        let deadline = Instant::now() + Duration::from_secs(20);
        while !directory.join("admin.sock").exists() {
            assert!(process.0.try_wait().unwrap().is_none(), "test host exited");
            assert!(Instant::now() < deadline, "test host did not start");
            std::thread::sleep(Duration::from_millis(30));
        }
        Self {
            _process: process,
            temp,
            directory,
            address,
        }
    }

    fn log(&self) -> String {
        std::fs::read_to_string(self.temp.path().join("host.log")).unwrap_or_default()
    }

    /// Lines of the host log containing every one of `needles`.
    fn count(&self, needles: &[&str]) -> usize {
        self.log()
            .lines()
            .filter(|line| needles.iter().all(|needle| line.contains(needle)))
            .count()
    }

    /// Wait until the host log has more than `seen` lines with `needles`.
    fn wait_more(&self, needles: &[&str], seen: usize, timeout: Duration) {
        let deadline = Instant::now() + timeout;
        while self.count(needles) <= seen {
            assert!(
                Instant::now() < deadline,
                "host did not log {needles:?}:\n{}",
                self.log()
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    fn wait_log(&self, needles: &[&str]) {
        self.wait_more(needles, 0, Duration::from_secs(30));
    }

    fn admin_result(&self, request: Value) -> Value {
        let mut socket =
            std::os::unix::net::UnixStream::connect(self.directory.join("admin.sock")).unwrap();
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
        let mut bytes = vec![0; u32::from_be_bytes(length) as usize];
        socket.read_exact(&mut bytes).unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    fn admin(&self, request: Value) -> Value {
        let response = self.admin_result(request.clone());
        assert!(
            response["error"].is_null(),
            "admin {request} failed: {}",
            response["error"]
        );
        response
    }

    /// `teleport pair` with `secret` (a code or an invite link) on stdin;
    /// the saved credential's path, or the client's error output.
    fn pair(&self, config: &str, secret: &str) -> Result<PathBuf, String> {
        let config = self.temp.path().join(config);
        // Pairing bounds its own time (ten seconds per exchange).
        let mut child = teleport()
            .args(["pair", &self.address])
            .env("XDG_CONFIG_HOME", &config)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        writeln!(child.stdin.take().unwrap(), "{secret}").unwrap();
        let output = child.wait_with_output().unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(!stdout.contains(secret.trim()), "client echoed the secret");
        if !output.status.success() {
            return Err(String::from_utf8_lossy(&output.stderr).into_owned());
        }
        let profiles: Value =
            serde_json::from_slice(&std::fs::read(config.join("teleport/profiles.json")).unwrap())
                .unwrap();
        Ok(PathBuf::from(profiles[0]["pairing_file"].as_str().unwrap()))
    }

    /// An owner device (code pairing).
    fn owner(&self, config: &str) -> PathBuf {
        let response = self.admin(json!("OpenPairing"));
        self.pair(config, response["code"].as_str().unwrap())
            .unwrap()
    }

    fn invite(&self, name: &str, uses: u32, seconds: u64, permissions: Value) -> String {
        let response = self.admin(json!({ "CreateInvite": {
            "name": name, "uses": uses, "seconds": seconds, "permissions": permissions
        }}));
        response["invite"].as_str().unwrap().to_owned()
    }

    /// A guest device enrolled through a one-use invite.
    fn guest(&self, config: &str, name: &str, permissions: Value) -> PathBuf {
        let link = self.invite(name, 1, 600, permissions);
        self.pair(config, &link).unwrap()
    }

    fn device(&self, name: &str) -> String {
        self.admin(json!("Status"))["devices"]
            .as_array()
            .unwrap()
            .iter()
            .find(|device| device["name"] == name)
            .unwrap_or_else(|| panic!("no device {name}"))["id"]
            .as_str()
            .unwrap()
            .to_owned()
    }

    fn session(&self, name: &str) -> u64 {
        self.admin(json!("Sessions"))["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .find(|session| session["name"] == name)
            .unwrap_or_else(|| panic!("no session {name}"))["id"]
            .as_u64()
            .unwrap()
    }

    /// The oldest guest waiting for approval.
    fn waiting(&self) -> u64 {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            let response = self.admin(json!("Sessions"));
            if let Some(id) = response["approvals"][0]["id"].as_u64() {
                return id;
            }
            assert!(
                Instant::now() < deadline,
                "no guest waiting:\n{}",
                self.log()
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    fn client(&self, credential: &Path, frames: &str, args: &[&str]) -> Client {
        let log = self.temp.path().join(format!(
            "client-{}.log",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let file = std::fs::File::create(&log).unwrap();
        let process = Process(
            teleport()
                .args(["client", &self.address, "--pairing-file"])
                .arg(credential)
                .args(["--headless-frames", frames, "--software-decoder"])
                .args(args)
                .env("RUST_LOG", "teleport=info")
                .stdout(file.try_clone().unwrap())
                .stderr(file)
                .spawn()
                .unwrap(),
        );
        Client { process, log }
    }
}

fn passed(client: &mut Client) -> String {
    let (status, log) = client.wait(Duration::from_secs(90));
    assert!(
        status.success() && log.contains("SMOKE PASS"),
        "client failed: {log}"
    );
    log
}

#[test]
#[ignore = "requires local TCP/UDP sockets and GStreamer runtime plugins"]
fn owner_and_gamepad_guest_share_one_desktop() {
    let host = Host::start(&[]);
    let owner = host.owner("owner");
    let guest = host.guest("guest", "Couch", json!({ "gamepad": true }));
    // Guests are stored as revocable guest devices.
    let status = host.admin(json!("Status"));
    let device = status["devices"]
        .as_array()
        .unwrap()
        .iter()
        .find(|device| device["name"] == "Couch")
        .unwrap();
    assert_eq!(device["source"], "invite");
    assert_eq!(device["guest"]["permissions"]["gamepad"], true);
    assert_eq!(device["guest"]["permissions"]["keyboard_mouse"], false);
    let mut owner_client = host.client(&owner, "450", &[]);
    host.wait_log(&["session started the desktop"]);
    let mut guest_client = host.client(&guest, "90", &["--smoke-guest-input"]);
    host.wait_log(&["guest asks to join", "Couch"]);
    let id = host.waiting();
    host.admin(json!({ "AnswerGuest": { "id": id, "allow": true } }));
    host.wait_log(&["session joined the shared desktop", "Couch"]);
    // Video fans out: the guest decodes its frames from the shared encoder.
    let log = passed(&mut guest_client);
    assert!(log.contains("waiting for the host") || log.contains("session access"));
    assert!(
        log.contains("role=\"guest\"") || log.contains("role=guest"),
        "{log}"
    );
    // Its keyboard and mouse were refused; its gamepad landed in its own
    // block (host slot 4: the owner has 0..=3).
    host.wait_log(&[
        "refused input from session",
        "Couch",
        "has not allowed you keyboard and mouse",
    ]);
    host.wait_log(&["gamepad connected", "pad=4"]);
    assert_eq!(host.count(&["gamepad connected", "pad=0"]), 0);
    assert_eq!(host.count(&["took keyboard and mouse control", "Couch"]), 0);
    // The owner's session went on throughout.
    assert!(owner_client.running(), "{}", owner_client.log());
    passed(&mut owner_client);
}

#[test]
#[ignore = "requires local TCP/UDP sockets and GStreamer runtime plugins"]
fn control_is_taken_back_and_guests_are_kicked_or_revoked() {
    let host = Host::start(&[]);
    let owner = host.owner("owner");
    let second_owner = host.owner("owner-2");
    let keys = host.guest(
        "keys",
        "Keys",
        json!({ "gamepad": true, "keyboard_mouse": true }),
    );
    let viewer = host.guest("viewer", "Viewer", json!({}));
    // Trusted guests join without approval (set by the owner).
    for name in ["Keys", "Viewer"] {
        let id = host.device(name);
        host.admin(json!({ "SetGuest": { "id": id, "permissions": null, "trusted": true } }));
    }
    let mut owner_client = host.client(&owner, "3000", &[]);
    host.wait_log(&["session started the desktop"]);
    let mut keys_client = host.client(&keys, "3000", &["--smoke-guest-input"]);
    // Nobody holds keyboard and mouse: the permitted guest takes them.
    host.wait_log(&["took keyboard and mouse control", "name=Keys"]);
    // Someone at the host takes control: the guest is refused.
    host.admin(json!("TakeControl"));
    host.wait_log(&["the host took keyboard and mouse control"]);
    host.wait_log(&[
        "refused input from session",
        "Keys",
        "Another session has keyboard and mouse control",
    ]);
    let taken = host.count(&["took keyboard and mouse control", "name=Keys"]);
    host.admin(json!("ReleaseControl"));
    host.wait_more(
        &["took keyboard and mouse control", "name=Keys"],
        taken,
        Duration::from_secs(20),
    );
    // An owner moving the mouse takes control back at once.
    let mut second_client = host.client(&second_owner, "3000", &["--smoke-guest-input"]);
    host.wait_log(&[
        "took keyboard and mouse control",
        "role=\"owner\"",
        "previous=Some(",
    ]);
    let refused = host.count(&["refused input from session", "Keys"]);
    host.wait_more(
        &["refused input from session", "Keys"],
        refused,
        Duration::from_secs(20),
    );
    // The guest was told (its client reads access updates).
    let deadline = Instant::now() + Duration::from_secs(20);
    while !keys_client.log().contains("control=\"the owner\"")
        && !keys_client.log().contains("control=the owner")
    {
        assert!(Instant::now() < deadline, "{}", keys_client.log());
        std::thread::sleep(Duration::from_millis(50));
    }
    // Kick: the guest is disconnected, the owners stay.
    let id = host.session("Keys");
    host.admin(json!({ "Kick": { "id": id } }));
    let (status, _) = keys_client.wait(Duration::from_secs(20));
    assert!(!status.success());
    host.wait_log(&["session left", "Keys", "disconnected by the host"]);
    // It cannot simply reconnect at once, even trusted.
    let mut back = host.client(&keys, "30", &[]);
    let (status, _) = back.wait(Duration::from_secs(20));
    assert!(!status.success());
    host.wait_log(&["guest refused for now", "Keys"]);
    // Revocation mid-session ends the guest's session at once.
    let mut viewer_client = host.client(&viewer, "3000", &[]);
    host.wait_log(&["session joined the shared desktop", "Viewer"]);
    let id = host.device("Viewer");
    host.admin(json!({ "RevokeDevice": { "id": id } }));
    let (status, _) = viewer_client.wait(Duration::from_secs(20));
    assert!(!status.success());
    host.wait_log(&["session left", "Viewer", "device access revoked"]);
    assert!(owner_client.running() && second_client.running());
    // The last owner leaving takes the guests along (default policy).
    let late = host.guest("late", "Late", json!({}));
    let id = host.device("Late");
    host.admin(json!({ "SetGuest": { "id": id, "permissions": null, "trusted": true } }));
    let mut late_client = host.client(&late, "3000", &[]);
    host.wait_log(&["session joined the shared desktop", "Late"]);
    drop(owner_client);
    drop(second_client);
    let (status, _) = late_client.wait(Duration::from_secs(30));
    assert!(!status.success());
    host.wait_log(&["session left", "Late", "the last owner left"]);
}

#[test]
#[ignore = "requires local TCP/UDP sockets and GStreamer runtime plugins"]
fn guests_wait_for_approval_and_are_denied_on_timeout() {
    let host = Host::start(&["--guest-approval-timeout", "5"]);
    let owner = host.owner("owner");
    let friend = host.guest("friend", "Friend", json!({ "gamepad": true }));
    let quiet = host.guest("quiet", "Quiet", json!({}));
    let trusted = host.guest("trusted", "Trusted", json!({}));
    let invited = host.guest("invited", "Invited", json!({}));
    let id = host.device("Trusted");
    host.admin(json!({ "SetGuest": { "id": id, "permissions": null, "trusted": true } }));
    // Denied by the host (admin socket, as the settings window does).
    let mut denied = host.client(&friend, "30", &[]);
    let id = host.waiting();
    host.admin(json!({ "AnswerGuest": { "id": id, "allow": false } }));
    let (status, log) = denied.wait(Duration::from_secs(20));
    assert!(!status.success(), "{log}");
    host.wait_log(&["guest not admitted", "Friend"]);
    // A denied guest cannot ask again at once (no prompt flooding).
    let mut again = host.client(&friend, "30", &[]);
    let (status, _) = again.wait(Duration::from_secs(20));
    assert!(!status.success());
    host.wait_log(&["guest refused for now", "Friend"]);
    // Nobody answers: denied after the timeout.
    let started = Instant::now();
    let mut ignored = host.client(&quiet, "30", &[]);
    let (status, log) = ignored.wait(Duration::from_secs(30));
    assert!(!status.success(), "{log}");
    assert!(started.elapsed() >= Duration::from_secs(4));
    assert!(log.contains("waiting for the host"), "{log}");
    host.wait_log(&["guest approval timed out"]);
    // Answering a request that is gone fails.
    assert!(
        !host.admin_result(json!({ "AnswerGuest": { "id": id, "allow": true } }))["error"]
            .is_null()
    );
    // Trusted guests still need approval while no owner is connected
    // (guests leave with the owner by default).
    let alone = host.client(&trusted, "3000", &[]);
    host.wait_log(&["guest asks to join", "Trusted"]);
    let id = host.waiting();
    host.admin(json!({ "AnswerGuest": { "id": id, "allow": true } }));
    host.wait_log(&["session started the desktop", "Trusted"]);
    // No pairing code while a guest watches the screen.
    let refused = host.admin_result(json!("OpenPairing"));
    assert!(
        refused["error"]
            .as_str()
            .is_some_and(|error| error.contains("guest is connected")),
        "{refused}"
    );
    drop(alone);
    host.wait_log(&["last session left"]);
    // The owner's own client approves from its prompt (Ctrl+Alt+Y).
    let owner_client = host.client(&owner, "900", &["--smoke-answer-guests", "allow"]);
    host.wait_log(&["session started the desktop", "Code-paired"]);
    let mut approved = host.client(&invited, "60", &[]);
    passed(&mut approved);
    host.wait_log(&["guest request answered by an owner's client", "allow=true"]);
    let log = owner_client.log();
    assert!(log.contains("a guest asks to join"), "{log}");
    // Owners never wait.
    assert_eq!(host.count(&["guest asks to join", "Code-paired"]), 0);
}

#[test]
#[ignore = "requires local TCP/UDP sockets and GStreamer runtime plugins"]
fn invites_are_limited_to_their_uses_and_lifetime() {
    let host = Host::start(&[]);
    // Another open invite keeps the enrollment port open, so used-up,
    // expired and revoked invites are refused by the exchange itself.
    host.invite("Keeper", 1, 600, json!({}));
    // Two uses: two guest devices, then refused (no replay).
    let link = host.invite("Pair", 2, 600, json!({ "gamepad": true }));
    assert!(link.starts_with("teleport://invite/"));
    host.pair("one", &link).unwrap();
    host.pair("two", &link).unwrap();
    let error = host.pair("three", &link).unwrap_err();
    assert!(error.contains("used up"), "{error}");
    let guests = host.admin(json!("Status"))["devices"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|device| device["source"] == "invite")
        .count();
    assert_eq!(guests, 2);
    // Expired.
    let link = host.invite("Soon", 1, 5, json!({}));
    std::thread::sleep(Duration::from_secs(6));
    assert!(host.pair("late", &link).is_err());
    // Revoked.
    let link = host.invite("Gone", 1, 600, json!({}));
    let invites = host.admin(json!("Sessions"))["invites"].clone();
    let id = invites
        .as_array()
        .unwrap()
        .iter()
        .find(|invite| invite["label"] == "Gone")
        .unwrap()["id"]
        .as_str()
        .unwrap()
        .to_owned();
    host.admin(json!({ "RevokeInvite": { "id": id } }));
    assert!(host.pair("revoked", &link).unwrap_err().contains("invite"));
    // A forged secret for a live invite.
    let link = host.invite("Real", 1, 600, json!({}));
    let forged = format!(
        "{}{}",
        &link[..link.len() - 1],
        if link.ends_with('0') { '1' } else { '0' }
    );
    assert!(host.pair("forged", &forged).is_err());
    host.pair("real", &link).unwrap();
    let open = host.admin(json!("Sessions"))["invites"].clone();
    assert_eq!(open.as_array().unwrap().len(), 1, "{open}");
    assert_eq!(open[0]["label"], "Keeper");
    // Nothing left to enroll: the port closes.
    let id = open[0]["id"].as_str().unwrap().to_owned();
    host.admin(json!({ "RevokeInvite": { "id": id } }));
    std::thread::sleep(Duration::from_millis(1500));
    assert!(
        host.pair("closed", &link)
            .unwrap_err()
            .contains("enrollment port")
    );
}

/// A device reconnecting (its old connection often not yet timed out)
/// replaces its own session; alone, it starts the stream afresh.
#[test]
#[ignore = "requires local TCP/UDP sockets and GStreamer runtime plugins"]
fn a_reconnect_replaces_its_own_session() {
    let host = Host::start(&[]);
    let owner = host.owner("owner");
    let mut old = host.client(&owner, "3000", &[]);
    host.wait_log(&["session started the desktop"]);
    let mut new = host.client(&owner, "60", &[]);
    let log = passed(&mut new);
    assert!(!log.contains("shared with another session"), "{log}");
    host.wait_log(&[
        "session left",
        "replaced by a new connection with the same credential",
    ]);
    assert_eq!(host.count(&["session started the desktop"]), 2);
    let (status, _) = old.wait(Duration::from_secs(20));
    assert!(!status.success());
}
