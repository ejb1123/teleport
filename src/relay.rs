//! Relay mode: reach a host through a MoQ relay instead of a direct UDP path.
//!
//! Both the host and the client connect *out* to the relay, so the host needs
//! no reachable port. Nothing the relay forwards is readable or forgeable by
//! it: every session is authenticated and sealed end to end by `crate::e2e`
//! and `crate::bridge`.
//!
//! Layout inside the relay namespace (derived from the host's pinned
//! fingerprint, so a client needs only the relay URL):
//!
//! * `<session>/hello`   client → host: `e2e::ClientHello`
//! * `<session>/reply`   host → client: `e2e::HostReply`
//! * `<session>/confirm` client → host: sealed key confirmation (and device
//!   signature), see `e2e::PendingSession`
//! * `<session>/controls` client → host: sealed input/pointer/agent tracks
//! * `<session>/desktop`  host → client: sealed metadata/video/audio/updates
//!
//! `<session>` is 128 random bits chosen by the client per connection.
//! Enrollment (pairing, password, system login) uses `<attempt>/request` and
//! `<attempt>/response` streams, here or on a pairing rendezvous namespace;
//! see `crate::relay_enroll`.
//!
//! `teleport relay` runs a minimal self-hosted relay for testing or for users
//! who prefer their own server to a third-party relay such as Cloudflare's.

use crate::{
    e2e::{self, ClientHandshake, Direction, SessionKeys},
    protocol,
};
use anyhow::{Context, Result};
use clap::Args;
use std::{sync::Arc, time::Duration};

const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(15);
/// Host-side bounds on unauthenticated handshake work.
#[cfg(target_os = "linux")]
const HELLO_READ_TIMEOUT: Duration = Duration::from_secs(2);
#[cfg(target_os = "linux")]
const CONFIRM_TIMEOUT: Duration = Duration::from_secs(5);
#[cfg(target_os = "linux")]
const MAX_PENDING_HELLOS: usize = 8;
#[cfg(target_os = "linux")]
const REPLAY_CACHE: usize = 4096;

#[derive(Args, Clone)]
pub struct Options {
    /// UDP address to accept QUIC relay connections on.
    #[arg(long, default_value = "0.0.0.0:4444")]
    pub listen: String,
    /// Directory holding a persistent relay TLS identity (created if absent),
    /// so clients can keep pinning the same relay fingerprint.
    #[arg(long)]
    pub identity_dir: Option<std::path::PathBuf>,
    /// MoQ versions to accept (repeatable). Defaults to moq-lite-05 and IETF
    /// MoQ Transport drafts 16 and 14.
    #[arg(long = "version", hide = true)]
    pub versions: Vec<String>,
    /// Shared secret every host and client must present (`--relay-key`).
    /// Without it anyone who can reach the relay may join a namespace and
    /// observe its traffic sizes and timing (never its content).
    #[arg(long, env = "TELEPORT_RELAY_KEY", hide_env_values = true)]
    pub key: Option<String>,
    /// UDP address of the built-in STUN responder (Binding only) that hosts
    /// and clients use to learn their public address for peer-to-peer
    /// connections; `off` disables it. Default: the --listen address, port
    /// 3478.
    #[arg(long, value_name = "ADDRESS|off")]
    pub stun_listen: Option<String>,
}

/// Start the relay's STUN responder (see `crate::stun`). A port already in
/// use is logged, not fatal: peer-to-peer then falls back to the relay.
async fn start_stun(options: &Options) -> Result<()> {
    let address = match options.stun_listen.as_deref() {
        Some("off") => return Ok(()),
        Some(address) => address
            .parse::<std::net::SocketAddr>()
            .with_context(|| format!("invalid --stun-listen {address}"))?,
        None => {
            let listen = tokio::net::lookup_host(options.listen.as_str())
                .await?
                .next()
                .context("--listen did not resolve")?;
            std::net::SocketAddr::new(listen.ip(), crate::stun::DEFAULT_PORT)
        }
    };
    match tokio::net::UdpSocket::bind(address).await {
        Ok(socket) => {
            tracing::info!(%address, "STUN responder ready (peer-to-peer address discovery)");
            tokio::spawn(async move {
                if let Err(error) = crate::stun::serve(socket).await {
                    tracing::warn!(%error, "STUN responder stopped");
                }
            });
        }
        Err(error) => {
            tracing::warn!(%address, %error, "STUN responder unavailable; peer-to-peer clients need another --stun server")
        }
    }
    Ok(())
}

