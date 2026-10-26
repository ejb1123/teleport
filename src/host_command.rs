//! Owner requests a host answers without a desktop session (G23,
//! docs/wake-and-unattended.md): `ping` (is it awake?), `wake` (as a wake
//! relay, send a magic packet on the host's LAN) and `suspend`.
//!
//! A request is an ordinary connection whose options name the command
//! (`command=wake&mac=…`): over QUIC to the host's address, or through the
//! end-to-end encrypted relay, where the options are bound into the
//! handshake transcript (`crate::e2e`). The host authenticates the
//! credential exactly as for a session (device keys, revocation, single-use
//! tickets) and only then acts; it never captures the desktop for a command.
//! It answers with one [`Reply`] on the `desktop` track, where a session
//! would put its `Desktop`. Wake and suspend are opt-in on the host
//! (`--wake-relay`, `--allow-suspend`) and owner-only. The MoQ relay cannot
//! issue a command: it cannot complete the handshake for any credential.

use crate::{protocol::Pairing, relay::Route, wake::Mac};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{path::PathBuf, time::Duration};

/// How long a direct attempt may take before `auto` tries the relay.
const DIRECT_ATTEMPT: Duration = Duration::from_millis(2500);
/// The whole exchange, once connected.
const REPLY_TIMEOUT: Duration = Duration::from_secs(10);

/// A request for the host.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Command {
    /// Answer if awake (used while waiting for a woken desktop).
    Ping,
    /// Send a magic packet for this address on the host's networks.
    Wake { mac: Mac },
    /// Suspend the host.
    Suspend,
}

impl Command {
    pub fn name(&self) -> &'static str {
        match self {
            Self::Ping => "ping",
            Self::Wake { .. } => "wake",
            Self::Suspend => "suspend",
        }
    }

    /// The connection options that carry this command.
    pub fn query(&self) -> String {
        let mut query = url::form_urlencoded::Serializer::new(String::new());
        query.append_pair("command", self.name());
        if let Self::Wake { mac } = self {
            query.append_pair("mac", &mac.to_string());
        }
        query.finish()
    }

    /// The command in a connection's options; `None` for a desktop session.
    /// Unknown commands and malformed arguments are errors (the connection
    /// is refused, not treated as a session).
    // Only hosts (Linux) read commands.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub fn parse(query: Option<&str>) -> Result<Option<Self>> {
        let pairs: Vec<(String, String)> =
            url::form_urlencoded::parse(query.unwrap_or_default().as_bytes())
                .map(|(key, value)| (key.into_owned(), value.into_owned()))
                .collect();
        let value = |name: &str| {
            let mut values = pairs.iter().filter(|(key, _)| key == name);
            let first = values.next().map(|(_, value)| value.as_str());
            ensure!(values.next().is_none(), "repeated {name}");
            Ok(first)
        };
        let Some(command) = value("command")? else {
            return Ok(None);
        };
        let command = match command {
            "ping" => Self::Ping,
            "suspend" => Self::Suspend,
            "wake" => Self::Wake {
                mac: value("mac")?.context("wake needs mac")?.parse()?,
            },
            other => anyhow::bail!("unknown host command {other:?}"),
        };
        ensure!(
            pairs
                .iter()
                .all(|(key, _)| key == "command" || (key == "mac" && command.name() == "wake")),
            "unexpected options for a host command"
        );
        Ok(Some(command))
    }
}

/// The host's answer.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Reply {
    /// The command answered (a `Desktop` has no such field, so a host that
    /// ignored the command is told apart).
    pub command: String,
    pub ok: bool,
    pub message: String,
}

/// A host to send a command to.
#[derive(Clone)]
pub struct Target {
    pub address: String,
    pub pairing_file: PathBuf,
    pub relay: Option<crate::relay::Endpoint>,
    pub route: Route,
}

