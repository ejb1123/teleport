#![cfg(target_os = "linux")]
//! A supervisor's view of a host (docs/supervision.md): the events file
//! and a host that exits when its session ends.

use std::{
    process::{Command, Stdio},
    time::{Duration, Instant},
};

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

fn events(path: &std::path::Path) -> Vec<serde_json::Value> {
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .map(|line| serde_json::from_str(line).expect("an events line is one JSON object"))
        .collect()
}

#[test]
#[ignore = "requires local UDP sockets and GStreamer plugins"]
fn events_follow_the_session_and_the_host_exits_after_it() {
    let temp = tempfile::tempdir().unwrap();
    let address = free_udp();
    let events_file = temp.path().join("events.jsonl");
    let log = std::fs::File::create(temp.path().join("host.log")).unwrap();
    let mut host = teleport()
        .args(["host", "--source", "test", "--encoder", "software"])
        .args(["--listen", &address, "--width", "640"])
        .arg("--identity-dir")
        .arg(temp.path().join("identity"))
        .arg("--events-file")
        .arg(&events_file)
        .arg("--exit-after-session")
        .stdout(log.try_clone().unwrap())
        .stderr(log)
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(20);
    while !events(&events_file).iter().any(|e| e["event"] == "ready") {
        assert!(Instant::now() < deadline, "no ready event");
        std::thread::sleep(Duration::from_millis(50));
    }
    let ready = events(&events_file);
    let pairing: serde_json::Value =
        serde_json::from_slice(&std::fs::read(temp.path().join("identity/pairing.json")).unwrap())
            .unwrap();
    assert_eq!(ready[0]["fingerprint"], pairing["fingerprint"]);
    // The host is still running before any session.
    assert!(host.try_wait().unwrap().is_none());

    let client = teleport()
        .args(["client", &address, "--pairing-file"])
        .arg(temp.path().join("identity/pairing.json"))
        .args(["--headless-frames", "30", "--software-decoder"])
        .stdout(Stdio::piped())
        .output()
        .unwrap();
    assert!(
        String::from_utf8_lossy(&client.stdout).contains("SMOKE PASS"),
        "client failed: {}",
        String::from_utf8_lossy(&client.stdout)
    );

    let deadline = Instant::now() + Duration::from_secs(20);
    let status = loop {
        if let Some(status) = host.try_wait().unwrap() {
            break status;
        }
        if Instant::now() > deadline {
            let _ = host.kill();
            panic!(
                "host did not exit after its session:\n{}",
                std::fs::read_to_string(temp.path().join("host.log")).unwrap_or_default()
            );
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    assert!(status.success(), "host exit status {status}");
    let names: Vec<String> = events(&events_file)
        .iter()
        .map(|e| {
            assert!(e["at"].as_u64().is_some_and(|at| at > 1_700_000_000_000));
            e["event"].as_str().unwrap().to_owned()
        })
        .collect();
    assert_eq!(
        names,
        ["ready", "session_joined", "session_left", "exiting"],
        "events: {names:?}"
    );
    let joined = &events(&events_file)[1];
    assert_eq!(joined["role"], "owner");
    assert_eq!(joined["transport"], "direct");
}