/// Run a relay. Each connection is scoped to its URL path, so unrelated hosts
/// sharing a relay never see each other's broadcasts.
pub async fn serve(options: Options) -> Result<()> {
    let mut config = moq_native::ServerConfig::default();
    config.bind = Some(options.listen.clone());
    config.version = versions(&options.versions)?;
    config.quic.idle_timeout = Some(protocol::QUIC_IDLE_TIMEOUT);
    config.quic.keep_alive = Some(protocol::QUIC_KEEP_ALIVE);
    config.quic.mtu_discovery = Some(protocol::QUIC_MTU_DISCOVERY);
    match options.identity_dir.as_deref() {
        Some(directory) => {
            let identity = crate::identity::open(directory)?;
            config.tls.cert = vec![identity.certificate];
            config.tls.key = vec![identity.key];
        }
        None => config.tls.generate = vec!["teleport-relay".into()],
    }
    let mut server = config.init()?;
    let fingerprint = server
        .certificates()
        .fingerprints()
        .first()
        .context("no relay certificate")?
        .clone();
    tracing::info!(address = %server.local_addr()?, %fingerprint, "relay ready; clients and hosts pass --relay-fingerprint to pin it");
    // Printed for scripts and tests that need the fingerprint before logs flush.
    println!("RELAY FINGERPRINT {fingerprint}");
    start_stun(&options).await?;
    let origin = moq_net::Origin::random().produce();
    while let Some(request) = server.accept().await {
        let namespace = request.path().trim_matches('/').to_owned();
        let presented = url::form_urlencoded::parse(request.query().unwrap_or_default().as_bytes())
            .find(|(name, _)| name == "key")
            .map(|(_, value)| value.into_owned())
            .unwrap_or_default();
        let admitted = options.key.as_ref().is_none_or(|key| {
            use subtle::ConstantTimeEq;
            bool::from(key.as_bytes().ct_eq(presented.as_bytes()))
        });
        if !admitted {
            let path = request.path().to_owned();
            let _ = request.close(403).await;
            tracing::warn!(
                path,
                presented_len = presented.len(),
                "rejected relay connection without the relay key"
            );
            continue;
        }
        if !valid_namespace(&namespace) {
            let _ = request.close(404).await;
            continue;
        }
        let Some(scoped) = origin.with_root(namespace.as_str()) else {
            let _ = request.close(404).await;
            continue;
        };
        tokio::spawn(async move {
            match request
                .with_publisher(&scoped)
                .with_subscriber(scoped.clone())
                .ok()
                .await
            {
                Ok(session) => {
                    let error = session.closed().await;
                    tracing::debug!(%error, "relay session closed");
                }
                Err(error) => tracing::debug!(%error, "relay session rejected"),
            }
        });
    }
    Ok(())
}

fn versions(requested: &[String]) -> Result<Vec<moq_net::Version>> {
    let defaults: Vec<String> = protocol::RELAY_VERSIONS
        .iter()
        .map(|v| v.to_string())
        .collect();
    let requested = if requested.is_empty() {
        &defaults[..]
    } else {
        requested
    };
    requested
        .iter()
        .map(|version| version.parse().map_err(anyhow::Error::msg))
        .collect()
}

pub(crate) fn valid_namespace(namespace: &str) -> bool {
    namespace.len() == 32 && namespace.bytes().all(|b| b.is_ascii_hexdigit())
}

pub(crate) fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// Relay namespace for a host: derived from its pinned certificate
/// fingerprint so clients need no extra configuration, but not equal to it.
pub fn namespace(fingerprint: &str) -> Result<String> {
    let fingerprint = moq_native::tls::parse_fingerprint(fingerprint)
        .map_err(|error| anyhow::anyhow!("invalid host fingerprint: {error}"))?;
    let mut context = ring::digest::Context::new(&ring::digest::SHA256);
    context.update(b"teleport-relay-namespace-v1");
    context.update(&fingerprint);
    Ok(hex(&context.finish().as_ref()[..16]))
}

