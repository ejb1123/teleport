//! Direct peer-to-peer connections across NATs, signalled through the relay.
//!
//! ICE-lite style hole punching for `--route auto`:
//!
//! 1. **Candidates.** Each side owns one UDP socket for both STUN and QUIC
//!    ([`Endpoint`]): a custom `quinn::AsyncUdpSocket` hands every datagram
//!    that carries the STUN magic cookie (and so lacks QUIC's fixed bit) to a
//!    STUN agent and everything else to quinn. The reflexive address STUN
//!    reports is therefore the NAT mapping QUIC itself will use. Candidates
//!    are the socket's interface addresses (`host`), the STUN-reported
//!    address (`srflx`) and, with `--upnp`, a NAT-PMP mapping (`mapped`).
//! 2. **Signalling** runs through the relay as an end-to-end handshake of its
//!    own (`e2e`), domain-separated from sessions: the credential identifier
//!    and keys use `signal_credential(SHA-256(token))`, so an old host never
//!    recognises the hello and a relay cannot turn a signalling exchange into
//!    a session or back. Candidates travel only sealed under the resulting
//!    keys (bound to the signed transcript): the relay neither reads nor
//!    injects them, and only a client that completed the handshake (including
//!    its device signature) learns the host's addresses.
//! 3. **Connectivity.** The host sends a few STUN indications from its QUIC
//!    socket to the client's candidates, opening its NAT for them, then
//!    answers. The client dials every host candidate from its own QUIC socket
//!    (staggered) and keeps the first QUIC handshake that verifies the pinned
//!    certificate. The session on top is an ordinary direct session: the
//!    credential rides the MoQ SETUP path and the host authorizes and leases
//!    it exactly as for its main listener. Hole punching changes
//!    reachability, never authentication.
//!
//! The host's peer-to-peer socket ignores QUIC connection attempts unless a
//! signalling exchange completed within the last [`ARM_WINDOW`].
//!
//! Capability gating: a client signals only to hosts that run with a relay;
//! a host that does not answer (older, or `--no-p2p`) costs the client at
//! most [`SIGNAL_TIMEOUT`], spent while the direct address is tried anyway.

use crate::{
    e2e::{self, Direction, Order, SessionKeys},
    protocol, relay, stun,
};
use anyhow::{Context, Result, anyhow, bail, ensure};
use clap::Args;
use moq_native::quinn::web_transport_quinn::{self, quinn};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    fmt,
    io::{self, IoSliceMut},
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    pin::Pin,
    sync::{Arc, Mutex},
    task::{Context as TaskContext, Poll, ready},
    time::{Duration, Instant},
};

/// Relay signalling, including relay connection and STUN, before the client
/// gives up on the peer-to-peer path. Runs alongside the direct attempt.
pub const SIGNAL_TIMEOUT: Duration = Duration::from_millis(2500);
/// QUIC dials to the host's candidates after the answer arrived.
pub const DIAL_TIMEOUT: Duration = Duration::from_millis(1500);
/// Upper bound of one peer-to-peer attempt (`--route auto` waits for it).
pub const ATTEMPT_TIMEOUT: Duration =
    Duration::from_millis(SIGNAL_TIMEOUT.as_millis() as u64 + DIAL_TIMEOUT.as_millis() as u64);
/// Host: how long after a signalling exchange its socket accepts QUIC.
const ARM_WINDOW: Duration = Duration::from_secs(30);
/// Host: wait for the client's sealed candidates after its confirmation.
#[cfg(target_os = "linux")]
const OFFER_TIMEOUT: Duration = Duration::from_secs(2);
/// Host: keep the answer published for the client to read.
#[cfg(target_os = "linux")]
const ANSWER_HOLD: Duration = Duration::from_secs(10);
/// Host: QUIC handshake plus MoQ SETUP on the peer-to-peer socket.
#[cfg(target_os = "linux")]
const ACCEPT_TIMEOUT: Duration = Duration::from_secs(5);
#[cfg(target_os = "linux")]
const MAX_PENDING_ACCEPTS: usize = 4;
/// Reuse a NAT-PMP mapping this long before asking the gateway again.
const MAPPING_REUSE: Duration = Duration::from_secs(1800);
const STUN_TIMEOUT: Duration = Duration::from_millis(700);
const STUN_RETRANSMIT: Duration = Duration::from_millis(200);
/// Spacing between QUIC dials to successive candidates.
const DIAL_STAGGER: Duration = Duration::from_millis(30);
/// Host punch times (ms after the offer): dense first, then sparse while the
/// client may still be dialing.
#[cfg(target_os = "linux")]
const PUNCH_SCHEDULE_MS: [u64; 11] = [0, 20, 50, 100, 200, 400, 700, 1000, 1500, 2000, 3000];
/// Candidates accepted from a peer (the rest of the message is rejected).
const MAX_CANDIDATES: usize = 8;
const MAX_HOST_CANDIDATES: usize = 5;
/// The client's hello `request` for a signalling exchange (bound into the
/// transcript; sessions never use it).
pub const SIGNAL_REQUEST: &str = "teleport-p2p-v1";
const SIGNAL_LABEL: &[u8] = b"teleport-p2p-signal-v1";
const OFFER_TRACK: &str = "p2p-offer";
const ANSWER_TRACK: &str = "p2p-answer";
/// SNI for direct QUIC; the pinned fingerprint authenticates, not the name.
const SERVER_NAME: &str = "teleport.local";

/// Candidate gathering options shared by host and client.
#[derive(Args, Clone, Debug, Default)]
pub struct GatherArgs {
    /// STUN server (host:port) for the peer-to-peer reflexive address; may
    /// repeat. Default: the self-hosted relay's built-in responder
    /// (`moqt://` relay host, port 3478). No public server is used unless
    /// named here.
    #[arg(long = "stun", value_name = "HOST:PORT")]
    pub stun: Vec<String>,
    /// Do not use STUN (peer-to-peer then offers only local and mapped
    /// addresses).
    #[arg(long, conflicts_with = "stun")]
    pub no_stun: bool,
    /// Opt in to asking the default gateway for a UDP port mapping (NAT-PMP)
    /// for the peer-to-peer socket; removed when Teleport exits.
    #[arg(long)]
    pub upnp: bool,
}

/// `teleport client` options for the peer-to-peer path.
#[derive(Args, Clone, Debug, Default)]
pub struct ClientArgs {
    /// Never try a peer-to-peer path through relay signalling (`--route
    /// auto` then goes from the direct address straight to the relay).
    #[arg(long)]
    pub no_p2p: bool,
    #[command(flatten)]
    pub gather: GatherArgs,
}

impl ClientArgs {
    pub fn enabled(&self) -> bool {
        !self.no_p2p
    }
}

/// `teleport host` options for the peer-to-peer path (needs `--relay`).
#[derive(Args, Clone, Debug)]
pub struct HostArgs {
    /// Do not answer peer-to-peer signalling from relay clients.
    #[arg(long)]
    pub no_p2p: bool,
    /// UDP address of the peer-to-peer QUIC socket. It accepts connections
    /// only shortly after an authenticated signalling exchange, with the same
    /// credential checks as --listen.
    #[arg(long, default_value = "[::]:0")]
    pub p2p_listen: String,
    #[command(flatten)]
    pub gather: GatherArgs,
}

