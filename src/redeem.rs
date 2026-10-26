//! Opening a session from a web page: a `teleport://redeem?url=...` link
//! (docs/tickets.md#links-from-a-web-page).
//!
//! The issuer's site hands the user a single-use HTTPS URL. The client makes
//! a fresh key, posts its public half there, and gets back a session ticket
//! bound to that key with what it needs to reach the host: the host
//! fingerprint (the ticket's audience) and the relay. It then redeems the
//! ticket through the relay as `teleport login-ticket` does. The URL is the
//! only credential the link carries and the server spends it on first use;
//! the client key never leaves this process.
use anyhow::{Context, Result, ensure};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use serde::Deserialize;
use std::{sync::Arc, time::Duration};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

pub const SCHEME: &str = "teleport";
const MAX_RESPONSE: usize = 64 * 1024;
const TIMEOUT: Duration = Duration::from_secs(20);

/// What the issuer's redeem URL answers.
#[derive(Debug, Deserialize)]
pub struct Grant {
    pub ticket: String,
    /// The host's display name.
    #[serde(default)]
    pub name: String,
    pub host_fingerprint: String,
    pub relay: GrantRelay,
}

#[derive(Debug, Deserialize)]
pub struct GrantRelay {
    pub url: url::Url,
    #[serde(default)]
    pub fingerprint: String,
    #[serde(default)]
    pub key: String,
}

impl GrantRelay {
    pub fn args(&self) -> crate::relay::RelayArgs {
        crate::relay::RelayArgs {
            relay: Some(self.url.clone()),
            relay_fingerprint: (!self.fingerprint.is_empty()).then(|| self.fingerprint.clone()),
            relay_key: (!self.key.is_empty()).then(|| self.key.clone()),
        }
    }
}

#[derive(Deserialize)]
struct Refusal {
    error: String,
}

fn loopback(url: &url::Url) -> bool {
    match url.host() {
        Some(url::Host::Ipv4(ip)) => ip.is_loopback(),
        Some(url::Host::Ipv6(ip)) => ip.is_loopback(),
        Some(url::Host::Domain(name)) => name == "localhost",
        None => false,
    }
}

/// The redeem URL in `link`: `teleport://redeem?url=<https URL>`, or the
/// URL itself. Plain `http` is accepted only on loopback (tests, a local
/// issuer).
pub fn redeem_url(link: &str) -> Result<url::Url> {
    let link = link.trim();
    let parsed: url::Url = link.parse().context("not a link")?;
    let url: url::Url = if parsed.scheme() == SCHEME {
        ensure!(
            parsed.host_str() == Some("redeem"),
            "not a Teleport session link"
        );
        let mut urls = parsed.query_pairs().filter(|(key, _)| key == "url");
        let (_, value) = urls.next().context("the link names no session")?;
        ensure!(urls.next().is_none(), "the link names two sessions");
        value.parse().context("the link's session URL is invalid")?
    } else {
        parsed
    };
    ensure!(
        url.scheme() == "https" || (url.scheme() == "http" && loopback(&url)),
        "the session URL must use https"
    );
    ensure!(
        url.username().is_empty() && url.password().is_none() && url.fragment().is_none(),
        "the session URL must not carry credentials or a fragment"
    );
    url.host_str().context("the session URL has no host")?;
    Ok(url)
}

/// A minimal HTTP/1.1 POST of `body` (JSON), `Connection: close`; returns
/// the status and body. Enough for one small JSON exchange: no redirects
/// (a redeem URL must not move), no compression.
async fn post(url: &url::Url, body: &[u8]) -> Result<(u16, Vec<u8>)> {
    let host = url.host_str().context("no host")?;
    let port = url.port_or_known_default().context("no port")?;
    let tcp = tokio::net::TcpStream::connect((host, port)).await?;
    let mut target = url.path().to_owned();
    if let Some(query) = url.query() {
        target.push('?');
        target.push_str(query);
    }
    let authority = match url.port() {
        Some(port) => format!("{host}:{port}"),
        None => host.to_owned(),
    };
    let request = format!(
        "POST {target} HTTP/1.1\r\nHost: {authority}\r\nUser-Agent: teleport/{}\r\n\
         Content-Type: application/json\r\nAccept: application/json\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n",
        env!("CARGO_PKG_VERSION"),
        body.len()
    );
    if url.scheme() == "https" {
        let mut tls = moq_native::tls::Client::default().build()?;
        tls.alpn_protocols = vec![b"http/1.1".to_vec()];
        let connector = tokio_rustls::TlsConnector::from(Arc::new(tls));
        let name = tokio_rustls::rustls::pki_types::ServerName::try_from(host.to_owned())?;
        let stream = connector.connect(name, tcp).await?;
        exchange(stream, &request, body).await
    } else {
        exchange(tcp, &request, body).await
    }
}

