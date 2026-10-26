//! Browser bindings for Teleport's end-to-end layer (`@teleport/web`).
//! `e2e` is the native application's `src/e2e.rs` itself, not a copy, so
//! both sides run one implementation (its host half is Linux-only and is not
//! compiled here).
#[path = "../../../src/e2e.rs"]
#[allow(dead_code)]
mod e2e;

/// What `e2e` needs from the native `crate::ticket`.
mod ticket {
    pub const PREFIX: &str = "tkt1.";
}

use std::collections::HashMap;
use wasm_bindgen::prelude::*;

fn bytes32(data: &[u8]) -> Result<[u8; 32], JsError> {
    data.try_into()
        .map_err(|_| JsError::new("expected 32 bytes"))
}

fn error(error: anyhow::Error) -> JsError {
    JsError::new(&format!("{error:#}"))
}

/// As the native bridge (`src/bridge.rs`): ordered tracks carry state that
/// must arrive completely and in order; media accepts reordering.
fn order(track: &str) -> e2e::Order {
    if matches!(track, "input" | "updates" | "desktop") || track.starts_with("ssh-agent") {
        e2e::Order::Strict
    } else {
        e2e::Order::Window
    }
}

/// The relay namespace of the host with this certificate fingerprint
/// (`relay::namespace`): the path a client dials.
#[wasm_bindgen]
pub fn namespace(fingerprint: &str) -> Result<String, JsError> {
    let fingerprint = moq_native::tls::parse_fingerprint(fingerprint).map_err(error)?;
    let mut context = ring::digest::Context::new(&ring::digest::SHA256);
    context.update(b"teleport-relay-namespace-v1");
    context.update(&fingerprint);
    Ok(context.finish().as_ref()[..16]
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect())
}

/// Random lowercase hex (`bytes` bytes): session ids on the relay.
#[wasm_bindgen]
pub fn random_hex(bytes: usize) -> String {
    let mut out = vec![0u8; bytes];
    ring::rand::SecureRandom::fill(&ring::rand::SystemRandom::new(), &mut out)
        .expect("browser randomness unavailable");
    out.iter().map(|b| format!("{b:02x}")).collect()
}

/// A fresh client key for one session. Its private half stays in this
/// object (WebAssembly memory) and is never exported.
#[wasm_bindgen]
pub struct ClientKey(Option<e2e::DeviceKey>);

#[wasm_bindgen]
impl ClientKey {
    #[wasm_bindgen(constructor)]
    pub fn new() -> Result<ClientKey, JsError> {
        let (key, seed) = e2e::DeviceKey::generate().map_err(error)?;
        drop(zeroize::Zeroizing::new(seed));
        Ok(ClientKey(Some(key)))
    }

    /// The raw 32-byte Ed25519 public key (the ticket's `cnf`).
    #[wasm_bindgen(js_name = publicKey)]
    pub fn public_key(&self) -> Result<Vec<u8>, JsError> {
        Ok(self
            .0
            .as_ref()
            .ok_or_else(|| JsError::new("client key already used"))?
            .public_key()
            .to_vec())
    }
}

/// One relayed session opened by a ticket in the handshake
/// (docs/tickets.md#tickets-in-the-handshake): build the hello, check the
/// host's reply against the pinned fingerprint, then seal what this client
/// sends and open what the host sends, per track.
#[wasm_bindgen]
pub struct TicketSession {
    handshake: Option<e2e::ClientHandshake>,
    keys: Option<e2e::SessionKeys>,
    sealers: HashMap<String, e2e::Sealer>,
    openers: HashMap<String, e2e::Opener>,
}

#[wasm_bindgen]
impl TicketSession {
    /// `request`: connection options, e.g. `codec=h264`. Takes the key.
    #[wasm_bindgen(constructor)]
    pub fn new(ticket: &str, request: &str, key: &mut ClientKey) -> Result<TicketSession, JsError> {
        let key = key.0.take().ok_or_else(|| JsError::new("client key already used"))?;
        let handshake = e2e::ClientHandshake::for_ticket(ticket, request, key).map_err(error)?;
        Ok(TicketSession {
            handshake: Some(handshake),
            keys: None,
            sealers: HashMap::new(),
            openers: HashMap::new(),
        })
    }

    /// The hello to publish (JSON).
    pub fn hello(&self) -> Result<Vec<u8>, JsError> {
        let handshake = self
            .handshake
            .as_ref()
            .ok_or_else(|| JsError::new("handshake already finished"))?;
        serde_json::to_vec(handshake.hello()).map_err(|e| JsError::new(&e.to_string()))
    }

    /// Verify the host's reply (JSON) against `fingerprint` and return the
    /// confirmation frames to publish, in order, in one group.
    pub fn finish(&mut self, reply: &[u8], fingerprint: &str) -> Result<js_sys::Array, JsError> {
        let handshake = self
            .handshake
            .take()
            .ok_or_else(|| JsError::new("handshake already finished"))?;
        let reply: e2e::HostReply =
            serde_json::from_slice(reply).map_err(|_| JsError::new("malformed host reply"))?;
        let established = handshake.finish(&reply, fingerprint).map_err(error)?;
        self.keys = Some(established.keys);
        let frames = js_sys::Array::new();
        for frame in established.confirmation {
            frames.push(&js_sys::Uint8Array::from(&frame[..]));
        }
        Ok(frames)
    }

    fn keys(&self) -> Result<&e2e::SessionKeys, JsError> {
        self.keys
            .as_ref()
            .ok_or_else(|| JsError::new("handshake not finished"))
    }

    /// Seal an object this client publishes on `track`, group `group`.
    pub fn seal(&mut self, track: &str, group: u64, plaintext: &[u8]) -> Result<Vec<u8>, JsError> {
        if !self.sealers.contains_key(track) {
            let sealer = self
                .keys()?
                .sealer(e2e::Direction::ClientToHost, track)
                .map_err(error)?;
            self.sealers.insert(track.to_owned(), sealer);
        }
        self.sealers
            .get_mut(track)
            .expect("inserted above")
            .seal(group, plaintext)
            .map_err(error)
    }

    /// Open an object the host published on `track`, group `group`.
    pub fn open(&mut self, track: &str, group: u64, sealed: &[u8]) -> Result<Vec<u8>, JsError> {
        if !self.openers.contains_key(track) {
            let opener = self
                .keys()?
                .opener(e2e::Direction::HostToClient, track, order(track))
                .map_err(error)?;
            self.openers.insert(track.to_owned(), opener);
        }
        self.openers
            .get_mut(track)
            .expect("inserted above")
            .open(group, sealed)
            .map_err(error)
    }
}

/// Open one object of `track` (host to client) with session keys derived from
/// fixed inputs: the known-answer check against the native implementation.
#[wasm_bindgen]
pub fn open_vector(
    shared: &[u8],
    credential_hash: &[u8],
    transcript: &[u8],
    track: &str,
    group: u64,
    sealed: &[u8],
) -> Result<Vec<u8>, JsError> {
    let keys = e2e::SessionKeys::vector(shared, &bytes32(credential_hash)?, &bytes32(transcript)?)
        .map_err(error)?;
    let mut opener = keys
        .opener(e2e::Direction::HostToClient, track, e2e::Order::Strict)
        .map_err(error)?;
    opener.open(group, sealed).map_err(error)
}
