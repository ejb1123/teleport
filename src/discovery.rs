//! Announcements for relays without namespace discovery.
//!
//! On a moq-lite relay (`teleport relay`) each side learns the other's
//! broadcasts from the relay's announcements. A moq-transport relay need not
//! offer them: Cloudflare's draft-14 relay ignores SUBSCRIBE_NAMESPACE and
//! forwards a SUBSCRIBE only for a track whose full name the subscriber
//! already knows. On a moq-transport session (see `crate::relay::Link`) the
//! peers therefore announce to each other through ordinary tracks:
//!
//! * Each side publishes a *door* broadcast: the host at [`HOST_DOOR`], every
//!   client connection at `<id>/door` for a random `<id>`.
//! * Track `announce/<asker>/<n>` on a door is that side's announcement stream
//!   for `<asker>`: one group whose frames are `+<path>` and `-<path>` as its
//!   broadcasts (doors excluded) appear and disappear.
//! * A client reads the host door with its own id as `<asker>`. That request is
//!   also how the host learns the client exists: it reads `<asker>/door` back.
//!
//! Every announced path becomes a broadcast in the reader's incoming origin
//! whose tracks are subscribed on the relay by name, so session code sees
//! announcements exactly as from a moq-lite relay. A side that leaves ends the
//! other's subscription to its door, which unannounces everything it had
//! announced (a relay forwards a publisher leaving, not a subscriber).
//!
//! The streams carry only broadcast paths (random session and attempt ids with
//! fixed suffixes), which the relay sees anyway in the track namespaces; every
//! payload stays end-to-end encrypted by `crate::bridge` and the handshakes.

