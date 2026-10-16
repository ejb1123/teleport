//! Minimal STUN (RFC 5389) Binding: just enough to learn a socket's public
//! (server-reflexive) address and to send hole-punching indications.
//!
//! Only the Binding method is understood. No authentication attributes are
//! produced or checked: a reflexive address is a hint that the peer-to-peer
//! dial verifies with the pinned TLS certificate, never trust (`crate::p2p`).
//! Requests are matched to responses by their random 96-bit transaction ID.
//!
//! `teleport relay` answers Binding requests on its own UDP port ([`serve`]),
//! so a self-hosted relay doubles as the STUN server and no third party learns
//! anyone's address.

use std::{
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    time::{Duration, Instant},
};

pub const MAGIC_COOKIE: u32 = 0x2112_a442;
pub const HEADER_LEN: usize = 20;
/// IANA default port for STUN over UDP.
pub const DEFAULT_PORT: u16 = 3478;
/// Largest message accepted (RFC 5389 §7.1 keeps STUN below the path MTU).
const MAX_MESSAGE: usize = 548;

const BINDING: u16 = 0x0001;
const CLASS_MASK: u16 = 0x0110;
const ATTR_MAPPED_ADDRESS: u16 = 0x0001;
const ATTR_XOR_MAPPED_ADDRESS: u16 = 0x0020;
/// Pre-RFC 5389 servers (RFC 3489bis drafts) used this code point.
const ATTR_XOR_MAPPED_ADDRESS_OLD: u16 = 0x8020;
const FAMILY_V4: u8 = 0x01;
const FAMILY_V6: u8 = 0x02;

pub type TransactionId = [u8; 12];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Class {
    Request,
    Indication,
    Success,
    Error,
}

impl Class {
    fn bits(self) -> u16 {
        match self {
            Self::Request => 0x0000,
            Self::Indication => 0x0010,
            Self::Success => 0x0100,
            Self::Error => 0x0110,
        }
    }

    fn from_bits(bits: u16) -> Self {
        match bits & CLASS_MASK {
            0x0000 => Self::Request,
            0x0010 => Self::Indication,
            0x0100 => Self::Success,
            _ => Self::Error,
        }
    }
}

/// A parsed Binding message.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Message {
    pub class: Class,
    pub transaction: TransactionId,
    /// XOR-MAPPED-ADDRESS, or MAPPED-ADDRESS from a server that sends only
    /// the old attribute.
    pub mapped: Option<SocketAddr>,
}

/// Cheap check used to demultiplex STUN from QUIC on one socket: the two top
/// bits of a STUN message are zero (QUIC always sets its fixed bit, 0x40),
/// and the magic cookie and a 4-byte-aligned length match the datagram.
pub fn looks_like_stun(packet: &[u8]) -> bool {
    packet.len() >= HEADER_LEN
        && packet.len() <= MAX_MESSAGE
        && packet[0] & 0xc0 == 0
        && packet[4..8] == MAGIC_COOKIE.to_be_bytes()
        && usize::from(u16::from_be_bytes([packet[2], packet[3]])) == packet.len() - HEADER_LEN
        && packet[3].is_multiple_of(4)
}

fn header(class: Class, transaction: &TransactionId, length: u16) -> Vec<u8> {
    let mut message = Vec::with_capacity(HEADER_LEN + usize::from(length));
    message.extend_from_slice(&(BINDING | class.bits()).to_be_bytes());
    message.extend_from_slice(&length.to_be_bytes());
    message.extend_from_slice(&MAGIC_COOKIE.to_be_bytes());
    message.extend_from_slice(transaction);
    message
}

/// A Binding request with no attributes (what every STUN server answers).
pub fn binding_request(transaction: &TransactionId) -> Vec<u8> {
    header(Class::Request, transaction, 0)
}

/// A Binding indication: no response is expected. Used as the hole-punching
/// datagram, so the peer's QUIC stack never sees it (`looks_like_stun`).
pub fn binding_indication(transaction: &TransactionId) -> Vec<u8> {
    header(Class::Indication, transaction, 0)
}

