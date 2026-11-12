//! A facet whose code locks its own `ctx.facets` (fragment's platform does,
//! so an app starts no facets of its own) is still aborted, and released
//! with its root: celld reaches the facet manager through a handle of its
//! own, which nothing on the state leads to, so the app can neither use the
//! manager nor replace the release.

#![cfg(unix)]
// A test of the binary drives real processes, sockets, files, and clocks.
#![allow(clippy::disallowed_methods)]

mod support;

use support::Dev;

const ROOT: &str = r#"
import { DurableObject } from "cloudflare:workers";

const APP = `
import { DurableObject } from "cloudflare:workers";

// A facet manager, wherever it is found: what an app locked out of facets
// must never reach.
const isManager = (value) => value !== null &&
  (typeof value === "object" || typeof value === "function") &&
  ["get", "abort", "delete"].every((name) => typeof value[name] === "function");

// Every property of the state and of its storage, own and inherited, string
// and symbol keyed, read through its descriptor: a key holding a manager.
const managers = (ctx) => {
  const found = [];
  for (const [where, root] of [["ctx", ctx], ["storage", ctx.storage]]) {
    for (let o = root, depth = 0; o !== null && depth < 8; o = Object.getPrototypeOf(o), depth++) {
      for (const key of Reflect.ownKeys(o)) {
        const descriptor = Object.getOwnPropertyDescriptor(o, key);
        let value;
        try {
          value = "value" in descriptor ? descriptor.value : descriptor.get?.call(root);
        } catch {
          continue;
        }
        if (isManager(value)) found.push(where + "." + String(key));
      }
    }
  }
  return found;
};

export class Escape extends DurableObject {
  fetch() {
    return new Response("unauthorized facet running");
  }
}

export class App extends DurableObject {
  constructor(ctx, env) {
    Object.defineProperty(ctx, "facets", {
      get() { throw new Error("an app starts no facets of its own"); },
      configurable: false,
    });
    super(ctx, env);
    this.calls = 0;
    // What an app may try against the host's release of it: replace each
    // property holding a release with one that throws. Not in the first
    // release, which probes the state as the host built it.
    if (RELEASE !== "one") for (const key of Reflect.ownKeys(ctx)) {
      try {
        if (typeof ctx[key]?._release === "function")
          ctx[key] = { _release() { throw new Error("the app replaced the release"); } };
      } catch {}
    }
  }
  async fetch(request) {
    if (new URL(request.url).pathname === "/probe") {
      let nested;
      try {
        // Codex's probe: the manager under another name, starting a facet.
        const stub = this.ctx._facets.get("unauthorized", () => ({ class: this.ctx.exports.Escape }));
        nested = await (await stub.fetch("https://escape/")).text();
      } catch (error) {
        nested = "refused";
      }
      return Response.json({ managers: managers(this.ctx), nested });
    }
    this.calls += 1;
    return new Response(RELEASE + " call " + this.calls);
  }
}`;

export class Root extends DurableObject {
  async fetch(request) {
    const release = new URL(request.url).searchParams.get("release");
    if (release !== this.release) {
      if (this.release !== undefined) this.ctx.facets.abort("app", new Error("a newer release"));
      this.release = release;
    }
    const app = this.ctx.facets.get("app", () => ({
      class: this.env.LOADER.get(release, () => ({
        compatibilityDate: "2026-01-01",
        mainModule: "app.js",
        modules: { "app.js": `const RELEASE = "${release}";\n` + APP },
      })).getDurableObjectClass("App"),
    }));
    try {
      return await app.fetch(new URL(request.url).searchParams.has("probe") ? "https://app/probe" : "https://app/");
    } catch (error) {
      return new Response(String(error), { status: 500 });
    }
  }
}

export default {
  fetch(request, env) {
    return env.ROOT.getByName("root").fetch(request);
  },
};
"#;

fn project(dir: &std::path::Path) {
    std::fs::write(
        dir.join("wrangler.jsonc"),
        r#"{
  "name": "facets",
  "main": "index.js",
  "compatibility_date": "2026-01-01",
  "no_bundle": true,
  "worker_loaders": [{ "binding": "LOADER" }],
  "durable_objects": { "bindings": [{ "name": "ROOT", "class_name": "Root" }] },
  "migrations": [{ "tag": "v1", "new_sqlite_classes": ["Root"] }]
}"#,
    )
    .unwrap();
    std::fs::write(dir.join("index.js"), ROOT).unwrap();
}

#[test]
fn a_facet_that_locks_its_facets_is_aborted_for_a_newer_release() {
    let dir = tempfile::tempdir().unwrap();
    project(dir.path());
    let dev = Dev::start(dir.path());
    let call = |release: &str| {
        let (status, body) = dev
            .get(&format!("/?release={release}"))
            .unwrap_or_else(|error| panic!("{release}: {error}\n{}", dev.output()));
        assert_eq!(status, 200, "{release}: {body}\n{}", dev.output());
        body
    };

    assert_eq!(call("one"), "one call 1");
    // The app reaches no facet manager through its state, and starts no
    // facet of its own.
    let probe: serde_json::Value = serde_json::from_str(&call("one&probe")).unwrap();
    assert_eq!(
        probe,
        serde_json::json!({ "managers": [], "nested": "refused" }),
        "{}",
        dev.output()
    );
    assert_eq!(call("one"), "one call 2");
    // The abort releases the running facet; the next get starts the newer
    // release's in its place.
    assert_eq!(call("two"), "two call 1");
    assert_eq!(call("two"), "two call 2");
    assert_eq!(call("three"), "three call 1");
    let output = dev.output();
    assert!(
        !output.contains("adopt failed") && !output.contains("did not close"),
        "{output}"
    );
}
