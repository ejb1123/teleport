//! One-time code enrollment. SPAKE2 authenticates both endpoints without sending
//! the code; directional HKDF keys confirm the client and encrypt the credential.
//! Normal desktop connections ALWAYS use the resulting pinned TLS certificate.
use crate::protocol::Pairing;
use anyhow::{Context, Result, ensure};
use ring::{aead, digest, hkdf, hmac};
use spake2::{Ed25519Group, Identity, Password, Spake2};
use std::time::Duration;
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    net::TcpStream,
};

/// Clients from before device keys.
pub(crate) const MAGIC_V1: &[u8; 8] = b"TPPAIR01";
/// Clients that offer a device key before the credential is issued. The magic
/// is part of the SPAKE2 transcript, so a relay can neither strip nor add it.
pub(crate) const MAGIC: &[u8; 8] = b"TPPAIR02";
const CLIENT: &[u8] = b"teleport/pair/v1/client";
const HOST: &[u8] = b"teleport/pair/v1/host";
pub(crate) const TIMEOUT: Duration = Duration::from_secs(10);
#[cfg(target_os = "linux")]
const LIFETIME: Duration = Duration::from_secs(300);
#[cfg(target_os = "linux")]
const ATTEMPTS: u8 = 5;

pub(crate) fn normalize(code: &str) -> Result<String> {
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
    /// Device key and credential binding (`crate::e2e::Registration`),
    /// used with [`MAGIC`].
    registration: crate::e2e::Registration,
}
impl Keys {
    fn new(magic: &[u8; 8], shared: &[u8], client: &[u8], host: &[u8]) -> Result<Self> {
        let mut transcript = magic.to_vec();
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
        let registration = crate::e2e::Registration::new(
            &derive(b"teleport/device-key-registration/v1")?,
            &transcript,
        )?;
        Ok(Self {
            confirmation,
            encryption,
            transcript,
            registration,
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

async fn write_packet(stream: &mut (impl AsyncWrite + Unpin), data: &[u8]) -> Result<()> {
    ensure!(data.len() <= 4096, "pairing message too large");
    stream.write_u16(data.len() as u16).await?;
    stream.write_all(data).await?;
    Ok(())
}
async fn read_packet(stream: &mut (impl AsyncRead + Unpin), maximum: usize) -> Result<Vec<u8>> {
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
    pair_with(address, code, true).await
}

/// A client from before device keys (`TPPAIR01`).
#[cfg(test)]
pub(crate) async fn pair_without_device_key(address: &str, code: &str) -> Result<Pairing> {
    pair_with(address, code, false).await
}

async fn pair_with(address: &str, code: &str, device_key: bool) -> Result<Pairing> {
    validate_address(address)?;
    let code = normalize(code)?;
    tokio::time::timeout(TIMEOUT, async {
        let stream = TcpStream::connect(address).await.context(
            "cannot reach pairing service; start host with --pair and allow TCP on its port",
        )?;
        pair_over(stream, &code, device_key).await
    })
    .await
    .context("pairing timed out; check host code and TCP firewall")?
}

/// Client side of code pairing over any ordered byte stream: direct TCP, or a
/// relay stream (`crate::relay_enroll`). The caller bounds the time.
/// `device_key`: offer a device key, which the host stores with the issued
/// credential ([`MAGIC`]); false only to emulate clients from before device
/// keys ([`MAGIC_V1`]).
pub(crate) async fn pair_over(
    mut stream: impl AsyncRead + AsyncWrite + Unpin,
    code: &str,
    device_key: bool,
) -> Result<Pairing> {
    let code = normalize(code)?;
    let magic = if device_key { MAGIC } else { MAGIC_V1 };
    let (state, message) = Spake2::<Ed25519Group>::start_a(
        &Password::new(code.as_bytes()),
        &Identity::new(CLIENT),
        &Identity::new(HOST),
    );
    stream.write_all(magic).await?;
    write_packet(&mut stream, &message).await?;
    stream.flush().await?;
    // Hosts from before device keys close the connection on `TPPAIR02`.
    let remote = read_packet(&mut stream, 128)
        .await
        .context("no pairing answer; the host may need a newer Teleport")?;
    let shared = state
        .finish(&remote)
        .map_err(|_| anyhow::anyhow!("invalid pairing exchange"))?;
    let keys = Keys::new(magic, &shared, &message, &remote)?;
    let confirmation = hmac::sign(&keys.confirmation, &keys.transcript);
    write_packet(&mut stream, confirmation.as_ref()).await?;
    // The device key goes before the credential exists (`crate::e2e::Registration`).
    let offer = match device_key {
        true => Some(crate::access::DeviceKeyOffer::new()?),
        false => None,
    };
    if let Some(offer) = &offer {
        write_packet(&mut stream, &offer.request(&keys.registration)).await?;
    }
    stream.flush().await?;
    let packet = read_packet(&mut stream, 4096).await.context(
        "pairing rejected; code may be wrong, used, expired, or locked after five attempts",
    )?;
    match offer {
        // Keyed unless the host explicitly (and authenticated) sent its own
        // pairing token, which has no device record.
        Some(offer) => offer
            .accept(&keys.registration, &packet)
            .context("pairing authentication failed; check the code on the host"),
        None => Ok(Pairing {
            device_key: None,
            ..keys.open(packet)?
        }),
    }
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
    stream: &mut (impl AsyncRead + AsyncWrite + Unpin),
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
    stream: &mut (impl AsyncRead + AsyncWrite + Unpin),
    code: &str,
    pairing: &Pairing,
    used: &mut bool,
    deadline: tokio::time::Instant,
    access: Option<DeviceEnrollment<'_>>,
) -> Result<()> {
    let mut magic = [0; 8];
    stream.read_exact(&mut magic).await?;
    ensure!(
        &magic == MAGIC || &magic == MAGIC_V1,
        "unsupported pairing protocol"
    );
    let keyed = &magic == MAGIC;
    let client = read_packet(stream, 128).await?;
    let (state, host) = Spake2::<Ed25519Group>::start_b(
        &Password::new(code.as_bytes()),
        &Identity::new(CLIENT),
        &Identity::new(HOST),
    );
    let shared = state
        .finish(&client)
        .map_err(|_| anyhow::anyhow!("invalid pairing exchange"))?;
    let keys = Keys::new(&magic, &shared, &client, &host)?;
    write_packet(stream, &host).await?;
    stream.flush().await?;
    let proof = read_packet(stream, 32).await?;
    hmac::verify(&keys.confirmation, &keys.transcript, &proof)
        .map_err(|_| anyhow::anyhow!("pairing authentication failed"))?;
    // A capable client's device key, before the code is consumed: without it
    // nothing is issued and the code stays usable.
    let device_key = match keyed {
        true => Some(crate::access::read_device_key(stream, &keys.registration).await?),
        false => None,
    };
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
    let seal =
        |pairing: &Pairing, stored: Option<&[u8; crate::e2e::DEVICE_PUBLIC_KEY_LEN]>| match keyed {
            true => Ok(keys
                .registration
                .seal_credential(stored, &serde_json::to_vec(pairing)?)),
            false => keys.seal(pairing),
        };
    let Some(access) = access else {
        // The host's own pairing file: no device record to store a key in,
        // which the sealed answer says explicitly.
        write_packet(stream, &seal(pairing, None)?).await?;
        stream.flush().await?;
        return Ok(());
    };
    let device = access
        .access
        .enroll_device("Code-paired device", device_key.as_ref())?;
    let packet = seal(&device.pairing, device_key.as_ref())?;
    access.access.deliver(stream, &device, &packet).await
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
        let ka = Keys::new(MAGIC, &a.finish(&msg_b).unwrap(), &msg_a, &msg_b)?;
        let kb = Keys::new(MAGIC, &b.finish(&msg_a).unwrap(), &msg_a, &msg_b)?;
        hmac::verify(
            &kb.confirmation,
            &kb.transcript,
            hmac::sign(&ka.confirmation, &ka.transcript).as_ref(),
        )
        .unwrap();
        let pairing = Pairing {
            token: "a".repeat(64),
            fingerprint: "b".repeat(64),
            device_key: None,
        };
        let mut encrypted = kb.seal(&pairing)?;
        assert_eq!(ka.open(encrypted.clone())?.token, pairing.token);
        encrypted[20] ^= 1;
        assert!(ka.open(encrypted).is_err());
        let wrong = Keys::new(MAGIC, b"wrong key", &msg_a, &msg_b)?;
        assert!(wrong.open(kb.seal(&pairing)?).is_err());
        Ok(())
    }
    #[cfg(target_os = "linux")]
    struct Meddled {
        client: Result<Pairing>,
        host: Result<()>,
        used: bool,
        /// A hash-only device record existed at some moment of the exchange.
        hash_only_seen: bool,
    }

    /// Code pairing through a [`crate::access::meddler`] relay, against a
    /// host with managed devices (`access`) or only its own pairing file.
    #[cfg(target_os = "linux")]
    async fn meddled_pair(
        access: Option<(&crate::access::ServerState, &std::sync::atomic::AtomicU64)>,
        device_key: bool,
        decide: impl Fn(bool, usize, &[u8]) -> crate::access::meddler::Action + Send + Sync + 'static,
    ) -> Meddled {
        let host_token = Pairing {
            token: "a".repeat(64),
            fingerprint: "b".repeat(64),
            device_key: None,
        };
        let (client, host, relay) = crate::access::meddler::spawn(decide);
        let mut used = false;
        let enrollment = access.map(|(access, epoch)| DeviceEnrollment {
            access,
            epoch,
            generation: epoch.load(std::sync::atomic::Ordering::SeqCst),
        });
        let deadline = tokio::time::Instant::now() + TIMEOUT;
        let accept = async {
            // The host hangs up when done, as `serve` and `host_admin` do.
            let mut host = host;
            accept_with_access(
                &mut host,
                "123456",
                &host_token,
                &mut used,
                deadline,
                enrollment,
            )
            .await
        };
        let exchange = async { tokio::join!(pair_over(client, "123-456", device_key), accept) };
        let hash_only = || {
            access.is_some_and(|(access, _)| {
                access.relay_credentials().iter().any(|c| {
                    c.hash_only() && c.hash != crate::e2e::credential_hash(&host_token.token)
                })
            })
        };
        let mut hash_only_seen = false;
        let watcher = async {
            loop {
                hash_only_seen |= hash_only();
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        };
        let (client, host) = tokio::select! {
            result = exchange => result,
            _ = watcher => unreachable!(),
        };
        relay.abort();
        Meddled {
            client,
            host,
            used,
            hash_only_seen: hash_only_seen || hash_only(),
        }
    }

    /// The fix for a relay that dropped or delayed the device key around the
    /// credential: a capable client's device is created with its key or not
    /// at all, and the client keeps nothing unless the host bound its key.
    #[tokio::test]
    #[cfg(target_os = "linux")]
    async fn keyed_code_pairing_resists_a_meddling_relay() -> Result<()> {
        use crate::access::meddler::Action;
        use std::os::unix::fs::PermissionsExt;
        // Towards the host: 0 magic, 1 SPAKE2, 2 confirmation, 3 device key.
        // Towards the client: 0 SPAKE2, 1 credential.
        const KEY: usize = 3;
        let directory = tempfile::tempdir()?;
        std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700))?;
        let host_token = Pairing {
            token: "a".repeat(64),
            fingerprint: "b".repeat(64),
            device_key: None,
        };
        let state = crate::access::ServerState::open(directory.path(), host_token.clone())?;
        let epoch = std::sync::atomic::AtomicU64::new(0);
        let access = Some((&*state, &epoch));
        let at_key = |action: fn() -> Action| {
            move |up: bool, index: usize, _: &[u8]| match up && index == KEY {
                true => action(),
                false => Action::Forward,
            }
        };

        // Device key dropped: nothing issued, the code is not even consumed.
        let run = meddled_pair(access, true, at_key(|| Action::HangUp)).await;
        assert!(run.client.is_err() && run.host.is_err() && !run.used && !run.hash_only_seen);
        assert!(state.status().devices.is_empty());

        // The host cannot deliver the credential: it removes the device.
        let run = meddled_pair(access, true, at_key(|| Action::ForwardThenHangUp)).await;
        assert!(run.client.is_err() && run.host.is_err() && run.used && !run.hash_only_seen);
        assert!(state.status().devices.is_empty());

        // Credential dropped after delivery: the client saves nothing; the
        // host keeps a keyed record nobody holds the key for. Pairing again
        // (with a new code) works.
        let run = meddled_pair(access, true, |up, index, _| match !up && index == 1 {
            true => Action::HangUp,
            false => Action::Forward,
        })
        .await;
        assert!(run.client.is_err() && run.host.is_ok() && run.used && !run.hash_only_seen);
        assert_eq!(state.status().devices.len(), 1);

        // Device key held back: no hash-only record at any point; keyed.
        let run = meddled_pair(
            access,
            true,
            at_key(|| Action::Delay(Duration::from_millis(300))),
        )
        .await;
        let paired = run.client?;
        run.host?;
        assert!(!run.hash_only_seen);
        let public =
            crate::e2e::DeviceKey::from_hex(paired.device_key.as_deref().unwrap())?.public_key();
        let hash = crate::e2e::credential_hash(&paired.token).to_vec();
        assert!(state.begin_session_hash(hash.clone(), None).is_err());
        drop(state.begin_session_hash(hash, Some(&public))?);

        // The magic is in the SPAKE2 transcript: stripping the capability, or
        // adding it to an old client's request, fails key confirmation.
        let swap = |to: &'static [u8; 8]| {
            move |up: bool, index: usize, _: &[u8]| match up && index == 0 {
                true => Action::Replace(to.to_vec()),
                false => Action::Forward,
            }
        };
        let run = meddled_pair(access, true, swap(MAGIC_V1)).await;
        assert!(run.client.is_err() && run.host.is_err() && !run.used);
        let run = meddled_pair(access, false, swap(MAGIC)).await;
        assert!(run.client.is_err() && run.host.is_err() && !run.used);
        assert_eq!(state.status().devices.len(), 2);

        // A client from before device keys still pairs, hash-only.
        let run = meddled_pair(access, false, |_, _, _| Action::Forward).await;
        let old = run.client?;
        run.host?;
        assert!(old.device_key.is_none() && run.hash_only_seen);
        assert!(state.authorized(&old.token));

        // A host pairing out its own token says so, authenticated: the
        // client saves it unkeyed (there is no device record to key).
        let run = meddled_pair(None, true, |_, _, _| Action::Forward).await;
        let shared = run.client?;
        run.host?;
        assert!(shared.token == host_token.token && shared.device_key.is_none());
        Ok(())
    }

    #[tokio::test]
    #[cfg(target_os = "linux")]
    #[ignore = "requires local TCP sockets"]
    async fn code_pairing_rejects_wrong_reuse_expiry_and_exhaustion() -> Result<()> {
        let credential = Pairing {
            token: "a".repeat(64),
            fingerprint: "b".repeat(64),
            device_key: None,
        };
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let address = listener.local_addr()?.to_string();
        let saved = credential.clone();
        let host = tokio::spawn(async move {
            serve(listener, "123456", &saved, Duration::from_secs(10)).await
        });
        assert!(pair(&address, "000000").await.is_err());
        let paired = pair(&address, "123456").await?;
        assert_eq!(paired.token, credential.token);
        // The host's own pairing file has no device record to key.
        assert!(paired.device_key.is_none());
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