/// How a client reaches a host that also has a relay configured.
#[derive(
    Clone,
    Copy,
    Debug,
    Default,
    PartialEq,
    Eq,
    serde::Serialize,
    serde::Deserialize,
    clap::ValueEnum,
)]
#[serde(rename_all = "lowercase")]
pub enum Route {
    /// Try the direct address briefly, then the relay.
    #[default]
    Auto,
    /// Only the direct address.
    Direct,
    /// Only the relay (always end-to-end encrypted).
    Relay,
}

impl Route {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Direct => "direct",
            Self::Relay => "relay",
        }
    }
}

/// How to reach a relay.
#[derive(Clone)]
pub struct Endpoint {
    /// e.g. `moqt://relay.example:4444` (self-hosted) or a provider's
    /// `https://` WebTransport URL; `?jwt=` carries a provider token.
    pub url: url::Url,
    /// Pin a self-hosted relay's certificate; `None` uses system roots.
    pub fingerprint: Option<String>,
    /// Access key for a relay started with `--key`.
    pub key: Option<String>,
}

/// Relay options for the enrollment commands (`pair`, `login`,
/// `login-system`), matching `teleport client`'s.
#[derive(Args, Clone, Default)]
pub struct RelayArgs {
    /// Enroll through this MoQ relay instead of TCP to the host (the host must
    /// run with the same --relay). The address is then only the profile name.
    #[arg(long)]
    pub relay: Option<url::Url>,
    /// Pin a self-hosted relay's certificate (printed by `teleport relay`).
    #[arg(long, requires = "relay")]
    pub relay_fingerprint: Option<String>,
    /// Access key for a relay started with `teleport relay --key`.
    #[arg(
        long,
        requires = "relay",
        env = "TELEPORT_RELAY_KEY",
        hide_env_values = true
    )]
    pub relay_key: Option<String>,
}

impl RelayArgs {
    pub fn endpoint(&self) -> Option<Endpoint> {
        self.relay.clone().map(|url| Endpoint {
            url,
            fingerprint: self.relay_fingerprint.clone(),
            key: self.relay_key.clone(),
        })
    }
}

impl Endpoint {
    /// The URL without its query, which may carry a provider token.
    pub fn display_url(&self) -> String {
        let mut url = self.url.clone();
        url.set_query(None);
        url.to_string()
    }
}

impl std::fmt::Debug for Endpoint {
    // Never print the access key or a provider token in the URL query.
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Endpoint")
            .field("url", &self.display_url())
            .field("fingerprint", &self.fingerprint)
            .field("key", &self.key.as_ref().map(|_| "<redacted>"))
            .finish()
    }
}

/// One connection to a relay, scoped to a host namespace.
pub struct Link {
    pub session: moq_net::Session,
    /// Broadcasts we publish to the relay.
    pub outgoing: moq_net::origin::Producer,
    /// Broadcasts the relay announces to us.
    pub incoming: moq_net::origin::Producer,
}

pub async fn connect(endpoint: &Endpoint, namespace: &str) -> Result<Link> {
    connect_with_keep_alive(endpoint, namespace, protocol::QUIC_KEEP_ALIVE).await
}

/// Keep-alive for idle watchers (presence): below the relay's idle timeout,
/// with a third of the packets of a session link.
pub const IDLE_KEEP_ALIVE: Duration = Duration::from_secs(3);

pub async fn connect_with_keep_alive(
    endpoint: &Endpoint,
    namespace: &str,
    keep_alive: Duration,
) -> Result<Link> {
    let mut url = endpoint.url.clone();
    url.set_path(&format!("/{namespace}"));
    if let Some(key) = &endpoint.key {
        url.query_pairs_mut().append_pair("key", key);
    }
    let mut config = moq_native::ClientConfig::default();
    config.version = versions(&[])?;
    config.quic.idle_timeout = Some(protocol::QUIC_IDLE_TIMEOUT);
    config.quic.keep_alive = Some(keep_alive);
    config.quic.mtu_discovery = Some(protocol::QUIC_MTU_DISCOVERY);
    if let Some(fingerprint) = &endpoint.fingerprint {
        config.tls.fingerprint = vec![fingerprint.clone()];
    }
    let outgoing = moq_net::Origin::random().produce();
    let incoming = moq_net::Origin::random().produce();
    let session = config
        .init()?
        .with_publisher(&outgoing)
        .with_subscriber(incoming.clone())
        .connect(url)
        .await
        .context("relay connection failed")?;
    Ok(Link {
        session,
        outgoing,
        incoming,
    })
}

