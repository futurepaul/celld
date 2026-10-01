# Containers

A container runs a program that is not JavaScript beside the Worker that needs
it. A Durable Object starts the container, reaches it over a TCP port, and stops
it. The container disk is ephemeral, so durable state belongs in the storage of
the object or in another binding. Containers are experimental in celld, so the
configuration keys, the `ctx.container` surface, the node defaults, and the
security boundary can change without notice. Read the
[Cloudflare Containers documentation](https://developers.cloudflare.com/containers/)
for the standard API.

## Example

The [Containers example](../../examples/container) starts a Python HTTP server
and reaches it through a container port.

<!-- celld-example: container -->

## The object and its container

A `containers` entry names a SQLite-backed Durable Object class of the same
script. Each object of that class gets a `ctx.container` handle and controls at
most one container.

celld implements `running`, `images`, `start()`, `monitor()`, `destroy()`,
`signal()`, `getTcpPort()`, `exec()`, `inspect()`, and `setInactivityTimeout()`
on both engines (see [Two engines](#two-engines)), and `snapshotContainer()`,
`interceptOutboundHttp()`, `interceptOutboundHttps()`, and
`interceptAllOutboundHttp()` on the krun engine. `start()` accepts `image`,
`containerSnapshot`, `instance`, `entrypoint`, `env`, `enableInternet`, and
`labels`, with Cloudflare's limits: at most 10 labels, names of 1 to 16 bytes,
values of at most 64. `image` is one of the class's images. It checks
`hardTimeout` and then ignores it, so a value of 0 or less throws and a valid
value changes nothing. `destroy()` stops a container whose `start()` is still
under way. `exec()` accepts `pty` and `signal`, and its process has `isPty` and
`resize()`. `snapshotDirectory()` and `interceptOutboundTcp()` reject with an
error. Read the
[`ctx.container` reference](https://developers.cloudflare.com/durable-objects/api/container/)
for the standard method list. The
[conformance example](../../examples/container-conformance) calls every method
and answers a verdict per method, on either engine and on Cloudflare.

On Linux, `getTcpPort(port)` reaches every port through the bridge address of
the container. A fetch with an `Upgrade: websocket` header gives a 101 response
with a `webSocket`, as on Cloudflare. The node logs a failed `start()` as
`container_start_failed`.

`@cloudflare/containers` runs as published, including the `sleepAfter` alarm,
`getContainer()`, `getRandom()`, and `switchPort()`. `@cloudflare/sandbox` also
runs as published on the `cloudflare/sandbox` image, over the HTTP and the RPC
transports. Its preview URLs need a wildcard hostname at the load balancer,
tunnels run cloudflared inside the container, and bucket mounts use the
credentials that the application passes. See the
[sandbox example](../../examples/sandbox).

## The image and the node

`celld deploy` builds a Dockerfile in the project, or pulls an image reference,
with the `docker` CLI on `PATH` or the CLI that `CELLD_DOCKER` names. A Podman
CLI works. It saves the image to `deploy/images/<key>.tar` in the fleet bucket,
where the key hashes the image layers and config, so a redeploy of an unchanged
image uploads nothing. A node loads the image into its own engine the first time
a cell of the class starts, and never contacts a registry. The `celld-fence`
image rides with every deployment that has a container.

`celld deploy` builds and pulls for `linux/amd64`, as Wrangler does.
`CELLD_CONTAINER_PLATFORM` selects another platform. `celld dev` builds for the
local machine and keeps the image in the local engine.

The container runs on the node that owns the cell. Each node that serves a
container class needs a Docker or Podman daemon. celld uses the Unix socket
that `DOCKER_HOST` names, or the default socket of Docker, Docker Desktop,
OrbStack, or Podman. A `DOCKER_HOST` value that is not a `unix://` URL gives
celld no socket. A node without an engine cannot activate a cell of a container
class, and it serves every other class.

A `containers` entry with `scheduling_policy: "durable_object"` declares
`images`, a name for each Dockerfile or image reference, and no `image`;
`ctx.container.images` maps each name to its reference, and an object passes
one to `start()`. A reference that is already a content key,
`celld-image:<sha256>`, is taken as it is, so a machine without the `docker`
CLI deploys an image that another machine built and loaded into the node's
engine.

## Two engines

`CELLD_CONTAINER_ENGINE` selects the node's engine: `docker`, the default, or
`krun:<path>`, the socket of a libkrun microVM engine that speaks Cloudflare's
container API itself (fragment's `sandcastle-engine`). Its `ports.sock` is in
the same directory.

On the krun engine, each container is a virtual machine in its own jail, cgroup,
and network namespace, so the engine and not a node-wide fence enforces
`enableInternet`, and a deployment for krun nodes carries no fence image. The
engine routes an intercepted request to a socket of celld's, which dispatches it
to the binding the object passed, on the service-call path; HTTPS interception
uses the CA at `/etc/cloudflare/certs/cloudflare-containers-ca.crt`, as on
Cloudflare. `getTcpPort()` reaches a port through a loopback listener per port,
whose connections take a socket from the engine, TCP over the VM's network
interface or a relay to the guest's loopback.

The Docker engine has no snapshots and no interception, and those calls reject
with an error naming the engine. Its `exec()` inherits the container's whole
environment, and its `kill()` signals a process by a pid that the daemon reports
from the host's namespace, so it can miss the process.

## The isolation boundary

Under the default runtime, a container shares the kernel of the node and is not
a virtual machine. celld drops all Linux capabilities, sets
`no-new-privileges`, keeps the default seccomp profile of the daemon, limits the
container to 1024 processes, and runs an init as PID 1 that reaps orphans. Two
containers on one node cannot connect to each other.

Before the first container starts, the node installs nftables rules on its
container bridges through a one-shot privileged container of the `celld-fence`
image. A container with `enableInternet: true` reaches the Internet, but not
the node, another node, the private ranges `10/8`, `172.16/12`, `192.168/16`,
and `100.64/10`, or the link-local range. A node that cannot install the rules
starts no container, and a deployment from a celld without the fence image
starts no container on a node that has the fence.

`enableInternet: false` attaches the container to an internal bridge with no
route out. On macOS, an internal network publishes no port, so `celld dev` on
macOS keeps egress on and logs a warning. The fence still applies.

`CELLD_CONTAINER_RUNTIME` names the OCI runtime for every container on the node,
for example `runsc` for gVisor or `kata` for a virtual machine. A `runtime` key
in a `containers` entry overrides it for that class; Cloudflare has no such key.
If the daemon lacks the runtime, every `start()` fails with the daemon error.
For code that you did not write, name a runtime that gives each container its
own kernel, and keep the secrets in the Worker.

A container under a named runtime gets a written `/etc/resolv.conf` with
`1.1.1.1` and `1.0.0.1`, or the `CELLD_CONTAINER_DNS` resolvers, because gVisor
cannot reach the built-in resolver of the engine. The fence permits the public
resolver. A container on the default runtime keeps the engine resolver unless
the operator sets `CELLD_CONTAINER_DNS`.

## Sleep, wake, and the node's resources

`setInactivityTimeout()` sets the idle window, and the default is 10 minutes, as
on Cloudflare. An idle eviction of the cell keeps the container running for that
window, and the next activation on the same node reconnects to it. A move to
another node, a node restart, a reset, and a node stop destroy the container.
Ctrl-C on `celld dev` destroys the containers and keeps the local state.

`instance_type` accepts `lite` and its older name `dev`, `basic`, `standard-1`
and its alias `standard`, `standard-2`, `standard-3`, and `standard-4`, with the
CPU and memory limits of
[the Cloudflare limits page](https://developers.cloudflare.com/containers/platform/limits/).
The default is `dev`: a sixteenth of a CPU and 256 MiB, as on Cloudflare. Name a
larger type when the image boots an interpreter.

The node counts the memory cap of every running container as committed memory,
because the container cgroup is outside the node process. A container-heavy
node therefore reports no headroom and sheds cells. Set the node memory ceiling
at or above the largest instance type in use, plus headroom.

Each node publishes its running container count per class in the shared
capacity sample. Before a start, a node sums the fleet count and its own live
count against `max_instances`. A start over the cap fails, and the node logs
`container_start_failed` with the limit.

## Differences from Cloudflare

- A `containers` entry accepts `class_name`, `image`, `images`,
  `scheduling_policy`, `name`, `instance_type`, `max_instances`, and `runtime`.
  Each other key stops the deployment. celld adds `runtime`, and it accepts
  `name` without using it.
- celld places a container on the node that owns the cell. Cloudflare can place
  a container away from its object.
- A node that serves a container class needs a Docker daemon, a Podman daemon,
  or the krun engine.
- celld does not enforce the disk size of an instance type.
- `max_instances` converges across the fleet instead of holding centrally, so
  the fleet can exceed the cap for one refresh. `celld dev` enforces no cap.
- `snapshotDirectory()` and `interceptOutboundTcp()` reject with an error, and
  on the Docker engine so do `snapshotContainer()` and the HTTP interception
  methods. `start()` checks `hardTimeout` and then ignores it, so a value of 0
  or less throws and a valid value has no effect.
- On the Docker engine, `exec()` inherits the container's environment, not only
  `PATH`, and its `kill()` can miss the process.
- `getTcpPort(port).connect()` gives a socket with the lifetime of the event
  that opened it. See [TCP sockets](../cloudflare-compat.md#tcp-sockets).
- A `monitor()` promise and an `exec()` process do not keep the object active
  after the handler answers.
- On macOS, the node reaches a container through a published port, so the image
  must declare the port with `EXPOSE`. `enableInternet: false` has no effect on
  macOS.
- The container bridges carry no IPv6 address, and the fence rejects the IPv6
  link-local and unique-local ranges as well.
- A move of an object to another node stops its container, so the first
  `@cloudflare/sandbox` call after the move can fail with the SDK's
  `OperationInterruptedError`, as after a container restart on Cloudflare. The
  next call starts a fresh container.

The [Cloudflare compatibility](../cloudflare-compat.md#services) page lists
the runtime APIs and the unsupported services.
