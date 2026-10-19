//! Wake-on-LAN (parsec-comparison G23, docs/wake-and-unattended.md).
//!
//! A host shares its [`WakeInfo`] with paired clients inside the
//! authenticated session (`Desktop::wake`); the client saves it in the
//! desktop's profile. To wake the desktop, the client broadcasts a magic
//! packet on its own LAN and, when the desktop is elsewhere, asks a saved
//! *wake relay* (another of the owner's Teleport hosts, opted in with
//! `--wake-relay`, on the desktop's network) to send it there through the
//! end-to-end encrypted relay (`crate::host_command`). The MoQ relay itself
//! never can: it cannot complete a session with a wake relay.
//!
//! A magic packet is six `0xff` bytes and the target's hardware address
//! sixteen times, in one UDP datagram (port 9, "discard"). Network cards with
//! Wake-on-LAN armed (`ethtool -s IF wol g`) watch for it while the machine
//! sleeps; nothing in the packet is secret or authenticated, so anybody on
//! the LAN can always wake a machine. Sending one is therefore not a
//! privilege; Teleport only limits who can make *its* hosts send them.

use anyhow::{Context, Result, ensure};

use std::{
    net::{Ipv4Addr, SocketAddrV4, UdpSocket},
    path::PathBuf,
    str::FromStr,
    time::{Duration, Instant},
};

/// UDP port magic packets go to (discard; some tools use 7).
pub const PORT: u16 = 9;
/// Six `0xff` and sixteen copies of the address.
pub const MAGIC_LEN: usize = 6 + 16 * 6;

/// A hardware (MAC) address.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Mac(pub [u8; 6]);

impl Mac {
    /// A unicast, non-zero address: one network card, not a group.
    pub fn is_unicast(&self) -> bool {
        self.0[0] & 1 == 0 && self.0 != [0; 6]
    }
}

impl FromStr for Mac {
    type Err = anyhow::Error;

    /// `aa:bb:cc:dd:ee:ff` or `aa-bb-cc-dd-ee-ff`, either case.
    fn from_str(text: &str) -> Result<Self> {
        let text = text.trim();
        let separator = if text.contains('-') { '-' } else { ':' };
        let mut bytes = [0u8; 6];
        let mut parts = text.split(separator);
        for byte in &mut bytes {
            let part = parts.next().context("a MAC address has six parts")?;
            ensure!(
                part.len() == 2 && part.bytes().all(|b| b.is_ascii_hexdigit()),
                "invalid MAC address {text:?}"
            );
            *byte = u8::from_str_radix(part, 16)?;
        }
        ensure!(parts.next().is_none(), "a MAC address has six parts");
        Ok(Self(bytes))
    }
}

impl std::fmt::Display for Mac {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let [a, b, c, d, e, f] = self.0;
        write!(formatter, "{a:02x}:{b:02x}:{c:02x}:{d:02x}:{e:02x}:{f:02x}")
    }
}

/// The magic packet that wakes `mac`.
pub fn magic_packet(mac: Mac) -> [u8; MAGIC_LEN] {
    let mut packet = [0xff; MAGIC_LEN];
    for copy in packet[6..].chunks_exact_mut(6) {
        copy.copy_from_slice(&mac.0);
    }
    packet
}

/// The address a magic packet wakes. Accepts a trailing four- or six-byte
/// SecureOn password (which Teleport never sends); anything else is `None`.
pub fn parse_magic_packet(bytes: &[u8]) -> Option<Mac> {
    if !matches!(bytes.len(), MAGIC_LEN | 106 | 108) || bytes[..6] != [0xff; 6] {
        return None;
    }
    let mac: [u8; 6] = bytes[6..12].try_into().ok()?;
    bytes[6..MAGIC_LEN]
        .chunks_exact(6)
        .all(|copy| copy == mac)
        .then_some(Mac(mac))
}

/// An IPv4 address with its prefix length, e.g. `192.168.1.20/24`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Ipv4Net {
    pub address: Ipv4Addr,
    pub prefix: u8,
}

impl Ipv4Net {
    fn mask(&self) -> u32 {
        u32::MAX
            .checked_shl(32 - u32::from(self.prefix))
            .unwrap_or(0)
    }

    /// The network address (host bits cleared).
    pub fn network(&self) -> Ipv4Addr {
        Ipv4Addr::from(u32::from(self.address) & self.mask())
    }

