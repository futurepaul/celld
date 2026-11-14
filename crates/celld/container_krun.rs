// Copyright 2026 Deno Land Inc. Apache-2.0 license.

//! The krun engine behind `ctx.container` (`container::Backend`): a node's
//! libkrun microVM engine, which speaks Cloudflare's container API itself
//! on a unix socket (fragment's `sandcastle-engine`, docs/krun-engine.md).
//! Each container is a VM, jailed, with its own network namespace and
//! egress proxy; the engine enforces `enableInternet`, the instance, and
//! the intercepts.
//!
//! Three things reach past the API's JSON:
//!
//! - **exec** upgrades to a framed stream (Docker's 8-byte header): 1
//!   stdout, 2 stderr, 3 exited, 4 started, 5 error to celld; 0 stdin, 6
//!   resize, 7 signal to the engine;
//! - **ports**: the engine's `ports.sock` hands over a connected socket
//!   (SCM_RIGHTS), TCP over the VM's NIC or the agent's socket to the
//!   guest's loopback. celld puts a loopback listener per (container, port)
//!   in front, so `fetch`, `connect`, and WebSocket upgrades dial an
//!   address as they do for Docker;
//! - **intercepts**: the engine sends an intercepted request to celld's
//!   socket naming the container and the intercept's index; celld
//!   dispatches it to the binding the object passed, on the service path.

// The engine's socket, the port listeners, and the intercept socket are
// I/O outside the execution boundary: no engine decision depends on when
// they answer, as for the Docker daemon's socket.
#![allow(clippy::disallowed_methods)]

use crate::asyncrt;
use crate::container::{
    node_prefix, Address, Backend, ExecIo, ExecParams, ExecStarted, InterceptRule, Launch,
    PortForwarder, RunExit,
};
use crate::docker::{Docker, Stream};
use anyhow::{anyhow, Context};
use bytes::Bytes;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt, AsyncWriteExt, Interest};
use tokio::sync::{mpsc, watch};

/// The exec stream's frames (the engine's `exec_stream`).
mod frames {
    pub const STDIN: u8 = 0;
    pub const STDOUT: u8 = 1;
    pub const STDERR: u8 = 2;
    pub const EXITED: u8 = 3;
    pub const STARTED: u8 = 4;
    pub const ERROR: u8 = 5;
    pub const RESIZE: u8 = 6;
    pub const SIGNAL: u8 = 7;
    pub const PAYLOAD_BYTES_MAX: usize = 1 << 20;

    pub fn encode(stream: u8, payload: &[u8]) -> Vec<u8> {
        assert!(payload.len() <= PAYLOAD_BYTES_MAX);
        let mut out = Vec::with_capacity(8 + payload.len());
        out.push(stream);
        out.extend_from_slice(&[0, 0, 0]);
        out.extend_from_slice(&(payload.len() as u32).to_be_bytes());
        out.extend_from_slice(payload);
        out
    }
}

/// fork: an intercepted request's body below this declared length is read
/// whole for the service call; one at it or past it, or of no declared
/// length, crosses as a stream, as the node's front streams a request's
/// (its `INGRESS_STREAM_THRESHOLD`), at most `CELLD_MAX_REQUEST_BODY_BYTES`.
/// A body held whole, at most 32 MiB, refused an agent's 40 MB sync.
const INTERCEPT_WHOLE_BYTES_MAX: u64 = 1 << 20;
/// A `ports.sock` reply, at most.
const PORT_REPLY_BYTES_MAX: usize = 4096;
/// fork: how often a running guest's output is read. The engine keeps the
/// tail of each stream (`GET /v1/containers/{name}/logs`, its last 64 KiB)
/// and offers no stream of it, so output past that between two reads is
/// missed, and said to be.
const OUTPUT_READ_EVERY: std::time::Duration = std::time::Duration::from_secs(1);
/// fork: the last read of a run's output, once it has ended, at most: the
/// run's end waits for it, so it is bounded.
const OUTPUT_LAST_READ_MAX: std::time::Duration = std::time::Duration::from_secs(2);
/// fork: the bytes of each stream the engine's logs answer (its
/// `LOG_TAIL_BYTES`): a first read this long may have missed output.
const OUTPUT_TAIL_BYTES: usize = 64 * 1024;
/// fork: the bytes at the end of one read that a later read is matched by.
const OUTPUT_ANCHOR_BYTES: usize = 256;
/// fork: a line of output as one event, at most; a longer one is cut there.
const OUTPUT_LINE_BYTES_MAX: usize = 16 * 1024;
/// A unix socket's path, at most: Linux's `sun_path` holds 108 bytes, the
/// last a NUL.
const SOCKET_PATH_BYTES_MAX: usize = 107;

pub(crate) struct KrunBackend {
    api: Docker,
    ports: PathBuf,
    /// celld's intercept socket, and its `unix:<path>` for the engine.
    intercepts: PathBuf,
    handler: String,
}

/// fork: where the intercept socket is bound. Beside the node's data when
/// that path fits a unix socket's; past it (`celld dev` in a project
/// several directories down, as a test's scratch is, where every bind
/// failed) in this user's runtime directory (`$XDG_RUNTIME_DIR/celld`,
/// owner-only), named by a hash of the path it would have had, so two
/// nodes never share one. Both are in the host's mount namespace, where
/// the engine (root) dials it.
fn intercept_socket(data_dir: &Path, node: &str, runtime_dir: Option<&Path>) -> anyhow::Result<PathBuf> {
    use sha2::Digest as _;
    let beside = data_dir.join(format!("intercepts-{}.sock", &node_prefix(node)[6..12]));
    if beside.as_os_str().len() <= SOCKET_PATH_BYTES_MAX {
        return Ok(beside);
    }
    let Some(runtime_dir) = runtime_dir.filter(|d| d.is_absolute()) else {
        return Err(anyhow!(
            "the intercept socket at {} is past a unix socket's {SOCKET_PATH_BYTES_MAX} bytes, and no \
             XDG_RUNTIME_DIR names a shorter place: give the node a shorter data directory",
            beside.display()
        ));
    };
    let digest = sha2::Sha256::digest(beside.as_os_str().as_encoded_bytes());
    let path = runtime_dir
        .join("celld")
        .join(format!("intercepts-{}.sock", &format!("{digest:x}")[..16]));
    anyhow::ensure!(
        path.as_os_str().len() <= SOCKET_PATH_BYTES_MAX,
        "the intercept socket at {} is past a unix socket's {SOCKET_PATH_BYTES_MAX} bytes too",
        path.display()
    );
    Ok(path)
}

