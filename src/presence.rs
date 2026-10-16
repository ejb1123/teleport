//! Host presence through a relay: is a saved desktop online right now?
//!
//! While connected to a relay, a host publishes one small object every
//! [`INTERVAL`] on the `presence` broadcast of its namespace. Each object is
//! signed with the host identity key (the key of the certificate clients pin),
//! covers the namespace, a wall-clock timestamp and a sequence number, and
//! carries the certificate so a client can check it against its pin.
//!
//! The relay (or another relay user) therefore cannot claim a host is online:
//! forged, re-signed or foreign-host objects fail verification, and replayed
//! objects are rejected once older than [`MAX_AGE`] or not newer than the last
//! accepted one. It *can* hide presence (drop objects or unannounce), so the
//! worst it achieves is "Offline" for a host that is up, or "Online" for at
//! most [`MAX_AGE`] after a host left, by replaying its last statement.
//! Freshness relies on host and client clocks agreeing within [`MAX_SKEW`].
//!
//! Relay load: one subscription per watching client and one ~1 KB object per
//! host every 30 s.

use crate::relay;
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

/// Broadcast (and track) name within the host's relay namespace.
pub const BROADCAST: &str = "presence";
const TRACK: &str = "presence";
/// How often a host restates its presence.
pub const INTERVAL: Duration = Duration::from_secs(30);
/// A statement older than this no longer counts as online.
pub const MAX_AGE: Duration = Duration::from_secs(75);
/// Accepted clock difference for statements from the future.
pub const MAX_SKEW: Duration = Duration::from_secs(30);
/// Domain separation from the handshake transcript signature.
const LABEL: &[u8] = b"teleport-presence-v1\0";
/// Wait this long after joining the relay before calling a host offline.
const ANNOUNCE_WAIT: Duration = Duration::from_secs(5);
const READ_TIMEOUT: Duration = Duration::from_secs(5);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const RETRY: Duration = Duration::from_secs(30);

/// What a client knows about a host.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    /// No relay configured, relay unreachable, or not checked yet.
    Unknown,
    /// A valid, fresh, signed statement from the pinned host.
    Online,
    /// The relay is reachable but the host is not announcing itself.
    Offline,
}

impl State {
    pub fn label(self) -> &'static str {
        match self {
            Self::Unknown => "Unknown",
            Self::Online => "Online",
            Self::Offline => "Offline",
        }
    }
}

/// One signed presence statement as carried on the relay.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Statement {
    /// The host's TLS certificate (DER); its SHA-256 is the pinned fingerprint.
    pub certificate: Vec<u8>,
    /// Relay namespace the statement is valid for.
    pub namespace: String,
    /// Host wall clock, milliseconds since the Unix epoch.
    pub timestamp_ms: u64,
    /// Increments with every statement of one host process.
    pub sequence: u64,
    pub signature: Vec<u8>,
}

fn signed_message(namespace: &str, timestamp_ms: u64, sequence: u64) -> Vec<u8> {
    let mut message = Vec::with_capacity(64);
    message.extend_from_slice(&(namespace.len() as u32).to_be_bytes());
    message.extend_from_slice(namespace.as_bytes());
    message.extend_from_slice(&timestamp_ms.to_be_bytes());
    message.extend_from_slice(&sequence.to_be_bytes());
    message
}

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_millis() as u64)
}

/// Sign a statement for `namespace` (host side).
#[cfg(target_os = "linux")]
pub fn sign(
    identity: &crate::e2e::HostIdentity,
    namespace: &str,
    timestamp_ms: u64,
    sequence: u64,
) -> Result<Statement> {
    Ok(Statement {
        certificate: identity.certificate().to_vec(),
        namespace: namespace.to_owned(),
        timestamp_ms,
        sequence,
        signature: identity
            .sign_labeled(LABEL, &signed_message(namespace, timestamp_ms, sequence))?,
    })
}

