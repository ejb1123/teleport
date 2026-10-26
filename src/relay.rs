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
//! On a moq-lite relay (`teleport relay`) the connection URL's path is the
//! namespace and peers find each other's broadcasts through the relay's
//! announcements. A moq-transport relay (a provider such as Cloudflare) keeps
//! the URL path for its own use (an access token), so the namespace is the
//! first segment of every track namespace instead, and peers announce their
//! broadcasts to each other themselves (`crate::discovery`).
//!
//! `teleport relay` runs a minimal self-hosted relay for testing or for users
//! who prefer their own server to a third-party relay such as Cloudflare's.

use crate::{
    e2e::{self, ClientHandshake, Direction},
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
    #[arg(long, conflicts_with = "tls_cert")]
    pub identity_dir: Option<std::path::PathBuf>,
    /// A certificate chain (PEM) from a CA, for example Let's Encrypt for
    /// the relay's DNS name: what browsers need for WebTransport
    /// (docs/relay.md#browsers). Clients then verify it with system roots
    /// instead of pinning --relay-fingerprint.
    #[arg(long, requires = "tls_key")]
    pub tls_cert: Option<std::path::PathBuf>,
    /// The private key (PEM) for --tls-cert.
    #[arg(long, requires = "tls_cert")]
    pub tls_key: Option<std::path::PathBuf>,
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
    /// Tests: announce nothing to moq-transport peers (answer
    /// SUBSCRIBE_NAMESPACE with an empty set) while still routing SUBSCRIBEs
    /// by name, as Cloudflare's relay does.
    #[arg(long, hide = true)]
    pub no_namespace_discovery: bool,
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
    match (
        options.identity_dir.as_deref(),
        &options.tls_cert,
        &options.tls_key,
    ) {
        (_, Some(cert), Some(key)) => {
            config.tls.cert = vec![cert.clone()];
            config.tls.key = vec![key.clone()];
        }
        (Some(directory), _, _) => {
            let identity = crate::identity::open(directory)?;
            config.tls.cert = vec![identity.certificate];
            config.tls.key = vec![identity.key];
        }
        // Short-lived (14 days) ECDSA P-256: what a browser accepts through
        // WebTransport serverCertificateHashes.
        _ => config.tls.generate = vec!["teleport-relay".into(), "localhost".into()],
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
        // Without discovery the session's publisher sees an origin that
        // announces nothing but resolves every exact path from the real one.
        let (published, resolver) = if options.no_namespace_discovery {
            let hidden = moq_net::Origin::random().produce();
            let resolver = resolve_unannounced(hidden.dynamic(), scoped.consume());
            (hidden, Some(resolver))
        } else {
            (scoped.clone(), None)
        };
        tokio::spawn(async move {
            match request
                .with_publisher(&published)
                .with_subscriber(scoped.clone())
                .ok()
                .await
            {
                Ok(session) => {
                    let error = match resolver {
                        Some(resolver) => tokio::select! {
                            error = session.closed() => error,
                            () = resolver => session.closed().await,
                        },
                        None => session.closed().await,
                    };
                    tracing::debug!(%error, "relay session closed");
                }
                Err(error) => tracing::debug!(%error, "relay session rejected"),
            }
        });
    }
    Ok(())
}