/// Publish one message; the returned handles must outlive its reader.
pub(crate) async fn publish_one(
    origin: &moq_net::origin::Producer,
    path: &str,
    payload: Vec<u8>,
) -> Result<(moq_net::broadcast::Producer, moq_net::track::Producer)> {
    publish(origin, path, vec![payload]).await
}

/// Publish a message of several frames in one group. Readers that expect one
/// frame (older hosts reading a confirmation) see only the first.
pub(crate) async fn publish(
    origin: &moq_net::origin::Producer,
    path: &str,
    frames: Vec<Vec<u8>>,
) -> Result<(moq_net::broadcast::Producer, moq_net::track::Producer)> {
    let mut broadcast =
        origin.create_broadcast(path, moq_net::broadcast::Route::new().with_announce(true))?;
    let mut track = broadcast.create_track(
        "message",
        moq_net::track::Info::default().with_ordered(true),
    )?;
    let mut group = track.append_group()?;
    for frame in frames {
        group.write_frame(moq_net::Timestamp::now(), bytes::Bytes::from(frame))?;
    }
    group.finish()?;
    Ok((broadcast, track))
}

pub(crate) async fn read_one(broadcast: &moq_net::broadcast::Consumer) -> Result<bytes::Bytes> {
    read_frames(broadcast, 1)
        .await?
        .pop()
        .context("empty handshake message")
}

/// The single-frame message the peer announces at `path` on this link.
pub(crate) async fn read_announced(link: &Link, path: &str) -> Result<bytes::Bytes> {
    let announced = link.incoming.consume();
    tokio::select! {
        broadcast = announced.announced_broadcast(path) => {
            read_one(&broadcast.context("relay closed before the host answered")?).await
        }
        error = link.session.closed() => {
            anyhow::bail!("relay closed the connection: {error} (check --relay-key)")
        }
    }
}

/// Up to the first `count` frames of a message (extra frames are ignored;
/// fewer means the sender finished the message early).
async fn read_frames(
    broadcast: &moq_net::broadcast::Consumer,
    count: usize,
) -> Result<Vec<bytes::Bytes>> {
    let mut track = broadcast
        .track("message")?
        .subscribe(moq_net::track::Subscription::default().with_group_start(0))
        .await?;
    let mut group = track
        .recv_group()
        .await?
        .context("handshake message missing")?;
    let mut frames = Vec::with_capacity(count);
    while frames.len() < count {
        match protocol::read_frame(&mut group, protocol::MAX_CONTROL).await? {
            Some(frame) => frames.push(frame),
            None => break,
        }
    }
    Ok(frames)
}

/// Client side: plaintext origins the unchanged session code uses, bridged to
/// the relay after an authenticated handshake.
pub struct ClientSession {
    pub link: Link,
    /// Publish `controls` here (plaintext; sealed by the bridge).
    pub outgoing: moq_net::origin::Producer,
    /// `desktop` is announced here (opened by the bridge).
    pub incoming: moq_net::origin::Producer,
    _bridges: Vec<AbortOnDrop>,
    _handshake: (
        (moq_net::broadcast::Producer, moq_net::track::Producer),
        (moq_net::broadcast::Producer, moq_net::track::Producer),
    ),
}

pub struct AbortOnDrop(pub tokio::task::JoinHandle<Result<()>>);
impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