impl KrunBackend {
    pub async fn new(
        socket: PathBuf,
        node: &str,
        data_dir: &Path,
        runtime: Option<crate::runtime::RuntimeManager>,
    ) -> anyhow::Result<KrunBackend> {
        let api = Docker::new(&socket);
        let health = api
            .expect("GET", "/v1/health", None, "the krun engine's health")
            .await?;
        tracing::info!(
            engine = %socket.display(),
            health = %String::from_utf8_lossy(&health.body),
            "containers run on the krun engine"
        );
        let ports = socket
            .parent()
            .map(|dir| dir.join("ports.sock"))
            .ok_or_else(|| anyhow!("the engine's socket has no directory"))?;
        let runtime_dir = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from);
        let intercepts = intercept_socket(data_dir, node, runtime_dir.as_deref())?;
        if let Some(dir) = intercepts.parent().filter(|d| *d != data_dir) {
            use std::os::unix::fs::DirBuilderExt as _;
            std::fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(dir)
                .with_context(|| format!("create {}", dir.display()))?;
        }
        serve_intercepts(&intercepts, runtime)?;
        Ok(KrunBackend {
            api,
            ports,
            handler: format!("unix:{}", intercepts.display()),
            intercepts,
        })
    }

    async fn post(&self, path: &str, body: Value, what: &str) -> anyhow::Result<Value> {
        let reply = self.api.call("POST", path, Some(body)).await?;
        if !reply.status.is_success() {
            return Err(anyhow!(
                "{what} failed with [{}] {}",
                reply.status.as_u16(),
                reply.message_or_error()
            ));
        }
        if reply.body.is_empty() {
            return Ok(Value::Null);
        }
        reply.json()
    }
}

#[async_trait::async_trait]
impl Backend for KrunBackend {
    fn kind(&self) -> &'static str {
        "krun"
    }

    async fn reap(&self, node: &str) -> anyhow::Result<()> {
        let list = self
            .api
            .expect("GET", "/v1/containers", None, "list containers")
            .await?
            .json()?;
        let prefix = node_prefix(node);
        for entry in list.as_array().into_iter().flatten() {
            if let Some(name) = entry.get("name").and_then(Value::as_str) {
                if name.starts_with(&prefix) {
                    let _ = self
                        .api
                        .call("POST", &format!("/v1/containers/{name}/destroy"), Some(json!({})))
                        .await;
                }
            }
        }
        Ok(())
    }

    async fn prepare(&self) -> anyhow::Result<()> {
        Ok(())
    }

    async fn has_image(&self, image: &str) -> anyhow::Result<bool> {
        let list = self
            .api
            .expect("GET", "/v1/images", None, "list images")
            .await?
            .json()?;
        Ok(list
            .as_array()
            .into_iter()
            .flatten()
            .any(|i| i.get("reference").and_then(Value::as_str) == Some(image)))
    }

    async fn load_image(&self, image: &str, tar: Bytes) -> anyhow::Result<()> {
        let reference =
            percent_encoding::utf8_percent_encode(image, percent_encoding::NON_ALPHANUMERIC);
        self.api
            .post_octets(&format!("/v1/images/load?reference={reference}"), tar, "load image")
            .await?;
        Ok(())
    }

    async fn running(&self, name: &str) -> anyhow::Result<Option<Address>> {
        let reply = self.api.call("GET", &format!("/v1/containers/{name}"), None).await?;
        if reply.status.as_u16() == 404 {
            return Ok(None);
        }
        if !reply.status.is_success() {
            return Err(anyhow!(
                "inspect container failed with [{}] {}",
                reply.status.as_u16(),
                reply.message_or_error()
            ));
        }
        Ok(Some(Address::Forwarded(Arc::new(Forwarder::new(name, &self.ports)))))
    }

    async fn create_and_start(&self, name: &str, launch: Launch) -> anyhow::Result<Address> {
        let env: serde_json::Map<String, Value> = launch
            .env
            .iter()
            .filter_map(|e| e.split_once('='))
            .map(|(k, v)| (k.to_string(), json!(v)))
            .collect();
        let labels: serde_json::Map<String, Value> =
            launch.labels.iter().map(|(k, v)| (k.clone(), json!(v))).collect();
        let mut body = json!({
            "enableInternet": launch.enable_internet,
            "env": env,
            "labels": labels,
            "instance": launch.instance.to_json(),
            "intercepts": intercepts_json(&launch.intercepts),
            "handler": self.handler,
        });
        match (&launch.image, &launch.snapshot) {
            (Some(image), _) => body["image"] = json!(image),
            (None, Some(id)) => body["containerSnapshot"] = json!({ "id": id }),
            (None, None) => return Err(anyhow!("a start names an image or a snapshot")),
        }
        if let Some(entrypoint) = &launch.entrypoint {
            body["entrypoint"] = json!(entrypoint);
        }
        self.post(
            &format!("/v1/containers/{name}/start?wait=ready"),
            body,
            "start container",
        )
        .await?;
        Ok(Address::Forwarded(Arc::new(Forwarder::new(name, &self.ports))))
    }

    async fn wait(&self, name: &str) -> anyhow::Result<RunExit> {
        let exit = self
            .api
            .expect("GET", &format!("/v1/containers/{name}/wait"), None, "wait container")
            .await?
            .json()?;
        let destroyed = exit.get("destroyed").and_then(Value::as_bool).unwrap_or(false);
        let code = exit.get("code").and_then(Value::as_i64);
        let signal = exit.get("signal").and_then(Value::as_i64);
        match (code, signal) {
            (Some(code), _) => Ok(RunExit { code, destroyed }),
            (None, Some(signal)) => Ok(RunExit {
                code: 128 + signal,
                destroyed,
            }),
            (None, None) if destroyed => Ok(RunExit {
                code: 137,
                destroyed,
            }),
            (None, None) => Err(anyhow!(
                "{}",
                exit.get("error")
                    .and_then(Value::as_str)
                    .unwrap_or("the container ended without an exit code")
            )),
        }
    }

    async fn signal(&self, name: &str, signal: u32) -> anyhow::Result<()> {
        self.post(
            &format!("/v1/containers/{name}/signal"),
            json!({ "signal": signal }),
            "signal container",
        )
        .await?;
        Ok(())
    }

    async fn remove(&self, name: &str) {
        let _ = self
            .api
            .call("POST", &format!("/v1/containers/{name}/destroy"), Some(json!({})))
            .await;
    }

    async fn exec(&self, name: &str, params: &ExecParams) -> anyhow::Result<ExecStarted> {
        let env: serde_json::Map<String, Value> =
            params.env.iter().map(|(k, v)| (k.clone(), json!(v))).collect();
        let mut body = json!({
            "cmd": params.cmd,
            "env": env,
            "stdin": params.stdin,
            "stdout": params.stdout,
            "stderr": params.stderr,
        });
        if let Some(cwd) = &params.cwd {
            body["cwd"] = json!(cwd);
        }
        if let Some(user) = &params.user {
            body["user"] = json!(user);
        }
        if let Some((cols, rows)) = params.pty {
            body["pty"] = json!({ "cols": cols, "rows": rows });
        }
        let stream = self
            .api
            .upgrade(&format!("/v1/containers/{name}/exec"), body, "sandcastle-exec")
            .await?;
        let (mut read, write) = tokio::io::split(stream);
        // The first frame is the start, with the pid.
        let (stream_id, payload) = read_frame(&mut read)
            .await?
            .ok_or_else(|| anyhow!("exec: the engine closed the stream before the start"))?;
        let pid = match stream_id {
            frames::STARTED => serde_json::from_slice::<Value>(&payload)
                .ok()
                .and_then(|v| v.get("pid")?.as_i64())
                .ok_or_else(|| anyhow!("exec: a start without a pid"))?,
            frames::ERROR => return Err(anyhow!("exec: {}", String::from_utf8_lossy(&payload))),
            other => return Err(anyhow!("exec: frame {other} before the start")),
        };
        let (stdout_tx, stdout_rx) = mpsc::channel(16);
        let (stderr_tx, stderr_rx) = mpsc::channel(16);
        let exit = watch::channel::<Option<Result<i64, String>>>(None).0;
        let exit_ = exit.clone();
        asyncrt::spawn(async move {
            let result = pump(read, stdout_tx, stderr_tx).await;
            exit_.send_replace(Some(result));
        })
        .detach();
        Ok(ExecStarted {
            pid,
            io: Box::new(KrunExec {
                write: tokio::sync::Mutex::new(Some(write)),
                stdin_open: Mutex::new(params.stdin),
                exit,
            }),
            stdout: stdout_rx,
            stderr: stderr_rx,
        })
    }

    async fn snapshot(&self, name: &str, snapshot_name: Option<String>) -> anyhow::Result<Value> {
        let s = self
            .post(
                &format!("/v1/containers/{name}/snapshots"),
                json!({ "name": snapshot_name }),
                "snapshot container",
            )
            .await?;
        let mut out = json!({ "id": s["id"], "size": s["size"] });
        if let Some(n) = s.get("name").filter(|n| !n.is_null()) {
            out["name"] = n.clone();
        }
        Ok(out)
    }

    async fn set_intercepts(&self, name: &str, intercepts: &[InterceptRule]) -> anyhow::Result<()> {
        let reply = self
            .api
            .call(
                "PUT",
                &format!("/v1/containers/{name}/intercepts"),
                Some(json!({ "intercepts": intercepts_json(intercepts) })),
            )
            .await?;
        if !reply.status.is_success() {
            return Err(anyhow!(
                "intercepts failed with [{}] {}",
                reply.status.as_u16(),
                reply.message_or_error()
            ));
        }
        Ok(())
    }

    fn restores_snapshots(&self) -> bool {
        true
    }

    fn intercepts(&self) -> bool {
        true
    }

    fn close(&self) {
        let _ = std::fs::remove_file(&self.intercepts);
    }

    fn follow_output(&self, name: &str, cell: &str, run: u64, from_now: bool) -> Option<crate::container::OutputEnd> {
        let output = Arc::new(Mutex::new(Output::new(cell, run)));
        let (api, name) = (self.api.clone(), name.to_string());
        let reader = {
            let (api, name, output) = (api.clone(), name.clone(), output.clone());
            asyncrt::spawn(async move {
                if from_now {
                    read_output(&api, &name, &output, Read::Unshown).await;
                }
                // Bounded by the run: the engine answers 404 once the
                // container is gone, and the run's end aborts it before.
                loop {
                    asyncrt::sleep(OUTPUT_READ_EVERY).await;
                    if !read_output(&api, &name, &output, Read::Shown).await {
                        return;
                    }
                }
            })
        };
        Some(Box::new(move |read_last: bool| -> futures_util::future::BoxFuture<'static, ()> {
            Box::pin(async move {
                // Cancelled only at its read of the engine: the output is
                // held under a lock no await is made under.
                reader.abort();
                if read_last {
                    let _ = asyncrt::timeout(OUTPUT_LAST_READ_MAX, read_output(&api, &name, &output, Read::Last)).await;
                }
            })
        }))
    }
}

