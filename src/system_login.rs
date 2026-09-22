//! Existing Linux account login, only inside TLS pinned to the host identity.
//! Credentials issued here authorize one connection, never persistent trust.
use anyhow::{Context, Result, ensure};
use std::{sync::Arc, time::Duration};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    net::TcpStream,
};
use zeroize::Zeroizing;

use crate::protocol::Pairing;

pub const MAGIC: &[u8; 8] = b"TPSYS001";
const ALPN: &[u8] = b"teleport-system-login/1";
const MAX_FRAME: usize = 4096;
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);
const LOGIN_TIMEOUT: Duration = Duration::from_secs(45);

async fn read_frame(stream: &mut (impl AsyncRead + Unpin)) -> Result<Zeroizing<Vec<u8>>> {
    let size = stream.read_u32().await? as usize;
    ensure!(size > 0 && size <= MAX_FRAME, "invalid system login frame");
    let mut bytes = Zeroizing::new(vec![0; size]);
    stream.read_exact(&mut bytes).await?;
    Ok(bytes)
}

async fn write_frame(stream: &mut (impl AsyncWrite + Unpin), bytes: &[u8]) -> Result<()> {
    ensure!(
        !bytes.is_empty() && bytes.len() <= MAX_FRAME,
        "invalid system login frame"
    );
    stream.write_u32(bytes.len() as u32).await?;
    stream.write_all(bytes).await?;
    stream.flush().await?;
    Ok(())
}

fn validate_credentials(username: &str, password: &str) -> Result<()> {
    ensure!(
        !username.is_empty()
            && username.len() <= 256
            && !username.contains('\0')
            && !password.is_empty()
            && password.len() <= 2048
            && !password.contains('\0'),
        "invalid system login credentials"
    );
    Ok(())
}

fn request(username: &str, password: &str) -> Result<Zeroizing<Vec<u8>>> {
    validate_credentials(username, password)?;
    let mut bytes = Zeroizing::new(Vec::with_capacity(2 + username.len() + password.len()));
    bytes.extend_from_slice(&(username.len() as u16).to_be_bytes());
    bytes.extend_from_slice(username.as_bytes());
    bytes.extend_from_slice(password.as_bytes());
    Ok(bytes)
}

#[cfg(any(target_os = "linux", test))]
fn credentials(bytes: &[u8]) -> Result<(&str, &str)> {
    ensure!(bytes.len() >= 2, "invalid system login credentials");
    let size = u16::from_be_bytes([bytes[0], bytes[1]]) as usize;
    ensure!(size <= bytes.len() - 2, "invalid system login credentials");
    let username = std::str::from_utf8(&bytes[2..2 + size])?;
    let password = std::str::from_utf8(&bytes[2 + size..])?;
    validate_credentials(username, password)?;
    Ok((username, password))
}

/// The fingerprint must come from an already trusted host or an independently
/// verified source. Never discover/accept a new pin on this password-bearing path.
pub async fn login(
    address: &str,
    username: &str,
    password: &str,
    fingerprint: &str,
) -> Result<Pairing> {
    crate::pairing::validate_address(address)?;
    validate_credentials(username, password)?;
    let expected = moq_native::tls::parse_fingerprint(fingerprint)?;
    let mut options = moq_native::tls::Client::default();
    options.fingerprint = vec![fingerprint.to_owned()];
    let mut tls = options.build()?;
    tls.alpn_protocols = vec![ALPN.to_vec()];
    // No early data: credentials are sent only after certificate verification.
    tls.enable_early_data = false;
    tokio::time::timeout(LOGIN_TIMEOUT, async {
        let mut stream = TcpStream::connect(address).await?;
        stream.write_all(MAGIC).await?;
        let connector = tokio_rustls::TlsConnector::from(Arc::new(tls));
        let name = tokio_rustls::rustls::pki_types::ServerName::try_from("teleport.local")?;
        let mut stream = tokio::time::timeout(HANDSHAKE_TIMEOUT, connector.connect(name, stream))
            .await
            .context("system login TLS handshake timed out")?
            .context("system login host identity verification failed")?;
        ensure!(
            stream.get_ref().1.alpn_protocol() == Some(ALPN),
            "invalid system login protocol"
        );
        write_frame(&mut stream, &request(username, password)?).await?;
        let reply = read_frame(&mut stream).await?;
        ensure!(reply.len() == 129 && reply[0] == 1, "system login failed");
        let token = std::str::from_utf8(&reply[1..65])?;
        let fingerprint = std::str::from_utf8(&reply[65..])?;
        ensure!(
            token.bytes().all(|byte| byte.is_ascii_hexdigit()),
            "invalid system login credential"
        );
        ensure!(
            moq_native::tls::parse_fingerprint(fingerprint)? == expected,
            "system login returned a different host identity"
        );
        Ok(Pairing {
            token: token.to_owned(),
            fingerprint: fingerprint.to_owned(),
        })
    })
    .await
    .context("system login timed out")?
}

