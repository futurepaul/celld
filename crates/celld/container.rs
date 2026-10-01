// Copyright 2026 Deno Land Inc. Apache-2.0 license.

//! Containers attached to Durable Objects: the engine side of
//! `ctx.container`.
//!
//! A cell of a class named in the deployment's `containers` config owns at
//! most one container, named after the cell, on the node that owns the
//! cell. The Durable Object supervises it through the ops in
//! `js/container.rs`; this module performs the effects against a container
//! engine behind the `Backend` trait: the Docker Engine API on a unix
//! socket (Podman serves the same API, `container_docker`), or a libkrun
//! microVM engine with Cloudflare's container API on its own socket
//! (`container_krun`). `CELLD_CONTAINER_ENGINE` picks one.
//!
//! The container is bound to the cell's ownership on this node, not to its
//! residency: an idle eviction leaves it running under an inactivity timer
//! and the next activation of the same cell reconnects to it by name, which
//! is what Cloudflare does and what the `@cloudflare/containers` class's
//! `sleepAfter` alarm relies on. Every other stop destroys it. The
//! container's disk is ephemeral on Cloudflare too, so a takeover that
//! starts fresh is conformant.
//!
//! Images travel through the bucket. `celld deploy` saves each image the
//! config names as a tar at `deploy/images/<id>.tar`, and a node loads it
//! into its engine the first time a cell of that class starts, so a node
//! never talks to a registry.

use crate::asyncrt;
use anyhow::anyhow;
use bytes::Bytes;
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, RwLock};
use std::time::Duration;
use tokio::sync::{mpsc, watch};

/// One `containers[]` entry of a deployment, as the node sees it.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ContainerSpec {
    pub class_name: String,
    /// The image a start that names none runs, `celld-image:<content key>`,
    /// so two deployments of one image share one tar and one load. `None`
    /// under the `durable_object` scheduling policy, whose objects name an
    /// image of `images` in `start()`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image: Option<String>,
    /// `ctx.container.images`: names to the references `start()` takes.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub images: BTreeMap<String, String>,
    /// `default` (or none) or `durable_object`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scheduling_policy: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instance_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_instances: Option<u64>,
    /// The OCI runtime this class runs under, such as `runsc` (gVisor) for
    /// untrusted code. Overrides the node's `CELLD_CONTAINER_RUNTIME` for
    /// this class; `None` takes the node default. A node whose daemon does
    /// not have the runtime fails every start of the class, so the class
    /// runs only where its isolation is available. celld extends the
    /// Cloudflare config here, which has no per-class runtime. Docker only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime: Option<String>,
}

impl ContainerSpec {
    /// Every image a cell of this class may start, the default first.
    pub fn all_images(&self) -> impl Iterator<Item = &String> {
        self.image.iter().chain(self.images.values())
    }
}

/// The reference every engine holds an image under, from its content key.
/// The key hashes the layer digests and the image config, so an engine
/// that rebuilds identical content under a new id still resolves it.
pub fn image_reference(key: &str) -> String {
    format!("celld-image:{key}")
}

/// The bucket key of one saved image, from its reference.
pub fn image_key(image: &str) -> String {
    format!(
        "deploy/images/{}.tar",
        image.trim_start_matches("celld-image:")
    )
}

/// The resources an instance type reserves. Cloudflare's published table;
/// a name outside it is refused at deploy.
pub fn instance_resources(instance_type: &str) -> Option<(f64, u64)> {
    let gib = 1024 * 1024 * 1024;
    Some(match instance_type {
        "lite" | "dev" => (1.0 / 16.0, 256 * 1024 * 1024),
        "basic" => (0.25, gib),
        "standard-1" | "standard" => (0.5, 4 * gib),
        "standard-2" => (1.0, 6 * gib),
        "standard-3" => (2.0, 8 * gib),
        "standard-4" => (4.0, 12 * gib),
        _ => return None,
    })
}

/// The instance type of a class that declares none, as on Cloudflare.
const DEFAULT_INSTANCE_TYPE: &str = "dev";

/// The size a start gives its container: a named type, or `start()`'s
/// custom `{vcpu, memoryMib, diskMb}`.
#[derive(Clone, Debug, PartialEq)]
pub enum Instance {
    Named(String),
    Custom { vcpu: f64, memory_mib: u64, disk_mb: u64 },
}

impl Instance {
    /// vCPUs (a fraction for the small types) and memory in bytes.
    pub fn resources(&self) -> Option<(f64, u64)> {
        match self {
            Instance::Named(name) => instance_resources(name),
            Instance::Custom {
                vcpu, memory_mib, ..
            } => Some((*vcpu, memory_mib * 1024 * 1024)),
        }
    }

    /// The engine API's form, Cloudflare's.
    pub fn to_json(&self) -> Value {
        match self {
            // celld's alias names, Cloudflare's names to the engine.
            Instance::Named(name) => json!(match name.as_str() {
                "dev" => "lite",
                "standard" => "standard-1",
                other => other,
            }),
            Instance::Custom {
                vcpu,
                memory_mib,
                disk_mb,
            } => json!({ "vcpu": vcpu, "memoryMib": memory_mib, "diskMb": disk_mb }),
        }
    }
}

/// The memory a container of this instance type reserves on the node. A
/// class that declares none gets the default type, as a start does. An
/// unknown name is refused at deploy, so a running container always maps;
/// this returns 0 for a name that somehow does not, which undercounts
/// rather than blocks a sample.
pub fn instance_memory_bytes(instance_type: Option<&str>) -> u64 {
    instance_resources(instance_type.unwrap_or(DEFAULT_INSTANCE_TYPE))
        .map(|(_, memory)| memory)
        .unwrap_or(0)
}

/// The memory this node commits to its running containers, for the node's
/// capacity accounting. Zero when no engine has connected. See
/// `celld_logic::pressure::Load::container_reserved_bytes` for why the node
/// counts the cap rather than the container's live usage.
pub fn reserved_memory_bytes() -> u64 {
    engine_if_ready().map_or(0, |engine| engine.reserved_memory_bytes())
}

