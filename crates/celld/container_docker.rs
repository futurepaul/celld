// Copyright 2026 Deno Land Inc. Apache-2.0 license.

//! The Docker engine behind `ctx.container` (`container::Backend`): the
//! Docker Engine API on the daemon's unix socket, Podman's too. Each
//! container joins one of two bridges, one with egress and one without,
//! and the node fences both with nftables from a one-shot privileged
//! container.

use crate::asyncrt;
use crate::container::{
    fence_image, Address, Backend, ExecIo, ExecParams, ExecStarted, InterceptRule, Launch,
    OutputMode, RunExit,
};
use crate::docker::{frame_header, Docker, Stream};
use anyhow::{anyhow, Context};
use bytes::Bytes;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::{mpsc, watch};

/// The resolvers a container gets when it runs under a non-default runtime
/// and the operator set none. gVisor's network stack does not reach
/// Docker's embedded resolver at `127.0.0.11`, so a container under it
/// cannot resolve a hostname though it reaches the Internet by address; a
/// public resolver, which the fence permits, restores name resolution.
/// `CELLD_CONTAINER_DNS` overrides this.
const DEFAULT_CONTAINER_DNS: &[&str] = &["1.1.1.1", "1.0.0.1"];

/// Processes one container can hold. Cloudflare publishes no number; this
/// is room for a build tool's process tree and far short of a fork bomb.
const PIDS_LIMIT: u64 = 1024;
/// The host interfaces of the two bridges, named so the fence can address
/// them; Docker's default `br-<id>` changes with every recreation.
const OPEN_BRIDGE: &str = "celld0";
const INTERNAL_BRIDGE: &str = "celld1";
const BRIDGE_NAME_OPTION: &str = "com.docker.network.bridge.name";
/// The fence, installed on the node from a one-shot privileged container of
/// the `celld-fence` image: a container may reach the Internet and nothing
/// of the node's own. Hooks before Docker's own chains (priority filter -
/// 10) so a verdict here is final. Input: no new connection from a bridge
/// to the node itself, which is where the internal listener and the public
/// listener bind; replies to connections the node opened still pass.
/// Forward: nothing to the private ranges, where the fleet, the VPC, and
/// the metadata service live. Rules on the host side of the bridge cover
/// every runtime, gVisor included, and no process inside a container can
/// see them, let alone remove them.
const FENCE_RULES: &str = r#"table inet celld
delete table inet celld
table inet celld {
  chain input {
    type filter hook input priority -10; policy accept;
    iifname { "celld0", "celld1" } ct state established,related accept
    iifname { "celld0", "celld1" } reject
  }
  chain forward {
    type filter hook forward priority -10; policy accept;
    iifname { "celld0", "celld1" } ip daddr { 169.254.0.0/16, 10.0.0.0/8, 172.16.0.0/12, 192.168.0.0/16, 100.64.0.0/10 } reject
    iifname { "celld0", "celld1" } ip6 daddr { fe80::/10, fc00::/7 } reject
  }
}
"#;

/// Docker's bridge option that forbids traffic between two containers on
/// the same bridge. The node still reaches every container, because it
/// speaks from the host side of the bridge.
const ICC_OPTION: &str = "com.docker.network.bridge.enable_icc";

pub(crate) struct DockerBackend {
    docker: Docker,
    node: String,
    /// See `container::Config::runtime`.
    runtime: Option<String>,
    /// See `container::Config::dns`.
    dns: Vec<String>,
    /// The absolute path of the container resolv.conf this engine wrote and
    /// bind-mounts; `None` when it could not be written.
    resolv_conf: Option<PathBuf>,
    networks: tokio::sync::Mutex<Option<(String, String)>>,
    /// Whether this process has installed the fence on the node's bridges.
    fenced: tokio::sync::Mutex<bool>,
}

