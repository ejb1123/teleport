#![cfg(target_os = "linux")]
//! Recording a session's metrics (docs/performance.md, "Recording
//! metrics"): a headless client against a test host writes one file that
//! `teleport metrics` lists, shows and exports.

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

fn read_records(path: &std::path::Path) -> Vec<serde_json::Value> {
    std::fs::read_to_string(path)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).expect("a metrics line is one JSON object"))
        .collect()
}

#[test]
#[ignore = "requires local UDP sockets and GStreamer plugins"]
fn a_recorded_session_is_listed_shown_and_exported() {
    let temp = tempfile::tempdir().unwrap();
    let address = free_udp();
    let metrics_dir = temp.path().join("metrics");
    let log = std::fs::File::create(temp.path().join("host.log")).unwrap();
    let mut host = teleport()
        .args(["host", "--source", "test", "--encoder", "software"])
        .args(["--listen", &address, "--width", "640"])
        .arg("--identity-dir")
        .arg(temp.path().join("identity"))
        .stdout(log.try_clone().unwrap())
        .stderr(log)
        .spawn()
        .unwrap();
    let pairing = temp.path().join("identity/pairing.json");
    let deadline = Instant::now() + Duration::from_secs(20);
    while !pairing.exists() {
        assert!(Instant::now() < deadline, "host wrote no pairing file");
        std::thread::sleep(Duration::from_millis(50));
    }

    // Enough frames for a few once-a-second samples at any test rate.
    let frames = 150;
    let client = teleport()
        .args(["client", &address, "--pairing-file"])
        .arg(&pairing)
        .args([
            "--headless-frames",
            &frames.to_string(),
            "--software-decoder",
        ])
        .arg("--metrics-dir")
        .arg(&metrics_dir)
        .stdout(Stdio::piped())
        .output()
        .unwrap();
    let _ = host.kill();
    let _ = host.wait();
    let output = String::from_utf8_lossy(&client.stdout);
    assert!(output.contains("SMOKE PASS"), "client failed: {output}");
    assert!(
        output.contains("recording session metrics"),
        "the client did not say where it records: {output}"
    );

    let files: Vec<_> = std::fs::read_dir(&metrics_dir)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect();
    assert_eq!(files.len(), 1, "one file per session: {files:?}");
    let file = &files[0];
    let name = file.file_name().unwrap().to_str().unwrap();
    assert!(
        name.len() == "2026-11-01T22-33-34Z.jsonl".len() && name.ends_with("Z.jsonl"),
        "file named after its start time: {name}"
    );
    let records = read_records(file);
    let kinds: Vec<&str> = records
        .iter()
        .map(|record| record["record"].as_str().unwrap())
        .collect();
    assert_eq!(kinds[0], "session", "{kinds:?}");
    assert_eq!(kinds[1], "stream", "{kinds:?}");
    assert_eq!(*kinds.last().unwrap(), "summary", "{kinds:?}");
    let samples = kinds.iter().filter(|kind| **kind == "sample").count();
    assert!(samples >= 1, "no once-a-second sample: {kinds:?}");

    let session = &records[0];
    assert_eq!(session["transport"], "direct");
    assert_eq!(session["headless"], true);
    assert_eq!(session["source"], "test");
    assert_eq!(session["version"], env!("CARGO_PKG_VERSION"));
    // Nothing that identifies the host or the user.
    let text = std::fs::read_to_string(file).unwrap();
    let pairing_text = std::fs::read_to_string(&pairing).unwrap();
    let pairing: serde_json::Value = serde_json::from_str(&pairing_text).unwrap();
    assert!(!text.contains(&address), "the address is in the file");
    assert!(
        !text.contains(pairing["fingerprint"].as_str().unwrap()),
        "the fingerprint is in the file"
    );

    let stream = &records[1];
    assert_eq!(stream["width"], 640);
    assert!(
        stream["codec"]
            .as_str()
            .is_some_and(|codec| !codec.is_empty())
    );
    assert!(
        stream["decoder"]
            .as_str()
            .is_some_and(|d| d.contains("ffmpeg")),
        "{stream}"
    );
    let sample = records.iter().find(|r| r["record"] == "sample").unwrap();
    assert!(sample["received_fps"].as_f64().unwrap() > 0.0, "{sample}");
    assert!(sample["video_mbps"].as_f64().unwrap() > 0.0, "{sample}");
    assert!(sample["warnings"].is_array(), "{sample}");
    let summary = records.last().unwrap();
    assert_eq!(summary["reason"], "closed");
    assert!(
        summary["displayed_frames"].as_u64().unwrap() >= frames,
        "{summary}"
    );
    assert_eq!(summary["samples"].as_u64().unwrap(), samples as u64);

    // The review commands read it back.
    let ls = teleport()
        .args(["metrics", "ls", "--dir"])
        .arg(&metrics_dir)
        .output()
        .unwrap();
    let ls = String::from_utf8_lossy(&ls.stdout);
    assert!(ls.contains(name) && ls.contains("direct"), "{ls}");
    let show = teleport()
        .args(["metrics", "show", "--dir"])
        .arg(&metrics_dir)
        .output()
        .unwrap();
    let show = String::from_utf8_lossy(&show.stdout);
    assert!(show.contains("Summary: closed"), "{show}");
    assert!(show.contains("(headless)"), "{show}");
    let export_path = temp.path().join("report.jsonl");
    let export = teleport()
        .args(["metrics", "export", "--dir"])
        .arg(&metrics_dir)
        .arg(&export_path)
        .output()
        .unwrap();
    assert!(
        export.status.success(),
        "{}",
        String::from_utf8_lossy(&export.stderr)
    );
    let exported = read_records(&export_path);
    assert_eq!(exported.len(), records.len());
    assert!(
        exported
            .iter()
            .all(|r| r["session"] == name.trim_end_matches(".jsonl"))
    );
}