use crate::relay::AbortOnDrop;
use anyhow::{Context as _, Result, ensure};
use std::{
    collections::HashMap,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

/// The host's door: well known, so a client needs nothing but the namespace.
pub const HOST_DOOR: &str = "door";
const DOOR_SUFFIX: &str = "/door";
const ANNOUNCE: &str = "announce";
/// Host side: client connections followed at once (each is one session or
/// enrollment attempt); later ones are refused until a slot frees.
const MAX_PEERS: usize = 32;
/// Paths accepted from one peer's announcement stream.
const MAX_PATHS: usize = 64;
const MAX_PATH: usize = 160;
/// How long a relay may take to route a new broadcast after we publish it
/// (Cloudflare: 130-270 ms measured). A SUBSCRIBE sooner fails, and each one
/// counts against the connection's request limit, so a broadcast is announced
/// to the peer only this long after it appeared.
const ROUTE_DELAY: Duration = Duration::from_millis(300);
/// A client's door, likewise; it gets a few tries.
const DOOR_ATTEMPTS: u32 = 4;
/// The host may be offline, so a client keeps asking, more and more slowly.
const RETRY_MIN: Duration = Duration::from_secs(1);
const RETRY_MAX: Duration = Duration::from_secs(30);
/// Tries per relayed track, for a relay slower than [`ROUTE_DELAY`].
const SUBSCRIBE_ATTEMPTS: u32 = 3;
/// Largest relayed frame a proxy copies (a sealed `protocol::MAX_FRAME`).
const MAX_FRAME: usize = crate::protocol::MAX_FRAME + 64;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Role {
    /// Owns [`HOST_DOOR`] and follows every client that asks.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    Host,
    /// Follows the host door.
    Client,
}

/// Running discovery for one relay connection; dropping it stops everything.
pub struct Discovery {
    requests: Arc<AtomicU64>,
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    heard: Arc<std::sync::Mutex<tokio::time::Instant>>,
    _tasks: Vec<AbortOnDrop>,
}

impl Discovery {
    /// moq-transport requests (SUBSCRIBE, PUBLISH_NAMESPACE) issued so far.
    pub fn requests(&self) -> u64 {
        self.requests.load(Ordering::Relaxed)
    }

    /// How long since a peer last announced a broadcast to us.
    #[cfg(target_os = "linux")]
    pub fn quiet(&self) -> Duration {
        self.heard.lock().unwrap().elapsed()
    }
}

fn is_door(path: &str) -> bool {
    path == HOST_DOOR || path.ends_with(DOOR_SUFFIX)
}

/// A path a peer may announce: a few short segments of safe characters.
fn valid_path(path: &str) -> bool {
    path.len() <= MAX_PATH
        && !is_door(path)
        && path.split('/').count() <= 4
        && path.split('/').all(|segment| {
            !segment.is_empty()
                && segment
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
        })
}

/// `announce/<asker>/<n>` → `<asker>`.
fn asker(track: &str) -> Option<&str> {
    let mut parts = track.split('/');
    let (Some(ANNOUNCE), Some(asker), Some(n), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return None;
    };
    (crate::relay::valid_namespace(asker) && n.parse::<u64>().is_ok()).then_some(asker)
}

/// Start discovery on a moq-transport relay connection. `outgoing` is what
/// we publish (rooted at the namespace); `network` is the session's
/// subscribe origin. Returns the incoming origin session code reads.
pub fn start(
    session: moq_net::Session,
    role: Role,
    namespace: &str,
    outgoing: moq_net::origin::Producer,
    network: moq_net::origin::Producer,
) -> Result<(moq_net::origin::Producer, Discovery)> {
    let requests = Arc::new(AtomicU64::new(0));
    let heard = Arc::new(std::sync::Mutex::new(tokio::time::Instant::now()));
    let incoming = moq_net::Origin::random().produce();
    let mirror = Mirror {
        namespace: namespace.to_owned(),
        id: crate::relay::hex(&crate::e2e::random::<16>()),
        network: network.consume(),
        incoming: incoming.clone(),
        requests: requests.clone(),
        published: Arc::default(),
        heard: heard.clone(),
    };
    let door_path = match role {
        Role::Host => HOST_DOOR.to_owned(),
        Role::Client => format!("{}{DOOR_SUFFIX}", mirror.id),
    };
    let door = outgoing.create_broadcast(
        door_path.as_str(),
        moq_net::broadcast::Route::new().with_announce(true),
    )?;
    let mut tasks = vec![
        AbortOnDrop(tokio::spawn(record_announcements(
            outgoing.consume(),
            mirror.clone(),
        ))),
        AbortOnDrop(tokio::spawn(serve_door(
            door,
            outgoing.consume(),
            role,
            mirror.clone(),
        ))),
    ];
    if role == Role::Client {
        tasks.push(AbortOnDrop(tokio::spawn(follow_host(mirror, session))));
    }
    Ok((
        incoming,
        Discovery {
            requests,
            heard,
            _tasks: tasks,
        },
    ))
}

/// What following a peer needs: where its broadcasts live and where ours go.
#[derive(Clone)]
struct Mirror {
    namespace: String,
    /// Our random id: the `<asker>` in our requests, and our door if a client.
    id: String,
    network: moq_net::origin::Consumer,
    incoming: moq_net::origin::Producer,
    requests: Arc<AtomicU64>,
    /// When each of our broadcasts appeared (see [`ROUTE_DELAY`]).
    published: Arc<std::sync::Mutex<HashMap<String, tokio::time::Instant>>>,
    /// When a peer last announced a broadcast to us.
    heard: Arc<std::sync::Mutex<tokio::time::Instant>>,
}

impl Mirror {
    /// Subscribe to `track` of the peer's `path` on the relay, by name.
    async fn subscribe(
        &self,
        path: &str,
        track: &str,
        subscription: moq_net::track::Subscription,
    ) -> Result<Remote> {
        let broadcast = self
            .network
            .request_broadcast(format!("{}/{path}", self.namespace).as_str())
            .await?;
        self.requests.fetch_add(1, Ordering::Relaxed);
        Ok(Remote {
            track: broadcast.track(track)?.subscribe(subscription).await?,
            _broadcast: broadcast,
        })
    }
}

/// A track subscribed on the relay by name. Holds its broadcast: the relay
/// session serves an unannounced broadcast only while someone holds it.
struct Remote {
    track: moq_net::track::Subscriber,
    _broadcast: moq_net::broadcast::Consumer,
}

impl Remote {
    async fn recv_group(&mut self) -> Result<Option<moq_net::group::Consumer>> {
        Ok(self.track.recv_group().await?)
    }
}

/// Note when each of our broadcasts appears; each costs one PUBLISH_NAMESPACE.
async fn record_announcements(outgoing: moq_net::origin::Consumer, mirror: Mirror) -> Result<()> {
    let mut announced = outgoing.announced();
    while let Some(announce) = announced.next().await {
        let path = announce.path.to_string();
        let mut published = mirror.published.lock().unwrap();
        if announce.broadcast.is_some() {
            mirror.requests.fetch_add(1, Ordering::Relaxed);
            published.insert(path, tokio::time::Instant::now());
        } else {
            published.remove(&path);
        }
    }
    Ok(())
}

/// One peer's announcement stream (and, on the host, the following of it).
struct Peer {
    generation: u64,
    _task: AbortOnDrop,
}

/// Answer requests on our door: announcement streams, and (host) following
/// each client that asks.
async fn serve_door(
    mut door: moq_net::broadcast::Producer,
    outgoing: moq_net::origin::Consumer,
    role: Role,
    mirror: Mirror,
) -> Result<()> {
    let mut requests = door.dynamic();
    let mut peers: HashMap<String, Peer> = HashMap::new();
    let (ended, mut ends) = tokio::sync::mpsc::unbounded_channel::<(String, u64)>();
    let mut generation = 0u64;
    let limit = match role {
        Role::Host => MAX_PEERS,
        Role::Client => 2,
    };
    loop {
        let request = tokio::select! {
            request = requests.requested_track() => match request {
                Ok(request) => request,
                Err(_) => break,
            },
            Some((peer, ended)) = ends.recv() => {
                // Only the task that ended, not a newer one for the same peer.
                if peers.get(&peer).is_some_and(|task| task.generation == ended) {
                    peers.remove(&peer);
                }
                continue;
            }
        };
        let Some(peer) = asker(request.name()).map(str::to_owned) else {
            request.reject(moq_net::Error::NotFound);
            continue;
        };
        if !peers.contains_key(&peer) && peers.len() >= limit {
            tracing::debug!("relay door busy; refusing a request");
            request.reject(moq_net::Error::NotFound);
            continue;
        }
        let stream = request.accept(moq_net::track::Info::default().with_ordered(true));
        generation += 1;
        let (outgoing, mirror, ended, name) = (
            outgoing.clone(),
            mirror.clone(),
            ended.clone(),
            peer.clone(),
        );
        let task = tokio::spawn(async move {
            match role {
                // A client is followed while its door answers; our stream to
                // it ends with it (the relay never says a subscriber left).
                Role::Host => {
                    let door = format!("{name}{DOOR_SUFFIX}");
                    let follow = async {
                        // Its door was published just before it asked.
                        tokio::time::sleep(ROUTE_DELAY).await;
                        follow(&mirror, &door, DOOR_ATTEMPTS, &mut false).await
                    };
                    tokio::select! {
                        _ = announce(outgoing, stream, &mirror) => {}
                        result = follow => {
                            if let Err(error) = result {
                                tracing::debug!(error = format!("{error:#}"), "relayed client door ended");
                            }
                        }
                    }
                }
                Role::Client => {
                    let _ = announce(outgoing, stream, &mirror).await;
                }
            }
            let _ = ended.send((name, generation));
            Ok(())
        });
        // Replaces (and so stops) an older stream for the same peer.
        peers.insert(
            peer,
            Peer {
                generation,
                _task: AbortOnDrop(task),
            },
        );
    }
    door.finish();
    Ok(())
}

/// Write our announcements (doors excluded) to one peer until it goes away.
/// A new broadcast is announced once the relay can route it ([`ROUTE_DELAY`]).
async fn announce(
    outgoing: moq_net::origin::Consumer,
    mut stream: moq_net::track::Producer,
    mirror: &Mirror,
) -> Result<()> {
    let mut group = stream.append_group()?;
    let mut write = |sign: char, path: &str| {
        tracing::debug!(%sign, path, "announcing to a relay peer");
        group.write_frame(
            moq_net::Timestamp::now(),
            bytes::Bytes::from(format!("{sign}{path}")),
        )
    };
    let mut announced = outgoing.announced();
    // Announced locally, not yet to the peer: (when to, path).
    let mut pending: Vec<(tokio::time::Instant, String)> = Vec::new();
    loop {
        let due = pending.iter().map(|(at, _)| *at).min();
        tokio::select! {
            announce = announced.next() => {
                let announce = announce.context("outgoing origin closed")?;
                let path = announce.path.to_string();
                if is_door(&path) {
                    continue;
                }
                if announce.broadcast.is_some() {
                    let since = mirror.published.lock().unwrap().get(&path).copied();
                    let since = since.unwrap_or_else(tokio::time::Instant::now);
                    pending.push((since + ROUTE_DELAY, path));
                } else if let Some(index) = pending.iter().position(|(_, held)| *held == path) {
                    // Gone before the peer heard of it.
                    pending.swap_remove(index);
                } else {
                    write('-', &path)?;
                }
            }
            _ = tokio::time::sleep_until(due.unwrap_or_else(tokio::time::Instant::now)), if due.is_some() => {
                let now = tokio::time::Instant::now();
                let (ready, waiting) = pending.into_iter().partition(|(at, _)| *at <= now);
                pending = waiting;
                for (_, path) in ready {
                    write('+', &path)?;
                }
            }
            _ = stream.unused() => return Ok(()),
        }
    }
}

/// Follow a peer's door: mirror its announcements into our incoming origin
/// until the door goes away. `attempts` bounds tries before it first answers;
/// `opened` says whether it did.
async fn follow(mirror: &Mirror, door: &str, attempts: u32, opened: &mut bool) -> Result<()> {
    let track = format!("{ANNOUNCE}/{}/{}", mirror.id, rand::random::<u32>());
    let mut attempt = 0;
    let (mut subscriber, mut group) = loop {
        attempt += 1;
        let open = async {
            let mut subscriber = mirror
                .subscribe(
                    door,
                    &track,
                    moq_net::track::Subscription::default().with_ordered(true),
                )
                .await?;
            let group = subscriber.recv_group().await?.context("door closed")?;
            Ok::<_, anyhow::Error>((subscriber, group))
        };
        match open.await {
            Ok(subscription) => {
                tracing::debug!(door, "following a relay door");
                *opened = true;
                break subscription;
            }
            Err(error) if attempt >= attempts => return Err(error),
            Err(_) => tokio::time::sleep(ROUTE_DELAY).await,
        }
    };
    let mut proxies: HashMap<String, Proxy> = HashMap::new();
    loop {
        let frame = tokio::select! {
            frame = crate::protocol::read_frame(&mut group, MAX_PATH + 1) => frame?,
            // The door's track ending (its publisher left) ends the stream.
            next = subscriber.recv_group() => match next? {
                Some(_) => continue,
                None => None,
            },
        };
        let Some(frame) = frame else {
            return Ok(());
        };
        let Ok(text) = std::str::from_utf8(&frame) else {
            continue;
        };
        let (sign, path) = text.split_at(text.len().min(1));
        tracing::debug!(door, sign, path, "relay peer announcement");
        if !valid_path(path) {
            continue;
        }
        match sign {
            "+" if !proxies.contains_key(path) && proxies.len() < MAX_PATHS => {
                *mirror.heard.lock().unwrap() = tokio::time::Instant::now();
                match Proxy::start(mirror, path) {
                    Ok(proxy) => {
                        proxies.insert(path.to_owned(), proxy);
                    }
                    Err(error) => {
                        tracing::debug!(%error, path, "cannot mirror a relayed broadcast")
                    }
                }
            }
            "-" => {
                proxies.remove(path);
            }
            _ => {}
        }
    }
}

/// Client side: follow the host door for the life of the connection, again
/// whenever the host comes back.
async fn follow_host(mirror: Mirror, session: moq_net::Session) -> Result<()> {
    let mut wait = RETRY_MIN;
    loop {
        let mut opened = false;
        tokio::select! {
            result = follow(&mirror, HOST_DOOR, 1, &mut opened) => {
                if let Err(error) = result {
                    tracing::debug!(error = format!("{error:#}"), "host door unavailable");
                }
            }
            _ = session.closed() => return Ok(()),
        }
        // A door that answered was the host leaving: it may be back soon.
        if opened {
            wait = RETRY_MIN;
        }
        tokio::time::sleep(wait).await;
        wait = (wait * 2).min(RETRY_MAX);
    }
}

/// A peer's broadcast mirrored into our incoming origin: each track a local
/// reader asks for is subscribed on the relay by name and copied.
struct Proxy {
    broadcast: moq_net::broadcast::Producer,
    _task: AbortOnDrop,
}

impl Proxy {
    fn start(mirror: &Mirror, path: &str) -> Result<Self> {
        ensure!(valid_path(path), "invalid relayed path");
        // Announced only once its track requests have somewhere to go: the
        // origin may announce from another thread as soon as it exists.
        let mut broadcast = mirror
            .incoming
            .create_broadcast(path, moq_net::broadcast::Route::new())?;
        let mut requests = broadcast.dynamic();
        broadcast.set_route(moq_net::broadcast::Route::new().with_announce(true))?;
        let (mirror, path) = (mirror.clone(), path.to_owned());
        let task = tokio::spawn(async move {
            let mut tracks = tokio::task::JoinSet::new();
            while let Ok(request) = requests.requested_track().await {
                while tracks.try_join_next().is_some() {}
                let name = request.name().to_owned();
                let subscription = request.subscription().unwrap_or_default();
                let local = request.accept(moq_net::track::Info::default());
                let (mirror, path) = (mirror.clone(), path.clone());
                tracks.spawn(async move {
                    if let Err(error) = copy(&mirror, &path, &name, subscription, local).await {
                        tracing::debug!(%error, path, name, "relayed track ended");
                    }
                });
            }
            Ok(())
        });
        Ok(Self {
            broadcast,
            _task: AbortOnDrop(task),
        })
    }
}

impl Drop for Proxy {
    fn drop(&mut self) {
        self.broadcast.finish();
    }
}

/// Copy one relayed track into a local one until either side ends.
async fn copy(
    mirror: &Mirror,
    path: &str,
    name: &str,
    subscription: moq_net::track::Subscription,
    mut local: moq_net::track::Producer,
) -> Result<()> {
    let mut attempt = 0;
    let opened = loop {
        attempt += 1;
        let opened = async {
            let mut remote = mirror.subscribe(path, name, subscription.clone()).await?;
            let first = remote.recv_group().await?;
            Ok::<_, anyhow::Error>((remote, first))
        };
        let opened = tokio::select! {
            opened = opened => opened,
            _ = local.unused() => return Ok(()),
        };
        match opened {
            Ok(opened) => break Ok(opened),
            Err(error) if attempt >= SUBSCRIBE_ATTEMPTS => break Err(error),
            Err(_) => tokio::time::sleep(ROUTE_DELAY).await,
        }
    };
    let (mut remote, mut next) = match opened {
        Ok(opened) => opened,
        Err(error) => {
            let _ = local.abort(moq_net::Error::NotFound);
            return Err(error);
        }
    };
    let mut groups = tokio::task::JoinSet::new();
    loop {
        let Some(mut group) = next else {
            while groups.join_next().await.is_some() {}
            local.finish()?;
            return Ok(());
        };
        while groups.try_join_next().is_some() {}
        let mut out = local.create_group(moq_net::group::Info {
            sequence: group.sequence,
        })?;
        groups.spawn(async move {
            while let Some((timestamp, _, frame)) =
                crate::protocol::read_timed_frame(&mut group, MAX_FRAME).await?
            {
                out.write_frame(timestamp, frame)?;
            }
            out.finish()?;
            Ok::<_, anyhow::Error>(())
        });
        let received = tokio::select! {
            received = remote.recv_group() => received,
            _ = local.unused() => return Ok(()),
        };
        next = match received {
            Ok(next) => next,
            Err(error) => {
                let _ = local.abort(moq_net::Error::Cancel);
                return Err(error);
            }
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_and_requests_are_validated() {
        let session = "0123456789abcdef0123456789abcdef";
        assert!(valid_path(&format!("{session}/hello")));
        assert!(valid_path("presence"));
        assert!(!valid_path(""));
        assert!(!valid_path("door"));
        assert!(!valid_path(&format!("{session}/door")));
        assert!(!valid_path("a//b"));
        assert!(!valid_path("a/b/c/d/e"));
        assert!(!valid_path("a b"));
        assert!(!valid_path(&"a".repeat(MAX_PATH + 1)));
        assert_eq!(asker(&format!("announce/{session}/7")), Some(session));
        assert_eq!(asker(&format!("announce/{session}")), None);
        assert_eq!(asker(&format!("announce/{session}/x")), None);
        assert_eq!(asker("announce/zz/1"), None);
        assert_eq!(asker(&format!("other/{session}/1")), None);
    }
}