/// fork: one run's output as shown so far: where each stream was read to.
struct Output {
    cell: String,
    run: u64,
    stdout: OutputCursor,
    stderr: OutputCursor,
}

impl Output {
    fn new(cell: &str, run: u64) -> Output {
        Output {
            cell: cell.to_string(),
            run,
            stdout: OutputCursor::default(),
            stderr: OutputCursor::default(),
        }
    }

    /// A newer read of both streams: the lines it ends, shown when `show`;
    /// `last`, the run's last read, shows the rest too.
    fn advance(&mut self, stdout: &str, stderr: &str, show: bool, last: bool) {
        let (cell, run) = (self.cell.as_str(), self.run);
        for (stream, cursor, tail) in [("stdout", &mut self.stdout, stdout), ("stderr", &mut self.stderr, stderr)] {
            let (mut lines, missed) = cursor.advance(tail.as_bytes());
            if last {
                lines.extend(cursor.rest(tail.as_bytes()));
            }
            if !show {
                continue;
            }
            if missed {
                tracing::warn!(
                    event = "container_output_missed",
                    cell,
                    run,
                    stream,
                    "the container wrote more than the engine keeps between two reads: some of its output is not shown"
                );
            }
            for line in lines {
                tracing::info!(event = "container_output", cell, run, stream, "{line}");
            }
        }
    }
}

/// fork: one stream of a run's output, read as the engine's tail of it.
#[derive(Default)]
struct OutputCursor {
    /// The tail the last read answered, up to the end of its last line: a
    /// line still being written (its last character perhaps in part, which
    /// the engine's lossy decoding replaces) is taken once it has ended.
    tail: Vec<u8>,
    /// Whether a read has answered yet.
    read: bool,
}

impl OutputCursor {
    /// A newer tail of the stream: the lines it ends, and whether output
    /// was missed before it (nothing of the last read is in it, or the
    /// first read is a whole tail).
    fn advance(&mut self, next: &[u8]) -> (Vec<String>, bool) {
        let (from, mut missed) = new_since(&self.tail, next);
        missed |= !self.read && next.len() >= OUTPUT_TAIL_BYTES;
        self.read = true;
        let fresh = &next[from..];
        let mut taken = fresh.iter().rposition(|b| *b == b'\n').map_or(0, |at| at + 1);
        // A line past the cap is cut there, so none is held for ever.
        if taken == 0 && fresh.len() >= OUTPUT_LINE_BYTES_MAX {
            taken = OUTPUT_LINE_BYTES_MAX;
        }
        let lines = fresh[..taken]
            .split_inclusive(|b| *b == b'\n')
            .flat_map(|line| line.strip_suffix(b"\n").unwrap_or(line).chunks(OUTPUT_LINE_BYTES_MAX))
            .map(text)
            .collect();
        self.tail = next[..from + taken].to_vec();
        (lines, missed)
    }

