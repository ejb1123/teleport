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

async fn write_packet(stream: &mut TcpStream, data: &[u8]) -> Result<()> {
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
        true,
        |credential, hash| crate::security_key::sign_u2f(&credential, &hash, None),
    )
    .await
}

async fn login_with_touch<F>(
    address: &str,
    username: &str,
    password: &str,
    device_name: &str,
    supports_u2f: bool,
    touch: F,
) -> Result<Pairing>
where
    F: FnOnce(crate::security_key::Credential, [u8; 32]) -> Result<crate::security_key::Assertion>
        + Send
        + 'static,
{
    crate::pairing::validate_address(address)?;
    let hello = Hello {
        username: username.into(),
        device_name: device_name.into(),
        u2f: supports_u2f,
    };
    validate_hello(&hello)?;
    ensure!(
        !password.is_empty() && password.len() <= 1024,
        "invalid password length"
    );
    tokio::time::timeout(TIMEOUT, async {
        let mut stream = TcpStream::connect(address).await.context(
            "cannot reach password access service; check TCP firewall and host settings",
        )?;
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
        let pairing = open_credential(&finish.session_key, &context, packet)?;
        ensure!(
            pairing.fingerprint == fingerprint,
            "authenticated host identity mismatch"
        );
        Ok(pairing)
    })
    .await
    .context("password login timed out")?
}