pub async fn client(
    endpoint: &Endpoint,
    pairing: &protocol::Pairing,
    request: &str,
) -> Result<ClientSession> {
    let link = connect(endpoint, &namespace(&pairing.fingerprint)?).await?;
    let session_id = hex(&e2e::random::<16>());
    let device = pairing
        .device_key
        .as_deref()
        .map(e2e::DeviceKey::from_hex)
        .transpose()?;
    let handshake = ClientHandshake::new(&pairing.token, request, device)?;
    let hello = publish_one(
        &link.outgoing,
        &format!("{session_id}/hello"),
        serde_json::to_vec(handshake.hello())?,
    )
    .await?;
    let replies = link.incoming.consume();
    let reply_path = format!("{session_id}/reply");
    let reply = tokio::time::timeout(HANDSHAKE_TIMEOUT, async {
        tokio::select! {
            broadcast = replies.announced_broadcast(reply_path.as_str()) => {
                let broadcast = broadcast.context("relay closed before the host replied")?;
                read_one(&broadcast).await
            }
            error = link.session.closed() => {
                anyhow::bail!("relay closed the connection: {error} (check --relay-key)")
            }
        }
    })
    .await
    .context("host did not answer through the relay; is it running with the same --relay and is this device still authorized?")??;
    let reply: e2e::HostReply = serde_json::from_slice(&reply).context("malformed host reply")?;
    let established = handshake.finish(&reply, &pairing.fingerprint)?;
    let keys = Arc::new(established.keys);
    // Key confirmation (plus the device signature for keyed credentials): the
    // host starts nothing (no lease, no capture) until this authenticates, so
    // a replayed hello costs it no more than a reply.
    let confirm = publish(
        &link.outgoing,
        &format!("{session_id}/confirm"),
        established.confirmation,
    )
    .await?;
    tracing::info!(session = %hex(&keys.session_id()), "end-to-end encrypted relay session established");
    let outgoing = moq_net::Origin::random().produce();
    let incoming = moq_net::Origin::random().produce();
    let export = {
        let local = outgoing.consume();
        let network = link.outgoing.create_broadcast(
            format!("{session_id}/controls").as_str(),
            moq_net::broadcast::Route::new().with_announce(true),
        )?;
        tokio::spawn(crate::bridge::export(
            async move {
                local
                    .announced_broadcast("controls")
                    .await
                    .context("local controls missing")
            },
            network,
            keys.clone(),
            Direction::ClientToHost,
        ))
    };
    let import = {
        let network_origin = link.incoming.consume();
        let local = incoming.create_broadcast(
            "desktop",
            moq_net::broadcast::Route::new().with_announce(true),
        )?;
        let path = format!("{session_id}/desktop");
        tokio::spawn(crate::bridge::import(
            async move {
                network_origin
                    .announced_broadcast(path.as_str())
                    .await
                    .context("host desktop missing on relay")
            },
            local,
            keys.clone(),
            Direction::HostToClient,
        ))
    };
    Ok(ClientSession {
        link,
        outgoing,
        incoming,
        _bridges: vec![AbortOnDrop(export), AbortOnDrop(import)],
        _handshake: (hello, confirm),
    })
}

/// A relayed client that has proven it holds the session keys. Nothing is
/// leased or captured for a client before it reaches this state.
#[cfg(target_os = "linux")]
pub struct Accepted {
    pub session_id: String,
    pub request: String,
    /// The credential the hello named, as `SHA-256(token)`.
    pub credential: [u8; 32],
    /// The device public key whose transcript signature the client proved,
    /// if the host stored one for the credential.
    pub device_key: Option<Vec<u8>>,
    pub keys: Arc<SessionKeys>,
    _reply: (moq_net::broadcast::Producer, moq_net::track::Producer),
}