impl Target {
    /// A saved desktop, reached as its connection settings say.
    pub fn saved(profile: &crate::profiles::Profile) -> Result<Self> {
        let pairing_file = profile
            .pairing_file
            .clone()
            .context("this desktop has no saved credential; sign in first")?;
        crate::profiles::check_private_file(&pairing_file)?;
        profile
            .trusted_fingerprint()?
            .context("this desktop's identity is not approved yet")?;
        let relay = profile
            .relay
            .as_ref()
            .map(|relay| Ok::<_, anyhow::Error>((relay.endpoint()?, relay.route)))
            .transpose()?;
        Ok(Self {
            address: profile.address.clone(),
            pairing_file,
            route: relay.as_ref().map_or(Route::Direct, |(_, route)| *route),
            relay: relay.map(|(endpoint, _)| endpoint),
        })
    }
}

/// `teleport suspend`.
#[derive(clap::Args)]
pub struct SuspendOptions {
    /// A saved desktop (or any address with --pairing-file).
    pub address: String,
    /// Credential for an address that is not saved.
    #[arg(long)]
    pub pairing_file: Option<PathBuf>,
    #[command(flatten)]
    pub relay: crate::relay::RelayArgs,
    #[arg(long, value_enum, default_value_t = Route::Auto)]
    pub route: Route,
}

pub async fn suspend(options: SuspendOptions) -> Result<()> {
    let target = match &options.pairing_file {
        Some(pairing_file) => Target {
            address: options.address.clone(),
            pairing_file: pairing_file.clone(),
            relay: options.relay.endpoint(),
            route: if options.relay.relay.is_some() {
                options.route
            } else {
                Route::Direct
            },
        },
        None => Target::saved(
            crate::profiles::load()?
                .iter()
                .find(|profile| profile.address == options.address)
                .context("no saved desktop has this address; pass --pairing-file")?,
        )?,
    };
    let reply = send(&target, &Command::Suspend).await?;
    println!("{}", reply.message);
    ensure!(reply.ok, "the host refused to suspend");
    Ok(())
}

/// Send `command` and wait for the host's reply.
pub async fn send(target: &Target, command: &Command) -> Result<Reply> {
    match (target.route, &target.relay) {
        (Route::Direct, _) | (_, None) => exchange(target, command, false, None).await,
        (Route::Relay, Some(_)) => exchange(target, command, true, None).await,
        (Route::Auto, Some(_)) => {
            match exchange(target, command, false, Some(DIRECT_ATTEMPT)).await {
                Ok(reply) => Ok(reply),
                Err(error) => {
                    tracing::debug!(
                        error = format!("{error:#}"),
                        "direct command failed; trying the relay"
                    );
                    exchange(target, command, true, None).await
                }
            }
        }
    }
}

async fn exchange(
    target: &Target,
    command: &Command,
    relayed: bool,
    connect_limit: Option<Duration>,
) -> Result<Reply> {
    let pairing = Pairing::read(&target.pairing_file)?;
    ensure!(
        !target.address.contains('/') && !target.address.contains('@'),
        "use host:port, not a URL"
    );
    let mut url: url::Url =
        format!("moqt://{}/teleport/{}", target.address, pairing.token).parse()?;
    url.set_query(Some(&command.query()));
    let connect = async {
        if relayed {
            let endpoint = target.relay.as_ref().context("no relay configured")?;
            let relay =
                crate::relay::client(endpoint, &pairing, url.query().unwrap_or_default(), None)
                    .await?;
            let (incoming, session) = (relay.incoming.clone(), relay.link.session.clone());
            Ok::<_, anyhow::Error>((incoming, session, Some(relay)))
        } else {
            let mut config = moq_native::ClientConfig::default();
            config.version = vec![
                crate::protocol::WIRE_VERSION
                    .parse()
                    .map_err(anyhow::Error::msg)?,
            ];
            config.quic.idle_timeout = Some(crate::protocol::QUIC_IDLE_TIMEOUT);
            config.quic.keep_alive = Some(crate::protocol::QUIC_KEEP_ALIVE);
            config.tls.fingerprint = vec![pairing.fingerprint.clone()];
            let outgoing = moq_net::Origin::random().produce();
            let incoming = moq_net::Origin::random().produce();
            let client = config
                .init()?
                .with_publisher(&outgoing)
                .with_subscriber(incoming.clone());
            let session = client.connect(url.clone()).await?;
            Ok((incoming, session, None))
        }
    };
    let (incoming, session, _relay) = match connect_limit {
        Some(limit) => tokio::time::timeout(limit, connect)
            .await
            .context("no direct answer")??,
        None => connect.await?,
    };
    let reply = tokio::time::timeout(REPLY_TIMEOUT, async {
        let broadcasts = incoming.consume();
        let remote = tokio::select! {
            result = broadcasts.announced_broadcast("desktop") => result.context("host did not answer")?,
            error = session.closed() => anyhow::bail!("host refused the request: {error}"),
        };
        let mut track = remote.track("desktop")?.subscribe(None).await?;
        let mut group = track.recv_group().await?.context("host reply missing")?;
        crate::protocol::read_frame(&mut group, crate::protocol::MAX_CONTROL)
            .await?
            .context("empty host reply")
    })
    .await
    .context("host did not answer in time")??;
    session.abort(moq_net::Error::Cancel);
    let reply: Reply = serde_json::from_slice(&reply)
        .map_err(|_| anyhow::anyhow!("the host does not take this request; update it"))?;
    ensure!(
        reply.command == command.name() && reply.message.len() <= 512,
        "unexpected host reply"
    );
    Ok(reply)
}

