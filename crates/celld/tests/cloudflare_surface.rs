//! A Worker written for Cloudflare finds its bindings on celld as it finds
//! them there: each with workerd's class name, which SDKs read to tell one
//! binding from another (workers-rs 0.8's `EnvBinding::get` compares
//! `constructor.name`), including the bindings of services a node does not
//! run (`send_email`, `images`, `browser`, `ai`), whose calls reject saying
//! so; its `secrets_store_secrets` read from the node's secrets backend; and
//! a container class on a node with no engine has its `ctx.container`, not
//! running, whose `start()` throws.

#![cfg(unix)]
// A test of the binary drives real processes, sockets, files, and clocks.
#![allow(clippy::disallowed_methods)]

mod support;

use std::os::unix::fs::PermissionsExt as _;
use std::path::Path;
use support::Dev;

const WORKER: &str = r#"
import { DurableObject, WorkerEntrypoint, WorkflowEntrypoint } from "cloudflare:workers";

export class Box extends DurableObject {}

export class Computer extends DurableObject {
  async fetch() {
    const c = this.ctx.container;
    let threw = null;
    try {
      c.start({ image: "stub" });
    } catch (e) {
      threw = String(e.message);
    }
    return Response.json({ has: c !== undefined, running: c.running, images: c.images, threw, after: c.running });
  }
}

export class Echo extends WorkerEntrypoint {
  async fetch() {
    return new Response("echo");
  }
}

export class Flow extends WorkflowEntrypoint {
  async run() {}
}

const NAMES = [
  "BOX", "COMPUTER", "BLOBS", "ASSETS", "ECHO", "DB", "QUEUE", "KV", "FLOW",
  "LOADER", "SECRET", "OTHER_SECRET", "EMAIL", "IMAGES", "BROWSER", "AI",
];

// Each call's settlement: its value, or its error's name, code and message.
const settle = async (f) => {
  try {
    return { value: await f() };
  } catch (e) {
    return { name: e?.name ?? null, code: e?.code ?? null, message: String(e?.message ?? e) };
  }
};

export default {
  async fetch(request, env) {
    switch (new URL(request.url).pathname) {
      case "/names":
        return Response.json(Object.fromEntries(NAMES.map((n) => [n, env[n]?.constructor?.name ?? null])));
      case "/secrets":
        return Response.json({
          secret: await settle(() => env.SECRET.get()),
          other: await settle(() => env.OTHER_SECRET.get()),
          again: await settle(() => env.SECRET.get()),
        });
      case "/stand-ins": {
        const stream = () => new Response(new Uint8Array([1, 2, 3])).body;
        return Response.json({
          email: await settle(() => env.EMAIL.send({ from: "a@example.com", to: "b@example.com", subject: "s", text: "t" })),
          output: await settle(() => env.IMAGES.input(stream()).transform({ width: 8 }).output({ format: "image/png" })),
          info: await settle(() => env.IMAGES.info(stream())),
          browser: await settle(() => env.BROWSER.fetch("https://example.com/")),
          ai: await settle(() => env.AI.run("@cf/meta/llama", { prompt: "x" })),
          // still the bindings they were: an assets fetch and a service call work
          asset: await settle(async () => (await env.ASSETS.fetch("https://assets.invalid/a.txt")).text()),
          echo: await settle(async () => (await env.ECHO.fetch("https://echo/")).text()),
          blob: await settle(async () => {
            await env.BLOBS.put("k", "v");
            return (await env.BLOBS.get("k")).text();
          }),
        });
      }
      case "/computer":
        return env.COMPUTER.getByName("c").fetch(request);
    }
    return new Response("where?", { status: 404 });
  },
};
"#;