/// Host side: turn relay announcements into confirmed sessions.
///
/// Unauthenticated work is bounded: hellos are read concurrently (at most
/// [`MAX_PENDING_HELLOS`] at once, each within [`HELLO_READ_TIMEOUT`]), a
/// client ephemeral key is answered at most once, unknown credentials get no
/// reply, and a client must prove the session keys (and, for a device with a
/// stored public key, sign the transcript with its device key) within
/// [`CONFIRM_TIMEOUT`] before it is handed to the session loop. With
/// `require_device_key`, credentials the host's records alone would
/// authenticate (the host pairing token and devices without a key) get no
/// reply at all. Ends when the relay connection does.
///
/// With `p2p`, peer-to-peer signalling hellos (`<attempt>/p2p-hello`, see
/// `crate::p2p`) go through the same bounds and checks, under their own
/// domain-separated credential; once confirmed they exchange candidates and
/// never become a session here.
#[cfg(target_os = "linux")]
#[allow(clippy::too_many_arguments)]
pub async fn intake(
    session: moq_net::Session,
    incoming: moq_net::origin::Producer,
    outgoing: moq_net::origin::Producer,
    access: Arc<crate::access::ServerState>,
    identity: Arc<e2e::HostIdentity>,
    accepted: tokio::sync::mpsc::Sender<Accepted>,
    require_device_key: bool,
    p2p: Option<Arc<crate::p2p::Host>>,
) -> Result<()> {
    use crate::p2p::paths;
    let mut announced = incoming.consume().announced();
    let slots = Arc::new(tokio::sync::Semaphore::new(MAX_PENDING_HELLOS));
    // Client ephemerals already answered (bounded, oldest evicted first).
    let seen = Arc::new(std::sync::Mutex::new((
        std::collections::HashSet::<Vec<u8>>::new(),
        std::collections::VecDeque::<Vec<u8>>::new(),
    )));
    let mut handlers = tokio::task::JoinSet::new();
    loop {
        while handlers.try_join_next().is_some() {}
        // A relay that refuses us (wrong --relay-key) closes the MoQ session
        // after the transport connected; notice it instead of waiting forever.
        let announce = tokio::select! {
            announce = announced.next() => announce.context("relay connection closed")?,
            error = session.closed() => anyhow::bail!("relay closed the connection: {error} (check --relay-key)"),
        };
        let Some(broadcast) = announce.broadcast else {
            continue;
        };
        let path = announce.path.to_string();
        let signal_hello = format!("/{}", paths::HELLO);
        let (session_id, signalling) = match (
            path.strip_suffix("/hello"),
            path.strip_suffix(signal_hello.as_str()),
        ) {
            (Some(session_id), _) => (session_id, false),
            (None, Some(session_id)) if p2p.is_some() => (session_id, true),
            _ => continue,
        };
        if !valid_namespace(session_id) {
            continue;
        }
        let Ok(permit) = slots.clone().try_acquire_owned() else {
            tracing::warn!("too many pending relay handshakes; ignoring hello");
            continue;
        };
        let session_id = session_id.to_owned();
        let (incoming, outgoing, access, identity, accepted, seen, p2p) = (
            incoming.clone(),
            outgoing.clone(),
            access.clone(),
            identity.clone(),
            accepted.clone(),
            seen.clone(),
            p2p.clone(),
        );
        handlers.spawn(async move {
            let _permit = permit;
            let hello: e2e::ClientHello =
                match tokio::time::timeout(HELLO_READ_TIMEOUT, read_one(&broadcast)).await {
                    Ok(Ok(bytes)) => match serde_json::from_slice(&bytes) {
                        Ok(hello) => hello,
                        Err(_) => return,
                    },
                    _ => return,
                };
            {
                let (set, order) = &mut *seen.lock().unwrap();
                if !set.insert(hello.ephemeral.clone()) {
                    tracing::warn!("ignoring replayed relay hello");
                    return;
                }
                order.push_back(hello.ephemeral.clone());
                if order.len() > REPLAY_CACHE
                    && let Some(old) = order.pop_front()
                {
                    set.remove(&old);
                }
            }
            // The request is bound into the transcript, so a relay cannot
            // move a hello between the session and signalling paths.
            if signalling != (hello.request == crate::p2p::SIGNAL_REQUEST) {
                return;
            }
            let mut candidates = access.relay_credentials();
            let hashes: Vec<[u8; 32]> = candidates
                .iter()
                .map(|c| {
                    if signalling {
                        crate::p2p::signal_credential(&c.hash)
                    } else {
                        c.hash
                    }
                })
                .collect();
            let Some(index) = e2e::HostIdentity::identify(&hello, &hashes) else {
                tracing::warn!("rejected unauthorized relayed client");
                return;
            };
            let candidate = candidates.swap_remove(index);
            if require_device_key && candidate.hash_only() {
                tracing::warn!(
                    "refused relayed credential without a device key (--relay-require-device-key); re-enroll the device"
                );
                return;
            }
            let (credential, device_key) = (candidate.hash, candidate.device_key);
            let Ok((reply, pending)) = identity.respond(&hello, &hashes[index]) else {
                return;
            };
            let Ok(reply) = serde_json::to_vec(&reply) else {
                return;
            };
            let (reply_path, confirm_path) = if signalling {
                (paths::REPLY, paths::CONFIRM)
            } else {
                ("reply", "confirm")
            };
            let Ok(reply) =
                publish_one(&outgoing, &format!("{session_id}/{reply_path}"), reply).await
            else {
                return;
            };
            let confirmed = tokio::time::timeout(CONFIRM_TIMEOUT, async {
                let broadcast = incoming
                    .consume()
                    .announced_broadcast(format!("{session_id}/{confirm_path}").as_str())
                    .await
                    .context("no confirmation")?;
                let objects = read_frames(
                    &broadcast,
                    e2e::PendingSession::expected_objects(device_key.as_deref()),
                )
                .await?;
                let objects: Vec<&[u8]> = objects.iter().map(|object| &object[..]).collect();
                pending.confirm(&objects, device_key.as_deref())
            })
            .await;
            let keys = match confirmed {
                Ok(Ok(keys)) => keys,
                Ok(Err(error)) => {
                    tracing::warn!(%error, "relayed client did not confirm the session");
                    return;
                }
                Err(_) => {
                    tracing::warn!("relayed client did not confirm the session in time");
                    return;
                }
            };
            if let Some(p2p) = p2p.filter(|_| signalling) {
                let grant = crate::p2p::Grant {
                    credential,
                    device_key,
                };
                if let Err(error) = p2p
                    .answer(&keys, &session_id, &incoming, &outgoing, grant)
                    .await
                {
                    tracing::info!(
                        error = format!("{error:#}"),
                        "peer-to-peer signalling failed"
                    );
                }
                return;
            }
            let _ = accepted
                .send(Accepted {
                    session_id,
                    request: hello.request,
                    credential,
                    device_key,
                    keys: Arc::new(keys),
                    _reply: reply,
                })
                .await;
        });
    }
}

