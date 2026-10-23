// `one/` holds one object. `/fill` puts 64 KiB objects under `fill/`, and
// `/list` lists `one/`, which should not cost more as `fill/` grows.
export default {
  async fetch(request, env) {
    const url = new URL(request.url);
    if (url.pathname === "/fill") {
      const from = Number(url.searchParams.get("from") ?? 0);
      const n = Number(url.searchParams.get("n") ?? 100);
      const body = new Uint8Array(64 * 1024);
      for (let i = from; i < from + n; i++) {
        await env.BUCKET.put(`fill/${String(i).padStart(6, "0")}`, body);
      }
      await env.BUCKET.put("one/x", "x");
      return new Response(`fill/ ${from}..${from + n}\n`);
    }
    if (url.pathname === "/list") {
      const listed = await env.BUCKET.list({ prefix: "one/" });
      return new Response(`${listed.objects.length}\n`);
    }
    return new Response("not found\n", { status: 404 });
  },
};
