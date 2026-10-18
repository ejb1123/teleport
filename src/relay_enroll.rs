//! Enrollment through a relay, so a host never needs a reachable port, not
//! even for first-time pairing.
//!
//! The enrollment protocols run exactly as over TCP, byte for byte, over an
//! ordered byte stream carried by a pair of relay tracks:
//!
//! * code pairing (`TPPAIR02`, SPAKE2) on a *rendezvous* namespace the host
//!   opens with the pairing code. The client learns it from the pairing link
//!   (`teleport://relay/<rendezvous>/<code>`), since before pairing it does not
//!   know the host fingerprint the normal namespace is derived from;
//! * password enrollment (`TPAUTH01`, OPAQUE) and Linux system login
//!   (`TPSYS001`, TLS pinned to the host certificate) on the host's normal
//!   namespace (`crate::relay::namespace`).
//!
//! Each attempt uses a fresh random `<attempt>` id inside the namespace:
//!
//! * `<attempt>/request`  client → host
//! * `<attempt>/response` host → client
//!
//! Every chunk written to the stream is one group holding one frame on an
//! ordered track, so the receiver can restore order and detect gaps. Finishing
//! the track is end of stream. The relay is untrusted: it can only see sizes
//! and timing of PAKE / TLS messages it cannot use (see docs/pairing.md).

use crate::{
    e2e, pairing,
    protocol::Pairing,
    relay::{self, AbortOnDrop, Endpoint, Link},
};
use anyhow::{Context, Result, ensure};
use std::{
    collections::BTreeMap,
    future::Future,
    pin::Pin,
    sync::{Arc, Mutex},
    task::{Context as TaskContext, Poll},
    time::Duration,
};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, DuplexStream, ReadBuf};

const TRACK: &str = "stream";
const REQUEST: &str = "request";
const RESPONSE: &str = "response";
/// Largest chunk (group) accepted from the peer.
const MAX_CHUNK: usize = 4096;
/// Total bytes accepted per direction and attempt; the enrollment protocols
/// need a few KiB at most.
const MAX_BYTES: usize = 64 * 1024;
/// How long our final message stays available on the relay after we close.
const LINGER: Duration = Duration::from_secs(10);
/// Groups may arrive out of order on separate QUIC streams.
const REORDER_WINDOW: u64 = 16;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// Code pairing through a relay: the host answers once it sees the request.
const PAIR_TIMEOUT: Duration = Duration::from_secs(20);
const SYSTEM_LOGIN_TIMEOUT: Duration = Duration::from_secs(45);
/// Host-side bounds on unauthenticated relayed enrollment work.
#[cfg(target_os = "linux")]
const MAX_PENDING: usize = 4;
#[cfg(target_os = "linux")]
const EXCHANGE_TIMEOUT: Duration = Duration::from_secs(100);
#[cfg(target_os = "linux")]
const REPLAY_CACHE: usize = 1024;

/// Prefix of a relay pairing link.
pub const LINK_PREFIX: &str = "teleport://relay/";

/// The secret a host shows when relay pairing opens: where to meet (the
/// rendezvous) and the one-time code. Treat it exactly like the code.
#[derive(Clone, PartialEq, Eq)]
pub struct Invite {
    /// 128 random bits as 32 lowercase hex digits, fresh per opened pairing.
    pub rendezvous: String,
    /// Six digits, normalized.
    pub code: String,
}

impl Invite {
    #[cfg(target_os = "linux")]
    pub fn new(code: &str) -> Result<Self> {
        Ok(Self {
            rendezvous: relay::hex(&e2e::random::<16>()),
            code: pairing::normalize(code)?,
        })
    }

    /// Parse `teleport://relay/<rendezvous>/<code>` (surrounding whitespace
    /// allowed; the code may contain a dash).
    pub fn parse(text: &str) -> Result<Self> {
        let text = text.trim();
        ensure!(text.len() <= 128, "invalid relay pairing link");
        let rest = text
            .strip_prefix(LINK_PREFIX)
            .context("enter the relay pairing link from the host (teleport://relay/...)")?;
        let (rendezvous, code) = rest
            .split_once('/')
            .context("relay pairing link is missing its code")?;
        let rendezvous = rendezvous.to_ascii_lowercase();
        ensure!(
            relay::valid_namespace(&rendezvous),
            "invalid relay pairing link"
        );
        Ok(Self {
            rendezvous,
            code: pairing::normalize(code)?,
        })
    }

