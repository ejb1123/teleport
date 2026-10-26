//! End-to-end encryption for sessions carried through an untrusted MoQ relay.
//!
//! Through a relay, QUIC/TLS terminates at the relay: the pinned certificate
//! fingerprint no longer authenticates the host, and every MoQ object would be
//! visible to the relay operator. This module authenticates both ends and
//! seals every object independently of the transport.
//!
//! Handshake (one round trip, carried on ordinary MoQ tracks):
//!
//! 1. Client → host `ClientHello`: a fresh X25519 public key and a credential
//!    identifier `HMAC(SHA-256(token), protocol ‖ ephemeral)`. The bearer token
//!    itself is never sent; hosts only store `SHA-256(token)` for devices and
//!    tickets, which is all they need to recognise the identifier. A fresh
//!    ephemeral key per session makes identifiers unlinkable for the relay.
//! 2. Host → client `HostReply`: its X25519 public key, its TLS certificate,
//!    and an ECDSA P-256 signature over the transcript with the certificate's
//!    key. The client accepts only the certificate whose SHA-256 matches the
//!    fingerprint it already pins, so the trust model is unchanged.
//! 3. Both derive per-direction keys with HKDF-SHA-256 over the X25519 shared
//!    secret *and* the credential hash, salted with the transcript hash:
//!    forward secrecy from the ephemeral exchange, and only a holder of an
//!    accepted credential can read or write the session.
//! 4. Client → host confirmation: the session identifier sealed under the
//!    client-to-host key, and, for a device enrolled with a [`DeviceKey`], an
//!    Ed25519 signature over the transcript (both ephemerals, the credential
//!    identifier, the request and the host certificate), also sealed. The
//!    host hands out [`SessionKeys`] only through
//!    [`PendingSession::confirm`], which requires that signature whenever the
//!    host stored a public key for the credential.
//!
//! Why the signature matters: hosts store `SHA-256(token)`, and steps 1–3 need
//! nothing more. Without step 4's signature, anyone who read the host's access
//! record could play the client: pick an ephemeral, compute the identifier,
//! derive the keys and confirm. The host stores only the public half of a
//! device key, so its records no longer suffice for keyed devices. Their
//! session keys still need the X25519 secret of one end, so a party holding
//! the stored hash cannot read a legitimate session either. Devices enrolled
//! before device keys, the host's own pairing token and system-login tickets
//! remain hash-authenticated (see `docs/relay.md`).
//!
//! Objects: each track gets its own ChaCha20-Poly1305 key derived from the
//! direction key and the track name, so nonces never collide across tracks and
//! an object cannot be spliced into another track. The nonce is a per-track
//! counter carried in clear in front of the ciphertext; the AAD binds the
//! session, the MoQ group sequence and the counter. Ordered tracks require a
//! strictly increasing counter (no replay or reordering); unordered tracks
//! (video, audio, pointer) accept reordering within a window but never a
//! repeated counter.
//!
//! Not hidden from the relay: IP addresses, track names, object sizes and
//! timing. Input timing in particular reveals typing cadence.

use anyhow::{Context, Result, anyhow, ensure};
use ring::{
    aead, agreement, digest, hkdf, hmac,
    rand::{SecureRandom, SystemRandom},
    signature::{self, KeyPair},
};
use serde::{Deserialize, Serialize};
use subtle::ConstantTimeEq;

const PROTOCOL: &[u8] = b"teleport-e2e-v1";
const HOST_SIGNATURE_LABEL: &[u8] = b"teleport-e2e-v1 host transcript";
const DEVICE_SIGNATURE_LABEL: &[u8] = b"teleport-e2e-v1 device transcript";
/// The "credential hash" of a ticket hello: a public constant, so before the
/// confirmation only the X25519 exchange keeps the session keys secret (the
/// host is authenticated by its transcript signature, as always). The client
/// is authenticated afterwards, by the sealed ticket and its `cnf` key's
/// transcript signature ([`PendingSession::confirm_ticket`]).
pub const TICKET_HELLO: [u8; 32] = *b"teleport-e2e-v1 ticket hello\0\0\0\0";
/// Track key label for the client's key-confirmation objects.
const CONFIRM_TRACK: &str = "confirm";
/// Length of an Ed25519 public key as stored by hosts.
pub const DEVICE_PUBLIC_KEY_LEN: usize = 32;
#[cfg(target_os = "linux")]
const DEVICE_SIGNATURE_LEN: usize = 64;
/// Unordered tracks accept objects up to this many counters behind the newest.
const REPLAY_WINDOW: u64 = 1024;
/// Counter prefix carried in clear before each sealed object.
const COUNTER_LEN: usize = 8;

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct ClientHello {
    pub ephemeral: Vec<u8>,
    pub credential_id: Vec<u8>,
    /// Connection options (the URL query of a direct connection). Visible to
    /// the relay, but bound into the signed transcript: a client whose
    /// request was altered rejects the host reply.
    pub request: String,
    /// A ticket hello (`crate::ticket`, docs/tickets.md#tickets-in-the-handshake):
    /// the credential is a session ticket the client sends SEALED in its
    /// confirmation, so the relay never sees it. The credential identifier
    /// is then keyed with [`TICKET_HELLO`], not a stored hash.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub ticket: bool,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct HostReply {
    pub ephemeral: Vec<u8>,
    pub certificate: Vec<u8>,
    pub signature: Vec<u8>,
}

/// `SHA-256(token)`: the form in which hosts already store device and ticket
/// credentials, and the only form this protocol ever uses.
pub fn credential_hash(token: &str) -> [u8; 32] {
    digest::digest(&digest::SHA256, token.as_bytes())
        .as_ref()
        .try_into()
        .expect("SHA-256 is 32 bytes")
}

/// A device's relay signing key (Ed25519). The client keeps the seed in its
/// private pairing file; the host stores only [`DeviceKey::public_key`], so
/// the host's records alone can no longer open relayed sessions as the device.
pub struct DeviceKey(signature::Ed25519KeyPair);

impl DeviceKey {
    /// A fresh key and its hex seed for the pairing file.
    pub fn generate() -> Result<(Self, String)> {
        let seed = zeroize::Zeroizing::new(random::<32>());
        let hex: String = seed.iter().map(|byte| format!("{byte:02x}")).collect();
        Ok((Self::from_seed(&seed)?, hex))
    }

    pub fn from_hex(hex: &str) -> Result<Self> {
        ensure!(
            hex.len() == 64 && hex.bytes().all(|b| b.is_ascii_hexdigit()),
            "invalid device key"
        );
        let mut seed = zeroize::Zeroizing::new([0u8; 32]);
        for (byte, pair) in seed.iter_mut().zip(hex.as_bytes().chunks(2)) {
            *byte = u8::from_str_radix(std::str::from_utf8(pair)?, 16)?;
        }
        Self::from_seed(&seed)
    }

    fn from_seed(seed: &[u8; 32]) -> Result<Self> {
        signature::Ed25519KeyPair::from_seed_unchecked(seed)
            .map(Self)
            .map_err(|_| anyhow!("invalid device key"))
    }

    pub fn public_key(&self) -> [u8; DEVICE_PUBLIC_KEY_LEN] {
        self.0
            .public_key()
            .as_ref()
            .try_into()
            .expect("Ed25519 public keys are 32 bytes")
    }