#[cfg(target_os = "linux")]
pub struct Config {
    pub helper: std::path::PathBuf,
    acceptor: tokio_rustls::TlsAcceptor,
    #[cfg(test)]
    authenticate_test: Option<TestAuthenticator>,
}

#[cfg(all(test, target_os = "linux"))]
type TestAuthenticator = fn(&crate::access::ServerState, &str, &str) -> Result<()>;

#[cfg(target_os = "linux")]
impl Config {
    pub fn new(directory: &std::path::Path, helper: std::path::PathBuf) -> Result<Self> {
        let cert = directory.join("certificate.pem");
        let key = directory.join("key.pem");
        crate::identity::private_file(&cert)?;
        crate::identity::private_file(&key)?;
        let mut options = moq_native::tls::Server::default();
        options.cert = vec![cert];
        options.key = vec![key];
        let tls = options.server_config(vec![ALPN.to_vec()])?;
        Ok(Self {
            helper,
            acceptor: tokio_rustls::TlsAcceptor::from(tls),
            #[cfg(test)]
            authenticate_test: None,
        })
    }

    async fn authenticate(
        &self,
        access: &crate::access::ServerState,
        username: &str,
        password: &str,
    ) -> Result<()> {
        #[cfg(test)]
        if let Some(authenticate) = self.authenticate_test {
            return authenticate(access, username, password);
        }
        let _ = access;
        crate::system_pam::authenticate(&self.helper, username, password).await
    }
}