/// A Binding success response carrying `mapped` as XOR-MAPPED-ADDRESS.
pub fn binding_success(transaction: &TransactionId, mapped: SocketAddr) -> Vec<u8> {
    let mapped = canonical(mapped);
    let (family, address): (u8, Vec<u8>) = match mapped.ip() {
        IpAddr::V4(ip) => (FAMILY_V4, ip.octets().to_vec()),
        IpAddr::V6(ip) => (FAMILY_V6, ip.octets().to_vec()),
    };
    let mask = xor_mask(transaction);
    let value_len = 4 + address.len();
    let mut message = header(Class::Success, transaction, (4 + value_len) as u16);
    message.extend_from_slice(&ATTR_XOR_MAPPED_ADDRESS.to_be_bytes());
    message.extend_from_slice(&(value_len as u16).to_be_bytes());
    message.push(0);
    message.push(family);
    message.extend_from_slice(&(mapped.port() ^ (MAGIC_COOKIE >> 16) as u16).to_be_bytes());
    message.extend(address.iter().zip(mask).map(|(byte, mask)| byte ^ mask));
    message
}

/// Cookie followed by the transaction ID: the XOR key for addresses.
fn xor_mask(transaction: &TransactionId) -> [u8; 16] {
    let mut mask = [0u8; 16];
    mask[..4].copy_from_slice(&MAGIC_COOKIE.to_be_bytes());
    mask[4..].copy_from_slice(transaction);
    mask
}

/// Parse a Binding message. Anything malformed, of another method, or with
/// attributes running past the message is rejected as a whole.
pub fn parse(packet: &[u8]) -> Option<Message> {
    if !looks_like_stun(packet) {
        return None;
    }
    let kind = u16::from_be_bytes([packet[0], packet[1]]);
    if kind & !CLASS_MASK != BINDING {
        return None;
    }
    let transaction: TransactionId = packet[8..20].try_into().ok()?;
    let mut xor_mapped = None;
    let mut mapped = None;
    let mut rest = &packet[HEADER_LEN..];
    while !rest.is_empty() {
        if rest.len() < 4 {
            return None;
        }
        let kind = u16::from_be_bytes([rest[0], rest[1]]);
        let length = usize::from(u16::from_be_bytes([rest[2], rest[3]]));
        let padded = length.div_ceil(4) * 4;
        if rest.len() < 4 + padded {
            return None;
        }
        let value = &rest[4..4 + length];
        match kind {
            ATTR_XOR_MAPPED_ADDRESS | ATTR_XOR_MAPPED_ADDRESS_OLD if xor_mapped.is_none() => {
                xor_mapped = Some(address(value, Some(&xor_mask(&transaction)))?);
            }
            ATTR_MAPPED_ADDRESS if mapped.is_none() => mapped = Some(address(value, None)?),
            _ => {}
        }
        rest = &rest[4 + padded..];
    }
    Some(Message {
        class: Class::from_bits(kind),
        transaction,
        mapped: xor_mapped.or(mapped),
    })
}

fn address(value: &[u8], mask: Option<&[u8; 16]>) -> Option<SocketAddr> {
    let (&[_, family, port_high, port_low], address) = value.split_first_chunk::<4>()?;
    let mut port = u16::from_be_bytes([port_high, port_low]);
    let unmask = |bytes: &[u8]| -> Vec<u8> {
        match mask {
            Some(mask) => bytes.iter().zip(mask).map(|(b, m)| b ^ m).collect(),
            None => bytes.to_vec(),
        }
    };
    if mask.is_some() {
        port ^= (MAGIC_COOKIE >> 16) as u16;
    }
    let ip = match (family, address.len()) {
        (FAMILY_V4, 4) => IpAddr::V4(Ipv4Addr::from(<[u8; 4]>::try_from(unmask(address)).ok()?)),
        (FAMILY_V6, 16) => IpAddr::V6(Ipv6Addr::from(<[u8; 16]>::try_from(unmask(address)).ok()?)),
        _ => return None,
    };
    Some(SocketAddr::new(ip, port))
}

