//! Persistent password enrollment using OPAQUE, followed by pinned QUIC access.
//! This integration has not received an independent security audit. Passwords
//! are never stored; enrollment records and device credentials remain secrets.
use anyhow::{Context, Result, ensure};
use opaque_ke::{
    CipherSuite, ClientLogin, ClientLoginFinishParameters, CredentialResponse, Identifiers,
};
use rand08::rngs::OsRng;
use ring::{aead, hkdf};
use serde::{Deserialize, Serialize};
use std::time::Duration;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
};

use crate::protocol::Pairing;

pub const MAGIC: [u8; 8] = *b"TPAUTH01";
const TIMEOUT: Duration = Duration::from_secs(90);
const MAX_PACKET: usize = 4096;

struct Suite;
impl CipherSuite for Suite {
    type OprfCs = opaque_ke::Ristretto255;
    type KeyExchange = opaque_ke::TripleDh<opaque_ke::Ristretto255, sha2::Sha512>;
    type Ksf = argon2::Argon2<'static>;
}

// TPAUTH01 fixes these parameters; changes require a new protocol/storage version.
// Argon2id v19: 64 MiB, three passes, one lane. Registration runs only in
// local administration; login stretching is client-side, not an unbounded
// unauthenticated server workload.
fn password_ksf() -> argon2::Argon2<'static> {
    argon2::Argon2::new(
        argon2::Algorithm::Argon2id,
        argon2::Version::V0x13,
        argon2::Params::new(65_536, 3, 1, None).expect("fixed Argon2 parameters"),
    )
}

#[derive(Serialize, Deserialize)]
struct Hello {
    username: String,
    device_name: String,
    #[serde(default)]
    u2f: bool,
    /// The client offers a device key before its credential is issued
    /// (`crate::e2e::Registration`). Part of the OPAQUE context, so a relay
    /// cannot strip it to downgrade the device to hash-only (or add it).
    /// Absent from clients that predate device keys.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    device_key: bool,
}

#[derive(Serialize, Deserialize)]
struct TouchChallenge {
    credential: crate::security_key::Credential,
    hash: [u8; 32],
}

fn touch_cipher(session: &[u8], label: &[u8]) -> Result<aead::LessSafeKey> {
    let prk = hkdf::Salt::new(hkdf::HKDF_SHA256, &MAGIC).extract(session);
    let mut key = [0; 32];
    prk.expand(&[label], hkdf::HKDF_SHA256)
        .map_err(|_| anyhow::anyhow!("key derivation failed"))?
        .fill(&mut key)
        .map_err(|_| anyhow::anyhow!("key derivation failed"))?;
    Ok(aead::LessSafeKey::new(
        aead::UnboundKey::new(&aead::CHACHA20_POLY1305, &key)
            .map_err(|_| anyhow::anyhow!("cipher initialization failed"))?,
    ))
}
const TOUCH_REQUEST: &[u8] = b"teleport/u2f/challenge/v1";
const TOUCH_REPLY: &[u8] = b"teleport/u2f/assertion/v1";
fn seal_touch(session: &[u8], label: &[u8], context: &[u8], mut data: Vec<u8>) -> Result<Vec<u8>> {
    let nonce: [u8; 12] = rand::random();
    touch_cipher(session, label)?
        .seal_in_place_append_tag(
            aead::Nonce::assume_unique_for_key(nonce),
            aead::Aad::from(context),
            &mut data,
        )
        .map_err(|_| anyhow::anyhow!("security-key message encryption failed"))?;
    let mut packet = nonce.to_vec();
    packet.extend(data);
    Ok(packet)
}
fn open_touch(session: &[u8], label: &[u8], context: &[u8], mut data: Vec<u8>) -> Result<Vec<u8>> {
    ensure!(data.len() >= 28, "invalid security-key message");
    let nonce = data[..12].try_into()?;
    Ok(touch_cipher(session, label)?
        .open_in_place(
            aead::Nonce::assume_unique_for_key(nonce),
            aead::Aad::from(context),
            &mut data[12..],
        )
        .map_err(|_| anyhow::anyhow!("security-key message authentication failed"))?
        .to_vec())
}

pub fn validate_username(username: &str) -> Result<()> {
    ensure!(
        !username.is_empty()
            && username.len() <= 64
            && username
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"_.-".contains(&b)),
        "username must be 1–64 ASCII letters, numbers, dots, underscores or hyphens (case-sensitive)"
    );
    Ok(())
}

#[cfg(any(target_os = "linux", test))]
pub fn validate_password(password: &str) -> Result<()> {
    ensure!(
        password.len() <= 1024 && password.chars().count() >= 12,
        "use a password or passphrase of at least 12 characters (maximum 1024 bytes)"
    );
    ensure!(
        !password
            .chars()
            .all(|c| c.is_ascii_digit() || c.is_whitespace()),
        "use a passphrase, not a permanent numeric code"
    );
    ensure!(
        password.chars().any(|c| !c.is_whitespace())
            && password.chars().any(|c| !password.starts_with(c)),
        "use a strong, non-repeating passphrase"
    );
    Ok(())
}

fn validate_hello(hello: &Hello) -> Result<()> {
    validate_username(&hello.username)?;
    ensure!(
        !hello.device_name.is_empty()
            && hello.device_name.len() <= 128
            && !hello.device_name.chars().any(char::is_control),
        "device name must be 1–128 bytes without control characters"
    );
    Ok(())
}

async fn write_packet(
    stream: &mut (impl tokio::io::AsyncWrite + Unpin),
    data: &[u8],
) -> Result<()> {
    ensure!(
        !data.is_empty() && data.len() <= MAX_PACKET,
        "invalid access message size"
    );
    stream.write_u16(data.len() as u16).await?;
    stream.write_all(data).await?;
    Ok(())
}
async fn read_packet(stream: &mut (impl tokio::io::AsyncRead + Unpin)) -> Result<Vec<u8>> {
    let length = stream.read_u16().await? as usize;
    ensure!(
        length > 0 && length <= MAX_PACKET,
        "invalid access message size"
    );
    let mut packet = vec![0; length];
    stream.read_exact(&mut packet).await?;
    Ok(packet)
}

/// How long a host waits for the device key a capable client announced.
#[cfg(target_os = "linux")]
const REGISTRATION_TIMEOUT: Duration = Duration::from_secs(10);

/// Client half of keyed enrollment (`crate::e2e::Registration`): a fresh
/// device key, offered before the host issues the credential. Device keys are
/// generated here, never accepted from the host.
pub(crate) struct DeviceKeyOffer {
    key: crate::e2e::DeviceKey,
    seed: zeroize::Zeroizing<String>,
}

impl DeviceKeyOffer {
    pub(crate) fn new() -> Result<Self> {
        let (key, seed) = crate::e2e::DeviceKey::generate()?;
        Ok(Self {
            key,
            seed: seed.into(),
        })
    }

    /// The sealed public key for the host.
    pub(crate) fn request(&self, registration: &crate::e2e::Registration) -> Vec<u8> {
        registration.request(&self.key)
    }

    /// The issued pairing, keyed only if the host bound exactly this key to
    /// it. Any failure is an enrollment failure: nothing is saved.
    pub(crate) fn accept(
        self,
        registration: &crate::e2e::Registration,
        packet: &[u8],
    ) -> Result<Pairing> {
        let bound = registration.open_credential(&self.key, packet)?;
        let pairing: Pairing = serde_json::from_slice(&bound.credential)?;
        pairing.validate()?;
        Ok(Pairing {
            device_key: bound.keyed.then(|| self.seed.to_string()),
            ..pairing
        })
    }
}

fn credential_key(session: &[u8]) -> Result<aead::LessSafeKey> {
    let prk = hkdf::Salt::new(hkdf::HKDF_SHA256, &MAGIC).extract(session);
    let mut key = [0; 32];
    prk.expand(
        &[b"teleport/password/device-credential/v1"],
        hkdf::HKDF_SHA256,
    )
    .map_err(|_| anyhow::anyhow!("key derivation failed"))?
    .fill(&mut key)
    .map_err(|_| anyhow::anyhow!("key derivation failed"))?;
    Ok(aead::LessSafeKey::new(
        aead::UnboundKey::new(&aead::CHACHA20_POLY1305, &key)
            .map_err(|_| anyhow::anyhow!("cipher initialization failed"))?,
    ))
}

fn open_credential(session: &[u8], context: &[u8], mut packet: Vec<u8>) -> Result<Pairing> {
    ensure!(packet.len() >= 28, "invalid credential response");
    let nonce: [u8; 12] = packet[..12].try_into()?;
    let plaintext = credential_key(session)?
        .open_in_place(
            aead::Nonce::assume_unique_for_key(nonce),
            aead::Aad::from(context),
            &mut packet[12..],
        )
        .map_err(|_| anyhow::anyhow!("credential authentication failed"))?;
    let pairing: Pairing = serde_json::from_slice(plaintext)?;
    pairing.validate()?;
    Ok(pairing)
}

/// Passwords are used only for this enrollment, not persisted by the client.
pub async fn login(
    address: &str,
    username: &str,
    password: &str,
    device_name: &str,
) -> Result<Pairing> {
    login_with_touch(
        address,
        username,
        password,
        device_name,
        Capabilities::CURRENT,
        |credential, hash| crate::security_key::sign_u2f(&credential, &hash, None),
    )
    .await
}

/// What a client announces in its [`Hello`].
#[derive(Clone, Copy)]
struct Capabilities {
    u2f: bool,
    device_key: bool,
}

impl Capabilities {
    const CURRENT: Self = Self {
        u2f: true,
        device_key: true,
    };
    /// A client from before U2F and device keys (tests only).
    #[cfg(test)]
    const OLD: Self = Self {
        u2f: false,
        device_key: false,
    };
}

async fn login_with_touch<F>(
    address: &str,
    username: &str,
    password: &str,
    device_name: &str,
    capabilities: Capabilities,
    touch: F,
) -> Result<Pairing>
where
    F: FnOnce(crate::security_key::Credential, [u8; 32]) -> Result<crate::security_key::Assertion>
        + Send
        + 'static,
{
    crate::pairing::validate_address(address)?;
    tokio::time::timeout(TIMEOUT, async {
        let stream = TcpStream::connect(address).await.context(
            "cannot reach password access service; check TCP firewall and host settings",
        )?;
        login_over(stream, username, password, device_name, capabilities, touch).await
    })
    .await
    .context("password login timed out")?
}

/// Password login over a relay stream (`crate::relay_enroll`); prompts for a
/// security-key touch when the host requires it.
pub(crate) async fn login_relayed(
    stream: impl tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
    username: &str,
    password: &str,
    device_name: &str,
) -> Result<Pairing> {
    tokio::time::timeout(
        TIMEOUT,
        login_over(
            stream,
            username,
            password,
            device_name,
            Capabilities::CURRENT,
            |credential, hash| crate::security_key::sign_u2f(&credential, &hash, None),
        ),
    )
    .await
    .context("password login timed out")?
}

