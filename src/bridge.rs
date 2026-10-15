//! Crypto bridge between a session's plaintext MoQ origins and an untrusted
//! relay.
//!
//! Session code is unchanged in relay mode: it publishes and subscribes on
//! local, in-process origins exactly as on a direct connection. This module
//! is the only place relayed objects are sealed and opened:
//!
//! * [`export`] serves the relay's requests for one of our broadcasts by
//!   subscribing to the local plaintext track and sealing every object.
//! * [`import`] serves local requests for the peer's broadcast by subscribing
//!   on the relay and opening every object before the session sees it.
//!
//! Group sequences are preserved so session logic (keyframe groups, video
//! generations, ordered control groups) behaves identically. Each track name
//! has exactly one sealer and one opener for the whole session, so a repeated
//! subscription can never reuse a nonce or reset replay protection.

use crate::{
    e2e::{Direction, Opener, Order, Sealer, SessionKeys},
    protocol,
};
use anyhow::{Context, Result};
use std::{collections::HashMap, sync::Arc, time::Duration};
use tokio::sync::Mutex;

/// Objects grow by the counter prefix and authentication tag when sealed.
const SEAL_OVERHEAD: usize = 8 + 16;
/// A relayed media group that stops delivering frames this long is abandoned.
const RELAYED_FRAME_TIMEOUT: Duration = Duration::from_secs(10);

/// Ordered tracks carry state the session must see completely and in order.
fn order(track: &str) -> Order {
    if matches!(track, "input" | "updates" | "desktop") || track.starts_with("ssh-agent") {
        Order::Strict
    } else {
        Order::Window
    }
}

/// Network-side subscription mirroring what the session itself requests.
fn network_subscription(track: &str) -> moq_net::track::Subscription {
    match track {
        "input" => moq_net::track::Subscription::default()
            .with_ordered(true)
            .with_priority(255)
            .with_latency_max(protocol::INPUT_TIMEOUT)
            .with_group_start(0),
        protocol::POINTER_TRACK => moq_net::track::Subscription::default().with_priority(255),
        "updates" => moq_net::track::Subscription::default()
            .with_ordered(true)
            .with_group_start(0)
            .with_priority(crate::media::UPDATES_PRIORITY),
        "desktop" => moq_net::track::Subscription::default().with_group_start(0),
        "opus" => crate::media::audio_subscription(),
        name if name.starts_with("ssh-agent") => crate::ssh_agent::subscription(),
        _ => crate::media::video_subscription(),
    }
}

/// Local subscription for tracks we export: complete history for ordered
/// tracks, newest-first otherwise.
fn local_subscription(track: &str) -> moq_net::track::Subscription {
    match order(track) {
        Order::Strict => moq_net::track::Subscription::default()
            .with_ordered(true)
            .with_group_start(0),
        Order::Window => moq_net::track::Subscription::default(),
    }
}

fn track_info(track: &str) -> moq_net::track::Info {
    match track {
        "input" => moq_net::track::Info::default()
            .with_ordered(true)
            .with_priority(255)
            .with_latency_max(protocol::INPUT_TIMEOUT),
        protocol::POINTER_TRACK => moq_net::track::Info::default().with_priority(255),
        "updates" => moq_net::track::Info::default().with_ordered(true),
        name if name == "desktop" || name.starts_with("ssh-agent") => {
            moq_net::track::Info::default().with_ordered(true)
        }
        "opus" => moq_net::track::Info::default(),
        _ => moq_net::track::Info::default().with_latency_max(Duration::from_millis(500)),
    }
}

/// Serve the relay's requests for `network` from the local plaintext
/// broadcast, sealing in `direction`. Runs until the broadcast closes.
pub fn export(
    local: impl std::future::Future<Output = Result<moq_net::broadcast::Consumer>> + Send + 'static,
    network: moq_net::broadcast::Producer,
    keys: Arc<SessionKeys>,
    direction: Direction,
) -> impl std::future::Future<Output = Result<()>> + Send + 'static {
    // Register before waiting for the local broadcast: requests that arrive
    // meanwhile must queue, not be answered "not found".
    let mut dynamic = network.dynamic();
    async move {
        let _network = network;
        let local = local.await?;
        let mut sealers: HashMap<String, Arc<Mutex<Sealer>>> = HashMap::new();
        let mut tasks = tokio::task::JoinSet::new();
        loop {
            let request = tokio::select! {
                request = dynamic.requested_track() => request?,
                Some(done) = tasks.join_next() => {
                    if let Ok(Err(error)) = done {
                        tracing::debug!(%error, "relay export track ended");
                    }
                    continue;
                }
            };
            let name = request.name().to_owned();
            let sealer = match sealers.get(&name) {
                Some(sealer) => sealer.clone(),
                None => {
                    let sealer = Arc::new(Mutex::new(keys.sealer(direction, &name)?));
                    sealers.insert(name.clone(), sealer.clone());
                    sealer
                }
            };
            let Ok(source) = local.track(&name) else {
                tracing::debug!(name, "relay requested a track we do not publish");
                continue;
            };
            let producer = request.accept(track_info(&name));
            tasks.spawn(async move {
                let subscriber = source.subscribe(local_subscription(&name)).await?;
                export_track(&name, subscriber, producer, sealer).await
            });
        }
    }
}

