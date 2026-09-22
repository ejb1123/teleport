//! One-time code enrollment. SPAKE2 authenticates both endpoints without sending
//! the code; directional HKDF keys confirm the client and encrypt the credential.
//! Normal desktop connections ALWAYS use the resulting pinned TLS certificate.
use crate::protocol::Pairing;
use anyhow::{Context, Result, ensure};
use ring::{aead, digest, hkdf, hmac};
use spake2::{Ed25519Group, Identity, Password, Spake2};
use std::time::Duration;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
};

const MAGIC: &[u8; 8] = b"TPPAIR01";
const CLIENT: &[u8] = b"teleport/pair/v1/client";
const HOST: &[u8] = b"teleport/pair/v1/host";
const TIMEOUT: Duration = Duration::from_secs(10);
#[cfg(target_os = "linux")]
const LIFETIME: Duration = Duration::from_secs(300);
#[cfg(target_os = "linux")]
const ATTEMPTS: u8 = 5;

fn normalize(code: &str) -> Result<String> {
    ensure!(
        code.len() <= 32,
        "enter the six-digit pairing code from the host"
    );
    let code: String = code
        .chars()
        .filter(|c| *c != '-' && !c.is_ascii_whitespace())
        .collect();
    ensure!(
        code.len() == 6 && code.bytes().all(|c| c.is_ascii_digit()),
        "enter the six-digit pairing code from the host"
    );
    Ok(code)
}

pub fn validate_address(address: &str) -> Result<()> {
    ensure!(
        !address.is_empty()
            && !address.contains(['/', '@', '?', '#'])
            && !address.chars().any(char::is_whitespace),
        "enter host:port, not a URL"
    );
    let url: url::Url = format!("moqt://{address}").parse()?;
    ensure!(
        url.host_str().is_some() && url.port().is_some(),
        "include the host port, e.g. desktop.local:4443"
    );
    Ok(())
}

struct Keys {
    confirmation: hmac::Key,
    encryption: aead::LessSafeKey,
    transcript: Vec<u8>,
}
impl Keys {
    fn new(shared: &[u8], client: &[u8], host: &[u8]) -> Result<Self> {
        let mut transcript = MAGIC.to_vec();
        transcript.extend_from_slice(client);
        transcript.extend_from_slice(host);
        let hash = digest::digest(&digest::SHA256, &transcript);
        let prk = hkdf::Salt::new(hkdf::HKDF_SHA256, hash.as_ref()).extract(shared);
        struct Length;
        impl hkdf::KeyType for Length {
            fn len(&self) -> usize {
                32
            }
        }
        let derive = |label: &[u8]| -> Result<[u8; 32]> {
            let mut key = [0; 32];
            prk.expand(&[label], Length)
                .map_err(|_| anyhow::anyhow!("pairing key derivation failed"))?
                .fill(&mut key)
                .map_err(|_| anyhow::anyhow!("pairing key derivation failed"))?;
            Ok(key)
        };
        let confirmation =
            hmac::Key::new(hmac::HMAC_SHA256, &derive(b"teleport/client-confirm/v1")?);
        let encryption = aead::LessSafeKey::new(
            aead::UnboundKey::new(
                &aead::CHACHA20_POLY1305,
                &derive(b"teleport/host-credential/v1")?,
            )
            .map_err(|_| anyhow::anyhow!("pairing cipher initialization failed"))?,
        );
        Ok(Self {
            confirmation,
            encryption,
            transcript,
        })
    }
    #[cfg(target_os = "linux")]
    fn seal(&self, pairing: &Pairing) -> Result<Vec<u8>> {
        let nonce: [u8; 12] = rand::random();
        let mut payload = serde_json::to_vec(pairing)?;
        self.encryption
            .seal_in_place_append_tag(
                aead::Nonce::assume_unique_for_key(nonce),
                aead::Aad::from(&self.transcript),
                &mut payload,
            )
            .map_err(|_| anyhow::anyhow!("credential encryption failed"))?;
        let mut packet = nonce.to_vec();
        packet.extend_from_slice(&payload);
        Ok(packet)
    }
    fn open(&self, mut packet: Vec<u8>) -> Result<Pairing> {
        ensure!(packet.len() >= 28, "invalid encrypted pairing response");
        let nonce: [u8; 12] = packet[..12].try_into()?;
        let plaintext = self
            .encryption
            .open_in_place(
                aead::Nonce::assume_unique_for_key(nonce),
                aead::Aad::from(&self.transcript),
                &mut packet[12..],
            )
            .map_err(|_| {
                anyhow::anyhow!("pairing authentication failed; check the code on the host")
            })?;
        let pairing: Pairing = serde_json::from_slice(plaintext)?;
        pairing.validate()?;
        Ok(pairing)
    }
}