/// This node's running containers per class, published in the node lease so
/// peers can sum a class's instances across the fleet for `max_instances`.
/// Empty when no engine has connected.
pub fn running_instances_by_class() -> std::collections::BTreeMap<String, u64> {
    engine_if_ready().map_or_else(Default::default, |engine| {
        engine.running_instances_by_class()
    })
}

/// Whether `host:port` is a port this node handed `scope` for its own
/// container (`getTcpPort`), which the public-only egress policy lets the
/// object reach though it is a private address.
pub fn owns_address(scope: &str, host: &str, port: u16) -> bool {
    let Some(engine) = engine_if_ready() else {
        return false;
    };
    let Some(cell) = engine.cell(scope) else {
        return false;
    };
    let state = cell.state.lock().unwrap();
    state.running && state.address.serves(host, port)
}

/// How a cell stop treats its container.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Release {
    /// An idle eviction: keep the container under its inactivity timer.
    Keep,
    /// Ownership leaves this node, or the cell is reset: destroy it.
    Destroy,
}

/// The default inactivity window after an idle eviction when the object
/// never set one. The `@cloudflare/containers` alarm wakes the object well
/// inside this to enforce `sleepAfter`, so this is the backstop for an
/// object that never wakes, not the policy.
const DEFAULT_INACTIVITY: Duration = Duration::from_secs(10 * 60);

/// Environment every container sees. The values mirror workerd's local
/// engine: applications read the names, never the values.
const DEFAULT_ENV: &[&str] = &[
    "CLOUDFLARE_COUNTRY_A2=XX",
    "CLOUDFLARE_DEPLOYMENT_ID=xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx",
    "CLOUDFLARE_LOCATION=loc01",
    "CLOUDFLARE_REGION=REGN",
    "CLOUDFLARE_APPLICATION_ID=xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx",
];

/// Cloudflare's intercept entries per container: a hostname or a glob
/// counts two (IPv4 and IPv6), an address or a range one.
pub const INTERCEPT_ENTRIES_MAX: usize = 128;

pub struct StartParams {
    /// One of the class's images; `None` takes the class's default.
    pub image: Option<String>,
    /// A snapshot id to restore (`containerSnapshot`), in place of an image.
    pub snapshot: Option<String>,
    pub instance: Option<Instance>,
    pub entrypoint: Option<Vec<String>>,
    pub env: Vec<(String, String)>,
    pub enable_internet: bool,
    pub labels: Vec<(String, String)>,
}

/// What a backend starts: everything decided, nothing left to look up.
pub(crate) struct Launch {
    pub scope: String,
    pub node: String,
    pub class: String,
    /// `None` when `snapshot` is the root.
    pub image: Option<String>,
    pub snapshot: Option<String>,
    /// `NAME=value`, the defaults first.
    pub env: Vec<String>,
    pub labels: Vec<(String, String)>,
    pub entrypoint: Option<Vec<String>>,
    pub enable_internet: bool,
    pub instance: Instance,
    pub runtime: Option<String>,
    pub intercepts: Vec<InterceptRule>,
}

/// How a run ended: the root process's code (137 for a kill), and whether
/// `destroy()` ended it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RunExit {
    pub code: i64,
    pub destroyed: bool,
}

/// One `interceptOutbound*` rule, in the engine's terms.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct InterceptRule {
    /// `http` or `https`.
    pub scheme: &'static str,
    /// A host, a `*.` glob, `*`, an `ip:port`, or a range; an HTTPS host
    /// may end in `:port`.
    pub target: String,
}

impl InterceptRule {
    /// Its entries in Cloudflare's count of 128.
    pub fn entries(&self) -> usize {
        let host = self.target.trim_end_matches(|c: char| c.is_ascii_digit() || c == ':');
        let address = self.target.contains('/')
            || host.parse::<std::net::IpAddr>().is_ok()
            || self.target.parse::<std::net::SocketAddr>().is_ok();
        if address {
            1
        } else {
            2
        }
    }
}

/// Where an intercepted request goes: the binding's route, as
/// `globalOutbound` keeps one, so it outlives the object's isolate.
#[derive(Clone, Debug)]
pub struct ServiceRoute {
    pub generation: crate::generation::GenerationId,
    pub script: String,
    pub entrypoint: Option<String>,
    pub props: Vec<u8>,
}

#[derive(Clone, Debug)]
pub(crate) struct Intercept {
    pub rule: InterceptRule,
    pub route: ServiceRoute,
}

/// What `inspect()` answers for the current run.
#[derive(Clone, Debug, Default)]
struct RunInfo {
    image: String,
    labels: BTreeMap<String, String>,
    from_snapshot: bool,
    started: bool,
    memory_bytes: u64,
}

#[derive(Default)]
struct CellState {
    running: bool,
    /// Where the node dials the container.
    address: Address,
    inactivity: Option<Duration>,
    /// The pending destroy after an idle eviction, aborted when the cell
    /// returns (dropping a task handle only detaches it).
    sweeper: Option<asyncrt::TaskHandle<()>>,
    /// Bumped per start so a wait from a previous run cannot report for
    /// this one.
    run: u64,
    /// The run `destroy()` ended, so its exit reads as destroyed.
    destroyed: Option<u64>,
    info: RunInfo,
    /// `interceptOutbound*` so far, in order: the engine names a match by
    /// its index, and intercepts are only ever added.
    intercepts: Vec<Intercept>,
}

/// Where the node reaches a container's ports.
#[derive(Clone, Default)]
pub(crate) enum Address {
    #[default]
    None,
    /// Any port at this address (Docker's bridge on Linux).
    Ip(String),
    /// Loopback ports the engine published (Docker elsewhere).
    Published(HashMap<u16, u16>),
    /// A loopback listener per port, made on first use (the krun engine).
    Forwarded(Arc<dyn PortForwarder>),
}

impl std::fmt::Debug for Address {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Address::None => f.write_str("None"),
            Address::Ip(ip) => write!(f, "Ip({ip})"),
            Address::Published(ports) => write!(f, "Published({ports:?})"),
            Address::Forwarded(_) => f.write_str("Forwarded"),
        }
    }
}

