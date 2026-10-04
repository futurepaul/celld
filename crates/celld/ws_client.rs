// Copyright 2026 Deno Land Inc. Apache-2.0 license.

// The real peer WebSocket transport is outside the Actor execution domain.
#![allow(clippy::disallowed_methods)]

//! Outbound WebSocket client.
//!
//! celld serves WebSockets with fastwebsockets, and connects with it too. That
//! matters most on the peer hop: a frame relayed from a client to the cell's
//! owner is framed, masked, and closed by one implementation on both sides
//! rather than translated between two.
//!
//! What this owns that a client library would have owned: the TLS setup, the
//! handshake headers, and the non-101 response. The last one is why
//! `fastwebsockets::handshake::client` is not called directly -- it discards
//! the response body, and `new WebSocket()` reports a declined upgrade with it.

use anyhow::anyhow;
use anyhow::Context;
use base64::Engine;
use fastwebsockets::Role;
use fastwebsockets::WebSocket;
use http_body_util::BodyExt;
use http_body_util::Empty;
use hyper::header::HeaderMap;
use hyper::header::HeaderValue;
use hyper::header::CONNECTION;
use hyper::header::HOST;
use hyper::header::SEC_WEBSOCKET_ACCEPT;
use hyper::header::SEC_WEBSOCKET_KEY;
use hyper::header::SEC_WEBSOCKET_VERSION;
use hyper::header::UPGRADE;
use hyper::upgrade::Upgraded;
use hyper::Request;
use hyper::StatusCode;
use hyper_util::rt::TokioIo;
use sha1::Digest;
use sha1::Sha1;
use std::sync::OnceLock;
use tokio::io::AsyncRead;
use tokio::io::AsyncWrite;

/// A server response that was not a 101.
pub struct Declined {
    pub status: StatusCode,
    pub headers: HeaderMap,
    pub body: Vec<u8>,
}

pub enum Error {
    /// The server answered and refused the upgrade. Callers read this: a 401
    /// revokes managed credentials, a stale-route header triggers a
    /// redispatch, and the isolate surfaces the whole thing to `onerror`.
    Declined(Box<Declined>),
    /// The node's egress policy kept the destination out; nothing was dialed.
    Refused(crate::egress::EgressRefused),
    /// No answer to read -- DNS, TCP, TLS, or a malformed handshake.
    Failed(anyhow::Error),
}

impl From<anyhow::Error> for Error {
    fn from(error: anyhow::Error) -> Self {
        Error::Failed(error)
    }
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Declined(declined) => {
                write!(f, "server refused the upgrade: {}", declined.status)
            }
            Error::Refused(refused) => write!(f, "{refused}"),
            Error::Failed(error) => write!(f, "{error}"),
        }
    }
}

pub struct Connection {
    pub socket: WebSocket<TokioIo<Upgraded>>,
    /// The 101's headers: the negotiated subprotocol, and on the peer hop the
    /// signature the caller verifies before trusting the tunnel.
    pub headers: HeaderMap,
}

/// Mozilla's roots, the same set the previous client compiled in, and the
/// operator's `CELLD_EXTRA_CA_FILE` (tls_roots.rs). Deliberately not the
/// platform store: a downloaded celld should reach `wss://` on a host that has
/// no `/etc/ssl/certs`.
fn tls_config() -> &'static tokio_rustls::TlsConnector {
    static CONFIG: OnceLock<tokio_rustls::TlsConnector> = OnceLock::new();
    CONFIG.get_or_init(|| {
        let config = rustls::ClientConfig::builder()
            .with_root_certificates(crate::tls_roots::store())
            .with_no_client_auth();
        tokio_rustls::TlsConnector::from(std::sync::Arc::new(config))
    })
}

/// RFC 6455's proof the server read the key rather than blindly answering 101.
fn accept_key(key: &str) -> String {
    let mut digest = Sha1::new();
    digest.update(key.as_bytes());
    digest.update(b"258EAFA5-E914-47DA-95CA-C5AB0DC85B11");
    base64::engine::general_purpose::STANDARD.encode(digest.finalize())
}