/// How a candidate address was learned.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    /// An address of one of the peer's own interfaces.
    Host,
    /// The peer's public address as a STUN server saw it.
    Srflx,
    /// A port mapping the peer's gateway granted (NAT-PMP).
    Mapped,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Host => "host",
            Self::Srflx => "srflx",
            Self::Mapped => "mapped",
        }
    }
}

impl fmt::Display for Kind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Candidate {
    pub kind: Kind,
    pub address: SocketAddr,
}

/// Sealed signalling message (both directions).
#[derive(Serialize, Deserialize)]
struct Candidates {
    version: u32,
    candidates: Vec<Candidate>,
}

/// Whether an address can be a peer's candidate at all: a unicast address
/// that is neither unspecified, loopback, multicast, broadcast, reserved, nor
/// IPv6 link-local (unusable without a scope), with a non-zero port.
pub fn valid_address(address: SocketAddr) -> bool {
    if address.port() == 0 {
        return false;
    }
    match address.ip().to_canonical() {
        IpAddr::V4(ip) => {
            !(ip.is_unspecified()
                || ip.is_loopback()
                || ip.is_multicast()
                || ip.is_broadcast()
                || ip.is_link_local()
                || ip.octets()[0] == 0
                || ip.octets()[0] >= 240)
        }
        IpAddr::V6(ip) => {
            !(ip.is_unspecified()
                || ip.is_loopback()
                || ip.is_multicast()
                || (ip.segments()[0] & 0xffc0) == 0xfe80)
        }
    }
}

/// Validate a peer's candidate list: too many entries reject the message;
/// unusable addresses are dropped; duplicates removed; IPv4-mapped IPv6
/// addresses canonicalised.
pub fn validate(candidates: Vec<Candidate>) -> Result<Vec<Candidate>> {
    ensure!(
        candidates.len() <= MAX_CANDIDATES,
        "too many peer-to-peer candidates ({})",
        candidates.len()
    );
    let mut valid: Vec<Candidate> = Vec::with_capacity(candidates.len());
    for candidate in candidates {
        let address = stun::canonical(candidate.address);
        if !valid_address(address) {
            tracing::debug!(%address, kind = %candidate.kind, "ignoring unusable peer-to-peer candidate");
            continue;
        }
        if valid.iter().any(|known| known.address == address) {
            continue;
        }
        valid.push(Candidate {
            kind: candidate.kind,
            address,
        });
    }
    Ok(valid)
}

/// The credential hash used for signalling: distinct from `SHA-256(token)`,
/// so a signalling hello is never a valid session hello (nor the reverse),
/// even for hosts that predate peer-to-peer support.
pub fn signal_credential(credential_hash: &[u8; 32]) -> [u8; 32] {
    let key = ring::hmac::Key::new(ring::hmac::HMAC_SHA256, credential_hash);
    ring::hmac::sign(&key, SIGNAL_LABEL)
        .as_ref()
        .try_into()
        .expect("HMAC-SHA-256 is 32 bytes")
}

fn seal_candidates(
    keys: &SessionKeys,
    direction: Direction,
    track: &str,
    candidates: &[Candidate],
) -> Result<Vec<u8>> {
    keys.sealer(direction, track)?.seal(
        0,
        &serde_json::to_vec(&Candidates {
            version: 1,
            candidates: candidates.to_vec(),
        })?,
    )
}

fn open_candidates(
    keys: &SessionKeys,
    direction: Direction,
    track: &str,
    sealed: &[u8],
) -> Result<Vec<Candidate>> {
    let plain = keys
        .opener(direction, track, Order::Strict)?
        .open(0, sealed)
        .context("peer-to-peer candidates failed authentication")?;
    let message: Candidates =
        serde_json::from_slice(&plain).context("malformed peer-to-peer candidates")?;
    ensure!(
        message.version == 1,
        "unsupported peer-to-peer candidates version"
    );
    validate(message.candidates)
}

// ── Shared socket ───────────────────────────────────────────────────

/// Outstanding STUN transactions: the server asked, and who waits.
type Pending = HashMap<stun::TransactionId, (SocketAddr, tokio::sync::oneshot::Sender<SocketAddr>)>;

/// One UDP socket for QUIC and STUN. Quinn sees only non-STUN datagrams.
#[derive(Debug)]
struct Demux {
    inner: Arc<dyn quinn::AsyncUdpSocket>,
    pending: Mutex<Pending>,
    ipv6: bool,
}

impl Demux {
    /// A STUN datagram that arrived on the QUIC socket: complete the matching
    /// transaction if it came from the server that was asked. Indications
    /// (the peer's punches) and anything unsolicited are dropped.
    fn divert(&self, packet: &[u8], from: SocketAddr) {
        let Some(message) = stun::parse(packet) else {
            return;
        };
        if message.class != stun::Class::Success {
            return;
        }
        let mut pending = self.pending.lock().unwrap();
        if pending
            .get(&message.transaction)
            .is_some_and(|(server, _)| *server == stun::canonical(from))
            && let Some((_, waiter)) = pending.remove(&message.transaction)
            && let Some(mapped) = message.mapped
        {
            let _ = waiter.send(stun::canonical(mapped));
        }
    }

    /// The destination as this socket's family needs it (IPv4-mapped on a
    /// dual-stack socket); `None` if unreachable from it.
    fn destination(&self, address: SocketAddr) -> Option<SocketAddr> {
        match (address, self.ipv6) {
            (SocketAddr::V4(v4), true) => Some(SocketAddr::new(
                IpAddr::V6(v4.ip().to_ipv6_mapped()),
                v4.port(),
            )),
            (SocketAddr::V6(_), false) => None,
            _ => Some(address),
        }
    }

    async fn send_raw(
        self: &Arc<Self>,
        destination: SocketAddr,
        contents: &[u8],
    ) -> io::Result<()> {
        let Some(destination) = self.destination(destination) else {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "address family not available on this socket",
            ));
        };
        let transmit = quinn::udp::Transmit {
            destination,
            ecn: None,
            contents,
            segment_size: None,
            src_ip: None,
        };
        let mut poller = None;
        loop {
            match self.inner.try_send(&transmit) {
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    let poller =
                        poller.get_or_insert_with(|| self.inner.clone().create_io_poller());
                    std::future::poll_fn(|cx| poller.as_mut().poll_writable(cx)).await?;
                }
                result => return result,
            }
        }
    }
}

impl quinn::AsyncUdpSocket for Demux {
    fn create_io_poller(self: Arc<Self>) -> Pin<Box<dyn quinn::UdpPoller>> {
        self.inner.clone().create_io_poller()
    }

    fn try_send(&self, transmit: &quinn::udp::Transmit) -> io::Result<()> {
        self.inner.try_send(transmit)
    }

