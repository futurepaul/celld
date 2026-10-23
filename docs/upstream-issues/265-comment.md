<!--
DRAFT for Paul to post himself, as a COMMENT on denoland/celld#265, not a new
issue: #265 ("celld dev: each list of the local store reads the whole
objects.sqlite3", kayue, open since 2026-10-12) is found 34 exactly, and two
comments there already add numbers and a fork fix (hazem3500/celld#1).
This comment adds what #265 lacks: v0.6.3 checked, a deterministic repro, and
the listings blocking runtime threads. The repro is in
/home/futurepaul/dev/finite/celld-issue-localstore-repro/ (index.js,
wrangler.jsonc). Verified 2026-10-22 on the unmodified v0.6.3 release binary
(celld-x86_64-unknown-linux-gnu, from `gh release download v0.6.3`).
Everything below the line is the comment.
-->

The following is AI-written (Claude Opus 5.5). It hit this in our e2e suite under `celld dev` and reduced it to the repro below.

Still in v0.6.3 (`8702f6d`, the release binary): `local_store.rs` is unchanged since v0.6.0. Two things to add:

1. **Listings block runtime threads.** `get_opts`, `put_opts`, `delete` and `copy` run their SQLite work in `asyncrt::blocking`, but `list`, `list_with_delimiter` and `list_paginated` call `metadata()` inline, on the async runtime's thread. So many listings at once take every runtime thread for a whole-file read each. In our suite, about 95 cells restore at once after a crash, with a ~200 MB store: the node read 130 GB in 45 s, all 32 runtime threads busy, each restore's source lookup took 17 to 33 s, and four crash checks timed out (one as `DurabilityUnproven`).
2. **A deterministic repro.** Listing an R2 prefix that holds one object costs a read of the whole store.

`wrangler.jsonc`:

```jsonc
{
  "name": "local-store-list-repro",
  "main": "index.js",
  "compatibility_date": "2026-01-01",
  "r2_buckets": [{ "binding": "BUCKET", "bucket_name": "repro" }]
}
```

`index.js`:

```js
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
```

```sh
celld dev --port 8799 --no-watch &
for i in $(seq 0 200 3000); do curl -s "localhost:8799/fill?from=$i&n=200"; done  # 3,200 x 64 KiB under fill/
curl -s -o /dev/null -w '%{time_total}\n' localhost:8799/list                     # one/: 1 object
time (for i in $(seq 32); do curl -s -o /dev/null localhost:8799/list & done; wait)
```

Listing `one/`, median of 7 (curl's `time_total`; Linux x86_64, 32 threads, warm page cache):

| `objects.sqlite3` | v0.6.3 | v0.6.3 + prefix range |
|---|---|---|
| 1 MB | 0.6 ms | 0.4 ms |
| 51 MB | 6.7 ms | 0.3 ms |
| 101 MB | 12.5 ms | 0.6 ms |
| 202 MB | 22.4 ms | 0.4 ms |
| 202 MB, 32 listings at once | 2,180 ms | 25 ms |

The prefix range alone (no new index) was enough for us. `metadata(prefix)` runs `WHERE key >= ?1 ORDER BY key` and stops at the first key that does not start with the prefix, so a listing reads only its prefix's run of the primary key; the callers filter as before. On a copy of our suite's crashed store, restores were ready in 2 s rather than 36 s, lookups took 17 ms, and the node read 842 MiB in 45 s rather than 130 GB. It's on our fork as one commit on v0.6.3: https://github.com/futurepaul/celld/commit/fcf061081ccf6f63f7fd95bad35ce0b3ba4e9943

Whatever the fix, moving the three listings into `asyncrt::blocking`, as the other operations are, would also keep a slow listing off the runtime's threads.
