//! Opt-in (`--upnp`) UDP port mapping with NAT-PMP (RFC 6886).
//!
//! Asks the default gateway to forward a public UDP port to the
//! peer-to-peer socket, giving the peer a `mapped` candidate that works even
//! when the NAT filters unsolicited packets. The mapping is deleted when its
//! [`Mapping`] guard drops (the session or host ends). PCP (RFC 6887) and
//! UPnP-IGD are not implemented; many gateways that speak PCP still answer
//! NAT-PMP. Gateway discovery reads the Linux routing table, so on other
//! systems this reports that no gateway was found.

use anyhow::{Context, Result, bail, ensure};
use std::{
    net::{Ipv4Addr, SocketAddr, SocketAddrV4},
    time::Duration,
};

const PORT: u16 = 5351;
const VERSION: u8 = 0;
const OP_EXTERNAL_ADDRESS: u8 = 0;
const OP_MAP_UDP: u8 = 1;
const RESPONSE: u8 = 128;
/// Requested lifetime; long enough for a session, renewed by a new one.
const LIFETIME_SECONDS: u32 = 3600;
/// RFC 6886 retransmits from 250 ms, doubling; we give up after 1 s.
const FIRST_TIMEOUT: Duration = Duration::from_millis(250);
const ATTEMPTS: u32 = 3;

/// A port mapping on the gateway; deleted (best effort) on drop.
pub struct Mapping {
    gateway: SocketAddr,
    internal_port: u16,
    pub external: SocketAddr,
}

impl Mapping {
    /// Drop the guard without deleting the mapping (a renewal of the same
    /// port replaced it).
    pub fn release(mut self) {
        self.internal_port = 0;
    }
}

impl Drop for Mapping {
    fn drop(&mut self) {
        if self.internal_port == 0 {
            return;
        }
        // One datagram, no wait: a lost delete expires after the lifetime.
        if let Ok(socket) = std::net::UdpSocket::bind("0.0.0.0:0") {
            let _ = socket.send_to(&map_request(self.internal_port, 0, 0), self.gateway);
        }
        tracing::debug!(external = %self.external, "NAT-PMP mapping removed");
    }
}

fn map_request(internal_port: u16, external_port: u16, lifetime: u32) -> [u8; 12] {
    let mut request = [0u8; 12];
    request[0] = VERSION;
    request[1] = OP_MAP_UDP;
    request[4..6].copy_from_slice(&internal_port.to_be_bytes());
    request[6..8].copy_from_slice(&external_port.to_be_bytes());
    request[8..12].copy_from_slice(&lifetime.to_be_bytes());
    request
}

/// Result code and body of a response to `op`, checked for size and kind.
fn response(packet: &[u8], op: u8, length: usize) -> Result<&[u8]> {
    ensure!(packet.len() >= length, "short NAT-PMP response");
    ensure!(
        packet[0] == VERSION && packet[1] == RESPONSE + op,
        "unexpected NAT-PMP response"
    );
    let code = u16::from_be_bytes([packet[2], packet[3]]);
    ensure!(
        code == 0,
        "gateway refused the NAT-PMP request (result {code})"
    );
    Ok(&packet[8..length])
}

fn parse_external_address(packet: &[u8]) -> Result<Ipv4Addr> {
    let body = response(packet, OP_EXTERNAL_ADDRESS, 12)?;
    Ok(Ipv4Addr::new(body[0], body[1], body[2], body[3]))
}

/// (internal port, external port, lifetime) of a mapping response.
fn parse_mapping(packet: &[u8]) -> Result<(u16, u16, u32)> {
    let body = response(packet, OP_MAP_UDP, 16)?;
    Ok((
        u16::from_be_bytes([body[0], body[1]]),
        u16::from_be_bytes([body[2], body[3]]),
        u32::from_be_bytes([body[4], body[5], body[6], body[7]]),
    ))
}

async fn exchange(
    socket: &tokio::net::UdpSocket,
    gateway: SocketAddr,
    request: &[u8],
    accept: impl Fn(&[u8]) -> bool,
) -> Result<Vec<u8>> {
    let mut timeout = FIRST_TIMEOUT;
    let mut buffer = [0u8; 64];
    for _ in 0..ATTEMPTS {
        socket.send_to(request, gateway).await?;
        let deadline = tokio::time::Instant::now() + timeout;
        while let Ok(received) =
            tokio::time::timeout_at(deadline, socket.recv_from(&mut buffer)).await
        {
            let (length, from) = received?;
            if from == gateway && accept(&buffer[..length]) {
                return Ok(buffer[..length].to_vec());
            }
        }
        timeout *= 2;
    }
    bail!("gateway {gateway} did not answer NAT-PMP")
}