    /// The directed (subnet) broadcast address; `None` for /31 and /32,
    /// which have none.
    pub fn broadcast(&self) -> Option<Ipv4Addr> {
        (self.prefix <= 30).then(|| Ipv4Addr::from(u32::from(self.address) | !self.mask()))
    }

    pub fn contains(&self, address: Ipv4Addr) -> bool {
        u32::from(address) & self.mask() == u32::from(self.network())
    }
}

impl FromStr for Ipv4Net {
    type Err = anyhow::Error;

    fn from_str(text: &str) -> Result<Self> {
        let (address, prefix) = text.split_once('/').context("expected ADDRESS/PREFIX")?;
        let prefix: u8 = prefix.parse()?;
        ensure!(prefix <= 32, "invalid prefix length");
        Ok(Self {
            address: address.parse()?,
            prefix,
        })
    }
}

impl std::fmt::Display for Ipv4Net {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}/{}", self.address, self.prefix)
    }
}

/// What a host tells its paired clients about waking it (`Desktop::wake`).
pub use crate::protocol::WakeInfo;

impl WakeInfo {
    /// Rejects malformed host data before a client saves or uses it.
    pub fn validate(&self) -> Result<()> {
        if let Some(mac) = &self.mac {
            ensure!(mac.len() <= 17, "invalid wake MAC");
            ensure!(mac.parse::<Mac>()?.is_unicast(), "invalid wake MAC");
        }
        if let Some(ipv4) = &self.ipv4 {
            ensure!(ipv4.len() <= 18, "invalid wake network");
            ipv4.parse::<Ipv4Net>()?;
        }
        Ok(())
    }

    pub fn mac(&self) -> Option<Mac> {
        self.mac.as_deref()?.parse().ok()
    }

    pub fn network(&self) -> Option<Ipv4Net> {
        self.ipv4.as_deref()?.parse().ok()
    }

    /// Both hosts are on the same IPv4 subnet (the same LAN, so one can wake
    /// the other with a broadcast).
    pub fn same_network(&self, other: &WakeInfo) -> bool {
        match (self.network(), other.network()) {
            (Some(a), Some(b)) => a.prefix == b.prefix && a.contains(b.address),
            _ => false,
        }
    }

    /// One line for the launcher and `teleport wake`.
    pub fn summary(&self) -> String {
        match (self.mac(), self.wake_on_lan) {
            (None, _) => "Wake-on-LAN: the host did not report a network card".into(),
            (Some(mac), Some(true)) => format!("Wake-on-LAN armed on the host ({mac})"),
            (Some(mac), Some(false)) => {
                format!(
                    "Wake-on-LAN is off on the host card ({mac}); see {}",
                    crate::docs::page("wake-and-unattended.md")
                )
            }
            (Some(mac), None) => format!("Wake-on-LAN state unknown ({mac})"),
        }
    }
}

/// An IPv4 address of a local network interface.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Interface {
    pub name: String,
    pub network: Ipv4Net,
    pub up: bool,
    pub broadcast: bool,
    pub loopback: bool,
}

/// Every IPv4 interface address of this machine (`getifaddrs`).
pub fn interfaces() -> Vec<Interface> {
    let mut list = Vec::new();
    let mut head: *mut libc::ifaddrs = std::ptr::null_mut();
    // Safety: getifaddrs fills `head` with a list we only read and then free
    // exactly once; every pointer is checked before it is dereferenced.
    unsafe {
        if libc::getifaddrs(&mut head) != 0 {
            return list;
        }
        let mut cursor = head;
        while !cursor.is_null() {
            let entry = &*cursor;
            cursor = entry.ifa_next;
            if entry.ifa_addr.is_null()
                || entry.ifa_netmask.is_null()
                || entry.ifa_name.is_null()
                || i32::from((*entry.ifa_addr).sa_family) != libc::AF_INET
            {
                continue;
            }
            let address = &*(entry.ifa_addr as *const libc::sockaddr_in);
            let mask = &*(entry.ifa_netmask as *const libc::sockaddr_in);
            let flags = entry.ifa_flags as libc::c_int;
            list.push(Interface {
                name: std::ffi::CStr::from_ptr(entry.ifa_name)
                    .to_string_lossy()
                    .into_owned(),
                network: Ipv4Net {
                    address: Ipv4Addr::from(u32::from_be(address.sin_addr.s_addr)),
                    prefix: u32::from_be(mask.sin_addr.s_addr).leading_ones() as u8,
                },
                up: flags & libc::IFF_UP != 0,
                broadcast: flags & libc::IFF_BROADCAST != 0,
                loopback: flags & libc::IFF_LOOPBACK != 0,
            });
        }
        libc::freeifaddrs(head);
    }
    list
}