    /// An Ed25519 signature over `message` (ticket redemption proves the
    /// key with it, `crate::ticket`).
    pub fn sign(&self, message: &[u8]) -> Vec<u8> {
        self.0.sign(message).as_ref().to_vec()
    }

    fn sign_transcript(&self, transcript: &[u8; 32]) -> Vec<u8> {
        self.0.sign(&device_signed(transcript)).as_ref().to_vec()
    }
}

fn device_signed(transcript: &[u8; 32]) -> Vec<u8> {
    let mut signed = DEVICE_SIGNATURE_LABEL.to_vec();
    signed.extend_from_slice(transcript);
    signed
}

/// Device-key binding at enrollment, carried inside a channel that is already
/// mutually authenticated (an OPAQUE login or a SPAKE2 code pairing) whose
/// transcript includes the client's device-key capability, so a relay can
/// neither strip nor add it:
///
/// 1. client → host, *before* any credential exists: the device public key,
///    sealed with a key derived from the enrollment secret and bound to the
///    enrollment context;
/// 2. host → client: the issued credential, sealed in the other direction
///    together with the public key the host stored in the same record update
///    as the token hash (or an explicit "no device record" marker for the
///    host's own pairing token).
///
/// A keyed device therefore never exists hash-only on the host, and the client
/// saves a pairing only after (2) names its own key: dropping, delaying or
/// truncating either message fails the enrollment instead of downgrading it.
pub struct Registration {
    to_host: aead::LessSafeKey,
    to_client: aead::LessSafeKey,
    context: Vec<u8>,
}

/// A credential opened by [`Registration::open_credential`].
pub struct BoundCredential {
    pub credential: Vec<u8>,
    /// The host stored the client's key with the credential. `false` only for
    /// the host's own pairing token, which has no device record to key.
    pub keyed: bool,
}

impl Registration {
    /// `secret` is the enrollment's shared secret (or a key derived from it);
    /// `context` is its transcript, which both ends already agree on.
    pub fn new(secret: &[u8], context: &[u8]) -> Result<Self> {
        let prk = hkdf::Salt::new(hkdf::HKDF_SHA256, b"teleport device key enrollment v2")
            .extract(secret);
        let key = |label: &[u8]| -> Result<aead::LessSafeKey> {
            let raw: [u8; 32] = expand(&prk, &[label])?;
            Ok(aead::LessSafeKey::new(
                aead::UnboundKey::new(&aead::CHACHA20_POLY1305, &raw)
                    .map_err(|_| anyhow!("invalid registration key"))?,
            ))
        };
        Ok(Self {
            to_host: key(b"client to host")?,
            to_client: key(b"host to client")?,
            context: context.to_vec(),
        })
    }

    fn seal(&self, key: &aead::LessSafeKey, mut body: Vec<u8>) -> Vec<u8> {
        let nonce = random::<12>();
        key.seal_in_place_append_tag(
            aead::Nonce::assume_unique_for_key(nonce),
            aead::Aad::from(&self.context),
            &mut body,
        )
        .expect("sealing an enrollment message cannot fail");
        let mut packet = nonce.to_vec();
        packet.extend_from_slice(&body);
        packet
    }

    fn open(&self, key: &aead::LessSafeKey, packet: &[u8]) -> Result<Vec<u8>> {
        ensure!(
            packet.len() > 12 + aead::MAX_TAG_LEN,
            "invalid device key enrollment message"
        );
        let mut body = packet[12..].to_vec();
        let opened = key
            .open_in_place(
                aead::Nonce::assume_unique_for_key(packet[..12].try_into().unwrap()),
                aead::Aad::from(&self.context),
                &mut body,
            )
            .map_err(|_| anyhow!("device key enrollment failed authentication"))?;
        Ok(opened.to_vec())
    }

    /// Client: the sealed public key, sent before the host issues anything.
    pub fn request(&self, key: &DeviceKey) -> Vec<u8> {
        self.seal(&self.to_host, key.public_key().to_vec())
    }

    /// Host: the public key a request carries.
    #[cfg(target_os = "linux")]
    pub fn open_request(&self, packet: &[u8]) -> Result<[u8; DEVICE_PUBLIC_KEY_LEN]> {
        self.open(&self.to_host, packet)?
            .try_into()
            .map_err(|_| anyhow!("invalid device key enrollment message"))
    }

    /// Host: the issued credential, bound to the public key stored with it
    /// (`None`: the host's own pairing token, which has no device record).
    #[cfg(target_os = "linux")]
    pub fn seal_credential(
        &self,
        stored: Option<&[u8; DEVICE_PUBLIC_KEY_LEN]>,
        credential: &[u8],
    ) -> Vec<u8> {
        let mut body = match stored {
            Some(public) => [&[1u8][..], public].concat(),
            None => vec![0],
        };
        body.extend_from_slice(credential);
        self.seal(&self.to_client, body)
    }

    /// Client: the credential, once the host has bound exactly `key` to it
    /// (or said explicitly that it keeps no device record for it).
    pub fn open_credential(&self, key: &DeviceKey, packet: &[u8]) -> Result<BoundCredential> {
        let body = self.open(&self.to_client, packet)?;
        match body.split_first() {
            Some((1, rest)) if rest.len() > DEVICE_PUBLIC_KEY_LEN => {
                let (stored, credential) = rest.split_at(DEVICE_PUBLIC_KEY_LEN);
                ensure!(
                    bool::from(stored.ct_eq(&key.public_key())),
                    "host stored a different device key"
                );
                Ok(BoundCredential {
                    credential: credential.to_vec(),
                    keyed: true,
                })
            }
            Some((0, credential)) if !credential.is_empty() => Ok(BoundCredential {
                credential: credential.to_vec(),
                keyed: false,
            }),
            _ => Err(anyhow!("invalid device key enrollment message")),
        }
    }
}

/// Also MACs the connection request, so a relay cannot alter it without the
/// host failing to recognise the credential at all.
fn credential_id(credential_hash: &[u8; 32], ephemeral: &[u8], request: &str) -> Vec<u8> {
    let key = hmac::Key::new(hmac::HMAC_SHA256, credential_hash);
    let mut context = hmac::Context::with_key(&key);
    context.update(PROTOCOL);
    context.update(&(ephemeral.len() as u32).to_be_bytes());
    context.update(ephemeral);
    context.update(request.as_bytes());
    context.sign().as_ref().to_vec()
}

fn transcript(hello: &ClientHello, host_ephemeral: &[u8], certificate: &[u8]) -> [u8; 32] {
    let mut context = digest::Context::new(&digest::SHA256);
    context.update(PROTOCOL);
    for part in [
        &hello.ephemeral[..],
        &hello.credential_id[..],
        hello.request.as_bytes(),
        host_ephemeral,
        certificate,
    ] {
        context.update(&(part.len() as u32).to_be_bytes());
        context.update(part);
    }
    // Only ticket hellos add a part, so every other transcript is unchanged.
    if hello.ticket {
        context.update(b"ticket");
    }
    context
        .finish()
        .as_ref()
        .try_into()
        .expect("SHA-256 is 32 bytes")
}

struct Len(usize);
impl hkdf::KeyType for Len {
    fn len(&self) -> usize {
        self.0
    }
}