impl Address {
    fn serves(&self, host: &str, port: u16) -> bool {
        match self {
            Address::None => false,
            Address::Ip(ip) => ip == host,
            Address::Published(ports) => host == "127.0.0.1" && ports.values().any(|p| *p == port),
            Address::Forwarded(f) => host == "127.0.0.1" && f.serves(port),
        }
    }
}

/// Loopback listeners that reach a container's ports.
pub(crate) trait PortForwarder: Send + Sync {
    /// `127.0.0.1:<port>` for the container's `port`, listening.
    fn address(&self, port: u16) -> Result<String, String>;
    /// Whether this forwarder listens on loopback `port`.
    fn serves(&self, port: u16) -> bool;
    /// Stops every listener.
    fn close(&self);
}

pub struct CellContainer {
    scope: String,
    name: String,
    spec: Arc<ContainerSpec>,
    state: Mutex<CellState>,
    /// Processes `exec()` started in this container. An object drops one
    /// after `output()`; the rest go with the container.
    processes: Mutex<Vec<u64>>,
    /// The exit of the current run, `None` while it runs. `Some(Err)` is
    /// an engine failure the wait could not attribute to the process.
    /// Written with `send_replace`: a plain `send` discards the value
    /// while nobody subscribes, and `monitor()` usually subscribes late.
    exit: watch::Sender<Option<(u64, Result<RunExit, String>)>>,
}

impl CellContainer {
    pub fn running(&self) -> bool {
        self.state.lock().unwrap().running
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    /// `host:port` for `getTcpPort(port)`.
    pub fn address(&self, port: u16) -> Result<String, String> {
        let state = self.state.lock().unwrap();
        if !state.running {
            return Err("the container is not running".to_string());
        }
        match &state.address {
            Address::Ip(ip) => Ok(format!("{ip}:{port}")),
            Address::Published(ports) => ports
                .get(&port)
                .map(|host| format!("127.0.0.1:{host}"))
                .ok_or_else(|| {
                    format!(
                        "The container is not listening on port {port}: on this platform a port \
                         must be declared with EXPOSE in the image"
                    )
                }),
            Address::Forwarded(forwarder) => forwarder.address(port),
            Address::None => Err(format!("The container is not listening on port {port}")),
        }
    }

    pub fn set_inactivity(&self, duration: Duration) {
        self.state.lock().unwrap().inactivity = Some(duration);
    }

    /// Open the next run, synchronously, before the start is even queued.
    /// The object's `start()` returns at once and its `monitor()` follows
    /// immediately, so the run they both mean must exist before either
    /// effect runs, or the monitor would find the previous run's exit and
    /// report the new container dead on arrival.
    pub fn begin_run(&self) -> u64 {
        let mut state = self.state.lock().unwrap();
        state.run += 1;
        state.running = true;
        state.address = Address::None;
        state.info = RunInfo::default();
        let run = state.run;
        drop(state);
        self.exit.send_replace(None);
        run
    }

    /// The run `monitor()` waits on: the newest one.
    pub fn current_run(&self) -> u64 {
        self.state.lock().unwrap().run
    }

    /// `inspect()`: the running container's image and labels, or `None`.
    /// The image is empty while it starts and when it came from a snapshot,
    /// as on Cloudflare.
    pub fn inspect(&self) -> Option<Value> {
        let state = self.state.lock().unwrap();
        if !state.running {
            return None;
        }
        let image = if state.info.started && !state.info.from_snapshot {
            state.info.image.clone()
        } else {
            String::new()
        };
        Some(json!({ "image": image, "labels": state.info.labels }))
    }

    /// `ctx.container.images`.
    pub fn images(&self) -> &BTreeMap<String, String> {
        &self.spec.images
    }
}

/// One container engine: the effects `ContainerEngine` orchestrates.
#[async_trait::async_trait]
pub(crate) trait Backend: Send + Sync {
    /// What the engine is, for messages.
    fn kind(&self) -> &'static str;
    /// Reap every container of `node` a previous process left behind.
    async fn reap(&self, node: &str) -> anyhow::Result<()>;
    /// What every start needs first (Docker: its bridges and the fence).
    async fn prepare(&self) -> anyhow::Result<()>;
    async fn has_image(&self, image: &str) -> anyhow::Result<bool>;
    /// Loads a `docker save` tar holding `image`.
    async fn load_image(&self, image: &str, tar: Bytes) -> anyhow::Result<()>;
    /// The container of that name, if it runs: where its ports are.
    async fn running(&self, name: &str) -> anyhow::Result<Option<Address>>;
    async fn create_and_start(&self, name: &str, launch: Launch) -> anyhow::Result<Address>;
    /// Waits for the root process to end.
    async fn wait(&self, name: &str) -> anyhow::Result<RunExit>;
    async fn signal(&self, name: &str, signal: u32) -> anyhow::Result<()>;
    /// Ends and removes the container; idempotent, errors ignored.
    async fn remove(&self, name: &str);
    async fn exec(&self, name: &str, params: &ExecParams) -> anyhow::Result<ExecStarted>;
    /// `snapshotContainer()`: `{id, size, name?}`.
    async fn snapshot(&self, name: &str, snapshot_name: Option<String>) -> anyhow::Result<Value>;
    /// The running container's intercepts, replaced whole (only ever grown).
    async fn set_intercepts(&self, name: &str, intercepts: &[InterceptRule]) -> anyhow::Result<()>;
    /// Whether `start()` may restore a snapshot.
    fn restores_snapshots(&self) -> bool;
    /// Whether the engine routes intercepted requests back to the object.
    fn intercepts(&self) -> bool;
}

pub struct ContainerEngine {
    backend: Arc<dyn Backend>,
    node: String,
    bucket: Option<crate::bucket::Bucket>,
    /// Per-image load lock, so two cells of one class starting together
    /// load the tar once.
    images: Mutex<HashMap<String, Arc<tokio::sync::Mutex<bool>>>>,
    cells: Mutex<HashMap<String, Arc<CellContainer>>>,
}

/// Which engine `CELLD_CONTAINER_ENGINE` names.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EngineChoice {
    /// The Docker Engine API, found as `Docker::discover` finds it.
    Docker,
    /// The krun engine's API socket (`krun:<path>`); its `ports.sock` is
    /// beside it.
    Krun(PathBuf),
}