/// Where to send a magic packet: the limited broadcast (the interface of the
/// default route), each local interface's subnet broadcast, and `extra`
/// (e.g. the saved desktop's subnet broadcast). No duplicates.
pub fn targets(local: &[Interface], extra: &[Ipv4Addr], port: u16) -> Vec<SocketAddrV4> {
    let mut addresses = vec![Ipv4Addr::BROADCAST];
    addresses.extend(
        local
            .iter()
            .filter(|interface| interface.up && interface.broadcast && !interface.loopback)
            .filter_map(|interface| interface.network.broadcast()),
    );
    addresses.extend(extra);
    let mut targets = Vec::new();
    for address in addresses {
        let target = SocketAddrV4::new(address, port);
        if !targets.contains(&target) {
            targets.push(target);
        }
    }
    targets
}

/// Send the magic packet for `mac` to every target. Returns how many sends
/// succeeded; fails only if none did.
pub fn send(mac: Mac, targets: &[SocketAddrV4]) -> Result<usize> {
    ensure!(
        mac.is_unicast(),
        "refusing to wake a group or zero MAC address"
    );
    let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0))?;
    socket.set_broadcast(true)?;
    let packet = magic_packet(mac);
    let mut sent = 0;
    let mut last_error = None;
    for target in targets {
        match socket.send_to(&packet, target) {
            Ok(_) => sent += 1,
            Err(error) => {
                tracing::debug!(%target, %error, "magic packet not sent");
                last_error = Some(error);
            }
        }
    }
    match last_error {
        Some(error) if sent == 0 => Err(error).context("no magic packet could be sent"),
        _ => Ok(sent),
    }
}

/// Send the magic packet for `mac` on this machine's networks.
pub fn send_local(mac: Mac, extra: &[Ipv4Addr], port: u16) -> Result<usize> {
    send(mac, &targets(&interfaces(), extra, port))
}

/// `teleport wake`.
#[derive(clap::Args)]
pub struct Options {
    /// A saved desktop: its saved hardware address and network, and any
    /// saved wake relay on that network.
    pub address: Option<String>,
    /// Wake this hardware address (instead of a saved desktop's).
    #[arg(long)]
    pub mac: Option<Mac>,
    /// Also send to this (subnet broadcast) address; repeatable.
    #[arg(long)]
    pub broadcast: Vec<Ipv4Addr>,
    /// UDP port of the magic packet.
    #[arg(long, default_value_t = PORT)]
    pub port: u16,
    /// Do not send on this machine's networks; only ask the wake relay.
    #[arg(long)]
    pub no_local: bool,
    /// Ask this Teleport host (started with `--wake-relay`) to send the
    /// packet on its network: a saved desktop's address, or any address with
    /// --via-pairing-file and the relay options, as for `teleport client`.
    #[arg(long)]
    pub via: Option<String>,
    #[arg(long, requires = "via")]
    pub via_pairing_file: Option<PathBuf>,
    #[command(flatten)]
    pub relay: crate::relay::RelayArgs,
    /// How to reach the wake relay given with --via-pairing-file.
    #[arg(long, value_enum, default_value_t = crate::relay::Route::Relay)]
    pub route: crate::relay::Route,
    /// Wait up to this many seconds for the saved desktop to answer (its
    /// presence through its relay, or a direct authenticated answer).
    #[arg(long, default_value_t = 0)]
    pub wait: u64,
}