    fn poll_recv(
        &self,
        cx: &mut TaskContext,
        bufs: &mut [IoSliceMut<'_>],
        meta: &mut [quinn::udp::RecvMeta],
    ) -> Poll<io::Result<usize>> {
        loop {
            let count = ready!(self.inner.poll_recv(cx, bufs, meta))?;
            let mut kept = 0;
            for index in 0..count {
                let entry = meta[index];
                // A GRO batch (several segments) is only ever QUIC: STUN
                // messages are never coalesced with each other here.
                if entry.stride >= entry.len && stun::looks_like_stun(&bufs[index][..entry.len]) {
                    self.divert(&bufs[index][..entry.len], entry.addr);
                    continue;
                }
                if kept != index {
                    let (front, back) = bufs.split_at_mut(index);
                    if front[kept].len() < entry.len {
                        continue;
                    }
                    front[kept][..entry.len].copy_from_slice(&back[0][..entry.len]);
                    meta[kept] = entry;
                }
                kept += 1;
            }
            if kept > 0 {
                return Poll::Ready(Ok(kept));
            }
        }
    }

    fn local_addr(&self) -> io::Result<SocketAddr> {
        self.inner.local_addr()
    }

    fn max_transmit_segments(&self) -> usize {
        self.inner.max_transmit_segments()
    }

    fn max_receive_segments(&self) -> usize {
        self.inner.max_receive_segments()
    }

    fn may_fragment(&self) -> bool {
        self.inner.may_fragment()
    }
}

/// A quinn endpoint on a socket that also speaks STUN.
pub struct Endpoint {
    quic: quinn::Endpoint,
    socket: Arc<Demux>,
    /// Opt-in gateway mapping of this socket's port, deleted on drop.
    mapping: Mutex<Option<(Instant, crate::natpmp::Mapping)>>,
}

impl Endpoint {
    /// Bind `address` (dual-stack for `[::]`), falling back to IPv4 when an
    /// unspecified IPv6 bind fails (IPv6 disabled).
    fn bind(address: SocketAddr, server: Option<quinn::ServerConfig>) -> Result<Self> {
        let socket = match moq_native::bind::udp(address) {
            Ok(socket) => socket,
            Err(error) if address.ip() == IpAddr::V6(Ipv6Addr::UNSPECIFIED) => {
                tracing::debug!(%error, "IPv6 unavailable for the peer-to-peer socket; using IPv4");
                moq_native::bind::udp(SocketAddr::new(
                    IpAddr::V4(Ipv4Addr::UNSPECIFIED),
                    address.port(),
                ))?
            }
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("cannot bind peer-to-peer socket {address}"));
            }
        };
        let ipv6 = socket.local_addr()?.is_ipv6();
        let runtime: Arc<dyn quinn::Runtime> = Arc::new(quinn::TokioRuntime);
        let inner = runtime.wrap_udp_socket(socket)?;
        let socket = Arc::new(Demux {
            inner,
            pending: Mutex::new(HashMap::new()),
            ipv6,
        });
        let quic = quinn::Endpoint::new_with_abstract_socket(
            quinn::EndpointConfig::default(),
            server,
            socket.clone(),
            runtime,
        )?;
        Ok(Self {
            quic,
            socket,
            mapping: Mutex::new(None),
        })
    }

    pub fn local_addr(&self) -> Result<SocketAddr> {
        Ok(stun::canonical(self.quic.local_addr()?))
    }

    /// Send one datagram outside QUIC (STUN, punches) from the shared socket.
    async fn send_raw(&self, destination: SocketAddr, contents: &[u8]) -> io::Result<()> {
        self.socket.send_raw(destination, contents).await
    }

    /// Ask one STUN server for this socket's reflexive address.
    async fn reflexive(&self, server: SocketAddr) -> Option<SocketAddr> {
        let server = stun::canonical(server);
        let transaction = e2e::random::<12>();
        let (sender, mut receiver) = tokio::sync::oneshot::channel();
        self.socket
            .pending
            .lock()
            .unwrap()
            .insert(transaction, (server, sender));
        let request = stun::binding_request(&transaction);
        let result = tokio::time::timeout(STUN_TIMEOUT, async {
            loop {
                if let Err(error) = self.send_raw(server, &request).await {
                    tracing::debug!(%server, %error, "STUN request not sent");
                    return None;
                }
                tokio::select! {
                    mapped = &mut receiver => return mapped.ok(),
                    _ = tokio::time::sleep(STUN_RETRANSMIT) => {}
                }
            }
        })
        .await
        .ok()
        .flatten();
        self.socket.pending.lock().unwrap().remove(&transaction);
        result
    }

    /// The gateway's public address for this socket (NAT-PMP), reusing a
    /// recent mapping. A renewed mapping of the same port replaces the old
    /// guard without deleting it.
    async fn mapped(&self, port: u16) -> Option<SocketAddr> {
        if let Some((created, mapping)) = &*self.mapping.lock().unwrap()
            && created.elapsed() < MAPPING_REUSE
        {
            return Some(mapping.external);
        }
        match crate::natpmp::map(port).await {
            Ok(mapping) => {
                let external = mapping.external;
                if let Some((_, old)) = self
                    .mapping
                    .lock()
                    .unwrap()
                    .replace((Instant::now(), mapping))
                {
                    old.release();
                }
                Some(external)
            }
            Err(error) => {
                tracing::info!(
                    error = format!("{error:#}"),
                    "no gateway port mapping (NAT-PMP)"
                );
                None
            }
        }
    }

    /// Host, reflexive and (opt-in) mapped candidates for this socket.
    async fn gather(&self, args: &GatherArgs, relay: Option<&relay::Endpoint>) -> Vec<Candidate> {
        let local = match self.local_addr() {
            Ok(local) => local,
            Err(_) => return Vec::new(),
        };
        let mut candidates: Vec<Candidate> = host_addresses(local)
            .into_iter()
            .map(|address| Candidate {
                kind: Kind::Host,
                address,
            })
            .collect();
        let servers = stun_servers(args, relay).await;
        let reflexive = futures_util::future::join_all(
            servers
                .iter()
                .filter(|server| self.socket.destination(**server).is_some())
                .map(|server| self.reflexive(*server)),
        )
        .await;
        for address in reflexive.into_iter().flatten() {
            if valid_address(address) && !candidates.iter().any(|c| c.address == address) {
                candidates.push(Candidate {
                    kind: Kind::Srflx,
                    address,
                });
            }
        }
        if servers.is_empty() && !args.no_stun {
            tracing::debug!("no STUN server configured for the peer-to-peer path");
        }
        if args.upnp
            && let Some(address) = self.mapped(local.port()).await
            && valid_address(address)
            && !candidates.iter().any(|c| c.address == address)
        {
            candidates.push(Candidate {
                kind: Kind::Mapped,
                address,
            });
        }
        candidates.truncate(MAX_CANDIDATES);
        candidates
    }
}