#[cfg(target_os = "linux")]
#[derive(Clone, Serialize, Deserialize, Debug)]
pub struct DeviceSummary {
    pub id: String,
    pub name: String,
    pub created_unix: u64,
    pub source: String,
}
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

    #[derive(Clone, Serialize, Deserialize)]
    struct Device {
        summary: DeviceSummary,
        token_hash: Vec<u8>,
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
    pub struct SessionLease {
        state: Arc<ServerState>,
        hash: Vec<u8>,
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
                        device.token_hash.len() == 32,
                        "invalid stored device credential"
                    );
                }
                if let Some(key) = &record.u2f {
                    crate::security_key::validate_u2f_credential(key, &legacy_pairing.fingerprint)?;
                }
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
        pub fn revoke_all_devices(&self) -> Result<()> {
            self.update(|r| {
                r.devices.clear();
                r.generation = r
                    .generation
                    .checked_add(1)
                    .context("access generation exhausted")?;
                Ok(())
            })
        }
        pub fn authorized(&self, token: &str) -> bool {
            if token.len() != 64 || !token.bytes().all(|b| b.is_ascii_hexdigit()) {
                return false;
            }
            if bool::from(token.as_bytes().ct_eq(self.legacy.token.as_bytes())) {
                return true;
            }
            let hash = token_hash(token);
            if self.system_login_allowed()
                && let Ok(generation) = self.system_generation()
                && let Ok(sessions) = self.system_sessions.lock()
                && sessions.get(&hash).is_some_and(|s| {
                    s.generation == generation
                        && (s.active || s.deadline > std::time::Instant::now())
                })
            {
                return true;
            }
            self.record
                .lock()
                .map(|r| {
                    r.devices.iter().fold(false, |accepted, d| {
                        accepted | bool::from(d.token_hash.ct_eq(&hash))
                    })
                })
                .unwrap_or(false)
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
            let hash = token_hash(token);
            // Atomic authorization and claim; absence from the ticket map must
            // never turn a concurrently revoked ticket into reusable trust.
            let record = self
                .record
                .lock()
                .map_err(|_| anyhow::anyhow!("access unavailable"))?;
            let saved = bool::from(token.as_bytes().ct_eq(self.legacy.token.as_bytes()))
                | record.devices.iter().fold(false, |accepted, d| {
                    accepted | bool::from(d.token_hash.ct_eq(&hash))
                });
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

        fn issue(&self, generation: u64, name: &str) -> Result<Pairing> {
            let token = crate::identity::token();
            self.update(|r| {
                ensure!(
                    r.generation == generation && r.account.is_some(),
                    "login failed"
                );
                ensure!(
                    r.devices.len() < 128,
                    "device limit reached; remove an old device on the host"
                );
                r.devices.push(Device {
                    summary: DeviceSummary {
                        id: crate::identity::token(),
                        name: name.into(),
                        created_unix: now(),
                        source: "password".into(),
                    },
                    token_hash: token_hash(&token),
                });
                Ok(())
            })?;
            Ok(Pairing {
                token,
                fingerprint: self.legacy.fingerprint.clone(),
            })
        }

        /// Local trusted caller only: invoke after one-time code proof succeeds.
        pub fn enroll_device(&self, name: &str) -> Result<Pairing> {
            ensure!(
                !name.is_empty() && name.len() <= 128 && !name.chars().any(char::is_control),
                "invalid device name"
            );
            let token = crate::identity::token();
            self.update(|r| {
                ensure!(
                    r.devices.len() < 128,
                    "device limit reached; remove an old device on the host"
                );
                r.devices.push(Device {
                    summary: DeviceSummary {
                        id: crate::identity::token(),
                        name: name.into(),
                        created_unix: now(),
                        source: "pairing".into(),
                    },
                    token_hash: token_hash(&token),
                });
                Ok(())
            })?;
            Ok(Pairing {
                token,
                fingerprint: self.legacy.fingerprint.clone(),
            })
        }
    }

    pub async fn handle(mut stream: TcpStream, state: Arc<ServerState>) -> Result<()> {
        let _permit = state
            .slots
            .try_acquire()
            .map_err(|_| anyhow::anyhow!("login temporarily unavailable"))?;
        state.admit(stream.peer_addr()?.ip())?;
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
            let pairing = state.issue(generation, &hello.device_name)?;
            let mut payload = serde_json::to_vec(&pairing)?;
            let nonce: [u8; 12] = rand::random();
            credential_key(&finish.session_key)?
                .seal_in_place_append_tag(
                    aead::Nonce::assume_unique_for_key(nonce),
                    aead::Aad::from(&context),
                    &mut payload,
                )
                .map_err(|_| anyhow::anyhow!("credential encryption failed"))?;
            let mut packet = nonce.to_vec();
            packet.extend_from_slice(&payload);
            write_packet(&mut stream, &packet).await?;
            Ok(())
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
                        if let Err(error) = handle(stream, state).await {
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
            let saved = state.issue(generation, "existing device")?;
            state.require_u2f(credential.clone())?;
            assert!(state.status().u2f_required);
            assert!(state.authorized(&saved.token));
            assert!(state.authorized(&state.legacy.token));
            let (address, task) = listener(state.clone()).await?;
            assert!(
                login_with_touch(
                    &address,
                    "ej",
                    "wrong password",
                    "work",
                    true,
                    |_, _| panic!("touch requested before password proof")
                )
                .await
                .is_err()
            );
            assert!(
                login_with_touch(&address, "ej", PASSWORD, "old", false, |_, _| panic!(
                    "old client reached touch"
                ))
                .await
                .is_err()
            );
            assert!(
                login_with_touch(
                    &address,
                    "ej",
                    PASSWORD,
                    "missing",
                    true,
                    |_, _| anyhow::bail!("no key connected")
                )
                .await
                .is_err()
            );
            let signed = Arc::new(Mutex::new(None));
            let signed_copy = signed.clone();
            let signing_key = key.clone();
            let enrolled =
                login_with_touch(&address, "ej", PASSWORD, "touched", true, move |_, hash| {
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
                })
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
                login_with_touch(&address, "ej", PASSWORD, "replay", true, move |_, _| Ok(
                    serde_json::from_slice(&previous)?
                ))
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
                    .issue(generation, "changed policy during touch")
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
            assert!(state.issue(generation, "in-flight login").is_err());
            let code_device = state.enroll_device("one-time paired")?;
            let generation = state.record.lock().unwrap().generation;
            state.set_password("ej", "Another long passphrase!")?;
            assert!(state.issue(generation, "stale-password login").is_err());
            assert!(state.authorized(&code_device.token));
            let generation = state.record.lock().unwrap().generation;
            state.disable_password()?;
            assert!(state.issue(generation, "disabled-password login").is_err());
            assert!(state.authorized(&code_device.token));
            state.revoke_all_devices()?;
            assert!(!state.authorized(&code_device.token));
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
pub use server::{ServerState, handle};

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
