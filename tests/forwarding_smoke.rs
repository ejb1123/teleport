#![cfg(target_os = "linux")]

use std::{
    io::{BufRead, Read, Write},
    os::unix::net::{UnixListener, UnixStream},
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

fn teleport() -> Command {
    let mut command = Command::new(
        std::env::var_os("TELEPORT_TEST_BINARY")
            .unwrap_or_else(|| env!("CARGO_BIN_EXE_teleport").into()),
    );
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
}

fn exchange(stream: &mut UnixStream, request: &[u8]) -> Vec<u8> {
    stream
        .write_all(&(request.len() as u32).to_be_bytes())
        .unwrap();
    stream.write_all(request).unwrap();
    let mut length = [0; 4];
    stream.read_exact(&mut length).unwrap();
    let length = u32::from_be_bytes(length) as usize;
    assert!((1..=65536).contains(&length));
    let mut response = vec![0; length];
    stream.read_exact(&mut response).unwrap();
    response
}

#[test]
#[ignore = "requires local UDP/Unix sockets and GStreamer plugins"]
fn agent_forwarding_uses_authenticated_desktop_and_cleans_up() {
    let temp = tempfile::tempdir_in("/tmp").unwrap();
    let pairing = temp.path().join("pairing.json");
    let local_agent = temp.path().join("local-agent.sock");
    let fake = UnixListener::bind(&local_agent).unwrap();
    fake.set_nonblocking(true).unwrap();
    let (observed, requests) = std::sync::mpsc::channel();
    let fake_worker = std::thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            match fake.accept() {
                Ok((mut stream, _)) => {
                    stream
                        .set_read_timeout(Some(Duration::from_secs(5)))
                        .unwrap();
                    let mut packet = [0; 5];
                    stream.read_exact(&mut packet).unwrap();
                    assert_eq!(packet, [0, 0, 0, 1, 11]);
                    stream.write_all(&[0, 0, 0, 5, 12, 0, 0, 0, 0]).unwrap();
                    observed.send(()).unwrap();
                    break;
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    assert!(Instant::now() < deadline, "client never called local agent");
                    std::thread::sleep(Duration::from_millis(20));
                }
                Err(error) => panic!("fake agent failed: {error}"),
            }
        }
    });
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
                "--width",
                "640",
                "--fps",
                "30",
                "--allow-ssh-agent",
                "--listen",
                &address,
                "--pairing-file",
            ])
            .arg(&pairing)
            .stdout(Stdio::piped())
            .spawn()
            .unwrap(),
    );
    let (path_sender, paths) = std::sync::mpsc::channel();
    let output = host.0.stdout.take().unwrap();
    std::thread::spawn(move || {
        for line in std::io::BufReader::new(output)
            .lines()
            .map_while(Result::ok)
        {
            if line.contains("SSH agent forwarding enabled")
                && let Some((_, rest)) = line.split_once("socket=")
            {
                let _ = path_sender.send(std::path::PathBuf::from(
                    rest.split_whitespace().next().unwrap(),
                ));
            }
        }
    });
    let deadline = Instant::now() + Duration::from_secs(15);
    while !pairing.exists() {
        assert!(host.0.try_wait().unwrap().is_none());
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(20));
    }
    let mut client = Process(
        teleport()
            .args([
                "client",
                &address,
                "--forward-ssh-agent",
                "--headless-frames",
                "100000",
                "--software-decoder",
                "--pairing-file",
            ])
            .arg(&pairing)
            .env("SSH_AUTH_SOCK", &local_agent)
            .stdout(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let forwarded = paths
        .recv_timeout(Duration::from_secs(20))
        .expect("no host agent socket");
    let mut stream = UnixStream::connect(&forwarded).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    stream
        .set_write_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    assert_eq!(exchange(&mut stream, &[11]), [12, 0, 0, 0, 0]);
    requests.recv_timeout(Duration::from_secs(5)).unwrap();
    fake_worker.join().unwrap();
    // Denied locally on the host, even after the local fake agent has gone away.
    assert_eq!(exchange(&mut stream, &[17]), [5]);
    assert_eq!(exchange(&mut stream, &[27]), [5]);
    client.0.kill().unwrap();
    client.0.wait().unwrap();
    let deadline = Instant::now() + Duration::from_secs(8);
    while forwarded.exists() {
        assert!(
            Instant::now() < deadline,
            "forwarded socket survived disconnect"
        );
        std::thread::sleep(Duration::from_millis(25));
    }
    assert!(!forwarded.parent().unwrap().exists());
}