impl EngineChoice {
    pub fn parse(value: Option<&str>) -> anyhow::Result<EngineChoice> {
        match value.filter(|v| !v.is_empty()) {
            None | Some("docker") => Ok(EngineChoice::Docker),
            Some(v) => match v.strip_prefix("krun:") {
                Some(path) if path.starts_with('/') => Ok(EngineChoice::Krun(PathBuf::from(path))),
                _ => Err(anyhow!(
                    "CELLD_CONTAINER_ENGINE is `docker` or `krun:<absolute path of the engine's socket>`"
                )),
            },
        }
    }
}

/// What the runtime told this module about the node: set once at start.
struct Config {
    node: String,
    bucket: Option<crate::bucket::Bucket>,
    /// `CELLD_CONTAINER_RUNTIME`: the OCI runtime every container starts
    /// under, such as `runsc` (gVisor) or `kata` (a VM). `None` is the
    /// daemon's default, which is `runc` unless the operator changed it. A
    /// class can override it, see `ContainerSpec::runtime`. Docker only.
    runtime: Option<String>,
    /// `CELLD_CONTAINER_DNS`: the resolvers a container gets. Docker only;
    /// see `container_docker`.
    dns: Vec<String>,
    /// The node's state directory: the Docker engine's resolv.conf, the
    /// krun engine's intercept socket.
    data_dir: PathBuf,
    /// `CELLD_CONTAINER_ENGINE`.
    choice: EngineChoice,
    /// Where intercepted requests are dispatched.
    runtime_manager: Option<crate::runtime::RuntimeManager>,
}

static CONFIG: RwLock<Option<Config>> = RwLock::new(None);
/// The deployment's container classes. Replaced on every generation swap;
/// a running container keeps the spec it started with.
static SPECS: RwLock<Vec<Arc<ContainerSpec>>> = RwLock::new(Vec::new());
/// The deployment's `celld-fence` image; see `Manifest::fence_image`.
static FENCE_IMAGE: RwLock<Option<String>> = RwLock::new(None);
/// The engine, connected on first use. A node whose deployment declares no
/// container class never opens the socket.
static ENGINE: tokio::sync::OnceCell<Arc<ContainerEngine>> = tokio::sync::OnceCell::const_new();

pub fn configure(
    node: String,
    bucket: Option<crate::bucket::Bucket>,
    data_dir: PathBuf,
    runtime_manager: Option<crate::runtime::RuntimeManager>,
) {
    let runtime = std::env::var("CELLD_CONTAINER_RUNTIME")
        .ok()
        .filter(|runtime| !runtime.is_empty());
    let dns = std::env::var("CELLD_CONTAINER_DNS")
        .unwrap_or_default()
        .split(',')
        .map(|resolver| resolver.trim().to_string())
        .filter(|resolver| !resolver.is_empty())
        .collect();
    let choice = match EngineChoice::parse(std::env::var("CELLD_CONTAINER_ENGINE").ok().as_deref()) {
        Ok(choice) => choice,
        Err(error) => {
            tracing::warn!(error = %format!("{error:#}"), "containers use Docker");
            EngineChoice::Docker
        }
    };
    *CONFIG.write().unwrap() = Some(Config {
        node,
        bucket,
        runtime,
        dns,
        data_dir,
        choice,
        runtime_manager,
    });
}

pub fn install_specs(specs: Vec<ContainerSpec>, fence_image: Option<String>) {
    *SPECS.write().unwrap() = specs.into_iter().map(Arc::new).collect();
    *FENCE_IMAGE.write().unwrap() = fence_image;
}

pub fn spec(class: &str) -> Option<Arc<ContainerSpec>> {
    SPECS
        .read()
        .unwrap()
        .iter()
        .find(|spec| spec.class_name == class)
        .cloned()
}

/// The deployment's fence image, which the Docker engine runs once.
pub(crate) fn fence_image() -> Option<String> {
    FENCE_IMAGE.read().unwrap().clone()
}

/// The engine, connecting on the first call. A failure is returned rather
/// than cached, so a daemon that comes up later is found by the next call.
pub async fn engine() -> anyhow::Result<Arc<ContainerEngine>> {
    ENGINE
        .get_or_try_init(|| async {
            let (node, bucket, runtime, dns, data_dir, choice, runtime_manager) = {
                let config = CONFIG.read().unwrap();
                let config = config.as_ref().ok_or_else(|| {
                    anyhow!("the container engine is not configured on this node")
                })?;
                (
                    config.node.clone(),
                    config.bucket.clone(),
                    config.runtime.clone(),
                    config.dns.clone(),
                    config.data_dir.clone(),
                    config.choice.clone(),
                    config.runtime_manager.clone(),
                )
            };
            let backend: Arc<dyn Backend> = match choice {
                EngineChoice::Docker => {
                    let docker = crate::docker::Docker::discover().ok_or_else(|| {
                        anyhow!(
                            "no container engine: set DOCKER_HOST to a unix socket, or run a \
                             Docker or Podman daemon on this node"
                        )
                    })?;
                    Arc::new(crate::container_docker::DockerBackend::new(
                        docker, node.clone(), runtime, dns, &data_dir,
                    ))
                }
                EngineChoice::Krun(socket) => Arc::new(
                    crate::container_krun::KrunBackend::new(socket, &node, &data_dir, runtime_manager)
                        .await?,
                ),
            };
            Ok(Arc::new(ContainerEngine::connect(backend, node, bucket).await?))
        })
        .await
        .cloned()
}

/// The engine only if a previous call connected it.
pub fn engine_if_ready() -> Option<Arc<ContainerEngine>> {
    ENGINE.get().cloned()
}

/// Destroy every container of this node at process exit. A preserve
/// shutdown (`celld dev` on Ctrl-C) keeps its cells resident and stops
/// none of them, and a handoff cut by the deadline leaves cells behind
/// too; without this their containers ran on until the next start of the
/// same node reaped them, invisible to the object that owned them.
pub async fn shutdown() {
    let Some(engine) = engine_if_ready() else {
        return;
    };
    if let Err(error) = engine.backend.reap(&engine.node).await {
        tracing::warn!(
            event = "container_shutdown_reap_failed",
            error = %format!("{error:#}"),
            "containers of this node may still be running"
        );
    }
}