    /// The line not yet ended in `next`, the run's last read: the rest of
    /// the stream.
    fn rest(&mut self, next: &[u8]) -> Option<String> {
        let (from, _) = new_since(&self.tail, next);
        self.tail = next.to_vec();
        (from < next.len()).then(|| text(&next[from..]))
    }
}

/// A line of output as text, without its carriage return.
fn text(line: &[u8]) -> String {
    String::from_utf8_lossy(line.strip_suffix(b"\r").unwrap_or(line)).into_owned()
}

/// Where the output new since `last` begins in `next`, two reads of one
/// stream's tail, and whether output was missed between them. While the
/// stream is shorter than the tail, `next` begins as `last` does. Past it,
/// `last`'s end is found in `next`: the latest place before which `next`
/// holds the last of `last`, matched by `last`'s final bytes first. The
/// engine decodes its tail lossily, so a tail that begins in a character
/// begins with a replacement for it, which the match skips.
fn new_since(last: &[u8], next: &[u8]) -> (usize, bool) {
    if next.starts_with(last) {
        return (last.len(), false);
    }
    const REPLACEMENT: &[u8] = "\u{FFFD}".as_bytes();
    let anchor = &last[last.len().saturating_sub(OUTPUT_ANCHOR_BYTES)..];
    // Bounded by `next`'s length, one step back each pass.
    let mut end = next.len();
    while end >= anchor.len() && !anchor.is_empty() {
        if next[end - anchor.len()..end] == *anchor {
            let held = &next[..end];
            let held = held.strip_prefix(REPLACEMENT).unwrap_or(held);
            if last.ends_with(held) {
                return (end, false);
            }
        }
        end -= 1;
    }
    (0, true)
}

/// fork: what one read of a run's output does with it.
#[derive(Clone, Copy, PartialEq)]
enum Read {
    /// Takes it as already shown (a container adopted from an earlier
    /// owner, whose output that owner showed).
    Unshown,
    /// Shows the lines it ends.
    Shown,
    /// The run's last: shows the rest too.
    Last,
}

/// One read of a run's output from the engine: whether the container is
/// still there to read again. A read that failed is tried at the next.
async fn read_output(api: &Docker, name: &str, output: &Mutex<Output>, read: Read) -> bool {
    let reply = match api.call("GET", &format!("/v1/containers/{name}/logs"), None).await {
        Ok(reply) => reply,
        Err(_) => return true,
    };
    if reply.status.as_u16() == 404 {
        return false;
    }
    if !reply.status.is_success() {
        return true;
    }
    let Ok(logs) = reply.json() else {
        return true;
    };
    let stream = |name: &str| logs.get(name).and_then(Value::as_str).unwrap_or("").to_string();
    output
        .lock()
        .unwrap()
        .advance(&stream("stdout"), &stream("stderr"), read != Read::Unshown, read == Read::Last);
    true
}

/// fork: the images `celld dev` built into Docker's store (each under the
/// content reference a node starts it by: `deploy::content_reference`),
/// loaded into the krun engine at `socket` when it holds none by that
/// reference. A dev deploy writes no image tar for its node to load
/// (`deploy::Options::local_images`), and the engine never reads Docker's
/// store, so without this a dev node on krun has no image to start. Each
/// `docker save` is piped to the engine as it is written, never held whole:
/// an agent's image is several GB. Answers the references it loaded.
pub(crate) async fn load_dev_images(socket: &Path, images: &[String]) -> anyhow::Result<Vec<String>> {
    let api = Docker::new(socket);
    let held = api
        .expect("GET", "/v1/images", None, "list the krun engine's images")
        .await?
        .json()?;
    let held: Vec<&str> = held
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|i| i.get("reference").and_then(Value::as_str))
        .collect();
    let mut loaded = vec![];
    // Bounded: the deployment's images.
    for image in images {
        if held.contains(&image.as_str()) {
            continue;
        }
        let mut save = tokio::process::Command::new(crate::deploy::engine_cli())
            .args(["save", image])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .context("save container image: is a Docker or Podman CLI on PATH?")?;
        let tar = save.stdout.take().context("docker save's output")?;
        let reference =
            percent_encoding::utf8_percent_encode(image, percent_encoding::NON_ALPHANUMERIC);
        let load = api
            .post_stream(&format!("/v1/images/load?reference={reference}"), tar, "load image")
            .await;
        let saved = save.wait_with_output().await.context("save container image")?;
        if !saved.status.success() {
            return Err(anyhow!(
                "save container image {image} failed:\n{}",
                String::from_utf8_lossy(&saved.stderr).trim_end()
            ));
        }
        load.with_context(|| format!("load {image} into the krun engine"))?;
        loaded.push(image.clone());
    }
    Ok(loaded)
}

fn intercepts_json(intercepts: &[InterceptRule]) -> Value {
    json!(intercepts
        .iter()
        .map(|i| json!({ "scheme": i.scheme, "target": i.target, "action": { "kind": "handler" } }))
        .collect::<Vec<_>>())
}

/// One frame of the exec stream, or `None` at its end.
async fn read_frame(
    read: &mut tokio::io::ReadHalf<Box<dyn Stream>>,
) -> anyhow::Result<Option<(u8, Vec<u8>)>> {
    let mut header = [0u8; 8];
    match read.read_exact(&mut header).await {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e).context("exec stream"),
    }
    let length = u32::from_be_bytes([header[4], header[5], header[6], header[7]]) as usize;
    anyhow::ensure!(
        header[1..4] == [0, 0, 0] && length <= frames::PAYLOAD_BYTES_MAX,
        "exec: a malformed frame"
    );
    let mut payload = vec![0u8; length];
    read.read_exact(&mut payload).await.context("exec stream")?;
    Ok(Some((header[0], payload)))
}