/// Interface addresses for a socket bound to `local` (all of them for an
/// unspecified bind), with its port. Loopback and link-local are left out.
fn host_addresses(local: SocketAddr) -> Vec<SocketAddr> {
    let ips = if local.ip().is_unspecified() {
        let dual = local.is_ipv6();
        let mut ips = interface_addresses();
        ips.retain(|ip| dual || ip.is_ipv4());
        // IPv4 first: the family most likely to work across NATs.
        ips.sort_by_key(|ip| ip.is_ipv6());
        ips
    } else {
        vec![local.ip()]
    };
    let mut addresses: Vec<SocketAddr> = Vec::new();
    for ip in ips {
        let address = SocketAddr::new(ip, local.port());
        if valid_address(address) && !addresses.contains(&address) {
            addresses.push(address);
        }
    }
    addresses.truncate(MAX_HOST_CANDIDATES);
    addresses
}

/// Container and VM bridges: their addresses are never another machine's
/// way in, and would crowd out real interfaces from the candidate list.
fn virtual_interface(name: &str) -> bool {
    [
        "docker", "br-", "veth", "virbr", "vmnet", "vboxnet", "cni", "podman",
    ]
    .iter()
    .any(|prefix| name.starts_with(prefix))
}

/// Up interfaces' unicast addresses (getifaddrs).
fn interface_addresses() -> Vec<IpAddr> {
    let mut addresses = Vec::new();
    let mut head: *mut libc::ifaddrs = std::ptr::null_mut();
    // SAFETY: getifaddrs fills `head` with a list we only read and free once.
    unsafe {
        if libc::getifaddrs(&mut head) != 0 {
            return addresses;
        }
        let mut cursor = head;
        while !cursor.is_null() {
            let entry = &*cursor;
            let up = entry.ifa_flags & libc::IFF_UP as libc::c_uint != 0;
            let name = if entry.ifa_name.is_null() {
                ""
            } else {
                std::ffi::CStr::from_ptr(entry.ifa_name)
                    .to_str()
                    .unwrap_or("")
            };
            if up && !entry.ifa_addr.is_null() && !virtual_interface(name) {
                match i32::from((*entry.ifa_addr).sa_family) {
                    libc::AF_INET => {
                        let address = &*(entry.ifa_addr as *const libc::sockaddr_in);
                        addresses.push(IpAddr::V4(Ipv4Addr::from(u32::from_be(
                            address.sin_addr.s_addr,
                        ))));
                    }
                    libc::AF_INET6 => {
                        let address = &*(entry.ifa_addr as *const libc::sockaddr_in6);
                        addresses.push(IpAddr::V6(Ipv6Addr::from(address.sin6_addr.s6_addr)));
                    }
                    _ => {}
                }
            }
            cursor = entry.ifa_next;
        }
        libc::freeifaddrs(head);
    }
    addresses
}

/// Configured STUN servers, or the self-hosted relay's responder.
async fn stun_servers(args: &GatherArgs, relay: Option<&relay::Endpoint>) -> Vec<SocketAddr> {
    if args.no_stun {
        return Vec::new();
    }
    let names: Vec<String> = if args.stun.is_empty() {
        // Only a `teleport relay` (moqt://) is known to answer STUN; a
        // provider relay is never sent anything it was not asked to carry.
        relay
            .filter(|relay| relay.url.scheme() == "moqt")
            .and_then(|relay| relay.url.host_str())
            .map(|host| vec![format!("{host}:{}", stun::DEFAULT_PORT)])
            .unwrap_or_default()
    } else {
        args.stun.clone()
    };
    let mut servers = Vec::new();
    for name in names {
        match tokio::time::timeout(STUN_TIMEOUT, tokio::net::lookup_host(name.as_str())).await {
            Ok(Ok(addresses)) => {
                // One address per family is enough.
                let mut found: Vec<SocketAddr> = Vec::new();
                for address in addresses {
                    if !found
                        .iter()
                        .any(|known| known.is_ipv4() == address.is_ipv4())
                    {
                        found.push(stun::canonical(address));
                    }
                }
                servers.extend(found);
            }
            _ => tracing::debug!(server = %name, "STUN server did not resolve"),
        }
    }
    servers
}

// ── QUIC configuration ──────────────────────────────────────────────

fn versions() -> Result<moq_net::Versions> {
    Ok(moq_net::Versions::from(vec![
        protocol::WIRE_VERSION
            .parse::<moq_net::Version>()
            .map_err(anyhow::Error::msg)?,
    ]))
}

fn alpns() -> Result<Vec<Vec<u8>>> {
    Ok(versions()?
        .alpns()
        .iter()
        .map(|alpn| alpn.as_bytes().to_vec())
        .collect())
}

/// The same transport settings as the direct listener (moq-native's quinn
/// backend with Teleport's `protocol::QUIC_*`).
fn transport() -> Result<Arc<quinn::TransportConfig>> {
    let mut transport = quinn::TransportConfig::default();
    transport.max_idle_timeout(Some(protocol::QUIC_IDLE_TIMEOUT.try_into()?));
    transport.keep_alive_interval(Some(protocol::QUIC_KEEP_ALIVE));
    if !protocol::QUIC_MTU_DISCOVERY {
        transport.mtu_discovery_config(None);
    }
    let streams = quinn::VarInt::from_u32(1024);
    transport.max_concurrent_bidi_streams(streams);
    transport.max_concurrent_uni_streams(streams);
    transport.congestion_controller_factory(Arc::new(quinn::congestion::BbrConfig::default()));
    Ok(Arc::new(transport))
}

fn crypto_provider() -> Arc<quinn::rustls::crypto::CryptoProvider> {
    Arc::new(quinn::rustls::crypto::ring::default_provider())
}

/// Accepts exactly the certificate whose SHA-256 is pinned, like the direct
/// path's `--fingerprint` verification, and checks the TLS 1.3 handshake
/// signature with it.
#[derive(Debug)]
struct Pinned {
    fingerprint: [u8; 32],
    provider: Arc<quinn::rustls::crypto::CryptoProvider>,
}

