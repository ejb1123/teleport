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
}

impl ClientHandshake {
    pub fn new(token: &str, request: &str) -> Result<Self> {
        let rng = SystemRandom::new();
        let secret = agreement::EphemeralPrivateKey::generate(&agreement::X25519, &rng)
            .map_err(|_| anyhow!("ephemeral key generation failed"))?;
        let ephemeral = secret
            .compute_public_key()
            .map_err(|_| anyhow!("ephemeral public key failed"))?
            .as_ref()
            .to_vec();
        let credential_hash = credential_hash(token);
        let hello = ClientHello {
            credential_id: credential_id(&credential_hash, &ephemeral, request),
            ephemeral,
            request: request.to_owned(),
        };
        Ok(Self {
            secret,
            hello,
            credential_hash,
        })
    }

    pub fn hello(&self) -> &ClientHello {
        &self.hello
    }

    /// Verify the host against the pinned certificate fingerprint (hex
    /// SHA-256 of the certificate DER, as in pairing files) and derive keys.
    pub fn finish(self, reply: &HostReply, pinned_fingerprint: &str) -> Result<SessionKeys> {
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
        SessionKeys::derive(&shared, &self.credential_hash, &transcript)
    }
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
    pub fn respond(
        &self,
        hello: &ClientHello,
        credential_hash: &[u8; 32],
    ) -> Result<(HostReply, SessionKeys)> {
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
            keys,
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
    let client = ClientHandshake::new("token", "").unwrap();
    let (reply, host_keys) = host
        .respond(client.hello(), &credential_hash("token"))
        .unwrap();
    (client.finish(&reply, &fingerprint).unwrap(), host_keys)
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;

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
        let client = ClientHandshake::new(token, "codec=h264").unwrap();
        let candidates = [credential_hash("someone else"), credential_hash(token)];
        let index = HostIdentity::identify(client.hello(), &candidates).unwrap();
        let (reply, host_keys) = fixture
            .host
            .respond(client.hello(), &candidates[index])
            .unwrap();
        let client_keys = client.finish(&reply, &fixture.fingerprint).unwrap();
        (client_keys, host_keys)
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
        let first = ClientHandshake::new(token, "codec=h264").unwrap();
        let second = ClientHandshake::new(token, "codec=h264").unwrap();
        let wire = serde_json::to_vec(first.hello()).unwrap();
        assert!(!wire.windows(token.len()).any(|w| w == token.as_bytes()));
        assert_ne!(first.hello().credential_id, second.hello().credential_id);
    }

    #[test]
    fn unknown_credential_is_not_identified() {
        let client = ClientHandshake::new("revoked or never issued", "codec=h264").unwrap();
        let candidates = [credential_hash("a"), credential_hash("b")];
        assert_eq!(HostIdentity::identify(client.hello(), &candidates), None);
    }

    #[test]
    fn client_rejects_wrong_host_or_forged_reply() {
        let fixture = fixture();
        let client = ClientHandshake::new("token", "codec=h264").unwrap();
        let (reply, _) = fixture
            .host
            .respond(client.hello(), &credential_hash("token"))
            .unwrap();
        // A relay (or anyone) presenting a different, unpinned certificate.
        let client2 = ClientHandshake::new("token", "codec=h264").unwrap();
        let (reply2, _) = fixture
            .host
            .respond(client2.hello(), &credential_hash("token"))
            .unwrap();
        assert!(
            ClientHandshake::new("token", "codec=h264")
                .unwrap()
                .finish(&reply, &fixture.other_fingerprint)
                .is_err()
        );
        // A reply signed for another hello cannot be replayed to this client.
        assert!(client.finish(&reply2, &fixture.fingerprint).is_err());
        // A substituted ephemeral key breaks the signature.
        let fresh = ClientHandshake::new("token", "codec=h264").unwrap();
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
        let client = ClientHandshake::new("token", "codec=h264").unwrap();
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
        let client = ClientHandshake::new("guess", "codec=h264").unwrap();
        // Host answered as if the hello were for a different credential.
        let (reply, host) = fixture
            .host
            .respond(client.hello(), &credential_hash("real"))
            .unwrap();
        let client = client.finish(&reply, &fixture.fingerprint).unwrap();
        let sealed = client
            .sealer(Direction::ClientToHost, "input")
            .unwrap()
            .seal(0, b"x")
            .unwrap();
        assert!(
            host.opener(Direction::ClientToHost, "input", Order::Strict)
                .unwrap()
                .open(0, &sealed)
                .is_err()
        );
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
