//! A Worker under `celld dev` reaches a server whose certificate comes from a
//! private CA only when `CELLD_EXTRA_CA_FILE` names that CA, on each of its
//! TLS paths: `fetch`, an outbound WebSocket handshake, and `connect()`.
//!
//! The run without the setting has `SSL_CERT_FILE` pointing at the CA, so it
//! also shows that the host's OpenSSL variable does not reach a Worker.

#![cfg(unix)]
// A test of the binary drives real processes, sockets, files, and clocks.
#![allow(clippy::disallowed_methods)]

use std::io::{Read as _, Write as _};
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const WORKER: &str = r#"
import { connect } from "cloudflare:sockets";

async function viaSocket(target) {
  const url = new URL(target);
  const socket = connect(
    { hostname: url.hostname, port: Number(url.port) },
    { secureTransport: "on" },
  );
  const writer = socket.writable.getWriter();
  await writer.write(new TextEncoder().encode(
    `GET / HTTP/1.1\r\nhost: ${url.host}\r\nconnection: close\r\n\r\n`));
  const reader = socket.readable.getReader();
  const decoder = new TextDecoder();
  let text = "";
  for (;;) {
    const { value, done } = await reader.read();
    if (done) break;
    text += decoder.decode(value, { stream: true });
  }
  const status = text.split(" ")[1];
  return `${status} ${text.slice(text.indexOf("\r\n\r\n") + 4)}`;
}

export default {
  async fetch(request) {
    const target = new URL(request.url).searchParams.get("target");
    const probes = {
      fetch: async () => {
        const response = await fetch(target);
        return `${response.status} ${await response.text()}`;
      },
      websocket: async () => {
        const response = await fetch(target, { headers: { Upgrade: "websocket" } });
        return `${response.status} ${await response.text()}`;
      },
      socket: () => viaSocket(target),
    };
    const outcome = {};
    for (const [name, probe] of Object.entries(probes)) {
      try {
        outcome[name] = `ok ${await probe()}`;
      } catch (error) {
        outcome[name] = `error ${error?.message ?? error}`;
      }
    }
    return new Response(JSON.stringify(outcome));
  },
};
"#;

/// What the private server answers, so a pass cannot come from elsewhere.
const BODY: &str = "from the private CA";

struct Pki {
    ca_pem: String,
    leaf: rustls::pki_types::CertificateDer<'static>,
    leaf_key: rustls::pki_types::PrivateKeyDer<'static>,
}

fn pki() -> Pki {
    use rcgen::{
        BasicConstraints, CertificateParams, DnType, ExtendedKeyUsagePurpose, IsCa, Issuer,
        KeyPair, KeyUsagePurpose,
    };
    let ca_key = KeyPair::generate().unwrap();
    let mut ca = CertificateParams::new(Vec::<String>::new()).unwrap();
    ca.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    ca.distinguished_name
        .push(DnType::CommonName, "celld extra CA test");
    ca.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
    let ca_pem = ca.self_signed(&ca_key).unwrap().pem();
    let issuer = Issuer::new(ca, ca_key);

    let leaf_key = KeyPair::generate().unwrap();
    let mut leaf = CertificateParams::new(vec!["localhost".to_string()]).unwrap();
    leaf.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
    let leaf = leaf.signed_by(&leaf_key, &issuer).unwrap();
    Pki {
        ca_pem,
        leaf: leaf.der().clone(),
        leaf_key: rustls::pki_types::PrivateKeyDer::Pkcs8(leaf_key.serialize_der().into()),
    }
}