/// IPv4-mapped IPv6 addresses (from dual-stack sockets) as plain IPv4.
pub fn canonical(address: SocketAddr) -> SocketAddr {
    SocketAddr::new(address.ip().to_canonical(), address.port())
}

/// Answers per second the relay's responder sends before it starts dropping
/// requests; bounds its use as a (small, ~2x) reflection amplifier.
const RESPONSES_PER_SECOND: u32 = 2000;

/// Answer Binding requests on `socket` until it fails. Everything else is
/// ignored. Each answer only tells the sender its own address.
pub async fn serve(socket: tokio::net::UdpSocket) -> std::io::Result<()> {
    let mut buffer = [0u8; MAX_MESSAGE + 1];
    let mut window = (Instant::now(), 0u32);
    loop {
        let (length, from) = socket.recv_from(&mut buffer).await?;
        let Some(message) = parse(&buffer[..length]) else {
            continue;
        };
        if message.class != Class::Request {
            continue;
        }
        let now = Instant::now();
        if now.duration_since(window.0) >= Duration::from_secs(1) {
            window = (now, 0);
        }
        if window.1 >= RESPONSES_PER_SECOND {
            continue;
        }
        window.1 += 1;
        // A send error for one peer (e.g. unreachable) must not stop the
        // responder for everyone else.
        let _ = socket
            .send_to(&binding_success(&message.transaction, from), from)
            .await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TRANSACTION: TransactionId = [
        0xb7, 0xe7, 0xa7, 0x01, 0xbc, 0x34, 0xd6, 0x86, 0xfa, 0x87, 0xdf, 0xae,
    ];

    /// RFC 5769 §2.2 (IPv4 response): header, SOFTWARE and XOR-MAPPED-ADDRESS
    /// as published. MESSAGE-INTEGRITY and FINGERPRINT are left out and the
    /// length adjusted, since this client neither sends nor checks them.
    #[test]
    fn decodes_rfc5769_ipv4_response() {
        let mut packet = vec![0x01, 0x01, 0x00, 0x1c, 0x21, 0x12, 0xa4, 0x42];
        packet.extend_from_slice(&TRANSACTION);
        packet.extend_from_slice(&[
            0x80, 0x22, 0x00, 0x0b, 0x74, 0x65, 0x73, 0x74, 0x20, 0x76, 0x65, 0x63, 0x74, 0x6f,
            0x72, 0x20, // SOFTWARE "test vector" (padded)
            0x00, 0x20, 0x00, 0x08, 0x00, 0x01, 0xa1, 0x47, 0xe1, 0x12, 0xa6, 0x43,
        ]);
        let message = parse(&packet).unwrap();
        assert_eq!(message.class, Class::Success);
        assert_eq!(message.transaction, TRANSACTION);
        assert_eq!(message.mapped, Some("192.0.2.1:32853".parse().unwrap()));
    }

    /// RFC 5769 §2.3 (IPv6 response), likewise without integrity attributes.
    #[test]
    fn decodes_rfc5769_ipv6_response() {
        let mut packet = vec![0x01, 0x01, 0x00, 0x18, 0x21, 0x12, 0xa4, 0x42];
        packet.extend_from_slice(&TRANSACTION);
        packet.extend_from_slice(&[
            0x00, 0x20, 0x00, 0x14, 0x00, 0x02, 0xa1, 0x47, 0x01, 0x13, 0xa9, 0xfa, 0xa5, 0xd3,
            0xf1, 0x79, 0xbc, 0x25, 0xf4, 0xb5, 0xbe, 0xd2, 0xb9, 0xd9,
        ]);
        let message = parse(&packet).unwrap();
        assert_eq!(
            message.mapped,
            Some(
                "[2001:db8:1234:5678:11:2233:4455:6677]:32853"
                    .parse()
                    .unwrap()
            )
        );
    }

    #[test]
    fn success_roundtrips_both_families_and_maps_v4_in_v6() {
        for address in [
            "203.0.113.7:4443",
            "[2001:db8::1]:65535",
            "[::ffff:198.51.100.2]:9",
        ] {
            let address: SocketAddr = address.parse().unwrap();
            let packet = binding_success(&TRANSACTION, address);
            assert!(looks_like_stun(&packet));
            let message = parse(&packet).unwrap();
            assert_eq!(message.class, Class::Success);
            assert_eq!(message.mapped, Some(canonical(address)));
        }
        assert_eq!(
            parse(&binding_success(
                &TRANSACTION,
                "[::ffff:198.51.100.2]:9".parse().unwrap()
            ))
            .unwrap()
            .mapped,
            Some("198.51.100.2:9".parse().unwrap())
        );
    }

    #[test]
    fn requests_and_indications_have_no_address() {
        let request = parse(&binding_request(&TRANSACTION)).unwrap();
        assert_eq!(request.class, Class::Request);
        assert_eq!(request.mapped, None);
        let indication = parse(&binding_indication(&TRANSACTION)).unwrap();
        assert_eq!(indication.class, Class::Indication);
        assert_eq!(binding_request(&TRANSACTION).len(), HEADER_LEN);
    }

    #[test]
    fn plain_mapped_address_is_used_when_no_xor_form() {
        let mut packet = header(Class::Success, &TRANSACTION, 12);
        packet.extend_from_slice(&[0x00, 0x01, 0x00, 0x08, 0x00, 0x01, 0x11, 0x5c, 10, 0, 0, 1]);
        assert_eq!(
            parse(&packet).unwrap().mapped,
            Some("10.0.0.1:4444".parse().unwrap())
        );
    }

    #[test]
    fn rejects_quic_and_malformed_messages() {
        // A QUIC long-header Initial: fixed bit set.
        let mut quic = vec![0xc3, 0, 0, 0, 1];
        quic.resize(1200, 0);
        assert!(!looks_like_stun(&quic));
        assert!(parse(&quic).is_none());
        // Wrong cookie, short, length mismatch, unaligned.
        let good = binding_request(&TRANSACTION);
        let mut bad = good.clone();
        bad[4] ^= 1;
        assert!(parse(&bad).is_none());
        assert!(parse(&good[..19]).is_none());
        let mut long = good.clone();
        long.extend_from_slice(&[0, 0, 0, 0]);
        assert!(parse(&long).is_none(), "length field not updated");
        // Attribute running past the end.
        let mut overrun = header(Class::Success, &TRANSACTION, 8);
        overrun.extend_from_slice(&[0x00, 0x20, 0x00, 0x08, 0x00, 0x01, 0, 0]);
        assert!(parse(&overrun).is_none());
        // Address of the wrong size for its family.
        let mut family = header(Class::Success, &TRANSACTION, 12);
        family.extend_from_slice(&[0x00, 0x20, 0x00, 0x08, 0x00, 0x02, 0, 0, 1, 2, 3, 4]);
        assert!(parse(&family).is_none());
        // Another method (Allocate, 0x0003) is not ours.
        let mut allocate = binding_request(&TRANSACTION);
        allocate[1] = 0x03;
        assert!(parse(&allocate).is_none());
        // Oversized.
        let mut huge = header(Class::Indication, &TRANSACTION, 600);
        huge.resize(HEADER_LEN + 600, 0);
        assert!(!looks_like_stun(&huge));
    }

    #[tokio::test]
    async fn responder_reports_the_senders_address() {
        let server = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let address = server.local_addr().unwrap();
        let task = tokio::spawn(serve(server));
        let client = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        // Garbage and indications get no answer; a request does.
        client.send_to(b"hello", address).await.unwrap();
        client
            .send_to(&binding_indication(&TRANSACTION), address)
            .await
            .unwrap();
        client
            .send_to(&binding_request(&TRANSACTION), address)
            .await
            .unwrap();
        let mut buffer = [0u8; 600];
        let (length, _) =
            tokio::time::timeout(Duration::from_secs(5), client.recv_from(&mut buffer))
                .await
                .unwrap()
                .unwrap();
        let message = parse(&buffer[..length]).unwrap();
        assert_eq!(message.class, Class::Success);
        assert_eq!(message.transaction, TRANSACTION);
        assert_eq!(message.mapped, Some(client.local_addr().unwrap()));
        task.abort();
    }
}