async fn write_packet(stream: &mut TcpStream, data: &[u8]) -> Result<()> {
    ensure!(data.len() <= 4096, "pairing message too large");
    stream.write_u16(data.len() as u16).await?;
    stream.write_all(data).await?;
    Ok(())
}
async fn read_packet(stream: &mut TcpStream, maximum: usize) -> Result<Vec<u8>> {
    let length = stream.read_u16().await? as usize;
    ensure!(
        length > 0 && length <= maximum,
        "invalid pairing message length"
    );
    let mut data = vec![0; length];
    stream.read_exact(&mut data).await?;
    Ok(data)
}

pub async fn pair(address: &str, code: &str) -> Result<Pairing> {
    validate_address(address)?;
    let code = normalize(code)?;
    tokio::time::timeout(TIMEOUT, async {
        let mut stream = TcpStream::connect(address).await.context(
            "cannot reach pairing service; start host with --pair and allow TCP on its port",
        )?;
        let (state, message) = Spake2::<Ed25519Group>::start_a(
            &Password::new(code.as_bytes()),
            &Identity::new(CLIENT),
            &Identity::new(HOST),
        );
        stream.write_all(MAGIC).await?;
        write_packet(&mut stream, &message).await?;
        let remote = read_packet(&mut stream, 128).await?;
        let shared = state
            .finish(&remote)
            .map_err(|_| anyhow::anyhow!("invalid pairing exchange"))?;
        let keys = Keys::new(&shared, &message, &remote)?;
        let confirmation = hmac::sign(&keys.confirmation, &keys.transcript);
        write_packet(&mut stream, confirmation.as_ref()).await?;
        let packet = read_packet(&mut stream, 4096).await.context(
            "pairing rejected; code may be wrong, used, expired, or locked after five attempts",
        )?;
        keys.open(packet)
    })
    .await
    .context("pairing timed out; check host code and TCP firewall")?
}

#[cfg(target_os = "linux")]
pub struct Enrollment(tokio::task::JoinHandle<()>);
#[cfg(target_os = "linux")]
impl Drop for Enrollment {
    fn drop(&mut self) {
        self.0.abort();
    }
}

#[cfg(target_os = "linux")]
pub async fn start(address: std::net::SocketAddr, pairing: Pairing) -> Result<Enrollment> {
    let listener = tokio::net::TcpListener::bind(address)
        .await
        .context("cannot open code pairing TCP port")?;
    let code = format!("{:06}", rand::random_range(0..1_000_000u32));
    println!(
        "Pairing code: {}-{} (one use, expires in 5 minutes; TCP {})",
        &code[..3],
        &code[3..],
        address.port()
    );
    Ok(Enrollment(tokio::spawn(async move {
        if let Err(error) = serve(listener, &code, &pairing, LIFETIME).await {
            tracing::warn!(%error, "code pairing stopped");
        }
    })))
}

#[cfg(target_os = "linux")]
async fn serve(
    listener: tokio::net::TcpListener,
    code: &str,
    pairing: &Pairing,
    lifetime: Duration,
) -> Result<()> {
    let deadline = tokio::time::Instant::now() + lifetime;
    for _ in 0..ATTEMPTS {
        if tokio::time::Instant::now() >= deadline {
            return Ok(());
        }
        let Ok(accepted) = tokio::time::timeout_at(deadline, listener.accept()).await else {
            tracing::info!("pairing code expired; restart with --pair for a new code");
            return Ok(());
        };
        let (mut stream, _) = accepted?;
        let mut used = false;
        let result = tokio::time::timeout_at(
            deadline.min(tokio::time::Instant::now() + TIMEOUT),
            accept(&mut stream, code, pairing, &mut used, deadline),
        )
        .await;
        if used {
            tracing::info!("pairing code consumed; future connections use saved trust");
            return Ok(());
        }
        if !matches!(result, Ok(Ok(()))) {
            tracing::warn!("pairing attempt rejected (code not logged)");
        }
    }
    tracing::warn!("pairing disabled after five attempts; restart with --pair for a new code");
    Ok(())
}

#[cfg(target_os = "linux")]
pub(crate) async fn accept(
    stream: &mut TcpStream,
    code: &str,
    pairing: &Pairing,
    used: &mut bool,
    deadline: tokio::time::Instant,
) -> Result<()> {
    accept_with_access(stream, code, pairing, used, deadline, None).await
}

#[cfg(target_os = "linux")]
pub(crate) struct DeviceEnrollment<'a> {
    pub access: &'a crate::access::ServerState,
    pub epoch: &'a std::sync::atomic::AtomicU64,
    pub generation: u64,
}