/// Verify a statement against the pinned host fingerprint and the client's
/// clock (`now_ms`). Returns the statement's `(timestamp_ms, sequence)`.
pub fn verify(bytes: &[u8], pinned_fingerprint: &str, now_ms: u64) -> Result<(u64, u64)> {
    let statement: Statement = serde_json::from_slice(bytes).context("malformed presence")?;
    ensure!(
        statement.namespace == relay::namespace(pinned_fingerprint)?,
        "presence is for another namespace"
    );
    crate::e2e::verify_labeled(
        &statement.certificate,
        pinned_fingerprint,
        LABEL,
        &signed_message(
            &statement.namespace,
            statement.timestamp_ms,
            statement.sequence,
        ),
        &statement.signature,
    )?;
    ensure!(
        statement.timestamp_ms <= now_ms.saturating_add(MAX_SKEW.as_millis() as u64),
        "presence is from the future; check the clocks"
    );
    ensure!(
        now_ms.saturating_sub(statement.timestamp_ms) <= MAX_AGE.as_millis() as u64,
        "presence has expired"
    );
    Ok((statement.timestamp_ms, statement.sequence))
}

/// Host side: publish signed presence on the relay until aborted or the
/// relay connection ends. Keep the returned task for as long as the link.
#[cfg(target_os = "linux")]
pub async fn publish(
    outgoing: moq_net::origin::Producer,
    identity: std::sync::Arc<crate::e2e::HostIdentity>,
    namespace: String,
) -> Result<()> {
    let mut broadcast = outgoing.create_broadcast(
        BROADCAST,
        moq_net::broadcast::Route::new().with_announce(true),
    )?;
    let mut track =
        broadcast.create_track(TRACK, moq_net::track::Info::default().with_ordered(true))?;
    let mut ticker = tokio::time::interval(INTERVAL);
    let mut sequence = 0u64;
    loop {
        ticker.tick().await;
        let statement = sign(&identity, &namespace, now_ms(), sequence)?;
        sequence += 1;
        let mut group = track.append_group()?;
        group.write_frame(
            moq_net::Timestamp::now(),
            bytes::Bytes::from(serde_json::to_vec(&statement)?),
        )?;
        group.finish()?;
    }
}

/// Client side: follow one host's presence over an established relay link,
/// reporting every change to `state`. Returns when the relay connection ends.
pub async fn observe(
    link: &relay::Link,
    fingerprint: &str,
    state: &tokio::sync::watch::Sender<State>,
) -> Result<()> {
    let consumer = link.incoming.consume();
    let mut announced = consumer
        .scope(&[moq_net::Path::new(BROADCAST)])
        .context("presence path unavailable")?
        .announced();
    let mut subscriber: Option<moq_net::track::Subscriber> = None;
    let mut last: Option<(u64, u64)> = None;
    let mut online_until: Option<tokio::time::Instant> = None;
    let decide_at = tokio::time::Instant::now() + ANNOUNCE_WAIT;
    loop {
        let now = tokio::time::Instant::now();
        let current = match online_until {
            Some(until) if until > now => State::Online,
            Some(_) => State::Offline,
            None if now >= decide_at => State::Offline,
            None => State::Unknown,
        };
        state.send_if_modified(|previous| {
            let changed = *previous != current;
            *previous = current;
            changed
        });
        let wake = match online_until {
            Some(until) if until > now => until,
            _ if now < decide_at => decide_at,
            _ => now + INTERVAL,
        };
        tokio::select! {
            announce = announced.next() => {
                let announce = announce.context("relay connection closed")?;
                if announce.path.as_str() != BROADCAST {
                    continue;
                }
                match announce.broadcast {
                    Some(broadcast) => {
                        let track = broadcast.track(TRACK)?;
                        subscriber = tokio::time::timeout(
                            READ_TIMEOUT,
                            track.subscribe(moq_net::track::Subscription::default()),
                        )
                        .await
                        .ok()
                        .and_then(Result::ok);
                    }
                    None => {
                        // The host left the relay (or the relay says so).
                        subscriber = None;
                        online_until = Some(tokio::time::Instant::now());
                    }
                }
            }
            group = async {
                match subscriber.as_mut() {
                    Some(subscriber) => subscriber.recv_group().await,
                    None => std::future::pending().await,
                }
            } => {
                let Ok(Some(mut group)) = group else {
                    subscriber = None;
                    continue;
                };
                let Ok(Ok(Some(bytes))) = tokio::time::timeout(
                    READ_TIMEOUT,
                    crate::protocol::read_frame(&mut group, crate::protocol::MAX_CONTROL),
                )
                .await
                else {
                    continue;
                };
                let now_ms = now_ms();
                match verify(&bytes, fingerprint, now_ms) {
                    Ok(stamp) if last.is_none_or(|last| stamp > last) => {
                        last = Some(stamp);
                        let remaining = (stamp.0 + MAX_AGE.as_millis() as u64).saturating_sub(now_ms);
                        online_until = Some(
                            tokio::time::Instant::now()
                                + Duration::from_millis(remaining).min(MAX_AGE),
                        );
                    }
                    Ok(_) => tracing::debug!("ignoring replayed presence"),
                    Err(error) => tracing::debug!(%error, "ignoring invalid presence"),
                }
            }
            _ = tokio::time::sleep_until(wake) => {}
            error = link.session.closed() => anyhow::bail!("relay closed the connection: {error}"),
        }
    }
}

