// The container conformance Worker: one Durable Object calls every method
// of `ctx.container` and answers a JSON verdict per method. The same file
// runs on Cloudflare, on celld with Docker, and on celld with the krun
// engine; a call an engine does not support is reported as unsupported,
// not as a failure.
//
//   GET /run          the whole suite against a fresh object
//   GET /run?id=name  against a named object
import { DurableObject, WorkerEntrypoint } from "cloudflare:workers";

// Where intercepted requests land: the binding the container's traffic is
// handed to.
export class Interceptor extends WorkerEntrypoint {
  async fetch(request) {
    const url = new URL(request.url);
    return new Response(
      `intercepted ${request.method} ${url.protocol}//${url.host}${url.pathname}`,
    );
  }
}

const PORT = 8080;
const CHECK_MS = 60000;
const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));
const UNSUPPORTED = /not supported by celld|not implemented in celld/;

async function within(ms, what, fn) {
  const deadline = Date.now() + ms;
  let last;
  while (Date.now() < deadline) {
    try {
      const value = await fn();
      if (value !== undefined) return value;
    } catch (error) {
      last = error;
    }
    await sleep(100);
  }
  throw new Error(`${what} within ${ms} ms${last ? `: ${last.message}` : ""}`);
}

async function text(stream) {
  return stream ? await new Response(stream).text() : "";
}