/// The exec's output to the readers, until its exit: the code (128 + n
/// for a signal).
async fn pump(
    mut read: tokio::io::ReadHalf<Box<dyn Stream>>,
    stdout: mpsc::Sender<Bytes>,
    stderr: mpsc::Sender<Bytes>,
) -> Result<i64, String> {
    // Bounded by the engine's stream, which ends with the exit frame.
    loop {
        let (stream, payload) = match read_frame(&mut read).await {
            Ok(Some(frame)) => frame,
            Ok(None) => return Err("exec: the stream ended before the exit".into()),
            Err(e) => return Err(format!("{e:#}")),
        };
        match stream {
            // A reader that went away: keep draining, so the engine is not
            // held on a full stream.
            frames::STDOUT => {
                let _ = stdout.send(Bytes::from(payload)).await;
            }
            frames::STDERR => {
                let _ = stderr.send(Bytes::from(payload)).await;
            }
            frames::EXITED => {
                let v: Value = serde_json::from_slice(&payload).unwrap_or(Value::Null);
                let code = v.get("code").and_then(Value::as_i64);
                let signal = v.get("signal").and_then(Value::as_i64);
                return match (code, signal) {
                    (Some(code), _) => Ok(code),
                    (None, Some(signal)) => Ok(128 + signal),
                    _ => Err("exec: an exit without a code".into()),
                };
            }
            frames::ERROR => return Err(format!("exec: {}", String::from_utf8_lossy(&payload))),
            other => return Err(format!("exec: frame {other} from the engine")),
        }
    }
}

struct KrunExec {
    write: tokio::sync::Mutex<Option<tokio::io::WriteHalf<Box<dyn Stream>>>>,
    stdin_open: Mutex<bool>,
    exit: watch::Sender<Option<Result<i64, String>>>,
}

impl KrunExec {
    async fn send(&self, stream: u8, payload: &[u8]) -> Result<(), String> {
        let mut guard = self.write.lock().await;
        let w = guard.as_mut().ok_or("the process has ended")?;
        w.write_all(&frames::encode(stream, payload))
            .await
            .map_err(|e| format!("exec stream: {e}"))?;
        w.flush().await.map_err(|e| format!("exec stream: {e}"))
    }
}

#[async_trait::async_trait]
impl ExecIo for KrunExec {
    async fn write(&self, bytes: &[u8]) -> Result<(), String> {
        if !*self.stdin_open.lock().unwrap() {
            return Err("stdin is closed".into());
        }
        for chunk in bytes.chunks(frames::PAYLOAD_BYTES_MAX) {
            if !chunk.is_empty() {
                self.send(frames::STDIN, chunk).await?;
            }
        }
        Ok(())
    }

    async fn close_stdin(&self) {
        let was_open = std::mem::replace(&mut *self.stdin_open.lock().unwrap(), false);
        if was_open {
            let _ = self.send(frames::STDIN, b"").await;
        }
    }

    async fn wait(&self) -> anyhow::Result<i64> {
        let mut exit = self.exit.subscribe();
        // Bounded by the pump, which sets the exit when the stream ends.
        loop {
            if let Some(result) = exit.borrow_and_update().clone() {
                return result.map_err(|e| anyhow!(e));
            }
            if exit.changed().await.is_err() {
                return Err(anyhow!("exec: the stream went away"));
            }
        }
    }

    async fn kill(&self, signal: u32) -> anyhow::Result<()> {
        self.send(frames::SIGNAL, &serde_json::to_vec(&json!({ "signal": signal }))?)
            .await
            .map_err(|e| anyhow!(e))
    }

    async fn resize(&self, cols: u16, rows: u16) -> anyhow::Result<()> {
        self.send(frames::RESIZE, &serde_json::to_vec(&json!({ "cols": cols, "rows": rows }))?)
            .await
            .map_err(|e| anyhow!(e))
    }
}

/// A loopback listener per port of one container, made on first use; each
/// connection takes a socket from the engine's `ports.sock` and is spliced
/// to it.
struct Forwarder {
    name: String,
    ports: PathBuf,
    listeners: Mutex<HashMap<u16, (u16, asyncrt::TaskHandle<()>)>>,
}

impl Forwarder {
    fn new(name: &str, ports: &Path) -> Forwarder {
        Forwarder {
            name: name.to_string(),
            ports: ports.to_path_buf(),
            listeners: Mutex::new(HashMap::new()),
        }
    }
}

impl PortForwarder for Forwarder {
    fn address(&self, port: u16) -> Result<String, String> {
        let mut listeners = self.listeners.lock().unwrap();
        if let Some((local, _)) = listeners.get(&port) {
            return Ok(format!("127.0.0.1:{local}"));
        }
        let listener = std::net::TcpListener::bind("127.0.0.1:0")
            .and_then(|l| l.set_nonblocking(true).map(|()| l))
            .map_err(|e| format!("a listener for port {port}: {e}"))?;
        let local = listener
            .local_addr()
            .map_err(|e| format!("a listener for port {port}: {e}"))?
            .port();
        let (name, ports) = (self.name.clone(), self.ports.clone());
        let task = asyncrt::spawn(async move {
            let Ok(listener) = tokio::net::TcpListener::from_std(listener) else {
                return;
            };
            // Unbounded by design: the container's life; `close` aborts it.
            loop {
                let Ok((mut client, _)) = listener.accept().await else {
                    continue;
                };
                let (name, ports) = (name.clone(), ports.clone());
                tokio::spawn(async move {
                    // A port nothing listens on yet: the connection closes,
                    // which `fetch` reports as not listening, and the
                    // containers library retries.
                    if let Ok(mut guest) = port_connect(&ports, &name, port).await {
                        let _ = client.set_nodelay(true);
                        let _ = tokio::io::copy_bidirectional(&mut client, &mut guest).await;
                    }
                });
            }
        });
        listeners.insert(port, (local, task));
        Ok(format!("127.0.0.1:{local}"))
    }

    fn serves(&self, port: u16) -> bool {
        self.listeners
            .lock()
            .unwrap()
            .values()
            .any(|(local, _)| *local == port)
    }

    fn close(&self) {
        for (_, (_, task)) in self.listeners.lock().unwrap().drain() {
            task.abort();
        }
    }
}

impl Drop for Forwarder {
    fn drop(&mut self) {
        self.close();
    }
}