    #[cfg(any(test, target_os = "linux"))]
    pub fn link(&self) -> String {
        format!(
            "{LINK_PREFIX}{}/{}-{}",
            self.rendezvous,
            &self.code[..3],
            &self.code[3..]
        )
    }

    /// The relay namespace host and client meet in. Derived (domain
    /// separated from host namespaces) rather than the rendezvous itself.
    pub fn namespace(&self) -> String {
        let mut context = ring::digest::Context::new(&ring::digest::SHA256);
        context.update(b"teleport-relay-pairing-v1");
        context.update(self.rendezvous.as_bytes());
        relay::hex(&context.finish().as_ref()[..16])
    }
}

/// An ordered, reliable byte stream through a relay. Reads end (EOF) when the
/// peer closes its side; dropping the stream closes ours after the relay has
/// had a chance to deliver what we wrote.
pub struct Stream {
    io: DuplexStream,
    failure: Arc<Mutex<Option<String>>>,
    _reader: AbortOnDrop,
    _link: Option<Link>,
}

impl Stream {
    /// Why the incoming direction stopped early, if it did (e.g. the relay
    /// closed the connection).
    fn failure(&self) -> Arc<Mutex<Option<String>>> {
        self.failure.clone()
    }
}

impl AsyncRead for Stream {
    fn poll_read(
        self: Pin<&mut Self>,
        context: &mut TaskContext<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.get_mut().io).poll_read(context, buffer)
    }
}

impl AsyncWrite for Stream {
    fn poll_write(
        self: Pin<&mut Self>,
        context: &mut TaskContext<'_>,
        buffer: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut self.get_mut().io).poll_write(context, buffer)
    }
    fn poll_flush(
        self: Pin<&mut Self>,
        context: &mut TaskContext<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.get_mut().io).poll_flush(context)
    }
    fn poll_shutdown(
        self: Pin<&mut Self>,
        context: &mut TaskContext<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.get_mut().io).poll_shutdown(context)
    }
}

/// Bridge a local byte stream to the relay: what we write is published on
/// `write_path`; the peer's broadcast (resolved by `peer`) is read in order.
fn pipe(
    outgoing: &moq_net::origin::Producer,
    write_path: &str,
    peer: impl Future<Output = Result<moq_net::broadcast::Consumer>> + Send + 'static,
    session: Option<moq_net::Session>,
) -> Result<Stream> {
    let network = outgoing.create_broadcast(
        write_path,
        moq_net::broadcast::Route::new().with_announce(true),
    )?;
    // Chunks go to a local track; the relay is served from it, from the first
    // chunk whenever it asks: a moq-transport relay forwards a subscription
    // from the publisher's live edge, which may be past chunks already written.
    let mut chunks = moq_net::broadcast::Info::new().produce();
    let mut track =
        chunks.create_track(TRACK, moq_net::track::Info::default().with_ordered(true))?;
    let server = serve_from_start(network.dynamic(), chunks.consume());
    let (local, remote) = tokio::io::duplex(2 * MAX_CHUNK);
    let (mut from_local, mut to_local) = tokio::io::split(remote);
    // Writer: not aborted when the stream drops, so the final message is still
    // published; it ends at local EOF plus LINGER.
    tokio::spawn(async move {
        let _network = network;
        let _chunks = chunks;
        let _server = AbortOnDrop(tokio::spawn(server));
        let mut buffer = vec![0; MAX_CHUNK];
        let mut total = 0;
        loop {
            let size = match from_local.read(&mut buffer).await {
                Ok(0) | Err(_) => break,
                Ok(size) => size,
            };
            total += size;
            if total > MAX_BYTES {
                return;
            }
            let Ok(mut group) = track.append_group() else {
                return;
            };
            if group
                .write_frame(
                    moq_net::Timestamp::now(),
                    bytes::Bytes::copy_from_slice(&buffer[..size]),
                )
                .is_err()
                || group.finish().is_err()
            {
                return;
            }
        }
        let _ = track.finish();
        tokio::time::sleep(LINGER).await;
    });
    let failure = Arc::new(Mutex::new(None));
    let reader = {
        let failure = failure.clone();
        tokio::spawn(async move {
            let result = tokio::select! {
                result = receive(peer, &mut to_local) => result,
                error = async { match &session { Some(session) => session.closed().await, None => std::future::pending().await } } => Err(anyhow::anyhow!(
                    "relay closed the connection: {error} (check --relay-key)"
                )),
            };
            if let Err(error) = &result {
                *failure.lock().unwrap() = Some(format!("{error:#}"));
            }
            // End of stream for the local reader in every case.
            let _ = to_local.shutdown().await;
            result
        })
    };
    Ok(Stream {
        io: local,
        failure,
        _reader: AbortOnDrop(reader),
        _link: None,
    })
}