/// Connect and load every image of the installed specs, ahead of the
/// first cell that needs one. Failures are logged: a node without an
/// engine still serves every other class, and the container class fails
/// at its first `start()` with the same message.
pub async fn prewarm() {
    let engine = match engine().await {
        Ok(engine) => engine,
        Err(error) => {
            tracing::warn!(error = %format!("{error:#}"), "containers are unavailable on this node");
            return;
        }
    };
    if let Err(error) = engine.backend.prepare().await {
        tracing::warn!(
            engine = engine.backend.kind(),
            error = %format!("{error:#}"),
            "the container engine is not ready; no container starts on this node"
        );
    }
    let specs = SPECS.read().unwrap().clone();
    for spec in specs {
        for image in spec.all_images() {
            if let Err(error) = engine.ensure_image(image).await {
                tracing::warn!(
                    class = %spec.class_name,
                    image = %image,
                    error = %format!("{error:#}"),
                    "container image is unavailable"
                );
            }
        }
    }
}

impl ContainerEngine {
    /// Connect to the engine and reap every container a previous process
    /// of this node left behind. A restarted node cannot know which of its
    /// containers still belong to cells it will own again, and a container
    /// whose object has lost track of it is a leak, so a restart starts
    /// clean. The next `start()` of each object creates a fresh one.
    async fn connect(
        backend: Arc<dyn Backend>,
        node: String,
        bucket: Option<crate::bucket::Bucket>,
    ) -> anyhow::Result<Self> {
        backend.reap(&node).await?;
        Ok(Self {
            backend,
            node,
            bucket,
            images: Mutex::new(HashMap::new()),
            cells: Mutex::new(HashMap::new()),
        })
    }