/// A connection to `port` in container `name`, as `ports.sock` hands it over.
async fn port_connect(ports: &Path, name: &str, port: u16) -> anyhow::Result<Box<dyn Stream>> {
    let mut s = tokio::net::UnixStream::connect(ports)
        .await
        .with_context(|| format!("the engine's ports socket at {}", ports.display()))?;
    let mut line = serde_json::to_vec(&json!({ "name": name, "port": port }))?;
    line.push(b'\n');
    s.write_all(&line).await?;
    let mut buf = vec![0u8; PORT_REPLY_BYTES_MAX];
    let (n, fd) = s
        .async_io(Interest::READABLE, || recv_with_fd(s.as_raw_fd(), &mut buf))
        .await?;
    let reply: Value = serde_json::from_slice(&buf[..n]).context("the ports socket's reply")?;
    if reply.get("ok").and_then(Value::as_bool) != Some(true) {
        return Err(anyhow!(
            "port {port}: {}",
            reply.get("error").and_then(Value::as_str).unwrap_or("refused")
        ));
    }
    let fd = fd.ok_or_else(|| anyhow!("port {port}: no socket in the reply"))?;
    let stream: Box<dyn Stream> = match reply.get("transport").and_then(Value::as_str) {
        Some("nic") => {
            let s = std::net::TcpStream::from(fd);
            s.set_nonblocking(true)?;
            Box::new(tokio::net::TcpStream::from_std(s)?)
        }
        Some("vsock") => {
            let s = std::os::unix::net::UnixStream::from(fd);
            s.set_nonblocking(true)?;
            Box::new(tokio::net::UnixStream::from_std(s)?)
        }
        other => return Err(anyhow!("port {port}: transport {other:?}")),
    };
    Ok(stream)
}

/// One message and, if one came with it, a descriptor (SCM_RIGHTS).
fn recv_with_fd(sock: RawFd, buf: &mut [u8]) -> std::io::Result<(usize, Option<OwnedFd>)> {
    let mut iov = libc::iovec {
        iov_base: buf.as_mut_ptr() as *mut libc::c_void,
        iov_len: buf.len(),
    };
    let mut control = [0u64; 4];
    // SAFETY: msghdr is plain data; all zeroes is a valid value.
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = control.as_mut_ptr() as *mut libc::c_void;
    msg.msg_controllen = std::mem::size_of_val(&control) as _;
    #[cfg(target_os = "linux")]
    let flags = libc::MSG_CMSG_CLOEXEC;
    #[cfg(not(target_os = "linux"))]
    let flags = 0;
    // SAFETY: `msg` points at `iov` and `control`, both live for the call.
    let n = unsafe { libc::recvmsg(sock, &mut msg, flags) };
    if n < 0 {
        return Err(std::io::Error::last_os_error());
    }
    let mut fd = None;
    // SAFETY: the kernel filled `control` up to msg_controllen; the CMSG
    // macros walk only that.
    unsafe {
        let mut c = libc::CMSG_FIRSTHDR(&msg);
        while !c.is_null() {
            if (*c).cmsg_level == libc::SOL_SOCKET && (*c).cmsg_type == libc::SCM_RIGHTS {
                let raw = std::ptr::read_unaligned(libc::CMSG_DATA(c) as *const RawFd);
                let owned = OwnedFd::from_raw_fd(raw);
                if fd.is_none() {
                    fd = Some(owned);
                }
            }
            c = libc::CMSG_NXTHDR(&msg, c);
        }
    }
    if msg.msg_flags & libc::MSG_CTRUNC != 0 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "a truncated control message",
        ));
    }
    Ok((n as usize, fd))
}

/// `Send`, not `Sync`: a service response streams from a `Send` source.
pub type InterceptBody = http_body_util::combinators::UnsyncBoxBody<Bytes, std::io::Error>;

/// fork: how an intercepted WebSocket upgrade is answered: given the
/// engine's request (its connection upgradable) and the service call to the
/// intercept's binding, the response to send, a 101 whose socket is pumped
/// once the engine hands its connection over. The node's own WebSocket
/// machinery answers it as it answers a client's upgrade, and it lives in
/// the binary (`main/websocket.rs`), so the binary sets it at start
/// (`answer_intercepted_websockets`). On Cloudflare a container's
/// WebSocket reaches the intercept's binding as any request does.
pub type InterceptedWebSocket = Arc<
    dyn Fn(
            hyper::Request<hyper::body::Incoming>,
            crate::engine_api::ServiceFetch,
        ) -> futures_util::future::BoxFuture<'static, hyper::Response<InterceptBody>>
        + Send
        + Sync,
>;

static INTERCEPTED_WEBSOCKET: std::sync::OnceLock<InterceptedWebSocket> = std::sync::OnceLock::new();

/// fork: sets how intercepted WebSocket upgrades are answered, once.
pub fn answer_intercepted_websockets(answer: InterceptedWebSocket) {
    let _ = INTERCEPTED_WEBSOCKET.set(answer);
}

/// celld's intercept socket: the engine's egress proxy sends each
/// intercepted request here, naming the container and the intercept.
fn serve_intercepts(
    path: &Path,
    runtime: Option<crate::runtime::RuntimeManager>,
) -> anyhow::Result<()> {
    let _ = asyncrt::fs().remove_file(path);
    let listener = std::os::unix::net::UnixListener::bind(path)
        .with_context(|| format!("the intercept socket at {}", path.display()))?;
    listener.set_nonblocking(true)?;
    // Only celld's own user and root (the engine) may connect.
    let c = std::ffi::CString::new(path.as_os_str().as_encoded_bytes())?;
    // SAFETY: chmod(2) on a NUL-terminated path.
    unsafe { libc::chmod(c.as_ptr(), 0o600) };
    tokio::spawn(async move {
        let Ok(listener) = tokio::net::UnixListener::from_std(listener) else {
            return;
        };
        // Unbounded by design: the process's life.
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                continue;
            };
            let runtime = runtime.clone();
            tokio::spawn(async move {
                let service = hyper::service::service_fn(move |req| {
                    let runtime = runtime.clone();
                    async move { Ok::<_, std::convert::Infallible>(intercepted(runtime, req).await) }
                });
                // with upgrades: a WebSocket's connection is handed over
                // on its 101 (`answer_intercepted_websockets`)
                let _ = hyper::server::conn::http1::Builder::new()
                    .serve_connection(hyper_util::rt::TokioIo::new(stream), service)
                    .with_upgrades()
                    .await;
            });
        }
    });
    Ok(())
}

/// fork: one intercepted request's start and end in the node's log, under
/// an id of its own: `container_intercept` as it reaches the node, then
/// `container_intercept_answered` with its answer's status, or
/// `container_intercept_dropped` when the guest's request goes away before
/// the binding answers (its connection closed). Its path, never its query.
struct InterceptLog {
    id: u64,
    started_us: u64,
    ended: bool,
}

static INTERCEPT_IDS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

impl InterceptLog {
    fn start(
        cell: &str,
        run: u64,
        index: usize,
        route: &crate::container::ServiceRoute,
        method: &str,
        host: &str,
        path: &str,
    ) -> InterceptLog {
        let id = INTERCEPT_IDS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        tracing::info!(
            event = "container_intercept",
            intercept = id,
            cell,
            run,
            index,
            binding = %route.script,
            entrypoint = route.entrypoint.as_deref().unwrap_or("default"),
            method,
            host,
            path,
            "a container's request reached its intercept"
        );
        InterceptLog {
            id,
            started_us: asyncrt::mono_us(),
            ended: false,
        }
    }