#[cfg(target_os = "linux")]
pub async fn handle(
    mut stream: TcpStream,
    config: &Config,
    access: Arc<crate::access::ServerState>,
) -> Result<()> {
    tokio::time::timeout(LOGIN_TIMEOUT, async {
        let ip = stream.peer_addr()?.ip();
        let mut magic = [0; 8];
        tokio::time::timeout(HANDSHAKE_TIMEOUT, stream.read_exact(&mut magic)).await??;
        ensure!(&magic == MAGIC, "invalid system login protocol");
        // Admission precedes TLS and PAM, and is persisted across restarts.
        access.system_admit(ip)?;
        let mut stream =
            tokio::time::timeout(HANDSHAKE_TIMEOUT, config.acceptor.accept(stream)).await??;
        ensure!(
            stream.get_ref().1.alpn_protocol() == Some(ALPN),
            "invalid system login protocol"
        );
        let result: Result<Pairing> = async {
            ensure!(access.system_login_allowed(), "system login failed");
            let generation = access.system_generation()?;
            let bytes = read_frame(&mut stream).await?;
            let (username, password) = credentials(&bytes)?;
            config.authenticate(&access, username, password).await?;
            access.issue_system_session(generation)
        }
        .await;
        let mut reply = Zeroizing::new(Vec::with_capacity(129));
        match result {
            Ok(pairing) => {
                let token = Zeroizing::new(pairing.token);
                ensure!(
                    token.len() == 64 && token.bytes().all(|byte| byte.is_ascii_hexdigit()),
                    "invalid system session credential"
                );
                moq_native::tls::parse_fingerprint(&pairing.fingerprint)?;
                reply.push(1);
                reply.extend_from_slice(token.as_bytes());
                reply.extend_from_slice(pairing.fingerprint.as_bytes());
            }
            Err(_) => reply.push(0), // Never disclose usernames, PAM errors, or policy details.
        }
        write_frame(&mut stream, &reply).await?;
        Ok(())
    })
    .await
    .context("system login timed out")?
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn credential_frames_are_bounded_and_borrowed() -> Result<()> {
        let bytes = request("ej", "a secure password")?;
        assert_eq!(credentials(&bytes)?, ("ej", "a secure password"));
        for bytes in [&[][..], &[0], &[0, 99, b'a'], &[0, 1, 255, b'x']] {
            assert!(credentials(bytes).is_err());
        }
        for (username, password) in [("", "x"), ("ej", ""), ("ej\0", "x"), ("ej", "x\0")] {
            assert!(request(username, password).is_err());
        }
        assert!(request(&"u".repeat(257), "x").is_err());
        assert!(request("ej", &"p".repeat(2049)).is_err());
        Ok(())
    }

    #[tokio::test]
    async fn framing_rejects_lengths_before_allocation() -> Result<()> {
        for size in [0u32, 4097, u32::MAX] {
            let bytes = size.to_be_bytes();
            assert!(read_frame(&mut &bytes[..]).await.is_err());
        }
        let bytes = [0, 0, 0, 2, 1];
        assert!(read_frame(&mut &bytes[..]).await.is_err());
        assert!(
            write_frame(&mut tokio::io::sink(), &[0; 4097])
                .await
                .is_err()
        );
        Ok(())
    }

    #[cfg(target_os = "linux")]
    async fn tls_fixture(wrong_pin: bool) -> Result<()> {
        use tokio_rustls::rustls::pki_types::{CertificateDer, pem::PemObject};
        let temp = tempfile::tempdir()?;
        let directory = temp.path().join("identity");
        let identity = crate::identity::open(&directory)?;
        let der = CertificateDer::from_pem_file(&identity.certificate)?;
        let fingerprint: String = ring::digest::digest(&ring::digest::SHA256, der.as_ref())
            .as_ref()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        let config = Config::new(&directory, "/unused-test-helper".into())?;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let address = listener.local_addr()?.to_string();
        let response_fingerprint = fingerprint.clone();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await?;
            let mut magic = [0; 8];
            stream.read_exact(&mut magic).await?;
            ensure!(&magic == MAGIC, "wrong magic");
            let Ok(mut tls) = config.acceptor.accept(stream).await else {
                return Ok::<bool, anyhow::Error>(false);
            };
            let Ok(bytes) = read_frame(&mut tls).await else {
                return Ok(false);
            };
            assert_eq!(credentials(&bytes)?, ("ej", "fixture password"));
            let mut response = Zeroizing::new(vec![1]);
            response.extend_from_slice("ab".repeat(32).as_bytes());
            response.extend_from_slice(response_fingerprint.as_bytes());
            write_frame(&mut tls, &response).await?;
            Ok(true)
        });
        let pin = if wrong_pin {
            "00".repeat(32)
        } else {
            fingerprint
        };
        let result = login(&address, "ej", "fixture password", &pin).await;
        if wrong_pin {
            assert!(result.is_err());
            // In particular, the fake server never received an application
            // credential frame when its certificate did not match the pin.
            assert!(!server.await??);
        } else {
            let pairing = result?;
            assert_eq!(pairing.token, "ab".repeat(32));
            assert_eq!(pairing.fingerprint, pin);
            assert!(server.await??);
        }
        Ok(())
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    #[ignore = "requires local TCP sockets; run outside the Nix build sandbox"]
    async fn pinned_tls_login_succeeds() -> Result<()> {
        tls_fixture(false).await
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    #[ignore = "requires local TCP sockets; run outside the Nix build sandbox"]
    async fn wrong_pin_never_discloses_password() -> Result<()> {
        tls_fixture(true).await
    }

    #[cfg(target_os = "linux")]
    async fn handler_fixture(
        username: &str,
        password: &str,
        revoke_in_flight: bool,
        u2f: bool,
    ) -> Result<()> {
        use tokio_rustls::rustls::pki_types::{CertificateDer, pem::PemObject};
        let temp = tempfile::tempdir()?;
        let directory = temp.path().join("identity");
        let identity = crate::identity::open(&directory)?;
        let der = CertificateDer::from_pem_file(&identity.certificate)?;
        let fingerprint: String = ring::digest::digest(&ring::digest::SHA256, der.as_ref())
            .as_ref()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        let state = crate::access::ServerState::open(
            &directory,
            Pairing {
                token: identity.token,
                fingerprint: fingerprint.clone(),
            },
        )?;
        let mut config = Config::new(&directory, "/unused-test-helper".into())?;
        config.authenticate_test = Some(if u2f {
            |_, _, _| panic!("PAM must not run under Teleport U2F policy")
        } else if revoke_in_flight {
            |state, _, _| state.revoke_all_devices()
        } else {
            |_, username, password| {
                // Models PAM's account-management result too, not just password
                // comparison: expired/locked/other-UID accounts must fail closed.
                ensure!(
                    username == "ej" && password == "fixture password",
                    "PAM fixture rejected account"
                );
                Ok(())
            }
        });
        if u2f {
            use ring::signature::{ECDSA_P256_SHA256_ASN1_SIGNING, EcdsaKeyPair, KeyPair};
            let rng = ring::rand::SystemRandom::new();
            let pkcs8 =
                EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_ASN1_SIGNING, &rng).unwrap();
            let key =
                EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_ASN1_SIGNING, pkcs8.as_ref(), &rng)
                    .unwrap();
            state.set_password("ej", "Fixture Teleport passphrase!")?;
            let credential = serde_json::from_value(serde_json::json!({
                "version": 2, "fingerprint": fingerprint,
                "rp": format!("f-{}.{}.teleport.invalid", &fingerprint[..32], &fingerprint[32..]),
                "id": [1, 2, 3], "public_key": key.public_key().as_ref()[1..],
                "algorithm": -7, "uv_required": false, "mode": "U2f"
            }))?;
            state.require_u2f(credential)?;
        }
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let address = listener.local_addr()?.to_string();
        let server_state = state.clone();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await?;
            handle(stream, &config, server_state).await
        });
        let result = login(&address, username, password, &fingerprint).await;
        server.await??;
        if username == "ej" && password == "fixture password" && !revoke_in_flight && !u2f {
            let pairing = result?;
            assert!(state.authorized(&pairing.token));
            let lease = state.begin_session(&pairing.token)?;
            assert!(state.begin_session(&pairing.token).is_err());
            drop(lease);
            assert!(!state.authorized(&pairing.token));
            assert!(state.begin_session(&pairing.token).is_err());
        } else {
            assert!(result.is_err());
        }
        // PAM login must never create a saved device record.
        assert!(state.status().devices.is_empty());
        Ok(())
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    #[ignore = "requires local TCP sockets; run outside the Nix build sandbox"]
    async fn handler_password_accounts_revocation_and_disconnect() -> Result<()> {
        handler_fixture("ej", "fixture password", false, false).await?;
        handler_fixture("ej", "wrong password", false, false).await?;
        for user in ["unknown", "expired", "locked", "another-uid", "root"] {
            handler_fixture(user, "fixture password", false, false).await?;
        }
        handler_fixture("ej", "fixture password", true, false).await?;
        Ok(())
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    #[ignore = "requires local TCP sockets and libfido2; run outside the Nix build sandbox"]
    async fn handler_u2f_policy_never_calls_pam() -> Result<()> {
        handler_fixture("ej", "fixture password", false, true).await
    }
}