/// Client side of OPAQUE login over any ordered byte stream. OPAQUE
/// authenticates the host's fingerprint, so the stream may be untrusted.
async fn login_over<F>(
    mut stream: impl tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
    username: &str,
    password: &str,
    device_name: &str,
    capabilities: Capabilities,
    touch: F,
) -> Result<Pairing>
where
    F: FnOnce(crate::security_key::Credential, [u8; 32]) -> Result<crate::security_key::Assertion>
        + Send
        + 'static,
{
    let hello = Hello {
        username: username.into(),
        device_name: device_name.into(),
        u2f: capabilities.u2f,
        device_key: capabilities.device_key,
    };
    validate_hello(&hello)?;
    ensure!(
        !password.is_empty() && password.len() <= 1024,
        "invalid password length"
    );
    stream.write_all(&MAGIC).await?;
    let hello_bytes = serde_json::to_vec(&hello)?;
    write_packet(&mut stream, &hello_bytes).await?;
    let fingerprint = String::from_utf8(read_packet(&mut stream).await?)?;
    moq_native::tls::parse_fingerprint(&fingerprint)?;
    let context = serde_json::to_vec(&(MAGIC, &hello_bytes, &fingerprint))?;
    let start = ClientLogin::<Suite>::start(&mut OsRng, password.as_bytes())?;
    write_packet(&mut stream, &start.message.serialize()).await?;
    let response = CredentialResponse::<Suite>::deserialize(&read_packet(&mut stream).await?)?;
    let finish = start
        .state
        .finish(
            &mut OsRng,
            password.as_bytes(),
            response,
            ClientLoginFinishParameters {
                ksf: Some(&password_ksf()),
                context: Some(&context),
                identifiers: Identifiers {
                    client: Some(username.as_bytes()),
                    server: Some(fingerprint.as_bytes()),
                },
            },
        )
        .map_err(|_| {
            anyhow::anyhow!("login failed; check username/password or host access settings")
        })?;
    write_packet(&mut stream, &finish.message.serialize()).await?;
    // The device key goes first: the host stores it in the same record as the
    // credential it then issues (see `crate::e2e::Registration`).
    let registration = crate::e2e::Registration::new(&finish.session_key, &context)?;
    let offer = match capabilities.device_key {
        true => Some(DeviceKeyOffer::new()?),
        false => None,
    };
    if let Some(offer) = &offer {
        write_packet(&mut stream, &offer.request(&registration)).await?;
    }
    let mut packet = read_packet(&mut stream).await?;
    if packet.starts_with(b"TPU2F001") {
        let challenge: TouchChallenge = serde_json::from_slice(&open_touch(
            &finish.session_key,
            TOUCH_REQUEST,
            &context,
            packet[8..].to_vec(),
        )?)?;
        crate::security_key::validate_u2f_credential(&challenge.credential, &fingerprint)?;
        eprintln!("Touch your YubiKey to authorize this new device.");
        let assertion =
            tokio::task::spawn_blocking(move || touch(challenge.credential, challenge.hash))
                .await??;
        write_packet(
            &mut stream,
            &seal_touch(
                &finish.session_key,
                TOUCH_REPLY,
                &context,
                serde_json::to_vec(&assertion)?,
            )?,
        )
        .await?;
        packet = read_packet(&mut stream).await?;
    }
    let pairing = match offer {
        Some(offer) => {
            let pairing = offer.accept(&registration, &packet).map_err(|error| {
                // A host from before device keys ignores the flag and seals
                // the credential as before. Only such a host can (the flag is
                // in the authenticated context), so this is not a downgrade
                // a relay can force: fail clearly, save nothing.
                if open_credential(&finish.session_key, &context, packet.clone()).is_ok() {
                    anyhow::anyhow!(
                        "the host does not support device keys; update Teleport on the host \
                         (and revoke the device it just listed)"
                    )
                } else {
                    error
                }
            })?;
            ensure!(
                pairing.device_key.is_some(),
                "host issued a password device without its device key"
            );
            pairing
        }
        None => Pairing {
            device_key: None,
            ..open_credential(&finish.session_key, &context, packet)?
        },
    };
    ensure!(
        pairing.fingerprint == fingerprint,
        "authenticated host identity mismatch"
    );
    Ok(pairing)
}

#[cfg(target_os = "linux")]
#[derive(Clone, Serialize, Deserialize, Debug)]
pub struct DeviceSummary {
    pub id: String,
    pub name: String,
    pub created_unix: u64,
    pub source: String,
    /// Set for a guest device (enrolled through an invite); owners' devices
    /// have none. Never set or cleared after enrollment except by
    /// [`ServerState::set_guest`], which only edits existing guests.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub guest: Option<GuestGrant>,
}

/// What a guest device was granted (docs/guests.md).
#[cfg(target_os = "linux")]
#[derive(Clone, Copy, Serialize, Deserialize, Debug, PartialEq, Eq)]
pub struct GuestGrant {
    pub permissions: crate::protocol::Permissions,
    /// Set by the owner: this guest joins without a live approval.
    #[serde(default)]
    pub trusted: bool,
}

/// Who an authorized credential belongs to, looked up live (permission
/// changes and revocation apply to running sessions).
#[cfg(target_os = "linux")]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionRole {
    pub role: crate::protocol::Role,
    /// The managed device id; `None` for the host pairing file and Linux
    /// login tickets (both owners).
    pub device: Option<String>,
    pub name: String,
    pub permissions: crate::protocol::Permissions,
    pub trusted: bool,
}

#[cfg(target_os = "linux")]
impl SessionRole {
    pub fn owner(name: &str, device: Option<String>) -> Self {
        Self {
            role: crate::protocol::Role::Owner,
            device,
            name: name.into(),
            permissions: crate::protocol::Permissions::ALL,
            trusted: true,
        }
    }

    pub fn is_owner(&self) -> bool {
        self.role == crate::protocol::Role::Owner
    }
}

/// An open invite, without its secret.
#[cfg(target_os = "linux")]
#[derive(Clone, Serialize, Deserialize, Debug)]
pub struct InviteSummary {
    pub id: String,
    pub label: String,
    pub permissions: crate::protocol::Permissions,
    pub uses_left: u32,
    pub expires_unix: u64,
}

/// A newly created invite: share [`NewInvite::link`] with the guest.
#[cfg(target_os = "linux")]
pub struct NewInvite {
    pub id: String,
    pub secret: String,
    pub expires_unix: u64,
}

#[cfg(target_os = "linux")]
impl NewInvite {
    pub fn link(&self) -> String {
        crate::pairing::invite_link(&self.id, &self.secret)
    }
}

/// Most open invites per host.
#[cfg(target_os = "linux")]
pub const MAX_INVITES: usize = 16;
/// Most uses one invite may grant.
#[cfg(target_os = "linux")]
pub const MAX_INVITE_USES: u32 = 50;
/// Longest invite lifetime (a week).
#[cfg(target_os = "linux")]
pub const MAX_INVITE_LIFETIME: Duration = Duration::from_secs(7 * 24 * 3600);

#[cfg(target_os = "linux")]
#[derive(Clone, Serialize, Deserialize, Debug)]
pub struct AccessStatus {
    pub username: Option<String>,
    pub devices: Vec<DeviceSummary>,
    pub u2f_required: bool,
}

#[cfg(target_os = "linux")]
mod server {
    use super::*;
    use opaque_ke::{
        ClientRegistration, ClientRegistrationFinishParameters, CredentialFinalization,
        CredentialRequest, ServerLogin, ServerLoginParameters, ServerRegistration, ServerSetup,
    };
    use ring::digest;
    use std::{
        collections::HashMap,
        fs,
        io::Write,
        net::IpAddr,
        os::unix::fs::{MetadataExt, OpenOptionsExt},
        path::{Path, PathBuf},
        sync::{Arc, Mutex},
        time::{SystemTime, UNIX_EPOCH},
    };
    use subtle::ConstantTimeEq;

    type DevicePublicKey = [u8; crate::e2e::DEVICE_PUBLIC_KEY_LEN];

    #[derive(Clone, Serialize, Deserialize)]
    struct Device {
        summary: DeviceSummary,
        token_hash: Vec<u8>,
        /// Ed25519 public half of the device's relay key, stored in the same
        /// update as `token_hash` (`crate::e2e::Registration`). Absent for
        /// devices enrolled before device keys or by clients without them;
        /// those stay hash-authenticated through a relay.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        public_key: Option<Vec<u8>>,
    }

    /// A credential a relayed hello may name (see `crate::e2e`).
    pub struct RelayCredential {
        /// `SHA-256(token)`.
        pub hash: [u8; 32],
        /// Required transcript-signing key, for devices that registered one.
        pub device_key: Option<Vec<u8>>,
        /// A memory-only, single-use system-login ticket.
        pub ticket: bool,
    }

    impl RelayCredential {
        /// Authenticated by data the host stores on disk alone: the host
        /// pairing token or a device without a registered key.
        pub fn hash_only(&self) -> bool {
            self.device_key.is_none() && !self.ticket
        }
    }
    #[derive(Clone, Serialize, Deserialize)]
    struct Account {
        username: String,
        registration: Vec<u8>,
    }
    #[derive(Clone, Serialize, Deserialize)]
    struct Record {
        version: u32,
        setup: Vec<u8>,
        account: Option<Account>,
        devices: Vec<Device>,
        generation: u64,
        attempts: Vec<u64>,
        #[serde(default)]
        u2f: Option<crate::security_key::Credential>,
        #[serde(default)]
        u2f_counter: u32,
        /// Open guest invites (G16). Uses are counted down in the same
        /// persisted update that stores the guest device, so an invite can
        /// never enroll more devices than it allows, even across restarts.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        invites: Vec<StoredInvite>,
    }
    #[derive(Clone, Serialize, Deserialize)]
    struct StoredInvite {
        id: String,
        /// The SPAKE2 password (128 random bits as hex). As secret as a
        /// device token; this file is private to the host user.
        secret: String,
        label: String,
        permissions: crate::protocol::Permissions,
        uses_left: u32,
        /// Devices enrolled so far (names them `label #n`).
        #[serde(default)]
        used: u32,
        expires_unix: u64,
    }
    impl StoredInvite {
        fn live(&self, time: u64) -> bool {
            self.uses_left > 0 && time < self.expires_unix
        }
    }
    fn valid_hex(text: &str, length: usize) -> bool {
        text.len() == length && text.bytes().all(|b| b.is_ascii_hexdigit())
    }
    /// Invite labels name guest devices in prompts and lists.
    fn valid_label(label: &str) -> bool {
        !label.trim().is_empty() && label.len() <= 64 && !label.chars().any(char::is_control)
    }
    pub struct ServerState {
        path: PathBuf,
        legacy: Pairing,
        record: Mutex<Record>,
        sources: Mutex<HashMap<IpAddr, Vec<u64>>>,
        slots: tokio::sync::Semaphore,
        system_sessions: Mutex<HashMap<Vec<u8>, SystemSession>>,
    }
    struct SystemSession {
        generation: u64,
        deadline: std::time::Instant,
        active: bool,
    }
    /// What a session presented beyond the credential hash.
    enum KeyProof {
        /// The bearer token itself (direct connections).
        Token,
        /// A relayed hello, with the device key whose signature was verified.
        Relay(Option<Vec<u8>>),
    }
    impl KeyProof {
        /// Whether a device record with `stored` key accepts this proof. A
        /// relayed session needs exactly the stored key (none for hash-only
        /// devices), so a key registered or changed later never vouches for
        /// a session proven without it.
        fn accepts(&self, stored: Option<&[u8]>) -> bool {
            match (self, stored) {
                (KeyProof::Token, _) => true,
                (KeyProof::Relay(proven), stored) => proven.as_deref() == stored,
            }
        }
    }
    pub struct SessionLease {
        state: Arc<ServerState>,
        hash: Vec<u8>,
        proof: KeyProof,
        /// Leased from a single-use system-login ticket.
        ticket: bool,
    }
    impl SessionLease {
        /// Whether the credential is the desktop owner's, for owner-only
        /// requests (`crate::host_command`). Every credential this host
        /// admits is the owner's today: paired devices, the host pairing
        /// token, and system-login tickets (the account password). Guest
        /// roles would narrow this.
        pub fn owner(&self) -> bool {
            true
        }

