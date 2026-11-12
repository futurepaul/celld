# `celld dev` fences itself when its disk is busy

Measured on the fork's `selfhost` (98d5272, v0.6.3 and the fork's fixes),
2026-11-11, on one Linux box: btrfs `/home` on NVMe, 32 cores.

## What happens

`celld dev` renews its node lease every ~3.3 s against a 10 s TTL. A
renewal is a compare-and-swap on the development store, a SQLite file at
`PROJECT/.celld/dev/objects.sqlite3`, bounded at 2.5 s. A timed-out
renewal is ambiguous, and the node then reads its lease back, with no
bound. When neither returns before the TTL, the watchdog fences the node
(`SELF-FENCE: node lease not renewed within TTL`, exit 3) and the dev
stack stops.

## The cause: the store's disk, not a thread

- **Not the runtime's threads.** The renewal runs on the core thread
  (`celld-core`), off the host workers. The store's calls run on Tokio's
  blocking pool (512 threads). In the box's fence on 2026-10-22, every
  store user stalled together: the renewal (2.5 s, then its read-back,
  which never returned), a listing (`posture repair failed … list`), and
  the LTX ship loop (5.5 s idle).
- **Disk writeback alone does it.** `run.sh` runs an idle `celld dev`
  with its state on this disk while `dd` writes 16 GB to the same disk
  and fsyncs. The load average stayed between 2 and 5. The node fenced
  within 20 s in each of three runs: a release build, a debug build, and
  a debug build with `NORMAL` (below). The renewals timed out at 2.5 s
  and the read-backs never landed. The stock v0.6.3 release binary
  fenced in two runs of three. In the third, one renewal timed out and
  the node survived.
- **Every SQLite call waits, read or write, whatever its `synchronous`.**
  `probe.py` timed one call every 200 ms, each on a fresh connection as
  the store makes them, beside 16 GB of writeback:

  | call | slowest |
  |---|---|
  | write, `synchronous=FULL` (the store's setting) | 10.8 s |
  | write, `synchronous=NORMAL` | 10.8 s |
  | read, WAL | 3.6 s |

  A `celld dev` built with `NORMAL` fenced as well. So the commit's fsync
  is not the cause: the filesystem holds every writer, and some readers,
  while it writes back.
- **Off that disk, nothing stalls.** With the project, and so its state,
  on tmpfs and the same load on `/home`, 17 renewals took at most 1 ms
  and the node stayed up.

## What would and would not help

- **A dedicated thread, a priority, or a fairer timer would not.** The
  timer fires on time and the attempt starts on time, then waits on the
  file system.
- **Keep the dev state off a busy disk.** That works today. For a stack
  that throws its state away (an e2e run), tmpfs is enough.
- **In celld,** two options. Neither is a contained fix, so both are left
  to upstream:
  - a single dev node, whose only rival is the next node of a reload,
    could take a longer TTL;
  - the dev lease could live outside the shared SQLite file.

  `celld dev` offers no setting for its lease today.
