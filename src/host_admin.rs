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
    SetPassword {
        username: String,
    },
    DisablePassword,
    /// Require password plus U2F touch for new password-enrolled devices.
    RequireU2f {
        credential_file: PathBuf,
    },
    DisableU2f,
    RevokeDevice {
        id: String,
    },
    RevokeAllDevices,
    /// Connected sessions, guests waiting for approval and open invites.
    Sessions,
    /// Invite guests: prints a link (as secret as a pairing code) that
    /// enrolls up to `--uses` guest devices until it expires.
    CreateInvite {
        /// Names the guest devices in prompts and lists.
        #[arg(long, default_value = "Guest")]
        name: String,
        #[arg(long, default_value_t = 1)]
        uses: u32,
        /// Lifetime in minutes (1 to 10080, a week).
        #[arg(long, default_value_t = 60, value_parser = clap::value_parser!(u64).range(1..=10080))]
        minutes: u64,
        /// What the guests may do besides watching: view, gamepad,
        /// keyboard-mouse, clipboard, microphone (comma separated).
        #[arg(long, default_value = "view,gamepad", value_parser = crate::protocol::Permissions::parse)]
        permissions: crate::protocol::Permissions,
    },
    RevokeInvite {
        id: String,
    },
    /// Let a waiting guest join.
    Approve {
        id: u64,
    },
    /// Refuse a waiting guest.
    Deny {
        id: u64,
    },
    /// Disconnect a session (a guest may ask again; revoke its device to
    /// keep it out).
    Kick {
        id: u64,
    },
    /// Change a guest device's permissions (applies to its running
    /// sessions at once) or its trusted flag (joins without approval).
    SetGuest {
        id: String,
        #[arg(long, value_parser = crate::protocol::Permissions::parse)]
        permissions: Option<crate::protocol::Permissions>,
        #[arg(long)]
        trusted: Option<bool>,
    },
    /// Take keyboard and mouse control for the person at the host.
    TakeControl,
    /// Let guests take keyboard and mouse control again.
    ReleaseControl,
}
#[derive(Serialize, Deserialize)]
pub enum Request {
    Status,
    OpenPairing,
    ClosePairing,
    SetPassword {
        username: String,
        password: String,
    },
    DisablePassword,
    RequireU2f {
        credential: crate::security_key::Credential,
    },
    DisableU2f,
    RevokeDevice {
        id: String,
    },
    RevokeAllDevices,
    Sessions,
    CreateInvite {
        name: String,
        uses: u32,
        /// Lifetime (5 s to a week).
        seconds: u64,
        permissions: crate::protocol::Permissions,
    },
    RevokeInvite {
        id: String,
    },
    AnswerGuest {
        id: u64,
        allow: bool,
    },
    Kick {
        id: u64,
    },
    SetGuest {
        id: String,
        permissions: Option<crate::protocol::Permissions>,
        trusted: Option<bool>,
    },
    TakeControl,
    ReleaseControl,
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
    #[serde(default)]
    pub u2f_required: bool,
    #[serde(default)]
    pub system_login: bool,
    pub devices: Vec<Device>,
    pub pairing_open: bool,
    pub code: Option<String>,
    /// With a relay: the pairing link (rendezvous and code; as secret as the code).
    #[serde(default)]
    pub relay_pairing: Option<String>,
    /// With a relay: this host's relay id (its namespace; not secret), used
    /// by `teleport login --relay-host`.
    #[serde(default)]
    pub relay_host: Option<String>,
    /// Connected sessions (docs/guests.md).
    #[serde(default)]
    pub sessions: Vec<crate::guests::SessionSummary>,
    /// Guests waiting for approval.
    #[serde(default)]
    pub approvals: Vec<crate::guests::ApprovalSummary>,
    /// Open invites (no secrets).
    #[serde(default)]
    pub invites: Vec<crate::access::InviteSummary>,
    /// A newly created invite's link: as secret as a pairing code.
    #[serde(default)]
    pub invite: Option<String>,
}
#[derive(Serialize, Deserialize)]
pub struct Device {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub source: String,
    /// Set for guest devices.
    #[serde(default)]
    pub guest: Option<crate::access::GuestGrant>,
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
        AdminCommand::RequireU2f { credential_file } => {
            let status = call(
                &options.identity_dir.clone().unwrap_or(default_directory()?),
                &Request::Status,
            )?;
            Request::RequireU2f {
                credential: crate::security_key::read_u2f_credential(
                    &credential_file,
                    &status.fingerprint,
                )?,
            }
        }
        AdminCommand::DisableU2f => Request::DisableU2f,
        AdminCommand::RevokeDevice { id } => Request::RevokeDevice { id },
        AdminCommand::RevokeAllDevices => Request::RevokeAllDevices,
        AdminCommand::Sessions => Request::Sessions,
        AdminCommand::CreateInvite {
            name,
            uses,
            minutes,
            permissions,
        } => Request::CreateInvite {
            name,
            uses,
            seconds: minutes.saturating_mul(60),
            permissions,
        },
        AdminCommand::RevokeInvite { id } => Request::RevokeInvite { id },
        AdminCommand::Approve { id } => Request::AnswerGuest { id, allow: true },
        AdminCommand::Deny { id } => Request::AnswerGuest { id, allow: false },
        AdminCommand::Kick { id } => Request::Kick { id },
        AdminCommand::SetGuest {
            id,
            permissions,
            trusted,
        } => Request::SetGuest {
            id,
            permissions,
            trusted,
        },
        AdminCommand::TakeControl => Request::TakeControl,
        AdminCommand::ReleaseControl => Request::ReleaseControl,
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
    /// Relay rendezvous serving the open code (see `crate::relay_enroll`).
    rendezvous: Option<(String, crate::relay::AbortOnDrop)>,
}
pub struct Control {
    access: Arc<crate::access::ServerState>,
    pairing: crate::protocol::Pairing,
    address: std::net::SocketAddr,
    network: Mutex<Network>,
    changed: Arc<Notify>,
    epoch: AtomicU64,
    system: Option<crate::system_login::Config>,
    /// Session ticket redemption (`crate::ticket`), when issuers are trusted.
    ticket: Option<crate::ticket::Config>,
    relay: Option<crate::relay::Endpoint>,
    hub: Arc<crate::guests::Hub>,
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
/// Rate-limit source for every relayed attempt: the relay hides client
/// addresses, so relayed logins share one bucket (plus the global limit).
const RELAY_SOURCE: std::net::IpAddr = std::net::IpAddr::V6(std::net::Ipv6Addr::UNSPECIFIED);

/// A newly opened one-time pairing code.
pub struct OpenedPairing {
    /// `123-456`, for direct (TCP) pairing.
    pub code: String,
    /// `teleport://relay/<rendezvous>/<code>` when the host uses a relay.
    pub relay_link: Option<String>,
}

/// Prefix the already-read protocol magic back onto a stream so handlers can
/// read it themselves.
struct Rewind<S> {
    magic: [u8; 8],
    offset: usize,
    inner: S,
}
impl<S: tokio::io::AsyncRead + Unpin> tokio::io::AsyncRead for Rewind<S> {
    fn poll_read(
        self: std::pin::Pin<&mut Self>,
        context: &mut std::task::Context<'_>,
        buffer: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        let this = self.get_mut();
        if this.offset < this.magic.len() {
            let size = (this.magic.len() - this.offset).min(buffer.remaining());
            buffer.put_slice(&this.magic[this.offset..this.offset + size]);
            this.offset += size;
            return std::task::Poll::Ready(Ok(()));
        }
        std::pin::Pin::new(&mut this.inner).poll_read(context, buffer)
    }
}
impl<S: tokio::io::AsyncWrite + Unpin> tokio::io::AsyncWrite for Rewind<S> {
    fn poll_write(
        self: std::pin::Pin<&mut Self>,
        context: &mut std::task::Context<'_>,
        buffer: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        std::pin::Pin::new(&mut self.get_mut().inner).poll_write(context, buffer)
    }
    fn poll_flush(
        self: std::pin::Pin<&mut Self>,
        context: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.get_mut().inner).poll_flush(context)
    }
    fn poll_shutdown(
        self: std::pin::Pin<&mut Self>,
        context: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.get_mut().inner).poll_shutdown(context)
    }
}

