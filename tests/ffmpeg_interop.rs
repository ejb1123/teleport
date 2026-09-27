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
#[ignore = "requires loopback sockets and legacy GStreamer codecs"]
fn direct_and_legacy_codecs_interoperate_in_both_directions() {
    for codec in ["h264", "h265"] {
        for (host_backend, client_backend) in [("ffmpeg", "gstreamer"), ("gstreamer", "ffmpeg")] {
            let temporary = tempfile::tempdir().unwrap();
            let pairing = temporary.path().join("pairing.json");
            let socket = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
            let address = socket.local_addr().unwrap().to_string();
            drop(socket);
            let mut host = Process(
                Command::new(env!("CARGO_BIN_EXE_teleport"))
                    .env("TELEPORT_VIDEO_BACKEND", host_backend)
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
                assert!(Instant::now() < deadline, "host not ready");
                std::thread::sleep(Duration::from_millis(25));
            }
            let mut client = Command::new(env!("CARGO_BIN_EXE_teleport"))
                .env("TELEPORT_VIDEO_BACKEND", client_backend)
                .args([
                    "client",
                    &address,
                    "--codec",
                    codec,
                    "--software-decoder",
                    "--width",
                    "640",
                    "--headless-frames",
                    "15",
                    "--pairing-file",
                ])
                .arg(&pairing)
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap();
            let deadline = Instant::now() + Duration::from_secs(30);
            while client.try_wait().unwrap().is_none() {
                if Instant::now() > deadline {
                    let _ = client.kill();
                    let _ = client.wait();
                    panic!("interop client timed out");
                }
                std::thread::sleep(Duration::from_millis(25));
            }
            let result = client.wait_with_output().unwrap();
            let log = String::from_utf8_lossy(&result.stdout);
            assert!(
                result.status.success() && log.contains("SMOKE PASS"),
                "{host_backend} -> {client_backend} {codec}: {log} {}",
                String::from_utf8_lossy(&result.stderr)
            );
            if client_backend == "ffmpeg" {
                assert!(log.contains("ffmpeg:"), "wrong decoder backend: {log}");
            }
        }
    }
}