async fn exchange(
    mut stream: impl AsyncRead + AsyncWrite + Unpin,
    request: &str,
    body: &[u8],
) -> Result<(u16, Vec<u8>)> {
    stream.write_all(request.as_bytes()).await?;
    stream.write_all(body).await?;
    stream.flush().await?;
    let mut response = Vec::new();
    (&mut stream)
        .take(MAX_RESPONSE as u64 + 1)
        .read_to_end(&mut response)
        .await?;
    ensure!(
        response.len() <= MAX_RESPONSE,
        "the session server answered too much"
    );
    parse_response(&response)
}

/// Status and body of a complete HTTP/1.1 response (`Content-Length`,
/// chunked, or until the connection closed).
fn parse_response(response: &[u8]) -> Result<(u16, Vec<u8>)> {
    let split = response
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .context("incomplete answer from the session server")?;
    let head = std::str::from_utf8(&response[..split]).context("invalid answer")?;
    let mut lines = head.split("\r\n");
    let status = lines
        .next()
        .and_then(|line| {
            let mut parts = line.split(' ');
            let version = parts.next()?;
            version.starts_with("HTTP/1.").then_some(())?;
            parts.next()?.parse::<u16>().ok()
        })
        .context("invalid answer from the session server")?;
    let mut length = None;
    let mut chunked = false;
    for line in lines {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.trim();
        if name.eq_ignore_ascii_case("content-length") {
            length = Some(value.parse::<usize>().context("invalid Content-Length")?);
        } else if name.eq_ignore_ascii_case("transfer-encoding") {
            chunked = value.eq_ignore_ascii_case("chunked");
        }
    }
    let rest = &response[split + 4..];
    let body = if chunked {
        dechunk(rest)?
    } else if let Some(length) = length {
        ensure!(
            rest.len() >= length,
            "truncated answer from the session server"
        );
        rest[..length].to_vec()
    } else {
        rest.to_vec()
    };
    Ok((status, body))
}

fn dechunk(mut data: &[u8]) -> Result<Vec<u8>> {
    let mut body = Vec::new();
    loop {
        let end = data
            .windows(2)
            .position(|w| w == b"\r\n")
            .context("truncated chunked answer")?;
        let size_text = std::str::from_utf8(&data[..end])?;
        let size = usize::from_str_radix(size_text.split(';').next().unwrap_or("").trim(), 16)
            .context("invalid chunk size")?;
        data = &data[end + 2..];
        if size == 0 {
            return Ok(body);
        }
        ensure!(data.len() >= size + 2, "truncated chunked answer");
        body.extend_from_slice(&data[..size]);
        data = &data[size + 2..];
    }
}

