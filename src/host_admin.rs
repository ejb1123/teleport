//! Private, same-user host control. Never exposed over TCP.
use anyhow::{Context, Result, ensure};
use clap::{Args, Subcommand};
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicU64, Ordering};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, UnixListener},
    sync::{Mutex, Notify},
};
use zeroize::{Zeroize, Zeroizing};

#[derive(Args)]
pub struct Options {
    #[arg(long)]
    pub identity_dir: Option<PathBuf>,
    #[command(subcommand)]
    pub command: AdminCommand,
}
#[derive(Subcommand)]
pub enum AdminCommand {
    Status,
    OpenPairing,
    ClosePairing,
    SetPassword { username: String },
    DisablePassword,
    RevokeDevice { id: String },
    RevokeAllDevices,
}
#[derive(Serialize, Deserialize)]
pub enum Request {
    Status,
    OpenPairing,
    ClosePairing,
    SetPassword { username: String, password: String },
    DisablePassword,
    RevokeDevice { id: String },
    RevokeAllDevices,
}
impl Request {
    pub fn clear_password(&mut self) {
        if let Self::SetPassword { password, .. } = self {
            password.zeroize();
        }
    }
}
#[derive(Serialize, Deserialize, Default)]
pub struct Response {
    pub error: Option<String>,
    pub listen: String,
    pub fingerprint: String,
    pub username: Option<String>,
    pub devices: Vec<Device>,
    pub pairing_open: bool,
    pub code: Option<String>,
}
#[derive(Serialize, Deserialize)]
pub struct Device {
    pub id: String,
    pub name: String,
}
pub fn default_directory() -> Result<PathBuf> {
    Ok(
        PathBuf::from(std::env::var_os("HOME").context("HOME is not set")?)
            .join(".local/state/teleport/host"),
    )
}
pub fn run(options: Options) -> Result<()> {
    let mut request = match options.command {
        AdminCommand::Status => Request::Status,
        AdminCommand::OpenPairing => Request::OpenPairing,
        AdminCommand::ClosePairing => Request::ClosePairing,
        AdminCommand::SetPassword { username } => {
            let password = rpassword::prompt_password("New Teleport password: ")?;
            let mut confirmation = rpassword::prompt_password("Confirm password: ")?;
            let matches = password == confirmation;
            confirmation.zeroize();
            ensure!(matches, "passwords do not match");
            Request::SetPassword { username, password }
        }
        AdminCommand::DisablePassword => Request::DisablePassword,
        AdminCommand::RevokeDevice { id } => Request::RevokeDevice { id },
        AdminCommand::RevokeAllDevices => Request::RevokeAllDevices,
    };
    let result = call(
        &options.identity_dir.unwrap_or(default_directory()?),
        &request,
    );
    request.clear_password();
    let response = result?;
    println!("{}", serde_json::to_string_pretty(&response)?);
    Ok(())
}
fn check_directory(directory: &Path) -> Result<()> {
    use std::os::unix::fs::MetadataExt;
    let metadata = std::fs::symlink_metadata(directory)?;
    ensure!(
        metadata.is_dir()
            && metadata.uid() == unsafe { libc::geteuid() }
            && metadata.mode() & 0o077 == 0,
        "host identity directory must be private and owned by this user"
    );
    Ok(())
}
fn socket_address(directory: &Path) -> Result<(std::fs::File, PathBuf)> {
    use std::os::{fd::AsRawFd, unix::fs::OpenOptionsExt};
    let directory = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
        .open(directory)?;
    // Linux resolves pathname Unix sockets by inode. Holding this directory fd
    // avoids SUN_LEN limits without relocating the private socket elsewhere.
    let address = PathBuf::from(format!(
        "/proc/self/fd/{}/admin.sock",
        directory.as_raw_fd()
    ));
    Ok((directory, address))
}
pub fn call(directory: &Path, request: &Request) -> Result<Response> {
    use std::io::{Read, Write};
    use std::os::{
        fd::AsRawFd,
        unix::fs::{FileTypeExt, MetadataExt},
    };
    check_directory(directory)?;
    let path = directory.join("admin.sock");
    let metadata = std::fs::symlink_metadata(&path)
        .context("host control unavailable; start the updated persistent host service")?;
    ensure!(
        metadata.file_type().is_socket()
            && metadata.uid() == unsafe { libc::geteuid() }
            && metadata.mode() & 0o077 == 0,
        "unsafe host admin socket"
    );
    let (_directory, address) = socket_address(directory)?;
    let mut stream = std::os::unix::net::UnixStream::connect(&address)
        .context("host control unavailable; start the updated persistent host service")?;
    let mut credentials: libc::ucred = unsafe { std::mem::zeroed() };
    let mut length = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    // SO_PEERCRED fills the fixed-sized ucred buffer for this connected Unix socket.
    ensure!(
        unsafe {
            libc::getsockopt(
                stream.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_PEERCRED,
                (&mut credentials as *mut libc::ucred).cast(),
                &mut length,
            )
        } == 0
            && credentials.uid == unsafe { libc::geteuid() },
        "admin server belongs to another user"
    );
    stream.set_read_timeout(Some(Duration::from_secs(15)))?;
    stream.set_write_timeout(Some(Duration::from_secs(15)))?;
    let data = Zeroizing::new(serde_json::to_vec(request)?);
    ensure!(data.len() <= 8192, "admin request too large");
    stream.write_all(&(data.len() as u32).to_be_bytes())?;
    stream.write_all(&data)?;
    let mut length = [0; 4];
    stream.read_exact(&mut length)?;
    let length = u32::from_be_bytes(length) as usize;
    ensure!(length <= 65536, "admin response too large");
    let mut data = vec![0; length];
    stream.read_exact(&mut data)?;
    let response: Response = serde_json::from_slice(&data)?;
    if let Some(error) = &response.error {
        anyhow::bail!("{error}");
    }
    Ok(response)
}