        /// The session came from a system login (PAM just verified the
        /// account password).
        pub fn system_login(&self) -> bool {
            self.ticket
        }

        /// Whether the session may continue: its credential is still
        /// accepted with the key proof it was leased with. Cheap enough to
        /// poll (one lock, at most 128 records).
        #[cfg(test)]
        pub fn authorized(&self) -> bool {
            self.state.authorized_with(&self.hash, &self.proof)
        }

        /// The session's current role and permissions, or `None` once it is
        /// no longer authorized. Read live from the access record, so an
        /// owner's permission change applies to running sessions.
        pub fn access(&self) -> Option<SessionRole> {
            self.state.role_with(&self.hash, &self.proof)
        }
    }
    impl Drop for SessionLease {
        fn drop(&mut self) {
            if let Ok(mut sessions) = self.state.system_sessions.lock() {
                sessions.remove(&self.hash);
            }
        }
    }
    fn now() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs()
    }
    fn token_hash(token: &str) -> Vec<u8> {
        digest::digest(&digest::SHA256, token.as_bytes())
            .as_ref()
            .to_vec()
    }
    /// A managed device with this credential hash that accepts `proof`
    /// (constant time over the hashes).
    fn saved_device(record: &Record, hash: &[u8], proof: &KeyProof) -> bool {
        matching_device(record, hash, proof).is_some()
    }
    /// The managed device with this credential hash that accepts `proof`;
    /// every record is compared (constant time over the hashes).
    fn matching_device<'a>(
        record: &'a Record,
        hash: &[u8],
        proof: &KeyProof,
    ) -> Option<&'a Device> {
        record.devices.iter().fold(None, |found, d| {
            let matches =
                bool::from(d.token_hash.ct_eq(hash)) & proof.accepts(d.public_key.as_deref());
            if matches { Some(d) } else { found }
        })
    }

    impl ServerState {
        pub fn open(identity_dir: &Path, legacy_pairing: Pairing) -> Result<Arc<Self>> {
            legacy_pairing.validate()?;
            let metadata = fs::symlink_metadata(identity_dir)?;
            ensure!(
                metadata.is_dir()
                    && metadata.uid() == unsafe { libc::geteuid() }
                    && metadata.mode() & 0o077 == 0,
                "access identity directory must be owned by this user, private, and not a symlink"
            );
            let path = identity_dir.join("access.json");
            let record = if path.try_exists()? {
                crate::identity::private_file(&path)?;
                ensure!(
                    fs::metadata(&path)?.len() <= 256 * 1024,
                    "access database too large"
                );
                let record: Record = serde_json::from_slice(&fs::read(&path)?)?;
                ensure!(
                    record.version == 1
                        && record.devices.len() <= 128
                        && record.attempts.len() <= 10,
                    "unsupported or invalid access database"
                );
                ServerSetup::<Suite>::deserialize(&record.setup)?;
                if let Some(account) = &record.account {
                    validate_username(&account.username)?;
                    ServerRegistration::<Suite>::deserialize(&account.registration)?;
                }
                for device in &record.devices {
                    ensure!(
                        device.token_hash.len() == 32
                            && device.public_key.as_ref().is_none_or(|key| {
                                key.len() == crate::e2e::DEVICE_PUBLIC_KEY_LEN
                            }),
                        "invalid stored device credential"
                    );
                }
                if let Some(key) = &record.u2f {
                    crate::security_key::validate_u2f_credential(key, &legacy_pairing.fingerprint)?;
                }
                ensure!(
                    record.invites.len() <= MAX_INVITES
                        && record.invites.iter().all(|invite| {
                            valid_hex(&invite.id, 32)
                                && valid_hex(&invite.secret, 32)
                                && invite.uses_left <= MAX_INVITE_USES
                                && valid_label(&invite.label)
                        }),
                    "invalid stored invite"
                );
                record
            } else {
                let record = Record {
                    version: 1,
                    setup: ServerSetup::<Suite>::new(&mut OsRng).serialize().to_vec(),
                    account: None,
                    devices: Vec::new(),
                    generation: 0,
                    attempts: Vec::new(),
                    u2f: None,
                    u2f_counter: 0,
                    invites: Vec::new(),
                };
                crate::identity::write_new(&path, &serde_json::to_vec(&record)?)?;
                record
            };
            Ok(Arc::new(Self {
                path,
                legacy: legacy_pairing,
                record: Mutex::new(record),
                sources: Mutex::new(HashMap::new()),
                slots: tokio::sync::Semaphore::new(4),
                system_sessions: Mutex::new(HashMap::new()),
            }))
        }

        fn persist(&self, record: &Record) -> Result<()> {
            crate::identity::private_file(&self.path)?;
            let parent = self.path.parent().context("missing access directory")?;
            let temporary = parent.join(format!(".access-{}.tmp", crate::identity::token()));
            let result = (|| {
                let mut file = fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .mode(0o600)
                    .open(&temporary)?;
                file.write_all(&serde_json::to_vec(record)?)?;
                file.sync_all()?;
                fs::rename(&temporary, &self.path)?;
                fs::File::open(parent)?.sync_all()?;
                Ok(())
            })();
            if result.is_err() {
                let _ = fs::remove_file(&temporary);
            }
            result
        }

        fn update(&self, change: impl FnOnce(&mut Record) -> Result<()>) -> Result<()> {
            let mut record = self
                .record
                .lock()
                .map_err(|_| anyhow::anyhow!("access state unavailable"))?;
            let mut next = record.clone();
            change(&mut next)?;
            self.persist(&next)?;
            *record = next;
            Ok(())
        }

        pub fn status(&self) -> AccessStatus {
            let record = self.record.lock().expect("access state lock");
            AccessStatus {
                username: record.account.as_ref().map(|a| a.username.clone()),
                devices: record.devices.iter().map(|d| d.summary.clone()).collect(),
                u2f_required: record.u2f.is_some(),
            }
        }

        /// Same-user local administration only. Existing trusted devices remain valid.
        pub fn require_u2f(&self, credential: crate::security_key::Credential) -> Result<()> {
            crate::security_key::validate_u2f_credential(&credential, &self.legacy.fingerprint)?;
            self.update(|r| {
                ensure!(r.account.is_some(), "configure a Teleport password first");
                r.u2f = Some(credential);
                r.u2f_counter = 0;
                r.generation = r
                    .generation
                    .checked_add(1)
                    .context("access generation exhausted")?;
                Ok(())
            })
        }
        pub fn disable_u2f(&self) -> Result<()> {
            self.update(|r| {
                r.u2f = None;
                r.u2f_counter = 0;
                r.generation = r
                    .generation
                    .checked_add(1)
                    .context("access generation exhausted")?;
                Ok(())
            })
        }

        fn accept_touch(
            &self,
            generation: u64,
            hash: &[u8; 32],
            assertion: &crate::security_key::Assertion,
        ) -> Result<()> {
            self.update(|r| {
                ensure!(
                    r.generation == generation,
                    "access settings changed; log in again"
                );
                let credential = r.u2f.as_ref().context("security-key settings changed")?;
                let counter = crate::security_key::verify_u2f(credential, hash, assertion)?;
                ensure!(
                    counter > r.u2f_counter,
                    "security-key counter did not advance; enrollment rejected"
                );
                r.u2f_counter = counter;
                Ok(())
            })
        }

        pub fn set_password(&self, username: &str, password: &str) -> Result<()> {
            validate_username(username)?;
            validate_password(password)?;
            self.update(|record| {
                let setup = ServerSetup::<Suite>::deserialize(&record.setup)?;
                let start = ClientRegistration::<Suite>::start(&mut OsRng, password.as_bytes())?;
                let response =
                    ServerRegistration::<Suite>::start(&setup, start.message, username.as_bytes())?;
                let finish = start.state.finish(
                    &mut OsRng,
                    password.as_bytes(),
                    response.message,
                    ClientRegistrationFinishParameters {
                        ksf: Some(&password_ksf()),
                        identifiers: Identifiers {
                            client: Some(username.as_bytes()),
                            server: Some(self.legacy.fingerprint.as_bytes()),
                        },
                    },
                )?;
                record.account = Some(Account {
                    username: username.into(),
                    registration: ServerRegistration::<Suite>::finish(finish.message)
                        .serialize()
                        .to_vec(),
                });
                record.devices.retain(|d| d.summary.source != "password");
                record.generation = record
                    .generation
                    .checked_add(1)
                    .context("access generation exhausted")?;
                Ok(())
            })
        }
        /// Disables new password logins and revokes all password-issued devices.
        pub fn disable_password(&self) -> Result<()> {
            self.update(|r| {
                r.account = None;
                r.devices.retain(|d| d.summary.source != "password");
                r.generation = r
                    .generation
                    .checked_add(1)
                    .context("access generation exhausted")?;
                Ok(())
            })
        }
        pub fn revoke_device(&self, id: &str) -> Result<()> {
            self.update(|r| {
                ensure!(
                    r.devices.iter().any(|d| d.summary.id == id),
                    "device not found"
                );
                r.devices.retain(|d| d.summary.id != id);
                Ok(())
            })
        }
        /// The panic button: every managed device, guests included, and
        /// every open invite (a leaked link must not enroll anyone after it).
        pub fn revoke_all_devices(&self) -> Result<()> {
            self.update(|r| {
                r.devices.clear();
                r.invites.clear();
                r.generation = r
                    .generation
                    .checked_add(1)
                    .context("access generation exhausted")?;
                Ok(())
            })
        }
        pub fn authorized(&self, token: &str) -> bool {
            token.len() == 64
                && token.bytes().all(|b| b.is_ascii_hexdigit())
                && self.authorized_hash(&token_hash(token))
        }

        /// Every currently acceptable credential as `SHA-256(token)`, with
        /// its device key if it has one: the legacy host token, managed
        /// devices and live system-login tickets. A relayed hello names one
        /// of these without revealing the token.
        pub fn relay_credentials(&self) -> Vec<RelayCredential> {
            let mut credentials = Vec::new();
            let mut push = |hash: &[u8], device_key: Option<&Vec<u8>>, ticket| {
                if let Ok(hash) = hash.try_into() {
                    credentials.push(RelayCredential {
                        hash,
                        device_key: device_key.cloned(),
                        ticket,
                    });
                }
            };
            push(&token_hash(&self.legacy.token), None, false);
            if let Ok(record) = self.record.lock() {
                for device in &record.devices {
                    push(&device.token_hash, device.public_key.as_ref(), false);
                }
            }
            if self.system_login_allowed()
                && let Ok(sessions) = self.system_sessions.lock()
            {
                for hash in sessions.keys() {
                    push(hash, None, true);
                }
            }
            credentials
        }

        /// Send an issued device its credential packet. If that fails, the
        /// device is removed again: nobody holds its credential.
        pub(crate) async fn deliver(
            &self,
            stream: &mut (impl tokio::io::AsyncWrite + Unpin),
            issued: &IssuedDevice,
            packet: &[u8],
        ) -> Result<()> {
            let result = async {
                write_packet(stream, packet).await?;
                stream.flush().await?;
                Ok::<_, anyhow::Error>(())
            }
            .await;
            if result.is_err()
                && let Err(error) = self.revoke_device(&issued.id)
            {
                tracing::warn!(%error, "could not remove an undelivered device");
            }
            result
        }

        /// Authorization by `SHA-256(token)` alone, as if the token was
        /// presented. Relayed sessions use [`SessionLease::authorized`],
        /// which also checks the device key.
        pub fn authorized_hash(&self, hash: &[u8]) -> bool {
            self.authorized_with(hash, &KeyProof::Token)
        }

        fn authorized_with(&self, hash: &[u8], proof: &KeyProof) -> bool {
            self.role_with(hash, proof).is_some()
        }

        /// Who `hash` (with `proof`) is, if it is authorized at all: the
        /// host pairing file and Linux login tickets are owners; a managed
        /// device is an owner unless it was enrolled through an invite.
        fn role_with(&self, hash: &[u8], proof: &KeyProof) -> Option<SessionRole> {
            if bool::from(token_hash(&self.legacy.token).ct_eq(hash)) {
                return Some(SessionRole::owner("Host pairing file", None));
            }
            if self.system_login_allowed()
                && let Ok(generation) = self.system_generation()
                && let Ok(sessions) = self.system_sessions.lock()
                && sessions.get(hash).is_some_and(|s| {
                    s.generation == generation
                        && (s.active || s.deadline > std::time::Instant::now())
                })
            {
                return Some(SessionRole::owner("Linux account login", None));
            }
            let record = self.record.lock().ok()?;
            let device = matching_device(&record, hash, proof)?;
            Some(match device.summary.guest {
                None => SessionRole::owner(&device.summary.name, Some(device.summary.id.clone())),
                Some(grant) => SessionRole {
                    role: crate::protocol::Role::Guest,
                    device: Some(device.summary.id.clone()),
                    name: device.summary.name.clone(),
                    permissions: grant.permissions,
                    trusted: grant.trusted,
                },
            })
        }

        /// A new guest invite (G16): `uses` enrollments until `lifetime`
        /// passes, each a guest device with `permissions`. Local trusted
        /// callers only (the admin socket).
        pub fn create_invite(
            &self,
            label: &str,
            uses: u32,
            lifetime: Duration,
            permissions: crate::protocol::Permissions,
        ) -> Result<NewInvite> {
            let label = label.trim();
            ensure!(
                valid_label(label),
                "invite name: 1-64 characters, no control characters"
            );
            ensure!(
                (1..=MAX_INVITE_USES).contains(&uses),
                "an invite allows 1-{MAX_INVITE_USES} uses"
            );
            ensure!(
                lifetime >= Duration::from_secs(5) && lifetime <= MAX_INVITE_LIFETIME,
                "an invite lasts from 5 seconds to a week"
            );
            let invite = NewInvite {
                id: crate::relay::hex(&crate::e2e::random::<16>()),
                secret: crate::relay::hex(&crate::e2e::random::<16>()),
                expires_unix: now().saturating_add(lifetime.as_secs()),
            };
            self.update(|r| {
                let time = now();
                r.invites.retain(|stored| stored.live(time));
                ensure!(
                    r.invites.len() < MAX_INVITES,
                    "too many open invites; revoke one first"
                );
                r.invites.push(StoredInvite {
                    id: invite.id.clone(),
                    secret: invite.secret.clone(),
                    label: label.into(),
                    permissions,
                    uses_left: uses,
                    used: 0,
                    expires_unix: invite.expires_unix,
                });
                Ok(())
            })?;
            Ok(invite)
        }

        /// Open invites (live ones only), without secrets.
        pub fn invites(&self) -> Vec<InviteSummary> {
            let time = now();
            self.record
                .lock()
                .map(|r| {
                    r.invites
                        .iter()
                        .filter(|invite| invite.live(time))
                        .map(|invite| InviteSummary {
                            id: invite.id.clone(),
                            label: invite.label.clone(),
                            permissions: invite.permissions,
                            uses_left: invite.uses_left,
                            expires_unix: invite.expires_unix,
                        })
                        .collect()
                })
                .unwrap_or_default()
        }

        /// Close an invite. Devices it already enrolled stay (revoke them
        /// separately).
        pub fn revoke_invite(&self, id: &str) -> Result<()> {
            self.update(|r| {
                let before = r.invites.len();
                r.invites.retain(|invite| invite.id != id);
                ensure!(r.invites.len() < before, "invite not found");
                Ok(())
            })
        }

        /// The SPAKE2 password of a live invite, for one enrollment attempt.
        pub(crate) fn invite_secret(&self, id: &str) -> Result<zeroize::Zeroizing<String>> {
            let record = self
                .record
                .lock()
                .map_err(|_| anyhow::anyhow!("access unavailable"))?;
            let time = now();
            record
                .invites
                .iter()
                .find(|invite| invite.id == id && invite.live(time))
                .map(|invite| zeroize::Zeroizing::new(invite.secret.clone()))
                .context("invite not found, used up or expired")
        }

        /// Local trusted caller only: invoke after the invite's SPAKE2 proof
        /// with `secret` succeeded. Spends one use of the invite and stores
        /// the guest device (with its key) in one persisted update, or does
        /// neither.
        pub fn enroll_guest(
            &self,
            invite: &str,
            secret: &str,
            public_key: &DevicePublicKey,
        ) -> Result<IssuedDevice> {
            self.add_device_with("invite", Some(public_key), |r| {
                let time = now();
                let stored = r
                    .invites
                    .iter_mut()
                    .find(|stored| {
                        stored.id == invite
                            && bool::from(stored.secret.as_bytes().ct_eq(secret.as_bytes()))
                    })
                    .context("invite revoked")?;
                ensure!(stored.live(time), "invite used up or expired");
                stored.uses_left -= 1;
                stored.used = stored.used.saturating_add(1);
                let name = if stored.used == 1 && stored.uses_left == 0 {
                    stored.label.clone()
                } else {
                    format!("{} #{}", stored.label, stored.used)
                };
                let grant = GuestGrant {
                    permissions: stored.permissions,
                    trusted: false,
                };
                r.invites.retain(|stored| stored.live(time));
                Ok((name, Some(grant)))
            })
        }

        /// Change a guest device's permissions and/or trusted flag. Owners'
        /// devices cannot be changed (or turned into guests) here.
        pub fn set_guest(
            &self,
            id: &str,
            permissions: Option<crate::protocol::Permissions>,
            trusted: Option<bool>,
        ) -> Result<()> {
            self.update(|r| {
                let device = r
                    .devices
                    .iter_mut()
                    .find(|d| d.summary.id == id)
                    .context("device not found")?;
                let grant = device
                    .summary
                    .guest
                    .as_mut()
                    .context("not a guest device; owners' devices always have full control")?;
                if let Some(permissions) = permissions {
                    grant.permissions = permissions;
                }
                if let Some(trusted) = trusted {
                    grant.trusted = trusted;
                }
                Ok(())
            })
        }

        pub fn system_login_allowed(&self) -> bool {
            // Never introduce a password-only alternative around configured
            // password+U2F enrollment. PAM-specific MFA needs its own UI.
            self.record.lock().is_ok_and(|r| r.u2f.is_none())
        }

        pub fn system_generation(&self) -> Result<u64> {
            Ok(self
                .record
                .lock()
                .map_err(|_| anyhow::anyhow!("access unavailable"))?
                .generation)
        }

        pub fn system_admit(&self, ip: IpAddr) -> Result<()> {
            self.admit(ip)
        }

        pub fn issue_system_session(&self, generation: u64) -> Result<Pairing> {
            let record = self
                .record
                .lock()
                .map_err(|_| anyhow::anyhow!("access unavailable"))?;
            ensure!(
                record.generation == generation && record.u2f.is_none(),
                "login failed"
            );
            let mut sessions = self
                .system_sessions
                .lock()
                .map_err(|_| anyhow::anyhow!("access unavailable"))?;
            sessions.retain(|_, s| {
                s.generation == generation && (s.active || s.deadline > std::time::Instant::now())
            });
            ensure!(sessions.len() < 16, "login temporarily unavailable");
            let token = crate::identity::token();
            sessions.insert(
                token_hash(&token),
                SystemSession {
                    generation,
                    deadline: std::time::Instant::now() + Duration::from_secs(60),
                    active: false,
                },
            );
            Ok(Pairing {
                token,
                fingerprint: self.legacy.fingerprint.clone(),
                device_key: None,
            })
        }

        /// System tickets are claimed exactly once; dropping the lease destroys
        /// them on disconnect, cancellation or failed stream setup. Saved trust
        /// remains reusable and its lease removal is a no-op.
        pub fn begin_session(self: &Arc<Self>, token: &str) -> Result<SessionLease> {
            ensure!(
                token.len() == 64 && token.bytes().all(|b| b.is_ascii_hexdigit()),
                "unauthorized session"
            );
            // The token itself was presented, which proves more than the hash.
            self.claim(token_hash(token), KeyProof::Token)
        }

        /// [`Self::begin_session`] by `SHA-256(token)` (relayed sessions).
        /// `proven_key` is the device key whose transcript signature the
        /// client proved; a device with a registered key is leased only with
        /// that key, so the stored hash alone never suffices for it.
        pub fn begin_session_hash(
            self: &Arc<Self>,
            hash: Vec<u8>,
            proven_key: Option<&[u8]>,
        ) -> Result<SessionLease> {
            self.claim(hash, KeyProof::Relay(proven_key.map(<[u8]>::to_vec)))
        }

        fn claim(self: &Arc<Self>, hash: Vec<u8>, proof: KeyProof) -> Result<SessionLease> {
            // Atomic authorization and claim; absence from the ticket map must
            // never turn a concurrently revoked ticket into reusable trust.
            let record = self
                .record
                .lock()
                .map_err(|_| anyhow::anyhow!("access unavailable"))?;
            let saved = bool::from(token_hash(&self.legacy.token).ct_eq(&hash))
                | saved_device(&record, &hash, &proof);
            let mut sessions = self
                .system_sessions
                .lock()
                .map_err(|_| anyhow::anyhow!("access unavailable"))?;
            if !saved {
                let session = sessions.get_mut(&hash).context("unauthorized session")?;
                ensure!(
                    record.u2f.is_none()
                        && session.generation == record.generation
                        && !session.active
                        && session.deadline > std::time::Instant::now(),
                    "system login ticket already used or expired"
                );
                session.active = true;
            }
            Ok(SessionLease {
                state: self.clone(),
                hash,
                proof,
                ticket: !saved,
            })
        }

        fn admit(&self, ip: IpAddr) -> Result<()> {
            let time = now();
            // Bound source bookkeeping. Global persistent limit is authoritative;
            // restarting the host cannot reset a brute-force guessing window.
            let mut sources = self
                .sources
                .lock()
                .map_err(|_| anyhow::anyhow!("access state unavailable"))?;
            sources.retain(|_, times| {
                times.retain(|t| time.saturating_sub(*t) < 60);
                !times.is_empty()
            });
            ensure!(
                sources.len() < 256 || sources.contains_key(&ip),
                "login temporarily unavailable"
            );
            let times = sources.entry(ip).or_default();
            ensure!(times.len() < 5, "login temporarily unavailable");
            self.update(|r| {
                r.attempts.retain(|t| time.saturating_sub(*t) < 60);
                ensure!(r.attempts.len() < 10, "login temporarily unavailable");
                r.attempts.push(time);
                Ok(())
            })?;
            times.push(time);
            Ok(())
        }

        fn issue(
            &self,
            generation: u64,
            name: &str,
            public_key: Option<&DevicePublicKey>,
        ) -> Result<IssuedDevice> {
            self.add_device(name, "password", public_key, |r| {
                ensure!(
                    r.generation == generation && r.account.is_some(),
                    "login failed"
                );
                Ok(())
            })
        }

        /// Local trusted caller only: invoke after one-time code proof succeeds.
        pub fn enroll_device(
            &self,
            name: &str,
            public_key: Option<&DevicePublicKey>,
        ) -> Result<IssuedDevice> {
            ensure!(
                !name.is_empty() && name.len() <= 128 && !name.chars().any(char::is_control),
                "invalid device name"
            );
            self.add_device(name, "pairing", public_key, |_| Ok(()))
        }

        /// A new device record. Its relay key, if any, is written in the same
        /// update as its token hash: a keyed device is never hash-only.
        fn add_device(
            &self,
            name: &str,
            source: &str,
            public_key: Option<&DevicePublicKey>,
            check: impl FnOnce(&Record) -> Result<()>,
        ) -> Result<IssuedDevice> {
            self.add_device_with(source, public_key, |r| {
                check(r)?;
                Ok((name.to_owned(), None))
            })
        }

        /// [`Self::add_device`] where `prepare` may change the record in the
        /// same persisted update (an invite's use) and names the device and
        /// its guest grant.
        fn add_device_with(
            &self,
            source: &str,
            public_key: Option<&DevicePublicKey>,
            prepare: impl FnOnce(&mut Record) -> Result<(String, Option<GuestGrant>)>,
        ) -> Result<IssuedDevice> {
            let token = crate::identity::token();
            let id = crate::identity::token();
            self.update(|r| {
                ensure!(
                    r.devices.len() < 128,
                    "device limit reached; remove an old device on the host"
                );
                let (name, guest) = prepare(r)?;
                r.devices.push(Device {
                    summary: DeviceSummary {
                        id: id.clone(),
                        name,
                        created_unix: now(),
                        source: source.into(),
                        guest,
                    },
                    token_hash: token_hash(&token),
                    public_key: public_key.map(|key| key.to_vec()),
                });
                Ok(())
            })?;
            Ok(IssuedDevice {
                pairing: Pairing {
                    token,
                    fingerprint: self.legacy.fingerprint.clone(),
                    device_key: None,
                },
                id,
            })
        }
    }

    /// Host: the device key a capable client sends before anything is issued
    /// (`crate::e2e::Registration`). Missing, late or forged fails enrollment.
    pub(crate) async fn read_device_key(
        stream: &mut (impl tokio::io::AsyncRead + Unpin),
        registration: &crate::e2e::Registration,
    ) -> Result<DevicePublicKey> {
        let packet = tokio::time::timeout(REGISTRATION_TIMEOUT, read_packet(stream))
            .await
            .context("device key timed out")??;
        registration.open_request(&packet)
    }

    /// A newly issued device credential.
    pub struct IssuedDevice {
        /// What the client receives (the host keeps only its hash).
        pub pairing: Pairing,
        pub id: String,
    }

    /// Host side of password login. `source` keys the per-source rate limit
    /// (all relayed attempts share one); the persistent global limit always applies.
    pub async fn handle(
        mut stream: impl tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
        source: IpAddr,
        state: Arc<ServerState>,
    ) -> Result<()> {
        let _permit = state
            .slots
            .try_acquire()
            .map_err(|_| anyhow::anyhow!("login temporarily unavailable"))?;
        state.admit(source)?;
        tokio::time::timeout(TIMEOUT, async {
            let (finish, context, hello, generation, u2f) =
                tokio::time::timeout(Duration::from_secs(15), async {
                    let mut magic = [0; 8];
                    stream.read_exact(&mut magic).await?;
                    ensure!(magic == MAGIC, "unsupported access protocol");
                    let hello_bytes = read_packet(&mut stream).await?;
                    let hello: Hello = serde_json::from_slice(&hello_bytes)?;
                    validate_hello(&hello)?;
                    let fingerprint = &state.legacy.fingerprint;
                    write_packet(&mut stream, fingerprint.as_bytes()).await?;
                    let context = serde_json::to_vec(&(MAGIC, &hello_bytes, fingerprint))?;
                    let request =
                        CredentialRequest::<Suite>::deserialize(&read_packet(&mut stream).await?)?;
                    let (setup, password_file, generation, u2f) = {
                        let record = state
                            .record
                            .lock()
                            .map_err(|_| anyhow::anyhow!("access state unavailable"))?;
                        let file = record
                            .account
                            .as_ref()
                            .filter(|a| a.username == hello.username)
                            .map(|a| ServerRegistration::<Suite>::deserialize(&a.registration))
                            .transpose()?;
                        (
                            ServerSetup::<Suite>::deserialize(&record.setup)?,
                            file,
                            record.generation,
                            record.u2f.clone(),
                        )
                    };
                    let parameters = ServerLoginParameters {
                        context: Some(&context),
                        identifiers: Identifiers {
                            client: Some(hello.username.as_bytes()),
                            server: Some(fingerprint.as_bytes()),
                        },
                    };
                    let start = ServerLogin::<Suite>::start(
                        &mut OsRng,
                        &setup,
                        password_file,
                        request,
                        hello.username.as_bytes(),
                        parameters.clone(),
                    )?;
                    write_packet(&mut stream, &start.message.serialize()).await?;
                    let finish = start
                        .state
                        .finish(
                            CredentialFinalization::<Suite>::deserialize(
                                &read_packet(&mut stream).await?,
                            )?,
                            parameters,
                        )
                        .map_err(|_| anyhow::anyhow!("login failed"))?;
                    Ok::<_, anyhow::Error>((finish, context, hello, generation, u2f))
                })
                .await
                .context("password proof timed out")??;
            // A capable client sends its device key right after the proof;
            // its credential will not exist without it.
            let registration = crate::e2e::Registration::new(&finish.session_key, &context)?;
            let device_key = match hello.device_key {
                true => Some(read_device_key(&mut stream, &registration).await?),
                false => None,
            };
            if let Some(credential) = u2f {
                ensure!(
                    hello.u2f,
                    "host requires password plus U2F; update your client"
                );
                // Fresh challenge binds this OPAQUE exchange, account and pinned host.
                let nonce: [u8; 32] = rand::random();
                let mut binding = b"teleport/u2f/login/v1".to_vec();
                binding.extend_from_slice(&context);
                binding.extend_from_slice(&finish.session_key);
                binding.extend_from_slice(&nonce);
                let hash: [u8; 32] = digest::digest(&digest::SHA256, &binding)
                    .as_ref()
                    .try_into()?;
                let challenge = TouchChallenge { credential, hash };
                let mut packet = b"TPU2F001".to_vec();
                packet.extend(seal_touch(
                    &finish.session_key,
                    TOUCH_REQUEST,
                    &context,
                    serde_json::to_vec(&challenge)?,
                )?);
                write_packet(&mut stream, &packet).await?;
                let assertion = serde_json::from_slice(&open_touch(
                    &finish.session_key,
                    TOUCH_REPLY,
                    &context,
                    read_packet(&mut stream).await?,
                )?)?;
                state.accept_touch(generation, &hash, &assertion)?;
            }
            let issued = state.issue(generation, &hello.device_name, device_key.as_ref())?;
            let mut payload = serde_json::to_vec(&issued.pairing)?;
            let packet = match &device_key {
                Some(public) => registration.seal_credential(Some(public), &payload),
                None => {
                    let nonce: [u8; 12] = rand::random();
                    credential_key(&finish.session_key)?
                        .seal_in_place_append_tag(
                            aead::Nonce::assume_unique_for_key(nonce),
                            aead::Aad::from(&context),
                            &mut payload,
                        )
                        .map_err(|_| anyhow::anyhow!("credential encryption failed"))?;
                    [&nonce[..], &payload].concat()
                }
            };
            state.deliver(&mut stream, &issued, &packet).await
        })
        .await
        .context("login timed out")?
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        const PASSWORD: &str = "Long test passphrase, not production!";

        fn state() -> Result<(tempfile::TempDir, Arc<ServerState>)> {
            let temporary = tempfile::tempdir()?;
            let identity = temporary.path().join("host");
            crate::identity::open(&identity)?;
            let state = ServerState::open(
                &identity,
                Pairing {
                    token: "a".repeat(64),
                    fingerprint: "b".repeat(64),
                    device_key: None,
                },
            )?;
            state.set_password("ej", PASSWORD)?;
            Ok((temporary, state))
        }

        async fn listener(
            state: Arc<ServerState>,
        ) -> Result<(String, tokio::task::JoinHandle<()>)> {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
            let address = listener.local_addr()?.to_string();
            let task = tokio::spawn(async move {
                while let Ok((stream, _)) = listener.accept().await {
                    let state = state.clone();
                    tokio::spawn(async move {
                        let source = stream.peer_addr().unwrap().ip();
                        if let Err(error) = handle(stream, source, state).await {
                            eprintln!("test access rejection: {error:#}");
                        }
                    });
                }
            });
            Ok((address, task))
        }

        #[tokio::test]
        #[ignore = "requires local TCP sockets; run outside the Nix build sandbox"]
        async fn password_login_rejection_device_revoke_and_persistence() -> Result<()> {
            let (_temporary, state) = state()?;
            let (address, task) = listener(state.clone()).await?;
            assert!(
                login(&address, "ej", "incorrect password", "work")
                    .await
                    .is_err()
            );
            assert!(login(&address, "unknown", PASSWORD, "work").await.is_err());
            let first = login(&address, "ej", PASSWORD, "work").await?;
            let second = login(&address, "ej", PASSWORD, "other").await?;
            assert_ne!(first.token, second.token);
            // Each login registered its own device key; the host stored only
            // the public halves.
            let seed = first.device_key.clone().expect("device key registered");
            assert_ne!(second.device_key, first.device_key);
            let public = crate::e2e::DeviceKey::from_hex(&seed)?.public_key();
            assert!(
                state
                    .relay_credentials()
                    .iter()
                    .any(|c| c.device_key.as_deref() == Some(&public[..]))
            );
            assert!(!fs::read_to_string(&state.path)?.contains(&seed));
            assert!(state.authorized(&first.token));
            assert!(state.authorized(&second.token));
            assert!(!state.authorized(&"f".repeat(64)));
            let reopened = ServerState::open(state.path.parent().unwrap(), state.legacy.clone())?;
            assert!(reopened.authorized(&first.token));
            state.revoke_device(&state.status().devices[0].id)?;
            assert!(!state.authorized(&first.token));
            assert!(state.authorized(&second.token));
            assert!(state.authorized(&state.legacy.token));
            state.set_password("ej", "Replacement test passphrase!")?;
            assert!(!state.authorized(&second.token));
            assert!(login(&address, "ej", PASSWORD, "work").await.is_err());
            state.disable_password()?;
            assert!(state.status().username.is_none());
            assert!(state.authorized(&state.legacy.token));
            task.abort();
            let disk = fs::read_to_string(&state.path)?;
            assert!(!disk.contains(PASSWORD));
            assert!(!disk.contains(&first.token));
            Ok(())
        }

        #[tokio::test]
        #[ignore = "requires local TCP sockets and libfido2; run outside the Nix build sandbox"]
        async fn u2f_password_enrollment_fails_closed_and_preserves_saved_devices() -> Result<()> {
            use ring::signature::{ECDSA_P256_SHA256_ASN1_SIGNING, EcdsaKeyPair, KeyPair};
            let (_temporary, state) = state()?;
            let rng = ring::rand::SystemRandom::new();
            let pkcs8 =
                EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_ASN1_SIGNING, &rng).unwrap();
            let key = Arc::new(
                EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_ASN1_SIGNING, pkcs8.as_ref(), &rng)
                    .unwrap(),
            );
            let fp = &state.legacy.fingerprint;
            let rp = format!("f-{}.{}.teleport.invalid", &fp[..32], &fp[32..]);
            let credential: crate::security_key::Credential =
                serde_json::from_value(serde_json::json!({
                    "version": 2, "fingerprint": fp, "rp": rp, "id": [1, 2, 3],
                    "public_key": key.public_key().as_ref()[1..], "algorithm": -7,
                    "uv_required": false, "mode": "U2f"
                }))?;
            let generation = state.record.lock().unwrap().generation;
            let saved = state.issue(generation, "existing device", None)?;
            state.require_u2f(credential.clone())?;
            assert!(state.status().u2f_required);
            assert!(state.authorized(&saved.pairing.token));
            assert!(state.authorized(&state.legacy.token));
            let (address, task) = listener(state.clone()).await?;
            assert!(
                login_with_touch(
                    &address,
                    "ej",
                    "wrong password",
                    "work",
                    Capabilities::CURRENT,
                    |_, _| panic!("touch requested before password proof")
                )
                .await
                .is_err()
            );
            assert!(
                login_with_touch(
                    &address,
                    "ej",
                    PASSWORD,
                    "old",
                    Capabilities::OLD,
                    |_, _| panic!("old client reached touch")
                )
                .await
                .is_err()
            );
            assert!(
                login_with_touch(
                    &address,
                    "ej",
                    PASSWORD,
                    "missing",
                    Capabilities::CURRENT,
                    |_, _| anyhow::bail!("no key connected")
                )
                .await
                .is_err()
            );
            let signed = Arc::new(Mutex::new(None));
            let signed_copy = signed.clone();
            let signing_key = key.clone();
            let enrolled = login_with_touch(
                &address,
                "ej",
                PASSWORD,
                "touched",
                Capabilities::CURRENT,
                move |_, hash| {
                    let mut authdata = digest::digest(&digest::SHA256, rp.as_bytes())
                        .as_ref()
                        .to_vec();
                    authdata.push(1);
                    authdata.extend_from_slice(&1u32.to_be_bytes());
                    let mut message = authdata.clone();
                    message.extend_from_slice(&hash);
                    let assertion = crate::security_key::Assertion {
                        authdata,
                        signature: signing_key
                            .sign(&ring::rand::SystemRandom::new(), &message)
                            .unwrap()
                            .as_ref()
                            .to_vec(),
                    };
                    *signed_copy.lock().unwrap() = Some((hash, serde_json::to_vec(&assertion)?));
                    Ok(assertion)
                },
            )
            .await?;
            assert!(state.authorized(&enrolled.token));
            let (previous_hash, previous) = signed.lock().unwrap().take().unwrap();
            let generation = state.record.lock().unwrap().generation;
            let old_assertion = serde_json::from_slice(&previous)?;
            assert!(
                state
                    .accept_touch(generation, &previous_hash, &old_assertion)
                    .is_err(),
                "same counter must not be accepted twice"
            );
            assert!(
                login_with_touch(
                    &address,
                    "ej",
                    PASSWORD,
                    "replay",
                    Capabilities::CURRENT,
                    move |_, _| Ok(serde_json::from_slice(&previous)?)
                )
                .await
                .is_err()
            );
            assert_eq!(state.status().devices.len(), 2);
            let reopened = ServerState::open(state.path.parent().unwrap(), state.legacy.clone())?;
            assert!(reopened.status().u2f_required);
            assert_eq!(reopened.record.lock().unwrap().u2f_counter, 1);
            state.set_password("ej", "Changed password retains U2F!")?;
            assert!(state.status().u2f_required);
            assert!(
                state
                    .accept_touch(generation, &previous_hash, &old_assertion)
                    .is_err()
            );
            assert!(
                state
                    .issue(generation, "changed policy during touch", None)
                    .is_err()
            );
            state.disable_u2f()?;
            assert!(!state.status().u2f_required);
            assert!(state.authorized(&state.legacy.token));
            task.abort();
            Ok(())
        }

        #[test]
        fn system_tickets_are_single_session_expiring_revocable_and_memory_only() -> Result<()> {
            let (_temporary, state) = state()?;
            let generation = state.system_generation()?;
            let ticket = state.issue_system_session(generation)?;
            assert!(state.authorized(&ticket.token));
            let reopened = ServerState::open(state.path.parent().unwrap(), state.legacy.clone())?;
            assert!(!reopened.authorized(&ticket.token));
            assert!(!fs::read_to_string(&state.path)?.contains(&ticket.token));
            let lease = state.begin_session(&ticket.token)?;
            // Only ticket sessions may unlock the desktop (--unlock-on-system-login).
            assert!(lease.system_login() && lease.owner());
            assert!(!state.begin_session(&state.legacy.token)?.system_login());
            assert!(state.begin_session(&ticket.token).is_err());
            assert!(state.authorized(&ticket.token));
            drop(lease);
            assert!(!state.authorized(&ticket.token));
            assert!(state.begin_session(&ticket.token).is_err());

            let expired = state.issue_system_session(generation)?;
            state
                .system_sessions
                .lock()
                .unwrap()
                .get_mut(&token_hash(&expired.token))
                .unwrap()
                .deadline = std::time::Instant::now() - Duration::from_secs(1);
            assert!(!state.authorized(&expired.token));
            assert!(state.begin_session(&expired.token).is_err());
            let active = state.issue_system_session(generation)?;
            let _lease = state.begin_session(&active.token)?;
            state.revoke_all_devices()?;
            assert!(!state.authorized(&active.token));
            assert!(state.issue_system_session(generation).is_err());
            // Old explicitly saved trust still works and is not consumed.
            drop(state.begin_session(&state.legacy.token)?);
            assert!(state.authorized(&state.legacy.token));
            Ok(())
        }

        #[test]
        fn invites_enroll_guests_within_their_uses_and_lifetime() -> Result<()> {
            use crate::protocol::{Permissions, Role};
            let (_temporary, state) = state()?;
            let key = || crate::e2e::DeviceKey::generate().map(|(key, _)| key.public_key());
            let hour = Duration::from_secs(3600);
            // Bounds: name, uses, lifetime.
            assert!(
                state
                    .create_invite("", 1, hour, Permissions::GUEST_DEFAULT)
                    .is_err()
            );
            assert!(
                state
                    .create_invite("Bad\nname", 1, hour, Permissions::GUEST_DEFAULT)
                    .is_err()
            );
            assert!(
                state
                    .create_invite("Alice", 0, hour, Permissions::GUEST_DEFAULT)
                    .is_err()
            );
            assert!(
                state
                    .create_invite(
                        "Alice",
                        MAX_INVITE_USES + 1,
                        hour,
                        Permissions::GUEST_DEFAULT
                    )
                    .is_err()
            );
            assert!(
                state
                    .create_invite(
                        "Alice",
                        1,
                        Duration::from_secs(4),
                        Permissions::GUEST_DEFAULT
                    )
                    .is_err()
            );
            assert!(
                state
                    .create_invite(
                        "Alice",
                        1,
                        MAX_INVITE_LIFETIME * 2,
                        Permissions::GUEST_DEFAULT
                    )
                    .is_err()
            );
            // Two uses: two guest devices, then nothing (no replay).
            let invite = state.create_invite("Couch", 2, hour, Permissions::GUEST_DEFAULT)?;
            assert_eq!(state.invites().len(), 1);
            assert_eq!(*state.invite_secret(&invite.id)?, invite.secret);
            // The wrong secret (another invite's) spends nothing.
            assert!(
                state
                    .enroll_guest(&invite.id, &"0".repeat(32), &key()?)
                    .is_err()
            );
            let first = state.enroll_guest(&invite.id, &invite.secret, &key()?)?;
            let second = state.enroll_guest(&invite.id, &invite.secret, &key()?)?;
            assert!(
                state
                    .enroll_guest(&invite.id, &invite.secret, &key()?)
                    .is_err()
            );
            assert!(state.invite_secret(&invite.id).is_err());
            assert!(state.invites().is_empty());
            let devices = state.status().devices;
            assert_eq!(
                devices.iter().map(|d| d.name.as_str()).collect::<Vec<_>>(),
                ["Couch #1", "Couch #2"]
            );
            assert!(devices.iter().all(|d| d.source == "invite"
                && d.guest
                    == Some(GuestGrant {
                        permissions: Permissions::GUEST_DEFAULT,
                        trusted: false
                    })));
            // A guest's session is a guest's, read live.
            let lease = state.begin_session(&first.pairing.token)?;
            let role = lease.access().unwrap();
            assert_eq!(role.role, Role::Guest);
            assert_eq!(role.permissions, Permissions::GUEST_DEFAULT);
            assert!(!role.trusted && !role.is_owner());
            let keys = Permissions {
                keyboard_mouse: true,
                ..Permissions::default()
            };
            state.set_guest(&first.id, Some(keys), Some(true))?;
            let role = lease.access().unwrap();
            assert_eq!((role.permissions, role.trusted), (keys, true));
            // Owners' devices are not guests and cannot be made guests here.
            let owner = state.enroll_device("Laptop", None)?;
            assert!(state.set_guest(&owner.id, Some(keys), None).is_err());
            let owner_lease = state.begin_session(&owner.pairing.token)?;
            assert!(owner_lease.access().unwrap().is_owner());
            assert_eq!(owner_lease.access().unwrap().permissions, Permissions::ALL);
            let legacy = state.begin_session(&state.legacy.token)?;
            assert!(legacy.access().unwrap().is_owner());
            // Revocation ends the role at once.
            state.revoke_device(&first.id)?;
            assert!(lease.access().is_none());
            assert!(
                state
                    .begin_session(&second.pairing.token)?
                    .access()
                    .is_some()
            );
            // Expired invites enroll nothing and are not listed.
            let expiring = state.create_invite("Later", 5, hour, Permissions::default())?;
            state.update(|r| {
                r.invites
                    .iter_mut()
                    .for_each(|invite| invite.expires_unix = now().saturating_sub(1));
                Ok(())
            })?;
            assert!(state.invites().is_empty());
            assert!(state.invite_secret(&expiring.id).is_err());
            assert!(
                state
                    .enroll_guest(&expiring.id, &expiring.secret, &key()?)
                    .is_err()
            );
            // Revoked invites likewise; uses survive a restart.
            let revoked = state.create_invite("Gone", 1, hour, Permissions::default())?;
            state.revoke_invite(&revoked.id)?;
            assert!(
                state
                    .enroll_guest(&revoked.id, &revoked.secret, &key()?)
                    .is_err()
            );
            let kept = state.create_invite("Kept", 2, hour, Permissions::default())?;
            state.enroll_guest(&kept.id, &kept.secret, &key()?)?;
            let directory = state.path.parent().unwrap().to_owned();
            let reopened = ServerState::open(&directory, state.legacy.clone())?;
            assert_eq!(reopened.invites()[0].uses_left, 1);
            reopened.enroll_guest(&kept.id, &kept.secret, &key()?)?;
            assert!(
                reopened
                    .enroll_guest(&kept.id, &kept.secret, &key()?)
                    .is_err()
            );
            // Revoke all removes guests and open invites too.
            let open = reopened.create_invite("Open", 3, hour, Permissions::default())?;
            reopened.revoke_all_devices()?;
            assert!(reopened.status().devices.is_empty());
            assert!(reopened.invites().is_empty());
            assert!(
                reopened
                    .enroll_guest(&open.id, &open.secret, &key()?)
                    .is_err()
            );
            Ok(())
        }

        #[test]
        fn device_keys_gate_relayed_leases_and_old_records_still_load() -> Result<()> {
            let (_temporary, state) = state()?;
            let (key, _) = crate::e2e::DeviceKey::generate()?;
            let public = key.public_key();
            let keyed = state.enroll_device("keyed", Some(&public))?;
            let legacy = state.enroll_device("legacy", None)?;
            let (other, _) = crate::e2e::DeviceKey::generate()?;
            let hash = |issued: &IssuedDevice| token_hash(&issued.pairing.token);
            let credentials = state.relay_credentials();
            let find = |h: &[u8]| credentials.iter().find(|c| c.hash[..] == *h).unwrap();
            assert_eq!(find(&hash(&keyed)).device_key.as_deref(), Some(&public[..]));
            assert!(!find(&hash(&keyed)).hash_only());
            assert!(find(&hash(&legacy)).hash_only());
            assert!(find(&token_hash(&state.legacy.token)).hash_only());
            let ticket = state.issue_system_session(state.system_generation()?)?;
            let ticket_credential = state.relay_credentials();
            let ticket_credential = ticket_credential
                .iter()
                .find(|c| c.hash[..] == token_hash(&ticket.token)[..])
                .unwrap();
            assert!(ticket_credential.ticket && !ticket_credential.hash_only());

            // Relayed: the stored hash alone does not lease a keyed device,
            // nor does a different key; the proven key does.
            assert!(state.begin_session_hash(hash(&keyed), None).is_err());
            assert!(
                state
                    .begin_session_hash(hash(&keyed), Some(&other.public_key()))
                    .is_err()
            );
            drop(state.begin_session_hash(hash(&keyed), Some(&public))?);
            // Legacy devices and the host token keep hash authentication.
            drop(state.begin_session_hash(hash(&legacy), None)?);
            drop(state.begin_session_hash(token_hash(&state.legacy.token), None)?);
            // Direct connections present the token itself: unchanged.
            drop(state.begin_session(&keyed.pairing.token)?);
            assert!(state.authorized(&keyed.pairing.token));

            // A live relayed lease ends once the stored key is no longer the
            // one it proved (a key added to, changed on or removed from the
            // device), or the device is revoked.
            let set_key = |id: &str, key: Option<&[u8]>| {
                state.update(|r| {
                    let device = r.devices.iter_mut().find(|d| d.summary.id == id).unwrap();
                    device.public_key = key.map(<[u8]>::to_vec);
                    Ok(())
                })
            };
            let lease = state.begin_session_hash(hash(&keyed), Some(&public))?;
            let unkeyed = state.begin_session_hash(hash(&legacy), None)?;
            let direct = state.begin_session(&keyed.pairing.token)?;
            assert!(lease.authorized() && unkeyed.authorized() && direct.authorized());
            set_key(&legacy.id, Some(&other.public_key()))?;
            assert!(!unkeyed.authorized());
            set_key(&keyed.id, Some(&other.public_key()))?;
            assert!(!lease.authorized() && direct.authorized());
            set_key(&keyed.id, None)?;
            assert!(!lease.authorized());
            set_key(&keyed.id, Some(&public))?;
            set_key(&legacy.id, None)?;
            assert!(lease.authorized() && unkeyed.authorized());
            let third = state.enroll_device("third", Some(&other.public_key()))?;
            let revoked = state.begin_session_hash(hash(&third), Some(&other.public_key()))?;
            state.revoke_device(&third.id)?;
            assert!(!revoked.authorized() && lease.authorized());
            drop((lease, unkeyed, direct, revoked));

            // The record keeps the key across restarts, never a private key,
            // and an access.json written before device keys still loads.
            let directory = state.path.parent().unwrap().to_owned();
            let reopened = ServerState::open(&directory, state.legacy.clone())?;
            assert!(reopened.begin_session_hash(hash(&keyed), None).is_err());
            let mut record: serde_json::Value = serde_json::from_slice(&fs::read(&state.path)?)?;
            for device in record["devices"].as_array_mut().unwrap() {
                device.as_object_mut().unwrap().remove("public_key");
            }
            fs::write(&state.path, serde_json::to_vec(&record)?)?;
            let old = ServerState::open(&directory, state.legacy.clone())?;
            assert_eq!(old.status().devices.len(), 2);
            assert!(
                old.relay_credentials()
                    .iter()
                    .all(|c| c.device_key.is_none())
            );
            drop(old.begin_session_hash(hash(&keyed), None)?);
            // A corrupt stored key fails closed at load.
            record["devices"][0]["public_key"] = serde_json::json!([1, 2, 3]);
            fs::write(&state.path, serde_json::to_vec(&record)?)?;
            assert!(ServerState::open(&directory, state.legacy.clone()).is_err());
            Ok(())
        }

        fn hash_only_devices(state: &ServerState) -> usize {
            let record = state.record.lock().unwrap();
            record
                .devices
                .iter()
                .filter(|d| d.public_key.is_none())
                .count()
        }

        /// A password login through a [`super::super::meddler`] relay. Also
        /// returns whether a hash-only device record existed at any moment
        /// of the exchange.
        async fn meddled_login(
            state: &Arc<ServerState>,
            capabilities: Capabilities,
            decide: impl Fn(bool, usize, &[u8]) -> super::super::meddler::Action + Send + Sync + 'static,
        ) -> (Result<Pairing>, Result<()>, bool) {
            // Fresh rate-limit budget per attempt.
            state.record.lock().unwrap().attempts.clear();
            state.sources.lock().unwrap().clear();
            let (client, host, relay) = super::super::meddler::spawn(decide);
            let watched = state.clone();
            let watcher = tokio::spawn(async move {
                loop {
                    if hash_only_devices(&watched) > 0 {
                        return true;
                    }
                    tokio::time::sleep(Duration::from_millis(1)).await;
                }
            });
            let host = tokio::spawn(handle(host, "127.0.0.1".parse().unwrap(), state.clone()));
            let client = login_over(client, "ej", PASSWORD, "relayed", capabilities, |_, _| {
                anyhow::bail!("no touch expected")
            })
            .await;
            let host = host.await.unwrap();
            relay.abort();
            watcher.abort();
            let hash_only_seen = watcher.await.unwrap_or(false) || hash_only_devices(state) > 0;
            (client, host, hash_only_seen)
        }

        /// The fix for a relay that dropped or delayed the device key around
        /// the credential: a capable client's device is created with its key
        /// or not at all, and the client keeps nothing unless the host bound
        /// its key to the credential.
        #[tokio::test]
        async fn keyed_password_enrollment_resists_a_meddling_relay() -> Result<()> {
            use super::super::meddler::Action;
            // Towards the host: 0 magic, 1 hello, 2 credential request,
            // 3 finalization, 4 device key. Towards the client:
            // 0 fingerprint, 1 credential response, 2 credential.
            const KEY: usize = 4;
            const CREDENTIAL: usize = 2;
            let (_temporary, state) = state()?;
            let current = Capabilities {
                u2f: false,
                device_key: true,
            };

            // Device key dropped: nothing is issued, nothing saved.
            let (client, host, hash_only) =
                meddled_login(&state, current, |up, index, _| match up && index == KEY {
                    true => Action::HangUp,
                    false => Action::Forward,
                })
                .await;
            assert!(client.is_err() && host.is_err() && !hash_only);
            assert!(state.status().devices.is_empty());

            // The host cannot deliver the credential: it removes the device.
            let (client, host, hash_only) =
                meddled_login(&state, current, |up, index, _| match up && index == KEY {
                    true => Action::ForwardThenHangUp,
                    false => Action::Forward,
                })
                .await;
            assert!(client.is_err() && host.is_err() && !hash_only);
            assert!(state.status().devices.is_empty());

            // Credential delivered but dropped by the relay: the client saves
            // nothing; the host keeps a keyed record nobody holds the key for
            // (useless with access.json alone), and enrolling again works.
            let (client, host, hash_only) =
                meddled_login(&state, current, |up, index, _| {
                    match !up && index == CREDENTIAL {
                        true => Action::HangUp,
                        false => Action::Forward,
                    }
                })
                .await;
            assert!(client.is_err() && host.is_ok() && !hash_only);
            assert_eq!(state.status().devices.len(), 1);

            // Device key held back (the old race window): still no hash-only
            // record at any point, and the device ends up keyed.
            let (client, host, hash_only) =
                meddled_login(&state, current, |up, index, _| match up && index == KEY {
                    true => Action::Delay(Duration::from_millis(300)),
                    false => Action::Forward,
                })
                .await;
            let paired = client?;
            host?;
            assert!(!hash_only);
            let public = crate::e2e::DeviceKey::from_hex(paired.device_key.as_deref().unwrap())?
                .public_key();
            let hash = token_hash(&paired.token);
            assert!(state.begin_session_hash(hash.clone(), None).is_err());
            drop(state.begin_session_hash(hash, Some(&public))?);
            assert_eq!(state.status().devices.len(), 2);

            // Stripping the capability (or adding it to an old client's
            // hello) changes the OPAQUE context: the login fails.
            let rewrite = |add: bool| {
                move |up: bool, index: usize, message: &[u8]| match up && index == 1 {
                    true => {
                        let mut hello: serde_json::Value = serde_json::from_slice(message).unwrap();
                        match add {
                            true => hello["device_key"] = true.into(),
                            false => drop(hello.as_object_mut().unwrap().remove("device_key")),
                        }
                        Action::Replace(serde_json::to_vec(&hello).unwrap())
                    }
                    false => Action::Forward,
                }
            };
            let (client, host, _) = meddled_login(&state, current, rewrite(false)).await;
            assert!(client.is_err() && host.is_err());
            let (client, host, _) = meddled_login(&state, Capabilities::OLD, rewrite(true)).await;
            assert!(client.is_err() && host.is_err());
            assert_eq!(state.status().devices.len(), 2);
            assert_eq!(hash_only_devices(&state), 0);

            // A client from before device keys still enrolls, hash-only.
            let (client, host, _) =
                meddled_login(&state, Capabilities::OLD, |_, _, _| Action::Forward).await;
            let old = client?;
            host?;
            assert!(old.device_key.is_none() && state.authorized(&old.token));
            drop(state.begin_session_hash(token_hash(&old.token), None)?);
            assert_eq!(hash_only_devices(&state), 1);
            Ok(())
        }

        #[test]
        fn persistent_rate_limit_and_revoke_generation() -> Result<()> {
            let (_temporary, state) = state()?;
            let ip: IpAddr = "127.0.0.1".parse()?;
            for _ in 0..5 {
                state.admit(ip)?;
            }
            assert!(state.admit(ip).is_err());
            for _ in 0..5 {
                state.admit("127.0.0.2".parse()?)?;
            }
            let reopened = ServerState::open(state.path.parent().unwrap(), state.legacy.clone())?;
            assert!(reopened.admit("127.0.0.3".parse()?).is_err());
            let generation = state.record.lock().unwrap().generation;
            state.revoke_all_devices()?;
            assert!(state.issue(generation, "in-flight login", None).is_err());
            let code_device = state.enroll_device("one-time paired", None)?;
            let generation = state.record.lock().unwrap().generation;
            state.set_password("ej", "Another long passphrase!")?;
            assert!(
                state
                    .issue(generation, "stale-password login", None)
                    .is_err()
            );
            assert!(state.authorized(&code_device.pairing.token));
            let generation = state.record.lock().unwrap().generation;
            state.disable_password()?;
            assert!(
                state
                    .issue(generation, "disabled-password login", None)
                    .is_err()
            );
            assert!(state.authorized(&code_device.pairing.token));
            state.revoke_all_devices()?;
            assert!(!state.authorized(&code_device.pairing.token));
            assert!(state.authorized(&state.legacy.token));
            Ok(())
        }

        #[test]
        fn opaque_replay_and_context_tampering_fail() -> Result<()> {
            let (_temporary, state) = state()?;
            let record = state.record.lock().unwrap();
            let setup = ServerSetup::<Suite>::deserialize(&record.setup)?;
            let account = record.account.as_ref().unwrap();
            let parameters = ServerLoginParameters {
                context: Some(b"original"),
                identifiers: Identifiers {
                    client: Some(b"ej"),
                    server: Some(state.legacy.fingerprint.as_bytes()),
                },
            };
            let client = ClientLogin::<Suite>::start(&mut OsRng, PASSWORD.as_bytes())?;
            let server = ServerLogin::<Suite>::start(
                &mut OsRng,
                &setup,
                Some(ServerRegistration::deserialize(&account.registration)?),
                client.message.clone(),
                b"ej",
                parameters.clone(),
            )?;
            let finish = client.state.finish(
                &mut OsRng,
                PASSWORD.as_bytes(),
                server.message,
                ClientLoginFinishParameters {
                    ksf: Some(&password_ksf()),
                    context: Some(b"original"),
                    identifiers: parameters.identifiers,
                },
            )?;
            let finalization = finish.message.serialize();
            server.state.finish(finish.message, parameters.clone())?;
            let replay_server = ServerLogin::<Suite>::start(
                &mut OsRng,
                &setup,
                Some(ServerRegistration::deserialize(&account.registration)?),
                client.message,
                b"ej",
                parameters.clone(),
            )?;
            assert!(
                replay_server
                    .state
                    .finish(
                        CredentialFinalization::deserialize(&finalization)?,
                        parameters.clone()
                    )
                    .is_err()
            );
            let client = ClientLogin::<Suite>::start(&mut OsRng, PASSWORD.as_bytes())?;
            let server = ServerLogin::<Suite>::start(
                &mut OsRng,
                &setup,
                Some(ServerRegistration::deserialize(&account.registration)?),
                client.message,
                b"ej",
                parameters.clone(),
            )?;
            assert!(
                client
                    .state
                    .finish(
                        &mut OsRng,
                        PASSWORD.as_bytes(),
                        server.message,
                        ClientLoginFinishParameters {
                            ksf: Some(&password_ksf()),
                            context: Some(b"tampered"),
                            identifiers: parameters.identifiers,
                        }
                    )
                    .is_err()
            );
            Ok(())
        }
    }
}

