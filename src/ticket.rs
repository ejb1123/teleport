//! Session tickets: access granted by an external issuer instead of a pairing
//! (docs/tickets.md).
//!
//! A fleet operator's service decides who may open a session on a host and
//! signs a short-lived ticket naming the host, the permissions and the
//! client's own key. The host trusts the issuer's Ed25519 public key
//! (`--ticket-issuer-key`), never a shared secret. A ticket is redeemed once,
//! inside TLS pinned to the host certificate (magic [`MAGIC`], over TCP or a
//! relay stream like system login), for a memory-only credential that opens
//! exactly one session and is bound to the client key, so a relayed session
//! also needs that key's transcript signature.
//!
//! Wire form: `tkt1.<payload>.<signature>`, both base64url without padding.
//! The signature is Ed25519 over `teleport-ticket-v1.` followed by the payload
//! segment exactly as written, so no JSON canonicalisation is involved.
use anyhow::{Context, Result, anyhow, ensure};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use ring::signature::{self, KeyPair};
use serde::{Deserialize, Serialize};
use std::{sync::Arc, time::Duration};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use zeroize::Zeroizing;

use crate::protocol::{Pairing, Permissions};

pub const PREFIX: &str = "tkt1.";
const SIGNED_PREFIX: &[u8] = b"teleport-ticket-v1.";
pub const MAGIC: &[u8; 8] = b"TPTKT001";
const ALPN: &[u8] = b"teleport-ticket/1";
/// Possession of the client key, signed over the TLS exporter of the
/// redemption connection, so the signature is useless on any other.
const PROOF_LABEL: &[u8] = b"teleport-ticket-proof-v1";
const EXPORTER_LABEL: &[u8] = b"EXPORTER-teleport-ticket";
const MAX_TICKET: usize = 2048;
const MAX_FRAME: usize = 4096;
/// Longest a ticket may be valid for redemption.
pub const MAX_VALIDITY: u64 = 600;
/// Longest session a ticket may grant (a day).
pub const MAX_SESSION: u64 = 86_400;
/// Clock difference tolerated between the issuer and the host.
pub const SKEW: u64 = 30;
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);
const REDEEM_TIMEOUT: Duration = Duration::from_secs(20);

/// What a ticket says. Unknown fields are refused: a field this host does
/// not understand may be a restriction it would fail to apply.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Claims {
    pub v: u32,
    pub iss: String,
    /// The host certificate's SHA-256 fingerprint (lowercase hex).
    pub aud: String,
    /// Who the session is for, as the host shows it.
    pub sub: String,
    /// The issuer's session id, for its audit log.
    pub sid: String,
    /// Granted beyond viewing: `keyboard_mouse`, `gamepad`, `clipboard`,
    /// `microphone`. Empty is view only.
    pub perms: Vec<String>,
    /// The client's Ed25519 public key (base64url, 32 bytes).
    pub cnf: String,
    pub nbf: u64,
    pub exp: u64,
    pub jti: String,
    /// The session ends this long after it starts.
    pub max_session_s: u64,
}

/// A ticket whose signature, audience and lifetime checked out. Only hosts
/// (Linux) read the fields; elsewhere `teleport ticket sign` uses [`verify`]
/// only as a check.
#[derive(Clone, Debug)]
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub struct Verified {
    pub claims: Claims,
    pub client_key: [u8; 32],
    pub permissions: Permissions,
    pub max_session: Duration,
}

fn printable(text: &str, limit: usize) -> bool {
    !text.is_empty() && text.len() <= limit && !text.chars().any(char::is_control)
}