/// Serve requests for broadcasts that `requests`' origin never announces from
/// `source`, by exact path (`--no-namespace-discovery`).
async fn resolve_unannounced(
    mut requests: moq_net::origin::Dynamic,
    source: moq_net::origin::Consumer,
) {
    while let Ok(request) = requests.requested_broadcast().await {
        let path = request.path().to_owned();
        match source.request_broadcast(&path).await {
            Ok(broadcast) => request.accept(broadcast),
            Err(error) => request.reject(error),
        }
    }
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
    /// The URL without its path and query, which may carry a provider token
    /// (Cloudflare's goes in the path).
    pub fn display_url(&self) -> String {
        let mut url = self.url.clone();
        url.set_query(None);
        if !url.path().trim_matches('/').is_empty() {
            url.set_path("/redacted");
        }
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
    /// Broadcasts the relay (or, on a provider relay, the peer) announces to us.
    pub incoming: moq_net::origin::Producer,
    /// Peer announcements on a moq-transport relay (`crate::discovery`).
    discovery: Option<crate::discovery::Discovery>,
    /// One handle per relayed host session using this connection.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    sessions: Arc<()>,
}

/// moq-transport relays need not raise a connection's request limit:
/// Cloudflare's grants request ids below 100, i.e. 50 client requests, and
/// never more. Past this many a host connection is replaced once idle.
const PROVIDER_REQUEST_BUDGET: u64 = 30;
/// A worn host connection is replaced only after this long without a peer
/// announcing anything (a handshake or enrollment in progress).
#[cfg(target_os = "linux")]
const RENEW_QUIET: Duration = Duration::from_secs(10);

impl Link {
    /// A moq-transport (provider) relay: no announcements, no datagrams.
    pub fn provider(&self) -> bool {
        self.discovery.is_some()
    }

    /// Whether this connection has used most of what a provider relay grants
    /// it (see [`PROVIDER_REQUEST_BUDGET`]); a long-lived user reconnects.
    pub fn worn(&self) -> bool {
        self.discovery
            .as_ref()
            .is_some_and(|discovery| discovery.requests() >= PROVIDER_REQUEST_BUDGET)
    }

    /// Host side: whether a worn connection can be replaced now, without
    /// cutting off a session or a handshake (no peer announced anything for a
    /// while).
    #[cfg(target_os = "linux")]
    pub fn renewable(&self) -> bool {
        self.worn()
            && Arc::strong_count(&self.sessions) == 1
            && self
                .discovery
                .as_ref()
                .is_some_and(|discovery| discovery.quiet() >= RENEW_QUIET)
    }
}

/// Connect as a client of the host whose namespace this is.
pub async fn connect(endpoint: &Endpoint, namespace: &str) -> Result<Link> {
    connect_as(
        endpoint,
        namespace,
        crate::discovery::Role::Client,
        protocol::QUIC_KEEP_ALIVE,
    )
    .await
}

/// Connect as the host that owns this namespace (or pairing rendezvous).
#[cfg(target_os = "linux")]
pub async fn connect_host(endpoint: &Endpoint, namespace: &str) -> Result<Link> {
    connect_as(
        endpoint,
        namespace,
        crate::discovery::Role::Host,
        protocol::QUIC_KEEP_ALIVE,
    )
    .await
}

/// Keep-alive for idle watchers (presence): below the relay's idle timeout,
/// with a third of the packets of a session link.
pub const IDLE_KEEP_ALIVE: Duration = Duration::from_secs(3);

pub async fn connect_with_keep_alive(
    endpoint: &Endpoint,
    namespace: &str,
    keep_alive: Duration,
) -> Result<Link> {
    connect_as(
        endpoint,
        namespace,
        crate::discovery::Role::Client,
        keep_alive,
    )
    .await
}

/// The URL to dial: a moq-lite relay takes the namespace as its path, while a
/// provider URL that has a path of its own (a token) keeps it.
fn dial_url(endpoint: &Endpoint, namespace: &str) -> url::Url {
    let mut url = endpoint.url.clone();
    if url.path().trim_matches('/').is_empty() {
        url.set_path(&format!("/{namespace}"));
    }
    if let Some(key) = &endpoint.key {
        url.query_pairs_mut().append_pair("key", key);
    }
    url
}

async fn connect_as(
    endpoint: &Endpoint,
    namespace: &str,
    role: crate::discovery::Role,
    keep_alive: Duration,
) -> Result<Link> {
    let url = dial_url(endpoint, namespace);
    let mut config = moq_native::ClientConfig::default();
    // `TELEPORT_RELAY_VERSIONS=moq-transport-14,...` pins what is offered, for
    // provider relays whose drafts differ in behaviour.
    let pinned: Vec<String> = std::env::var("TELEPORT_RELAY_VERSIONS")
        .map(|value| {
            value
                .split(',')
                .map(|version| version.trim().to_owned())
                .filter(|version| !version.is_empty())
                .collect()
        })
        .unwrap_or_default();
    config.version = versions(&pinned)?;
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
    if session.version().is_lite() {
        return Ok(Link {
            session,
            outgoing,
            incoming,
            discovery: None,
            sessions: Arc::default(),
        });
    }
    // moq-transport: the namespace travels in band and peers announce
    // themselves (see the module documentation).
    let outgoing = outgoing
        .with_root(namespace)
        .context("invalid relay namespace")?;
    let (incoming, discovery) =
        crate::discovery::start(session.clone(), role, namespace, outgoing.clone(), incoming)?;
    tracing::debug!(version = ?session.version(), "moq-transport relay: peers announce themselves");
    Ok(Link {
        session,
        outgoing,
        incoming,
        discovery: Some(discovery),
        sessions: Arc::default(),
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
    pub keys: Arc<e2e::SessionKeys>,
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
    /// Turns true once the client's controls broadcast is gone from the
    /// relay (the client left); the relay connection itself is shared.
    pub gone: tokio::sync::watch::Receiver<bool>,
    _bridges: Vec<AbortOnDrop>,
    /// Counts this session against the connection (`Link::renewable`).
    _uses: Arc<()>,
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
    // The client leaving: its controls broadcast ends on the relay.
    let (left, gone) = tokio::sync::watch::channel(false);
    let watcher = {
        let network_origin = link.incoming.consume();
        let path = format!("{session_id}/controls");
        tokio::spawn(async move {
            if let Some(controls) = network_origin.announced_broadcast(path.as_str()).await {
                controls.closed().await;
            }
            let _ = left.send(true);
            Ok(())
        })
    };
    Ok(HostSession {
        outgoing,
        incoming,
        gone,
        _bridges: vec![
            AbortOnDrop(export),
            AbortOnDrop(import),
            AbortOnDrop(watcher),
        ],
        _uses: link.sessions.clone(),
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

    #[test]
    fn provider_tokens_keep_their_path_and_stay_out_of_logs() {
        let namespace = "0123456789abcdef0123456789abcdef";
        let endpoint = |url: &str, key: Option<&str>| Endpoint {
            url: url.parse().unwrap(),
            fingerprint: None,
            key: key.map(str::to_owned),
        };
        // A self-hosted relay is scoped by the namespace as its path.
        let own = endpoint("moqt://relay.example:4444", Some("secret"));
        assert_eq!(
            dial_url(&own, namespace).as_str(),
            format!("moqt://relay.example:4444/{namespace}?key=secret")
        );
        // A provider token in the path is kept; the namespace goes in band.
        let cloudflare = endpoint(
            "https://draft-16.cloudflare.mediaoverquic.com/eyJtoken",
            None,
        );
        assert_eq!(
            dial_url(&cloudflare, namespace).as_str(),
            "https://draft-16.cloudflare.mediaoverquic.com/eyJtoken"
        );
        let jwt = endpoint("https://relay.example/?jwt=eyJtoken", None);
        for endpoint in [cloudflare, jwt] {
            let shown = format!("{} {endpoint:?}", endpoint.display_url());
            assert!(!shown.contains("eyJtoken"), "{shown}");
        }
        assert_eq!(own.display_url(), "moqt://relay.example:4444");
    }
}