/// Host side (Linux).
#[cfg(target_os = "linux")]
pub mod host {
    use super::*;
    use std::{
        collections::VecDeque,
        sync::{Arc, Mutex},
        time::Instant,
    };

    /// At most this many wakes per minute, whoever asks.
    const WAKES_PER_MINUTE: usize = 6;
    /// Time for the reply to reach the client before the host suspends.
    const SUSPEND_DELAY: Duration = Duration::from_secs(2);
    /// How long the host keeps a command connection open for the reply.
    const LINGER: Duration = Duration::from_secs(10);

    /// What this host's owner allowed (host options).
    pub struct Policy {
        pub wake_relay: bool,
        /// Empty: any unicast address.
        pub wake_allow: Vec<Mac>,
        pub suspend: bool,
        pub suspend_command: Vec<String>,
        wakes: Mutex<VecDeque<Instant>>,
    }

    /// Done after the reply went out.
    pub enum After {
        Nothing,
        Suspend(Vec<String>),
    }

    impl Policy {
        pub fn new(
            wake_relay: bool,
            wake_allow: Vec<Mac>,
            suspend: bool,
            suspend_command: &str,
        ) -> Arc<Self> {
            Arc::new(Self {
                wake_relay,
                wake_allow,
                suspend,
                suspend_command: suspend_command
                    .split_whitespace()
                    .map(str::to_owned)
                    .collect(),
                wakes: Mutex::new(VecDeque::new()),
            })
        }

        /// Decide (and for a wake, act). `owner`: the credential is the
        /// desktop owner's (see `access::SessionLease::owner`).
        pub async fn execute(&self, command: &Command, owner: bool) -> (Reply, After) {
            let refuse = |message: &str| {
                (
                    Reply {
                        command: command.name().into(),
                        ok: false,
                        message: message.into(),
                    },
                    After::Nothing,
                )
            };
            let accept = |message: String, after| {
                (
                    Reply {
                        command: command.name().into(),
                        ok: true,
                        message,
                    },
                    after,
                )
            };
            match command {
                Command::Ping => accept("awake".into(), After::Nothing),
                _ if !owner => refuse("only the owner's devices may ask this"),
                Command::Wake { .. } if !self.wake_relay => {
                    refuse("this host is not a wake relay (start it with --wake-relay)")
                }
                Command::Wake { mac } if !mac.is_unicast() => refuse("not a device address"),
                Command::Wake { mac }
                    if !self.wake_allow.is_empty() && !self.wake_allow.contains(mac) =>
                {
                    refuse("this wake relay may not wake that address")
                }
                Command::Wake { mac } => {
                    if !self.admit_wake(Instant::now()) {
                        return refuse("too many wake requests; try again in a minute");
                    }
                    let mac = *mac;
                    match tokio::task::spawn_blocking(move || {
                        crate::wake::send_local(mac, &[], crate::wake::PORT)
                    })
                    .await
                    {
                        Ok(Ok(sent)) => {
                            tracing::info!(%mac, sent, "sent a magic packet as wake relay");
                            accept(
                                format!("sent a magic packet for {mac} to {sent} address(es)"),
                                After::Nothing,
                            )
                        }
                        Ok(Err(error)) => {
                            tracing::warn!(%mac, error = format!("{error:#}"), "wake relay could not send");
                            refuse("could not send the magic packet")
                        }
                        Err(_) => refuse("could not send the magic packet"),
                    }
                }
                Command::Suspend if !self.suspend || self.suspend_command.is_empty() => {
                    refuse("this host does not suspend on request (start it with --allow-suspend)")
                }
                Command::Suspend => accept(
                    format!("suspending in {} s", SUSPEND_DELAY.as_secs()),
                    After::Suspend(self.suspend_command.clone()),
                ),
            }
        }