/// `teleport wake`: print what was sent, and with --wait whether the
/// desktop came up.
pub async fn run(options: Options) -> Result<()> {
    let profiles = match &options.address {
        Some(_) => crate::profiles::load()?,
        None => Vec::new(),
    };
    let profile = options
        .address
        .as_ref()
        .map(|address| {
            profiles
                .iter()
                .find(|profile| &profile.address == address)
                .context("no saved desktop has this address")
        })
        .transpose()?;
    let saved = profile.and_then(|profile| profile.wake.as_ref());
    let mac = match (options.mac, saved.and_then(WakeInfo::mac)) {
        (Some(mac), _) | (None, Some(mac)) => mac,
        (None, None) => anyhow::bail!(
            "no hardware address: connect to the desktop once (it shares it), or pass --mac"
        ),
    };
    let mut extra = options.broadcast.clone();
    extra.extend(saved.and_then(|info| info.network()?.broadcast()));
    if !options.no_local {
        let sent = tokio::task::spawn_blocking({
            let extra = extra.clone();
            let port = options.port;
            move || send_local(mac, &extra, port)
        })
        .await??;
        println!("Sent a magic packet for {mac} to {sent} address(es) on this network");
    }
    // Wake relays: the one asked for, or saved ones on the desktop's network.
    let mut relays = Vec::new();
    match (&options.via, &options.via_pairing_file) {
        (Some(address), Some(pairing_file)) => relays.push(crate::host_command::Target {
            address: address.clone(),
            pairing_file: pairing_file.clone(),
            relay: options.relay.endpoint(),
            route: if options.relay.relay.is_some() {
                options.route
            } else {
                crate::relay::Route::Direct
            },
        }),
        (Some(address), None) => {
            let profiles = crate::profiles::load()?;
            let relay = profiles
                .iter()
                .find(|profile| &profile.address == address)
                .context("--via names no saved desktop; add --via-pairing-file")?;
            relays.push(crate::host_command::Target::saved(relay)?);
        }
        _ => {
            if let Some(profile) = profile {
                for relay in wake_relays(profile, &profiles) {
                    relays.push(crate::host_command::Target::saved(relay)?);
                }
            }
        }
    }
    let mut relayed = false;
    for relay in &relays {
        match crate::host_command::send(relay, &crate::host_command::Command::Wake { mac }).await {
            Ok(reply) if reply.ok => {
                relayed = true;
                println!("Wake relay {}: {}", relay.address, reply.message);
            }
            Ok(reply) => println!("Wake relay {} refused: {}", relay.address, reply.message),
            Err(error) => println!("Wake relay {} unavailable: {error:#}", relay.address),
        }
    }
    ensure!(
        !options.no_local || relayed,
        "no wake relay sent the magic packet"
    );
    if options.wait > 0 {
        let profile = profile.context("--wait needs a saved desktop")?;
        let deadline = Instant::now() + Duration::from_secs(options.wait);
        match wait_until_awake(profile, mac, &extra, options.port, deadline).await {
            Some(how) => println!("AWAKE ({how})"),
            None => {
                println!("NO ANSWER");
                anyhow::bail!("the desktop did not answer within {} s", options.wait);
            }
        }
    }
    Ok(())
}

/// The launcher's Wake for a saved desktop: a magic packet on this network,
/// the same through every saved wake relay on the desktop's network, then
/// wait (at most `wait`) for the desktop to answer. `progress` gets status
/// lines. Returns whether it answered.
pub async fn wake_saved(
    profile: crate::profiles::Profile,
    profiles: Vec<crate::profiles::Profile>,
    wait: Duration,
    progress: impl Fn(String),
) -> Result<bool> {
    let info = profile
        .wake
        .as_ref()
        .context("connect to this desktop once so it can share how to wake it")?;
    let mac = info
        .mac()
        .context("the host did not report its network card")?;
    let extra: Vec<Ipv4Addr> = info
        .network()
        .and_then(|network| network.broadcast())
        .into_iter()
        .collect();
    let local = {
        let extra = extra.clone();
        tokio::task::spawn_blocking(move || send_local(mac, &extra, PORT)).await?
    };
    let mut sent = match local {
        Ok(count) => {
            progress(format!(
                "Magic packet sent on this network ({count} addresses)..."
            ));
            true
        }
        Err(error) => {
            progress(format!("Could not send on this network: {error:#}"));
            false
        }
    };
    for relay in wake_relays(&profile, &profiles) {
        let label = relay.label().to_owned();
        let result = match crate::host_command::Target::saved(relay) {
            Ok(target) => {
                crate::host_command::send(&target, &crate::host_command::Command::Wake { mac })
                    .await
            }
            Err(error) => Err(error),
        };
        match result {
            Ok(reply) if reply.ok => {
                sent = true;
                progress(format!("{label} sent the magic packet on its network..."));
            }
            Ok(reply) => progress(format!("{label} refused to wake it: {}", reply.message)),
            Err(error) => progress(format!("Wake relay {label} unavailable: {error:#}")),
        }
    }
    ensure!(sent, "no magic packet could be sent");
    progress(format!(
        "Waiting for {} to wake (up to {} s)...",
        profile.label(),
        wait.as_secs()
    ));
    Ok(
        wait_until_awake(&profile, mac, &extra, PORT, Instant::now() + wait)
            .await
            .is_some(),
    )
}

