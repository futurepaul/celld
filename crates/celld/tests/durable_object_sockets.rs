//! Durable Object WebSockets under `celld dev`, as workerd runs them.
//!
//! An object's fetch handler that awaited an outbound socket's events answers
//! once it returns, whatever op of its own is still pending.

#![cfg(unix)]
// A test of the binary drives real processes, sockets, files, and clocks.
#![allow(clippy::disallowed_methods)]

use std::io::{Read as _, Write as _};
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// How long the handler's own leftover op runs: far longer than any answer
/// here may take, so an answer that waits for it fails the bound below.
const HOLD_MS: u64 = 30_000;

/// An answer that did not wait for the leftover op arrives well within this.
const ANSWER_BOUND: Duration = Duration::from_secs(10);

const OUTBOUND_WORKER: &str = r#"
export class Client {
  async fetch(request) {
    const url = new URL(request.url);
    const target = url.searchParams.get("target");
    // Work the handler leaves running, as a container monitor's long poll
    // does: an op of this event's own that outlasts its answer.
    if (url.searchParams.has("hold")) setTimeout(() => {}, Number(url.searchParams.get("hold")));
    const seen = [];
    for (let round = 0; round < 3; round++) {
      const response = await fetch(target, { headers: { Upgrade: "websocket" } });
      const socket = response.webSocket;
      socket.accept();
      let echoed;
      const message = new Promise((resolve) => { echoed = resolve; });
      socket.addEventListener("message", (event) => echoed(event.data));
      const closed = new Promise((resolve) => {
        socket.addEventListener("close", (event) => resolve(event.code));
      });
      socket.send(`ping ${round}`);
      seen.push(await message);
      socket.close(1000, "done");
      seen.push(await closed);
    }
    // The await above resumed inside the socket's close event, and the
    // handler returns there: nothing of this event's own runs after it.
    return Response.json(seen);
  }
}

export default {
  fetch(request, env) {
    return env.CLIENT.getByName("client").fetch(request);
  },
};
"#;

/// A WebSocket echo server on an ephemeral port: each text or binary message
/// comes back as it was, and a close is answered with the same close.
fn serve_echo() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { continue };
            std::thread::spawn(move || {
                let _ = echo(stream);
            });
        }
    });
    port
}