fn expand<const N: usize>(prk: &hkdf::Prk, info: &[&[u8]]) -> Result<[u8; N]> {
    let mut out = [0u8; N];
    prk.expand(info, Len(N))
        .and_then(|okm| okm.fill(&mut out))
        .map_err(|_| anyhow!("key derivation failed"))?;
    Ok(out)
}

/// Keys for one established session. `client_to_host` and `host_to_client`
/// are direction keys; each track derives its own key from them.
pub struct SessionKeys {
    session: [u8; 16],
    client_to_host: [u8; 32],
    host_to_client: [u8; 32],
}

impl SessionKeys {
    fn derive(shared: &[u8], credential_hash: &[u8; 32], transcript: &[u8; 32]) -> Result<Self> {
        let mut ikm = Vec::with_capacity(shared.len() + 32);
        ikm.extend_from_slice(shared);
        ikm.extend_from_slice(credential_hash);
        let prk = hkdf::Salt::new(hkdf::HKDF_SHA256, transcript).extract(&ikm);
        Ok(Self {
            session: expand(&prk, &[PROTOCOL, b" session"])?,
            client_to_host: expand(&prk, &[PROTOCOL, b" client to host"])?,
            host_to_client: expand(&prk, &[PROTOCOL, b" host to client"])?,
        })
    }

    /// Identifier both ends agree on; useful in logs (not secret, not a key).
    pub fn session_id(&self) -> [u8; 16] {
        self.session
    }

    fn track(&self, direction: Direction, track: &str) -> Result<TrackKey> {
        let base = match direction {
            Direction::ClientToHost => &self.client_to_host,
            Direction::HostToClient => &self.host_to_client,
        };
        let prk = hkdf::Prk::new_less_safe(hkdf::HKDF_SHA256, base);
        let raw: [u8; 32] = expand(&prk, &[PROTOCOL, b" track ", track.as_bytes()])?;
        let key = aead::UnboundKey::new(&aead::CHACHA20_POLY1305, &raw)
            .map_err(|_| anyhow!("invalid track key"))?;
        Ok(TrackKey {
            key: aead::LessSafeKey::new(key),
            session: self.session,
        })
    }

    /// Sealer for a track this side publishes.
    pub fn sealer(&self, direction: Direction, track: &str) -> Result<Sealer> {
        Ok(Sealer {
            key: self.track(direction, track)?,
            counter: 0,
        })
    }