impl quinn::rustls::client::danger::ServerCertVerifier for Pinned {
    fn verify_server_cert(
        &self,
        end_entity: &quinn::rustls::pki_types::CertificateDer<'_>,
        _intermediates: &[quinn::rustls::pki_types::CertificateDer<'_>],
        _server_name: &quinn::rustls::pki_types::ServerName<'_>,
        _ocsp_response: &[u8],
        _now: quinn::rustls::pki_types::UnixTime,
    ) -> Result<quinn::rustls::client::danger::ServerCertVerified, quinn::rustls::Error> {
        use subtle::ConstantTimeEq;
        let actual = ring::digest::digest(&ring::digest::SHA256, end_entity.as_ref());
        if bool::from(actual.as_ref().ct_eq(&self.fingerprint)) {
            Ok(quinn::rustls::client::danger::ServerCertVerified::assertion())
        } else {
            Err(quinn::rustls::Error::InvalidCertificate(
                quinn::rustls::CertificateError::ApplicationVerificationFailure,
            ))
        }
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &quinn::rustls::pki_types::CertificateDer<'_>,
        _dss: &quinn::rustls::DigitallySignedStruct,
    ) -> Result<quinn::rustls::client::danger::HandshakeSignatureValid, quinn::rustls::Error> {
        Err(quinn::rustls::Error::PeerIncompatible(
            quinn::rustls::PeerIncompatible::Tls13RequiredForQuic,
        ))
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &quinn::rustls::pki_types::CertificateDer<'_>,
        dss: &quinn::rustls::DigitallySignedStruct,
    ) -> Result<quinn::rustls::client::danger::HandshakeSignatureValid, quinn::rustls::Error> {
        quinn::rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<quinn::rustls::SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}

fn client_config(fingerprint: &str) -> Result<quinn::ClientConfig> {
    let fingerprint = moq_native::tls::parse_fingerprint(fingerprint)
        .map_err(|error| anyhow!("invalid pinned fingerprint: {error}"))?;
    let provider = crypto_provider();
    let mut tls = quinn::rustls::ClientConfig::builder_with_provider(provider.clone())
        .with_protocol_versions(&[&quinn::rustls::version::TLS13])?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(Pinned {
            fingerprint,
            provider,
        }))
        .with_no_client_auth();
    tls.alpn_protocols = alpns()?;
    // Every connection verifies the pinned certificate afresh.
    tls.resumption = quinn::rustls::client::Resumption::disabled();
    let mut config = quinn::ClientConfig::new(Arc::new(
        quinn::crypto::rustls::QuicClientConfig::try_from(tls)?,
    ));
    config.transport_config(transport()?);
    Ok(config)
}

// ── Client ──────────────────────────────────────────────────────────

/// The peer-to-peer path could not be set up at the transport level
/// (signalling, no candidates, no QUIC handshake), so nothing reached a host
/// that accepted the credential and the relay may be tried instead.
#[derive(Debug)]
pub struct Unavailable;

impl fmt::Display for Unavailable {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("peer-to-peer path unavailable")
    }
}

/// Keeps the client's peer-to-peer endpoint alive with its session.
pub struct Connected {
    pub session: moq_net::Session,
    pub kind: Kind,
    _endpoint: Endpoint,
}

/// Signal through `relay`, punch, and open a MoQ session to the host on the
/// first candidate whose QUIC handshake verifies the pinned certificate.
/// `target` is the direct session's SETUP path (`/teleport/<token>?...`).
/// Errors before a QUIC connection exists carry [`Unavailable`].
pub async fn connect(
    args: &ClientArgs,
    relay: &relay::Endpoint,
    pairing: &protocol::Pairing,
    target: String,
    publish: &moq_net::origin::Producer,
    subscribe: moq_net::origin::Producer,
) -> Result<Connected> {
    let started = Instant::now();
    let (endpoint, remote) = tokio::time::timeout(SIGNAL_TIMEOUT, async {
        let endpoint = Endpoint::bind("[::]:0".parse().unwrap(), None)?;
        let remote = signal(&endpoint, args, relay, pairing).await?;
        Ok::<_, anyhow::Error>((endpoint, remote))
    })
    .await
    .map_err(|_| anyhow!("no peer-to-peer answer through the relay within {SIGNAL_TIMEOUT:?} (host offline, older, or --no-p2p)"))
    .and_then(|result| result)
    .map_err(|error| error.context(Unavailable))?;
    if remote.is_empty() {
        return Err(
            anyhow!("the host offered no usable peer-to-peer candidates").context(Unavailable),
        );
    }
    tracing::info!(
        candidates = %describe(&remote),
        elapsed_ms = started.elapsed().as_millis() as u64,
        "peer-to-peer candidates exchanged through the relay; dialing"
    );
    let (connection, chosen) = dial(&endpoint, &remote, &pairing.fingerprint)
        .await
        .map_err(|error| error.context(Unavailable))?;
    tracing::info!(
        kind = %chosen.kind,
        address = %chosen.address,
        elapsed_ms = started.elapsed().as_millis() as u64,
        "peer-to-peer QUIC handshake verified the pinned certificate"
    );
    let (session, driver) = moq_net::Client::new()
        .with_versions(versions()?)
        .with_publisher(publish)
        .with_subscriber(subscribe)
        .with_path(target)
        .connect(web_transport_quinn::Session::raw(connection))
        .await
        .context("host rejected the peer-to-peer session")?;
    tokio::spawn(driver);
    Ok(Connected {
        session,
        kind: chosen.kind,
        _endpoint: endpoint,
    })
}

fn describe(candidates: &[Candidate]) -> String {
    candidates
        .iter()
        .map(|candidate| format!("{}={}", candidate.kind, candidate.address))
        .collect::<Vec<_>>()
        .join(",")
}

/// Client side of the signalling exchange: authenticate the host through
/// the relay, send our candidates sealed, and return the host's.
async fn signal(
    endpoint: &Endpoint,
    args: &ClientArgs,
    relay_endpoint: &relay::Endpoint,
    pairing: &protocol::Pairing,
) -> Result<Vec<Candidate>> {
    let device = pairing
        .device_key
        .as_deref()
        .map(e2e::DeviceKey::from_hex)
        .transpose()?;
    let handshake = e2e::ClientHandshake::from_hash(
        signal_credential(&e2e::credential_hash(&pairing.token)),
        SIGNAL_REQUEST,
        device,
    )?;
    let id = relay::hex(&e2e::random::<16>());
    let started = Instant::now();
    // Gather while the relay connects and the host answers the hello.
    let exchange = async {
        let link = relay::connect(relay_endpoint, &relay::namespace(&pairing.fingerprint)?).await?;
        tracing::debug!(
            elapsed_ms = started.elapsed().as_millis() as u64,
            "signalling: relay connected"
        );
        let hello = relay::publish_one(
            &link.outgoing,
            &format!("{id}/{}", paths::HELLO),
            serde_json::to_vec(handshake.hello())?,
        )
        .await?;
        let reply = relay::read_announced(&link, &format!("{id}/{}", paths::REPLY)).await?;
        let reply: e2e::HostReply =
            serde_json::from_slice(&reply).context("malformed host reply")?;
        tracing::debug!(
            elapsed_ms = started.elapsed().as_millis() as u64,
            "signalling: host replied"
        );
        Ok::<_, anyhow::Error>((link, hello, reply))
    };
    let gather = async {
        let local = endpoint.gather(&args.gather, Some(relay_endpoint)).await;
        tracing::debug!(
            elapsed_ms = started.elapsed().as_millis() as u64,
            candidates = %describe(&local),
            "signalling: local candidates gathered"
        );
        local
    };
    let (exchange, local) = tokio::join!(exchange, gather);
    let (link, _hello, reply) = exchange?;
    // Verifies the host's transcript signature against the pinned certificate.
    let established = handshake.finish(&reply, &pairing.fingerprint)?;
    let _confirm = relay::publish(
        &link.outgoing,
        &format!("{id}/{}", paths::CONFIRM),
        established.confirmation,
    )
    .await?;
    let _offer = relay::publish_one(
        &link.outgoing,
        &format!("{id}/{}", paths::OFFER),
        seal_candidates(
            &established.keys,
            Direction::ClientToHost,
            OFFER_TRACK,
            &local,
        )?,
    )
    .await?;
    tracing::debug!(
        elapsed_ms = started.elapsed().as_millis() as u64,
        "signalling: candidates offered"
    );
    let answer = relay::read_announced(&link, &format!("{id}/{}", paths::ANSWER)).await?;
    open_candidates(
        &established.keys,
        Direction::HostToClient,
        ANSWER_TRACK,
        &answer,
    )
}