    /// Make an image present in the engine, loading it from the bucket when
    /// it is not. Idempotent and serialized per image.
    pub async fn ensure_image(&self, image: &str) -> anyhow::Result<()> {
        let lock = self
            .images
            .lock()
            .unwrap()
            .entry(image.to_string())
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(false)))
            .clone();
        let mut loaded = lock.lock().await;
        if *loaded {
            return Ok(());
        }
        if self.backend.has_image(image).await? {
            tracing::info!(image, "container image present in the engine");
            *loaded = true;
            return Ok(());
        }
        let bucket = self
            .bucket
            .as_ref()
            .ok_or_else(|| anyhow!("image {image} is not present in the container engine"))?;
        let key = image_key(image);
        let (tar, _) = bucket.get(&key).await?.ok_or_else(|| {
            anyhow!("image {image} is not in the bucket at {key}; run `celld deploy`")
        })?;
        let bytes = tar.len();
        self.backend.load_image(image, tar).await?;
        tracing::info!(image, bytes, "container image loaded from the bucket");
        *loaded = true;
        Ok(())
    }

    /// The cell's container handle, adopting a container a previous
    /// activation on this node left running. Called at cell start for a
    /// class with a spec; the object's `running` reads the answer.
    pub async fn attach(&self, scope: &str, class: &str) -> anyhow::Result<Arc<CellContainer>> {
        let spec = spec(class).ok_or_else(|| anyhow!("class {class} has no container"))?;
        let cell = {
            let mut cells = self.cells.lock().unwrap();
            let cell = cells.entry(scope.to_string()).or_insert_with(|| {
                Arc::new(CellContainer {
                    scope: scope.to_string(),
                    name: container_name(&self.node, scope),
                    spec,
                    state: Mutex::new(CellState::default()),
                    processes: Mutex::new(Vec::new()),
                    exit: watch::channel(None).0,
                })
            });
            // A returning cell cancels the destroy its eviction armed.
            if let Some(sweeper) = cell.state.lock().unwrap().sweeper.take() {
                sweeper.abort();
            }
            cell.clone()
        };
        let running = self.backend.running(&cell.name).await?;
        // A container this handle did not start, left running by an
        // earlier owner of the name: give it a run so `monitor()` has an
        // exit to wait for. The run opens before the address lands, because
        // opening one clears it.
        let adopted = running.is_some() && !cell.running();
        if adopted {
            let run = cell.begin_run();
            cell.state.lock().unwrap().info.started = true;
            self.watch_exit(&cell, run);
        }
        let mut state = cell.state.lock().unwrap();
        state.running = running.is_some();
        state.address = running.unwrap_or_default();
        drop(state);
        Ok(cell)
    }

    /// The cell's handle, if the cell started on this node.
    pub fn cell(&self, scope: &str) -> Option<Arc<CellContainer>> {
        self.cells.lock().unwrap().get(scope).cloned()
    }

    /// The cell whose container has this name.
    pub(crate) fn cell_named(&self, name: &str) -> Option<Arc<CellContainer>> {
        self.cells
            .lock()
            .unwrap()
            .values()
            .find(|cell| cell.name == name)
            .cloned()
    }

    /// The route of the intercept a container's engine named by index.
    pub(crate) fn intercept_route(&self, name: &str, index: usize) -> Option<ServiceRoute> {
        let cell = self.cell_named(name)?;
        let state = cell.state.lock().unwrap();
        state.intercepts.get(index).map(|i| i.route.clone())
    }

    /// The memory the node's running containers reserve, summed over their
    /// instance caps. A cell whose container is not running reserves
    /// nothing: a stopped container holds no memory, and an idle eviction
    /// stops it before the sample would count it.
    pub fn reserved_memory_bytes(&self) -> u64 {
        self.cells
            .lock()
            .unwrap()
            .values()
            .filter_map(|cell| {
                let state = cell.state.lock().unwrap();
                state.running.then(|| match state.info.memory_bytes {
                    0 => instance_memory_bytes(cell.spec.instance_type.as_deref()),
                    bytes => bytes,
                })
            })
            .sum()
    }

    /// This node's running containers per class.
    fn running_instances_by_class(&self) -> std::collections::BTreeMap<String, u64> {
        let mut counts = std::collections::BTreeMap::new();
        for cell in self.cells.lock().unwrap().values() {
            if cell.running() {
                *counts.entry(cell.spec.class_name.clone()).or_insert(0) += 1;
            }
        }
        counts
    }

    /// Refuse a start that would put the class over its fleet-wide
    /// `max_instances`. The fleet count is this node's own running
    /// containers of the class, read live, plus every peer's count from the
    /// shared capacity sample; `begin_run` already marked the starting cell
    /// running, so the live count includes it. A class with no
    /// `max_instances`, or a node with no bucket to read the sample from as
    /// under `celld dev`, has no fleet ceiling. See
    /// [`crate::ownership_store::fleet_class_instances`] for the staleness
    /// the sample admits.
    async fn enforce_instance_ceiling(&self, cell: &CellContainer) -> anyhow::Result<()> {
        let Some(max) = cell.spec.max_instances else {
            return Ok(());
        };
        let class = &cell.spec.class_name;
        let here = self
            .cells
            .lock()
            .unwrap()
            .values()
            .filter(|other| other.running() && other.spec.class_name == *class)
            .count() as u64;
        let elsewhere = match &self.bucket {
            Some(bucket) => {
                crate::ownership_store::fleet_class_instances(bucket, class, &self.node).await
            }
            None => 0,
        };
        anyhow::ensure!(
            here + elsewhere <= max,
            "container class {class} is at its max_instances limit of {max}: \
             {here} on this node and {elsewhere} on other nodes",
        );
        Ok(())
    }

    /// Start the cell's container for the run `begin_run` opened. The
    /// object's `start()` returns before this completes, as on Cloudflare;
    /// a failure surfaces through `monitor()`.
    pub async fn start(
        &self,
        cell: &Arc<CellContainer>,
        run: u64,
        params: StartParams,
    ) -> anyhow::Result<()> {
        let started = self.create_and_start(cell, params).await;
        let address = match started {
            Ok(address) => address,
            Err(error) => {
                // `start()` is fire-and-forget for the object, and its
                // `monitor()` sees this error only if it was already
                // waiting; say it here too, or a start that fails fast
                // leaves no trace anywhere.
                tracing::warn!(
                    event = "container_start_failed",
                    cell = %cell.scope,
                    engine = self.backend.kind(),
                    error = %format!("{error:#}"),
                    "the container did not start"
                );
                let mut state = cell.state.lock().unwrap();
                state.running = false;
                drop(state);
                cell.exit
                    .send_replace(Some((run, Err(format!("{error:#}")))));
                return Err(error);
            }
        };
        // `destroy()` may have come while the start was under way (`start()`
        // returns at once, and its object can destroy at once): what the
        // start made goes too, and the run ends destroyed.
        let destroyed = {
            let mut state = cell.state.lock().unwrap();
            let destroyed = state.destroyed == Some(run) || state.run != run;
            if !destroyed {
                state.running = true;
                state.address = address;
                state.info.started = true;
            }
            destroyed
        };
        if destroyed {
            self.backend.remove(&cell.name).await;
            cell.exit.send_replace(Some((
                run,
                Ok(RunExit {
                    code: 137,
                    destroyed: true,
                }),
            )));
            return Ok(());
        }
        self.watch_exit(cell, run);
        Ok(())
    }

    /// Wait for the container's root process to end and publish the exit
    /// for `run`; a later run's start has already replaced the state.
    fn watch_exit(&self, cell: &Arc<CellContainer>, run: u64) {
        let backend = self.backend.clone();
        let cell_ = cell.clone();
        asyncrt::spawn(async move {
            let result = backend
                .wait(&cell_.name)
                .await
                .map_err(|error| format!("{error:#}"));
            let mut state = cell_.state.lock().unwrap();
            let destroyed = state.destroyed == Some(run);
            if state.run == run {
                state.running = false;
                if let Address::Forwarded(forwarder) = std::mem::take(&mut state.address) {
                    forwarder.close();
                }
            }
            drop(state);
            let result = result.map(|exit| RunExit {
                code: exit.code,
                destroyed: exit.destroyed || destroyed,
            });
            cell_.exit.send_replace(Some((run, result)));
        })
        .detach();
    }

    async fn create_and_start(
        &self,
        cell: &CellContainer,
        params: StartParams,
    ) -> anyhow::Result<Address> {
        self.backend.prepare().await?;
        self.enforce_instance_ceiling(cell).await?;
        let spec = &cell.spec;
        let image = match (&params.image, &params.snapshot) {
            (Some(_), Some(_)) => {
                return Err(anyhow!("start(): pass an image or a containerSnapshot, not both"))
            }
            (Some(image), None) => {
                if !spec.all_images().any(|known| known == image) {
                    return Err(anyhow!(
                        "start(): the image must be one of ctx.container.images or the class's image"
                    ));
                }
                Some(image.clone())
            }
            (None, Some(_)) => {
                if !self.backend.restores_snapshots() {
                    return Err(anyhow!(
                        "start(): containerSnapshot is not supported by celld's {} engine",
                        self.backend.kind()
                    ));
                }
                None
            }
            (None, None) => Some(spec.image.clone().ok_or_else(|| {
                anyhow!("start(): this class has the durable_object scheduling policy; pass an image")
            })?),
        };
        if let Some(image) = &image {
            self.ensure_image(image).await?;
        }
        let instance = params.instance.clone().unwrap_or_else(|| {
            Instance::Named(
                spec.instance_type
                    .clone()
                    .unwrap_or_else(|| DEFAULT_INSTANCE_TYPE.to_string()),
            )
        });
        let (_, memory_bytes) = instance
            .resources()
            .ok_or_else(|| anyhow!("start(): {instance:?} is not an instance type"))?;
        let mut env: Vec<String> = DEFAULT_ENV.iter().map(|entry| entry.to_string()).collect();
        env.push(format!("CLOUDFLARE_DURABLE_OBJECT_ID={}", cell.scope));
        env.extend(
            params
                .env
                .iter()
                .map(|(name, value)| format!("{name}={value}")),
        );
        let intercepts = {
            let mut state = cell.state.lock().unwrap();
            state.info = RunInfo {
                image: image.clone().unwrap_or_default(),
                labels: params.labels.iter().cloned().collect(),
                from_snapshot: params.snapshot.is_some(),
                started: false,
                memory_bytes,
            };
            state.intercepts.iter().map(|i| i.rule.clone()).collect()
        };
        // A container under a non-default runtime is Docker's matter; the
        // class names one, else the node default.
        let runtime = spec.runtime.clone();
        self.backend
            .create_and_start(
                &cell.name,
                Launch {
                    scope: cell.scope.clone(),
                    node: self.node.clone(),
                    class: spec.class_name.clone(),
                    image,
                    snapshot: params.snapshot,
                    env,
                    labels: params.labels,
                    entrypoint: params.entrypoint,
                    enable_internet: params.enable_internet,
                    instance,
                    runtime,
                    intercepts,
                },
            )
            .await
    }

    /// Wait for the current run to end.
    pub async fn monitor(&self, cell: &Arc<CellContainer>, run: u64) -> Result<RunExit, String> {
        let mut receiver = cell.exit.subscribe();
        loop {
            if let Some((ended, result)) = receiver.borrow_and_update().clone() {
                if ended >= run {
                    return result;
                }
            }
            if receiver.changed().await.is_err() {
                return Err("the container engine went away".to_string());
            }
        }
    }

    pub async fn destroy(&self, cell: &Arc<CellContainer>) -> anyhow::Result<()> {
        {
            let mut state = cell.state.lock().unwrap();
            state.destroyed = Some(state.run);
        }
        self.backend.remove(&cell.name).await;
        let mut state = cell.state.lock().unwrap();
        state.running = false;
        if let Address::Forwarded(forwarder) = std::mem::take(&mut state.address) {
            forwarder.close();
        }
        Ok(())
    }

    pub async fn signal(&self, cell: &Arc<CellContainer>, signal: u32) -> anyhow::Result<()> {
        self.backend.signal(&cell.name, signal).await
    }

    /// `snapshotContainer({name})`.
    pub async fn snapshot(&self, cell: &Arc<CellContainer>, name: Option<String>) -> anyhow::Result<Value> {
        if !cell.running() {
            return Err(anyhow!("snapshotContainer() requires a running container."));
        }
        self.backend.snapshot(&cell.name, name).await
    }

    /// `interceptOutbound*`: added for the cell's container's life, and to a
    /// running container at once. Cloudflare's count of 128 entries holds.
    pub async fn intercept(
        &self,
        cell: &Arc<CellContainer>,
        rule: InterceptRule,
        route: ServiceRoute,
    ) -> anyhow::Result<()> {
        // Refused before it is recorded: a rule kept for an engine that
        // cannot route it would refuse every later start.
        anyhow::ensure!(
            self.backend.intercepts(),
            "interceptOutboundHttp() is not supported by celld's {} engine",
            self.backend.kind()
        );
        let rules = {
            let mut state = cell.state.lock().unwrap();
            let used: usize = state.intercepts.iter().map(|i| i.rule.entries()).sum();
            anyhow::ensure!(
                used + rule.entries() <= INTERCEPT_ENTRIES_MAX,
                "a container takes at most {INTERCEPT_ENTRIES_MAX} intercept entries (a hostname counts two)"
            );
            state.intercepts.push(Intercept { rule, route });
            let running = state.running && state.info.started;
            running.then(|| state.intercepts.iter().map(|i| i.rule.clone()).collect::<Vec<_>>())
        };
        if let Some(rules) = rules {
            self.backend.set_intercepts(&cell.name, &rules).await?;
        }
        Ok(())
    }

    /// The cell left this node's runtime. An idle eviction keeps the
    /// container for its inactivity window; anything else destroys it, and
    /// the stop waits for that: a drain ends the process right after its
    /// last stop, and a destroy left to a detached task did not always get
    /// its two daemon calls in before the exit.
    pub async fn release(self: &Arc<Self>, scope: &str, release: Release) {
        let Some(cell) = self.cell(scope) else {
            return;
        };
        match release {
            Release::Keep => {
                let window = cell
                    .state
                    .lock()
                    .unwrap()
                    .inactivity
                    .unwrap_or(DEFAULT_INACTIVITY);
                let engine = self.clone();
                let cell_ = cell.clone();
                let sweeper = asyncrt::spawn(async move {
                    asyncrt::sleep(window).await;
                    engine.forget(&cell_).await;
                });
                if let Some(previous) = cell.state.lock().unwrap().sweeper.replace(sweeper) {
                    previous.abort();
                }
            }
            Release::Destroy => self.forget(&cell).await,
        }
    }

    async fn forget(&self, cell: &Arc<CellContainer>) {
        let _ = self.destroy(cell).await;
        for id in cell.processes.lock().unwrap().drain(..) {
            drop_process(id);
        }
        let mut cells = self.cells.lock().unwrap();
        if cells
            .get(&cell.scope)
            .is_some_and(|current| Arc::ptr_eq(current, cell))
        {
            cells.remove(&cell.scope);
        }
    }

    pub async fn exec(
        &self,
        cell: &Arc<CellContainer>,
        params: ExecParams,
    ) -> anyhow::Result<Arc<ExecProcess>> {
        if !cell.running() {
            return Err(anyhow!("exec() requires a running container."));
        }
        let pty = params.pty.is_some();
        let started = self.backend.exec(&cell.name, &params).await?;
        let process = Arc::new(ExecProcess {
            id: next_exec_id(),
            pid: started.pid,
            pty,
            io: started.io,
            stdout: tokio::sync::Mutex::new(started.stdout),
            stderr: tokio::sync::Mutex::new(started.stderr),
            exit_code: tokio::sync::Mutex::new(None),
        });
        processes()
            .lock()
            .unwrap()
            .insert(process.id, process.clone());
        cell.processes.lock().unwrap().push(process.id);
        Ok(process)
    }
}