/// Map `internal_port` (UDP) on `gateway`; returns the public address.
pub async fn map_via(gateway: SocketAddr, internal_port: u16) -> Result<Mapping> {
    let socket = tokio::net::UdpSocket::bind("0.0.0.0:0").await?;
    let is_response = |op: u8| move |packet: &[u8]| packet.len() >= 2 && packet[1] == RESPONSE + op;
    let address = exchange(
        &socket,
        gateway,
        &[VERSION, OP_EXTERNAL_ADDRESS],
        is_response(OP_EXTERNAL_ADDRESS),
    )
    .await?;
    let address = parse_external_address(&address)?;
    let mapped = exchange(
        &socket,
        gateway,
        &map_request(internal_port, internal_port, LIFETIME_SECONDS),
        is_response(OP_MAP_UDP),
    )
    .await?;
    let (internal, external, lifetime) = parse_mapping(&mapped)?;
    ensure!(
        internal == internal_port && external != 0 && lifetime > 0,
        "gateway returned an unusable NAT-PMP mapping"
    );
    let external = SocketAddr::V4(SocketAddrV4::new(address, external));
    tracing::info!(%external, internal_port, lifetime, "NAT-PMP mapping created");
    Ok(Mapping {
        gateway,
        internal_port,
        external,
    })
}

/// Map `internal_port` on the default gateway.
pub async fn map(internal_port: u16) -> Result<Mapping> {
    let gateway = default_gateway().context("no IPv4 default gateway found")?;
    map_via(
        SocketAddr::V4(SocketAddrV4::new(gateway, PORT)),
        internal_port,
    )
    .await
}

/// The IPv4 default gateway from `/proc/net/route` (Linux only).
fn default_gateway() -> Option<Ipv4Addr> {
    let table = std::fs::read_to_string("/proc/net/route").ok()?;
    parse_route_table(&table)
}

fn parse_route_table(table: &str) -> Option<Ipv4Addr> {
    const RTF_UP: u32 = 0x1;
    const RTF_GATEWAY: u32 = 0x2;
    table.lines().skip(1).find_map(|line| {
        let fields: Vec<&str> = line.split_whitespace().collect();
        let (destination, gateway, flags) = (fields.get(1)?, fields.get(2)?, fields.get(3)?);
        let flags = u32::from_str_radix(flags, 16).ok()?;
        if *destination != "00000000" || flags & (RTF_UP | RTF_GATEWAY) != RTF_UP | RTF_GATEWAY {
            return None;
        }
        // The kernel prints the address as a native-endian word.
        let gateway = u32::from_str_radix(gateway, 16).ok()?;
        Some(Ipv4Addr::from(gateway.to_ne_bytes()))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_map_request_and_parses_responses() {
        assert_eq!(
            map_request(4443, 4443, 3600),
            [0, 1, 0, 0, 0x11, 0x5b, 0x11, 0x5b, 0, 0, 0x0e, 0x10]
        );
        let external = [0, 128, 0, 0, 0, 0, 0, 9, 203, 0, 113, 5];
        assert_eq!(
            parse_external_address(&external).unwrap(),
            Ipv4Addr::new(203, 0, 113, 5)
        );
        let mapping = [
            0, 129, 0, 0, 0, 0, 0, 9, 0x11, 0x5b, 0xc3, 0x50, 0, 0, 0x0e, 0x10,
        ];
        assert_eq!(parse_mapping(&mapping).unwrap(), (4443, 50000, 3600));
        // Refusals, wrong opcodes and short packets are errors.
        let mut refused = mapping;
        refused[3] = 2;
        assert!(parse_mapping(&refused).is_err());
        assert!(parse_mapping(&external).is_err());
        assert!(parse_mapping(&mapping[..15]).is_err());
    }

    #[test]
    #[cfg(target_endian = "little")]
    fn finds_the_default_gateway() {
        let table = "Iface\tDestination\tGateway \tFlags\tRefCnt\tUse\tMetric\tMask\n\
                     wlan0\t0001A8C0\t00000000\t0001\t0\t0\t600\t00FFFFFF\n\
                     wlan0\t00000000\t0101A8C0\t0003\t0\t0\t600\t00000000\n";
        assert_eq!(
            parse_route_table(table),
            Some(Ipv4Addr::new(192, 168, 1, 1))
        );
        assert_eq!(parse_route_table("Iface\tDestination\n"), None);
    }

    /// A fake gateway on loopback: external address, then the mapping, then
    /// the delete sent when the guard drops.
    #[tokio::test]
    async fn maps_and_unmaps_with_a_gateway() {
        let gateway = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let address = gateway.local_addr().unwrap();
        let fake = tokio::spawn(async move {
            let mut buffer = [0u8; 64];
            let mut deleted = false;
            for _ in 0..3 {
                let (length, from) = gateway.recv_from(&mut buffer).await.unwrap();
                let request = &buffer[..length];
                let reply: Vec<u8> = match request[1] {
                    OP_EXTERNAL_ADDRESS => vec![0, 128, 0, 0, 0, 0, 0, 1, 198, 51, 100, 7],
                    OP_MAP_UDP => {
                        let lifetime = &request[8..12];
                        if lifetime == [0, 0, 0, 0] {
                            deleted = true;
                            continue;
                        }
                        let mut reply = vec![0, 129, 0, 0, 0, 0, 0, 1];
                        reply.extend_from_slice(&request[4..6]);
                        reply.extend_from_slice(&40000u16.to_be_bytes());
                        reply.extend_from_slice(lifetime);
                        reply
                    }
                    _ => continue,
                };
                gateway.send_to(&reply, from).await.unwrap();
            }
            deleted
        });
        let mapping = map_via(address, 5555).await.unwrap();
        assert_eq!(mapping.external, "198.51.100.7:40000".parse().unwrap());
        drop(mapping);
        assert!(
            tokio::time::timeout(Duration::from_secs(5), fake)
                .await
                .unwrap()
                .unwrap()
        );
    }
}