/// Answer the relay's requests for our stream track with every chunk from
/// the first.
async fn serve_from_start(
    mut requests: moq_net::broadcast::Dynamic,
    chunks: moq_net::broadcast::Consumer,
) -> Result<()> {
    let mut copies = tokio::task::JoinSet::new();
    loop {
        let request = requests.requested_track().await?;
        while copies.try_join_next().is_some() {}
        if request.name() != TRACK {
            request.reject(moq_net::Error::NotFound);
            continue;
        }
        let mut out = request.accept(moq_net::track::Info::default().with_ordered(true));
        let mut source = chunks
            .track(TRACK)?
            .subscribe(
                moq_net::track::Subscription::default()
                    .with_ordered(true)
                    .with_group_start(0),
            )
            .await?;
        copies.spawn(async move {
            while let Some(mut group) = source.recv_group().await? {
                let mut copy = out.create_group(moq_net::group::Info {
                    sequence: group.sequence,
                })?;
                while let Some(frame) = crate::protocol::read_frame(&mut group, MAX_CHUNK).await? {
                    copy.write_frame(moq_net::Timestamp::now(), frame)?;
                }
                copy.finish()?;
            }
            out.finish()?;
            Ok::<_, anyhow::Error>(())
        });
    }
}

async fn receive(
    peer: impl Future<Output = Result<moq_net::broadcast::Consumer>>,
    output: &mut (impl AsyncWrite + Unpin),
) -> Result<()> {
    let broadcast = peer.await?;
    let mut track = broadcast
        .track(TRACK)?
        .subscribe(
            moq_net::track::Subscription::default()
                .with_ordered(true)
                .with_group_start(0),
        )
        .await?;
    let mut pending = BTreeMap::new();
    let mut sequence = 0u64;
    let mut total = 0;
    loop {
        let mut group = match pending.remove(&sequence) {
            Some(group) => group,
            None => match track.recv_group().await? {
                None => {
                    ensure!(pending.is_empty(), "relay stream ended with a gap");
                    return Ok(());
                }
                Some(group) => {
                    ensure!(
                        group.sequence >= sequence && group.sequence - sequence <= REORDER_WINDOW,
                        "relay stream group outside the reorder window"
                    );
                    if group.sequence != sequence {
                        ensure!(
                            pending.insert(group.sequence, group).is_none(),
                            "duplicate relay stream group"
                        );
                        continue;
                    }
                    group
                }
            },
        };
        sequence += 1;
        let frame = crate::protocol::read_frame(&mut group, MAX_CHUNK)
            .await?
            .context("empty relay stream group")?;
        ensure!(!frame.is_empty(), "empty relay stream chunk");
        total += frame.len();
        ensure!(total <= MAX_BYTES, "relay stream too long");
        output.write_all(&frame).await?;
    }
}

/// Client side: open a new enrollment attempt in `namespace` on the relay.
pub async fn open(endpoint: &Endpoint, namespace: &str) -> Result<Stream> {
    let link = tokio::time::timeout(CONNECT_TIMEOUT, relay::connect(endpoint, namespace))
        .await
        .context("relay connection timed out")??;
    let attempt = relay::hex(&e2e::random::<16>());
    let incoming = link.incoming.consume();
    let response = format!("{attempt}/{RESPONSE}");
    let mut stream = pipe(
        &link.outgoing,
        &format!("{attempt}/{REQUEST}"),
        async move {
            incoming
                .announced_broadcast(response.as_str())
                .await
                .context("relay closed before the host answered")
        },
        Some(link.session.clone()),
    )?;
    stream._link = Some(link);
    Ok(stream)
}