    fn ms(&self) -> u64 {
        asyncrt::mono_us().saturating_sub(self.started_us) / 1000
    }

    fn answered(&mut self, status: u16) {
        self.ended = true;
        tracing::info!(
            event = "container_intercept_answered",
            intercept = self.id,
            status,
            ms = self.ms(),
            "the intercept's binding answered"
        );
    }

    fn failed(&mut self, error: &anyhow::Error) {
        self.ended = true;
        tracing::warn!(
            event = "container_intercept_answered",
            intercept = self.id,
            status = 502,
            ms = self.ms(),
            error = %format!("{error:#}"),
            "the intercept's binding failed"
        );
    }
}

impl Drop for InterceptLog {
    fn drop(&mut self) {
        if !self.ended {
            tracing::warn!(
                event = "container_intercept_dropped",
                intercept = self.id,
                ms = self.ms(),
                "the container's request went away before its binding answered"
            );
        }
    }
}

fn plain(status: u16, text: String) -> hyper::Response<InterceptBody> {
    use http_body_util::BodyExt;
    let mut r = hyper::Response::new(
        http_body_util::Full::new(Bytes::from(text))
            .map_err(|never| match never {})
            .boxed_unsync(),
    );
    *r.status_mut() = hyper::StatusCode::from_u16(status).unwrap_or(hyper::StatusCode::BAD_GATEWAY);
    r
}

