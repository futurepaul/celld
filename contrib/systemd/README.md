# celld as its own locked-down user

`fragment-celld.service` runs celld for a self-hosted Fragment as the system
user `fragment-celld`, with no capabilities and a read-only view of the
machine. `systemd-analyze security` scores it **1.1 (OK)**.

| File | What it is |
| --- | --- |
| `fragment-celld.service` | The unit. Each group of settings says what it buys. |
| `install.sh` | The one-time install, run as root: user, binaries, directories, settings, unit. |
| `celld.env` | The settings template, installed as `/etc/fragment-celld/celld.env`. |
| `try-user.sh` | Runs celld under the unit's own hardening as a `systemd --user` unit, with no root. |

## What celld can touch

| Path | Owner, mode | celld's access |
| --- | --- | --- |
| `/usr/local/lib/fragment-celld/{celld,esbuild}` | root, 0755 | run |
| `/opt/fragment/cell` (the cell's Worker project) | root, 0755 | read |
| `/opt/fragment/cell/.celld` → `/var/lib/fragment-celld/cell` | symlink | read and write (the state) |
| `/etc/fragment-celld/celld.env` | root:fragment-celld, 0640 | read, through systemd |
| `/var/lib/sandcastle/engine.sock` | the engine's client, 0600 | connect |
| everything else | | read-only; `/home`, `/root` and `/run/user` hidden; private `/tmp` and `/dev` |

The state is a symlink, not a mount over `.celld`, because celld hands the
engine socket paths under it (the intercept socket), and the engine resolves
them in the host's mount namespace.

Network: AF_UNIX, AF_INET and AF_INET6 only. That covers the engine, loopback
services such as OpenBao, and outbound HTTPS. A Worker's egress policy is
celld's (`CELLD_EGRESS_PUBLIC_ONLY`), not the unit's.

## Off on purpose

| Setting | Why it is off |
| --- | --- |
| `MemoryDenyWriteExecute` | V8's JIT and WebAssembly write machine code and then make it executable. With it on, V8 stops with a fatal error when it creates the first isolate. |
| `PrivateNetwork`, `IPAddressDeny` | celld serves on the host's network and calls out to arbitrary HTTPS hosts. |
| `ProcSubset=pid` | It hides `/proc/meminfo`. celld reads that to size its memory-pressure threshold when no `MemoryMax=` applies, and without it the threshold quietly turns off. |
| `@resources` in the syscall filter | Refused, together with `@privileged`. Nothing celld does needs them. |

`@pkey` is allowed on top of `@system-service`: V8 guards its pointer tables
with x86 memory protection keys. When the pkey calls are refused with EPERM,
V8 runs without that protection and says nothing; when they are killed, it
crashes.

## Installing it (once, as root)

```sh
sudo contrib/systemd/install.sh target/release/celld /path/to/esbuild
```

Then:

1. **The cell's project.** Copy the cell's rendered Worker project (its
   wrangler config and build) into `/opt/fragment/cell`, owned by root, and
   name the config file in `CELLD_CONFIG`.
2. **The engine.** sandcastle's engine serves one client: its socket is
   `0600`, owned by `client_uid:client_gid` from `engine.json`, and its state
   directory is `root:client_gid 0710`. Set both to `fragment-celld`'s ids
   (`id -u fragment-celld`, `id -g fragment-celld`) and restart the engine.
   Any other user then loses the engine, including a developer's
   `celld dev`.
3. **Start it:** `sudo systemctl enable --now fragment-celld`, and
   `journalctl -u fragment-celld -f` to watch it.

## Proving it without root

`try-user.sh` reads the unit's `[Service]` settings and runs `celld dev` under
them as a transient `systemd --user` unit. It swaps only what a user manager
cannot do: the user, `ProtectHome=yes` (a tmpfs with the binaries and the
project bound in), the state directory, and the settings file. For example:

```sh
contrib/systemd/try-user.sh target/release/celld "$(command -v esbuild)" \
  examples/hello 9906 CELLD_CONTAINER_ENGINE=krun:/var/lib/sandcastle/engine.sock
curl http://127.0.0.1:9906/
systemctl --user stop celld-try-9906
```

The hardened unit cannot reach Docker, so a Worker with containers names an
image already loaded into the engine (`"image": "celld-image:<sha256>"`), not
a Dockerfile; `examples/container-conformance/README.md` says how.

On 2026-11-11 this ran, with celld on V8 15.2.124.45: a storage and D1 Worker
(17 of 17 checks), a probe of the optimizing JIT, WebAssembly, outbound HTTPS
and a loopback service, and the container conformance Worker on the krun
engine (25 of 25). The same storage Worker passed under the real system unit,
as `fragment-celld`, in a container booting systemd (255), where `install.sh`
ran as root; it stopped cleanly on SIGINT and came back.