/// Open a WebSocket to `url`, sending `extra` alongside the handshake headers,
/// and dialing under `egress`: a Worker's socket takes the node's rule
/// (`CELLD_EGRESS_PUBLIC_ONLY`), celld's own takes `Policy::OPEN`. A non-101
/// answer, a redirect included, is returned rather than followed, so the
/// dial is the only connection the rule has to judge.
///
/// The caller owns the timeout: nothing here bounds how long a server may take
/// to answer.
pub async fn connect(
    url: &str,
    extra: HeaderMap,
    egress: crate::egress::Policy,
) -> Result<Connection, Error> {
    let url = url::Url::parse(url).context("parse WebSocket URL")?;
    // `fetch("http://…", { headers: { Upgrade: "websocket" } })` is the
    // Workers idiom for an outbound socket, and the only form a container
    // port offers, so the HTTP schemes count as their WebSocket twins.
    let tls = match url.scheme() {
        "ws" | "http" => false,
        "wss" | "https" => true,
        scheme => return Err(anyhow!("not a WebSocket scheme: {scheme}").into()),
    };
    let host = url
        .host_str()
        .context("WebSocket URL has no host")?
        .to_string();
    let port = url.port_or_known_default().context("no port")?;
    let authority = match (tls, port) {
        (false, 80) | (true, 443) => host.clone(),
        _ => format!("{host}:{port}"),
    };
    let target = match url.query() {
        Some(query) => format!("{}?{query}", url.path()),
        None => url.path().to_string(),
    };

    let tcp = match egress.connect(&host, port).await {
        Ok(tcp) => tcp,
        Err(crate::egress::DialError::Refused(refused)) => return Err(Error::Refused(refused)),
        Err(crate::egress::DialError::Io(error)) => {
            return Err(anyhow::Error::new(error)
                .context(format!("connect {authority}"))
                .into())
        }
    };
    let stream: Box<dyn Stream> = if tls {
        let name = rustls::pki_types::ServerName::try_from(host.clone())
            .map_err(|_| anyhow!("invalid TLS server name: {host}"))?;
        Box::new(
            tls_config()
                .connect(name, tcp)
                .await
                .with_context(|| format!("TLS handshake with {authority}"))?,
        )
    } else {
        Box::new(tcp)
    };

    let key = fastwebsockets::handshake::generate_key();
    let mut request = Request::builder()
        .method("GET")
        .uri(&target)
        .header(HOST, &authority)
        .header(UPGRADE, "websocket")
        .header(CONNECTION, "Upgrade")
        .header(SEC_WEBSOCKET_KEY, &key)
        .header(SEC_WEBSOCKET_VERSION, "13")
        .body(Empty::<bytes::Bytes>::new())
        .context("build WebSocket handshake")?;
    request.headers_mut().extend(extra);

    let (mut sender, connection) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
        .await
        .with_context(|| format!("HTTP handshake with {authority}"))?;
    // The upgrade is completed by polling this, so it has to outlive the
    // response -- and keep running afterwards to drive the upgraded stream.
    tokio::spawn(async move {
        let _ = connection.with_upgrades().await;
    });

    let mut response = sender
        .send_request(request)
        .await
        .with_context(|| format!("WebSocket handshake with {authority}"))?;
    if response.status() != StatusCode::SWITCHING_PROTOCOLS {
        let status = response.status();
        let headers = response.headers().clone();
        let body = response
            .into_body()
            .collect()
            .await
            .map(|body| body.to_bytes().to_vec())
            .unwrap_or_default();
        return Err(Error::Declined(Box::new(Declined {
            status,
            headers,
            body,
        })));
    }
    if response
        .headers()
        .get(SEC_WEBSOCKET_ACCEPT)
        .map(HeaderValue::as_bytes)
        != Some(accept_key(&key).as_bytes())
    {
        return Err(anyhow!("server returned an invalid Sec-WebSocket-Accept").into());
    }

    let headers = response.headers().clone();
    let upgraded = hyper::upgrade::on(&mut response)
        .await
        .with_context(|| format!("upgrade the connection to {authority}"))?;
    Ok(Connection {
        socket: WebSocket::after_handshake(TokioIo::new(upgraded), Role::Client),
        headers,
    })
}