        fn admit_wake(&self, now: Instant) -> bool {
            let mut wakes = self.wakes.lock().unwrap();
            while wakes
                .front()
                .is_some_and(|at| now.duration_since(*at) >= Duration::from_secs(60))
            {
                wakes.pop_front();
            }
            if wakes.len() >= WAKES_PER_MINUTE {
                return false;
            }
            wakes.push_back(now);
            true
        }
    }

    impl After {
        pub async fn run(self) {
            if let Self::Suspend(command) = self {
                tokio::time::sleep(SUSPEND_DELAY).await;
                tracing::warn!(?command, "suspending at the owner's request");
                let Some((program, args)) = command.split_first() else {
                    return;
                };
                match tokio::process::Command::new(program)
                    .args(args)
                    .stdin(std::process::Stdio::null())
                    .status()
                    .await
                {
                    Ok(status) if status.success() => (),
                    Ok(status) => tracing::warn!(%status, "suspend command failed"),
                    Err(error) => tracing::warn!(%error, "suspend command failed"),
                }
            }
        }
    }

    /// The published reply; finished (cleanly ended) when dropped.
    pub struct Published {
        broadcast: moq_net::broadcast::Producer,
        track: moq_net::track::Producer,
    }

    impl Drop for Published {
        fn drop(&mut self) {
            let _ = self.track.finish();
            self.broadcast.finish();
        }
    }

    /// Publish `reply` where a session publishes its `Desktop`.
    pub fn publish(outgoing: &moq_net::origin::Producer, reply: &Reply) -> Result<Published> {
        let mut broadcast = outgoing.create_broadcast(
            "desktop",
            moq_net::broadcast::Route::new().with_announce(true),
        )?;
        let mut track = broadcast.create_track("desktop", None)?;
        let mut group = track.append_group()?;
        group.write_frame(moq_net::Timestamp::now(), serde_json::to_vec(reply)?)?;
        group.finish()?;
        Ok(Published { broadcast, track })
    }

