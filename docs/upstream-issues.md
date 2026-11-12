# Upstream celld issues

What fragment's self-hosted lane found in upstream celld (denoland/celld),
kept here rather than posted. Paul posts upstream himself: drafts are
terse, with a minimal repro checked on an unmodified release. Last
checked against v0.6.3 (2026-10-22).

## Posted

- **#254** (Paul, 2026-10-06): in a Durable Object, an outbound WebSocket
  opened after the request has responded fails with "isolate stopped
  reading WebSocket" (the continuation's actor scope is empty). Open, no
  replies; still so on v0.6.3. fragment works around it: a container's
  reports come back in as requests of their own.

## Upstream's own issue, our comment drafted

- **#265** (kayue): `celld dev`'s `LocalStore` reads its whole bucket for
  every listing, and on the runtime's threads. Our comment adds the v0.6.3
  check, a deterministic repro and the thread starvation:
  `upstream-issues/265-comment.md`, repro in `upstream-issues/265-repro/`.
  This fork's `selfhost` carries a prefix-range stopgap (on v0.6.3).

## Not posted

- **Binding classes read as `Object`.** R2 (`R2Bucket`), assets and
  service bindings (`Fetcher`) all report `Object` as their class, so
  workers-rs's typed reads refuse them ("Binding cannot be cast to the
  type R2Bucket from Object"). fragment reads them by what they do.
- **An early 413 can be lost.** A Worker that answers 413 from a request's
  declared length, before reading its body, has the connection closed
  under the upload (hyper closes on the unread body), so the client sees a
  broken write rather than the 413. Needs a lingering close.
- **A killed client's WebSocket lingers.** On celld, a browser killed
  outright stayed in a fragment's presence ("you and 1 other here") where
  workerd dropped it. fragment's card renderer now closes its browser
  before killing it.
- **`celld dev` halts itself on one slow lease renewal.** A single node,
  with no peers to hand cells to, still renews a 10 s node lease every
  ~3.3 s, and one renewal that stalled past its 2.5 s timeout (its disk
  was busy: writeback holds every call on the dev store's SQLite file,
  not a starved thread; measured, with a repro, in
  `upstream-issues/dev-self-fence.md`, 2026-11-11) fenced the node
  ("SELF-FENCE: node lease not renewed within
  TTL", exit 3), taking the whole dev stack down. No setting lengthens the
  lease. Seen on v0.6.3, 2026-10-22.
- **An alarm more than about two years ahead panics the node.** In any
  object, `await ctx.storage.setAlarm(Date.UTC(2100, 0, 1))` stops
  `celld dev` at once: `thread 'celld-core' panicked at
  crates/celld/actor/effects.rs:252:26: invalid deadline; err=Invalid`
  (tokio-util's `DelayQueue` takes at most 2^36 - 1 ms). Once such an
  alarm is stored, every later start panics the same way. workerd takes
  it. Seen on v0.6.3, 2026-11-11; fixed on this fork (PR #9).
- **A body forwarded to an object loses its length.** In a Worker,
  `env.BOX.getByName("x").fetch("https://x/", { method: "PUT", body:
  request.body })` gives the object neither Content-Length nor
  Transfer-Encoding, whatever the incoming request declared; so does a
  string body or a `FixedLengthStream`. workerd sends a known length as
  Content-Length and an unknown one as chunked. `fetch(request)`, the
  request forwarded whole, keeps its headers. Seen on v0.6.3,
  2026-11-11; fixed on this fork (PR #9).
- **A teed upload can hang.** A Worker that tees an incoming body past
  1 MiB (a host stream), forwards one branch to an object and then reads
  the other sometimes never answers: `curl --data-binary @3mb.bin` timed
  out in 2 of 4 tries, with or without `Expect: 100-continue`. Seen on
  v0.6.3, 2026-11-12, while checking a tee's length for PR #9; not
  fixed.
- **A private CA reaches only `fetch`.** Stock celld trusts
  `SSL_CERT_FILE` for a Worker's `fetch`, but not for outbound WebSockets
  or `connect()`. This fork's `CELLD_EXTRA_CA_FILE` covers all three.

## Fixed on this fork, not raised upstream

Each fix's test fails on stock v0.6.3:

- a state's `facets` is assigned, not defined (`krun-engine`, fork PR #1);
- an answer held behind a socket's close: 30 s, the handler's leftover
  timer (fork PR #3);
- a socket one object returns to another loses its close: `error`, then
  1006 (fork PR #4);
- `CELLD_EGRESS_PUBLIC_ONLY` covers outbound WebSockets and `connect()`
  (fork PR #2).