impl DockerBackend {
    pub fn new(
        docker: Docker,
        node: String,
        runtime: Option<String>,
        dns: Vec<String>,
        data_dir: &Path,
    ) -> DockerBackend {
        // Write the resolv.conf once, to bind into every container that runs
        // under a non-default runtime. gVisor cannot reach Docker's embedded
        // resolver, so a bind-mounted file with a public resolver, which the
        // fence permits, is the only resolv.conf the container can use.
        let resolvers: Vec<String> = if dns.is_empty() {
            DEFAULT_CONTAINER_DNS
                .iter()
                .map(|r| r.to_string())
                .collect()
        } else {
            dns.clone()
        };
        let resolv_conf = {
            let path = data_dir.join("container-resolv.conf");
            let body: String = resolvers
                .iter()
                .map(|resolver| format!("nameserver {resolver}\n"))
                .collect();
            let fs = asyncrt::fs();
            match fs
                .create_dir_all(data_dir)
                .and_then(|()| fs.write(&path, body.as_bytes()))
            {
                Ok(()) => Some(path),
                Err(error) => {
                    tracing::warn!(event = "container_resolv_write_failed", %error);
                    None
                }
            }
        };
        DockerBackend {
            docker,
            node,
            runtime,
            dns,
            resolv_conf,
            networks: tokio::sync::Mutex::new(None),
            fenced: tokio::sync::Mutex::new(false),
        }
    }

    /// The two bridges every container joins: one with egress, one
    /// without. Docker implements an internal network by omitting the
    /// masquerade rule and the default route; the node still reaches the
    /// bridge address, which is all ingress needs.
    async fn networks(&self) -> anyhow::Result<(String, String)> {
        let mut guard = self.networks.lock().await;
        if let Some(names) = guard.as_ref() {
            return Ok(names.clone());
        }
        for (name, internal) in [("celld", false), ("celld-internal", true)] {
            // A bridge keeps the options it was created with, so one from
            // before containers were isolated from each other is replaced.
            // The replacement fails while a container is attached, which a
            // node start after its reap never has; a failure keeps the old
            // bridge and says so rather than refusing every container.
            let current = self
                .docker
                .call("GET", &format!("/networks/{name}"), None)
                .await?;
            if current.status.is_success() {
                let bridge = if internal {
                    INTERNAL_BRIDGE
                } else {
                    OPEN_BRIDGE
                };
                let current_options = current
                    .json()
                    .ok()
                    .and_then(|network| network.get("Options").cloned())
                    .unwrap_or(Value::Null);
                let option = |key: &str| current_options.get(key).and_then(Value::as_str);
                if option(ICC_OPTION) == Some("false") && option(BRIDGE_NAME_OPTION) == Some(bridge)
                {
                    continue;
                }
                let removed = self
                    .docker
                    .call("DELETE", &format!("/networks/{name}"), None)
                    .await?;
                if !removed.status.is_success() {
                    tracing::warn!(
                        network = name,
                        error = %removed.message(),
                        "the container bridge predates container isolation and is in use; \
                         containers on it can reach each other until the node restarts idle"
                    );
                    continue;
                }
            }
            let reply = self
                .docker
                .call(
                    "POST",
                    "/networks/create",
                    Some(json!({
                        "Name": name,
                        "Driver": "bridge",
                        "Internal": internal,
                        "Options": {
                            ICC_OPTION: "false",
                            BRIDGE_NAME_OPTION: if internal { INTERNAL_BRIDGE } else { OPEN_BRIDGE },
                        },
                    })),
                )
                .await?;
            // 409: it exists, which is the steady state.
            if !reply.status.is_success() && reply.status.as_u16() != 409 {
                return Err(anyhow!(
                    "create network {name} failed with [{}] {}",
                    reply.status.as_u16(),
                    reply.message()
                ));
            }
        }
        let names = ("celld".to_string(), "celld-internal".to_string());
        *guard = Some(names.clone());
        Ok(names)
    }