#[derive(Clone)]
struct Code {
    value: String,
    deadline: tokio::time::Instant,
    attempts: u8,
    generation: u64,
}
struct Network {
    listener: Option<Arc<TcpListener>>,
    code: Option<Code>,
}
pub struct Control {
    access: Arc<crate::access::ServerState>,
    pairing: crate::protocol::Pairing,
    address: std::net::SocketAddr,
    network: Mutex<Network>,
    changed: Arc<Notify>,
    epoch: AtomicU64,
}
pub struct Service {
    tasks: Vec<tokio::task::JoinHandle<()>>,
    path: PathBuf,
    inode: u64,
    device: u64,
}
impl Drop for Service {
    fn drop(&mut self) {
        for task in &self.tasks {
            task.abort();
        }
        use std::os::unix::fs::MetadataExt;
        if std::fs::symlink_metadata(&self.path)
            .is_ok_and(|m| m.ino() == self.inode && m.dev() == self.device)
        {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}
impl Control {
    pub async fn open_pairing(&self) -> Result<String> {
        let mut network = self.network.lock().await;
        self.bind(&mut network).await?;
        let value = format!("{:06}", rand::random_range(0..1_000_000u32));
        let printable = format!("{}-{}", &value[..3], &value[3..]);
        network.code = Some(Code {
            value,
            deadline: tokio::time::Instant::now() + Duration::from_secs(300),
            attempts: 5,
            generation: self.epoch.fetch_add(1, Ordering::SeqCst).wrapping_add(1),
        });
        self.changed.notify_one();
        Ok(printable)
    }
    async fn bind(&self, network: &mut Network) -> Result<()> {
        if network.listener.is_none() {
            network.listener = Some(Arc::new(
                TcpListener::bind(self.address)
                    .await
                    .context("cannot open enrollment TCP port")?,
            ));
        }
        Ok(())
    }
    async fn request(&self, request: Request) -> Result<Response> {
        let mut code = None;
        match request {
            Request::Status => (),
            Request::OpenPairing => code = Some(self.open_pairing().await?),
            Request::ClosePairing => {
                let mut network = self.network.lock().await;
                network.code = None;
                self.epoch.fetch_add(1, Ordering::SeqCst);
                self.changed.notify_one();
            }
            Request::SetPassword { username, password } => {
                let password = Zeroizing::new(password);
                self.bind(&mut *self.network.lock().await).await?;
                let access = self.access.clone();
                let changed = self.changed.clone();
                tokio::task::spawn_blocking(move || {
                    let result = access.set_password(&username, &password);
                    changed.notify_one();
                    result
                })
                .await??;
                self.changed.notify_one();
            }
            Request::DisablePassword => {
                self.access.disable_password()?;
                self.changed.notify_one();
            }
            Request::RevokeDevice { id } => self.access.revoke_device(&id)?,
            Request::RevokeAllDevices => self.access.revoke_all_devices()?,
        }
        let status = self.access.status();
        Ok(Response {
            error: None,
            listen: self.address.to_string(),
            fingerprint: self.pairing.fingerprint.clone(),
            username: status.username,
            devices: status
                .devices
                .into_iter()
                .map(|d| Device {
                    id: d.id,
                    name: d.name,
                })
                .collect(),
            pairing_open: self.network.lock().await.code.as_ref().is_some_and(|c| {
                c.deadline > tokio::time::Instant::now()
                    && c.attempts > 0
                    && c.generation == self.epoch.load(Ordering::SeqCst)
            }),
            code,
        })
    }
    async fn network(self: Arc<Self>) -> Result<()> {
        loop {
            let listener = {
                let mut network = self.network.lock().await;
                if network.code.as_ref().is_some_and(|c| {
                    c.deadline <= tokio::time::Instant::now()
                        || c.attempts == 0
                        || c.generation != self.epoch.load(Ordering::SeqCst)
                }) {
                    network.code = None;
                }
                if network.code.is_none() && self.access.status().username.is_none() {
                    network.listener = None;
                } else {
                    self.bind(&mut network).await?;
                }
                network.listener.clone()
            };
            let Some(listener) = listener else {
                self.changed.notified().await;
                continue;
            };
            let accepted = tokio::select! { biased; _ = self.changed.notified() => continue, result = listener.accept() => result?, _ = tokio::time::sleep(Duration::from_secs(1)) => continue };
            let (mut stream, _) = accepted;
            // A single bounded enrollment exchange at a time limits resource use.
            let exchange = tokio::time::timeout(Duration::from_secs(10), async {
                let mut magic = [0; 8];
                loop {
                    let n = stream.peek(&mut magic).await?;
                    ensure!(n > 0, "closed enrollment connection");
                    if n == 8 {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
                if &magic == b"TPAUTH01" {
                    crate::access::handle(stream, self.access.clone()).await?;
                } else if &magic == b"TPPAIR01" {
                    let attempt = {
                        let mut network = self.network.lock().await;
                        let code = network.code.as_mut().context("pairing closed")?;
                        ensure!(
                            code.attempts > 0 && code.deadline > tokio::time::Instant::now(),
                            "pairing expired"
                        );
                        code.attempts -= 1;
                        code.clone()
                    };
                    let mut used = false;
                    let result = crate::pairing::accept_with_access(
                        &mut stream,
                        &attempt.value,
                        &self.pairing,
                        &mut used,
                        attempt.deadline,
                        Some(crate::pairing::DeviceEnrollment {
                            access: &self.access,
                            epoch: &self.epoch,
                            generation: attempt.generation,
                        }),
                    )
                    .await;
                    result?;
                }
                Ok::<_, anyhow::Error>(())
            });
            tokio::select! { _ = exchange => (), _ = self.changed.notified() => () }
        }
    }
}
pub async fn start(
    directory: &Path,
    address: std::net::SocketAddr,
    pairing: crate::protocol::Pairing,
    access: Arc<crate::access::ServerState>,
) -> Result<(Service, Arc<Control>)> {
    use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
    check_directory(directory)?;
    let path = directory.join("admin.sock");
    let (_directory, address_path) = socket_address(directory)?;
    if let Ok(metadata) = std::fs::symlink_metadata(&path) {
        ensure!(
            metadata.file_type().is_socket() && metadata.uid() == unsafe { libc::geteuid() },
            "unsafe existing admin socket"
        );
        match std::os::unix::net::UnixStream::connect(&address_path) {
            Err(error) if error.raw_os_error() == Some(libc::ECONNREFUSED) => (),
            Err(error) => return Err(error).context("cannot check existing admin socket"),
            Ok(_) => anyhow::bail!("another host owns this identity admin socket"),
        }
        std::fs::remove_file(&path)?;
    }
    let listener = UnixListener::bind(&address_path)?;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    let socket_metadata = std::fs::symlink_metadata(&path)?;
    let control = Arc::new(Control {
        access,
        pairing,
        address,
        network: Mutex::new(Network {
            listener: None,
            code: None,
        }),
        changed: Arc::new(Notify::new()),
        epoch: AtomicU64::new(0),
    });
    if control.access.status().username.is_some() {
        control.bind(&mut *control.network.lock().await).await?;
    }
    let admin = control.clone();
    let task = tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                break;
            };
            if !stream
                .peer_cred()
                .is_ok_and(|c| c.uid() == unsafe { libc::geteuid() })
            {
                continue;
            }
            let _ = tokio::time::timeout(Duration::from_secs(15), async {
                let length = stream.read_u32().await? as usize;
                ensure!(length <= 8192, "admin request too large");
                let mut data = Zeroizing::new(vec![0; length]);
                stream.read_exact(&mut data).await?;
                let parsed = serde_json::from_slice(&data);
                data.zeroize();
                let result = match parsed {
                    Ok(request) => admin.request(request).await,
                    Err(_) => anyhow::bail!("invalid admin request"),
                };
                let response = result.unwrap_or_else(|e| Response {
                    error: Some(e.to_string()),
                    ..Default::default()
                });
                let data = serde_json::to_vec(&response)?;
                ensure!(data.len() <= 65536, "admin response too large");
                stream.write_u32(data.len() as u32).await?;
                stream.write_all(&data).await?;
                Ok::<_, anyhow::Error>(())
            })
            .await;
        }
    });
    let network = control.clone();
    let network_task = tokio::spawn(async move {
        if let Err(error) = network.network().await {
            tracing::warn!(%error, "enrollment listener stopped");
        }
    });
    Ok((
        Service {
            tasks: vec![task, network_task],
            path,
            inode: socket_metadata.ino(),
            device: socket_metadata.dev(),
        },
        control,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn local_admin_and_revocable_code_enrollment() -> Result<()> {
        let directory = tempfile::tempdir()?;
        std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700))?;
        let pairing = crate::protocol::Pairing {
            token: "a".repeat(64),
            fingerprint: "b".repeat(64),
        };
        let access = crate::access::ServerState::open(directory.path(), pairing.clone())?;
        let reservation = TcpListener::bind("127.0.0.1:0").await?;
        let address = reservation.local_addr()?;
        drop(reservation);
        let (_service, control) =
            start(directory.path(), address, pairing.clone(), access.clone()).await?;
        assert_eq!(
            std::fs::metadata(directory.path().join("admin.sock"))?.mode() & 0o777,
            0o600
        );
        assert!(tokio::net::TcpStream::connect(address).await.is_err());
        let path = directory.path().to_owned();
        let status = tokio::task::spawn_blocking(move || call(&path, &Request::Status)).await??;
        assert!(status.username.is_none());
        assert!(status.devices.is_empty());
        let first = control.request(Request::OpenPairing).await?;
        let mut stalled = tokio::net::TcpStream::connect(address).await?;
        stalled.write_all(b"TPPAIR01").await?;
        tokio::time::sleep(Duration::from_millis(50)).await;
        control.request(Request::ClosePairing).await?;
        let mut byte = [0];
        let closed = tokio::time::timeout(Duration::from_secs(1), stalled.read(&mut byte)).await?;
        assert!(matches!(closed, Ok(0) | Err(_)));
        assert!(access.status().devices.is_empty());
        let response = control.request(Request::OpenPairing).await?;
        assert!(
            crate::pairing::pair(&address.to_string(), first.code.as_deref().unwrap())
                .await
                .is_err()
        );
        assert!(control.request(Request::Status).await?.pairing_open);
        let device =
            crate::pairing::pair(&address.to_string(), response.code.as_deref().unwrap()).await?;
        assert_ne!(device.token, pairing.token);
        assert!(access.authorized(&device.token));
        assert_eq!(access.status().devices.len(), 1);
        let id = access.status().devices[0].id.clone();
        control.request(Request::RevokeDevice { id }).await?;
        assert!(!access.authorized(&device.token));
        assert!(access.authorized(&pairing.token));
        assert!(!control.request(Request::Status).await?.pairing_open);
        Ok(())
    }