/// Relay paths of one signalling exchange, below `<attempt>/`.
pub(crate) mod paths {
    pub const HELLO: &str = "p2p-hello";
    pub const REPLY: &str = "p2p-reply";
    pub const CONFIRM: &str = "p2p-confirm";
    pub const OFFER: &str = "p2p-offer";
    pub const ANSWER: &str = "p2p-answer";
}

/// Dial all candidates (staggered, best first) and keep the first QUIC
/// handshake that verifies the pinned certificate.
async fn dial(
    endpoint: &Endpoint,
    candidates: &[Candidate],
    fingerprint: &str,
) -> Result<(quinn::Connection, Candidate)> {
    use futures_util::StreamExt;
    let config = client_config(fingerprint)?;
    let mut ordered: Vec<Candidate> = candidates
        .iter()
        .copied()
        .filter(|candidate| endpoint.socket.destination(candidate.address).is_some())
        .collect();
    // Host candidates first (same network), then mapped, then reflexive;
    // IPv4 before IPv6 within each.
    ordered.sort_by_key(|candidate| {
        let rank = match candidate.kind {
            Kind::Host => 0,
            Kind::Mapped => 1,
            Kind::Srflx => 2,
        };
        (rank, candidate.address.is_ipv6())
    });
    ensure!(
        !ordered.is_empty(),
        "no candidate reachable from this socket"
    );
    let mut attempts = futures_util::stream::FuturesUnordered::new();
    for (index, candidate) in ordered.into_iter().enumerate() {
        let (quic, config) = (endpoint.quic.clone(), config.clone());
        attempts.push(async move {
            tokio::time::sleep(DIAL_STAGGER * index as u32).await;
            let connection = quic
                .connect_with(config, candidate.address, SERVER_NAME)?
                .await?;
            Ok::<_, anyhow::Error>((connection, candidate))
        });
    }
    let mut failures = Vec::new();
    let winner = tokio::time::timeout(DIAL_TIMEOUT, async {
        while let Some(result) = attempts.next().await {
            match result {
                Ok(winner) => return Some(winner),
                Err(error) => failures.push(format!("{error:#}")),
            }
        }
        None
    })
    .await;
    match winner {
        Ok(Some(winner)) => Ok(winner),
        Ok(None) => bail!("no candidate answered: {}", failures.join("; ")),
        Err(_) => bail!(
            "no QUIC handshake with the host's candidates within {DIAL_TIMEOUT:?}{}",
            if failures.is_empty() {
                String::new()
            } else {
                format!(" ({})", failures.join("; "))
            }
        ),
    }
}

// ── Host ────────────────────────────────────────────────────────────

/// A MoQ session request that arrived on the host's peer-to-peer socket.
#[cfg(target_os = "linux")]
pub struct Request {
    inner: Box<moq_net::Request<web_transport_quinn::Session>>,
    pub remote: SocketAddr,
}

/// A direct session request from the main listener or the peer-to-peer
/// socket; both are authorized identically by the host.
#[cfg(target_os = "linux")]
pub enum Incoming {
    Listener(moq_native::Request),
    P2p(Request),
}

#[cfg(target_os = "linux")]
impl Incoming {
    /// The request path without its query (`/teleport/<token>`).
    pub fn path(&self) -> &str {
        match self {
            Self::Listener(request) => request.path(),
            Self::P2p(request) => {
                let setup = request.inner.path();
                let path = setup.split_once('?').map_or(setup, |(path, _)| path);
                if path == "/" { "" } else { path }
            }
        }
    }

    /// The connection options (URL query) without the leading `?`.
    pub fn query(&self) -> Option<&str> {
        match self {
            Self::Listener(request) => request.query(),
            Self::P2p(request) => request.inner.path().split_once('?').map(|(_, query)| query),
        }
    }

    /// Reject the session (unauthorized).
    pub async fn close(self) {
        match self {
            Self::Listener(request) => {
                let _ = request.close(403).await;
            }
            Self::P2p(request) => (*request.inner).close(moq_net::Error::Unauthorized),
        }
    }

    /// Accept, publishing `publish` and receiving the client's broadcasts
    /// into `subscribe`. The origins are attached before this returns.
    pub fn ok(
        self,
        publish: &moq_net::origin::Producer,
        subscribe: moq_net::origin::Producer,
    ) -> impl std::future::Future<Output = Result<moq_net::Session>> + use<> {
        use futures_util::future::Either;
        match self {
            Self::Listener(request) => {
                let request = request.with_publisher(publish).with_subscriber(subscribe);
                Either::Left(async move { Ok(request.ok().await?) })
            }
            Self::P2p(request) => {
                let request = (*request.inner)
                    .with_publisher(publish)
                    .with_subscriber(subscribe);
                Either::Right(async move {
                    let (session, driver) = request.ok().await?;
                    tokio::spawn(driver);
                    Ok(session)
                })
            }
        }
    }
}

/// The host's peer-to-peer socket and signalling responder.
#[cfg(target_os = "linux")]
pub struct Host {
    endpoint: Endpoint,
    gather: GatherArgs,
    relay: relay::Endpoint,
    armed_until: Mutex<Option<Instant>>,
}

#[cfg(target_os = "linux")]
impl Host {
    /// Bind the peer-to-peer socket with the host's TLS identity and start
    /// accepting. Requests that complete the QUIC handshake and MoQ SETUP
    /// arrive on the returned channel for the host's usual authorization.
    pub fn start(
        args: &HostArgs,
        relay: relay::Endpoint,
        identity: &crate::identity::Identity,
    ) -> Result<(Arc<Self>, tokio::sync::mpsc::Receiver<Request>)> {
        let certificate = crate::identity::read_pem(&identity.certificate, "CERTIFICATE")?;
        let key = crate::identity::read_pem(&identity.key, "PRIVATE KEY")?;
        let mut tls = quinn::rustls::ServerConfig::builder_with_provider(crypto_provider())
            .with_protocol_versions(&[&quinn::rustls::version::TLS13])?
            .with_no_client_auth()
            .with_single_cert(
                vec![quinn::rustls::pki_types::CertificateDer::from(certificate)],
                quinn::rustls::pki_types::PrivateKeyDer::Pkcs8(key.into()),
            )?;
        tls.alpn_protocols = alpns()?;
        // As the main listener: no resumption, every handshake shows the
        // certificate the client pins.
        tls.send_tls13_tickets = 0;
        let mut server = quinn::ServerConfig::with_crypto(Arc::new(
            quinn::crypto::rustls::QuicServerConfig::try_from(tls)?,
        ));
        server.transport_config(transport()?);
        let address: SocketAddr = args
            .p2p_listen
            .parse()
            .with_context(|| format!("invalid --p2p-listen {}", args.p2p_listen))?;
        let endpoint = Endpoint::bind(address, Some(server))?;
        let host = Arc::new(Self {
            endpoint,
            gather: args.gather.clone(),
            relay,
            armed_until: Mutex::new(None),
        });
        tracing::info!(address = %host.endpoint.local_addr()?, "peer-to-peer socket ready (answers relay signalling)");
        let (sender, receiver) = tokio::sync::mpsc::channel(MAX_PENDING_ACCEPTS);
        tokio::spawn(host.clone().accept(sender));
        Ok((host, receiver))
    }