/// An HTTPS server on an ephemeral port. Each connection reads one request
/// and answers `BODY`. A handshake the client refuses is recorded, so the run
/// without the CA can show the refusal was the certificate's.
fn serve(pki: &Pki) -> (u16, Arc<Mutex<Vec<String>>>) {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let config = rustls::ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(vec![pki.leaf.clone()], pki.leaf_key.clone_key())
        .unwrap();
    let config = Arc::new(config);
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let failures = Arc::new(Mutex::new(Vec::new()));
    let recorded = failures.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { continue };
            let config = config.clone();
            let recorded = recorded.clone();
            std::thread::spawn(move || {
                if let Err(error) = answer(stream, config) {
                    recorded.lock().unwrap().push(error.to_string());
                }
            });
        }
    });
    (port, failures)
}

fn answer(stream: TcpStream, config: Arc<rustls::ServerConfig>) -> std::io::Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(10)))?;
    let connection = rustls::ServerConnection::new(config).map_err(std::io::Error::other)?;
    let mut tls = rustls::StreamOwned::new(connection, stream);
    let mut request = Vec::new();
    let mut byte = [0u8; 1];
    while !request.ends_with(b"\r\n\r\n") {
        if tls.read(&mut byte)? == 0 || request.len() > 16 * 1024 {
            return Ok(());
        }
        request.push(byte[0]);
    }
    write!(
        tls,
        "HTTP/1.1 200 OK\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{BODY}",
        BODY.len()
    )?;
    tls.conn.send_close_notify();
    tls.flush()
}

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// One `celld dev`, stopped by its PID when dropped.
struct Dev {
    child: Child,
    port: u16,
    output: Arc<Mutex<String>>,
}