impl Control {
    pub async fn open_pairing(self: &Arc<Self>) -> Result<OpenedPairing> {
        let mut network = self.network.lock().await;
        self.bind(&mut network).await?;
        let value = format!("{:06}", rand::random_range(0..1_000_000u32));
        let printable = format!("{}-{}", &value[..3], &value[3..]);
        let deadline = tokio::time::Instant::now() + Duration::from_secs(300);
        let generation = self.epoch.fetch_add(1, Ordering::SeqCst).wrapping_add(1);
        // A new code always replaces the previous rendezvous.
        network.rendezvous = None;
        let relay_link = if let Some(endpoint) = &self.relay {
            let invite = crate::relay_enroll::Invite::new(&value)?;
            let task = tokio::spawn(self.clone().rendezvous(
                endpoint.clone(),
                invite.namespace(),
                generation,
                deadline,
            ));
            let link = invite.link();
            network.rendezvous = Some((link.clone(), crate::relay::AbortOnDrop(task)));
            Some(link)
        } else {
            None
        };
        network.code = Some(Code {
            value,
            deadline,
            attempts: 5,
            generation,
        });
        self.changed.notify_one();
        Ok(OpenedPairing {
            code: printable,
            relay_link,
        })
    }

    fn pairing_current(&self, generation: u64, deadline: tokio::time::Instant) -> bool {
        self.epoch.load(Ordering::SeqCst) == generation && tokio::time::Instant::now() < deadline
    }