const CONFIG: &str = r#"{
  "name": "test",
  "main": "index.js",
  "compatibility_date": "2026-01-01",
  "no_bundle": true,
  "assets": { "directory": "public", "binding": "ASSETS", "run_worker_first": true },
  "durable_objects": { "bindings": [
    { "name": "BOX", "class_name": "Box" },
    { "name": "COMPUTER", "class_name": "Computer" }
  ] },
  "migrations": [{ "tag": "v1", "new_sqlite_classes": ["Box", "Computer"] }],
  "containers": [{
    "class_name": "Computer",
    "scheduling_policy": "durable_object",
    "images": { "stub": { "dockerfile": "./Dockerfile" } }
  }],
  "r2_buckets": [{ "binding": "BLOBS", "bucket_name": "blobs" }],
  "services": [{ "binding": "ECHO", "service": "test", "entrypoint": "Echo" }],
  "d1_databases": [{ "binding": "DB", "database_name": "db" }],
  "queues": { "producers": [{ "binding": "QUEUE", "queue": "q" }] },
  "kv_namespaces": [{ "binding": "KV", "id": "kv" }],
  "workflows": [{ "binding": "FLOW", "name": "flow", "class_name": "Flow" }],
  "worker_loaders": [{ "binding": "LOADER" }],
  "secrets_store_secrets": [
    { "binding": "SECRET", "store_id": "0123456789abcdef0123456789abcdef", "secret_name": "app-secret" },
    { "binding": "OTHER_SECRET", "store_id": "0123456789abcdef0123456789abcdef", "secret_name": "not-held" }
  ],
  "send_email": [{ "name": "EMAIL" }],
  "images": { "binding": "IMAGES" },
  "browser": { "binding": "BROWSER" },
  "ai": { "binding": "AI" }
}"#;

fn project(dir: &Path) {
    std::fs::write(dir.join("wrangler.jsonc"), CONFIG).unwrap();
    std::fs::write(dir.join("index.js"), WORKER).unwrap();
    // never built: a node with no engine builds no image
    std::fs::write(dir.join("Dockerfile"), "FROM scratch\n").unwrap();
    std::fs::create_dir(dir.join("public")).unwrap();
    std::fs::write(dir.join("public/a.txt"), "an asset").unwrap();
}

/// The node's settings: no container engine, and a `docker` that fails
/// whatever it is asked, so an image build would stop the deploy.
fn no_engine(dir: &Path) -> Vec<(&'static str, String)> {
    let docker = dir.join("docker-refuses");
    std::fs::write(&docker, "#!/bin/sh\necho \"docker was run: $*\" >&2\nexit 1\n").unwrap();
    std::fs::set_permissions(&docker, std::fs::Permissions::from_mode(0o755)).unwrap();
    vec![("CELLD_CONTAINER_ENGINE", "none".to_string()), ("CELLD_DOCKER", docker.display().to_string())]
}