/// Run `exchange` on a relay stream, explaining a relay-level failure (wrong
/// relay key, relay gone) rather than a bare end of stream.
async fn run<T>(
    stream: Stream,
    timeout: Duration,
    what: &str,
    exchange: impl AsyncFnOnce(Stream) -> Result<T>,
) -> Result<T> {
    let failure = stream.failure();
    let result = tokio::time::timeout(timeout, exchange(stream))
        .await
        .map_err(|_| anyhow::anyhow!("{what} timed out through the relay"))
        .flatten();
    result.map_err(|error| match failure.lock().unwrap().take() {
        Some(reason) => error.context(reason),
        None => error,
    })
}

/// Code pairing through a relay. The result is the normal credential; later
/// sessions use the host namespace derived from its fingerprint.
pub async fn pair(endpoint: &Endpoint, invite: &Invite) -> Result<Pairing> {
    let stream = open(endpoint, &invite.namespace()).await?;
    run(stream, PAIR_TIMEOUT, "pairing", async |stream| {
        pairing::pair_over(stream, &invite.code, true)
            .await
            .context(
                "no pairing through the relay; check the link, that pairing is still open \
             on the host, and that both use the same relay",
            )
    })
    .await
}

/// Guest enrollment with an invite through a relay, on the host's own
/// namespace (the link's relay host id). SPAKE2 authenticates the host; the
/// id must match the identity it proves.
pub async fn join(endpoint: &Endpoint, invite: &pairing::InviteLink) -> Result<Pairing> {
    let host = invite.relay_host.clone().context(
        "this invite link has no relay host id; ask the host for a link made with a relay",
    )?;
    let stream = open(endpoint, &host).await?;
    let pairing = run(stream, PAIR_TIMEOUT, "invite", async |stream| {
        pairing::join_over(stream, invite).await.context(
            "no invite enrollment through the relay; check the link, that it is still valid, \
             and that both use the same relay",
        )
    })
    .await?;
    ensure!(
        relay::namespace(&pairing.fingerprint)? == host,
        "the authenticated host does not match the invite's relay host id"
    );
    Ok(pairing)
}

/// Password (OPAQUE) enrollment through a relay. `host` is the host's relay id
/// (its namespace, shown by `teleport host-admin status`); OPAQUE
/// authenticates the host, and the id must match the identity it proves.
pub async fn login(
    endpoint: &Endpoint,
    host: &str,
    username: &str,
    password: &str,
    device_name: &str,
) -> Result<Pairing> {
    let host = host.trim().to_ascii_lowercase();
    ensure!(
        relay::valid_namespace(&host),
        "the host relay id is 32 hexadecimal digits (teleport host-admin status)"
    );
    let stream = open(endpoint, &host).await?;
    let pairing = run(
        stream,
        Duration::from_secs(95),
        "password login",
        async |stream| crate::access::login_relayed(stream, username, password, device_name).await,
    )
    .await?;
    ensure!(
        relay::namespace(&pairing.fingerprint)? == host,
        "the authenticated host does not match the relay host id"
    );
    Ok(pairing)
}

/// Linux account login through a relay. The password travels only inside TLS
/// pinned to `fingerprint`, which runs end to end through the relay stream.
pub async fn login_system(
    endpoint: &Endpoint,
    username: &str,
    password: &str,
    fingerprint: &str,
) -> Result<Pairing> {
    let stream = open(endpoint, &relay::namespace(fingerprint)?).await?;
    run(
        stream,
        SYSTEM_LOGIN_TIMEOUT,
        "system login",
        async |stream| {
            crate::system_login::login_over(stream, username, password, fingerprint).await
        },
    )
    .await
}