/// Saved desktops that can wake `target`: their hosts opted in to the wake
/// relay role, and share the target's IPv4 subnet. The target itself never.
pub fn wake_relays<'a>(
    target: &crate::profiles::Profile,
    profiles: &'a [crate::profiles::Profile],
) -> Vec<&'a crate::profiles::Profile> {
    let Some(wake) = &target.wake else {
        return Vec::new();
    };
    profiles
        .iter()
        .filter(|profile| profile.address != target.address && profile.pairing_file.is_some())
        .filter(|profile| {
            profile
                .wake
                .as_ref()
                .is_some_and(|relay| relay.wake_relay && relay.same_network(wake))
        })
        .collect()
}

/// After waking: resend the packet every 10 s and wait for the desktop's
/// presence (through its relay) or a direct authenticated answer.
pub async fn wait_until_awake(
    profile: &crate::profiles::Profile,
    mac: Mac,
    extra: &[Ipv4Addr],
    port: u16,
    deadline: Instant,
) -> Option<&'static str> {
    let target = crate::host_command::Target::saved(profile).ok();
    let presence = profile
        .relay
        .as_ref()
        .filter(|relay| relay.route != crate::relay::Route::Direct)
        .and_then(|relay| relay.endpoint().ok())
        .zip(profile.trusted_fingerprint().ok().flatten());
    let mut resend = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if let Some((endpoint, fingerprint)) = &presence
            && crate::presence::check(endpoint, fingerprint, Duration::from_secs(6)).await
                == crate::presence::State::Online
        {
            return Some("online through its relay");
        }
        if let Some(target) = &target {
            let direct = crate::host_command::Target {
                route: crate::relay::Route::Direct,
                ..target.clone()
            };
            if let Ok(Ok(reply)) = tokio::time::timeout(
                Duration::from_secs(3),
                crate::host_command::send(&direct, &crate::host_command::Command::Ping),
            )
            .await
                && reply.ok
            {
                return Some("answered directly");
            }
        }
        if Instant::now() >= resend {
            resend = Instant::now() + Duration::from_secs(10);
            let extra = extra.to_vec();
            let _ = tokio::task::spawn_blocking(move || send_local(mac, &extra, port)).await;
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
    None
}

/// `teleport wake-listen`: print the magic packets this machine receives,
/// to check that wakes reach it (while it is awake).
#[derive(clap::Args)]
pub struct ListenOptions {
    /// Address to listen on.
    #[arg(long, default_value = "0.0.0.0")]
    pub address: Ipv4Addr,
    #[arg(long, default_value_t = PORT)]
    pub port: u16,
    /// Stop after this many packets (0: never).
    #[arg(long, default_value_t = 0)]
    pub count: u32,
    /// Stop after this many seconds without a packet (0: never).
    #[arg(long, default_value_t = 0)]
    pub timeout: u64,
}

pub fn listen(options: ListenOptions) -> Result<()> {
    use std::io::Write;
    let socket = UdpSocket::bind((options.address, options.port))
        .with_context(|| format!("cannot listen on UDP port {}", options.port))?;
    if options.timeout > 0 {
        socket.set_read_timeout(Some(Duration::from_secs(options.timeout)))?;
    }
    println!("LISTENING {}", socket.local_addr()?);
    std::io::stdout().flush()?;
    let mut buffer = [0u8; 256];
    let mut received = 0;
    while options.count == 0 || received < options.count {
        let (length, from) = match socket.recv_from(&mut buffer) {
            Ok(result) => result,
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                anyhow::bail!("no magic packet within {} s", options.timeout)
            }
            Err(error) => return Err(error.into()),
        };
        match parse_magic_packet(&buffer[..length]) {
            Some(mac) => {
                received += 1;
                println!("WAKE {mac} from {from}");
            }
            None => println!("IGNORED {length} bytes from {from} (not a magic packet)"),
        }
        std::io::stdout().flush()?;
    }
    Ok(())
}

/// Host side: what this host shares about waking it (Linux).
#[cfg(target_os = "linux")]
pub mod host {
    use super::*;