    /// Install the fence on the node's bridges, once per process. Every
    /// container start waits on this and fails when it fails: a node that
    /// cannot fence its bridges runs no container, because an unfenced
    /// container can reach the node's internal listener and the cloud's
    /// metadata service. The rules live in the kernel and outlive this
    /// process; a restart re-applies them, which is idempotent.
    async fn ensure_fence(&self) -> anyhow::Result<()> {
        let mut fenced = self.fenced.lock().await;
        if *fenced {
            return Ok(());
        }
        let image = fence_image().ok_or_else(|| {
            anyhow!(
                "this deployment has no fence image; deploy it again with this celld, which \
                 saves the celld-fence image beside the container images"
            )
        })?;
        let engine = crate::container::engine().await?;
        engine.ensure_image(&image).await?;
        self.networks().await?;
        let body = json!({
            "Image": image,
            "Env": [format!("CELLD_NFT={FENCE_RULES}")],
            "Cmd": ["sh", "-c", "printf '%s' \"$CELLD_NFT\" | nft -f -"],
            "Labels": { "celld.node": self.node, "celld.fence": "1" },
            "HostConfig": { "NetworkMode": "host", "CapAdd": ["NET_ADMIN"] },
        });
        let created = self
            .docker
            .expect(
                "POST",
                "/containers/create",
                Some(body),
                "create fence container",
            )
            .await?
            .json()?;
        let id = created
            .get("Id")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("create fence container answered without an id"))?
            .to_string();
        let outcome = async {
            self.docker
                .expect(
                    "POST",
                    &format!("/containers/{id}/start"),
                    None,
                    "start fence container",
                )
                .await?;
            let waited = self
                .docker
                .expect(
                    "POST",
                    &format!("/containers/{id}/wait"),
                    None,
                    "wait fence container",
                )
                .await?
                .json()?;
            let code = waited
                .get("StatusCode")
                .and_then(Value::as_i64)
                .unwrap_or(-1);
            if code != 0 {
                let logs = self.logs(&id).await.unwrap_or_default();
                return Err(anyhow!(
                    "the fence container exited with {code}: {}",
                    logs.trim()
                ));
            }
            Ok(())
        }
        .await;
        let _ = self
            .docker
            .call("DELETE", &format!("/containers/{id}?force=true"), None)
            .await;
        outcome.context("fence the container bridges")?;
        tracing::info!(
            event = "container_bridges_fenced",
            bridges = format!("{OPEN_BRIDGE},{INTERNAL_BRIDGE}"),
            "containers can reach the Internet and nothing of the node's own"
        );
        *fenced = true;
        Ok(())
    }

    /// Both output streams of a stopped container, for an error message.
    async fn logs(&self, id: &str) -> anyhow::Result<String> {
        let reply = self
            .docker
            .expect(
                "GET",
                &format!("/containers/{id}/logs?stdout=true&stderr=true"),
                None,
                "container logs",
            )
            .await?;
        let mut text = Vec::new();
        let mut rest: &[u8] = &reply.body;
        while rest.len() >= 8 {
            let (_, length) = frame_header(rest[..8].try_into().unwrap());
            let end = (8 + length).min(rest.len());
            text.extend_from_slice(&rest[8..end]);
            rest = &rest[end..];
        }
        Ok(String::from_utf8_lossy(&text).into_owned())
    }
}