#[cfg(target_os = "linux")]
pub(crate) use server::read_device_key;
#[cfg(target_os = "linux")]
pub use server::{ServerState, SessionLease, handle};

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn empty_and_oversized_packets_rejected_before_payload() -> Result<()> {
        for length in [0, (MAX_PACKET + 1) as u16, u16::MAX] {
            let (mut writer, mut reader) = tokio::io::duplex(2);
            writer.write_u16(length).await?;
            assert!(read_packet(&mut reader).await.is_err());
        }
        Ok(())
    }
    #[test]
    fn password_and_username_validation() {
        for bad in [
            "123456",
            "12345678901234567890",
            "aaaaaaaaaaaa",
            "            ",
        ] {
            assert!(validate_password(bad).is_err());
        }
        assert!(validate_password("  exact pass phrase  ").is_ok());
        for bad in ["", "has spaces", "user/name", "über"] {
            assert!(validate_username(bad).is_err());
        }
        assert!(validate_username("EJ-work_1").is_ok());
    }

    #[test]
    fn u2f_encrypted_messages_bind_role_session_and_context() -> Result<()> {
        let packet = seal_touch(b"session", TOUCH_REQUEST, b"context", b"challenge".to_vec())?;
        assert_eq!(
            open_touch(b"session", TOUCH_REQUEST, b"context", packet.clone())?,
            b"challenge"
        );
        assert!(open_touch(b"session", TOUCH_REPLY, b"context", packet.clone()).is_err());
        assert!(open_touch(b"other session", TOUCH_REQUEST, b"context", packet.clone()).is_err());
        assert!(open_touch(b"session", TOUCH_REQUEST, b"other context", packet.clone()).is_err());
        let mut tampered = packet;
        tampered[12] ^= 1;
        assert!(open_touch(b"session", TOUCH_REQUEST, b"context", tampered).is_err());
        Ok(())
    }

    #[test]
    fn encrypted_credential_tampering_fails() -> Result<()> {
        let pairing = Pairing {
            token: "a".repeat(64),
            fingerprint: "b".repeat(64),
            device_key: None,
        };
        let session = b"test session secret";
        let nonce = [0u8; 12];
        let mut payload = serde_json::to_vec(&pairing)?;
        credential_key(session)?
            .seal_in_place_append_tag(
                aead::Nonce::assume_unique_for_key(nonce),
                aead::Aad::from(b"context"),
                &mut payload,
            )
            .unwrap();
        let mut packet = nonce.to_vec();
        packet.extend_from_slice(&payload);
        assert_eq!(
            open_credential(session, b"context", packet.clone())?.token,
            pairing.token
        );
        assert!(open_credential(session, b"changed", packet.clone()).is_err());
        packet[12] ^= 1;
        assert!(open_credential(session, b"context", packet).is_err());
        Ok(())
    }
}