fn echo(mut stream: TcpStream) -> std::io::Result<()> {
    use base64::Engine as _;
    use sha1::Digest as _;

    stream.set_read_timeout(Some(Duration::from_secs(30)))?;
    let mut request = Vec::new();
    let mut byte = [0u8; 1];
    while !request.ends_with(b"\r\n\r\n") {
        if stream.read(&mut byte)? == 0 || request.len() > 16 * 1024 {
            return Ok(());
        }
        request.push(byte[0]);
    }
    let request = String::from_utf8_lossy(&request);
    let key = request
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("sec-websocket-key")
                .then(|| value.trim().to_string())
        })
        .unwrap_or_default();
    let accept = base64::engine::general_purpose::STANDARD.encode(sha1::Sha1::digest(
        format!("{key}258EAFA5-E914-47DA-95CA-C5AB0DC85B11").as_bytes(),
    ));
    write!(
        stream,
        "HTTP/1.1 101 Switching Protocols\r\nupgrade: websocket\r\nconnection: Upgrade\r\nsec-websocket-accept: {accept}\r\n\r\n"
    )?;
    loop {
        let mut head = [0u8; 2];
        stream.read_exact(&mut head)?;
        let opcode = head[0] & 0x0f;
        let mut length = u64::from(head[1] & 0x7f);
        if length == 126 {
            let mut extended = [0u8; 2];
            stream.read_exact(&mut extended)?;
            length = u64::from(u16::from_be_bytes(extended));
        } else if length == 127 {
            let mut extended = [0u8; 8];
            stream.read_exact(&mut extended)?;
            length = u64::from_be_bytes(extended);
        }
        assert!(length <= 64 * 1024, "a test frame of {length} bytes");
        // A client masks every frame it sends (RFC 6455, 5.3).
        let mut mask = [0u8; 4];
        if head[1] & 0x80 != 0 {
            stream.read_exact(&mut mask)?;
        }
        let mut payload = vec![0u8; length as usize];
        stream.read_exact(&mut payload)?;
        for (index, byte) in payload.iter_mut().enumerate() {
            *byte ^= mask[index % 4];
        }
        let mut frame = vec![0x80 | opcode];
        if payload.len() < 126 {
            frame.push(payload.len() as u8);
        } else {
            frame.push(126);
            frame.extend_from_slice(&(payload.len() as u16).to_be_bytes());
        }
        frame.extend_from_slice(&payload);
        match opcode {
            0x1 | 0x2 => stream.write_all(&frame)?,
            0x8 => {
                stream.write_all(&frame)?;
                return Ok(());
            }
            0x9 => {
                frame[0] = 0x80 | 0xa;
                stream.write_all(&frame)?;
            }
            _ => {}
        }
    }
}

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// A Wrangler project of one script and its Durable Object classes.
fn project(dir: &Path, worker: &str, classes: &[(&str, &str)]) {
    let bindings = classes
        .iter()
        .map(|(binding, class)| format!(r#"{{ "name": "{binding}", "class_name": "{class}" }}"#))
        .collect::<Vec<_>>()
        .join(", ");
    let names = classes
        .iter()
        .map(|(_, class)| format!(r#""{class}""#))
        .collect::<Vec<_>>()
        .join(", ");
    std::fs::write(
        dir.join("wrangler.jsonc"),
        format!(
            r#"{{
  "name": "sockets",
  "main": "index.js",
  "compatibility_date": "2026-01-01",
  "no_bundle": true,
  "durable_objects": {{ "bindings": [{bindings}] }},
  "migrations": [{{ "tag": "v1", "new_sqlite_classes": [{names}] }}]
}}"#
        ),
    )
    .unwrap();
    std::fs::write(dir.join("index.js"), worker).unwrap();
}

/// One `celld dev`, stopped by its PID when dropped.
struct Dev {
    child: Child,
    port: u16,
    output: Arc<Mutex<String>>,
}

impl Dev {
    fn start(project: &Path) -> Dev {
        let port = free_port();
        let mut child = Command::new(env!("CARGO_BIN_EXE_celld"))
            .arg("dev")
            .arg(project)
            .args(["--port", &port.to_string(), "--no-watch", "--logs"])
            .env("NO_COLOR", "1")
            .env("RUST_LOG", "info")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
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

    /// A 200's JSON body, and how long it took.
    fn json(&self, path: &str) -> (serde_json::Value, Duration) {
        let started = Instant::now();
        let (status, body) = self
            .get(path)
            .unwrap_or_else(|error| panic!("GET {path}: {error}\n{}", self.output()));
        let took = started.elapsed();
        assert_eq!(status, 200, "GET {path}: {body}\n{}", self.output());
        let value = serde_json::from_str(&body).unwrap_or_else(|error| panic!("{error}: {body}"));
        (value, took)
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
fn a_handler_that_returns_in_a_socket_event_is_answered_at_once() {
    let echo = serve_echo();
    let dir = tempfile::tempdir().unwrap();
    project(dir.path(), OUTBOUND_WORKER, &[("CLIENT", "Client")]);
    let dev = Dev::start(dir.path());
    let expected = serde_json::json!(["ping 0", 1000, "ping 1", 1000, "ping 2", 1000]);

    // Nothing of the handler's own outstanding: the driver's poll finds it.
    let target = format!("ws://127.0.0.1:{echo}/");
    let (seen, _) = dev.json(&format!("/?target={target}"));
    assert_eq!(seen, expected);

    // An op of its own still pending, which used to hold the finished answer
    // until it ended. Twice, so a second request to the warm object counts.
    for _ in 0..2 {
        let (seen, took) = dev.json(&format!("/?target={target}&hold={HOLD_MS}"));
        assert_eq!(seen, expected);
        assert!(
            took < ANSWER_BOUND,
            "answered after {took:?}, waiting on the handler's leftover timer:\n{}",
            dev.output()
        );
    }
}