    /// Serve the open code on its relay rendezvous until the code is used,
    /// closed, replaced or expires. Attempts share the code's five-attempt
    /// budget with direct TCP pairing.
    async fn rendezvous(
        self: Arc<Self>,
        endpoint: crate::relay::Endpoint,
        namespace: String,
        generation: u64,
        deadline: tokio::time::Instant,
    ) -> Result<()> {
        let closed = async {
            while self.pairing_current(generation, deadline) {
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
        };
        tokio::pin!(closed);
        loop {
            let connected = tokio::select! {
                connected = tokio::time::timeout(
                    Duration::from_secs(10),
                    crate::relay::connect_host(&endpoint, &namespace),
                ) => connected,
                _ = &mut closed => return Ok(()),
            };
            match connected {
                Ok(Ok(link)) => {
                    tracing::info!(relay = %endpoint.display_url(), "relay pairing open");
                    let control = Arc::downgrade(&self);
                    let serve = crate::relay_enroll::serve(
                        link.session.clone(),
                        link.incoming.clone(),
                        link.outgoing.clone(),
                        move |stream| {
                            let control = control.clone();
                            async move {
                                let Some(control) = control.upgrade() else {
                                    return;
                                };
                                if let Err(error) =
                                    control.exchange(stream, RELAY_SOURCE, true).await
                                {
                                    tracing::warn!(%error, "relayed pairing attempt rejected (code not logged)");
                                }
                            }
                        },
                    );
                    tokio::pin!(serve);
                    tokio::select! {
                        result = &mut serve => {
                            if let Err(error) = result {
                                tracing::warn!(%error, "relay pairing connection lost; reconnecting");
                            }
                        }
                        _ = &mut closed => {
                            // Accept nothing new, but keep the connection (and
                            // running exchanges) long enough to deliver a
                            // credential that was just issued.
                            tokio::time::sleep(Duration::from_secs(15)).await;
                            drop(link);
                            return Ok(());
                        }
                    }
                }
                Ok(Err(error)) => tracing::warn!(%error, "relay unavailable for pairing; retrying"),
                Err(_) => tracing::warn!("relay connection for pairing timed out; retrying"),
            }
            tokio::select! {
                _ = tokio::time::sleep(Duration::from_secs(2)) => (),
                _ = &mut closed => return Ok(()),
            }
        }
    }

    /// Accept enrollment on a relay connection to the host's own namespace:
    /// password (OPAQUE), system login (pinned TLS) and code pairing.
    pub async fn serve_relay(
        self: Arc<Self>,
        session: moq_net::Session,
        incoming: moq_net::origin::Producer,
        outgoing: moq_net::origin::Producer,
    ) -> Result<()> {
        let control = Arc::downgrade(&self);
        drop(self);
        crate::relay_enroll::serve(session, incoming, outgoing, move |stream| {
            let control = control.clone();
            async move {
                let Some(control) = control.upgrade() else {
                    return;
                };
                if let Err(error) = control.exchange(stream, RELAY_SOURCE, false).await {
                    tracing::warn!(%error, "relayed enrollment rejected");
                }
            }
        })
        .await
    }

    /// One enrollment exchange on any transport, dispatched by protocol magic.
    /// `pairing_only` restricts a pairing rendezvous to code pairing.
    async fn exchange(
        self: &Arc<Self>,
        mut stream: impl tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send,
        source: std::net::IpAddr,
        pairing_only: bool,
    ) -> Result<()> {
        let mut magic = [0; 8];
        tokio::time::timeout(Duration::from_secs(5), stream.read_exact(&mut magic))
            .await
            .context("enrollment protocol header timed out")??;
        let mut stream = Rewind {
            magic,
            offset: 0,
            inner: stream,
        };
        match &magic {
            b"TPAUTH01" if !pairing_only => {
                crate::access::handle(stream, source, self.access.clone()).await
            }
            b"TPSYS001" if !pairing_only => {
                let config = self.system.as_ref().context("system login is disabled")?;
                crate::system_login::handle(stream, source, config, self.access.clone()).await
            }
            b"TPTKT001" if !pairing_only => {
                let config = self
                    .ticket
                    .as_ref()
                    .context("session tickets are disabled")?;
                crate::ticket::handle(stream, source, config, self.access.clone()).await
            }
            b"TPINVT01" if !pairing_only => tokio::time::timeout(
                Duration::from_secs(10),
                crate::pairing::accept_invite(&mut stream, &self.access),
            )
            .await
            .context("invite exchange timed out")?,
            b"TPPAIR01" | b"TPPAIR02" => {
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
                tokio::time::timeout(
                    Duration::from_secs(10),
                    crate::pairing::accept_with_access(
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
                    ),
                )
                .await
                .context("pairing exchange timed out")?
            }
            _ => anyhow::bail!("unsupported enrollment protocol"),
        }
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
    async fn request(self: &Arc<Self>, request: Request) -> Result<Response> {
        use crate::guests::Command;
        let mut code = None;
        let mut relay_pairing = None;
        let mut invite = None;
        match request {
            Request::Sessions => (),
            Request::CreateInvite {
                name,
                uses,
                seconds,
                permissions,
            } => {
                let created = self.access.create_invite(
                    &name,
                    uses,
                    Duration::from_secs(seconds),
                    permissions,
                )?;
                // Direct enrollment needs the TCP port; relayed enrollment
                // uses the host's own relay namespace, named in the link.
                self.bind(&mut *self.network.lock().await).await?;
                self.changed.notify_one();
                invite = Some(match &self.relay {
                    Some(_) => format!(
                        "{}/{}",
                        created.link(),
                        crate::relay::namespace(&self.pairing.fingerprint)?
                    ),
                    None => created.link(),
                });
            }
            Request::RevokeInvite { id } => self.access.revoke_invite(&id)?,
            Request::AnswerGuest { id, allow } => self.hub.answer(id, allow)?,
            Request::Kick { id } => self.hub.command(Command::Kick(id))?,
            Request::SetGuest {
                id,
                permissions,
                trusted,
            } => {
                self.access.set_guest(&id, permissions, trusted)?;
                self.hub.poke(Command::Refresh);
            }
            Request::TakeControl => self.hub.command(Command::TakeControl)?,
            Request::ReleaseControl => self.hub.command(Command::ReleaseControl)?,
            Request::Status => (),
            Request::OpenPairing => {
                // A pairing code enrolls an owner device; guests watch the
                // screen it would be shown on.
                ensure!(
                    !self.hub.guest_connected(),
                    "a guest is connected and could read the pairing code off the screen; disconnect guests before pairing"
                );
                let opened = self.open_pairing().await?;
                code = Some(opened.code);
                relay_pairing = opened.relay_link;
            }
            Request::ClosePairing => {
                let mut network = self.network.lock().await;
                network.code = None;
                network.rendezvous = None;
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
            Request::RequireU2f { credential } => {
                self.access.require_u2f(credential)?;
                self.changed.notify_one();
            }
            Request::DisableU2f => {
                self.access.disable_u2f()?;
                self.changed.notify_one();
            }
            // Running sessions of revoked devices end now, not at the next
            // once-a-second check.
            Request::RevokeDevice { id } => {
                self.access.revoke_device(&id)?;
                self.hub.poke(Command::Refresh);
            }
            Request::RevokeAllDevices => {
                self.access.revoke_all_devices()?;
                self.hub.poke(Command::Refresh);
            }
        }
        let status = self.access.status();
        Ok(Response {
            error: None,
            listen: self.address.to_string(),
            fingerprint: self.pairing.fingerprint.clone(),
            username: status.username,
            u2f_required: status.u2f_required,
            system_login: self.system.is_some() && self.access.system_login_allowed(),
            devices: status
                .devices
                .into_iter()
                .map(|d| Device {
                    id: d.id,
                    name: d.name,
                    source: d.source,
                    guest: d.guest,
                })
                .collect(),
            pairing_open: self.network.lock().await.code.as_ref().is_some_and(|c| {
                c.deadline > tokio::time::Instant::now()
                    && c.attempts > 0
                    && c.generation == self.epoch.load(Ordering::SeqCst)
            }),
            code,
            relay_pairing,
            relay_host: self
                .relay
                .as_ref()
                .and_then(|_| crate::relay::namespace(&self.pairing.fingerprint).ok()),
            sessions: self.hub.sessions(),
            approvals: self.hub.approvals(),
            invites: self.access.invites(),
            invite,
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
                if network.code.is_none()
                    && self.access.status().username.is_none()
                    && self.system.is_none()
                    && self.access.invites().is_empty()
                {
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
            let (stream, peer) = accepted;
            // A single bounded enrollment exchange at a time limits resource use.
            let exchange = tokio::time::timeout(
                Duration::from_secs(95),
                self.exchange(stream, peer.ip(), false),
            );
            tokio::select! { _ = exchange => (), _ = self.changed.notified() => () }
        }
    }
}
#[cfg(test)]
pub async fn start(
    directory: &Path,
    address: std::net::SocketAddr,
    pairing: crate::protocol::Pairing,
    access: Arc<crate::access::ServerState>,
) -> Result<(Service, Arc<Control>)> {
    start_with_system(
        directory,
        address,
        pairing,
        access,
        None,
        None,
        crate::guests::Hub::new(Default::default()),
    )
    .await
}

pub async fn start_with_system(
    directory: &Path,
    address: std::net::SocketAddr,
    pairing: crate::protocol::Pairing,
    access: Arc<crate::access::ServerState>,
    helper: Option<PathBuf>,
    relay: Option<crate::relay::Endpoint>,
    hub: Arc<crate::guests::Hub>,
) -> Result<(Service, Arc<Control>)> {
    use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
    check_directory(directory)?;
    let system = helper
        .map(|path| crate::system_login::Config::new(directory, path))
        .transpose()?;
    let ticket = access
        .tickets_enabled()
        .then(|| crate::ticket::Config::new(directory))
        .transpose()?;
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
            rendezvous: None,
        }),
        changed: Arc::new(Notify::new()),
        epoch: AtomicU64::new(0),
        system,
        ticket,
        relay,
        hub,
    });
    if control.access.status().username.is_some()
        || control.system.is_some()
        || control.ticket.is_some()
        || !control.access.invites().is_empty()
    {
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
            device_key: None,
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
        // A relay in front of the port that drops the device key (sent after
        // the code proof) gets nothing issued, and the code stays usable.
        let (client, mut relayed, meddler) =
            crate::access::meddler::spawn(|up, index, _| match up && index == 3 {
                true => crate::access::meddler::Action::HangUp,
                false => crate::access::meddler::Action::Forward,
            });
        let mut tcp = tokio::net::TcpStream::connect(address).await?;
        let bridge =
            tokio::spawn(
                async move { tokio::io::copy_bidirectional(&mut relayed, &mut tcp).await },
            );
        assert!(
            crate::pairing::pair_over(client, response.code.as_deref().unwrap(), true)
                .await
                .is_err()
        );
        meddler.abort();
        bridge.abort();
        assert!(access.status().devices.is_empty());
        assert!(control.request(Request::Status).await?.pairing_open);
        let device =
            crate::pairing::pair(&address.to_string(), response.code.as_deref().unwrap()).await?;
        assert_ne!(device.token, pairing.token);
        assert!(access.authorized(&device.token));
        // The code-paired device registered a relay key; the host holds only
        // its public half, so its hash alone cannot open a relayed session.
        let public =
            crate::e2e::DeviceKey::from_hex(device.device_key.as_deref().unwrap())?.public_key();
        let hash = crate::e2e::credential_hash(&device.token);
        assert!(
            access
                .relay_credentials()
                .iter()
                .any(|c| c.hash == hash && c.device_key.as_deref() == Some(&public[..]))
        );
        assert!(access.begin_session_hash(hash.to_vec(), None).is_err());
        // A client from before device keys still enrolls (hash-only).
        let response = control.request(Request::OpenPairing).await?;
        let old = crate::pairing::pair_without_device_key(
            &address.to_string(),
            response.code.as_deref().unwrap(),
        )
        .await?;
        assert!(old.device_key.is_none() && access.authorized(&old.token));
        let old_hash = crate::e2e::credential_hash(&old.token);
        assert!(
            access
                .relay_credentials()
                .iter()
                .any(|c| c.hash == old_hash && c.hash_only())
        );
        let id = access.status().devices[1].id.clone();
        control.request(Request::RevokeDevice { id }).await?;
        assert!(!access.authorized(&old.token));
        assert_eq!(access.status().devices.len(), 1);
        assert_eq!(access.status().devices.len(), 1);
        let id = access.status().devices[0].id.clone();
        control.request(Request::RevokeDevice { id }).await?;
        assert!(!access.authorized(&device.token));
        assert!(access.authorized(&pairing.token));
        assert!(!control.request(Request::Status).await?.pairing_open);
        Ok(())
    }

    /// Password enrollment and code pairing purely through a real (in-process)
    /// relay, with no TCP reachable by the client.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    #[ignore = "requires local UDP sockets"]
    async fn password_and_code_enrollment_through_a_relay() -> Result<()> {
        use crate::relay_enroll::{self, Invite};
        use tokio_rustls::rustls::pki_types::{CertificateDer, pem::PemObject};
        const PASSWORD: &str = "Relay test passphrase, not production!";
        let directory = tempfile::tempdir()?;
        std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700))?;
        // A self-hosted relay with a pinned certificate and an access key.
        let relay_identity = directory.path().join("relay");
        let certificate = crate::identity::open(&relay_identity)?.certificate;
        let der = CertificateDer::from_pem_file(&certificate)?;
        let relay_fingerprint =
            crate::relay::hex(ring::digest::digest(&ring::digest::SHA256, der.as_ref()).as_ref());
        let socket = std::net::UdpSocket::bind("127.0.0.1:0")?;
        let relay_address = socket.local_addr()?;
        drop(socket);
        let relay = tokio::spawn(crate::relay::serve(crate::relay::Options {
            listen: relay_address.to_string(),
            identity_dir: Some(relay_identity),
            versions: Vec::new(),
            key: Some("relay key".into()),
            stun_listen: Some("off".into()),
            no_namespace_discovery: false,
        }));
        let endpoint = crate::relay::Endpoint {
            url: format!("moqt://{relay_address}").parse()?,
            fingerprint: Some(relay_fingerprint),
            key: Some("relay key".into()),
        };
        // The host: password set, enrollment served on its relay namespace.
        let host = directory.path().join("host");
        std::fs::create_dir(&host)?;
        std::fs::set_permissions(&host, std::fs::Permissions::from_mode(0o700))?;
        let pairing = crate::protocol::Pairing {
            token: "a".repeat(64),
            fingerprint: "b".repeat(64),
            device_key: None,
        };
        let access = crate::access::ServerState::open(&host, pairing.clone())?;
        access.set_password("ej", PASSWORD)?;
        let (_service, control) = start_with_system(
            &host,
            "127.0.0.1:0".parse()?,
            pairing.clone(),
            access.clone(),
            None,
            Some(endpoint.clone()),
            crate::guests::Hub::new(Default::default()),
        )
        .await?;
        let namespace = crate::relay::namespace(&pairing.fingerprint)?;
        let link = crate::relay::connect_host(&endpoint, &namespace).await?;
        let _enrollment = tokio::spawn(control.clone().serve_relay(
            link.session.clone(),
            link.incoming.clone(),
            link.outgoing.clone(),
        ));
        let status = control.request(Request::Status).await?;
        assert_eq!(status.relay_host.as_deref(), Some(namespace.as_str()));
        // OPAQUE through the relay; a wrong password or host id gets nothing.
        assert!(
            relay_enroll::login(
                &endpoint,
                &namespace,
                "ej",
                "Wrong passphrase, rejected!",
                "x"
            )
            .await
            .is_err()
        );
        let device =
            relay_enroll::login(&endpoint, &namespace, "ej", PASSWORD, "relay device").await?;
        assert!(access.authorized(&device.token));
        assert_eq!(device.fingerprint, pairing.fingerprint);
        // Code pairing on the rendezvous the opened code names.
        let opened = control.request(Request::OpenPairing).await?;
        let link = opened.relay_pairing.expect("relay pairing link");
        let invite = Invite::parse(&link)?;
        assert_eq!(invite.code, opened.code.unwrap().replace('-', ""));
        let wrong = Invite {
            code: if invite.code == "000000" {
                "000001"
            } else {
                "000000"
            }
            .into(),
            ..invite.clone()
        };
        // The rendezvous connects asynchronously; requests wait for it.
        assert!(relay_enroll::pair(&endpoint, &wrong).await.is_err());
        let paired = relay_enroll::pair(&endpoint, &invite).await?;
        assert!(access.authorized(&paired.token));
        assert_eq!(access.status().devices.len(), 2);
        // Both enrolled with their device keys, through the relay.
        assert!(device.device_key.is_some() && paired.device_key.is_some());
        let host_hash = crate::e2e::credential_hash(&pairing.token);
        assert!(
            access
                .relay_credentials()
                .iter()
                .all(|c| c.hash == host_hash || !c.hash_only())
        );
        assert!(!control.request(Request::Status).await?.pairing_open);
        // Single use.
        assert!(relay_enroll::pair(&endpoint, &invite).await.is_err());
        relay.abort();
        Ok(())
    }

    #[tokio::test]
    async fn socket_drop_does_not_remove_replacement() -> Result<()> {
        let directory = tempfile::tempdir()?;
        std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700))?;
        let pairing = crate::protocol::Pairing {
            token: "a".repeat(64),
            fingerprint: "b".repeat(64),
            device_key: None,
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
            device_key: None,
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