/// Follow a host's presence indefinitely, reconnecting to the relay as
/// needed. `state` reads Unknown while the relay is unreachable.
pub async fn watch(
    endpoint: relay::Endpoint,
    fingerprint: String,
    state: tokio::sync::watch::Sender<State>,
) {
    let Ok(namespace) = relay::namespace(&fingerprint) else {
        return;
    };
    loop {
        match tokio::time::timeout(
            CONNECT_TIMEOUT,
            relay::connect_with_keep_alive(&endpoint, &namespace, relay::IDLE_KEEP_ALIVE),
        )
        .await
        {
            Ok(Ok(link)) => {
                if let Err(error) = observe(&link, &fingerprint, &state).await {
                    tracing::debug!(%error, "presence watch ended");
                }
            }
            Ok(Err(error)) => tracing::debug!(%error, "relay unavailable for presence"),
            Err(_) => tracing::debug!("relay connection timed out for presence"),
        }
        state.send_replace(State::Unknown);
        tokio::time::sleep(RETRY).await;
    }
}

/// One-shot check: connect, wait for a verdict (at most `wait`), disconnect.
pub async fn check(endpoint: &relay::Endpoint, fingerprint: &str, wait: Duration) -> State {
    let attempt = async {
        let link = tokio::time::timeout(
            CONNECT_TIMEOUT,
            relay::connect_with_keep_alive(
                endpoint,
                &relay::namespace(fingerprint)?,
                relay::IDLE_KEEP_ALIVE,
            ),
        )
        .await
        .context("relay connection timed out")??;
        let (sender, mut receiver) = tokio::sync::watch::channel(State::Unknown);
        let verdict = tokio::select! {
            result = observe(&link, fingerprint, &sender) => { result?; State::Unknown }
            changed = receiver.wait_for(|state| *state != State::Unknown) => {
                changed.map_or(State::Unknown, |state| *state)
            }
        };
        Ok::<_, anyhow::Error>(verdict)
    };
    match tokio::time::timeout(wait, attempt).await {
        Ok(Ok(state)) => state,
        Ok(Err(error)) => {
            tracing::warn!(%error, "presence check failed");
            State::Unknown
        }
        Err(_) => State::Unknown,
    }
}