export class Conformance extends DurableObject {
  async fetch() {
    const c = this.ctx.container;
    const results = {};
    // Each check has a budget, so one that hangs is a failure, not a hung
    // suite.
    const check = async (name, fn) => {
      const t0 = Date.now();
      try {
        const detail = await Promise.race([
          fn(),
          sleep(CHECK_MS).then(() => {
            throw new Error(`no answer within ${CHECK_MS} ms`);
          }),
        ]);
        results[name] = { ok: true, ms: Date.now() - t0, detail };
      } catch (error) {
        const message = error?.message ?? String(error);
        results[name] = UNSUPPORTED.test(message)
          ? { ok: null, unsupported: true, detail: message }
          : { ok: false, ms: Date.now() - t0, error: message };
      }
    };
    const expect = (cond, detail) => {
      if (!cond) throw new Error(`expected: ${JSON.stringify(detail)}`);
      return detail;
    };
    const port = () => c.getTcpPort(PORT);
    const get = async (path, ms = 15000) => {
      const r = await port().fetch(`http://container${path}`, { signal: AbortSignal.timeout(ms) });
      return [r.status, await r.text()];
    };
    const listening = () => within(30000, "the server listens", async () => {
      const [status] = await get("/", 3000);
      return status === 200 ? true : undefined;
    });
    const stop = async () => {
      if (c.running) {
        const ended = c.monitor().catch(() => {});
        await c.destroy();
        await ended;
      }
    };
    await stop();
    const image = c.images.base;

    await check("images", () => expect(typeof image === "string" && image.length > 0, c.images));
    await check("not_running_at_first", async () =>
      expect(!c.running && (await c.inspect()) === null, { running: c.running }));
    await check("start", async () => {
      c.start({
        image,
        enableInternet: false,
        env: { GREETING: "hi" },
        labels: { suite: "conformance" },
      });
      return expect(c.running, { running: c.running });
    });
    await check("getTcpPort_fetch", async () => expect(await listening(), "listening"));
    await check("inspect", async () => {
      const info = await within(10000, "inspect names the image", async () => {
        const i = await c.inspect();
        return i?.image ? i : undefined;
      });
      return expect(info.image === image && info.labels.suite === "conformance", info);
    });
    await check("start_env", async () => {
      const [, body] = await get("/env");
      return expect(JSON.parse(body).GREETING === "hi", "GREETING=hi");
    });
    await check("getTcpPort_connect", async () => {
      const socket = port().connect(`container:${PORT}`);
      const writer = socket.writable.getWriter();
      await writer.write(new TextEncoder().encode("GET / HTTP/1.0\r\nHost: c\r\n\r\n"));
      const reply = await text(socket.readable);
      return expect(reply.startsWith("HTTP/1.0 200") && reply.endsWith("ok"), reply.split("\r\n")[0]);
    });
    await check("exec_output", async () => {
      const p = await c.exec(["sh", "-c", "echo out; echo err >&2; exit 3"]);
      const o = await p.output();
      const out = new TextDecoder().decode(o.stdout);
      const err = new TextDecoder().decode(o.stderr);
      return expect(out === "out\n" && err === "err\n" && o.exitCode === 3 && p.pid > 0, { out, err, code: o.exitCode });
    });
    await check("exec_env_inherits_path_only", async () => {
      const p = await c.exec(["sh", "-c", 'echo "$GREETING|$PATH"']);
      const out = new TextDecoder().decode((await p.output()).stdout).trim();
      return expect(out.startsWith("|/"), out);
    });
    await check("exec_stdin", async () => {
      const p = await c.exec(["cat"], { stdin: "pipe" });
      const w = p.stdin.getWriter();
      await w.write(new TextEncoder().encode("piped"));
      await w.close();
      const out = await text(p.stdout);
      return expect(out === "piped" && (await p.exitCode) === 0, out);
    });
    await check("exec_stderr_combined", async () => {
      const p = await c.exec(["sh", "-c", "echo a; echo b >&2"], { stderr: "combined" });
      const out = await text(p.stdout);
      return expect(p.stderr === null && out.includes("a") && out.includes("b"), out);
    });
    await check("exec_pty_and_resize", async () => {
      const p = await c.exec(["sh", "-c", "stty size; read x; stty size"], {
        stdin: "pipe",
        pty: { cols: 50, rows: 20 },
      });
      const reader = p.stdout.getReader();
      let out = "";
      while (!out.includes("20 50")) {
        const { value, done } = await reader.read();
        if (done) break;
        out += new TextDecoder().decode(value);
      }
      p.resize(90, 30);
      await sleep(200);
      const w = p.stdin.getWriter();
      await w.write(new TextEncoder().encode("\n"));
      for (;;) {
        const { value, done } = await reader.read();
        if (done) break;
        out += new TextDecoder().decode(value);
      }
      return expect(p.isPty && out.includes("20 50") && out.includes("30 90"), out.replace(/\r/g, ""));
    });
    await check("exec_kill", async () => {
      const p = await c.exec(["sleep", "20"]);
      p.kill(9);
      return expect((await p.exitCode) === 137, await p.exitCode);
    });
    await check("exec_abort_signal", async () => {
      const abort = new AbortController();
      const p = await c.exec(["sleep", "20"], { signal: abort.signal });
      abort.abort();
      return expect((await p.exitCode) === 137, await p.exitCode);
    });
    await check("internet_off", async () => {
      const [, body] = await get(`/fetch?url=${encodeURIComponent("https://example.com/")}`);
      return expect(JSON.parse(body).error !== undefined, JSON.parse(body));
    });
    await check("interceptOutboundHttp", async () => {
      await c.interceptOutboundHttp("intercepted.example", this.ctx.exports.Interceptor);
      const [, body] = await get(`/fetch?url=${encodeURIComponent("http://intercepted.example/hello")}`);
      const r = JSON.parse(body);
      return expect(r.body === "intercepted GET http://intercepted.example/hello", r);
    });
    await check("interceptOutboundHttps", async () => {
      await c.interceptOutboundHttps("secure.example", this.ctx.exports.Interceptor);
      const [, body] = await get(`/fetch?url=${encodeURIComponent("https://secure.example/tls")}`);
      const r = JSON.parse(body);
      return expect(r.body === "intercepted GET https://secure.example/tls", r);
    });
    await check("interceptAllOutboundHttp", async () => {
      await c.interceptAllOutboundHttp(this.ctx.exports.Interceptor);
      const [, body] = await get(`/fetch?url=${encodeURIComponent("http://anything.example/all")}`);
      const r = JSON.parse(body);
      return expect(r.body === "intercepted GET http://anything.example/all", r);
    });
    await check("setInactivityTimeout", async () => {
      await c.setInactivityTimeout(60000);
      let refused = false;
      try {
        await c.setInactivityTimeout(21600001);
      } catch {
        refused = true;
      }
      return expect(refused, "past 6 hours is refused");
    });
    let snapshot;
    await check("snapshotContainer", async () => {
      await get("/write?v=snapshotted");
      snapshot = await c.snapshotContainer({ name: "conformance" });
      return expect(typeof snapshot.id === "string" && snapshot.size > 0 && snapshot.name === "conformance", snapshot);
    });
    await check("destroy_then_monitor_resolves", async () => {
      const ended = c.monitor();
      await c.destroy();
      await ended;
      return expect(!c.running, "monitor() resolved");
    });
    await check("start_from_snapshot", async () => {
      if (!snapshot) {
        const why = results.snapshotContainer;
        throw new Error(why?.unsupported ? why.detail : "no snapshot to start from");
      }
      c.start({ containerSnapshot: snapshot, enableInternet: false });
      await listening();
      const [, marker] = await get("/read");
      const info = await c.inspect();
      return expect(marker === "snapshotted" && info.image === "", { marker, image: info.image });
    });
    await stop();
    await check("monitor_rejects_exit_code", async () => {
      c.start({ image, enableInternet: false, entrypoint: ["sh", "-c", "exit 3"] });
      try {
        await c.monitor();
      } catch (error) {
        return expect(error.exitCode === 3, error.message);
      }
      throw new Error("monitor() resolved for exit 3");
    });
    await check("signal", async () => {
      c.start({ image, enableInternet: false });
      await listening();
      const ended = c.monitor().then(() => 0, (error) => error.exitCode);
      c.signal(15);
      return expect((await ended) === 42, await ended);
    });
    await check("destroy_with_error_rejects_monitor", async () => {
      c.start({ image, enableInternet: false });
      const ended = c.monitor().then(() => "resolved", (error) => error.message);
      await c.destroy(new Error("boom"));
      return expect((await ended) === "boom", await ended);
    });
    await stop();

    const verdicts = Object.values(results);
    const summary = {
      passed: verdicts.filter((r) => r.ok === true).length,
      failed: verdicts.filter((r) => r.ok === false).length,
      unsupported: verdicts.filter((r) => r.unsupported).length,
    };
    return Response.json({ summary, results });
  }
}

export default {
  async fetch(request, env) {
    const url = new URL(request.url);
    if (url.pathname !== "/run") return new Response("GET /run", { status: 404 });
    const name = url.searchParams.get("id") ?? `run-${Date.now()}`;
    return env.CONFORMANCE.get(env.CONFORMANCE.idFromName(name)).fetch(request);
  },
};