    fn armed(&self) -> bool {
        self.armed_until
            .lock()
            .unwrap()
            .is_some_and(|until| Instant::now() < until)
    }

    fn arm(&self) {
        *self.armed_until.lock().unwrap() = Some(Instant::now() + ARM_WINDOW);
    }

    async fn accept(self: Arc<Self>, requests: tokio::sync::mpsc::Sender<Request>) {
        let slots = Arc::new(tokio::sync::Semaphore::new(MAX_PENDING_ACCEPTS));
        while let Some(incoming) = self.endpoint.quic.accept().await {
            // Unsolicited Initials get no answer at all.
            if !self.armed() {
                incoming.ignore();
                continue;
            }
            let Ok(permit) = slots.clone().try_acquire_owned() else {
                incoming.ignore();
                continue;
            };
            let requests = requests.clone();
            tokio::spawn(async move {
                let _permit = permit;
                let remote = stun::canonical(incoming.remote_address());
                match tokio::time::timeout(ACCEPT_TIMEOUT, accept_one(incoming)).await {
                    Ok(Ok(inner)) => {
                        if requests
                            .try_send(Request {
                                inner: Box::new(inner),
                                remote,
                            })
                            .is_err()
                        {
                            tracing::debug!(%remote, "peer-to-peer request dropped (host busy)");
                        }
                    }
                    Ok(Err(error)) => {
                        tracing::debug!(%remote, error = format!("{error:#}"), "peer-to-peer handshake failed")
                    }
                    Err(_) => tracing::debug!(%remote, "peer-to-peer handshake timed out"),
                }
            });
        }
    }

    /// Host side of a signalling exchange whose client has confirmed the
    /// session keys: read its sealed candidates, open our NAT towards them,
    /// and answer with ours. The returned handles keep the answer published.
    pub async fn answer(
        &self,
        keys: &SessionKeys,
        id: &str,
        incoming: &moq_net::origin::Producer,
        outgoing: &moq_net::origin::Producer,
    ) -> Result<()> {
        let offer = tokio::time::timeout(OFFER_TIMEOUT, async {
            let broadcast = incoming
                .consume()
                .announced_broadcast(format!("{id}/{}", paths::OFFER).as_str())
                .await
                .context("no peer-to-peer offer")?;
            relay::read_one(&broadcast).await
        })
        .await
        .context("peer-to-peer offer timed out")??;
        let remote = open_candidates(keys, Direction::ClientToHost, OFFER_TRACK, &offer)?;
        let local = self.endpoint.gather(&self.gather, Some(&self.relay)).await;
        self.arm();
        tracing::info!(
            client = %describe(&remote),
            host = %describe(&local),
            "peer-to-peer signalling answered; punching"
        );
        self.punch(&remote);
        let answer = relay::publish_one(
            outgoing,
            &format!("{id}/{}", paths::ANSWER),
            seal_candidates(keys, Direction::HostToClient, ANSWER_TRACK, &local)?,
        )
        .await?;
        tokio::spawn(async move {
            tokio::time::sleep(ANSWER_HOLD).await;
            drop(answer);
        });
        Ok(())
    }

    /// Send STUN indications from the QUIC socket to each client candidate,
    /// so our NAT already expects the client's QUIC packets.
    fn punch(&self, remote: &[Candidate]) {
        let socket = self.endpoint.socket.clone();
        let targets: Vec<SocketAddr> = remote.iter().map(|c| c.address).collect();
        tokio::spawn(async move {
            let started = Instant::now();
            for at in PUNCH_SCHEDULE_MS {
                tokio::time::sleep_until((started + Duration::from_millis(at)).into()).await;
                let punch = stun::binding_indication(&e2e::random::<12>());
                for target in &targets {
                    let _ = socket.send_raw(*target, &punch).await;
                }
            }
        });
    }
}