/// Where an exec's output goes, as `exec()`'s options say.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum OutputMode {
    Pipe,
    Ignore,
    /// stderr only: folded into stdout.
    Combined,
}

pub struct ExecParams {
    pub cmd: Vec<String>,
    pub env: Vec<(String, String)>,
    pub cwd: Option<String>,
    pub user: Option<String>,
    pub stdin: bool,
    pub stdout: OutputMode,
    pub stderr: OutputMode,
    /// A pseudo-terminal of `(cols, rows)`.
    pub pty: Option<(u16, u16)>,
}

/// An exec's running half, the engine's: its input, its end, its signals.
#[async_trait::async_trait]
pub(crate) trait ExecIo: Send + Sync {
    async fn write(&self, bytes: &[u8]) -> Result<(), String>;
    async fn close_stdin(&self);
    /// The exit code (128 + n for a signal).
    async fn wait(&self) -> anyhow::Result<i64>;
    async fn kill(&self, signal: u32) -> anyhow::Result<()>;
    async fn resize(&self, cols: u16, rows: u16) -> anyhow::Result<()>;
}

/// What a backend's `exec` returns once the process has started.
pub(crate) struct ExecStarted {
    pub pid: i64,
    pub io: Box<dyn ExecIo>,
    pub stdout: mpsc::Receiver<Bytes>,
    pub stderr: mpsc::Receiver<Bytes>,
}