impl Dev {
    fn start(project: &Path, env: &[(&str, &Path)]) -> Dev {
        let port = free_port();
        let mut command = Command::new(env!("CARGO_BIN_EXE_celld"));
        command
            .arg("dev")
            .arg(project)
            .args(["--port", &port.to_string(), "--no-watch", "--logs"])
            .env("NO_COLOR", "1")
            .env("RUST_LOG", "info")
            .env_remove("CELLD_EXTRA_CA_FILE")
            .env_remove("CELLD_EGRESS_PUBLIC_ONLY")
            .env_remove("SSL_CERT_FILE")
            .env_remove("SSL_CERT_DIR")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for (name, value) in env {
            command.env(name, value);
        }
        let mut child = command.spawn().unwrap();
        let output = Arc::new(Mutex::new(String::new()));
        for mut stream in [
            Box::new(child.stdout.take().unwrap()) as Box<dyn std::io::Read + Send>,
            Box::new(child.stderr.take().unwrap()),
        ] {
            let output = output.clone();
            std::thread::spawn(move || {
                let mut chunk = [0u8; 4096];
                while let Ok(read) = stream.read(&mut chunk) {
                    if read == 0 {
                        break;
                    }
                    output
                        .lock()
                        .unwrap()
                        .push_str(&String::from_utf8_lossy(&chunk[..read]));
                }
            });
        }
        let mut dev = Dev {
            child,
            port,
            output,
        };
        let deadline = Instant::now() + Duration::from_secs(90);
        loop {
            if let Ok((200, _)) = dev.get("/.well-known/celld/health") {
                return dev;
            }
            if let Some(status) = dev.child.try_wait().unwrap() {
                panic!("celld dev exited with {status}:\n{}", dev.output());
            }
            assert!(
                Instant::now() < deadline,
                "celld dev did not become ready:\n{}",
                dev.output()
            );
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    fn output(&self) -> String {
        self.output.lock().unwrap().clone()
    }

    /// HTTP/1.0, so the body is the bytes up to the close.
    fn get(&self, path: &str) -> std::io::Result<(u16, String)> {
        let mut stream = TcpStream::connect(("127.0.0.1", self.port))?;
        stream.set_read_timeout(Some(Duration::from_secs(60)))?;
        write!(stream, "GET {path} HTTP/1.0\r\nhost: 127.0.0.1\r\n\r\n")?;
        let mut response = String::new();
        stream.read_to_string(&mut response)?;
        let status = response
            .split(' ')
            .nth(1)
            .and_then(|status| status.parse().ok())
            .unwrap_or(0);
        let body = response
            .split_once("\r\n\r\n")
            .map_or("", |(_, body)| body)
            .to_string();
        Ok((status, body))
    }

    fn probe(&self, server: u16) -> serde_json::Map<String, serde_json::Value> {
        let path = format!("/?target=https://localhost:{server}/");
        let (status, body) = self.get(&path).unwrap();
        assert_eq!(status, 200, "{body}\n{}", self.output());
        serde_json::from_str(&body).unwrap_or_else(|error| panic!("{error}: {body}"))
    }
}

impl Drop for Dev {
    fn drop(&mut self) {
        // SIGTERM lets the supervisor stop its node; the node also dies with
        // the supervisor (PR_SET_PDEATHSIG), so the kill below is a backstop.
        let pid = self.child.id() as libc::pid_t;
        unsafe {
            libc::kill(pid, libc::SIGTERM);
        }
        let deadline = Instant::now() + Duration::from_secs(30);
        while Instant::now() < deadline {
            if let Ok(Some(_)) = self.child.try_wait() {
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn a_worker_trusts_the_extra_ca_only_when_named() {
    let pki = pki();
    let (server, failures) = serve(&pki);

    let dir = tempfile::tempdir().unwrap();
    let project = dir.path().join("project");
    std::fs::create_dir(&project).unwrap();
    std::fs::write(
        project.join("wrangler.jsonc"),
        r#"{ "name": "extra-ca", "main": "index.js", "compatibility_date": "2026-01-01", "no_bundle": true }"#,
    )
    .unwrap();
    std::fs::write(project.join("index.js"), WORKER).unwrap();
    let ca = dir.path().join("ca.pem");
    std::fs::write(&ca, &pki.ca_pem).unwrap();

    // Without the setting every path refuses the server's certificate, even
    // with the CA in SSL_CERT_FILE.
    {
        let dev = Dev::start(&project, &[("SSL_CERT_FILE", &ca)]);
        let outcome = dev.probe(server);
        for probe in ["fetch", "websocket", "socket"] {
            let result = outcome[probe].as_str().unwrap();
            assert!(result.starts_with("error "), "{probe}: {result}");
        }
        // The server saw each client reject its certificate's issuer.
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let refusals = failures
                .lock()
                .unwrap()
                .iter()
                .filter(|failure| failure.contains("UnknownCA"))
                .count();
            if refusals >= 3 {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "{refusals} refusals: {:?}",
                failures.lock().unwrap()
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    // With it, every path verifies the server and reads its answer.
    {
        let dev = Dev::start(&project, &[("CELLD_EXTRA_CA_FILE", &ca)]);
        let outcome = dev.probe(server);
        for probe in ["fetch", "websocket", "socket"] {
            let result = outcome[probe].as_str().unwrap();
            assert_eq!(result, format!("ok 200 {BODY}"), "{probe}");
        }
        let output = dev.output();
        assert!(
            output
                .lines()
                .any(|line| line.contains("tls_extra_roots") && line.contains("roots=1")),
            "the node logs its extra roots:\n{output}"
        );
    }

    // A bundle that is not one stops `celld dev` before a node starts.
    let key = dir.path().join("key.pem");
    std::fs::write(&key, rcgen::KeyPair::generate().unwrap().serialize_pem()).unwrap();
    let refused = Command::new(env!("CARGO_BIN_EXE_celld"))
        .arg("dev")
        .arg(&project)
        .args(["--port", &free_port().to_string(), "--no-watch"])
        .env("CELLD_EXTRA_CA_FILE", &key)
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(!refused.status.success());
    let stderr = String::from_utf8_lossy(&refused.stderr);
    assert!(
        stderr.contains(&format!(
            "CELLD_EXTRA_CA_FILE {} holds a private key; it must hold certificates only",
            key.display()
        )),
        "{stderr}"
    );
}