#[derive(clap::Args)]
pub struct Options {
    /// A saved desktop's address: use its relay settings and pinned identity.
    pub address: Option<String>,
    /// Relay URL (instead of a saved desktop).
    #[arg(long, env = "TELEPORT_RELAY_URL", hide_env_values = true)]
    pub relay: Option<url::Url>,
    /// Pin a self-hosted relay's certificate.
    #[arg(long)]
    pub relay_fingerprint: Option<String>,
    /// Access key for a relay started with `teleport relay --key`.
    #[arg(long, env = "TELEPORT_RELAY_KEY", hide_env_values = true)]
    pub relay_key: Option<String>,
    /// The host's pinned certificate fingerprint (instead of a saved desktop).
    #[arg(long)]
    pub host_fingerprint: Option<String>,
    /// Give up (and report Unknown) after this many seconds.
    #[arg(long, default_value_t = 15)]
    pub wait: u64,
}

/// `teleport presence`: print ONLINE, OFFLINE or UNKNOWN for one host.
pub async fn run(options: Options) -> Result<()> {
    let (endpoint, fingerprint) = match &options.address {
        Some(address) => {
            let profiles = crate::profiles::load()?;
            let profile = profiles
                .iter()
                .find(|profile| &profile.address == address)
                .context("no saved desktop has this address")?;
            (
                profile
                    .relay
                    .as_ref()
                    .context("this desktop has no relay settings")?
                    .endpoint()?,
                profile
                    .trusted_fingerprint()?
                    .context("this desktop's identity is not approved yet")?,
            )
        }
        None => (
            relay::Endpoint {
                url: options
                    .relay
                    .clone()
                    .context("pass a saved desktop address, or --relay and --host-fingerprint")?,
                fingerprint: options.relay_fingerprint.clone(),
                key: options.relay_key.clone(),
            },
            options
                .host_fingerprint
                .clone()
                .context("--host-fingerprint is required with --relay")?,
        ),
    };
    moq_native::tls::parse_fingerprint(&fingerprint)
        .map_err(|error| anyhow::anyhow!("invalid host fingerprint: {error}"))?;
    let state = check(&endpoint, &fingerprint, Duration::from_secs(options.wait)).await;
    println!("{}", state.label().to_ascii_uppercase());
    Ok(())
}

/// A desktop whose presence should be followed.
#[derive(Clone)]
pub struct Target {
    pub address: String,
    pub endpoint: relay::Endpoint,
    /// Pinned host fingerprint.
    pub fingerprint: String,
}

/// Background presence for the launcher: never blocks the UI thread.
pub struct Monitor {
    runtime: Option<tokio::runtime::Runtime>,
    watches: HashMap<
        String,
        (
            tokio::task::JoinHandle<()>,
            tokio::sync::watch::Receiver<State>,
        ),
    >,
}

impl Default for Monitor {
    fn default() -> Self {
        Self::new()
    }
}

impl Monitor {
    pub fn new() -> Self {
        Self {
            runtime: None,
            watches: HashMap::new(),
        }
    }

    /// Replace the followed desktops. Cheap when nothing changed is up to the
    /// caller: every call restarts all watches.
    pub fn follow(&mut self, targets: Vec<Target>) {
        for (_, (task, _)) in self.watches.drain() {
            task.abort();
        }
        if targets.is_empty() {
            return;
        }
        if self.runtime.is_none() {
            match tokio::runtime::Builder::new_multi_thread()
                .worker_threads(1)
                .thread_name("teleport-presence")
                .enable_all()
                .build()
            {
                Ok(runtime) => self.runtime = Some(runtime),
                Err(error) => {
                    tracing::warn!(%error, "presence monitor unavailable");
                    return;
                }
            }
        }
        let runtime = self.runtime.as_ref().expect("runtime was just created");
        for target in targets {
            let (sender, receiver) = tokio::sync::watch::channel(State::Unknown);
            let task = runtime.spawn(watch(target.endpoint, target.fingerprint, sender));
            self.watches.insert(target.address, (task, receiver));
        }
    }