    /// The interface of the IPv4 default route with the lowest metric, from
    /// `/proc/net/route`.
    pub fn default_route_interface(table: &str) -> Option<String> {
        const RTF_UP: u32 = 0x1;
        table
            .lines()
            .skip(1)
            .filter_map(|line| {
                let fields: Vec<&str> = line.split_whitespace().collect();
                let (name, destination, flags, metric, mask) = (
                    *fields.first()?,
                    *fields.get(1)?,
                    u32::from_str_radix(fields.get(3)?, 16).ok()?,
                    fields.get(6)?.parse::<u32>().ok()?,
                    *fields.get(7)?,
                );
                (destination == "00000000" && mask == "00000000" && flags & RTF_UP != 0)
                    .then(|| (metric, name.to_owned()))
            })
            .min()
            .map(|(_, name)| name)
    }

    /// Whether `ethtool IF` reports magic-packet wake armed: `Some(true)` for
    /// "Wake-on: g" (possibly with other modes), `Some(false)` for any other
    /// mode, `None` when it does not say (no driver support, no ethtool).
    pub fn parse_ethtool_wake_on(output: &str) -> Option<bool> {
        output.lines().find_map(|line| {
            let modes = line.trim().strip_prefix("Wake-on:")?.trim();
            Some(modes.contains('g'))
        })
    }

    fn read_mac(interface: &str) -> Option<Mac> {
        let text = std::fs::read_to_string(format!("/sys/class/net/{interface}/address")).ok()?;
        text.trim().parse().ok().filter(Mac::is_unicast)
    }

