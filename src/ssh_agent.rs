//! Experimental, double-opt-in SSH agent forwarding, carried only inside pinned MoQ.
//! RFC 9987 framing; only identities and SSH public-key userauth signing are allowed.
//! Agent mutation, PKCS#11 loading, and all extensions are deliberately denied.
use anyhow::{Context, Result, ensure};
use std::{collections::BTreeMap, path::PathBuf, time::Duration};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;

pub const REQUEST_TRACK: &str = "ssh-agent-requests-v1";
pub const RESPONSE_TRACK: &str = "ssh-agent-responses-v1";
const MAX_PACKET: usize = 64 * 1024;
const TIMEOUT: Duration = Duration::from_secs(60);
const FAILURE: &[u8] = &[5];

/// Session teardown stops the relay and closes pending local agent connections.
/// Providers may finish an already-started operation; no result is delivered after teardown.
pub struct Task {
    task: tokio::task::JoinHandle<()>,
    #[cfg(target_os = "linux")]
    _socket: Option<SocketDirectory>,
}

impl Drop for Task {
    fn drop(&mut self) {
        self.task.abort();
        // SocketDirectory removes only our socket and our empty private directory.
    }
}

pub async fn closed(task: &mut Option<Task>) -> Result<()> {
    if let Some(task) = task {
        (&mut task.task)
            .await
            .context("SSH forwarding worker failed")?;
        anyhow::bail!("SSH agent forwarding stopped; disconnecting to revoke forwarding");
    }
    std::future::pending().await
}

pub fn agent_path() -> Result<PathBuf> {
    use std::os::unix::fs::FileTypeExt;
    let path = PathBuf::from(std::env::var_os("SSH_AUTH_SOCK").context(
        "--forward-ssh-agent requires a local SSH_AUTH_SOCK; start/load your agent locally",
    )?);
    ensure!(
        std::fs::metadata(&path)?.file_type().is_socket(),
        "SSH_AUTH_SOCK is not a Unix socket"
    );
    Ok(path)
}

pub fn create_track(
    broadcast: &mut moq_net::broadcast::Producer,
    name: &str,
) -> Result<moq_net::track::Producer> {
    Ok(broadcast.create_track(name, moq_net::track::Info::default().with_ordered(true))?)
}

pub fn subscription() -> moq_net::track::Subscription {
    moq_net::track::Subscription::default()
        .with_ordered(true)
        .with_group_start(0)
}

struct Reader {
    track: moq_net::track::Subscriber,
    pending: BTreeMap<u64, moq_net::group::Consumer>,
    sequence: u64,
}

impl Reader {
    fn new(track: moq_net::track::Subscriber) -> Self {
        Self {
            track,
            pending: BTreeMap::new(),
            sequence: 0,
        }
    }

    async fn read(&mut self) -> Result<Vec<u8>> {
        let mut group =
            crate::protocol::ordered_group(&mut self.track, &mut self.pending, self.sequence)
                .await?;
        self.sequence = self
            .sequence
            .checked_add(1)
            .context("SSH agent sequence overflow")?;
        let packet =
            tokio::time::timeout(TIMEOUT, crate::protocol::read_frame(&mut group, MAX_PACKET))
                .await
                .context("SSH agent frame timeout")??
                .context("empty SSH agent group")?;
        ensure!(!packet.is_empty(), "empty SSH agent packet");
        // Exactly one frame per group; do not allow an unbounded stream of extras.
        ensure!(
            tokio::time::timeout(TIMEOUT, group.next_frame())
                .await??
                .is_none(),
            "extra SSH agent frame"
        );
        Ok(packet.to_vec())
    }
}

fn send(track: &mut moq_net::track::Producer, packet: &[u8]) -> Result<()> {
    ensure!(
        !packet.is_empty() && packet.len() <= MAX_PACKET,
        "SSH agent packet size invalid"
    );
    let mut group = track.append_group()?;
    group.write_frame(
        moq_net::Timestamp::now(),
        bytes::Bytes::copy_from_slice(packet),
    )?;
    group.finish()?;
    Ok(())
}