async fn export_track(
    name: &str,
    mut subscriber: moq_net::track::Subscriber,
    mut producer: moq_net::track::Producer,
    sealer: Arc<Mutex<Sealer>>,
) -> Result<()> {
    if name == protocol::POINTER_TRACK {
        while let Some(datagram) = subscriber.recv_datagram().await? {
            let sealed = sealer
                .lock()
                .await
                .seal(datagram.sequence, &datagram.payload)?;
            producer.write_datagram(moq_net::Datagram {
                sequence: datagram.sequence,
                timestamp: datagram.timestamp,
                payload: sealed.into(),
            })?;
        }
        return Ok(());
    }
    let strict = order(name) == Order::Strict;
    let mut groups = tokio::task::JoinSet::new();
    while let Some(mut group) = subscriber.recv_group().await? {
        while groups.try_join_next().is_some() {}
        let sequence = group.sequence;
        let mut out = producer.create_group(moq_net::group::Info { sequence })?;
        let sealer = sealer.clone();
        let copy = async move {
            while let Some(frame) = protocol::read_frame(&mut group, protocol::MAX_FRAME).await? {
                let sealed = sealer.lock().await.seal(sequence, &frame)?;
                out.write_frame(moq_net::Timestamp::now(), bytes::Bytes::from(sealed))?;
            }
            out.finish()?;
            Ok::<_, anyhow::Error>(())
        };
        if strict {
            // Sealing order must match group order for the strict opener.
            copy.await?;
        } else {
            // A long video group must not delay the next keyframe group.
            groups.spawn(copy);
        }
    }
    Ok(())
}

/// Serve local requests for the peer's broadcast from the relay, opening in
/// `direction`. Runs until the local broadcast closes.
pub fn import(
    network: impl std::future::Future<Output = Result<moq_net::broadcast::Consumer>> + Send + 'static,
    local: moq_net::broadcast::Producer,
    keys: Arc<SessionKeys>,
    direction: Direction,
) -> impl std::future::Future<Output = Result<()>> + Send + 'static {
    // Register before the peer's broadcast appears (see `export`).
    let mut dynamic = local.dynamic();
    async move {
        let _local = local;
        let network = network.await?;
        let mut openers: HashMap<String, Arc<Mutex<Opener>>> = HashMap::new();
        let mut tasks: tokio::task::JoinSet<Result<()>> = tokio::task::JoinSet::new();
        loop {
            let request = tokio::select! {
                request = dynamic.requested_track() => request?,
                Some(done) = tasks.join_next() => {
                    // A strict track that failed authentication ends the session:
                    // the peer (or relay) sent something that is not ours.
                    if let Ok(Err(error)) = done {
                        return Err(error.context("relayed track failed"));
                    }
                    continue;
                }
            };
            let name = request.name().to_owned();
            let opener = match openers.get(&name) {
                Some(opener) => opener.clone(),
                None => {
                    let opener =
                        Arc::new(Mutex::new(keys.opener(direction, &name, order(&name))?));
                    openers.insert(name.clone(), opener.clone());
                    opener
                }
            };
            let Ok(source) = network.track(&name) else {
                tracing::debug!(name, "peer broadcast has no such track");
                continue;
            };
            let producer = request.accept(track_info(&name));
            tasks.spawn(async move {
                let subscriber = source.subscribe(network_subscription(&name)).await?;
                import_track(&name, subscriber, producer, opener).await
            });
        }
    }
}