    fn wake_on_lan(interface: &str) -> Option<bool> {
        // Reading the setting needs no privilege (ethtool's netlink API).
        let output = std::process::Command::new("ethtool")
            .arg(interface)
            .stdin(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .output()
            .ok()?;
        parse_ethtool_wake_on(&String::from_utf8_lossy(&output.stdout))
    }

    /// The interface the host is reached on: the one holding the listen
    /// address, or (listening on all addresses or loopback) the default
    /// route's.
    pub fn choose_interface(
        listen: &str,
        interfaces: &[Interface],
        route_table: &str,
    ) -> Option<Interface> {
        let listen_ip =
            listen
                .parse::<std::net::SocketAddr>()
                .ok()
                .and_then(|address| match address.ip() {
                    std::net::IpAddr::V4(ip) if !ip.is_unspecified() && !ip.is_loopback() => {
                        Some(ip)
                    }
                    _ => None,
                });
        let name = match listen_ip {
            Some(ip) => interfaces
                .iter()
                .find(|interface| interface.network.address == ip)?
                .name
                .clone(),
            None => default_route_interface(route_table)?,
        };
        interfaces
            .iter()
            .find(|interface| interface.name == name && !interface.loopback)
            .cloned()
    }

    /// This host's [`WakeInfo`]; `wake_relay`/`suspend` are its options.
    pub fn info(listen: &str, wake_relay: bool, suspend: bool) -> WakeInfo {
        let table = std::fs::read_to_string("/proc/net/route").unwrap_or_default();
        let interface = choose_interface(listen, &interfaces(), &table);
        let mac = interface.as_ref().and_then(|i| read_mac(&i.name));
        WakeInfo {
            mac: mac.map(|mac| mac.to_string()),
            wake_on_lan: interface
                .as_ref()
                .filter(|_| mac.is_some())
                .and_then(|i| wake_on_lan(&i.name)),
            ipv4: interface.map(|i| i.network.to_string()),
            wake_relay,
            suspend,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MAC: Mac = Mac([0x00, 0x1b, 0x21, 0x3c, 0x4d, 0x5e]);

    #[test]
    fn magic_packet_is_six_ff_and_sixteen_addresses() {
        let packet = magic_packet(MAC);
        assert_eq!(packet.len(), 102);
        assert_eq!(packet[..6], [0xff; 6]);
        for copy in 0..16 {
            assert_eq!(packet[6 + copy * 6..12 + copy * 6], MAC.0);
        }
        assert_eq!(parse_magic_packet(&packet), Some(MAC));
    }

    #[test]
    fn parsing_rejects_anything_but_a_magic_packet() {
        let packet = magic_packet(MAC);
        // A SecureOn password may follow; other lengths may not.
        let mut with_password = packet.to_vec();
        with_password.extend([1, 2, 3, 4, 5, 6]);
        assert_eq!(parse_magic_packet(&with_password), Some(MAC));
        assert_eq!(parse_magic_packet(&packet[..101]), None);
        assert_eq!(
            parse_magic_packet(&[packet.as_slice(), &[0]].concat()),
            None
        );
        let mut header = packet;
        header[3] = 0xfe;
        assert_eq!(parse_magic_packet(&header), None);
        let mut copy = packet;
        copy[101] ^= 1;
        assert_eq!(parse_magic_packet(&copy), None);
        assert_eq!(parse_magic_packet(&[]), None);
    }

    #[test]
    fn mac_addresses_parse_and_print_canonically() {
        assert_eq!("00:1B:21:3c:4d:5e".parse::<Mac>().unwrap(), MAC);
        assert_eq!("00-1b-21-3c-4d-5e".parse::<Mac>().unwrap(), MAC);
        assert_eq!(MAC.to_string(), "00:1b:21:3c:4d:5e");
        for bad in [
            "",
            "00:1b:21:3c:4d",
            "00:1b:21:3c:4d:5e:6f",
            "0:1b:21:3c:4d:5e",
            "00:1b:21:3c:4d:zz",
            "00:1b-21:3c:4d:5e",
        ] {
            assert!(bad.parse::<Mac>().is_err(), "{bad}");
        }
        assert!(MAC.is_unicast());
        assert!(!Mac([0xff; 6]).is_unicast());
        assert!(!Mac([0x01, 0, 0x5e, 0, 0, 1]).is_unicast());
        assert!(!Mac([0; 6]).is_unicast());
        assert!(send(Mac([0xff; 6]), &[]).is_err());
    }

    #[test]
    fn networks_give_directed_broadcasts_and_membership() {
        let net: Ipv4Net = "192.168.1.20/24".parse().unwrap();
        assert_eq!(net.network(), Ipv4Addr::new(192, 168, 1, 0));
        assert_eq!(net.broadcast(), Some(Ipv4Addr::new(192, 168, 1, 255)));
        assert!(net.contains(Ipv4Addr::new(192, 168, 1, 7)));
        assert!(!net.contains(Ipv4Addr::new(192, 168, 2, 7)));
        let wide: Ipv4Net = "10.1.2.3/8".parse().unwrap();
        assert_eq!(wide.broadcast(), Some(Ipv4Addr::new(10, 255, 255, 255)));
        assert_eq!("10.0.0.1/31".parse::<Ipv4Net>().unwrap().broadcast(), None);
        assert_eq!("10.0.0.1/32".parse::<Ipv4Net>().unwrap().broadcast(), None);
        let any: Ipv4Net = "10.0.0.1/0".parse().unwrap();
        assert!(any.contains(Ipv4Addr::new(8, 8, 8, 8)));
        assert!("10.0.0.1/33".parse::<Ipv4Net>().is_err());
        assert!("10.0.0.1".parse::<Ipv4Net>().is_err());
    }

    #[test]
    fn targets_cover_limited_and_subnet_broadcasts_once() {
        let interface = |name: &str, network: &str, loopback: bool| Interface {
            name: name.into(),
            network: network.parse().unwrap(),
            up: true,
            broadcast: !loopback,
            loopback,
        };
        let local = [
            interface("lo", "127.0.0.1/8", true),
            interface("eth0", "192.168.1.20/24", false),
            Interface {
                up: false,
                ..interface("eth1", "10.0.0.2/24", false)
            },
            interface("wg0", "10.8.0.2/32", false),
        ];
        let targets = targets(&local, &[Ipv4Addr::new(192, 168, 1, 255)], 9);
        assert_eq!(
            targets,
            vec![
                SocketAddrV4::new(Ipv4Addr::BROADCAST, 9),
                SocketAddrV4::new(Ipv4Addr::new(192, 168, 1, 255), 9),
            ]
        );
    }

    #[test]
    fn wake_info_validates_and_matches_networks() {
        let info = WakeInfo {
            mac: Some(MAC.to_string()),
            wake_on_lan: Some(true),
            ipv4: Some("192.168.1.20/24".into()),
            ..Default::default()
        };
        info.validate().unwrap();
        assert_eq!(info.mac(), Some(MAC));
        let relay = WakeInfo {
            ipv4: Some("192.168.1.2/24".into()),
            wake_relay: true,
            ..Default::default()
        };
        assert!(info.same_network(&relay) && relay.same_network(&info));
        let elsewhere = WakeInfo {
            ipv4: Some("192.168.2.2/24".into()),
            ..relay.clone()
        };
        assert!(!info.same_network(&elsewhere));
        assert!(!info.same_network(&WakeInfo::default()));
        for bad in [
            WakeInfo {
                mac: Some("ff:ff:ff:ff:ff:ff".into()),
                ..Default::default()
            },
            WakeInfo {
                mac: Some("nonsense".into()),
                ..Default::default()
            },
            WakeInfo {
                ipv4: Some("192.168.1.1/40".into()),
                ..Default::default()
            },
        ] {
            assert!(bad.validate().is_err(), "{bad:?}");
        }
        // Older hosts send no wake info; older clients ignore it.
        let parsed: WakeInfo = serde_json::from_str("{}").unwrap();
        assert_eq!(parsed, WakeInfo::default());
    }

    #[test]
    fn wake_relays_are_opted_in_saved_hosts_on_the_target_network() {
        let profile = |address: &str, wake: Option<WakeInfo>| crate::profiles::Profile {
            name: String::new(),
            address: address.into(),
            pairing_file: Some("/nonexistent".into()),
            fingerprint: None,
            relay: None,
            wake,
        };
        let lan = |ip: &str, relay| WakeInfo {
            ipv4: Some(ip.into()),
            wake_relay: relay,
            ..Default::default()
        };
        let target = profile(
            "gaming:4443",
            Some(WakeInfo {
                mac: Some(MAC.to_string()),
                ..lan("192.168.1.20/24", true)
            }),
        );
        let profiles = vec![
            target.clone(),
            profile("nas:4443", Some(lan("192.168.1.5/24", true))),
            profile("laptop:4443", Some(lan("192.168.1.9/24", false))),
            profile("office:4443", Some(lan("10.0.0.5/24", true))),
            profile("old:4443", None),
        ];
        let relays: Vec<&str> = wake_relays(&target, &profiles)
            .iter()
            .map(|profile| profile.address.as_str())
            .collect();
        assert_eq!(relays, ["nas:4443"]);
        // Without saved wake info nothing qualifies.
        assert!(wake_relays(&profile("x:1", None), &profiles).is_empty());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn host_reads_the_default_route_and_ethtool() {
        let table = "Iface\tDestination\tGateway \tFlags\tRefCnt\tUse\tMetric\tMask\t\tMTU\tWindow\tIRTT\n\
            wlan0\t00000000\t0101A8C0\t0003\t0\t0\t600\t00000000\t0\t0\t0\n\
            eth0\t00000000\t0101A8C0\t0003\t0\t0\t100\t00000000\t0\t0\t0\n\
            eth0\t0001A8C0\t00000000\t0001\t0\t0\t100\t00FFFFFF\t0\t0\t0\n\
            down0\t00000000\t0101A8C0\t0002\t0\t0\t1\t00000000\t0\t0\t0\n";
        assert_eq!(
            host::default_route_interface(table).as_deref(),
            Some("eth0")
        );
        assert_eq!(host::default_route_interface("Iface\n"), None);
        let ethtool = "Settings for eth0:\n\tSupports Wake-on: pumbg\n\tWake-on: g\n";
        assert_eq!(host::parse_ethtool_wake_on(ethtool), Some(true));
        let off = "\tSupports Wake-on: pumbg\n\tWake-on: d\n";
        assert_eq!(host::parse_ethtool_wake_on(off), Some(false));
        assert_eq!(host::parse_ethtool_wake_on("\tWake-on: ug\n"), Some(true));
        assert_eq!(host::parse_ethtool_wake_on("Settings for wlan0:\n"), None);
        let interfaces = [
            Interface {
                name: "lo".into(),
                network: "127.0.0.1/8".parse().unwrap(),
                up: true,
                broadcast: false,
                loopback: true,
            },
            Interface {
                name: "eth0".into(),
                network: "192.168.1.20/24".parse().unwrap(),
                up: true,
                broadcast: true,
                loopback: false,
            },
            Interface {
                name: "wlan0".into(),
                network: "192.168.1.21/24".parse().unwrap(),
                up: true,
                broadcast: true,
                loopback: false,
            },
        ];
        let chosen = |listen| host::choose_interface(listen, &interfaces, table).map(|i| i.name);
        assert_eq!(chosen("0.0.0.0:4443").as_deref(), Some("eth0"));
        assert_eq!(chosen("127.0.0.1:4443").as_deref(), Some("eth0"));
        assert_eq!(chosen("192.168.1.21:4443").as_deref(), Some("wlan0"));
        assert_eq!(chosen("10.9.9.9:4443"), None);
    }
}
