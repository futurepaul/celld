//! A body a Worker forwards to a Durable Object is framed as workerd frames
//! it: a known length (an incoming request's declared one, a held body's, a
//! `FixedLengthStream`'s, a tee branch's) reaches the object as its
//! Content-Length, and an unknown one as chunked. A body read from declares
//! no length any more. Each expectation is what workerd 2026-09-30
//! answered for the same Worker.

#![cfg(unix)]
// A test of the binary drives real processes, sockets, files, and clocks.
#![allow(clippy::disallowed_methods)]

mod support;

use support::{project, Dev};

const WORKER: &str = r#"
import { WorkerEntrypoint } from "cloudflare:workers";

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

export class Echo extends WorkerEntrypoint {
  async fetch(request) {
    return new Box().fetch(request);
  }
}

// A prefix read off the body, then the rest forwarded to `to`.
const rest = async (request, to) => {
  const reader = request.body.getReader();
  const first = await reader.read();
  reader.releaseLock();
  const seen = await (await to(request.body)).json();
  return Response.json({ ...seen, first: first.value?.byteLength ?? 0 });
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
      case "rest": return rest(request, put);
      case "rest-service":
        return rest(request, (body) => env.ECHO.fetch("https://echo/", { method: "PUT", body }));
      case "teed": {
        const [left, right] = request.body.tee();
        const seen = await (await put(left)).json();
        return Response.json({ ...seen, other: (await new Response(right).arrayBuffer()).byteLength });
      }
      case "none": return box.fetch("https://box/", { method: "POST" });
      case "get": return box.fetch("https://box/");
    }
    return new Response("how?", { status: 400 });
  },
};
"#;

/// Held in memory: at most `INGRESS_STREAM_THRESHOLD` (1 MiB).
const HELD: usize = 300_000;

/// Past what celld holds in memory, so the body arrives as a stream.
const STREAMED: usize = 3_000_000;

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
    // A service binding to this Worker's own entrypoint, beside the object.
    let config = dir.path().join("wrangler.jsonc");
    let with_service = std::fs::read_to_string(&config).unwrap().replace(
        r#""durable_objects""#,
        r#""services": [{ "binding": "ECHO", "service": "test", "entrypoint": "Echo" }],
  "durable_objects""#,
    );
    std::fs::write(&config, with_service).unwrap();
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

    for size in [11, HELD, STREAMED] {
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
    // A body a prefix was read from declares no length any more: the rest
    // goes chunked, never under the whole's length. workerd refuses such a
    // stream as a body ("This ReadableStream is disturbed"); celld forwards
    // the rest. The same through a service binding.
    for size in [11, HELD, STREAMED] {
        for how in ["rest", "rest-service"] {
            let answer = send(how, vec![b'a'; size].into());
            let first = answer["first"].as_u64().unwrap() as usize;
            assert!(first > 0, "{how}, {size}: {answer}");
            assert_eq!(
                answer,
                serde_json::json!({
                    "length": null,
                    "encoding": "chunked",
                    "read": size - first,
                    "first": first,
                }),
                "{how}, {size} bytes:\n{}",
                dev.output()
            );
        }
    }
    // Each branch of a tee keeps the length, as on workerd.
    for size in [11, HELD, STREAMED] {
        let mut teed = seen(Some(size), false, size);
        teed["other"] = size.into();
        assert_eq!(send("teed", vec![b'a'; size].into()), teed, "teed, {size}:\n{}", dev.output());
    }
    assert_eq!(send("string", "".into()), seen(Some(5), false, 5));
    assert_eq!(send("fixed", "".into()), seen(Some(5), false, 5));
    assert_eq!(send("unknown", "".into()), seen(None, true, 5));
    assert_eq!(send("none", "".into()), seen(None, false, 0));
    assert_eq!(send("get", "".into()), seen(None, false, 0));
}