async fn read_packet(stream: &mut UnixStream) -> Result<Vec<u8>> {
    let length = stream.read_u32().await? as usize;
    ensure!(
        (1..=MAX_PACKET).contains(&length),
        "SSH agent packet size invalid"
    );
    let mut packet = vec![0; length];
    stream.read_exact(&mut packet).await?;
    Ok(packet)
}

async fn write_packet(stream: &mut UnixStream, packet: &[u8]) -> Result<()> {
    ensure!(
        !packet.is_empty() && packet.len() <= MAX_PACKET,
        "SSH agent packet size invalid"
    );
    stream.write_u32(packet.len() as u32).await?;
    stream.write_all(packet).await?;
    Ok(())
}

fn string<'a>(input: &mut &'a [u8]) -> Option<&'a [u8]> {
    let length = u32::from_be_bytes(input.get(..4)?.try_into().ok()?) as usize;
    let result = input.get(4..4usize.checked_add(length)?)?;
    *input = &input[4 + length..];
    Some(result)
}

/// Validate on the client too: the authenticated host is not trusted to filter requests.
fn allowed(packet: &[u8]) -> bool {
    if packet == [11] {
        return true;
    } // SSH_AGENTC_REQUEST_IDENTITIES
    if packet.first() != Some(&13) || packet.len() > MAX_PACKET {
        return false;
    }
    let mut request = &packet[1..];
    let Some(key) = string(&mut request) else {
        return false;
    };
    let Some(mut data) = string(&mut request) else {
        return false;
    };
    // Only standard RSA SHA-2 flags, no unknown flag semantics.
    if !matches!(request, [0, 0, 0, 0 | 2 | 4]) {
        return false;
    }
    // RFC 4252 public-key authentication, not arbitrary data signing (including
    // ssh-keygen -Y and WebAuthn signatures). The session hash is opaque here;
    // this is NOT destination binding and cannot make an untrusted host safe.
    let Some(session_id) = string(&mut data) else {
        return false;
    };
    if !(16..=64).contains(&session_id.len()) || data.first() != Some(&50) {
        return false;
    }
    data = &data[1..];
    if !string(&mut data).is_some_and(|user| !user.is_empty() && user.len() <= 256) {
        return false;
    }
    if string(&mut data) != Some(b"ssh-connection".as_slice())
        || string(&mut data) != Some(b"publickey".as_slice())
        || data.first() != Some(&1)
    {
        return false;
    }
    data = &data[1..];
    if !string(&mut data).is_some_and(|algorithm| !algorithm.is_empty() && algorithm.len() <= 128) {
        return false;
    }
    string(&mut data) == Some(key) && data.is_empty()
}

async fn agent_request(path: &std::path::Path, packet: &[u8]) -> Result<Vec<u8>> {
    if !allowed(packet) {
        return Ok(FAILURE.to_vec());
    }
    // A fresh local connection for each request intentionally cannot inherit a
    // previous caller's OpenSSH destination bindings. Constrained keys fail closed.
    let mut agent = UnixStream::connect(path)
        .await
        .context("local SSH agent unavailable")?;
    ensure!(
        agent.peer_cred()?.uid() == unsafe { libc::geteuid() },
        "local SSH agent belongs to another user"
    );
    write_packet(&mut agent, packet).await?;
    let response = read_packet(&mut agent).await?;
    ensure!(
        response == FAILURE || matches!((packet[0], response[0]), (11, 12) | (13, 14)),
        "unexpected SSH agent response"
    );
    Ok(response)
}

