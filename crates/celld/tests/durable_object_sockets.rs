//! Durable Object WebSockets under `celld dev`, as workerd runs them.
//!
//! An object's fetch handler that awaited an outbound socket's events answers
//! once it returns, whatever op of its own is still pending; and a socket one
//! object returns to another from `fetch` carries text and binary both ways
//! and closes cleanly, with the closing handshake the object answers.

#![cfg(unix)]
// A test of the binary drives real processes, sockets, files, and clocks.
#![allow(clippy::disallowed_methods)]

mod support;

use std::io::{Read as _, Write as _};
use std::net::{TcpListener, TcpStream};
use std::time::Duration;

use support::{project, Dev};

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

const PAIR_WORKER: &str = r#"
export class Server {
  async fetch(request) {
    const answer = new URL(request.url).searchParams.get("answer");
    const [client, server] = Object.values(new WebSocketPair());
    server.accept();
    server.addEventListener("message", (event) => {
      if (typeof event.data === "string") {
        server.send(`text ${event.data}`);
      } else {
        server.send(new Uint8Array(event.data).map((byte) => byte + 1));
      }
    });
    server.addEventListener("close", (event) => {
      const reply = () => server.close(1000, `answered ${event.code} ${event.reason}`);
      if (answer === "at once") reply();
      // After a round trip of its own, as an object that bridges this socket
      // onto another closes once the other end has.
      if (answer === "later") setTimeout(reply, 50);
    });
    return new Response(null, { status: 101, webSocket: client });
  }
}

export class Caller {
  constructor(ctx, env) {
    this.env = env;
  }

  async fetch(request) {
    const url = new URL(request.url);
    const answer = url.searchParams.get("answer");
    const rounds = Number(url.searchParams.get("rounds"));
    const stub = this.env.SERVER.getByName("server");
    const seen = [];
    for (let round = 0; round < rounds; round++) {
      const response = await stub.fetch(
        `https://server/?answer=${encodeURIComponent(answer)}`,
        { headers: { Upgrade: "websocket" } },
      );
      const socket = response.webSocket;
      socket.accept();
      const got = [];
      let replied;
      const replies = new Promise((resolve) => { replied = resolve; });
      const late = setTimeout(() => replied(), 10_000);
      socket.addEventListener("message", (event) => {
        got.push(typeof event.data === "string"
          ? event.data
          : Array.from(new Uint8Array(event.data)).join(","));
        if (got.length === 3) replied();
      });
      socket.addEventListener("error", () => got.push("error"));
      const closed = new Promise((resolve) => {
        socket.addEventListener("close", (event) => {
          resolve(`close ${event.code} ${event.reason} ${event.wasClean}`);
        });
      });
      socket.send(`hello ${round}`);
      socket.send(new Uint8Array([round, 10]));
      socket.send(new Uint8Array([round, 20]).buffer);
      await replies;
      clearTimeout(late);
      socket.close(1000, "done");
      got.push(await closed);
      seen.push(got);
    }
    return Response.json(seen);
  }
}

export default {
  fetch(request, env) {
    return env.CALLER.getByName("caller").fetch(request);
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

#[test]
fn a_socket_one_object_returns_to_another_carries_both_ways_and_closes_cleanly() {
    let dir = tempfile::tempdir().unwrap();
    project(
        dir.path(),
        PAIR_WORKER,
        &[("CALLER", "Caller"), ("SERVER", "Server")],
    );
    let dev = Dev::start(dir.path());
    let round = |round: u8, close: &str| {
        serde_json::json!([
            format!("text hello {round}"),
            format!("{},11", round + 1),
            format!("{},21", round + 1),
            close,
        ])
    };

    // The server end answers the caller's close in its close handler, and
    // after a round trip of its own: the caller sees that answer, cleanly.
    for answer in ["at once", "later"] {
        let (seen, _) = dev.json(&format!("/?answer={}&rounds=3", answer.replace(' ', "%20")));
        let close = "close 1000 answered 1000 done true";
        assert_eq!(
            seen,
            serde_json::json!([round(0, close), round(1, close), round(2, close)]),
            "{answer}:\n{}",
            dev.output()
        );
    }

    // A server end that never answers: past the bound the caller's own close
    // is echoed, still clean, rather than held open for good.
    let (seen, took) = dev.json("/?answer=never&rounds=1");
    assert_eq!(seen, serde_json::json!([round(0, "close 1000 done true")]));
    assert!(
        took >= Duration::from_secs(5) && took < Duration::from_secs(5) + ANSWER_BOUND,
        "the echo came after {took:?}"
    );
}