fn secrets_file(dir: &Path, mode: u32) -> String {
    let path = dir.join("secrets.json");
    std::fs::write(&path, r#"{"backend":"vars","values":{"app-secret":"the-value","unbound-one":"x"}}"#).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
    path.display().to_string()
}

fn start(dir: &Path, secrets: Option<String>) -> Dev {
    let mut env = no_engine(dir);
    if let Some(file) = secrets {
        env.push(("CELLD_SECRETS_FILE", file));
    }
    let env: Vec<(&str, &str)> = env.iter().map(|(k, v)| (*k, v.as_str())).collect();
    Dev::start_with(dir, &env)
}

// Goal: every binding's class is workerd's, the ones workers-rs checks
// among them; the stand-ins reject saying which service the node lacks
// (mail as Email Sending's own refusal, typed); the other bindings still
// work beside them; and a container class on a node with no engine has a
// container that is not running and whose start throws, its deploy having
// built no image. Replay: asked twice, the same answers.
#[test]
fn bindings_have_workerds_classes_and_stand_ins_reject() {
    let dir = tempfile::tempdir().unwrap();
    project(dir.path());
    let dev = start(dir.path(), Some(secrets_file(dir.path(), 0o600)));
    let expected = serde_json::json!({
        "BOX": "DurableObjectNamespace",
        "COMPUTER": "DurableObjectNamespace",
        "BLOBS": "R2Bucket",
        "ASSETS": "Fetcher",
        "ECHO": "Fetcher",
        "DB": "D1Database",
        "QUEUE": "WorkerQueue",
        "KV": "KvNamespace",
        "FLOW": "WorkflowImpl",
        "LOADER": "WorkerLoader",
        "SECRET": "Fetcher",
        "OTHER_SECRET": "Fetcher",
        "EMAIL": "SendEmail",
        "IMAGES": "ImagesBindingImpl",
        "BROWSER": "Fetcher",
        "AI": "Ai",
    });
    for _ in 0..2 {
        assert_eq!(dev.json("/names").0, expected, "{}", dev.output());
    }

    for _ in 0..2 {
        let (got, _) = dev.json("/stand-ins");
        let refused = |key: &str, message: &str| {
            assert_eq!(got[key]["message"], message, "{key}: {got}");
            assert!(got[key].get("value").is_none(), "{key}: {got}");
        };
        refused("email", "this node has no mailer");
        assert_eq!(got["email"]["code"], "E_NO_MAILER", "{got}");
        refused("output", "this node has no Images");
        refused("info", "this node has no Images");
        refused("browser", "this node has no Browser Rendering");
        refused("ai", "this node has no Workers AI");
        assert_eq!(got["asset"]["value"], "an asset", "{got}");
        assert_eq!(got["echo"]["value"], "echo", "{got}");
        assert_eq!(got["blob"]["value"], "v", "{got}");
    }

    for _ in 0..2 {
        let (got, _) = dev.json("/computer");
        assert_eq!(
            got,
            serde_json::json!({ "has": true, "running": false, "images": {}, "threw": "this node runs no computers", "after": false }),
            "{}",
            dev.output()
        );
    }
    assert!(!dev.output().contains("docker was run"), "no image was built:\n{}", dev.output());
}

// Goal: a secrets_store_secrets binding answers its secret from the node's
// backend; one the backend does not hold rejects (missing), as workerd's
// does; with no backend, every read rejects (unbound). A fleet node refuses
// the vars backend, and a secrets file others may read stops the start.
#[test]
fn secrets_read_from_the_nodes_backend_alone() {
    let dir = tempfile::tempdir().unwrap();
    project(dir.path());
    {
        let dev = start(dir.path(), Some(secrets_file(dir.path(), 0o600)));
        for _ in 0..2 {
            let (got, _) = dev.json("/secrets");
            assert_eq!(got["secret"], serde_json::json!({ "value": "the-value" }), "{got}");
            assert_eq!(got["again"], serde_json::json!({ "value": "the-value" }), "{got}");
            assert_eq!(got["other"]["name"], "SecretsError", "{got}");
            assert_eq!(got["other"]["code"], "missing", "{got}");
            assert!(got["other"]["message"].as_str().unwrap().contains("not-held is not among this node's secrets"), "{got}");
        }
        assert!(!dev.output().contains("the-value"), "no value in the node's log");
    }

    let other = tempfile::tempdir().unwrap();
    project(other.path());
    let dev = start(other.path(), None);
    let (got, _) = dev.json("/secrets");
    assert_eq!(got["secret"]["code"], "unbound", "{got}");
    assert!(got["secret"]["message"].as_str().unwrap().contains("this node has no secrets backend"), "{got}");
    drop(dev);

    // a fleet node: vars is refused before it starts, saying why
    let file = secrets_file(dir.path(), 0o600);
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_celld"))
        .args(["--no-control-plane", "--listen", "127.0.0.1:0"])
        .env("CELLD_SECRETS_FILE", &file)
        .env_remove("CELLD_INTERNAL_DEV_STORE")
        .output()
        .unwrap();
    let said = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success() && said.contains("`celld dev`'s alone"), "{said}");

    // a file others may read stops `celld dev` too
    let loose = secrets_file(dir.path(), 0o644);
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_celld"))
        .arg("dev")
        .arg(dir.path())
        .args(["--no-watch"])
        .env("CELLD_SECRETS_FILE", &loose)
        .output()
        .unwrap();
    let said = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success() && said.contains("readable by others"), "{said}");
}