    /// A direct (or peer-to-peer) command connection, already authorized.
    pub async fn direct<L: Send + 'static>(
        request: crate::p2p::Incoming,
        command: Command,
        policy: Arc<Policy>,
        owner: bool,
        lease: L,
    ) {
        let _lease = lease;
        let outgoing = moq_net::Origin::random().produce();
        let incoming = moq_net::Origin::random().produce();
        let session = request.ok(&outgoing, incoming);
        let (reply, after) = policy.execute(&command, owner).await;
        tracing::info!(
            command = command.name(),
            ok = reply.ok,
            owner,
            "host command"
        );
        let published = publish(&outgoing, &reply);
        let linger = async {
            // The client closes once it has read the reply.
            if let Ok(session) = session.await
                && published.is_ok()
            {
                let _ = tokio::time::timeout(LINGER, session.closed()).await;
            }
        };
        tokio::join!(linger, after.run());
    }

    /// A relayed command whose client confirmed the session keys.
    pub async fn relayed<L: Send + 'static>(
        bridged: crate::relay::HostSession,
        accepted: crate::relay::Accepted,
        command: Command,
        policy: Arc<Policy>,
        owner: bool,
        lease: L,
    ) {
        let _lease = lease;
        let (reply, after) = policy.execute(&command, owner).await;
        tracing::info!(
            command = command.name(),
            ok = reply.ok,
            owner,
            "relayed host command"
        );
        let published = publish(&bridged.outgoing, &reply);
        // There is no per-session close on the shared relay link to wait
        // for, so the reply stays up briefly.
        let linger = async {
            if published.is_ok() {
                tokio::time::sleep(LINGER).await;
            }
        };
        tokio::join!(linger, after.run());
        drop((published, bridged, accepted));
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        const MAC: Mac = Mac([0x00, 0x1b, 0x21, 0x3c, 0x4d, 0x5e]);

        #[tokio::test]
        async fn commands_are_opt_in_and_owner_only() {
            let closed = Policy::new(false, Vec::new(), false, "true");
            let (reply, _) = closed.execute(&Command::Ping, false).await;
            assert!(reply.ok && reply.command == "ping");
            for command in [Command::Wake { mac: MAC }, Command::Suspend] {
                let (reply, after) = closed.execute(&command, true).await;
                assert!(!reply.ok, "{command:?}");
                assert!(matches!(after, After::Nothing));
                assert_eq!(reply.command, command.name());
            }
            let open = Policy::new(
                true,
                vec![Mac([2, 0, 0, 0, 0, 1])],
                true,
                "systemctl suspend",
            );
            // Not the owner: refused even when enabled.
            for command in [Command::Wake { mac: MAC }, Command::Suspend] {
                assert!(!open.execute(&command, false).await.0.ok);
            }
            // An allow list limits which machines the relay wakes; the
            // refusal comes before any packet is sent.
            let (reply, _) = open.execute(&Command::Wake { mac: MAC }, true).await;
            assert!(!reply.ok && reply.message.contains("may not wake"));
            let (reply, after) = open.execute(&Command::Suspend, true).await;
            assert!(reply.ok);
            assert!(
                matches!(after, After::Suspend(command) if command == ["systemctl", "suspend"])
            );
            let broadcast = Policy::new(true, Vec::new(), false, "");
            let (reply, _) = broadcast
                .execute(
                    &Command::Wake {
                        mac: Mac([0xff; 6]),
                    },
                    true,
                )
                .await;
            assert!(!reply.ok);
        }

        #[test]
        fn wakes_are_rate_limited() {
            let policy = Policy::new(true, Vec::new(), false, "");
            let start = Instant::now();
            for _ in 0..WAKES_PER_MINUTE {
                assert!(policy.admit_wake(start));
            }
            assert!(!policy.admit_wake(start + Duration::from_secs(59)));
            assert!(policy.admit_wake(start + Duration::from_secs(60)));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commands_round_trip_through_connection_options() {
        let mac: Mac = "00:1b:21:3c:4d:5e".parse().unwrap();
        for command in [Command::Ping, Command::Suspend, Command::Wake { mac }] {
            let query = command.query();
            assert!(query.len() <= 128, "fits a relayed hello: {query}");
            assert_eq!(Command::parse(Some(&query)).unwrap(), Some(command));
        }
        assert_eq!(Command::parse(None).unwrap(), None);
        // A desktop session's options are not a command.
        assert_eq!(
            Command::parse(Some("codec=h264&range=hdr10")).unwrap(),
            None
        );
        for bad in [
            "command=reboot",
            "command=wake",
            "command=wake&mac=ff",
            "command=ping&command=suspend",
            "command=suspend&mac=00:1b:21:3c:4d:5e",
            "command=ping&codec=h264",
            "command=wake&mac=00:1b:21:3c:4d:5e&mac=00:1b:21:3c:4d:5f",
        ] {
            assert!(Command::parse(Some(bad)).is_err(), "{bad}");
        }
    }

    #[test]
    fn a_desktop_is_not_a_reply() {
        let desktop = serde_json::json!({ "version": 2, "width": 1, "height": 1, "fps": 1,
            "source": "test", "monitors": [], "active_monitor": 0, "audio": false,
            "clipboard": false });
        assert!(serde_json::from_value::<Reply>(desktop).is_err());
    }
}