    #[tokio::test]
    async fn socket_drop_does_not_remove_replacement() -> Result<()> {
        let directory = tempfile::tempdir()?;
        std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700))?;
        let pairing = crate::protocol::Pairing {
            token: "a".repeat(64),
            fingerprint: "b".repeat(64),
        };
        let access = crate::access::ServerState::open(directory.path(), pairing.clone())?;
        let (service, _) = start(directory.path(), "127.0.0.1:0".parse()?, pairing, access).await?;
        let path = directory.path().join("admin.sock");
        std::fs::remove_file(&path)?;
        let replacement = std::os::unix::net::UnixListener::bind(&path)?;
        drop(service);
        assert!(path.exists());
        drop(replacement);
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn long_identity_path_supports_local_control() -> Result<()> {
        let temporary = tempfile::tempdir()?;
        let directory = temporary.path().join("long-identity-directory-".repeat(6));
        std::fs::create_dir(&directory)?;
        std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700))?;
        let pairing = crate::protocol::Pairing {
            token: "a".repeat(64),
            fingerprint: "b".repeat(64),
        };
        let access = crate::access::ServerState::open(&directory, pairing.clone())?;
        let (_service, _) = start(&directory, "127.0.0.1:0".parse()?, pairing, access).await?;
        let response =
            tokio::task::spawn_blocking(move || call(&directory, &Request::Status)).await??;
        assert!(response.devices.is_empty());
        Ok(())
    }

    #[test]
    fn rejects_public_identity_directory() -> Result<()> {
        let directory = tempfile::tempdir()?;
        std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o755))?;
        assert!(check_directory(directory.path()).is_err());
        Ok(())
    }
}