/// One intercepted request, to the binding the object passed for it.
async fn intercepted(
    runtime: Option<crate::runtime::RuntimeManager>,
    req: hyper::Request<hyper::body::Incoming>,
) -> hyper::Response<InterceptBody> {
    use futures_util::StreamExt;
    use http_body_util::BodyExt;
    let header = |name: &str| {
        req.headers()
            .get(name)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string)
    };
    let (Some(container), Some(index)) = (
        header("x-sandcastle-container"),
        header("x-sandcastle-intercept").and_then(|i| i.parse::<usize>().ok()),
    ) else {
        return plain(400, "an intercepted request names its container and intercept".into());
    };
    let Some(runtime) = runtime else {
        return plain(502, "no Worker runtime on this node".into());
    };
    let Some((route, cell, run)) =
        crate::container::engine_if_ready().and_then(|e| e.intercept_route(&container, index))
    else {
        return plain(502, format!("no intercept {index} for {container}"));
    };
    let scheme = header("x-sandcastle-scheme").unwrap_or_else(|| "http".into());
    let host = header("host")
        .or_else(|| header("x-sandcastle-host"))
        .unwrap_or_default();
    let path = req
        .uri()
        .path_and_query()
        .map(|p| p.as_str().to_string())
        .unwrap_or_else(|| "/".into());
    let url = format!("{scheme}://{host}{path}");
    let method = req.method().as_str().to_string();
    // fork: the request's start and its end (its answer's head, or the
    // request going away before it) are logged under its cell, run and
    // binding: a guest's request the binding holds is otherwise invisible.
    let mut log = InterceptLog::start(&cell, run, index, &route, &method, &host, req.uri().path());
    let headers: Vec<(String, String)> = req
        .headers()
        .iter()
        .filter(|(name, _)| !name.as_str().starts_with("x-sandcastle-"))
        .map(|(name, value)| {
            (
                name.as_str().to_string(),
                String::from_utf8_lossy(value.as_bytes()).into_owned(),
            )
        })
        .collect();
    let entrypoint = route.entrypoint.map(|name| crate::WorkerFetchEntrypoint {
        name,
        props: route.props,
    });
    // fork: a WebSocket's upgrade goes to the binding as the node answers a
    // client's (`InterceptedWebSocket`); its request has no body.
    if crate::cell_dispatch::requests_websocket_upgrade(&headers) {
        let Some(answer) = INTERCEPTED_WEBSOCKET.get() else {
            return plain(502, "this node answers no intercepted WebSocket".into());
        };
        let call = crate::engine_api::ServiceFetch {
            generation: route.generation,
            script: route.script,
            entrypoint,
            url,
            method,
            body: crate::engine_api::RequestBody::Bytes(Bytes::new()),
            headers,
            cancel: None,
        };
        let response = answer(req, call).await;
        log.answered(response.status().as_u16());
        return response;
    }
    let declared = req
        .headers()
        .get(hyper::header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok());
    let bodyless =
        matches!(*req.method(), hyper::Method::GET | hyper::Method::HEAD) || declared == Some(0);
    let body = if bodyless || declared.is_some_and(|length| length < INTERCEPT_WHOLE_BYTES_MAX) {
        match http_body_util::Limited::new(req.into_body(), INTERCEPT_WHOLE_BYTES_MAX as usize)
            .collect()
            .await
        {
            Ok(b) => crate::engine_api::RequestBody::Bytes(b.to_bytes()),
            Err(_) => {
                log.answered(413);
                return plain(413, "an intercepted request's body is longer than it said".into());
            }
        }
    } else {
        let max = match crate::env_vars::positive_or(
            "CELLD_MAX_REQUEST_BODY_BYTES",
            crate::actor::DEFAULT_MAX_REQUEST_BODY_BYTES,
        ) {
            Ok(max) => max,
            Err(e) => {
                log.answered(502);
                return plain(502, format!("{e:#}"));
            }
        };
        let chunks = http_body_util::Limited::new(req.into_body(), max)
            .into_data_stream()
            .map(move |chunk| {
                chunk.map(|bytes| bytes.to_vec()).map_err(|error| {
                    if error.downcast_ref::<http_body_util::LengthLimitError>().is_some() {
                        format!("an intercepted request's body is at most {max} bytes")
                    } else {
                        error.to_string()
                    }
                })
            });
        match crate::http_streams::register_body_stream(Box::pin(chunks)) {
            Ok(id) => crate::engine_api::RequestBody::Stream(id),
            Err(e) => {
                log.answered(502);
                return plain(502, e);
            }
        }
    };
    let call = crate::engine_api::ServiceFetch {
        generation: route.generation,
        script: route.script,
        entrypoint,
        url,
        method,
        body,
        headers,
        cancel: None,
    };
    let response = match runtime.fetch_service(call).await {
        Ok(r) => r,
        Err(e) => {
            log.failed(&e);
            return plain(502, format!("the intercept's binding: {e:#}"));
        }
    };
    log.answered(response.status);
    let body: InterceptBody = match response.stream {
        Some(stream) => BodyExt::boxed_unsync(http_body_util::StreamBody::new(stream.map(|chunk| {
            chunk
                .map(|bytes| hyper::body::Frame::data(Bytes::from(bytes)))
                .map_err(std::io::Error::other)
        }))),
        None => http_body_util::Full::new(Bytes::from(response.body))
            .map_err(|never| match never {})
            .boxed_unsync(),
    };
    let mut r = hyper::Response::new(body);
    *r.status_mut() =
        hyper::StatusCode::from_u16(response.status).unwrap_or(hyper::StatusCode::BAD_GATEWAY);
    for (name, value) in response.headers {
        if let (Ok(n), Ok(v)) = (
            hyper::header::HeaderName::from_bytes(name.as_bytes()),
            hyper::header::HeaderValue::from_str(&value),
        ) {
            r.headers_mut().append(n, v);
        }
    }
    r
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::container::OutputMode;

    #[test]
    fn frames_have_docker_headers() {
        assert_eq!(frames::encode(frames::STDIN, b"ab"), vec![0, 0, 0, 0, 0, 0, 0, 2, b'a', b'b']);
        assert_eq!(frames::encode(frames::RESIZE, b""), vec![6, 0, 0, 0, 0, 0, 0, 0]);
    }

    // Goal: the mode names the engine takes are the ones celld sends.
    #[test]
    fn output_modes_as_the_engine_names_them() {
        assert_eq!(json!(OutputMode::Combined), json!("combined"));
        assert_eq!(json!(OutputMode::Ignore), json!("ignore"));
        assert_eq!(json!(OutputMode::Pipe), json!("pipe"));
    }

    /// Goal: the intercept socket is beside the node's data when that path
    /// fits a unix socket's; past it, in the runtime directory, by a hash of
    /// the data directory (two deep ones never share one), and with no
    /// runtime directory it is refused, saying why. Replay: the same answer.
    #[test]
    fn the_intercept_socket_fits_sun_path() {
        let run = Path::new("/run/user/1000");
        let short = Path::new("/var/lib/celld");
        for _ in 0..2 {
            let beside = intercept_socket(short, "node-1", Some(run)).unwrap();
            assert!(beside.starts_with(short), "{}", beside.display());
            let deep = PathBuf::from(format!("/home/{}/.celld/dev/runtime", "d".repeat(90)));
            let other = PathBuf::from(format!("/home/{}/.celld/dev/runtime", "e".repeat(90)));
            let moved = intercept_socket(&deep, "node-1", Some(run)).unwrap();
            assert!(moved.starts_with("/run/user/1000/celld"), "{}", moved.display());
            assert!(moved.to_string_lossy().ends_with(".sock"), "{}", moved.display());
            assert!(moved.as_os_str().len() <= SOCKET_PATH_BYTES_MAX);
            assert_ne!(moved, intercept_socket(&other, "node-1", Some(run)).unwrap());
            let e = intercept_socket(&deep, "node-1", None).unwrap_err().to_string();
            assert!(e.contains("XDG_RUNTIME_DIR") && e.contains("107 bytes"), "{e}");
            assert!(intercept_socket(&deep, "node-1", Some(Path::new("relative"))).is_err());
        }
    }

    /// The engine's tail of a stream that has `end` bytes so far.
    fn engine_tail(stream: &[u8], end: usize) -> String {
        String::from_utf8_lossy(&stream[end.saturating_sub(OUTPUT_TAIL_BYTES)..end]).into_owned()
    }

    /// Goal: reads of a growing stream's tail show each line once, in
    /// order, whole, past the engine's 64 KiB as before it, with lines
    /// whose tails begin or end inside a character; the run's last read
    /// shows the line not yet ended. Replay: a read with nothing new shows
    /// nothing.
    #[test]
    fn output_reads_show_each_line_once() {
        let mut stream = Vec::new();
        for i in 0..6000 {
            stream.extend_from_slice(format!("{{\"at\":{i},\"event\":\"é{}\"}}\n", "x".repeat(i % 37)).as_bytes());
        }
        stream.extend_from_slice("no newline yet é".as_bytes());
        let mut cursor = OutputCursor::default();
        let mut shown = vec![];
        // reads at uneven points, some inside a two-byte character
        let mut end = 0;
        let mut step = 1;
        while end < stream.len() {
            end = (end + step).min(stream.len());
            step = (step * 7 + 13) % 9000 + 1;
            let tail = engine_tail(&stream, end);
            let (lines, missed) = cursor.advance(tail.as_bytes());
            assert!(!missed, "nothing is missed at {end}");
            shown.extend(lines);
            let (again, missed) = cursor.advance(tail.as_bytes());
            assert!(again.is_empty() && !missed, "a read with nothing new shows nothing");
        }
        let tail = engine_tail(&stream, stream.len());
        shown.extend(cursor.rest(tail.as_bytes()));
        let want: Vec<String> = String::from_utf8(stream).unwrap().split('\n').map(str::to_string).collect();
        assert_eq!(shown.len(), want.len());
        assert_eq!(shown, want);
    }

    /// Goal: more than the engine keeps between two reads is said to be
    /// missed, and the newer tail's lines are shown; a first read that is a
    /// whole tail may have missed some; a line past the cap is cut there.
    #[test]
    fn output_past_the_tail_is_missed_and_said() {
        let line = |i: usize| format!("line {i:08}\n");
        let stream: String = (0..20_000).map(line).collect();
        let mut cursor = OutputCursor::default();
        let (lines, missed) = cursor.advance(engine_tail(stream.as_bytes(), 1000).as_bytes());
        assert!(!missed && lines.len() == 1000 / 14 && lines[0] == "line 00000000");
        let (lines, missed) = cursor.advance(engine_tail(stream.as_bytes(), 200_000).as_bytes());
        assert!(missed, "past 64 KiB between reads");
        assert_eq!(lines.last().map(String::as_str), Some("line 00014284"));
        let mut first = OutputCursor::default();
        assert!(first.advance(engine_tail(stream.as_bytes(), 100_000).as_bytes()).1, "a first read of a whole tail");
        let mut long = OutputCursor::default();
        let one = "y".repeat(OUTPUT_LINE_BYTES_MAX + 10);
        let (lines, _) = long.advance(one.as_bytes());
        assert_eq!(lines, vec!["y".repeat(OUTPUT_LINE_BYTES_MAX)]);
        assert_eq!(long.rest(one.as_bytes()), Some("y".repeat(10)));
    }

    #[test]
    fn intercepts_as_the_engine_takes_them() {
        let rules = [InterceptRule {
            scheme: "https",
            target: "api.example.com".into(),
        }];
        assert_eq!(
            intercepts_json(&rules),
            json!([{ "scheme": "https", "target": "api.example.com", "action": { "kind": "handler" } }])
        );
    }
}
