//! A body a Worker forwards to a Durable Object is framed as workerd frames
//! it: a known length (an incoming request's declared one, a held body's, a
//! `FixedLengthStream`'s) reaches the object as its Content-Length, and an
//! unknown one as chunked. Each expectation is what workerd 2026-09-30
//! answered for the same Worker.

#![cfg(unix)]
// A test of the binary drives real processes, sockets, files, and clocks.
#![allow(clippy::disallowed_methods)]

mod support;

use support::{project, Dev};

const WORKER: &str = r#"
export class Box {
  async fetch(request) {
    return Response.json({
      length: request.headers.get("content-length"),
      encoding: request.headers.get("transfer-encoding"),
      read: (await request.arrayBuffer()).byteLength,
    });
  }
}

const once = (bytes) => {
  const { readable, writable } = new TransformStream();
  const writer = writable.getWriter();
  writer.write(bytes);
  writer.close();
  return readable;
};

export default {
  async fetch(request, env) {
    const box = env.BOX.getByName("box");
    const put = (body) => box.fetch("https://box/", { method: "PUT", body });
    switch (new URL(request.url).searchParams.get("how")) {
      case "body": return put(request.body);
      case "request":
        return box.fetch(new Request("https://box/", { method: "PUT", body: request.body }));
      case "whole": return box.fetch(request);
      case "string": return put("hello");
      case "fixed": {
        const { readable, writable } = new FixedLengthStream(5);
        const writer = writable.getWriter();
        writer.write(new TextEncoder().encode("hello"));
        writer.close();
        return put(readable);
      }
      case "unknown": return put(once(new TextEncoder().encode("hello")));
      case "none": return box.fetch("https://box/", { method: "POST" });
      case "get": return box.fetch("https://box/");
    }
    return new Response("how?", { status: 400 });
  },
};
"#;

/// Past what celld holds in memory, so the body arrives as a stream.
const STREAMED: usize = 300_000;

fn seen(length: Option<usize>, chunked: bool, read: usize) -> serde_json::Value {
    serde_json::json!({
        "length": length.map(|length| length.to_string()),
        "encoding": chunked.then_some("chunked"),
        "read": read,
    })
}

#[test]
fn a_forwarded_body_reaches_an_object_with_its_length() {
    let dir = tempfile::tempdir().unwrap();
    project(dir.path(), WORKER, &[("BOX", "Box")]);
    let dev = Dev::start(dir.path());
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let client = reqwest::Client::new();
    let send = |how: &str, body: reqwest::Body| {
        let url = format!("http://127.0.0.1:{}/?how={how}", dev.port);
        runtime.block_on(async {
            let response = client.put(url).body(body).send().await.unwrap();
            assert_eq!(response.status(), 200, "{how}:\n{}", dev.output());
            response.json::<serde_json::Value>().await.unwrap()
        })
    };
    let chunked = |bytes: Vec<u8>| {
        reqwest::Body::wrap_stream(futures_util::stream::iter([Ok::<_, std::io::Error>(bytes)]))
    };

    for size in [11, STREAMED] {
        let bytes = vec![b'a'; size];
        for how in ["body", "request", "whole"] {
            assert_eq!(
                send(how, bytes.clone().into()),
                seen(Some(size), false, size),
                "{how}, {size} bytes declared:\n{}",
                dev.output()
            );
            assert_eq!(
                send(how, chunked(bytes.clone())),
                seen(None, true, size),
                "{how}, {size} bytes chunked:\n{}",
                dev.output()
            );
        }
    }
    assert_eq!(send("string", "".into()), seen(Some(5), false, 5));
    assert_eq!(send("fixed", "".into()), seen(Some(5), false, 5));
    assert_eq!(send("unknown", "".into()), seen(None, true, 5));
    assert_eq!(send("none", "".into()), seen(None, false, 0));
    assert_eq!(send("get", "".into()), seen(None, false, 0));
}