/// Host side of a confirmed relayed session: plaintext origins for the
/// unchanged session code, bridged to the relay.
#[cfg(target_os = "linux")]
pub struct HostSession {
    /// Publish `desktop` here (plaintext; sealed by the bridge).
    pub outgoing: moq_net::origin::Producer,
    /// `controls` is announced here (opened by the bridge).
    pub incoming: moq_net::origin::Producer,
    _bridges: Vec<AbortOnDrop>,
}

#[cfg(target_os = "linux")]
pub fn host(link: &Link, accepted: &Accepted) -> Result<HostSession> {
    let keys = accepted.keys.clone();
    let session_id = &accepted.session_id;
    let outgoing = moq_net::Origin::random().produce();
    let incoming = moq_net::Origin::random().produce();
    let export = {
        let local = outgoing.consume();
        let network = link.outgoing.create_broadcast(
            format!("{session_id}/desktop").as_str(),
            moq_net::broadcast::Route::new().with_announce(true),
        )?;
        tokio::spawn(crate::bridge::export(
            async move {
                local
                    .announced_broadcast("desktop")
                    .await
                    .context("local desktop missing")
            },
            network,
            keys.clone(),
            Direction::HostToClient,
        ))
    };
    let import = {
        let network_origin = link.incoming.consume();
        let local = incoming.create_broadcast(
            "controls",
            moq_net::broadcast::Route::new().with_announce(true),
        )?;
        let path = format!("{session_id}/controls");
        tokio::spawn(crate::bridge::import(
            async move {
                network_origin
                    .announced_broadcast(path.as_str())
                    .await
                    .context("client controls missing on relay")
            },
            local,
            keys,
            Direction::ClientToHost,
        ))
    };
    Ok(HostSession {
        outgoing,
        incoming,
        _bridges: vec![AbortOnDrop(export), AbortOnDrop(import)],
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn namespace_is_stable_hex_and_not_the_fingerprint() {
        let fingerprint = "ab".repeat(32);
        let namespace = namespace(&fingerprint).unwrap();
        assert!(valid_namespace(&namespace));
        assert_eq!(namespace, super::namespace(&fingerprint).unwrap());
        assert!(!fingerprint.contains(&namespace));
        assert!(super::namespace("not hex").is_err());
    }
}