#[async_trait::async_trait]
impl Backend for DockerBackend {
    fn kind(&self) -> &'static str {
        "Docker"
    }

    async fn reap(&self, node: &str) -> anyhow::Result<()> {
        let filters = json!({ "label": [format!("celld.node={node}")] }).to_string();
        let reply = self
            .docker
            .expect(
                "GET",
                &format!(
                    "/containers/json?all=true&filters={}",
                    percent_encoding::utf8_percent_encode(
                        &filters,
                        percent_encoding::NON_ALPHANUMERIC
                    )
                ),
                None,
                "list containers",
            )
            .await?;
        let list = reply.json()?;
        for entry in list.as_array().into_iter().flatten() {
            if let Some(id) = entry.get("Id").and_then(Value::as_str) {
                let _ = self
                    .docker
                    .call("DELETE", &format!("/containers/{id}?force=true"), None)
                    .await;
            }
        }
        Ok(())
    }

    async fn prepare(&self) -> anyhow::Result<()> {
        self.ensure_fence().await
    }

    async fn has_image(&self, image: &str) -> anyhow::Result<bool> {
        let reply = self
            .docker
            .call("GET", &format!("/images/{image}/json"), None)
            .await?;
        Ok(reply.status.is_success())
    }

    async fn load_image(&self, _image: &str, tar: Bytes) -> anyhow::Result<()> {
        self.docker
            .post_octets("/images/load?quiet=true", tar, "load image")
            .await?;
        Ok(())
    }

    async fn running(&self, name: &str) -> anyhow::Result<Option<Address>> {
        let reply = self
            .docker
            .call("GET", &format!("/containers/{name}/json"), None)
            .await?;
        if reply.status.as_u16() == 404 {
            return Ok(None);
        }
        if !reply.status.is_success() {
            return Err(anyhow!(
                "inspect container failed with [{}] {}",
                reply.status.as_u16(),
                reply.message()
            ));
        }
        let info = reply.json()?;
        let running = info
            .pointer("/State/Running")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        Ok(running.then(|| address_of(&info)))
    }

    async fn create_and_start(&self, name: &str, launch: Launch) -> anyhow::Result<Address> {
        let image = launch
            .image
            .clone()
            .ok_or_else(|| anyhow!("the Docker engine starts an image"))?;
        if !launch.intercepts.is_empty() {
            return Err(anyhow!(
                "interceptOutboundHttp() is not supported by celld's Docker engine"
            ));
        }
        let (open, internal) = self.networks().await?;
        // A previous run's container may still be named: the wait task
        // observed its exit but nothing removed it, or a restart adopted a
        // stopped one. The name is the cell's, so it is ours to remove.
        let _ = self
            .docker
            .call("DELETE", &format!("/containers/{name}?force=true"), None)
            .await;
        let mut labels = serde_json::Map::new();
        labels.insert("celld.node".into(), json!(launch.node));
        labels.insert("celld.cell".into(), json!(launch.scope));
        labels.insert("celld.class".into(), json!(launch.class));
        for (label, value) in &launch.labels {
            labels.insert(format!("celld.user.{label}"), json!(value));
        }
        // Off Linux the node reaches a container only through published
        // ports, and Docker publishes nothing on an internal network, so
        // `enableInternet: false` would make the container unreachable.
        // Development there keeps egress; the compat page says so.
        let offline = !launch.enable_internet && cfg!(target_os = "linux");
        if !launch.enable_internet && !offline {
            tracing::warn!(
                cell = %launch.scope,
                "enableInternet: false is not enforced on this platform; the container keeps egress"
            );
        }
        // A container is a tenant's process tree, not the operator's: no
        // capability, no privilege gain through setuid binaries, the
        // daemon's seccomp profile, and a process ceiling. What a class
        // legitimately needs beyond this is a config question for later,
        // not a default. The kernel boundary itself is the runtime's.
        let mut host_config = json!({
            "NetworkMode": if offline { &internal } else { &open },
            "PublishAllPorts": !cfg!(target_os = "linux"),
            "CapDrop": ["ALL"],
            "SecurityOpt": ["no-new-privileges"],
            "PidsLimit": PIDS_LIMIT,
            // An init as PID 1 reaps orphaned children. Without it a process
            // whose parent exits becomes a zombie the container holds until
            // it stops, and enough of them exhaust the PID ceiling; a fleet
            // fork bomb left exactly these behind.
            "Init": true,
        });
        // A class can name its own runtime, else the node default. A class
        // that needs isolation names `runsc`, and the node's daemon must
        // have it or the start fails, so the class runs only where its
        // isolation is real.
        let runtime = launch
            .runtime
            .as_deref()
            .or(self.runtime.as_deref())
            .filter(|runtime| !runtime.is_empty());
        if let Some(runtime) = runtime {
            host_config["Runtime"] = json!(runtime);
        }
        // A container under a non-default runtime gets an explicit resolver
        // through a bind-mounted resolv.conf: gVisor cannot reach Docker's
        // embedded resolver at `127.0.0.11`, and on a user bridge Docker
        // keeps that address in resolv.conf whatever `--dns` says, so the
        // file itself must name a reachable resolver. An operator who set
        // `CELLD_CONTAINER_DNS` gets it for every container.
        let wants_resolver = runtime.is_some() || !self.dns.is_empty();
        if wants_resolver {
            if let Some(resolv) = &self.resolv_conf {
                let bind = format!("{}:/etc/resolv.conf:ro", resolv.display());
                host_config["Binds"] = json!([bind]);
            }
        }
        // Every container has a limit: a class without an instance type
        // gets Cloudflare's default type rather than the node.
        let (vcpu, memory) = launch
            .instance
            .resources()
            .ok_or_else(|| anyhow!("{:?} is not an instance type", launch.instance))?;
        host_config["NanoCpus"] = json!((vcpu * 1e9) as u64);
        host_config["Memory"] = json!(memory);
        host_config["MemorySwap"] = json!(memory);
        let mut body = json!({
            "Image": image,
            "Env": launch.env,
            "Labels": labels,
            "HostConfig": host_config,
        });
        if let Some(entrypoint) = launch.entrypoint {
            body["Cmd"] = json!(entrypoint);
        }
        // The daemon can answer 409 for a name whose previous container is
        // still being removed, so a fresh start after `destroy()` retries
        // briefly, as workerd's engine does.
        let mut reply = self
            .docker
            .call(
                "POST",
                &format!("/containers/create?name={name}"),
                Some(body.clone()),
            )
            .await?;
        for _ in 0..20 {
            if reply.status.as_u16() != 409 {
                break;
            }
            asyncrt::sleep(Duration::from_millis(100)).await;
            let _ = self
                .docker
                .call("DELETE", &format!("/containers/{name}?force=true"), None)
                .await;
            reply = self
                .docker
                .call(
                    "POST",
                    &format!("/containers/create?name={name}"),
                    Some(body.clone()),
                )
                .await?;
        }
        if reply.status.as_u16() == 404 {
            return Err(anyhow!("No such image available named {image}"));
        }
        if !reply.status.is_success() {
            return Err(anyhow!(
                "Create container failed with [{}] {}",
                reply.status.as_u16(),
                reply.message()
            ));
        }
        self.docker
            .expect(
                "POST",
                &format!("/containers/{name}/start"),
                None,
                "start container",
            )
            .await?;
        let info = self
            .docker
            .expect(
                "GET",
                &format!("/containers/{name}/json"),
                None,
                "inspect container",
            )
            .await?
            .json()?;
        Ok(address_of(&info))
    }

    async fn wait(&self, name: &str) -> anyhow::Result<RunExit> {
        let reply = self
            .docker
            .call("POST", &format!("/containers/{name}/wait"), None)
            .await?;
        if !reply.status.is_success() {
            return Err(anyhow!(
                "wait failed with [{}] {}",
                reply.status.as_u16(),
                reply.message()
            ));
        }
        let code = reply
            .json()?
            .get("StatusCode")
            .and_then(Value::as_i64)
            .ok_or_else(|| anyhow!("wait answered without a status code"))?;
        Ok(RunExit {
            code,
            destroyed: false,
        })
    }

    async fn signal(&self, name: &str, signal: u32) -> anyhow::Result<()> {
        self.docker
            .expect(
                "POST",
                &format!("/containers/{name}/kill?signal={signal}"),
                None,
                "signal container",
            )
            .await?;
        Ok(())
    }

    async fn remove(&self, name: &str) {
        // Kill first so the wait task reports 137 before the removal makes
        // the name disappear under it.
        let _ = self
            .docker
            .call(
                "POST",
                &format!("/containers/{name}/kill?signal=SIGKILL"),
                None,
            )
            .await;
        let _ = self
            .docker
            .call("DELETE", &format!("/containers/{name}?force=true"), None)
            .await;
    }

    async fn exec(&self, name: &str, params: &ExecParams) -> anyhow::Result<ExecStarted> {
        let tty = params.pty.is_some();
        let mut body = json!({
            "AttachStdin": true,
            "AttachStdout": true,
            "AttachStderr": true,
            "Tty": tty,
            "Cmd": params.cmd,
        });
        if let Some((cols, rows)) = params.pty {
            body["ConsoleSize"] = json!([rows, cols]);
        }
        if !params.env.is_empty() {
            body["Env"] = json!(params
                .env
                .iter()
                .map(|(name, value)| format!("{name}={value}"))
                .collect::<Vec<_>>());
        }
        if let Some(cwd) = &params.cwd {
            body["WorkingDir"] = json!(cwd);
        }
        if let Some(user) = &params.user {
            body["User"] = json!(user);
        }
        let created = self
            .docker
            .expect(
                "POST",
                &format!("/containers/{name}/exec"),
                Some(body),
                "create exec",
            )
            .await?
            .json()?;
        let exec_id = created
            .get("Id")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("exec create answered without an id"))?
            .to_string();
        let stream = self
            .docker
            .hijack(
                &format!("/exec/{exec_id}/start"),
                json!({ "Detach": false, "Tty": tty }),
            )
            .await?;
        // Docker reports `Running: false` with no pid before it has spawned
        // the process, and a finished exec keeps its pid, so a pid of zero
        // is the one answer that means "not yet": retry it briefly, as
        // workerd does. Breaking on `Running: false` here lost the pid of a
        // short command whenever the inspect landed in that window.
        let mut pid = 0;
        for _ in 0..20 {
            let info = self
                .docker
                .expect(
                    "GET",
                    &format!("/exec/{exec_id}/json"),
                    None,
                    "inspect exec",
                )
                .await?
                .json()?;
            pid = info.get("Pid").and_then(Value::as_i64).unwrap_or(0);
            if pid != 0 {
                break;
            }
            asyncrt::sleep(Duration::from_millis(50)).await;
        }
        let (read, write) = tokio::io::split(stream);
        let (stdout_tx, stdout_rx) = mpsc::channel(16);
        let (stderr_tx, stderr_rx) = mpsc::channel(16);
        let ended = watch::channel(false).0;
        let ended_ = ended.clone();
        let (stdout_mode, stderr_mode) = (params.stdout, params.stderr);
        asyncrt::spawn(async move {
            demux(read, stdout_tx, stderr_tx, tty, stdout_mode, stderr_mode).await;
            ended_.send_replace(true);
        })
        .detach();
        Ok(ExecStarted {
            pid,
            io: Box::new(DockerExec {
                exec_id,
                container: name.to_string(),
                pid,
                docker: self.docker.clone(),
                stdin: tokio::sync::Mutex::new(Some(write)),
                ended,
            }),
            stdout: stdout_rx,
            stderr: stderr_rx,
        })
    }

    async fn snapshot(&self, _name: &str, _snapshot_name: Option<String>) -> anyhow::Result<Value> {
        Err(anyhow!(
            "snapshotContainer() is not supported by celld's Docker engine"
        ))
    }

    async fn set_intercepts(&self, _name: &str, _intercepts: &[InterceptRule]) -> anyhow::Result<()> {
        Err(anyhow!(
            "interceptOutboundHttp() is not supported by celld's Docker engine"
        ))
    }

    fn restores_snapshots(&self) -> bool {
        false
    }

    fn intercepts(&self) -> bool {
        false
    }
}