    /// Opener for a track the peer publishes.
    pub fn opener(&self, direction: Direction, track: &str, order: Order) -> Result<Opener> {
        Ok(Opener {
            key: self.track(direction, track)?,
            order,
            newest: None,
            seen: std::collections::BTreeSet::new(),
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
    ClientToHost,
    HostToClient,
}

/// Replay policy for a track's objects.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Order {
    /// Every object must have a higher counter than the last accepted one.
    Strict,
    /// Reordering within [`REPLAY_WINDOW`] is accepted; repeats never are.
    Window,
}

struct TrackKey {
    key: aead::LessSafeKey,
    session: [u8; 16],
}

impl TrackKey {
    fn nonce(counter: u64) -> aead::Nonce {
        let mut nonce = [0u8; 12];
        nonce[4..].copy_from_slice(&counter.to_be_bytes());
        aead::Nonce::assume_unique_for_key(nonce)
    }

    fn aad(&self, group: u64, counter: u64) -> [u8; 32] {
        let mut aad = [0u8; 32];
        aad[..16].copy_from_slice(&self.session);
        aad[16..24].copy_from_slice(&group.to_be_bytes());
        aad[24..].copy_from_slice(&counter.to_be_bytes());
        aad
    }
}

pub struct Sealer {
    key: TrackKey,
    counter: u64,
}

impl Sealer {
    /// Seal one object for MoQ group `group` (the group sequence the relay
    /// preserves; datagrams use their own sequence).
    pub fn seal(&mut self, group: u64, plaintext: &[u8]) -> Result<Vec<u8>> {
        let counter = self.counter;
        self.counter = counter.checked_add(1).context("track counter exhausted")?;
        let mut out = Vec::with_capacity(COUNTER_LEN + plaintext.len() + aead::MAX_TAG_LEN);
        out.extend_from_slice(&counter.to_be_bytes());
        let mut body = plaintext.to_vec();
        self.key
            .key
            .seal_in_place_append_tag(
                TrackKey::nonce(counter),
                aead::Aad::from(self.key.aad(group, counter)),
                &mut body,
            )
            .map_err(|_| anyhow!("seal failed"))?;
        out.extend_from_slice(&body);
        Ok(out)
    }
}

pub struct Opener {
    key: TrackKey,
    order: Order,
    newest: Option<u64>,
    /// Counters accepted within the replay window (unordered tracks).
    seen: std::collections::BTreeSet<u64>,
}

impl Opener {
    pub fn open(&mut self, group: u64, sealed: &[u8]) -> Result<Vec<u8>> {
        ensure!(
            sealed.len() >= COUNTER_LEN + aead::MAX_TAG_LEN,
            "sealed object too short"
        );
        let counter = u64::from_be_bytes(sealed[..COUNTER_LEN].try_into().unwrap());
        match (self.order, self.newest) {
            (Order::Strict, newest) => {
                // Contiguous from zero: the relay cannot drop, replay or
                // reorder anything on an ordered track without detection.
                ensure!(
                    counter == newest.map_or(0, |newest| newest + 1),
                    "missing, replayed or reordered object on ordered track"
                )
            }
            (Order::Window, Some(newest)) => {
                ensure!(
                    counter.saturating_add(REPLAY_WINDOW) > newest && !self.seen.contains(&counter),
                    "replayed or stale object"
                )
            }
            (_, None) => {}
        }
        let mut body = sealed[COUNTER_LEN..].to_vec();
        let plaintext_len = self
            .key
            .key
            .open_in_place(
                TrackKey::nonce(counter),
                aead::Aad::from(self.key.aad(group, counter)),
                &mut body,
            )
            .map_err(|_| anyhow!("object failed authentication"))?
            .len();
        body.truncate(plaintext_len);
        // Only authenticated objects advance replay state.
        self.newest = Some(self.newest.map_or(counter, |newest| newest.max(counter)));
        if self.order == Order::Window {
            self.seen.insert(counter);
            let floor = self.newest.unwrap().saturating_sub(REPLAY_WINDOW);
            self.seen = self.seen.split_off(&floor);
        }
        Ok(body)
    }
}

/// Client half of the handshake.
pub struct ClientHandshake {
    secret: agreement::EphemeralPrivateKey,
    hello: ClientHello,
    credential_hash: [u8; 32],
    device: Option<DeviceKey>,
    /// A ticket hello's ticket, sent sealed in the confirmation.
    ticket: Option<String>,
}

/// A client's verified handshake: the session keys and the confirmation
/// objects to send to the host (in order, in one group).
pub struct Established {
    pub keys: SessionKeys,
    pub confirmation: Vec<Vec<u8>>,
}

impl ClientHandshake {
    /// `device` is the credential's device key, if it was enrolled with one;
    /// the confirmation then carries its transcript signature.
    pub fn new(token: &str, request: &str, device: Option<DeviceKey>) -> Result<Self> {
        Self::from_hash(credential_hash(token), request, device)
    }

    /// Everything a client computes needs only the hash: this is also what an
    /// attacker holding the host's records can do (tests use it as such).
    pub(crate) fn from_hash(
        credential_hash: [u8; 32],
        request: &str,
        device: Option<DeviceKey>,
    ) -> Result<Self> {
        let rng = SystemRandom::new();
        let secret = agreement::EphemeralPrivateKey::generate(&agreement::X25519, &rng)
            .map_err(|_| anyhow!("ephemeral key generation failed"))?;
        let ephemeral = secret
            .compute_public_key()
            .map_err(|_| anyhow!("ephemeral public key failed"))?
            .as_ref()
            .to_vec();
        let hello = ClientHello {
            credential_id: credential_id(&credential_hash, &ephemeral, request),
            ephemeral,
            request: request.to_owned(),
            ticket: false,
        };
        Ok(Self {
            secret,
            hello,
            credential_hash,
            device,
            ticket: None,
        })
    }

    /// A ticket hello: `ticket` (`tkt1...`) authorizes the session, `device`
    /// is the client key the ticket names (`cnf`), which signs the transcript.
    pub fn for_ticket(ticket: &str, request: &str, device: DeviceKey) -> Result<Self> {
        ensure!(
            ticket.starts_with(crate::ticket::PREFIX),
            "not a session ticket"
        );
        let mut handshake = Self::from_hash(TICKET_HELLO, request, Some(device))?;
        handshake.hello.ticket = true;
        handshake.ticket = Some(ticket.to_owned());
        Ok(handshake)
    }

    pub fn hello(&self) -> &ClientHello {
        &self.hello
    }

    /// Verify the host against the pinned certificate fingerprint (hex
    /// SHA-256 of the certificate DER, as in pairing files), derive keys and
    /// build the key confirmation (with the device signature, if keyed).
    pub fn finish(self, reply: &HostReply, pinned_fingerprint: &str) -> Result<Established> {
        let pinned = moq_native::tls::parse_fingerprint(pinned_fingerprint)
            .map_err(|error| anyhow!("invalid pinned fingerprint: {error}"))?;
        let actual = digest::digest(&digest::SHA256, &reply.certificate);
        ensure!(
            bool::from(actual.as_ref().ct_eq(pinned.as_ref())),
            "host certificate does not match the approved fingerprint"
        );
        let (_, certificate) = x509_parser::parse_x509_certificate(&reply.certificate)
            .map_err(|_| anyhow!("host certificate is not valid X.509"))?;
        let public_key = &certificate.public_key().subject_public_key.data;
        let transcript = transcript(&self.hello, &reply.ephemeral, &reply.certificate);
        let mut signed = HOST_SIGNATURE_LABEL.to_vec();
        signed.extend_from_slice(&transcript);
        signature::UnparsedPublicKey::new(&signature::ECDSA_P256_SHA256_ASN1, public_key)
            .verify(&signed, &reply.signature)
            .map_err(|_| anyhow!("host transcript signature is invalid"))?;
        let shared = agreement::agree_ephemeral(
            self.secret,
            &agreement::UnparsedPublicKey::new(&agreement::X25519, &reply.ephemeral),
            |shared| shared.to_vec(),
        )
        .map_err(|_| anyhow!("key agreement failed"))?;
        let keys = SessionKeys::derive(&shared, &self.credential_hash, &transcript)?;
        // Sealed so the relay cannot link the signature to a device key.
        let mut sealer = keys.sealer(Direction::ClientToHost, CONFIRM_TRACK)?;
        let mut confirmation = vec![sealer.seal(0, &keys.session_id())?];
        if let Some(ticket) = &self.ticket {
            confirmation.push(sealer.seal(0, ticket.as_bytes())?);
        }
        if let Some(device) = &self.device {
            confirmation.push(sealer.seal(0, &device.sign_transcript(&transcript))?);
        }
        Ok(Established { keys, confirmation })
    }
}

/// A host's answered handshake. Its keys are released only by
/// [`PendingSession::confirm`], so nothing can be accepted from a client that
/// has not confirmed them (and, for a keyed device, signed the transcript).
#[cfg(target_os = "linux")]
pub struct PendingSession {
    keys: SessionKeys,
    transcript: [u8; 32],
}

#[cfg(target_os = "linux")]
impl PendingSession {
    /// Confirmation objects to read for a credential with or without a
    /// stored device key.
    pub fn expected_objects(device_key: Option<&[u8]>) -> usize {
        1 + usize::from(device_key.is_some())
    }

    /// Check the client's confirmation. `device_key` is the public key the
    /// host stored for the credential: when present, a valid transcript
    /// signature by it is required; knowing the credential hash is not enough.
    pub fn confirm(self, objects: &[&[u8]], device_key: Option<&[u8]>) -> Result<SessionKeys> {
        let mut opener = self
            .keys
            .opener(Direction::ClientToHost, CONFIRM_TRACK, Order::Strict)?;
        let first = objects.first().context("missing key confirmation")?;
        let opened = opener.open(0, first)?;
        ensure!(
            bool::from(opened.ct_eq(&self.keys.session_id())),
            "key confirmation mismatch"
        );
        if let Some(public) = device_key {
            ensure!(
                public.len() == DEVICE_PUBLIC_KEY_LEN,
                "invalid stored device key"
            );
            let sealed = objects.get(1).context("device did not sign the session")?;
            let proof = opener.open(0, sealed)?;
            ensure!(
                proof.len() == DEVICE_SIGNATURE_LEN,
                "malformed device signature"
            );
            signature::UnparsedPublicKey::new(&signature::ED25519, public)
                .verify(&device_signed(&self.transcript), &proof)
                .map_err(|_| anyhow!("device signature is invalid"))?;
        }
        Ok(self.keys)
    }
}

#[cfg(target_os = "linux")]
impl PendingSession {
    /// Objects in a ticket hello's confirmation: the session id, the
    /// ticket and the `cnf` key's transcript signature.
    pub const TICKET_OBJECTS: usize = 3;

    /// Check a ticket hello's confirmation. `redeem` is given the ticket and
    /// a check that a key signed this session's transcript; it must verify the
    /// ticket (issuer, audience, lifetime, single use) and require that check
    /// for the ticket's `cnf` key. Keys are released only if it succeeds.
    pub fn confirm_ticket<T>(
        self,
        objects: &[&[u8]],
        redeem: impl FnOnce(&str, &dyn Fn(&[u8; 32]) -> bool) -> Result<T>,
    ) -> Result<(SessionKeys, T)> {
        let mut opener = self
            .keys
            .opener(Direction::ClientToHost, CONFIRM_TRACK, Order::Strict)?;
        let first = objects.first().context("missing key confirmation")?;
        ensure!(
            bool::from(opener.open(0, first)?.ct_eq(&self.keys.session_id())),
            "key confirmation mismatch"
        );
        let ticket = opener.open(0, objects.get(1).context("missing session ticket")?)?;
        let ticket = std::str::from_utf8(&ticket).context("malformed session ticket")?;
        let proof = opener.open(
            0,
            objects.get(2).context("client did not sign the session")?,
        )?;
        ensure!(
            proof.len() == DEVICE_SIGNATURE_LEN,
            "malformed client signature"
        );
        let signed = device_signed(&self.transcript);
        let proves = |key: &[u8; 32]| {
            signature::UnparsedPublicKey::new(&signature::ED25519, key)
                .verify(&signed, &proof)
                .is_ok()
        };
        let granted = redeem(ticket, &proves)?;
        Ok((self.keys, granted))
    }
}

/// Verify a `HostIdentity::sign_labeled` signature: `certificate` must be the
/// one the client pins (SHA-256 of its DER equals `pinned_fingerprint`).
pub fn verify_labeled(
    certificate: &[u8],
    pinned_fingerprint: &str,
    label: &[u8],
    message: &[u8],
    signature_bytes: &[u8],
) -> Result<()> {
    ensure!(
        !label.is_empty() && label != HOST_SIGNATURE_LABEL,
        "signature label must be specific to its use"
    );
    let pinned = moq_native::tls::parse_fingerprint(pinned_fingerprint)
        .map_err(|error| anyhow!("invalid pinned fingerprint: {error}"))?;
    let actual = digest::digest(&digest::SHA256, certificate);
    ensure!(
        bool::from(actual.as_ref().ct_eq(pinned.as_ref())),
        "host certificate does not match the approved fingerprint"
    );
    let (_, parsed) = x509_parser::parse_x509_certificate(certificate)
        .map_err(|_| anyhow!("host certificate is not valid X.509"))?;
    let mut signed = label.to_vec();
    signed.extend_from_slice(message);
    signature::UnparsedPublicKey::new(
        &signature::ECDSA_P256_SHA256_ASN1,
        &parsed.public_key().subject_public_key.data,
    )
    .verify(&signed, signature_bytes)
    .map_err(|_| anyhow!("host signature is invalid"))
}

/// Host identity used to answer handshakes: the TLS certificate the client
/// pins and its P-256 private key.
#[cfg(target_os = "linux")]
pub struct HostIdentity {
    certificate: Vec<u8>,
    key: signature::EcdsaKeyPair,
}

#[cfg(target_os = "linux")]
impl HostIdentity {
    /// `pkcs8` is the DER private key from the identity directory's key.pem.
    pub fn new(certificate: Vec<u8>, pkcs8: &[u8]) -> Result<Self> {
        let key = signature::EcdsaKeyPair::from_pkcs8(
            &signature::ECDSA_P256_SHA256_ASN1_SIGNING,
            pkcs8,
            &SystemRandom::new(),
        )
        .map_err(|_| anyhow!("host identity key is not a P-256 PKCS#8 key"))?;
        let (_, parsed) = x509_parser::parse_x509_certificate(&certificate)
            .map_err(|_| anyhow!("host certificate is not valid X.509"))?;
        ensure!(
            parsed.public_key().subject_public_key.data.as_ref() == key.public_key().as_ref(),
            "host key does not match its certificate"
        );
        Ok(Self { certificate, key })
    }

    /// The certificate clients pin (DER).
    pub fn certificate(&self) -> &[u8] {
        &self.certificate
    }

    /// Sign `label ‖ message` with the host key, for statements other than
    /// the handshake transcript (e.g. relay presence). Each use needs its own
    /// label, distinct from the transcript's, so signatures never cross over.
    pub fn sign_labeled(&self, label: &[u8], message: &[u8]) -> Result<Vec<u8>> {
        ensure!(
            !label.is_empty() && label != HOST_SIGNATURE_LABEL,
            "signature label must be specific to its use"
        );
        let mut signed = label.to_vec();
        signed.extend_from_slice(message);
        Ok(self
            .key
            .sign(&SystemRandom::new(), &signed)
            .map_err(|_| anyhow!("signing failed"))?
            .as_ref()
            .to_vec())
    }

    /// Return which candidate credential (by index) the hello names, if any.
    /// Every candidate is compared, so timing does not reveal the position.
    pub fn identify(hello: &ClientHello, candidates: &[[u8; 32]]) -> Option<usize> {
        let mut found = None;
        for (index, candidate) in candidates.iter().enumerate() {
            let expected = credential_id(candidate, &hello.ephemeral, &hello.request);
            if bool::from(expected.ct_eq(&hello.credential_id)) {
                found = Some(index);
            }
        }
        found
    }

    /// Answer a hello whose credential has been identified and authorized.
    /// The reply reveals nothing secret; the session keys stay inside the
    /// [`PendingSession`] until the client confirms them.
    pub fn respond(
        &self,
        hello: &ClientHello,
        credential_hash: &[u8; 32],
    ) -> Result<(HostReply, PendingSession)> {
        ensure!(
            hello.ephemeral.len() == 32
                && hello.credential_id.len() == 32
                && hello.request.len() <= 128,
            "malformed client hello"
        );
        let rng = SystemRandom::new();
        let secret = agreement::EphemeralPrivateKey::generate(&agreement::X25519, &rng)
            .map_err(|_| anyhow!("ephemeral key generation failed"))?;
        let ephemeral = secret
            .compute_public_key()
            .map_err(|_| anyhow!("ephemeral public key failed"))?
            .as_ref()
            .to_vec();
        let transcript = transcript(hello, &ephemeral, &self.certificate);
        let mut signed = HOST_SIGNATURE_LABEL.to_vec();
        signed.extend_from_slice(&transcript);
        let signature = self
            .key
            .sign(&rng, &signed)
            .map_err(|_| anyhow!("transcript signing failed"))?
            .as_ref()
            .to_vec();
        let shared = agreement::agree_ephemeral(
            secret,
            &agreement::UnparsedPublicKey::new(&agreement::X25519, &hello.ephemeral),
            |shared| shared.to_vec(),
        )
        .map_err(|_| anyhow!("key agreement failed"))?;
        let keys = SessionKeys::derive(&shared, credential_hash, &transcript)?;
        Ok((
            HostReply {
                ephemeral,
                certificate: self.certificate.clone(),
                signature,
            },
            PendingSession { keys, transcript },
        ))
    }
}

/// Random bytes for relay namespaces and the like.
pub fn random<const N: usize>() -> [u8; N] {
    let mut out = [0u8; N];
    SystemRandom::new()
        .fill(&mut out)
        .expect("system randomness unavailable");
    out
}

/// A matched (client, host) key pair from a real handshake, for tests of
/// code built on top of this module.
#[cfg(all(test, target_os = "linux"))]
pub fn test_session() -> (SessionKeys, SessionKeys) {
    let certified = rcgen::generate_simple_self_signed(vec!["teleport.local".into()]).unwrap();
    let der = certified.cert.der().to_vec();
    let fingerprint: String = digest::digest(&digest::SHA256, &der)
        .as_ref()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    let host = HostIdentity::new(der, &certified.signing_key.serialize_der()).unwrap();
    let client = ClientHandshake::new("token", "", None).unwrap();
    let (reply, pending) = host
        .respond(client.hello(), &credential_hash("token"))
        .unwrap();
    let established = client.finish(&reply, &fingerprint).unwrap();
    let objects: Vec<&[u8]> = established.confirmation.iter().map(Vec::as_slice).collect();
    let host_keys = pending.confirm(&objects, None).unwrap();
    (established.keys, host_keys)
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;

    /// A ticket hello: the ticket travels only inside the sealed
    /// confirmation, and the host learns which key signed the transcript.
    /// A confirmation signed by another key, or naming another ticket, is
    /// refused before any keys are released.
    #[cfg(target_os = "linux")]
    #[test]
    fn ticket_hellos_seal_the_ticket_and_prove_its_key() -> Result<()> {
        let certified = rcgen::generate_simple_self_signed(vec!["teleport.local".into()])?;
        let der = certified.cert.der().to_vec();
        let fingerprint: String = digest::digest(&digest::SHA256, &der)
            .as_ref()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        let host = HostIdentity::new(der, &certified.signing_key.serialize_der())?;
        let ticket = "tkt1.payload.signature";
        let run = |signer: DeviceKey, expected: [u8; 32]| -> Result<Option<String>> {
            let client = ClientHandshake::for_ticket(ticket, "codec=h264", signer)?;
            assert!(client.hello().ticket);
            let hello_json = serde_json::to_string(client.hello())?;
            assert!(!hello_json.contains("tkt1"), "the hello leaks the ticket");
            let (reply, pending) = host.respond(client.hello(), &TICKET_HELLO)?;
            let established = client.finish(&reply, &fingerprint)?;
            assert_eq!(
                established.confirmation.len(),
                PendingSession::TICKET_OBJECTS
            );
            assert!(
                established
                    .confirmation
                    .iter()
                    .all(|o| !o.windows(4).any(|w| w == b"tkt1"))
            );
            let objects: Vec<&[u8]> = established.confirmation.iter().map(Vec::as_slice).collect();
            Ok(pending
                .confirm_ticket(&objects, |seen, proves| {
                    ensure!(proves(&expected), "the ticket's key did not sign");
                    Ok(seen.to_owned())
                })
                .ok()
                .map(|(_, seen)| seen))
        };
        let (key, _) = DeviceKey::generate()?;
        let public = key.public_key();
        assert_eq!(run(key, public)?.as_deref(), Some(ticket));
        let (other, _) = DeviceKey::generate()?;
        assert_eq!(run(other, public)?, None);
        // Not a ticket.
        assert!(ClientHandshake::for_ticket("nope", "", DeviceKey::generate()?.0).is_err());
        Ok(())
    }

    struct Fixture {
        host: HostIdentity,
        fingerprint: String,
        other_fingerprint: String,
    }

    fn fixture() -> Fixture {
        let make = || {
            let certified =
                rcgen::generate_simple_self_signed(vec!["teleport.local".into()]).unwrap();
            let der = certified.cert.der().to_vec();
            let fingerprint: String = digest::digest(&digest::SHA256, &der)
                .as_ref()
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect();
            (der, certified.signing_key.serialize_der(), fingerprint)
        };
        let (certificate, key, fingerprint) = make();
        let (_, _, other_fingerprint) = make();
        Fixture {
            host: HostIdentity::new(certificate, &key).unwrap(),
            fingerprint,
            other_fingerprint,
        }
    }

    fn handshake(fixture: &Fixture, token: &str) -> (SessionKeys, SessionKeys) {
        let client = ClientHandshake::new(token, "codec=h264", None).unwrap();
        let candidates = [credential_hash("someone else"), credential_hash(token)];
        let index = HostIdentity::identify(client.hello(), &candidates).unwrap();
        let (reply, pending) = fixture
            .host
            .respond(client.hello(), &candidates[index])
            .unwrap();
        let established = client.finish(&reply, &fixture.fingerprint).unwrap();
        let host_keys = confirm(pending, &established.confirmation, None).unwrap();
        (established.keys, host_keys)
    }

    fn confirm(
        pending: PendingSession,
        objects: &[Vec<u8>],
        device_key: Option<&[u8]>,
    ) -> Result<SessionKeys> {
        let objects: Vec<&[u8]> = objects.iter().map(Vec::as_slice).collect();
        pending.confirm(&objects, device_key)
    }

    /// One relayed handshake attempt, as far as the host is concerned: the
    /// client side is built from `credential_hash` alone (what an attacker
    /// with the host's records has) plus an optional device key.
    fn attempt(
        fixture: &Fixture,
        credential: [u8; 32],
        device: Option<DeviceKey>,
        stored_key: Option<&[u8]>,
    ) -> Result<(SessionKeys, SessionKeys)> {
        let client = ClientHandshake::from_hash(credential, "codec=h264", device)?;
        let index = HostIdentity::identify(client.hello(), &[credential]).context("unknown")?;
        assert_eq!(index, 0);
        let (reply, pending) = fixture.host.respond(client.hello(), &credential)?;
        let established = client.finish(&reply, &fixture.fingerprint)?;
        let host = confirm(pending, &established.confirmation, stored_key)?;
        Ok((established.keys, host))
    }

    #[test]
    fn keyed_device_handshake_agrees_and_signature_stays_sealed() {
        let fixture = fixture();
        let (device, _) = DeviceKey::generate().unwrap();
        let public = device.public_key();
        let client = ClientHandshake::new("device-token", "codec=h264", Some(device)).unwrap();
        let (reply, pending) = fixture
            .host
            .respond(client.hello(), &credential_hash("device-token"))
            .unwrap();
        let established = client.finish(&reply, &fixture.fingerprint).unwrap();
        assert_eq!(established.confirmation.len(), 2);
        assert_eq!(PendingSession::expected_objects(Some(&public)), 2);
        assert_eq!(PendingSession::expected_objects(None), 1);
        let host = confirm(pending, &established.confirmation, Some(&public)).unwrap();
        assert_eq!(established.keys.session_id(), host.session_id());
        let sealed = established
            .keys
            .sealer(Direction::ClientToHost, "input")
            .unwrap()
            .seal(0, b"x")
            .unwrap();
        assert_eq!(
            host.opener(Direction::ClientToHost, "input", Order::Strict)
                .unwrap()
                .open(0, &sealed)
                .unwrap(),
            b"x"
        );
    }

    #[test]
    fn stored_hash_alone_cannot_complete_a_keyed_devices_handshake() {
        let fixture = fixture();
        // What the host keeps for an enrolled device: SHA-256(token) and the
        // device's public key. The attacker has both, but not the token or
        // the device's private key.
        let (device, _) = DeviceKey::generate().unwrap();
        let public = device.public_key();
        let stored_hash = credential_hash("device-token");
        // Sanity: the genuine device gets in.
        assert!(attempt(&fixture, stored_hash, Some(device), Some(&public)).is_ok());
        // The hash alone produces a valid identifier and a host reply...
        let error = attempt(&fixture, stored_hash, None, Some(&public))
            .err()
            .expect("hash-only client accepted for a keyed device");
        assert!(error.to_string().contains("did not sign"), "{error:#}");
        // ...and signing with the attacker's own key does not help.
        let (own, _) = DeviceKey::generate().unwrap();
        let error = attempt(&fixture, stored_hash, Some(own), Some(&public))
            .err()
            .expect("foreign device key accepted");
        assert!(
            error.to_string().contains("signature is invalid"),
            "{error:#}"
        );
    }

    #[test]
    fn tampered_or_transplanted_device_signatures_fail() {
        let fixture = fixture();
        let (device, seed) = DeviceKey::generate().unwrap();
        let public = device.public_key();
        let credential = credential_hash("device-token");
        type Mutate<'a> = &'a dyn Fn(&mut Vec<Vec<u8>>, &SessionKeys, &[u8; 32]);
        let run = |mutate: Mutate| {
            let device = DeviceKey::from_hex(&seed).unwrap();
            let client = ClientHandshake::from_hash(credential, "codec=h264", Some(device))?;
            let (reply, pending) = fixture.host.respond(client.hello(), &credential)?;
            let transcript = pending.transcript;
            let mut established = client.finish(&reply, &fixture.fingerprint)?;
            mutate(
                &mut established.confirmation,
                &established.keys,
                &transcript,
            );
            confirm(pending, &established.confirmation, Some(&public))
        };
        assert!(run(&|_, _, _| {}).is_ok());
        // Bit flip in the sealed signature.
        assert!(run(&|objects, _, _| *objects[1].last_mut().unwrap() ^= 1).is_err());
        // Signature missing, duplicated or swapped with the confirmation.
        assert!(run(&|objects, _, _| objects.truncate(1)).is_err());
        assert!(run(&|objects, _, _| objects[1] = objects[0].clone()).is_err());
        assert!(run(&|objects, _, _| objects.swap(0, 1)).is_err());
        assert!(run(&|objects, _, _| objects.clear()).is_err());
        // A genuine device signature over a different transcript (another
        // session, another request, another host), re-sealed by someone who
        // knows this session's keys, is rejected.
        let other = |transcript: &[u8; 32]| {
            let mut changed = *transcript;
            changed[0] ^= 1;
            DeviceKey::from_hex(&seed)
                .unwrap()
                .sign_transcript(&changed)
        };
        assert!(
            run(&|objects, keys, transcript| {
                let mut sealer = keys.sealer(Direction::ClientToHost, CONFIRM_TRACK).unwrap();
                *objects = vec![
                    sealer.seal(0, &keys.session_id()).unwrap(),
                    sealer.seal(0, &other(transcript)).unwrap(),
                ];
            })
            .is_err()
        );
        // The correct signature re-sealed the same way is fine (the check
        // above failed on the signature, not on the sealing).
        assert!(
            run(&|objects, keys, transcript| {
                let device = DeviceKey::from_hex(&seed).unwrap();
                let mut sealer = keys.sealer(Direction::ClientToHost, CONFIRM_TRACK).unwrap();
                *objects = vec![
                    sealer.seal(0, &keys.session_id()).unwrap(),
                    sealer.seal(0, &device.sign_transcript(transcript)).unwrap(),
                ];
            })
            .is_ok()
        );
        // A signature captured from one session never fits another, even
        // for the same device and request: the transcript covers both
        // ephemerals.
        let client = ClientHandshake::from_hash(
            credential,
            "codec=h264",
            Some(DeviceKey::from_hex(&seed).unwrap()),
        )
        .unwrap();
        let (reply, _) = fixture.host.respond(client.hello(), &credential).unwrap();
        let first = client.finish(&reply, &fixture.fingerprint).unwrap();
        let captured_signature = first
            .keys
            .opener(Direction::ClientToHost, CONFIRM_TRACK, Order::Strict)
            .map(|mut opener| {
                opener.open(0, &first.confirmation[0]).unwrap();
                opener.open(0, &first.confirmation[1]).unwrap()
            })
            .unwrap();
        assert!(
            run(&|objects, keys, _| {
                let mut sealer = keys.sealer(Direction::ClientToHost, CONFIRM_TRACK).unwrap();
                *objects = vec![
                    sealer.seal(0, &keys.session_id()).unwrap(),
                    sealer.seal(0, &captured_signature).unwrap(),
                ];
            })
            .is_err()
        );
        // A malformed stored key fails closed.
        let client = ClientHandshake::from_hash(credential, "codec=h264", None).unwrap();
        let (reply, pending) = fixture.host.respond(client.hello(), &credential).unwrap();
        let established = client.finish(&reply, &fixture.fingerprint).unwrap();
        assert!(confirm(pending, &established.confirmation, Some(&[0; 31])).is_err());
    }

    #[test]
    fn legacy_devices_keep_hash_authentication_both_ways() {
        let fixture = fixture();
        let credential = credential_hash("legacy-token");
        // Old client (no key) to a host that stored no key.
        assert!(attempt(&fixture, credential, None, None).is_ok());
        // A keyed client to a host that never registered its key (an older
        // host, or a lost acknowledgement): the extra object is ignored.
        let (device, _) = DeviceKey::generate().unwrap();
        let (client, host) = attempt(&fixture, credential, Some(device), None).unwrap();
        assert_eq!(client.session_id(), host.session_id());
        // An unkeyed confirmation still has to be right.
        let client = ClientHandshake::from_hash(credential, "codec=h264", None).unwrap();
        let (reply, pending) = fixture.host.respond(client.hello(), &credential).unwrap();
        let mut established = client.finish(&reply, &fixture.fingerprint).unwrap();
        *established.confirmation[0].last_mut().unwrap() ^= 1;
        assert!(confirm(pending, &established.confirmation, None).is_err());
    }

    #[test]
    fn device_keys_roundtrip_through_hex_and_reject_garbage() {
        let (key, hex) = DeviceKey::generate().unwrap();
        assert_eq!(
            DeviceKey::from_hex(&hex).unwrap().public_key(),
            key.public_key()
        );
        assert_ne!(DeviceKey::generate().unwrap().1, hex);
        for bad in ["", "00", &"g".repeat(64), &"0".repeat(63), &"0".repeat(66)] {
            assert!(DeviceKey::from_hex(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn device_key_registration_is_bound_to_its_enrollment() {
        let (key, _) = DeviceKey::generate().unwrap();
        let client = Registration::new(b"enrollment secret", b"context").unwrap();
        let host = Registration::new(b"enrollment secret", b"context").unwrap();
        let request = client.request(&key);
        assert!(
            !request
                .windows(DEVICE_PUBLIC_KEY_LEN)
                .any(|w| w == key.public_key())
        );
        let public = host.open_request(&request).unwrap();
        assert_eq!(public, key.public_key());
        let issued = host.seal_credential(Some(&public), b"credential");
        let opened = client.open_credential(&key, &issued).unwrap();
        assert!(opened.keyed && opened.credential == b"credential");
        // The host's own token: explicitly unkeyed, still authenticated.
        let unkeyed = client
            .open_credential(&key, &host.seal_credential(None, b"host token"))
            .unwrap();
        assert!(!unkeyed.keyed && unkeyed.credential == b"host token");
        // Directions are distinct: a reflected request is not a credential.
        assert!(client.open_credential(&key, &request).is_err());
        assert!(host.open_request(&issued).is_err());
        // Another enrollment (secret or context) cannot inject a key or a
        // credential.
        for (secret, context) in [
            (&b"other secret"[..], &b"context"[..]),
            (b"enrollment secret", b"other"),
        ] {
            let other = Registration::new(secret, context).unwrap();
            assert!(other.open_request(&request).is_err());
            for stored in [Some(&public), None] {
                assert!(
                    client
                        .open_credential(&key, &other.seal_credential(stored, b"credential"))
                        .is_err()
                );
            }
        }
        // Tampering, truncation and a credential bound to another key fail.
        let mut tampered = request.clone();
        tampered[20] ^= 1;
        assert!(host.open_request(&tampered).is_err());
        assert!(host.open_request(&request[..request.len() - 1]).is_err());
        let mut tampered = issued.clone();
        tampered[12] ^= 1;
        assert!(client.open_credential(&key, &tampered).is_err());
        let (other_key, _) = DeviceKey::generate().unwrap();
        assert!(
            client
                .open_credential(
                    &key,
                    &host.seal_credential(Some(&other_key.public_key()), b"credential")
                )
                .is_err()
        );
    }

    #[test]
    fn handshake_agrees_and_objects_roundtrip_both_directions() {
        let fixture = fixture();
        let (client, host) = handshake(&fixture, "device-token");
        assert_eq!(client.session_id(), host.session_id());
        let mut up = client.sealer(Direction::ClientToHost, "input").unwrap();
        let mut up_open = host
            .opener(Direction::ClientToHost, "input", Order::Strict)
            .unwrap();
        let mut down = host.sealer(Direction::HostToClient, "h264").unwrap();
        let mut down_open = client
            .opener(Direction::HostToClient, "h264", Order::Window)
            .unwrap();
        for group in 0..3 {
            let sealed = up.seal(group, b"key press").unwrap();
            assert!(
                !sealed.windows(9).any(|w| w == b"key press"),
                "plaintext leaked"
            );
            assert_eq!(up_open.open(group, &sealed).unwrap(), b"key press");
            let frame = vec![group as u8; 5000];
            assert_eq!(
                down_open
                    .open(group, &down.seal(group, &frame).unwrap())
                    .unwrap(),
                frame
            );
        }
    }

    #[test]
    fn hello_never_contains_the_token_and_is_unlinkable() {
        let token = "0123456789abcdef0123456789abcdef";
        let first = ClientHandshake::new(token, "codec=h264", None).unwrap();
        let second = ClientHandshake::new(token, "codec=h264", None).unwrap();
        let wire = serde_json::to_vec(first.hello()).unwrap();
        assert!(!wire.windows(token.len()).any(|w| w == token.as_bytes()));
        assert_ne!(first.hello().credential_id, second.hello().credential_id);
    }

    #[test]
    fn unknown_credential_is_not_identified() {
        let client = ClientHandshake::new("revoked or never issued", "codec=h264", None).unwrap();
        let candidates = [credential_hash("a"), credential_hash("b")];
        assert_eq!(HostIdentity::identify(client.hello(), &candidates), None);
    }

    #[test]
    fn client_rejects_wrong_host_or_forged_reply() {
        let fixture = fixture();
        let client = ClientHandshake::new("token", "codec=h264", None).unwrap();
        let (reply, _) = fixture
            .host
            .respond(client.hello(), &credential_hash("token"))
            .unwrap();
        // A relay (or anyone) presenting a different, unpinned certificate.
        let client2 = ClientHandshake::new("token", "codec=h264", None).unwrap();
        let (reply2, _) = fixture
            .host
            .respond(client2.hello(), &credential_hash("token"))
            .unwrap();
        assert!(
            ClientHandshake::new("token", "codec=h264", None)
                .unwrap()
                .finish(&reply, &fixture.other_fingerprint)
                .is_err()
        );
        // A reply signed for another hello cannot be replayed to this client.
        assert!(client.finish(&reply2, &fixture.fingerprint).is_err());
        // A substituted ephemeral key breaks the signature.
        let fresh = ClientHandshake::new("token", "codec=h264", None).unwrap();
        let (mut forged, _) = fixture
            .host
            .respond(fresh.hello(), &credential_hash("token"))
            .unwrap();
        forged.ephemeral = random::<32>().to_vec();
        assert!(fresh.finish(&forged, &fixture.fingerprint).is_err());
    }

    #[test]
    fn client_detects_a_relay_altering_its_request() {
        let fixture = fixture();
        let client = ClientHandshake::new("token", "codec=h264", None).unwrap();
        let mut altered = client.hello().clone();
        altered.request = "codec=h264&ssh-agent=1".into();
        // The host no longer recognises the credential at all...
        assert_eq!(
            HostIdentity::identify(&altered, &[credential_hash("token")]),
            None
        );
        // ...and even a host that answered anyway would be rejected.
        let (reply, _) = fixture
            .host
            .respond(&altered, &credential_hash("token"))
            .unwrap();
        assert!(client.finish(&reply, &fixture.fingerprint).is_err());
    }

    #[test]
    fn wrong_credential_cannot_read_or_write_the_session() {
        let fixture = fixture();
        let client = ClientHandshake::new("guess", "codec=h264", None).unwrap();
        // Host answered as if the hello were for a different credential.
        let (reply, host) = fixture
            .host
            .respond(client.hello(), &credential_hash("real"))
            .unwrap();
        let client = client.finish(&reply, &fixture.fingerprint).unwrap();
        // The host's pending keys (reachable here only because tests see
        // private fields) cannot open what the client seals...
        let sealed = client
            .keys
            .sealer(Direction::ClientToHost, "input")
            .unwrap()
            .seal(0, b"x")
            .unwrap();
        assert!(
            host.keys
                .opener(Direction::ClientToHost, "input", Order::Strict)
                .unwrap()
                .open(0, &sealed)
                .is_err()
        );
        // ...and its confirmation fails, so they are never released.
        assert!(confirm(host, &client.confirmation, None).is_err());
    }

    #[test]
    fn tamper_splice_replay_and_reorder_are_rejected() {
        let fixture = fixture();
        let (client, host) = handshake(&fixture, "token");
        let mut input = client.sealer(Direction::ClientToHost, "input").unwrap();
        let first = input.seal(0, b"a").unwrap();
        let second = input.seal(0, b"b").unwrap();
        let open = |track: &str, order| host.opener(Direction::ClientToHost, track, order).unwrap();

        let mut tampered = first.clone();
        *tampered.last_mut().unwrap() ^= 1;
        assert!(open("input", Order::Strict).open(0, &tampered).is_err());
        // Same bytes on another track, group, or direction fail.
        assert!(open("pointer", Order::Window).open(0, &first).is_err());
        assert!(open("input", Order::Strict).open(1, &first).is_err());
        assert!(
            client
                .opener(Direction::ClientToHost, "input", Order::Strict)
                .unwrap()
                .open(0, &first)
                .is_ok()
        );
        assert!(
            host.opener(Direction::HostToClient, "input", Order::Strict)
                .unwrap()
                .open(0, &first)
                .is_err()
        );
        // Ordered track: replay and reordering fail; failure does not advance.
        let mut strict = open("input", Order::Strict);
        assert!(strict.open(0, &tampered).is_err());
        strict.open(0, &first).unwrap();
        assert!(strict.open(0, &first).is_err());
        strict.open(0, &second).unwrap();
        // Dropping the first object (a gap) is detected too.
        let mut gapped = open("input", Order::Strict);
        assert!(gapped.open(0, &second).is_err());
        // Unordered track: reordering is fine, repeats are not.
        let mut video = client.sealer(Direction::ClientToHost, "pointer").unwrap();
        let objects: Vec<_> = (0..4).map(|n| video.seal(n, &[n as u8]).unwrap()).collect();
        let mut window = open("pointer", Order::Window);
        for n in [2, 0, 3, 1] {
            assert_eq!(window.open(n as u64, &objects[n]).unwrap(), [n as u8]);
        }
        assert!(window.open(2, &objects[2]).is_err());
    }

    #[test]
    fn host_identity_requires_matching_key() {
        let a = rcgen::generate_simple_self_signed(vec!["a".into()]).unwrap();
        let b = rcgen::generate_simple_self_signed(vec!["b".into()]).unwrap();
        assert!(HostIdentity::new(a.cert.der().to_vec(), &b.signing_key.serialize_der()).is_err());
        assert!(HostIdentity::new(a.cert.der().to_vec(), &a.signing_key.serialize_der()).is_ok());
    }
}
