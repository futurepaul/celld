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
    const tag = this.ctx.props?.tag;
    if (request.headers.get("upgrade")?.toLowerCase() === "websocket") {
      const [client, server] = Object.values(new WebSocketPair());
      server.accept();
      server.addEventListener("message", (e) => server.send(`intercepted ${url.host}${url.pathname}: ${e.data}`));
      return new Response(null, { status: 101, webSocket: client });
    }
    if (request.method === "POST") {
      let bytes = 0;
      for await (const chunk of request.body) bytes += chunk.byteLength;
      return new Response(`intercepted POST ${url.host}${url.pathname}: ${bytes} bytes`);
    }
    return new Response(
      `${tag ? `${tag}: ` : ""}intercepted ${request.method} ${url.protocol}//${url.host}${url.pathname}`,
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
  async fetch(request) {
    const c = this.ctx.container;
    // A monitor() set by an event that then answers: the container's exit
    // settles it later all the same (`monitor_after_its_event`).
    const step = new URL(request.url).pathname;
    if (step === "/monitor/start") {
      await this.ctx.storage.delete("ended");
      c.start({ image: c.images.base, enableInternet: false });
      c.monitor().then(
        () => this.ctx.storage.put("ended", "resolved"),
        (error) => this.ctx.storage.put("ended", `exit ${error.exitCode}`),
      );
      return new Response("started");
    }
    if (step === "/monitor/kill") {
      c.signal(9);
      return new Response("killed");
    }
    // A watch left by an answered event, its run ended while another event
    // waited, and that event starting the next run before any other event
    // could settle the watch: the watch still hears its own run's end
    // (`monitor_across_a_restart`).
    if (step === "/restart/start") {
      await this.ctx.storage.delete("restart");
      // it ends on its own, 8 s on: no event's kill needs to go out
      c.start({ image: c.images.base, enableInternet: false, entrypoint: ["sh", "-c", "sleep 8; exit 3"] });
      c.monitor().then(
        () => this.ctx.storage.put("restart", "resolved"),
        (error) => this.ctx.storage.put("restart", `exit ${error.exitCode}`),
      );
      return new Response("started");
    }
    if (step === "/restart/race") {
      // Held in this turn while the container stops, so no other event (an
      // exit's wake included) runs between its end and the next start; a
      // runtime whose `running` moves only between turns ends the loop by
      // its count and waits as any event would.
      let reads = 0;
      const d0 = Date.now();
      for (; c.running && reads < 1e8; reads++);
      const inTurn = !c.running;
      // and a while longer in the turn, as an event doing other work would,
      // so the end is recorded before the next start opens its run
      for (let i = 0; i < 3e5; i++) c.running;
      const spun = Date.now() - d0;
      const t0 = Date.now();
      while (c.running && Date.now() - t0 < 10000) await sleep(1);
      c.start({ image: c.images.base, enableInternet: false });
      return Response.json({ reads, inTurn, spun });
    }
    if (step === "/restart/ended") {
      return Response.json({ ended: (await this.ctx.storage.get("restart")) ?? null });
    }
    if (step === "/restart/stop") {
      if (c.running) {
        const ended = c.monitor().catch(() => {});
        await c.destroy();
        await ended;
      }
      return new Response("stopped");
    }
    if (step === "/monitor/ended") {
      return Response.json({ ended: (await this.ctx.storage.get("ended")) ?? null, running: c.running });
    }
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
    // An intercept set while the start is under way, as an object sets its
    // own right after `start()`, applies once the container is up. First of
    // the intercepts: the catch-all below would answer any host.
    await check("interceptOutboundHttp_while_starting", async () => {
      c.start({ image, enableInternet: false });
      await c.interceptOutboundHttp("early.example", this.ctx.exports.Interceptor);
      await listening();
      const [, body] = await get(`/fetch?url=${encodeURIComponent("http://early.example/early")}`);
      const r = JSON.parse(body);
      return expect(r.body === "intercepted GET http://early.example/early", r);
    });
    await stop();
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
    // A WebSocket to an intercepted host reaches its binding, which answers it.
    await check("interceptOutboundHttp_websocket", async () => {
      await c.interceptOutboundHttp("socket.example", this.ctx.exports.Interceptor);
      const [, body] = await get(`/ws?url=${encodeURIComponent("ws://socket.example/talk")}&say=hello`);
      const r = JSON.parse(body);
      return expect(r.said === "intercepted socket.example/talk: hello", r);
    });
    // A request body larger than any held whole (40 MiB) reaches the binding.
    await check("interceptOutboundHttp_large_body", async () => {
      const [, body] = await get(`/post?url=${encodeURIComponent("http://intercepted.example/big")}&mb=40`, 60000);
      const r = JSON.parse(body);
      return expect(r.body === `intercepted POST intercepted.example/big: ${40 << 20} bytes`, r);
    });
    // A host intercepted again takes the new binding in its place, and
    // counts once: the Sandbox SDK's DirectoryBackup registers its host
    // again for each grant. 70 times is 140 entries, past the 128 a
    // container takes.
    await check("interceptOutboundHttp_again_replaces", async () => {
      for (let i = 1; i <= 70; i++) {
        await c.interceptOutboundHttp("intercepted.example", this.ctx.exports.Interceptor({ props: { tag: `again ${i}` } }));
      }
      const [, body] = await get(`/fetch?url=${encodeURIComponent("http://intercepted.example/hello")}`);
      const r = JSON.parse(body);
      return expect(r.body === "again 70: intercepted GET http://intercepted.example/hello", r);
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
    // An intercept set while a start is under way answers for the rule the
    // engine took, or fails: one it refuses (a host with an underscore) is
    // the call's error, never a success the guest finds nothing behind.
    // Last, since a runtime may keep the refused rule for the object's life.
    await check("interceptOutboundHttp_refused_while_starting", async () => {
      c.start({ image, enableInternet: false });
      let refused = null;
      try {
        await c.interceptOutboundHttp("not_a_host.example", this.ctx.exports.Interceptor);
      } catch (error) {
        refused = error?.message ?? String(error);
      }
      await stop();
      return expect(refused !== null, { refused });
    });

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
    const answer = await env.CONFORMANCE.get(env.CONFORMANCE.idFromName(name)).fetch(request);
    const verdict = await answer.json();
    // Each step its own event on a fresh object: the monitor() was set by an
    // event that answered before the container was killed.
    const t0 = Date.now();
    const box = env.CONFORMANCE.get(env.CONFORMANCE.idFromName(`${name}-monitor`));
    const step = (path) => box.fetch(`http://conformance${path}`);
    let result;
    try {
      await step("/monitor/start");
      await sleep(5000);
      await step("/monitor/kill");
      const ended = await within(30000, "the exit settles monitor()", async () => {
        const seen = await (await step("/monitor/ended")).json();
        return seen.ended === null ? undefined : seen;
      });
      result = ended.ended === "exit 137"
        ? { ok: true, ms: Date.now() - t0, detail: ended }
        : { ok: false, ms: Date.now() - t0, error: `expected: ${JSON.stringify(ended)}` };
    } catch (error) {
      result = { ok: false, ms: Date.now() - t0, error: error?.message ?? String(error) };
    }
    verdict.results.monitor_after_its_event = result;
    verdict.summary[result.ok ? "passed" : "failed"] += 1;
    // A restart with an event pending (`/restart/*` above), on a fresh object.
    const t1 = Date.now();
    const again = env.CONFORMANCE.get(env.CONFORMANCE.idFromName(`${name}-restart`));
    const at = (path) => again.fetch(`http://conformance${path}`);
    let restart;
    try {
      await at("/restart/start");
      await sleep(6000);
      const raced = await (await at("/restart/race")).json();
      await sleep(3000);
      const seen = { ...(await (await at("/restart/ended")).json()), raced };
      restart = seen.ended === "exit 3"
        ? { ok: true, ms: Date.now() - t1, detail: seen }
        : { ok: false, ms: Date.now() - t1, error: `expected: ${JSON.stringify(seen)}` };
    } catch (error) {
      restart = { ok: false, ms: Date.now() - t1, error: error?.message ?? String(error) };
    } finally {
      await at("/restart/stop").catch(() => {});
    }
    verdict.results.monitor_across_a_restart = restart;
    verdict.summary[restart.ok ? "passed" : "failed"] += 1;
    return Response.json(verdict);
  },
};