type WriteHalf = tokio::io::WriteHalf<Box<dyn Stream>>;

struct DockerExec {
    exec_id: String,
    container: String,
    pid: i64,
    docker: Docker,
    stdin: tokio::sync::Mutex<Option<WriteHalf>>,
    /// True once the hijacked stream reached EOF, which the daemon sends
    /// when the process exits.
    ended: watch::Sender<bool>,
}

#[async_trait::async_trait]
impl ExecIo for DockerExec {
    async fn write(&self, bytes: &[u8]) -> Result<(), String> {
        let mut guard = self.stdin.lock().await;
        let stdin = guard.as_mut().ok_or("stdin is closed")?;
        stdin
            .write_all(bytes)
            .await
            .map_err(|error| format!("stdin write failed: {error}"))?;
        stdin
            .flush()
            .await
            .map_err(|error| format!("stdin flush failed: {error}"))
    }

    /// Half-closing the hijacked connection is how the daemon learns the
    /// process's stdin reached EOF.
    async fn close_stdin(&self) {
        if let Some(mut stdin) = self.stdin.lock().await.take() {
            let _ = stdin.shutdown().await;
        }
    }

    async fn wait(&self) -> anyhow::Result<i64> {
        let mut ended = self.ended.subscribe();
        while !*ended.borrow_and_update() {
            if ended.changed().await.is_err() {
                break;
            }
        }
        // The stream closes when the process exits, but the daemon records
        // the exit code a moment later.
        for _ in 0..40 {
            let info = self
                .docker
                .expect(
                    "GET",
                    &format!("/exec/{}/json", self.exec_id),
                    None,
                    "inspect exec",
                )
                .await?
                .json()?;
            if !info
                .get("Running")
                .and_then(Value::as_bool)
                .unwrap_or(false)
            {
                if let Some(code) = info.get("ExitCode").and_then(Value::as_i64) {
                    return Ok(code);
                }
                break;
            }
            asyncrt::sleep(Duration::from_millis(50)).await;
        }
        Err(anyhow!("the process did not report an exit code"))
    }