trait Stream: AsyncRead + AsyncWrite + Send + Unpin {}
impl<S: AsyncRead + AsyncWrite + Send + Unpin> Stream for S {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::egress::tests::LOOPBACK_IS_PUBLIC;
    use crate::egress::Policy;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    /// A server that declines one upgrade with a 403, so a dial that lands
    /// shows as `Declined` without a WebSocket implementation on this side.
    async fn decline_once(listener: &TcpListener) {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut request = Vec::new();
        while !request.ends_with(b"\r\n\r\n") {
            let mut byte = [0u8];
            if stream.read(&mut byte).await.unwrap() == 0 {
                break;
            }
            request.push(byte[0]);
        }
        stream
            .write_all(b"HTTP/1.1 403 Forbidden\r\ncontent-length: 0\r\nconnection: close\r\n\r\n")
            .await
            .unwrap();
    }

    async fn declined(url: &str, egress: Policy, listener: &TcpListener) {
        let (result, _) = tokio::join!(
            connect(url, HeaderMap::new(), egress),
            decline_once(listener)
        );
        match result {
            Err(Error::Declined(declined)) => assert_eq!(declined.status, StatusCode::FORBIDDEN),
            Err(error) => panic!("{url}: expected the server's 403, got {error}"),
            Ok(_) => panic!("{url}: expected the server's 403, got a socket"),
        }
    }

    fn refusal(result: Result<Connection, Error>) -> String {
        match result {
            Err(Error::Refused(refused)) => refused.0,
            Err(error) => panic!("expected a refusal, got {error}"),
            Ok(_) => panic!("expected a refusal, got a socket"),
        }
    }

    // Invalid: a public-only socket never dials a non-public literal or a
    // name with no public address, in either spelling of the scheme.
    #[tokio::test]
    async fn public_only_refuses_private_destinations() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        for url in [
            format!("ws://127.0.0.1:{port}/"),
            format!("http://127.0.0.1:{port}/chat"),
            format!("ws://[::1]:{port}/"),
            "wss://10.0.0.1/".to_string(),
            "ws://169.254.169.254/latest".to_string(),
            "https://[fdaa:0:bfe:a7b:21a:59a5:632b:2]:8081/".to_string(),
        ] {
            let message = refusal(connect(&url, HeaderMap::new(), Policy::PUBLIC).await);
            assert!(message.starts_with("egress refused: "), "{url}: {message}");
            assert!(
                message.ends_with(" is not a public address"),
                "{url}: {message}"
            );
        }
        let url = format!("ws://localhost:{port}/");
        let message = refusal(connect(&url, HeaderMap::new(), Policy::PUBLIC).await);
        assert_eq!(
            message,
            "egress refused: localhost resolves to no public address"
        );
    }

    // Valid: what the rule calls public is dialed, by literal and by name.
    #[tokio::test]
    async fn public_only_reaches_public_destinations() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        declined(
            &format!("ws://127.0.0.1:{port}/"),
            LOOPBACK_IS_PUBLIC,
            &listener,
        )
        .await;
        declined(
            &format!("http://localhost:{port}/"),
            LOOPBACK_IS_PUBLIC,
            &listener,
        )
        .await;
    }

    // Off: the socket dials as it always did.
    #[tokio::test]
    async fn without_the_setting_a_socket_is_unchanged() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        declined(&format!("ws://127.0.0.1:{port}/"), Policy::OPEN, &listener).await;
        declined(&format!("ws://localhost:{port}/"), Policy::OPEN, &listener).await;
    }
}
