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
  ~3.3 s, and one renewal that stalled past its 2.5 s timeout (the box
  was busy) fenced the node ("SELF-FENCE: node lease not renewed within
  TTL", exit 3), taking the whole dev stack down. No setting lengthens the
  lease. Seen on v0.6.3, 2026-10-22.
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