/// A malicious relay for enrollment tests. It forwards a framed exchange (an
/// 8-byte magic from the client, then u16-length packets both ways) and lets
/// the test drop, delay or rewrite any message.
#[cfg(test)]
pub(crate) mod meddler {
    use std::time::Duration;
    use tokio::io::{AsyncReadExt, AsyncWriteExt, DuplexStream, ReadHalf, WriteHalf};

    pub enum Action {
        Forward,
        Replace(Vec<u8>),
        Delay(Duration),
        /// Forward, then hang up on both ends.
        ForwardThenHangUp,
        /// Swallow the message and hang up on both ends.
        HangUp,
    }

    /// `(client end, host end, relay task)`. `decide(to_host, index, message)`:
    /// messages are numbered per direction; towards the host, 0 is the magic.
    pub fn spawn(
        decide: impl Fn(bool, usize, &[u8]) -> Action + Send + Sync + 'static,
    ) -> (DuplexStream, DuplexStream, tokio::task::JoinHandle<()>) {
        let (client, client_side) = tokio::io::duplex(64 * 1024);
        let (host, host_side) = tokio::io::duplex(64 * 1024);
        let task = tokio::spawn(async move {
            let (client_read, client_write) = tokio::io::split(client_side);
            let (host_read, host_write) = tokio::io::split(host_side);
            let up = pump(client_read, host_write, true, &decide);
            let down = pump(host_read, client_write, false, &decide);
            tokio::pin!(up, down);
            // A hang-up drops every half (both connections); a clean end of
            // one direction lets the other finish.
            tokio::select! {
                hung_up = &mut up => if !hung_up { down.await; },
                hung_up = &mut down => if !hung_up { up.await; },
            }
        });
        (client, host, task)
    }