fn identifier(text: &str) -> bool {
    !text.is_empty()
        && text.len() <= 64
        && text
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

fn lower_hex(text: &str, length: usize) -> bool {
    text.len() == length && text.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

fn permissions(perms: &[String]) -> Result<Permissions> {
    let mut granted = Permissions {
        gamepad: false,
        keyboard_mouse: false,
        clipboard: false,
        microphone: false,
    };
    for perm in perms {
        let flag = match perm.as_str() {
            "keyboard_mouse" => &mut granted.keyboard_mouse,
            "gamepad" => &mut granted.gamepad,
            "clipboard" => &mut granted.clipboard,
            "microphone" => &mut granted.microphone,
            other => anyhow::bail!("unknown ticket permission {other:?}"),
        };
        ensure!(!*flag, "repeated ticket permission");
        *flag = true;
    }
    Ok(granted)
}

/// An issuer public key: 64 hex digits (the raw Ed25519 key).
pub fn parse_issuer_key(text: &str) -> Result<[u8; 32]> {
    let text = text.trim().to_ascii_lowercase();
    ensure!(
        lower_hex(&text, 64),
        "a ticket issuer key is 64 hexadecimal digits (a raw Ed25519 public key)"
    );
    let mut key = [0; 32];
    for (byte, pair) in key.iter_mut().zip(text.as_bytes().chunks(2)) {
        *byte = u8::from_str_radix(std::str::from_utf8(pair)?, 16)?;
    }
    Ok(key)
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// Sign `claims` with an issuer's Ed25519 seed (`teleport ticket sign`, tests).
pub fn sign(claims: &Claims, issuer_seed: &[u8; 32]) -> Result<String> {
    let key = signature::Ed25519KeyPair::from_seed_unchecked(issuer_seed)
        .map_err(|_| anyhow!("invalid issuer key"))?;
    let payload = URL_SAFE_NO_PAD.encode(serde_json::to_vec(claims)?);
    let mut message = SIGNED_PREFIX.to_vec();
    message.extend_from_slice(payload.as_bytes());
    let signature = URL_SAFE_NO_PAD.encode(key.sign(&message));
    Ok(format!("{PREFIX}{payload}.{signature}"))
}

/// The public key (hex) of an issuer seed.
pub fn issuer_public_key(issuer_seed: &[u8; 32]) -> Result<String> {
    let key = signature::Ed25519KeyPair::from_seed_unchecked(issuer_seed)
        .map_err(|_| anyhow!("invalid issuer key"))?;
    Ok(hex(key.public_key().as_ref()))
}

/// The host a ticket names (`aud`), read WITHOUT verifying the signature:
/// only for a client choosing where to send it. The host verifies.
pub fn audience(text: &str) -> Result<String> {
    let body = text.strip_prefix(PREFIX).context("not a session ticket")?;
    let (payload, _) = body.split_once('.').context("malformed session ticket")?;
    let claims: Claims = serde_json::from_slice(
        &URL_SAFE_NO_PAD
            .decode(payload)
            .context("malformed session ticket")?,
    )
    .context("malformed session ticket")?;
    ensure!(
        lower_hex(&claims.aud, 64),
        "malformed session ticket audience"
    );
    Ok(claims.aud)
}

/// Check a ticket for the host with `fingerprint` at `now` (unix seconds).
/// The caller still has to enforce single use of `jti` and the client's
/// possession of `cnf`.
pub fn verify(text: &str, issuers: &[[u8; 32]], fingerprint: &str, now: u64) -> Result<Verified> {
    ensure!(!issuers.is_empty(), "this host accepts no session tickets");
    ensure!(text.len() <= MAX_TICKET, "session ticket too long");
    let body = text.strip_prefix(PREFIX).context("not a session ticket")?;
    let (payload, signature_text) = body.split_once('.').context("malformed session ticket")?;
    let signature_bytes = URL_SAFE_NO_PAD
        .decode(signature_text)
        .context("malformed session ticket signature")?;
    let mut message = SIGNED_PREFIX.to_vec();
    message.extend_from_slice(payload.as_bytes());
    ensure!(
        issuers.iter().any(|issuer| {
            signature::UnparsedPublicKey::new(&signature::ED25519, issuer)
                .verify(&message, &signature_bytes)
                .is_ok()
        }),
        "session ticket not signed by a trusted issuer"
    );
    // Only signed bytes are parsed.
    let claims: Claims = serde_json::from_slice(
        &URL_SAFE_NO_PAD
            .decode(payload)
            .context("malformed session ticket")?,
    )
    .context("malformed session ticket")?;
    ensure!(claims.v == 1, "unsupported session ticket version");
    ensure!(
        printable(&claims.iss, 64)
            && printable(&claims.sub, 64)
            && identifier(&claims.sid)
            && identifier(&claims.jti),
        "malformed session ticket"
    );
    ensure!(
        lower_hex(&claims.aud, 64) && claims.aud == fingerprint.to_ascii_lowercase(),
        "session ticket is for another host"
    );
    ensure!(
        claims.exp > claims.nbf && claims.exp - claims.nbf <= MAX_VALIDITY,
        "session ticket lifetime out of range"
    );
    ensure!(
        now + SKEW >= claims.nbf && now < claims.exp + SKEW,
        "session ticket expired or not yet valid"
    );
    ensure!(
        (1..=MAX_SESSION).contains(&claims.max_session_s),
        "session ticket duration out of range"
    );
    let client_key: [u8; 32] = URL_SAFE_NO_PAD
        .decode(&claims.cnf)
        .ok()
        .and_then(|key| key.try_into().ok())
        .context("malformed session ticket client key")?;
    let permissions = permissions(&claims.perms)?;
    let max_session = Duration::from_secs(claims.max_session_s);
    Ok(Verified {
        claims,
        client_key,
        permissions,
        max_session,
    })
}

fn proof_message(exporter: &[u8; 32]) -> Vec<u8> {
    let mut message = PROOF_LABEL.to_vec();
    message.extend_from_slice(exporter);
    message
}

async fn read_frame(stream: &mut (impl AsyncRead + Unpin)) -> Result<Zeroizing<Vec<u8>>> {
    let size = stream.read_u32().await? as usize;
    ensure!(size > 0 && size <= MAX_FRAME, "invalid ticket frame");
    let mut bytes = Zeroizing::new(vec![0; size]);
    stream.read_exact(&mut bytes).await?;
    Ok(bytes)
}

async fn write_frame(stream: &mut (impl AsyncWrite + Unpin), bytes: &[u8]) -> Result<()> {
    ensure!(
        !bytes.is_empty() && bytes.len() <= MAX_FRAME,
        "invalid ticket frame"
    );
    stream.write_u32(bytes.len() as u32).await?;
    stream.write_all(bytes).await?;
    stream.flush().await?;
    Ok(())
}

/// Client: redeem `ticket` with the host pinned at `fingerprint`, proving
/// the client key `device` (hex seed) the ticket names. Returns a one-session
/// credential carrying that key, for `teleport client --pairing-file`.
pub(crate) async fn redeem_over(
    mut stream: impl AsyncRead + AsyncWrite + Unpin,
    ticket: &str,
    device: &str,
    fingerprint: &str,
) -> Result<Pairing> {
    ensure!(
        ticket.starts_with(PREFIX) && ticket.len() <= MAX_TICKET,
        "not a session ticket"
    );
    let key = crate::e2e::DeviceKey::from_hex(device)?;
    let expected = moq_native::tls::parse_fingerprint(fingerprint)?;
    let mut options = moq_native::tls::Client::default();
    options.fingerprint = vec![fingerprint.to_owned()];
    let mut tls = options.build()?;
    tls.alpn_protocols = vec![ALPN.to_vec()];
    // Nothing is sent before the certificate is verified.
    tls.enable_early_data = false;
    stream.write_all(MAGIC).await?;
    let connector = tokio_rustls::TlsConnector::from(Arc::new(tls));
    let name = tokio_rustls::rustls::pki_types::ServerName::try_from("teleport.local")?;
    let mut stream = tokio::time::timeout(HANDSHAKE_TIMEOUT, connector.connect(name, stream))
        .await
        .context("ticket TLS handshake timed out")?
        .context("ticket host identity verification failed")?;
    ensure!(
        stream.get_ref().1.alpn_protocol() == Some(ALPN),
        "invalid ticket protocol"
    );
    let mut exporter = [0; 32];
    stream
        .get_ref()
        .1
        .export_keying_material(&mut exporter, EXPORTER_LABEL, None)
        .map_err(|error| anyhow!("ticket channel binding failed: {error}"))?;
    write_frame(&mut stream, ticket.as_bytes()).await?;
    write_frame(&mut stream, &key.sign(&proof_message(&exporter))).await?;
    let reply = read_frame(&mut stream).await?;
    ensure!(
        reply.len() == 129 && reply[0] == 1,
        "the host refused the session ticket"
    );
    let token = std::str::from_utf8(&reply[1..65])?;
    let returned = std::str::from_utf8(&reply[65..])?;
    ensure!(
        token.bytes().all(|byte| byte.is_ascii_hexdigit()),
        "invalid ticket credential"
    );
    ensure!(
        moq_native::tls::parse_fingerprint(returned)? == expected,
        "ticket redemption returned a different host identity"
    );
    Ok(Pairing {
        token: token.to_owned(),
        fingerprint: returned.to_owned(),
        device_key: Some(device.to_owned()),
    })
}

/// Client: [`redeem_over`] on a direct TCP connection to the host.
pub async fn redeem(
    address: &str,
    ticket: &str,
    device: &str,
    fingerprint: &str,
) -> Result<Pairing> {
    crate::pairing::validate_address(address)?;
    tokio::time::timeout(REDEEM_TIMEOUT, async {
        let stream = tokio::net::TcpStream::connect(address).await?;
        redeem_over(stream, ticket, device, fingerprint).await
    })
    .await
    .context("ticket redemption timed out")?
}

/// `teleport ticket`: keys and tickets for a small self-hosted issuer, and
/// for testing one (docs/tickets.md).
#[derive(clap::Subcommand)]
pub enum Command {
    /// Make an issuer key: the seed goes to FILE (0600), the public key
    /// (for hosts' --ticket-issuer-key) to stdout.
    IssuerKeygen { out: std::path::PathBuf },
    /// Make a client key: the seed goes to FILE (0600), the public key (a
    /// ticket's `cnf`) to stdout.
    ClientKeygen { out: std::path::PathBuf },
    /// Sign a ticket with the issuer seed in --issuer-key-file.
    Sign {
        #[arg(long)]
        issuer_key_file: std::path::PathBuf,
        #[arg(long, default_value = "teleport")]
        issuer: String,
        /// The host certificate fingerprint (`teleport host-admin status`).
        #[arg(long)]
        host_fingerprint: String,
        /// The client public key from `client-keygen`.
        #[arg(long)]
        client_key: String,
        /// Who the session is for, as the host shows it.
        #[arg(long)]
        name: String,
        #[arg(long, default_value = "manual")]
        session_id: String,
        /// keyboard_mouse, gamepad, clipboard or microphone (repeatable;
        /// none: view only).
        #[arg(long = "perm")]
        perms: Vec<String>,
        /// Seconds the ticket can be redeemed for.
        #[arg(long, default_value_t = 300, value_parser = clap::value_parser!(u64).range(1..=MAX_VALIDITY))]
        valid: u64,
        /// Seconds the session may last.
        #[arg(long, default_value_t = 3600, value_parser = clap::value_parser!(u64).range(1..=MAX_SESSION))]
        max_session: u64,
    },
}

fn read_seed(path: &std::path::Path) -> Result<Zeroizing<[u8; 32]>> {
    let text = Zeroizing::new(std::fs::read_to_string(path)?);
    let text = text.trim();
    ensure!(
        lower_hex(text, 64),
        "{} does not hold a 64-digit hex seed",
        path.display()
    );
    let mut seed = Zeroizing::new([0u8; 32]);
    for (byte, pair) in seed.iter_mut().zip(text.as_bytes().chunks(2)) {
        *byte = u8::from_str_radix(std::str::from_utf8(pair)?, 16)?;
    }
    Ok(seed)
}

/// A client key seed (hex) saved by `client-keygen`.
pub fn read_client_seed(path: &std::path::Path) -> Result<Zeroizing<String>> {
    Ok(Zeroizing::new(hex(read_seed(path)?.as_ref())))
}

fn write_seed(path: &std::path::Path, seed: &[u8; 32]) -> Result<()> {
    crate::identity::write_new(path, hex(seed).as_bytes())
}

pub fn run(command: Command) -> Result<()> {
    match command {
        Command::IssuerKeygen { out } => {
            let seed = Zeroizing::new(crate::e2e::random::<32>());
            write_seed(&out, &seed)?;
            println!("{}", issuer_public_key(&seed)?);
        }
        Command::ClientKeygen { out } => {
            let seed = Zeroizing::new(crate::e2e::random::<32>());
            write_seed(&out, &seed)?;
            let key = crate::e2e::DeviceKey::from_hex(&hex(seed.as_ref()))?;
            println!("{}", URL_SAFE_NO_PAD.encode(key.public_key()));
        }
        Command::Sign {
            issuer_key_file,
            issuer,
            host_fingerprint,
            client_key,
            name,
            session_id,
            perms,
            valid,
            max_session,
        } => {
            let seed = read_seed(&issuer_key_file)?;
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)?
                .as_secs();
            let claims = Claims {
                v: 1,
                iss: issuer,
                aud: host_fingerprint.to_ascii_lowercase(),
                sub: name,
                sid: session_id,
                perms,
                cnf: client_key,
                nbf: now,
                exp: now + valid,
                jti: hex(&crate::e2e::random::<16>()),
                max_session_s: max_session,
            };
            let ticket = sign(&claims, &seed)?;
            // Refuse to print a ticket no host would accept.
            verify(
                &ticket,
                &[parse_issuer_key(&issuer_public_key(&seed)?)?],
                &claims.aud,
                now,
            )?;
            println!("{ticket}");
        }
    }
    Ok(())
}

/// Host side: the TLS identity tickets are redeemed under (the host
/// certificate clients pin).
#[cfg(target_os = "linux")]
pub struct Config {
    acceptor: tokio_rustls::TlsAcceptor,
}

#[cfg(target_os = "linux")]
impl Config {
    pub fn new(directory: &std::path::Path) -> Result<Self> {
        let cert = directory.join("certificate.pem");
        let key = directory.join("key.pem");
        crate::identity::private_file(&cert)?;
        crate::identity::private_file(&key)?;
        let mut options = moq_native::tls::Server::default();
        options.cert = vec![cert];
        options.key = vec![key];
        let tls = options.server_config(vec![ALPN.to_vec()])?;
        Ok(Self {
            acceptor: tokio_rustls::TlsAcceptor::from(tls),
        })
    }
}

/// Host: redeem one ticket ([`MAGIC`] already peeked, still on the stream).
#[cfg(target_os = "linux")]
pub async fn handle(
    mut stream: impl AsyncRead + AsyncWrite + Unpin,
    ip: std::net::IpAddr,
    config: &Config,
    access: Arc<crate::access::ServerState>,
) -> Result<()> {
    tokio::time::timeout(REDEEM_TIMEOUT, async {
        let mut magic = [0; 8];
        tokio::time::timeout(HANDSHAKE_TIMEOUT, stream.read_exact(&mut magic)).await??;
        ensure!(&magic == MAGIC, "invalid ticket protocol");
        ensure!(
            access.tickets_enabled(),
            "this host accepts no session tickets"
        );
        // Bounded like every other unauthenticated login attempt.
        access.system_admit(ip)?;
        let mut stream =
            tokio::time::timeout(HANDSHAKE_TIMEOUT, config.acceptor.accept(stream)).await??;
        ensure!(
            stream.get_ref().1.alpn_protocol() == Some(ALPN),
            "invalid ticket protocol"
        );
        let mut exporter = [0; 32];
        stream
            .get_ref()
            .1
            .export_keying_material(&mut exporter, EXPORTER_LABEL, None)
            .map_err(|error| anyhow!("ticket channel binding failed: {error}"))?;
        let result: Result<Pairing> = async {
            let ticket = read_frame(&mut stream).await?;
            let proof = read_frame(&mut stream).await?;
            let ticket = std::str::from_utf8(&ticket).context("malformed session ticket")?;
            access.redeem_ticket(ticket, |client_key| {
                signature::UnparsedPublicKey::new(&signature::ED25519, client_key)
                    .verify(&proof_message(&exporter), &proof)
                    .is_ok()
            })
        }
        .await;
        let mut reply = Zeroizing::new(Vec::with_capacity(129));
        match result {
            Ok(pairing) => {
                reply.push(1);
                reply.extend_from_slice(pairing.token.as_bytes());
                reply.extend_from_slice(pairing.fingerprint.as_bytes());
            }
            Err(error) => {
                // The client learns only "refused"; the host log says why.
                tracing::warn!(error = format!("{error:#}"), "session ticket refused");
                reply.push(0);
            }
        }
        write_frame(&mut stream, &reply).await?;
        Ok(())
    })
    .await
    .context("ticket redemption timed out")?
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) const ISSUER_SEED: [u8; 32] = [7; 32];

    pub(crate) fn claims(fingerprint: &str, client: &crate::e2e::DeviceKey, now: u64) -> Claims {
        Claims {
            v: 1,
            iss: "test-issuer".into(),
            aud: fingerprint.into(),
            sub: "Support (Test)".into(),
            sid: "session-1".into(),
            perms: vec![],
            cnf: URL_SAFE_NO_PAD.encode(client.public_key()),
            nbf: now,
            exp: now + 300,
            jti: format!("jti-{now}-{}", hex(&crate::e2e::random::<8>())),
            max_session_s: 3600,
        }
    }

    pub(crate) fn issuer() -> [u8; 32] {
        parse_issuer_key(&issuer_public_key(&ISSUER_SEED).unwrap()).unwrap()
    }

    fn now() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs()
    }

    /// The vector an independent issuer implementation (TRaX studio-api,
    /// Go) produced from fixed seeds: its signature must verify here, and
    /// re-signing the same payload must give the same bytes (Ed25519 is
    /// deterministic).
    #[test]
    fn known_answer_vector_from_another_implementation() -> Result<()> {
        const TICKET: &str = "tkt1.eyJ2IjoxLCJpc3MiOiJ0cmF4LXRlc3QiLCJhdWQiOiJhYmFiYWJhYmFiYWJhYmFiYWJhYmFiYWJhYmFiYWJhYmFiYWJhYmFiYWJhYmFiYWJhYmFiYWJhYmFiYWJhYmFiIiwic3ViIjoiVFJhWCBzdXBwb3J0IChUZXN0KSIsInNpZCI6IjBmMWUyZDNjNGI1YTY5Nzg4Nzk2YTViNGMzZDJlMWYwIiwicGVybXMiOltdLCJjbmYiOiJLYXk2NFVHOHl2Q3lMaHFVMDAwTHh6WWVVbTBMX2hMSWw1UzhreUtXYmRjIiwibmJmIjoxNzYxNDAwMDAwLCJleHAiOjE3NjE0MDAzMDAsImp0aSI6InZlY3Rvci1qdGktMDAwMSIsIm1heF9zZXNzaW9uX3MiOjM2MDB9.mercpqM59AW9sPChPHpu-rbg6U9gjocKLPRuSdqMlb3d88RCawEdLDGqYyvrRzbAgGqVIUntOhBHXecSp4Y_Aw";
        let issuer_seed: [u8; 32] = std::array::from_fn(|i| i as u8);
        let client_seed: String = (0x20..0x40u8).map(|b| format!("{b:02x}")).collect();
        let client = crate::e2e::DeviceKey::from_hex(&client_seed)?;
        let issuer = parse_issuer_key(&issuer_public_key(&issuer_seed)?)?;
        let verified = verify(TICKET, &[issuer], &"ab".repeat(32), 1_761_400_100)?;
        assert_eq!(verified.client_key, client.public_key());
        assert_eq!(verified.claims.jti, "vector-jti-0001");
        assert_eq!(verified.claims.sub, "TRaX support (Test)");
        assert!(!verified.permissions.keyboard_mouse);
        assert_eq!(sign(&verified.claims, &issuer_seed)?, TICKET);
        Ok(())
    }

    #[test]
    fn verification_refuses_every_altered_or_misdirected_ticket() -> Result<()> {
        let (client, _) = crate::e2e::DeviceKey::generate()?;
        let fingerprint = "cd".repeat(32);
        let time = now();
        let good = claims(&fingerprint, &client, time);
        let ticket = sign(&good, &ISSUER_SEED)?;
        let verified = verify(&ticket, &[issuer()], &fingerprint, time)?;
        assert_eq!(verified.max_session, Duration::from_secs(3600));

        // Another issuer, another host, no issuers.
        assert!(verify(&ticket, &[[9; 32]], &fingerprint, time).is_err());
        assert!(verify(&ticket, &[issuer()], &"ef".repeat(32), time).is_err());
        assert!(verify(&ticket, &[], &fingerprint, time).is_err());
        // Expired, not yet valid (beyond the skew).
        assert!(verify(&ticket, &[issuer()], &fingerprint, time + 300 + SKEW).is_err());
        assert!(verify(&ticket, &[issuer()], &fingerprint, time - SKEW - 1).is_err());
        assert!(verify(&ticket, &[issuer()], &fingerprint, time + 300 + SKEW - 1).is_ok());

        // A payload byte changed after signing.
        let (head, signature) = ticket.rsplit_once('.').unwrap();
        let mut tampered = head.to_owned().into_bytes();
        let last = tampered.len() - 1;
        tampered[last] = if tampered[last] == b'A' { b'B' } else { b'A' };
        let tampered = format!("{}.{signature}", String::from_utf8(tampered)?);
        assert!(verify(&tampered, &[issuer()], &fingerprint, time).is_err());

        // Well signed but out of range, unknown permission or unknown field.
        for change in [
            |c: &mut Claims| c.exp = c.nbf + MAX_VALIDITY + 1,
            |c: &mut Claims| c.max_session_s = 0,
            |c: &mut Claims| c.max_session_s = MAX_SESSION + 1,
            |c: &mut Claims| c.perms = vec!["root".into()],
            |c: &mut Claims| c.perms = vec!["gamepad".into(), "gamepad".into()],
            |c: &mut Claims| c.sid = "has space".into(),
            |c: &mut Claims| c.v = 2,
            |c: &mut Claims| c.cnf = "AAAA".into(),
        ] {
            let mut bad = good.clone();
            change(&mut bad);
            assert!(verify(&sign(&bad, &ISSUER_SEED)?, &[issuer()], &fingerprint, time).is_err());
        }
        let mut extra = serde_json::to_value(&good)?;
        extra["admin"] = true.into();
        let payload = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&extra)?);
        let key = signature::Ed25519KeyPair::from_seed_unchecked(&ISSUER_SEED).unwrap();
        let mut message = SIGNED_PREFIX.to_vec();
        message.extend_from_slice(payload.as_bytes());
        let unknown = format!(
            "{PREFIX}{payload}.{}",
            URL_SAFE_NO_PAD.encode(key.sign(&message))
        );
        assert!(verify(&unknown, &[issuer()], &fingerprint, time).is_err());

        // Permissions map onto the guest model.
        let mut control = good.clone();
        control.perms = vec!["keyboard_mouse".into()];
        let verified = verify(
            &sign(&control, &ISSUER_SEED)?,
            &[issuer()],
            &fingerprint,
            time,
        )?;
        assert!(verified.permissions.keyboard_mouse && !verified.permissions.clipboard);
        Ok(())
    }
}
