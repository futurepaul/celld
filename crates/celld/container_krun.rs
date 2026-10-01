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

/// An intercepted request's body, at most: it is held whole for the
/// service call.
const INTERCEPT_BODY_BYTES_MAX: usize = 32 << 20;
/// A `ports.sock` reply, at most.
const PORT_REPLY_BYTES_MAX: usize = 4096;

pub(crate) struct KrunBackend {
    api: Docker,
    ports: PathBuf,
    /// `unix:<path>` of celld's intercept socket, for the engine.
    handler: String,
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
        let intercepts = data_dir.join(format!("intercepts-{}.sock", &node_prefix(node)[6..12]));
        serve_intercepts(&intercepts, runtime)?;
        Ok(KrunBackend {
            api,
            ports,
            handler: format!("unix:{}", intercepts.display()),
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
type InterceptBody = http_body_util::combinators::UnsyncBoxBody<Bytes, std::io::Error>;

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
                let _ = hyper::server::conn::http1::Builder::new()
                    .serve_connection(hyper_util::rt::TokioIo::new(stream), service)
                    .await;
            });
        }
    });
    Ok(())
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
    let Some(route) =
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
    let body = match http_body_util::Limited::new(req.into_body(), INTERCEPT_BODY_BYTES_MAX)
        .collect()
        .await
    {
        Ok(b) => b.to_bytes(),
        Err(_) => {
            return plain(
                413,
                format!("an intercepted request's body is at most {INTERCEPT_BODY_BYTES_MAX} bytes"),
            )
        }
    };
    let call = crate::engine_api::ServiceFetch {
        generation: route.generation,
        script: route.script,
        entrypoint: route.entrypoint.map(|name| crate::WorkerFetchEntrypoint {
            name,
            props: route.props,
        }),
        url,
        method,
        body: crate::engine_api::RequestBody::Bytes(body),
        headers,
        cancel: None,
    };
    let response = match runtime.fetch_service(call).await {
        Ok(r) => r,
        Err(e) => return plain(502, format!("the intercept's binding: {e:#}")),
    };
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