pub fn client(
    path: PathBuf,
    requests: moq_net::track::Subscriber,
    mut responses: moq_net::track::Producer,
) -> Task {
    let task = tokio::spawn(async move {
        let result: Result<()> = async {
            let mut reader = Reader::new(requests);
            loop {
                let packet = reader.read().await?;
                // One in-flight operation and a maximum of ten requests/second.
                tokio::time::sleep(Duration::from_millis(100)).await;
                let response =
                    match tokio::time::timeout(TIMEOUT, agent_request(&path, &packet)).await {
                        Ok(Ok(response)) => response,
                        _ => FAILURE.to_vec(),
                    };
                send(&mut responses, &response)?;
            }
        }
        .await;
        if result.is_err() {
            tracing::warn!("SSH agent forwarding stopped; reconnect to re-enable");
        }
    });
    Task {
        task,
        #[cfg(target_os = "linux")]
        _socket: None,
    }
}

#[cfg(target_os = "linux")]
struct SocketDirectory {
    path: PathBuf,
    socket_identity: Option<(u64, u64)>,
}

#[cfg(target_os = "linux")]
impl SocketDirectory {
    fn create() -> Result<Self> {
        use std::os::unix::fs::DirBuilderExt;
        // Fixed short parent avoids sockaddr_un limits with deep TMPDIR values.
        let path =
            PathBuf::from("/tmp").join(format!("teleport-agent-{:032x}", rand::random::<u128>()));
        std::fs::DirBuilder::new().mode(0o700).create(&path)?;
        Ok(Self {
            path,
            socket_identity: None,
        })
    }
    fn socket(&self) -> PathBuf {
        self.path.join("agent.sock")
    }
    fn remember_socket(&mut self) -> Result<()> {
        use std::os::unix::fs::MetadataExt;
        let metadata = std::fs::symlink_metadata(self.socket())?;
        self.socket_identity = Some((metadata.dev(), metadata.ino()));
        Ok(())
    }
}

#[cfg(target_os = "linux")]
impl Drop for SocketDirectory {
    fn drop(&mut self) {
        use std::os::unix::fs::MetadataExt;
        if std::fs::symlink_metadata(self.socket())
            .is_ok_and(|metadata| self.socket_identity == Some((metadata.dev(), metadata.ino())))
        {
            let _ = std::fs::remove_file(self.socket());
        }
        let _ = std::fs::remove_dir(&self.path);
    }
}