/// Host side: accept enrollment attempts announced in a relay namespace and
/// hand each stream to `handler`.
///
/// Unauthenticated work is bounded: at most [`MAX_PENDING`] attempts run at
/// once (others are ignored), each for at most [`EXCHANGE_TIMEOUT`] and
/// [`MAX_BYTES`] per direction; an attempt id is served at most once. Ends when
/// the relay connection does.
#[cfg(target_os = "linux")]
pub async fn serve<F, Fut>(
    session: moq_net::Session,
    incoming: moq_net::origin::Producer,
    outgoing: moq_net::origin::Producer,
    handler: F,
) -> Result<()>
where
    F: Fn(Stream) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = ()> + Send + 'static,
{
    let mut announced = incoming.consume().announced();
    let slots = Arc::new(tokio::sync::Semaphore::new(MAX_PENDING));
    let mut seen = Seen::default();
    let handler = Arc::new(handler);
    let mut handlers = tokio::task::JoinSet::new();
    let suffix = format!("/{REQUEST}");
    loop {
        while handlers.try_join_next().is_some() {}
        let announce = tokio::select! {
            announce = announced.next() => announce.context("relay connection closed")?,
            error = session.closed() => anyhow::bail!("relay closed the connection: {error} (check --relay-key)"),
        };
        let Some(broadcast) = announce.broadcast else {
            continue;
        };
        let path = announce.path.to_string();
        let Some(attempt) = path.strip_suffix(suffix.as_str()) else {
            continue;
        };
        if !relay::valid_namespace(attempt) {
            continue;
        }
        if !seen.insert(attempt) {
            tracing::warn!("ignoring replayed relay enrollment attempt");
            continue;
        }
        let Ok(permit) = slots.clone().try_acquire_owned() else {
            tracing::warn!("too many pending relay enrollments; ignoring attempt");
            continue;
        };
        let Ok(stream) = pipe(
            &outgoing,
            &format!("{attempt}/{RESPONSE}"),
            async move { Ok(broadcast) },
            Some(session.clone()),
        ) else {
            continue;
        };
        let handler = handler.clone();
        handlers.spawn(async move {
            let _permit = permit;
            let _ = tokio::time::timeout(EXCHANGE_TIMEOUT, handler(stream)).await;
        });
    }
}

/// Attempt ids already served (bounded, oldest forgotten first).
#[cfg(target_os = "linux")]
#[derive(Default)]
struct Seen {
    set: std::collections::HashSet<String>,
    order: std::collections::VecDeque<String>,
}