    /// The Engine API has no exec kill, so the signal is delivered by a
    /// second exec of `kill`, as workerd does.
    async fn kill(&self, signal: u32) -> anyhow::Result<()> {
        let body = json!({
            "AttachStdin": false, "AttachStdout": false, "AttachStderr": false,
            "Cmd": ["kill", format!("-{signal}"), self.pid.to_string()],
        });
        let created = self
            .docker
            .expect(
                "POST",
                &format!("/containers/{}/exec", self.container),
                Some(body),
                "create exec",
            )
            .await?
            .json()?;
        let id = created
            .get("Id")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("exec create answered without an id"))?;
        self.docker
            .expect(
                "POST",
                &format!("/exec/{id}/start"),
                Some(json!({ "Detach": true })),
                "start exec",
            )
            .await?;
        Ok(())
    }

    async fn resize(&self, cols: u16, rows: u16) -> anyhow::Result<()> {
        self.docker
            .expect(
                "POST",
                &format!("/exec/{}/resize?h={rows}&w={cols}", self.exec_id),
                None,
                "resize exec",
            )
            .await?;
        Ok(())
    }
}

/// Split Docker's multiplexed stream into stdout and stderr chunks until
/// the daemon closes it; a PTY's stream is raw, all stdout. A stream the
/// caller ignores is read and dropped, so the process never blocks on it.
async fn demux(
    mut read: tokio::io::ReadHalf<Box<dyn Stream>>,
    stdout: mpsc::Sender<Bytes>,
    stderr: mpsc::Sender<Bytes>,
    tty: bool,
    stdout_mode: OutputMode,
    stderr_mode: OutputMode,
) {
    let deliver = |stream: u8| -> Option<&mpsc::Sender<Bytes>> {
        match (stream, stdout_mode, stderr_mode) {
            (2, _, OutputMode::Pipe) => Some(&stderr),
            (2, OutputMode::Pipe, OutputMode::Combined) => Some(&stdout),
            (2, _, _) => None,
            (_, OutputMode::Pipe, _) => Some(&stdout),
            _ => None,
        }
    };
    if tty {
        let mut buf = vec![0u8; 64 * 1024];
        loop {
            let n = match read.read(&mut buf).await {
                Ok(0) | Err(_) => return,
                Ok(n) => n,
            };
            if let Some(target) = deliver(1) {
                let _ = target.send(Bytes::copy_from_slice(&buf[..n])).await;
            }
        }
    }
    let mut header = [0u8; 8];
    loop {
        if read.read_exact(&mut header).await.is_err() {
            return;
        }
        let (stream, length) = frame_header(&header);
        let mut payload = vec![0u8; length];
        if read.read_exact(&mut payload).await.is_err() {
            return;
        }
        if let Some(target) = deliver(stream) {
            // A reader that went away: keep draining so the process is not
            // blocked on a full pipe.
            let _ = target.send(Bytes::from(payload)).await;
        }
    }
}

fn address_of(info: &Value) -> Address {
    if cfg!(target_os = "linux") {
        let ip = info
            .pointer("/NetworkSettings/Networks")
            .and_then(Value::as_object)
            .and_then(|networks| networks.values().next())
            .and_then(|network| network.get("IPAddress"))
            .and_then(Value::as_str)
            .filter(|ip| !ip.is_empty());
        return ip.map_or(Address::None, |ip| Address::Ip(ip.to_string()));
    }
    let mut ports = HashMap::new();
    if let Some(map) = info
        .pointer("/NetworkSettings/Ports")
        .and_then(Value::as_object)
    {
        for (key, bindings) in map {
            let Some(port) = key
                .strip_suffix("/tcp")
                .and_then(|port| port.parse::<u16>().ok())
            else {
                continue;
            };
            let host = bindings
                .as_array()
                .into_iter()
                .flatten()
                .find_map(|binding| binding.get("HostPort")?.as_str()?.parse::<u16>().ok());
            if let Some(host) = host {
                ports.insert(port, host);
            }
        }
    }
    Address::Published(ports)
}