    /// Returns whether it hung up.
    async fn pump(
        mut from: ReadHalf<DuplexStream>,
        mut to: WriteHalf<DuplexStream>,
        to_host: bool,
        decide: &(impl Fn(bool, usize, &[u8]) -> Action + Send + Sync),
    ) -> bool {
        for index in 0.. {
            let magic = to_host && index == 0;
            let message = if magic {
                let mut magic = vec![0; 8];
                from.read_exact(&mut magic).await.map(|_| magic)
            } else {
                match from.read_u16().await {
                    Ok(length) => {
                        let mut packet = vec![0; length.into()];
                        from.read_exact(&mut packet).await.map(|_| packet)
                    }
                    Err(error) => Err(error),
                }
            };
            let Ok(message) = message else { break };
            let frame = |message: &[u8]| match magic {
                true => message.to_vec(),
                false => [&(message.len() as u16).to_be_bytes()[..], message].concat(),
            };
            let (out, hang_up) = match decide(to_host, index, &message) {
                Action::Forward => (Some(frame(&message)), false),
                Action::Replace(other) => (Some(frame(&other)), false),
                Action::Delay(delay) => {
                    tokio::time::sleep(delay).await;
                    (Some(frame(&message)), false)
                }
                Action::ForwardThenHangUp => (Some(frame(&message)), true),
                Action::HangUp => (None, true),
            };
            if let Some(out) = out
                && to.write_all(&out).await.is_err()
            {
                break;
            }
            if hang_up {
                return true;
            }
        }
        let _ = to.shutdown().await;
        false
    }
}