    pub fn state(&self, address: &str) -> State {
        self.watches
            .get(address)
            .map_or(State::Unknown, |(_, receiver)| *receiver.borrow())
    }
}

impl Drop for Monitor {
    fn drop(&mut self) {
        for (_, (task, _)) in self.watches.drain() {
            task.abort();
        }
        if let Some(runtime) = self.runtime.take() {
            runtime.shutdown_background();
        }
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;

    fn host() -> (crate::e2e::HostIdentity, String) {
        let certified = rcgen::generate_simple_self_signed(vec!["teleport.local".into()]).unwrap();
        let der = certified.cert.der().to_vec();
        let fingerprint: String = ring::digest::digest(&ring::digest::SHA256, &der)
            .as_ref()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        (
            crate::e2e::HostIdentity::new(der, &certified.signing_key.serialize_der()).unwrap(),
            fingerprint,
        )
    }

    #[test]
    fn signed_fresh_presence_verifies_against_the_pin() -> Result<()> {
        let (identity, fingerprint) = host();
        let namespace = relay::namespace(&fingerprint)?;
        let now = now_ms();
        let statement = serde_json::to_vec(&sign(&identity, &namespace, now, 7)?)?;
        assert_eq!(verify(&statement, &fingerprint, now)?, (now, 7));
        // Slightly fast host clocks and the whole freshness window are fine.
        assert!(verify(&statement, &fingerprint, now - 10_000).is_ok());
        assert!(verify(&statement, &fingerprint, now + MAX_AGE.as_millis() as u64).is_ok());
        Ok(())
    }

    #[test]
    fn forged_expired_and_foreign_presence_is_rejected() -> Result<()> {
        let (identity, fingerprint) = host();
        let (other, other_fingerprint) = host();
        let namespace = relay::namespace(&fingerprint)?;
        let now = now_ms();
        let valid = sign(&identity, &namespace, now, 1)?;

        // Expired, or claiming to be from the future.
        let old = serde_json::to_vec(&sign(&identity, &namespace, now - 76_000, 1)?)?;
        assert!(verify(&old, &fingerprint, now).is_err());
        let future = serde_json::to_vec(&sign(&identity, &namespace, now + 31_000, 1)?)?;
        assert!(verify(&future, &fingerprint, now).is_err());

        // A relay altering the timestamp or sequence breaks the signature.
        let mut altered = valid.clone();
        altered.timestamp_ms += 1;
        assert!(verify(&serde_json::to_vec(&altered)?, &fingerprint, now).is_err());
        let mut altered = valid.clone();
        altered.sequence += 1;
        assert!(verify(&serde_json::to_vec(&altered)?, &fingerprint, now).is_err());

        // Another host's key, even with our certificate attached, or its own
        // certificate and namespace: not the pinned host.
        let mut forged = sign(&other, &namespace, now, 1)?;
        assert!(verify(&serde_json::to_vec(&forged)?, &fingerprint, now).is_err());
        forged.certificate = valid.certificate.clone();
        assert!(verify(&serde_json::to_vec(&forged)?, &fingerprint, now).is_err());
        let foreign = sign(&other, &relay::namespace(&other_fingerprint)?, now, 1)?;
        assert!(verify(&serde_json::to_vec(&foreign)?, &fingerprint, now).is_err());

        // Our signature for a different namespace does not transfer.
        let elsewhere = sign(&identity, &relay::namespace(&other_fingerprint)?, now, 1)?;
        assert!(verify(&serde_json::to_vec(&elsewhere)?, &fingerprint, now).is_err());

        // A handshake-style signature label cannot be produced through this API.
        assert!(identity.sign_labeled(b"", b"x").is_err());
        assert!(verify(b"not json", &fingerprint, now).is_err());
        Ok(())
    }
}
