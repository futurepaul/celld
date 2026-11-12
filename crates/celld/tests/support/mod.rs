//! What the tests of the `celld dev` binary share: a project on disk and
//! one running `celld dev` over it.

// Each test binary uses a different part of this module.
#![allow(dead_code)]

use std::io::{Read as _, Write as _};
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// A Wrangler project of one script and its Durable Object classes.
pub fn project(dir: &Path, worker: &str, classes: &[(&str, &str)]) {
    let bindings = classes
        .iter()
        .map(|(binding, class)| format!(r#"{{ "name": "{binding}", "class_name": "{class}" }}"#))
        .collect::<Vec<_>>()
        .join(", ");
    let names = classes
        .iter()
        .map(|(_, class)| format!(r#""{class}""#))
        .collect::<Vec<_>>()
        .join(", ");
    std::fs::write(
        dir.join("wrangler.jsonc"),
        format!(
            r#"{{
  "name": "test",
  "main": "index.js",
  "compatibility_date": "2026-01-01",
  "no_bundle": true,
  "durable_objects": {{ "bindings": [{bindings}] }},
  "migrations": [{{ "tag": "v1", "new_sqlite_classes": [{names}] }}]
}}"#
        ),
    )
    .unwrap();
    std::fs::write(dir.join("index.js"), worker).unwrap();
}

/// One `celld dev`, stopped by its PID when dropped.
pub struct Dev {
    child: Child,
    pub port: u16,
    output: Arc<Mutex<String>>,
}

impl Dev {
    pub fn start(project: &Path) -> Dev {
        Dev::start_with(project, &[])
    }

    /// One `celld dev` with `env` set beside the defaults.
    pub fn start_with(project: &Path, env: &[(&str, &str)]) -> Dev {
        let port = free_port();
        let mut child = Command::new(env!("CARGO_BIN_EXE_celld"))
            .arg("dev")
            .arg(project)
            .args(["--port", &port.to_string(), "--no-watch", "--logs"])
            .env("NO_COLOR", "1")
            .env("RUST_LOG", "info")
            .envs(env.iter().copied())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let output = Arc::new(Mutex::new(String::new()));
        for mut stream in [
            Box::new(child.stdout.take().unwrap()) as Box<dyn std::io::Read + Send>,
            Box::new(child.stderr.take().unwrap()),
        ] {
            let output = output.clone();
            std::thread::spawn(move || {
                let mut chunk = [0u8; 4096];
                while let Ok(read) = stream.read(&mut chunk) {
                    if read == 0 {
                        break;
                    }
                    output
                        .lock()
                        .unwrap()
                        .push_str(&String::from_utf8_lossy(&chunk[..read]));
                }
            });
        }
        let mut dev = Dev {
            child,
            port,
            output,
        };
        let deadline = Instant::now() + Duration::from_secs(90);
        loop {
            if let Ok((200, _)) = dev.get("/.well-known/celld/health") {
                return dev;
            }
            if let Some(status) = dev.child.try_wait().unwrap() {
                panic!("celld dev exited with {status}:\n{}", dev.output());
            }
            assert!(
                Instant::now() < deadline,
                "celld dev did not become ready:\n{}",
                dev.output()
            );
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    /// Whether the process has exited, and how.
    pub fn exited(&mut self) -> Option<std::process::ExitStatus> {
        self.child.try_wait().unwrap()
    }

    pub fn output(&self) -> String {
        self.output.lock().unwrap().clone()
    }

    /// HTTP/1.0, so the body is the bytes up to the close.
    pub fn get(&self, path: &str) -> std::io::Result<(u16, String)> {
        let mut stream = TcpStream::connect(("127.0.0.1", self.port))?;
        stream.set_read_timeout(Some(Duration::from_secs(60)))?;
        write!(stream, "GET {path} HTTP/1.0\r\nhost: 127.0.0.1\r\n\r\n")?;
        let mut response = String::new();
        stream.read_to_string(&mut response)?;
        let status = response
            .split(' ')
            .nth(1)
            .and_then(|status| status.parse().ok())
            .unwrap_or(0);
        let body = response
            .split_once("\r\n\r\n")
            .map_or("", |(_, body)| body)
            .to_string();
        Ok((status, body))
    }

    /// A 200's JSON body, and how long it took.
    pub fn json(&self, path: &str) -> (serde_json::Value, Duration) {
        let started = Instant::now();
        let (status, body) = self
            .get(path)
            .unwrap_or_else(|error| panic!("GET {path}: {error}\n{}", self.output()));
        let took = started.elapsed();
        assert_eq!(status, 200, "GET {path}: {body}\n{}", self.output());
        let value = serde_json::from_str(&body).unwrap_or_else(|error| panic!("{error}: {body}"));
        (value, took)
    }
}

impl Drop for Dev {
    fn drop(&mut self) {
        // SIGTERM lets the supervisor stop its node; the node also dies with
        // the supervisor (PR_SET_PDEATHSIG), so the kill below is a backstop.
        let pid = self.child.id() as libc::pid_t;
        unsafe {
            libc::kill(pid, libc::SIGTERM);
        }
        let deadline = Instant::now() + Duration::from_secs(30);
        while Instant::now() < deadline {
            if let Ok(Some(_)) = self.child.try_wait() {
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