#[cfg(target_os = "linux")]
pub fn host(
    mut requests: moq_net::track::Producer,
    responses: moq_net::track::Subscriber,
) -> Result<Task> {
    use std::os::unix::fs::PermissionsExt;
    let mut directory = SocketDirectory::create()?;
    let listener = tokio::net::UnixListener::bind(directory.socket())?;
    directory.remember_socket()?;
    std::fs::set_permissions(directory.socket(), std::fs::Permissions::from_mode(0o600))?;
    tracing::warn!(socket = %directory.socket().display(), "SSH agent forwarding enabled for this session; set SSH_AUTH_SOCK explicitly; disconnect removes socket");
    let task = tokio::spawn(async move {
        let result: Result<()> = async {
            let mut reader = Reader::new(responses);
            loop {
                let (mut stream, _) = listener.accept().await?;
                // Filesystem isolation plus kernel peer credentials, not pathname trust alone.
                ensure!(
                    stream.peer_cred()?.uid() == unsafe { libc::geteuid() },
                    "SSH agent socket peer mismatch"
                );
                loop {
                    let packet = match tokio::time::timeout(TIMEOUT, read_packet(&mut stream)).await
                    {
                        Ok(Ok(packet)) => packet,
                        _ => break,
                    };
                    if !allowed(&packet) {
                        if !matches!(
                            tokio::time::timeout(TIMEOUT, write_packet(&mut stream, FAILURE)).await,
                            Ok(Ok(()))
                        ) {
                            break;
                        }
                        continue;
                    }
                    send(&mut requests, &packet)?;
                    // A timeout closes the relay rather than mis-associating a late reply.
                    let response =
                        tokio::time::timeout(TIMEOUT + Duration::from_secs(5), reader.read())
                            .await??;
                    if !matches!(
                        tokio::time::timeout(TIMEOUT, write_packet(&mut stream, &response)).await,
                        Ok(Ok(()))
                    ) {
                        break;
                    }
                }
            }
        }
        .await;
        if result.is_err() {
            tracing::warn!("SSH agent forwarding stopped; reconnect to re-enable");
        }
    });
    Ok(Task {
        task,
        _socket: Some(directory),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn put(output: &mut Vec<u8>, value: &[u8]) {
        output.extend_from_slice(&(value.len() as u32).to_be_bytes());
        output.extend_from_slice(value);
    }
    fn signature() -> Vec<u8> {
        let key = b"test-key";
        let mut data = Vec::new();
        put(&mut data, &[42; 32]);
        data.push(50);
        put(&mut data, b"user");
        put(&mut data, b"ssh-connection");
        put(&mut data, b"publickey");
        data.push(1);
        put(&mut data, b"ssh-ed25519");
        put(&mut data, key);
        let mut request = vec![13];
        put(&mut request, key);
        put(&mut request, &data);
        request.extend_from_slice(&[0; 4]);
        request
    }
    #[test]
    fn rejects_mutation_extensions_and_arbitrary_signatures() {
        assert!(allowed(&[11]));
        assert!(allowed(&signature()));
        for kind in [1, 7, 9, 17, 18, 19, 20, 21, 22, 23, 25, 26, 27] {
            assert!(!allowed(&[kind]));
        }
        assert!(!allowed(&[11, 0]));
        assert!(!allowed(&[13, 255, 255, 255, 255]));
        let mut arbitrary = vec![13];
        put(&mut arbitrary, b"key");
        put(&mut arbitrary, b"SSHSIG arbitrary signing denied");
        arbitrary.extend_from_slice(&[0; 4]);
        assert!(!allowed(&arbitrary));
        for length in 0..signature().len() {
            assert!(!allowed(&signature()[..length]));
        }
    }

    #[tokio::test]
    async fn rejects_oversized_socket_frame_before_allocation() -> Result<()> {
        let (mut writer, mut reader) = UnixStream::pair()?;
        writer.write_u32(MAX_PACKET as u32 + 1).await?;
        assert!(read_packet(&mut reader).await.is_err());
        Ok(())
    }

    #[tokio::test]
    async fn fake_agent_success_and_mutation_denial() -> Result<()> {
        let dir = tempfile::tempdir_in("/tmp")?;
        let path = dir.path().join("agent.sock");
        let listener = tokio::net::UnixListener::bind(&path)?;
        let fake = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await?;
            assert_eq!(read_packet(&mut socket).await?, [11]);
            write_packet(&mut socket, &[12, 0, 0, 0, 0]).await
        });
        assert_eq!(agent_request(&path, &[11]).await?, [12, 0, 0, 0, 0]);
        fake.await??;
        // No listener now: disallowed messages must not even connect locally.
        assert_eq!(agent_request(&path, &[17]).await?, FAILURE);
        assert_eq!(agent_request(&path, &[27]).await?, FAILURE);
        Ok(())
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn private_socket_directory_cleanup() -> Result<()> {
        use std::os::unix::fs::PermissionsExt;
        let mut dir = SocketDirectory::create()?;
        let path = dir.path.clone();
        assert_eq!(
            std::fs::metadata(&path)?.permissions().mode() & 0o777,
            0o700
        );
        let _socket = std::os::unix::net::UnixListener::bind(dir.socket())?;
        dir.remember_socket()?;
        drop(dir);
        assert!(!path.exists());
        Ok(())
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn cleanup_preserves_replaced_socket() -> Result<()> {
        let mut dir = SocketDirectory::create()?;
        let path = dir.path.clone();
        let original = std::os::unix::net::UnixListener::bind(dir.socket())?;
        dir.remember_socket()?;
        std::fs::remove_file(dir.socket())?;
        let replacement = std::os::unix::net::UnixListener::bind(dir.socket())?;
        let socket = dir.socket();
        drop(dir);
        assert!(socket.exists());
        drop(original);
        drop(replacement);
        std::fs::remove_file(socket)?;
        std::fs::remove_dir(path)?;
        Ok(())
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    #[ignore = "requires OpenSSH tools; starts only a temporary isolated agent"]
    async fn real_openssh_signing_over_moq_and_disconnect_cleanup() -> Result<()> {
        struct Process(std::process::Child);
        impl Drop for Process {
            fn drop(&mut self) {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }
        let directory = tempfile::tempdir_in("/tmp")?;
        let agent_path = directory.path().join("real-agent.sock");
        let key_path = directory.path().join("test-key");
        let generated = std::process::Command::new("ssh-keygen")
            .args(["-q", "-t", "ed25519", "-N", "", "-f"])
            .arg(&key_path)
            .output()?;
        ensure!(generated.status.success(), "test key generation failed");
        let _agent = Process(
            std::process::Command::new("ssh-agent")
                .args(["-D", "-a"])
                .arg(&agent_path)
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()?,
        );
        for _ in 0..100 {
            if agent_path.exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let loaded = std::process::Command::new("ssh-add")
            .arg(&key_path)
            .env("SSH_AUTH_SOCK", &agent_path)
            .output()?;
        ensure!(loaded.status.success(), "test agent key load failed");

        let mut broadcast = moq_net::broadcast::Info::new().produce();
        let requests = create_track(&mut broadcast, REQUEST_TRACK)?;
        let responses = create_track(&mut broadcast, RESPONSE_TRACK)?;
        let mut request_reader = requests.consume().subscribe(subscription()).await?;
        request_reader.start_at(0);
        let mut response_reader = responses.consume().subscribe(subscription()).await?;
        response_reader.start_at(0);
        let client_task = client(agent_path, request_reader, responses);
        let host_task = host(requests, response_reader)?;
        let remote_socket = host_task._socket.as_ref().unwrap().socket();
        let mut stream = UnixStream::connect(&remote_socket).await?;
        write_packet(&mut stream, &[11]).await?;
        let identities =
            tokio::time::timeout(Duration::from_secs(5), read_packet(&mut stream)).await??;
        assert_eq!(&identities[..5], &[12, 0, 0, 0, 1]);
        let mut entries = &identities[5..];
        let key = string(&mut entries).unwrap();
        let mut key_parts = key;
        assert_eq!(string(&mut key_parts), Some(b"ssh-ed25519".as_slice()));
        let public_key = string(&mut key_parts).unwrap();
        let mut data = Vec::new();
        put(&mut data, &[42; 32]);
        data.push(50);
        put(&mut data, b"user");
        put(&mut data, b"ssh-connection");
        put(&mut data, b"publickey");
        data.push(1);
        put(&mut data, b"ssh-ed25519");
        put(&mut data, key);
        let mut request = vec![13];
        put(&mut request, key);
        put(&mut request, &data);
        request.extend_from_slice(&[0; 4]);
        write_packet(&mut stream, &request).await?;
        let signature =
            tokio::time::timeout(Duration::from_secs(5), read_packet(&mut stream)).await??;
        assert_eq!(signature[0], 14);
        let mut envelope = &signature[1..];
        let mut blob = string(&mut envelope).unwrap();
        assert_eq!(string(&mut blob), Some(b"ssh-ed25519".as_slice()));
        let signature = string(&mut blob).unwrap();
        ring::signature::UnparsedPublicKey::new(&ring::signature::ED25519, public_key)
            .verify(&data, signature)
            .map_err(|_| anyhow::anyhow!("forwarded signature did not verify"))?;
        // A remote ssh-add -D and provider loading may not mutate the local agent.
        for mutation in [&[19][..], &[20][..], &[27][..]] {
            write_packet(&mut stream, mutation).await?;
            assert_eq!(read_packet(&mut stream).await?, FAILURE);
        }
        write_packet(&mut stream, &[11]).await?;
        assert_eq!(read_packet(&mut stream).await?, identities);
        drop(host_task);
        drop(client_task);
        assert!(!remote_socket.exists());
        let mut byte = [0u8];
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(2), stream.read(&mut byte)).await??,
            0
        );
        Ok(())
    }
}