#[cfg(target_os = "linux")]
impl Seen {
    fn insert(&mut self, attempt: &str) -> bool {
        if !self.set.insert(attempt.to_owned()) {
            return false;
        }
        self.order.push_back(attempt.to_owned());
        if self.order.len() > REPLAY_CACHE
            && let Some(old) = self.order.pop_front()
        {
            self.set.remove(&old);
        }
        true
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    #[test]
    fn invite_links_round_trip_and_reject_malformed_input() {
        let invite = Invite {
            rendezvous: "0123456789abcdef0123456789abcdef".into(),
            code: "123456".into(),
        };
        let link = invite.link();
        assert_eq!(
            link,
            "teleport://relay/0123456789abcdef0123456789abcdef/123-456"
        );
        assert!(Invite::parse(&format!("  {link}\n")).unwrap() == invite);
        assert!(
            Invite::parse(
                &link
                    .to_uppercase()
                    .replace("TELEPORT://RELAY/", LINK_PREFIX)
            )
            .unwrap()
                == invite
        );
        for bad in [
            "123-456",
            "teleport://relay/0123456789abcdef0123456789abcdef",
            "teleport://relay/0123456789abcdef0123456789abcdef/12345",
            "teleport://relay/0123/123-456",
            "teleport://relay/0123456789abcdef0123456789abcdeg/123-456",
            "https://relay/0123456789abcdef0123456789abcdef/123-456",
        ] {
            assert!(Invite::parse(bad).is_err(), "{bad}");
        }
        // The relay sees only the namespace: valid, stable, and neither the
        // rendezvous nor dependent on the code.
        let namespace = invite.namespace();
        assert!(relay::valid_namespace(&namespace));
        assert_ne!(namespace, invite.rendezvous);
        let other_code = Invite {
            code: "654321".into(),
            ..invite.clone()
        };
        assert_eq!(namespace, other_code.namespace());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn replayed_attempt_ids_are_served_once() {
        let mut seen = Seen::default();
        assert!(seen.insert("a"));
        assert!(!seen.insert("a"));
        for index in 0..REPLAY_CACHE {
            assert!(seen.insert(&index.to_string()));
        }
        assert!(seen.order.len() <= REPLAY_CACHE && seen.set.len() <= REPLAY_CACHE);
    }

    /// A relay-shaped loopback: both ends publish into one in-process origin,
    /// exactly as two relay connections scoped to the same namespace would.
    pub(crate) fn loopback(origin: &moq_net::origin::Producer, attempt: &str) -> (Stream, Stream) {
        let peer = origin.consume();
        let response = format!("{attempt}/{RESPONSE}");
        let client = pipe(
            origin,
            &format!("{attempt}/{REQUEST}"),
            async move {
                peer.announced_broadcast(response.as_str())
                    .await
                    .context("host gone")
            },
            None,
        )
        .unwrap();
        let peer = origin.consume();
        let request = format!("{attempt}/{REQUEST}");
        let host = pipe(
            origin,
            &format!("{attempt}/{RESPONSE}"),
            async move {
                peer.announced_broadcast(request.as_str())
                    .await
                    .context("client gone")
            },
            None,
        )
        .unwrap();
        (client, host)
    }

    pub(crate) fn origin() -> moq_net::origin::Producer {
        moq_net::Origin::random().produce()
    }

    #[tokio::test]
    async fn stream_carries_bytes_in_order_and_signals_end_of_stream() -> Result<()> {
        let origin = origin();
        let (mut client, mut host) = loopback(&origin, &"1".repeat(32));
        let sent: Vec<u8> = (0..3 * MAX_CHUNK + 123).map(|i| (i % 251) as u8).collect();
        client.write_all(&sent).await?;
        client.shutdown().await?;
        let mut received = Vec::new();
        tokio::time::timeout(Duration::from_secs(5), host.read_to_end(&mut received)).await??;
        assert_eq!(received, sent);
        host.write_all(b"reply").await?;
        drop(host);
        let mut reply = Vec::new();
        tokio::time::timeout(Duration::from_secs(5), client.read_to_end(&mut reply)).await??;
        assert_eq!(reply, b"reply");
        Ok(())
    }

    #[tokio::test]
    async fn stream_stops_after_the_per_attempt_byte_limit() -> Result<()> {
        let origin = origin();
        let (mut client, mut host) = loopback(&origin, &"2".repeat(32));
        tokio::spawn(async move {
            let _ = client.write_all(&vec![7; MAX_BYTES + MAX_CHUNK]).await;
        });
        let mut received = Vec::new();
        tokio::time::timeout(Duration::from_secs(5), host.read_to_end(&mut received)).await??;
        assert!(received.len() <= MAX_BYTES, "{}", received.len());
        Ok(())
    }

    async fn receive_groups(sequences: &[u64]) -> Result<Vec<u8>> {
        let origin = origin();
        let mut broadcast = origin.create_broadcast(
            "groups",
            moq_net::broadcast::Route::new().with_announce(true),
        )?;
        let mut track =
            broadcast.create_track(TRACK, moq_net::track::Info::default().with_ordered(true))?;
        for &sequence in sequences {
            let mut group = track.create_group(moq_net::group::Info { sequence })?;
            group.write_frame(
                moq_net::Timestamp::now(),
                bytes::Bytes::from(vec![b'a' + sequence as u8]),
            )?;
            group.finish()?;
        }
        track.finish()?;
        let consumer = origin.consume();
        let mut output = Vec::new();
        tokio::time::timeout(
            Duration::from_secs(5),
            receive(
                async move {
                    consumer
                        .announced_broadcast("groups")
                        .await
                        .context("missing")
                },
                &mut output,
            ),
        )
        .await??;
        drop(broadcast);
        Ok(output)
    }

    #[tokio::test]
    async fn framing_restores_order_and_rejects_gaps_and_far_groups() -> Result<()> {
        assert_eq!(receive_groups(&[1, 0, 3, 2]).await?, b"abcd");
        assert!(receive_groups(&[0, 2]).await.is_err(), "gap accepted");
        assert!(
            receive_groups(&[REORDER_WINDOW + 1]).await.is_err(),
            "group far outside the window accepted"
        );
        Ok(())
    }

    /// Records what a host reads (everything the client sent through the
    /// relay), e.g. to replay it or check it for secrets.
    #[cfg(target_os = "linux")]
    pub(crate) struct Recorder<S>(pub S, pub Arc<Mutex<Vec<u8>>>);
    #[cfg(target_os = "linux")]
    impl<S: AsyncRead + Unpin> AsyncRead for Recorder<S> {
        fn poll_read(
            self: Pin<&mut Self>,
            context: &mut TaskContext<'_>,
            buffer: &mut ReadBuf<'_>,
        ) -> Poll<std::io::Result<()>> {
            let this = self.get_mut();
            let before = buffer.filled().len();
            let result = Pin::new(&mut this.0).poll_read(context, buffer);
            this.1
                .lock()
                .unwrap()
                .extend_from_slice(&buffer.filled()[before..]);
            result
        }
    }
    #[cfg(target_os = "linux")]
    impl<S: AsyncWrite + Unpin> AsyncWrite for Recorder<S> {
        fn poll_write(
            self: Pin<&mut Self>,
            context: &mut TaskContext<'_>,
            buffer: &[u8],
        ) -> Poll<std::io::Result<usize>> {
            Pin::new(&mut self.get_mut().0).poll_write(context, buffer)
        }
        fn poll_flush(
            self: Pin<&mut Self>,
            context: &mut TaskContext<'_>,
        ) -> Poll<std::io::Result<()>> {
            Pin::new(&mut self.get_mut().0).poll_flush(context)
        }
        fn poll_shutdown(
            self: Pin<&mut Self>,
            context: &mut TaskContext<'_>,
        ) -> Poll<std::io::Result<()>> {
            Pin::new(&mut self.get_mut().0).poll_shutdown(context)
        }
    }

    #[cfg(target_os = "linux")]
    async fn host_attempt(
        host: impl AsyncRead + AsyncWrite + Unpin + Send + 'static,
        credential: &Pairing,
    ) -> tokio::task::JoinHandle<(bool, bool)> {
        let credential = credential.clone();
        tokio::spawn(async move {
            let mut host = host;
            let mut used = false;
            let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
            let result =
                pairing::accept(&mut host, "123456", &credential, &mut used, deadline).await;
            (result.is_ok(), used)
        })
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn relayed_code_pairing_rejects_wrong_codes_and_replayed_transcripts() -> Result<()> {
        let origin = origin();
        let credential = Pairing {
            token: "c".repeat(64),
            fingerprint: "d".repeat(64),
            device_key: None,
        };
        let pair = async |client: Stream, code: &str| {
            tokio::time::timeout(PAIR_TIMEOUT, pairing::pair_over(client, code, true))
                .await
                .map_err(anyhow::Error::from)
                .flatten()
        };
        // A wrong code fails on both sides and consumes nothing.
        let (client, host) = loopback(&origin, &"a".repeat(32));
        let host = host_attempt(host, &credential).await;
        assert!(pair(client, "000-000").await.is_err());
        assert_eq!(host.await?, (false, false));
        // The right code yields the credential; record what the host read.
        let recorded = Arc::new(Mutex::new(Vec::new()));
        let (client, host) = loopback(&origin, &"b".repeat(32));
        let host = host_attempt(Recorder(host, recorded.clone()), &credential).await;
        assert_eq!(pair(client, "123-456").await?.token, credential.token);
        assert_eq!(host.await?, (true, true));
        // Replaying that client's bytes (as a relay could) against a fresh host
        // exchange fails key confirmation: no credential is ever sent.
        let (mut client, host) = loopback(&origin, &"e".repeat(32));
        let host = host_attempt(host, &credential).await;
        let transcript = recorded.lock().unwrap().clone();
        assert!(transcript.starts_with(pairing::MAGIC));
        client.write_all(&transcript).await?;
        client.shutdown().await?;
        let mut answer = Vec::new();
        tokio::time::timeout(Duration::from_secs(5), client.read_to_end(&mut answer)).await??;
        assert_eq!(host.await?, (false, false));
        // Only the host's fresh SPAKE2 message (length prefix + 33 bytes).
        assert_eq!(answer.len(), 2 + 33);
        Ok(())
    }

    /// A relay that ends the client's request after `left` bytes.
    #[cfg(target_os = "linux")]
    struct Cut<S> {
        inner: S,
        left: usize,
    }
    #[cfg(target_os = "linux")]
    impl<S: AsyncRead + Unpin> AsyncRead for Cut<S> {
        fn poll_read(
            self: Pin<&mut Self>,
            context: &mut TaskContext<'_>,
            buffer: &mut ReadBuf<'_>,
        ) -> Poll<std::io::Result<()>> {
            let this = self.get_mut();
            let mut chunk = vec![0; buffer.remaining().min(this.left)];
            if chunk.is_empty() {
                return Poll::Ready(Ok(()));
            }
            let mut limited = ReadBuf::new(&mut chunk);
            let result = Pin::new(&mut this.inner).poll_read(context, &mut limited);
            this.left -= limited.filled().len();
            buffer.put_slice(limited.filled());
            result
        }
    }
    #[cfg(target_os = "linux")]
    impl<S: AsyncWrite + Unpin> AsyncWrite for Cut<S> {
        fn poll_write(
            self: Pin<&mut Self>,
            context: &mut TaskContext<'_>,
            buffer: &[u8],
        ) -> Poll<std::io::Result<usize>> {
            Pin::new(&mut self.get_mut().inner).poll_write(context, buffer)
        }
        fn poll_flush(
            self: Pin<&mut Self>,
            context: &mut TaskContext<'_>,
        ) -> Poll<std::io::Result<()>> {
            Pin::new(&mut self.get_mut().inner).poll_flush(context)
        }
        fn poll_shutdown(
            self: Pin<&mut Self>,
            context: &mut TaskContext<'_>,
        ) -> Poll<std::io::Result<()>> {
            Pin::new(&mut self.get_mut().inner).poll_shutdown(context)
        }
    }

    /// A relay that forwards a keyed client's code proof but not the device
    /// key that follows: the host issues nothing and keeps the code, the
    /// client saves nothing, and the same code then pairs, keyed.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn relayed_code_pairing_without_the_device_key_issues_nothing() -> Result<()> {
        use std::os::unix::fs::PermissionsExt;
        // Magic, SPAKE2 message and key confirmation (length-prefixed).
        const BEFORE_KEY: usize = 8 + 2 + 33 + 2 + 32;
        let origin = origin();
        let directory = tempfile::tempdir()?;
        std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700))?;
        let host_token = Pairing {
            token: "c".repeat(64),
            fingerprint: "d".repeat(64),
            device_key: None,
        };
        let access = crate::access::ServerState::open(directory.path(), host_token.clone())?;
        let epoch = std::sync::atomic::AtomicU64::new(0);
        for (attempt, cut) in [("a", Some(BEFORE_KEY)), ("b", None)] {
            let (client, host) = loopback(&origin, &attempt.repeat(32));
            let mut used = false;
            let accept = async {
                // Dropped (closing the stream) as soon as the host is done.
                let mut host = Cut {
                    inner: host,
                    left: cut.unwrap_or(usize::MAX),
                };
                pairing::accept_with_access(
                    &mut host,
                    "123456",
                    &host_token,
                    &mut used,
                    tokio::time::Instant::now() + Duration::from_secs(10),
                    Some(pairing::DeviceEnrollment {
                        access: &access,
                        epoch: &epoch,
                        generation: 0,
                    }),
                )
                .await
            };
            let (client, host) = tokio::join!(
                tokio::time::timeout(PAIR_TIMEOUT, pairing::pair_over(client, "123-456", true)),
                accept,
            );
            let client = client.map_err(anyhow::Error::from).flatten();
            if cut.is_some() {
                assert!(client.is_err() && host.is_err() && !used);
                assert!(access.status().devices.is_empty());
            } else {
                let paired = client?;
                host?;
                assert!(used && paired.device_key.is_some());
                assert!(access.relay_credentials().iter().all(|c| {
                    c.hash == e2e::credential_hash(&host_token.token) || !c.hash_only()
                }));
            }
        }
        Ok(())
    }
}