async fn import_track(
    name: &str,
    mut subscriber: moq_net::track::Subscriber,
    mut producer: moq_net::track::Producer,
    opener: Arc<Mutex<Opener>>,
) -> Result<()> {
    if name == protocol::POINTER_TRACK {
        while let Some(datagram) = subscriber.recv_datagram().await? {
            // Unauthenticated or replayed pointer samples are dropped; the
            // next genuine one supersedes them.
            match opener
                .lock()
                .await
                .open(datagram.sequence, &datagram.payload)
            {
                Ok(plain) => producer.write_datagram(moq_net::Datagram {
                    sequence: datagram.sequence,
                    timestamp: datagram.timestamp,
                    payload: plain.into(),
                })?,
                Err(error) => tracing::debug!(%error, "dropping relayed pointer datagram"),
            }
        }
        return Ok(());
    }
    let limit = protocol::MAX_FRAME + SEAL_OVERHEAD;
    if order(name) == Order::Strict {
        let mut pending = std::collections::BTreeMap::new();
        let mut expected = 0;
        loop {
            let mut group = protocol::ordered_group(&mut subscriber, &mut pending, expected)
                .await
                .with_context(|| format!("relayed {name} groups"))?;
            expected += 1;
            let mut out = producer.create_group(moq_net::group::Info {
                sequence: group.sequence,
            })?;
            while let Some(frame) = protocol::read_frame(&mut group, limit).await? {
                let plain = opener.lock().await.open(group.sequence, &frame)?;
                out.write_frame(moq_net::Timestamp::now(), bytes::Bytes::from(plain))?;
            }
            out.finish()?;
        }
    }
    let mut groups = tokio::task::JoinSet::new();
    while let Some(mut group) = subscriber.recv_group().await? {
        while groups.try_join_next().is_some() {}
        let sequence = group.sequence;
        let mut producer = producer.clone();
        let opener = opener.clone();
        groups.spawn(async move {
            // The local group exists only once its first frame authenticates:
            // a relay-injected group (e.g. a huge sequence that would make the
            // receiver ignore every real group) never reaches the session.
            let mut out: Option<moq_net::group::Producer> = None;
            loop {
                let frame = match tokio::time::timeout(
                    RELAYED_FRAME_TIMEOUT,
                    protocol::read_frame(&mut group, limit),
                )
                .await
                {
                    Ok(frame) => frame?,
                    Err(_) => return Ok(()), // stalled group: abandon it
                };
                let Some(frame) = frame else { break };
                match opener.lock().await.open(sequence, &frame) {
                    Ok(plain) => {
                        let out = match &mut out {
                            Some(out) => out,
                            None => out
                                .insert(producer.create_group(moq_net::group::Info { sequence })?),
                        };
                        out.write_frame(moq_net::Timestamp::now(), bytes::Bytes::from(plain))?
                    }
                    Err(error) => {
                        // Media: abandon this group; the decoder recovers at
                        // the next keyframe (and asks for one).
                        tracing::warn!(%error, sequence, "dropping unauthenticated relayed group");
                        return Ok(());
                    }
                }
            }
            if let Some(mut out) = out {
                out.finish()?;
            }
            Ok::<_, anyhow::Error>(())
        });
    }
    Ok(())
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;

    #[tokio::test]
    async fn relay_sees_only_ciphertext_and_peer_gets_plaintext() -> Result<()> {
        let (client_keys, host_keys) = crate::e2e::test_session();
        let (client_keys, host_keys) = (Arc::new(client_keys), Arc::new(host_keys));
        let secret = b"hunter2 typed into the remote desktop";

        // Host session publishes plaintext locally.
        let host_local = moq_net::Origin::random().produce();
        let mut desktop = host_local.create_broadcast(
            "desktop",
            moq_net::broadcast::Route::new().with_announce(true),
        )?;
        let mut updates = desktop.create_track("updates", track_info("updates"))?;
        for _ in 0..3 {
            let mut group = updates.append_group()?;
            group.write_frame(moq_net::Timestamp::now(), bytes::Bytes::from_static(secret))?;
            group.finish()?;
        }

        // The "relay": a plain origin anyone on the path could read.
        let relay = moq_net::Origin::random().produce();
        let network = relay.create_broadcast(
            "s/desktop",
            moq_net::broadcast::Route::new().with_announce(true),
        )?;
        let local = host_local.consume();
        let export_task = tokio::spawn(export(
            async move {
                local
                    .announced_broadcast("desktop")
                    .await
                    .context("missing")
            },
            network,
            host_keys,
            Direction::HostToClient,
        ));

        // What the relay can see.
        let seen = relay
            .consume()
            .announced_broadcast("s/desktop")
            .await
            .context("relay broadcast")?;
        let mut raw = seen
            .track("updates")?
            .subscribe(network_subscription("updates"))
            .await?;
        let mut group = raw.recv_group().await?.context("group")?;
        let frame = protocol::read_frame(&mut group, protocol::MAX_FRAME)
            .await?
            .context("frame")?;
        assert!(
            !frame.windows(7).any(|w| w == b"hunter2"),
            "relay could read plaintext"
        );

        // The client side opens it.
        let client_local = moq_net::Origin::random().produce();
        let imported = client_local.create_broadcast(
            "desktop",
            moq_net::broadcast::Route::new().with_announce(true),
        )?;
        let relay_view = relay.consume();
        let import_task = tokio::spawn(import(
            async move {
                relay_view
                    .announced_broadcast("s/desktop")
                    .await
                    .context("missing")
            },
            imported,
            client_keys,
            Direction::HostToClient,
        ));
        let desktop = client_local
            .consume()
            .announced_broadcast("desktop")
            .await
            .context("local desktop")?;
        let mut plain = desktop
            .track("updates")?
            .subscribe(local_subscription("updates"))
            .await?;
        for _ in 0..3 {
            let mut group = tokio::time::timeout(Duration::from_secs(5), plain.recv_group())
                .await??
                .context("group")?;
            let frame = protocol::read_frame(&mut group, protocol::MAX_FRAME)
                .await?
                .context("frame")?;
            assert_eq!(&frame[..], secret);
        }
        export_task.abort();
        import_task.abort();
        Ok(())
    }
}