#[cfg(target_os = "linux")]
pub(crate) async fn accept_with_access(
    stream: &mut TcpStream,
    code: &str,
    pairing: &Pairing,
    used: &mut bool,
    deadline: tokio::time::Instant,
    access: Option<DeviceEnrollment<'_>>,
) -> Result<()> {
    let mut magic = [0; 8];
    stream.read_exact(&mut magic).await?;
    ensure!(&magic == MAGIC, "unsupported pairing protocol");
    let client = read_packet(stream, 128).await?;
    let (state, host) = Spake2::<Ed25519Group>::start_b(
        &Password::new(code.as_bytes()),
        &Identity::new(CLIENT),
        &Identity::new(HOST),
    );
    let shared = state
        .finish(&client)
        .map_err(|_| anyhow::anyhow!("invalid pairing exchange"))?;
    let keys = Keys::new(&shared, &client, &host)?;
    write_packet(stream, &host).await?;
    let proof = read_packet(stream, 32).await?;
    hmac::verify(&keys.confirmation, &keys.transcript, &proof)
        .map_err(|_| anyhow::anyhow!("pairing authentication failed"))?;
    ensure!(
        tokio::time::Instant::now() < deadline,
        "pairing code expired"
    );
    // Consume before releasing credentials, even if the peer disconnects mid-write.
    if let Some(access) = &access {
        use std::sync::atomic::Ordering;
        access
            .epoch
            .compare_exchange(
                access.generation,
                access.generation.wrapping_add(1),
                Ordering::SeqCst,
                Ordering::SeqCst,
            )
            .map_err(|_| anyhow::anyhow!("pairing code closed or replaced"))?;
    }
    *used = true;
    let device = access
        .map(|access| access.access.enroll_device("Code-paired device"))
        .transpose()?;
    write_packet(stream, &keys.seal(device.as_ref().unwrap_or(pairing))?).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn code_and_address_validation() {
        assert_eq!(normalize("123-456\n").unwrap(), "123456");
        for code in ["12345", "abcdef", "１２３４５６", "1234567"] {
            assert!(normalize(code).is_err());
        }
        for address in ["host", "host:123/a", "user@host:1", "host:2?x"] {
            assert!(validate_address(address).is_err());
        }
        assert!(validate_address("[::1]:4443").is_ok());
    }
    #[test]
    #[cfg(target_os = "linux")]
    fn authenticated_keys_reject_wrong_code_and_tampering() -> Result<()> {
        let (a, msg_a) = Spake2::<Ed25519Group>::start_a(
            &Password::new(b"123456"),
            &Identity::new(CLIENT),
            &Identity::new(HOST),
        );
        let (b, msg_b) = Spake2::<Ed25519Group>::start_b(
            &Password::new(b"123456"),
            &Identity::new(CLIENT),
            &Identity::new(HOST),
        );
        let ka = Keys::new(&a.finish(&msg_b).unwrap(), &msg_a, &msg_b)?;
        let kb = Keys::new(&b.finish(&msg_a).unwrap(), &msg_a, &msg_b)?;
        hmac::verify(
            &kb.confirmation,
            &kb.transcript,
            hmac::sign(&ka.confirmation, &ka.transcript).as_ref(),
        )
        .unwrap();
        let pairing = Pairing {
            token: "a".repeat(64),
            fingerprint: "b".repeat(64),
        };
        let mut encrypted = kb.seal(&pairing)?;
        assert_eq!(ka.open(encrypted.clone())?.token, pairing.token);
        encrypted[20] ^= 1;
        assert!(ka.open(encrypted).is_err());
        let wrong = Keys::new(b"wrong key", &msg_a, &msg_b)?;
        assert!(wrong.open(kb.seal(&pairing)?).is_err());
        Ok(())
    }
    #[tokio::test]
    #[cfg(target_os = "linux")]
    #[ignore = "requires local TCP sockets"]
    async fn code_pairing_rejects_wrong_reuse_expiry_and_exhaustion() -> Result<()> {
        let credential = Pairing {
            token: "a".repeat(64),
            fingerprint: "b".repeat(64),
        };
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let address = listener.local_addr()?.to_string();
        let saved = credential.clone();
        let host = tokio::spawn(async move {
            serve(listener, "123456", &saved, Duration::from_secs(10)).await
        });
        assert!(pair(&address, "000000").await.is_err());
        assert_eq!(pair(&address, "123456").await?.token, credential.token);
        host.await??;
        assert!(pair(&address, "123456").await.is_err());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let address = listener.local_addr()?.to_string();
        serve(listener, "123456", &credential, Duration::ZERO).await?;
        assert!(pair(&address, "123456").await.is_err());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let address = listener.local_addr()?.to_string();
        let host = tokio::spawn(async move {
            serve(listener, "123456", &credential, Duration::from_secs(10)).await
        });
        for _ in 0..ATTEMPTS {
            assert!(pair(&address, "000000").await.is_err());
        }
        host.await??;
        assert!(pair(&address, "123456").await.is_err());
        Ok(())
    }
}