/// QUIC handshake and MoQ SETUP for one connection on the peer-to-peer
/// socket. Authorization happens afterwards, in the host's session loop.
#[cfg(target_os = "linux")]
async fn accept_one(
    incoming: quinn::Incoming,
) -> Result<moq_net::Request<web_transport_quinn::Session>> {
    let connection = incoming.await?;
    let alpn = connection
        .handshake_data()
        .and_then(|data| data.downcast::<quinn::crypto::rustls::HandshakeData>().ok())
        .and_then(|data| data.protocol);
    let expected = alpns()?;
    ensure!(
        alpn.as_ref().is_some_and(|alpn| expected.contains(alpn)),
        "unexpected ALPN on the peer-to-peer socket"
    );
    let request = moq_net::Server::new()
        .with_versions(versions()?)
        .accept_request(web_transport_quinn::Session::raw(connection))
        .await?;
    Ok(request)
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;

    fn candidate(kind: Kind, address: &str) -> Candidate {
        Candidate {
            kind,
            address: address.parse().unwrap(),
        }
    }

    #[test]
    fn rejects_unusable_addresses_and_keeps_routable_ones() {
        for bad in [
            "0.0.0.0:4443",
            "127.0.0.1:4443",
            "224.0.0.251:5353",
            "239.255.255.250:1900",
            "255.255.255.255:4443",
            "169.254.1.1:4443",
            "240.0.0.1:4443",
            "0.1.2.3:4443",
            "10.0.0.1:0",
            "[::]:4443",
            "[::1]:4443",
            "[ff02::1]:4443",
            "[fe80::1]:4443",
            "[::ffff:127.0.0.1]:4443",
        ] {
            assert!(!valid_address(bad.parse().unwrap()), "{bad} accepted");
        }
        for good in [
            "10.1.0.2:4443",
            "192.168.1.20:50000",
            "203.0.113.9:1",
            "[2001:db8::7]:4443",
            "[fd00::1]:4443",
            "[::ffff:198.51.100.2]:9",
        ] {
            assert!(valid_address(good.parse().unwrap()), "{good} rejected");
        }
    }

    #[test]
    fn validation_drops_bogus_dedupes_and_bounds_the_list() {
        let list = vec![
            candidate(Kind::Host, "10.1.0.2:5000"),
            candidate(Kind::Host, "224.0.0.1:5000"),
            candidate(Kind::Srflx, "[::ffff:198.51.100.2]:5000"),
            candidate(Kind::Srflx, "198.51.100.2:5000"),
            candidate(Kind::Host, "127.0.0.1:5000"),
        ];
        let valid = validate(list).unwrap();
        assert_eq!(
            valid,
            vec![
                candidate(Kind::Host, "10.1.0.2:5000"),
                candidate(Kind::Srflx, "198.51.100.2:5000"),
            ]
        );
        let flood = (0..=MAX_CANDIDATES as u16)
            .map(|port| candidate(Kind::Host, &format!("10.0.0.1:{}", port + 1)))
            .collect();
        assert!(validate(flood).is_err());
    }

    #[test]
    fn signalling_credential_is_domain_separated() {
        let hash = e2e::credential_hash("token");
        let derived = signal_credential(&hash);
        assert_ne!(derived, hash);
        assert_eq!(derived, signal_credential(&hash));
        assert_ne!(derived, signal_credential(&e2e::credential_hash("other")));
    }

    #[test]
    fn candidates_open_only_in_their_session_track_and_direction() {
        let (client, host) = e2e::test_session();
        let (other, _) = e2e::test_session();
        let offer = vec![
            candidate(Kind::Host, "10.2.0.2:6000"),
            candidate(Kind::Srflx, "203.0.113.2:6000"),
        ];
        let sealed =
            seal_candidates(&client, Direction::ClientToHost, OFFER_TRACK, &offer).unwrap();
        assert_eq!(
            open_candidates(&host, Direction::ClientToHost, OFFER_TRACK, &sealed).unwrap(),
            offer
        );
        // Plaintext never appears on the relay.
        assert!(!String::from_utf8_lossy(&sealed).contains("203.0.113.2"));
        // Another session, track or direction cannot open it.
        assert!(open_candidates(&other, Direction::ClientToHost, OFFER_TRACK, &sealed).is_err());
        assert!(open_candidates(&host, Direction::ClientToHost, ANSWER_TRACK, &sealed).is_err());
        assert!(open_candidates(&host, Direction::HostToClient, OFFER_TRACK, &sealed).is_err());
        // Tampering is detected.
        let mut tampered = sealed.clone();
        *tampered.last_mut().unwrap() ^= 1;
        assert!(open_candidates(&host, Direction::ClientToHost, OFFER_TRACK, &tampered).is_err());
        // A relay cannot inject its own (unsealed) candidate list.
        let forged = serde_json::to_vec(&Candidates {
            version: 1,
            candidates: vec![candidate(Kind::Host, "198.51.100.66:4443")],
        })
        .unwrap();
        assert!(open_candidates(&host, Direction::ClientToHost, OFFER_TRACK, &forged).is_err());
        // Bogus addresses inside an authentic message are still dropped.
        let bogus = seal_candidates(
            &client,
            Direction::ClientToHost,
            OFFER_TRACK,
            &[
                candidate(Kind::Host, "239.1.1.1:4443"),
                candidate(Kind::Srflx, "203.0.113.2:6000"),
            ],
        )
        .unwrap();
        assert_eq!(
            open_candidates(&host, Direction::ClientToHost, OFFER_TRACK, &bogus).unwrap(),
            vec![candidate(Kind::Srflx, "203.0.113.2:6000")]
        );
    }

    #[test]
    fn host_addresses_skip_loopback_and_keep_the_port() {
        let addresses = host_addresses("0.0.0.0:4321".parse().unwrap());
        assert!(addresses.len() <= MAX_HOST_CANDIDATES);
        assert!(
            addresses
                .iter()
                .all(|a| a.port() == 4321 && a.is_ipv4() && valid_address(*a))
        );
        assert_eq!(
            host_addresses("10.9.8.7:4321".parse().unwrap()),
            vec!["10.9.8.7:4321".parse::<SocketAddr>().unwrap()]
        );
        assert!(host_addresses("127.0.0.1:4321".parse().unwrap()).is_empty());
    }

    /// STUN and QUIC share one socket: the STUN answer reaches the agent,
    /// and the reflexive address is the one QUIC uses (here, loopback).
    #[tokio::test]
    async fn stun_on_the_quic_socket_reports_its_mapping() {
        let server = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let server_address = server.local_addr().unwrap();
        let responder = tokio::spawn(stun::serve(server));
        let endpoint = Endpoint::bind("127.0.0.1:0".parse().unwrap(), None).unwrap();
        let mapped = endpoint.reflexive(server_address).await;
        assert_eq!(mapped, Some(endpoint.local_addr().unwrap()));
        // An answer from somewhere else for an outstanding transaction is ignored.
        let liar = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let transaction = e2e::random::<12>();
        let (sender, receiver) = tokio::sync::oneshot::channel();
        endpoint
            .socket
            .pending
            .lock()
            .unwrap()
            .insert(transaction, (server_address, sender));
        liar.send_to(
            &stun::binding_success(&transaction, "198.51.100.66:1".parse().unwrap()),
            endpoint.local_addr().unwrap(),
        )
        .await
        .unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(300), receiver)
                .await
                .is_err(),
            "spoofed STUN answer accepted"
        );
        responder.abort();
    }

    /// A punch (STUN indication) and a QUIC handshake arrive on the same
    /// socket: quinn completes the handshake, the indication never reaches it,
    /// and the pinned verifier refuses another certificate.
    #[tokio::test]
    async fn quic_handshake_on_the_shared_socket_is_pinned() {
        let certified = rcgen::generate_simple_self_signed(vec!["teleport.local".into()]).unwrap();
        let der = certified.cert.der().to_vec();
        let fingerprint: String = ring::digest::digest(&ring::digest::SHA256, &der)
            .as_ref()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        let mut tls = quinn::rustls::ServerConfig::builder_with_provider(crypto_provider())
            .with_protocol_versions(&[&quinn::rustls::version::TLS13])
            .unwrap()
            .with_no_client_auth()
            .with_single_cert(
                vec![quinn::rustls::pki_types::CertificateDer::from(der)],
                quinn::rustls::pki_types::PrivateKeyDer::Pkcs8(
                    certified.signing_key.serialize_der().into(),
                ),
            )
            .unwrap();
        tls.alpn_protocols = alpns().unwrap();
        let server = quinn::ServerConfig::with_crypto(Arc::new(
            quinn::crypto::rustls::QuicServerConfig::try_from(tls).unwrap(),
        ));
        let host = Endpoint::bind("127.0.0.1:0".parse().unwrap(), Some(server)).unwrap();
        let host_address = host.local_addr().unwrap();
        let client = Endpoint::bind("127.0.0.1:0".parse().unwrap(), None).unwrap();
        // Punch first: must not disturb quinn on the receiving side.
        host.send_raw(
            client.local_addr().unwrap(),
            &stun::binding_indication(&e2e::random::<12>()),
        )
        .await
        .unwrap();
        let accept = tokio::spawn({
            let quic = host.quic.clone();
            async move { quic.accept().await.unwrap().await.unwrap() }
        });
        let candidate = Candidate {
            kind: Kind::Host,
            address: host_address,
        };
        let (connection, chosen) = dial(&client, &[candidate], &fingerprint).await.unwrap();
        assert_eq!(chosen, candidate);
        assert_eq!(stun::canonical(connection.remote_address()), host_address);
        accept.await.unwrap();
        // A different pin fails the handshake.
        let wrong = "ab".repeat(32);
        assert!(dial(&client, &[candidate], &wrong).await.is_err());
    }
}