/// Post this client's key to the redeem URL and return the grant, checked
/// against what the client can verify itself.
pub async fn fetch(url: &url::Url, client: &crate::e2e::DeviceKey) -> Result<Grant> {
    let body = serde_json::to_vec(&serde_json::json!({
        "client_key": URL_SAFE_NO_PAD.encode(client.public_key()),
    }))?;
    let (status, answer) = tokio::time::timeout(TIMEOUT, post(url, &body))
        .await
        .context("the session server did not answer in time")??;
    if !(200..300).contains(&status) {
        let reason = serde_json::from_slice::<Refusal>(&answer)
            .map(|refusal| refusal.error)
            .unwrap_or_else(|_| format!("HTTP {status}"));
        anyhow::bail!("the session server refused: {reason}");
    }
    let grant: Grant =
        serde_json::from_slice(&answer).context("invalid answer from the session server")?;
    moq_native::tls::parse_fingerprint(&grant.host_fingerprint)
        .context("the session server named an invalid host")?;
    ensure!(
        grant.ticket.starts_with(crate::ticket::PREFIX),
        "the session server sent no session ticket"
    );
    ensure!(
        grant.name.len() <= 128 && !grant.name.chars().any(char::is_control),
        "invalid host name from the session server"
    );
    Ok(grant)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    #[test]
    fn links_name_one_https_url_and_nothing_else() {
        let link = "teleport://redeem?url=https%3A%2F%2Fapi.example%2Fredeem%2Fabc";
        assert_eq!(
            redeem_url(link).unwrap().as_str(),
            "https://api.example/redeem/abc"
        );
        assert_eq!(
            redeem_url("https://api.example/redeem/abc")
                .unwrap()
                .as_str(),
            "https://api.example/redeem/abc"
        );
        assert!(redeem_url("http://127.0.0.1:8080/r/x").is_ok());
        for bad in [
            "teleport://redeem?url=http%3A%2F%2Fapi.example%2Fr",
            "http://api.example/r",
            "teleport://pair?url=https%3A%2F%2Fapi.example%2Fr",
            "teleport://redeem",
            "teleport://redeem?url=https%3A%2F%2Fa.example%2F1&url=https%3A%2F%2Fb.example%2F2",
            "https://user:pw@api.example/r",
            "https://api.example/r#frag",
            "ftp://api.example/r",
            "not a link",
        ] {
            assert!(redeem_url(bad).is_err(), "accepted {bad}");
        }
    }

    #[test]
    fn responses_parse_by_length_chunked_or_close() {
        let (status, body) =
            parse_response(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\n{}trailing").unwrap();
        assert_eq!((status, body.as_slice()), (200, b"{}".as_slice()));
        let (status, body) = parse_response(
            b"HTTP/1.1 410 Gone\r\nTransfer-Encoding: chunked\r\n\r\n3\r\n{\"a\r\n4;x=y\r\n\":1}\r\n0\r\n\r\n",
        )
        .unwrap();
        assert_eq!((status, body.as_slice()), (410, b"{\"a\":1}".as_slice()));
        let (_, body) = parse_response(b"HTTP/1.0 200 OK\r\n\r\nall").unwrap();
        assert_eq!(body, b"all");
        assert!(parse_response(b"HTTP/1.1 200 OK\r\nContent-Length: 9\r\n\r\nshort").is_err());
        assert!(parse_response(b"SSH-2.0\r\n\r\n").is_err());
        assert!(parse_response(b"HTTP/1.1 200 OK\r\n").is_err());
    }

    /// One HTTP exchange on loopback: what the client posts, and the answer.
    pub(crate) async fn serve_once(
        answer: String,
    ) -> (url::Url, tokio::task::JoinHandle<serde_json::Value>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/redeem/secret", listener.local_addr().unwrap())
            .parse()
            .unwrap();
        let task = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            let mut buffer = [0; 4096];
            loop {
                let n = stream.read(&mut buffer).await.unwrap();
                request.extend_from_slice(&buffer[..n]);
                if let Some(split) = request.windows(4).position(|w| w == b"\r\n\r\n") {
                    let head = String::from_utf8_lossy(&request[..split]).to_lowercase();
                    let length: usize = head
                        .lines()
                        .find_map(|l| l.strip_prefix("content-length:"))
                        .unwrap()
                        .trim()
                        .parse()
                        .unwrap();
                    if request.len() >= split + 4 + length {
                        assert!(head.starts_with("post /redeem/secret http/1.1"));
                        assert!(!head.contains("cookie") && !head.contains("authorization"));
                        let body = request[split + 4..split + 4 + length].to_vec();
                        stream.write_all(answer.as_bytes()).await.unwrap();
                        stream.shutdown().await.unwrap();
                        return serde_json::from_slice(&body).unwrap();
                    }
                }
            }
        });
        (url, task)
    }

    #[tokio::test]
    #[ignore = "requires local TCP sockets; run outside the Nix build sandbox"]
    async fn fetch_posts_only_the_public_key_and_checks_the_grant() -> Result<()> {
        let (client, _) = crate::e2e::DeviceKey::generate()?;
        let grant = serde_json::json!({
            "ticket": "tkt1.payload.signature",
            "name": "Lab box",
            "host_fingerprint": "ab".repeat(32),
            "relay": {"url": "moqt://relay.example:4444", "fingerprint": "", "key": "k"},
        })
        .to_string();
        let (url, server) = serve_once(format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{grant}",
            grant.len()
        ))
        .await;
        let got = fetch(&url, &client).await?;
        let posted = server.await?;
        assert_eq!(
            posted,
            serde_json::json!({"client_key": URL_SAFE_NO_PAD.encode(client.public_key())})
        );
        assert_eq!(got.name, "Lab box");
        let relay = got.relay.args();
        assert!(relay.relay_fingerprint.is_none() && relay.relay_key.as_deref() == Some("k"));

        // A refusal carries the server's reason.
        let refusal = r#"{"error":"this session link is no longer valid"}"#;
        let (url, _server) = serve_once(format!(
            "HTTP/1.1 410 Gone\r\nContent-Length: {}\r\n\r\n{refusal}",
            refusal.len()
        ))
        .await;
        let error = fetch(&url, &client).await.unwrap_err().to_string();
        assert!(error.contains("no longer valid"), "{error}");

        // A grant without a ticket, or for a malformed host, is refused.
        for bad in [
            serde_json::json!({"ticket": "x", "host_fingerprint": "ab".repeat(32),
                "relay": {"url": "moqt://r:1"}}),
            serde_json::json!({"ticket": "tkt1.a.b", "host_fingerprint": "zz",
                "relay": {"url": "moqt://r:1"}}),
        ] {
            let bad = bad.to_string();
            let (url, _server) = serve_once(format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{bad}",
                bad.len()
            ))
            .await;
            assert!(fetch(&url, &client).await.is_err());
        }
        Ok(())
    }
}
