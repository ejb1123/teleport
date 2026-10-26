//! Browser bindings for Teleport's end-to-end layer. `e2e` is the native
//! application's `src/e2e.rs` itself, not a copy, so both sides run one
//! implementation (its host half is Linux-only and is not compiled here).
#[path = "../../../src/e2e.rs"]
#[allow(dead_code)]
mod e2e;

use wasm_bindgen::prelude::*;

fn bytes32(data: &[u8]) -> Result<[u8; 32], JsError> {
    data.try_into()
        .map_err(|_| JsError::new("expected 32 bytes"))
}

fn error(error: anyhow::Error) -> JsError {
    JsError::new(&format!("{error:#}"))
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

/// Start a client handshake for the credential `token` (64 hex digits) with a
/// fresh device key: X25519 and Ed25519 from the browser's randomness. Returns
/// the hello's ephemeral key and credential identifier (hex), different on
/// every call.
#[wasm_bindgen]
pub fn client_hello_id(token: &str) -> Result<String, JsError> {
    let (device, _seed) = e2e::DeviceKey::generate().map_err(error)?;
    let handshake = e2e::ClientHandshake::new(token, "", Some(device)).map_err(error)?;
    let hello = handshake.hello();
    Ok(hello
        .ephemeral
        .iter()
        .chain(&hello.credential_id)
        .map(|b| format!("{b:02x}"))
        .collect())
}