pub struct ExecProcess {
    pub id: u64,
    pub pid: i64,
    pub pty: bool,
    io: Box<dyn ExecIo>,
    stdout: tokio::sync::Mutex<mpsc::Receiver<Bytes>>,
    stderr: tokio::sync::Mutex<mpsc::Receiver<Bytes>>,
    exit_code: tokio::sync::Mutex<Option<i64>>,
}

fn processes() -> &'static Mutex<HashMap<u64, Arc<ExecProcess>>> {
    static PROCESSES: OnceLock<Mutex<HashMap<u64, Arc<ExecProcess>>>> = OnceLock::new();
    PROCESSES.get_or_init(Default::default)
}

fn next_exec_id() -> u64 {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

pub fn process(id: u64) -> Option<Arc<ExecProcess>> {
    processes().lock().unwrap().get(&id).cloned()
}

pub fn drop_process(id: u64) {
    processes().lock().unwrap().remove(&id);
}

impl ExecProcess {
    /// One chunk of stdout (`1`) or stderr (`2`); empty at end of stream.
    pub async fn read(&self, which: u8) -> Bytes {
        let mut receiver = match which {
            1 => self.stdout.lock().await,
            _ => self.stderr.lock().await,
        };
        receiver.recv().await.unwrap_or_default()
    }

    pub async fn write(&self, bytes: &[u8]) -> Result<(), String> {
        self.io.write(bytes).await
    }

    pub async fn close_stdin(&self) {
        self.io.close_stdin().await;
    }

    pub async fn wait(&self) -> anyhow::Result<i64> {
        let mut exit = self.exit_code.lock().await;
        if let Some(code) = *exit {
            return Ok(code);
        }
        let code = self.io.wait().await?;
        *exit = Some(code);
        Ok(code)
    }

    pub async fn kill(&self, signal: u32) -> anyhow::Result<()> {
        self.io.kill(signal).await
    }

    pub async fn resize(&self, cols: u16, rows: u16) -> anyhow::Result<()> {
        if !self.pty {
            return Err(anyhow!("resize() requires a process started with pty"));
        }
        self.io.resize(cols, rows).await
    }
}

/// A container name from a node and a cell scope: the scope's characters
/// are not all legal, so the name is a hash and the scope rides in a label
/// (Docker) or in celld's map (krun). The node is part of it because two
/// nodes can share one engine in a development or test setup, and a name
/// from the scope alone let one node adopt, or reap, the other's container
/// for the same object; its own tag up front lets a node reap its own by
/// prefix.
pub(crate) fn container_name(node: &str, scope: &str) -> String {
    use sha2::Digest;
    let digest = sha2::Sha256::digest(format!("{node}\n{scope}").as_bytes());
    format!("{}{:x}", node_prefix(node), digest)[..30].to_string()
}

/// The start of every container name of `node`.
pub(crate) fn node_prefix(node: &str) -> String {
    use sha2::Digest;
    let digest = sha2::Sha256::digest(node.as_bytes());
    format!("celld-{}-", &format!("{digest:x}")[..6])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_carry_their_node() {
        let a = container_name("node-a", "Box:1");
        assert_eq!(a.len(), 30);
        assert!(a.starts_with(&node_prefix("node-a")));
        assert!(!a.starts_with(&node_prefix("node-b")));
        assert_ne!(a, container_name("node-a", "Box:2"));
    }

    #[test]
    fn intercept_entries_count_as_cloudflare_counts() {
        let rule = |target: &str| InterceptRule { scheme: "http", target: target.into() };
        assert_eq!(rule("api.example.com").entries(), 2);
        assert_eq!(rule("*.example.com").entries(), 2);
        assert_eq!(rule("*").entries(), 2);
        assert_eq!(rule("api.example.com:8443").entries(), 2);
        assert_eq!(rule("15.0.0.1:80").entries(), 1);
        assert_eq!(rule("203.0.113.0/24").entries(), 1);
    }

    #[test]
    fn engine_choice() {
        assert_eq!(EngineChoice::parse(None).unwrap(), EngineChoice::Docker);
        assert_eq!(EngineChoice::parse(Some("docker")).unwrap(), EngineChoice::Docker);
        assert_eq!(
            EngineChoice::parse(Some("krun:/var/lib/e/engine.sock")).unwrap(),
            EngineChoice::Krun("/var/lib/e/engine.sock".into())
        );
        assert!(EngineChoice::parse(Some("krun:relative.sock")).is_err());
        assert!(EngineChoice::parse(Some("podman")).is_err());
    }

    #[test]
    fn instances_to_the_engine() {
        assert_eq!(Instance::Named("dev".into()).to_json(), json!("lite"));
        assert_eq!(Instance::Named("standard".into()).to_json(), json!("standard-1"));
        assert_eq!(
            Instance::Custom { vcpu: 2.0, memory_mib: 6144, disk_mb: 8000 }.to_json(),
            json!({"vcpu": 2.0, "memoryMib": 6144, "diskMb": 8000})
        );
        assert_eq!(Instance::Named("basic".into()).resources(), Some((0.25, 1 << 30)));
    }
}
