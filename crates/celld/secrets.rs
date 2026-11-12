//! fork: a node's secrets backend, which a Worker's `secrets_store_secrets`
//! bindings read (docs/services/secrets-store.md). Cloudflare binds each
//! secret of its Secrets Store to a Worker as a `Fetcher` whose `get()`
//! answers the value; celld binds the same (harness.js,
//! `__makeSecretsStoreSecret`), and its `get()` asks the host, which reads
//! the secret by its `secret_name` from the backend the node's operator
//! names. The Worker never sees the backend, its address or its
//! credentials: they are the node's configuration, not the Worker's
//! variables, and they stay in this process, outside every isolate.
//!
//! `CELLD_SECRETS_FILE` names a JSON file (owner-only: no group or other
//! bits) that says which backend, and nothing falls back to one it does not
//! name:
//!
//! - `{"backend": "openbao", "addr": "http://127.0.0.1:8200", "mount":
//!   "fragment", "prefix"?: "team/app", "namespace"?: "corp/eng",
//!   "token_file": "/etc/celld/openbao.token"}` (or `"token"` in place of
//!   `"token_file"`): Vault's KV v2 API, OpenBao's or a company's HashiCorp
//!   Vault, `GET <addr>/v1/<mount>/data/[<prefix>/]<secret_name>`, the
//!   value its field `value`. A token file is read again at every read, so
//!   a token renewed or replaced there is the one used; renewing it is its
//!   owner's (an agent, or the stack that minted it). A read is bounded
//!   (`READ_TIMEOUT`, `ANSWER_BYTES_MAX`) and never follows a redirect, so
//!   the token goes nowhere else.
//! - `{"backend": "vars", "values": {"<secret_name>": "<value>", ...}}`:
//!   the values in the file itself, for a `celld dev` node alone (dev and
//!   tests); a node of a fleet refuses it at start (`check_node`).
//!
//! Unset, the node has no backend, and every read rejects saying so. A read
//! that fails answers why, as a `code` (`unbound`, `missing`, `refused`,
//! `sealed`, `down`, `failed`, `malformed`) and a message that names the
//! secret and the backend, never a value or a token. An unknown secret is
//! `missing`: it rejects, as workerd's binding does. Nothing is cached
//! here; a Worker keeps what it read as long as it likes.
//!
//! The backends were fragment's (its cell's `secrets.mjs`, a stand-in for
//! the store's bindings on a runtime without them); they live here so that
//! a Worker reads its secrets on celld as it reads them on Cloudflare.

use anyhow::{anyhow, bail, Context as _};
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::Duration;

/// The variable that names the node's secrets file.
pub const VARIABLE: &str = "CELLD_SECRETS_FILE";
/// The secrets file, at most: a `vars` file's values, with room.
pub const FILE_MAX_BYTES: u64 = 1 << 20;
/// The most values a `vars` file holds.
pub const VALUES_MAX: usize = 1024;
/// A secret's value, at most: the Secrets Store's own limit.
pub const VALUE_MAX_BYTES: usize = 64 * 1024;
/// A secret's name, at most: the Secrets Store's own rule (letters, digits,
/// `-` and `_`).
pub const NAME_MAX_BYTES: usize = 255;
/// One read of OpenBao, its answer read whole, at most.
pub const READ_TIMEOUT: Duration = Duration::from_secs(5);
/// OpenBao's answer, at most: one value and its metadata.
pub const ANSWER_BYTES_MAX: usize = 256 * 1024;
/// A token, at most, and its file: Vault's are about 100 bytes.
pub const TOKEN_MAX_BYTES: usize = 1024;
const TOKEN_FILE_MAX_BYTES: u64 = 4096;
/// A mount, a prefix or a namespace, at most.
const PATH_BYTES_MAX: usize = 128;
/// The field a secret's value is in.
const VALUE_FIELD: &str = "value";

/// Whether `name` is a store secret's name.
pub fn valid_secret_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= NAME_MAX_BYTES
        && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// Why a secret could not be read: `code` is the kind, `message` says what
/// and where, with no value or token in it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Failure {
    pub code: &'static str,
    pub message: String,
}

fn failure(code: &'static str, message: impl Into<String>) -> Failure {
    Failure {
        code,
        message: message.into(),
    }
}

/// The backend a node reads its secrets from.
pub enum Backend {
    Vars { values: BTreeMap<String, String>, file: PathBuf },
    OpenBao(OpenBao),
}

impl std::fmt::Debug for Backend {
    // Names and places only: never a value or a token.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Backend::Vars { values, file } => f
                .debug_struct("Vars")
                .field("names", &values.keys().collect::<Vec<_>>())
                .field("file", file)
                .finish(),
            Backend::OpenBao(bao) => f.debug_struct("OpenBao").field("addr", &bao.addr).field("mount", &bao.mount).finish(),
        }
    }
}

/// Vault's KV v2 API at `addr`.
pub struct OpenBao {
    addr: String,
    mount: String,
    prefix: Option<String>,
    namespace: Option<String>,
    token: Token,
}

enum Token {
    Given(String),
    File(PathBuf),
}

/// Whether `token` is one: printable ASCII, no spaces, bounded.
fn valid_token(token: &str) -> bool {
    !token.is_empty() && token.len() <= TOKEN_MAX_BYTES && token.bytes().all(|b| b.is_ascii_graphic())
}

/// Whether `v` is a path of segments of letters, digits, `_`, `-` and `.`,
/// never `.` or `..`, within bounds.
fn segments(v: &str) -> bool {
    !v.is_empty()
        && v.len() <= PATH_BYTES_MAX
        && v.split('/').all(|s| {
            !s.is_empty() && s != "." && s != ".." && s.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.'))
        })
}

/// Whether `addr` is an origin: `http(s)://host[:port]`, no path, user or
/// query.
fn origin(addr: &str) -> bool {
    let Some(rest) = addr.strip_prefix("http://").or_else(|| addr.strip_prefix("https://")) else {
        return false;
    };
    let (host, port) = match rest.strip_prefix('[') {
        Some(v6) => match v6.split_once(']') {
            Some((ip, after)) => (ip, after),
            None => return false,
        },
        None => match rest.rsplit_once(':') {
            Some((host, _)) => (host, &rest[host.len()..]),
            None => (rest, ""),
        },
    };
    let host_ok = if rest.starts_with('[') {
        host.parse::<std::net::Ipv6Addr>().is_ok()
    } else {
        !host.is_empty() && host.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-'))
    };
    let port_ok = match port.strip_prefix(':') {
        None => port.is_empty(),
        Some(digits) => (1..=5).contains(&digits.len()) && digits.bytes().all(|b| b.is_ascii_digit()) && digits.parse::<u16>().is_ok(),
    };
    host_ok && port_ok
}

/// Reads `path`, at most `max` bytes, refusing one any but its owner may
/// read or write: it holds secrets.
#[allow(clippy::disallowed_methods)] // The operator's configuration file, not node storage.
fn owner_only(path: &Path, max: u64, what: &str) -> anyhow::Result<Vec<u8>> {
    use std::io::Read as _;
    let file = std::fs::File::open(path).with_context(|| format!("{what} {}", path.display()))?;
    let meta = file.metadata().with_context(|| format!("{what} {}", path.display()))?;
    if !meta.is_file() {
        bail!("{what} {} is not a file", path.display());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let mode = meta.permissions().mode() & 0o777;
        if mode & 0o077 != 0 {
            bail!("{what} {} is readable by others (mode {mode:o}): it holds secrets, so make it its owner's alone (chmod 600)", path.display());
        }
    }
    if meta.len() > max {
        bail!("{what} {} is {} bytes; at most {max} are read", path.display(), meta.len());
    }
    let mut bytes = Vec::new();
    file.take(max + 1).read_to_end(&mut bytes).with_context(|| format!("{what} {}", path.display()))?;
    if bytes.len() as u64 > max {
        bail!("{what} {} is over {max} bytes", path.display());
    }
    Ok(bytes)
}

/// The token in `path`, checked.
fn token_from(path: &Path) -> anyhow::Result<String> {
    let bytes = owner_only(path, TOKEN_FILE_MAX_BYTES, "the OpenBao token file")?;
    let token = String::from_utf8(bytes).map_err(|_| anyhow!("the OpenBao token file {} is not text", path.display()))?;
    let token = token.trim();
    if !valid_token(token) {
        bail!("the OpenBao token file {} holds no token (printable ASCII, at most {TOKEN_MAX_BYTES} bytes)", path.display());
    }
    Ok(token.to_string())
}

/// A string field of `config`, if present, or why it is refused. The
/// refusal names the field and the kind of value it holds, never the value:
/// the field may be the token.
fn string_field<'a>(config: &'a serde_json::Map<String, Value>, key: &str) -> anyhow::Result<Option<&'a str>> {
    match config.get(key) {
        None => Ok(None),
        Some(Value::String(s)) => Ok(Some(s)),
        Some(other) => {
            let kind = match other {
                Value::Null => "null",
                Value::Bool(_) => "true or false",
                Value::Number(_) => "a number",
                Value::Array(_) => "an array",
                Value::Object(_) => "an object",
                Value::String(_) => unreachable!("matched above"),
            };
            bail!("{VARIABLE}'s `{key}` is a string, not {kind}")
        }
    }
}

impl Backend {
    /// The backend `text` (a secrets file's contents, read from `file`)
    /// names, or why it is refused. A refusal names the setting and what it
    /// takes, never what it held: a token put in the wrong field would
    /// otherwise reach the node's log.
    pub fn parse(text: &[u8], file: &Path) -> anyhow::Result<Backend> {
        let parsed: Value = serde_json::from_slice(text).map_err(|e| anyhow!("{VARIABLE} {} is not JSON: {e}", file.display()))?;
        let Value::Object(config) = parsed else {
            bail!("{VARIABLE} {} holds an object naming its backend", file.display());
        };
        let known: &[&str] = match string_field(&config, "backend")? {
            Some("vars") => &["backend", "values"],
            Some("openbao") => &["backend", "addr", "mount", "prefix", "namespace", "token", "token_file"],
            Some(_) => bail!("{VARIABLE}'s backend is openbao or vars"),
            None => bail!("{VARIABLE} names its backend (`\"backend\": \"openbao\"`, or `\"vars\"` for `celld dev`)"),
        };
        let unknown: Vec<&str> = config.keys().map(String::as_str).filter(|k| !known.contains(k)).collect();
        if !unknown.is_empty() {
            bail!("{VARIABLE}'s backend takes {}, not {}", known.join(", "), unknown.join(", "));
        }
        if config["backend"] == "vars" {
            let Some(Value::Object(given)) = config.get("values") else {
                bail!("{VARIABLE}'s backend vars holds its `values`: an object of each secret's name and value");
            };
            if given.len() > VALUES_MAX {
                bail!("{VARIABLE} holds {} values; at most {VALUES_MAX}", given.len());
            }
            let mut values = BTreeMap::new();
            for (name, value) in given {
                if !valid_secret_name(name) {
                    bail!("{VARIABLE} names a secret by what is no name: a name is at most {NAME_MAX_BYTES} letters, digits, `-` and `_`");
                }
                let Value::String(value) = value else {
                    bail!("{VARIABLE}'s value of {name} is a string");
                };
                if value.is_empty() || value.len() > VALUE_MAX_BYTES {
                    bail!("{VARIABLE}'s value of {name} is 1 to {VALUE_MAX_BYTES} bytes, not {}", value.len());
                }
                values.insert(name.clone(), value.clone());
            }
            return Ok(Backend::Vars { values, file: file.to_path_buf() });
        }
        let addr = string_field(&config, "addr")?.unwrap_or_default();
        if !origin(addr) {
            bail!("{VARIABLE}'s addr is OpenBao's origin (https://vault.example:8200, no path)");
        }
        let mount = string_field(&config, "mount")?.unwrap_or_default();
        if !segments(mount) {
            bail!("{VARIABLE}'s mount is a KV v2 mount's path");
        }
        let optional = |key: &str| -> anyhow::Result<Option<String>> {
            match string_field(&config, key)? {
                None => Ok(None),
                Some(v) if segments(v) => Ok(Some(v.to_string())),
                Some(_) => bail!("{VARIABLE}'s {key}, when set, is a path"),
            }
        };
        let prefix = optional("prefix")?;
        let namespace = optional("namespace")?;
        let token = match (string_field(&config, "token")?, string_field(&config, "token_file")?) {
            (Some(token), None) if valid_token(token) => Token::Given(token.to_string()),
            (Some(_), None) => bail!("{VARIABLE}'s token is not a token (printable ASCII, at most {TOKEN_MAX_BYTES} bytes)"),
            (None, Some(path)) if Path::new(path).is_absolute() => {
                // read now, so a node whose file is wrong does not start
                token_from(Path::new(path))?;
                Token::File(PathBuf::from(path))
            }
            (None, Some(_)) => bail!("{VARIABLE}'s token_file is an absolute path, not a relative one"),
            (Some(_), Some(_)) => bail!("{VARIABLE} names OpenBao's token or its token_file, not both"),
            (None, None) => bail!("{VARIABLE}'s backend openbao reads with a token: name its `token_file` (or its `token`)"),
        };
        Ok(Backend::OpenBao(OpenBao {
            addr: addr.to_string(),
            mount: mount.to_string(),
            prefix,
            namespace,
            token,
        }))
    }

    /// The secret named `name`, or why not.
    pub async fn read(&self, name: &str) -> Result<String, Failure> {
        if !valid_secret_name(name) {
            return Err(failure("failed", format!("{name:?} is no secret's name")));
        }
        match self {
            Backend::Vars { values, file } => values.get(name).cloned().ok_or_else(|| {
                failure("missing", format!("{name} is not among this node's secrets ({VARIABLE} {})", file.display()))
            }),
            Backend::OpenBao(bao) => bao.read(name).await,
        }
    }
}

/// The client OpenBao is read with: the roots a Worker trusts (Mozilla's
/// and `CELLD_EXTRA_CA_FILE`'s, tls_roots.rs), no proxy, no redirect, each
/// read bounded. Not a Worker's egress: OpenBao is the node's own service,
/// often on loopback, which a Worker's fetch never reaches.
fn client() -> &'static reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        let mut builder = reqwest::Client::builder()
            .tls_built_in_native_certs(false)
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(READ_TIMEOUT);
        for certificate in crate::tls_roots::extra_certificates() {
            let certificate = reqwest::Certificate::from_der(certificate).expect("a checked root");
            builder = builder.add_root_certificate(certificate);
        }
        builder.build().expect("build the secrets backend's HTTP client")
    })
}

/// The first of an answer's `errors`, for a message: bounded, one line.
fn said(body: &Value) -> String {
    let first = body["errors"][0].as_str().unwrap_or("");
    let first: String = first.split_whitespace().collect::<Vec<_>>().join(" ");
    if first.is_empty() {
        return String::new();
    }
    let cut: String = first.chars().take(200).collect();
    format!(": {cut}")
}

impl OpenBao {
    async fn read(&self, name: &str) -> Result<String, Failure> {
        let place = format!("OpenBao at {}", self.addr);
        let under = self.prefix.as_ref().map(|p| format!("{p}/")).unwrap_or_default();
        let path = format!("{}/{under}{name}", self.mount);
        let put = format!("bao kv put -mount={} {under}{name} {VALUE_FIELD}=…", self.mount);
        let token = match &self.token {
            Token::Given(token) => token.clone(),
            Token::File(file) => token_from(file).map_err(|e| failure("refused", format!("{place}: {e:#}")))?,
        };
        let mut request = client()
            .get(format!("{}/v1/{}/data/{under}{name}", self.addr, self.mount))
            .header("x-vault-token", token)
            .header("accept", "application/json");
        if let Some(namespace) = &self.namespace {
            request = request.header("x-vault-namespace", namespace);
        }
        let timed_out = || failure("down", format!("{place} did not answer for {path} within {} s", READ_TIMEOUT.as_secs()));
        let mut response = match request.send().await {
            Ok(response) => response,
            Err(e) if e.is_timeout() => return Err(timed_out()),
            Err(e) => {
                // the error's chain, without its URL (no token is in it,
                // but the place says where)
                let e = e.without_url();
                let mut cause = String::new();
                let mut source = std::error::Error::source(&e);
                while let Some(s) = source {
                    cause = s.to_string();
                    source = s.source();
                }
                let why = if cause.is_empty() { e.to_string() } else { format!("{e}: {cause}") };
                return Err(failure("down", format!("{place} is not answering ({why})")));
            }
        };
        let status = response.status().as_u16();
        if response.content_length().is_some_and(|n| n > ANSWER_BYTES_MAX as u64) {
            return Err(failure("failed", format!("{place} answered {status} for {path} with over {ANSWER_BYTES_MAX} bytes")));
        }
        let mut body = Vec::new();
        loop {
            match response.chunk().await {
                Ok(Some(chunk)) => {
                    if body.len() + chunk.len() > ANSWER_BYTES_MAX {
                        return Err(failure("failed", format!("{place} answered {status} for {path} with over {ANSWER_BYTES_MAX} bytes")));
                    }
                    body.extend_from_slice(&chunk);
                }
                Ok(None) => break,
                Err(e) if e.is_timeout() => return Err(timed_out()),
                Err(e) => return Err(failure("down", format!("{place} broke off its answer for {path} ({})", e.without_url()))),
            }
        }
        // a proxy's page, or nothing, is said by its status
        let body: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
        match status {
            200 => match body["data"]["data"][VALUE_FIELD].as_str() {
                Some(value) if !value.is_empty() => Ok(value.to_string()),
                _ => Err(failure(
                    "malformed",
                    format!("{path} in {place} has no \"{VALUE_FIELD}\" field: a secret's value is its \"{VALUE_FIELD}\" ({put})"),
                )),
            },
            404 => Err(failure("missing", format!("{path} is not in {place} ({put})"))),
            403 => Err(failure(
                "refused",
                format!(
                    "{place} refused the node's token for {path} (403{}): the token is wrong or has lapsed, or its policy does not cover the path",
                    said(&body)
                ),
            )),
            503 => Err(failure("sealed", format!("{place} is sealed, or not ready (503{})", said(&body)))),
            _ => Err(failure("failed", format!("{place} answered {status} for {path}{}", said(&body)))),
        }
    }
}

/// The node's backend, read once (`load`): `None` when it names none.
static BACKEND: OnceLock<Option<Backend>> = OnceLock::new();

fn from_env() -> anyhow::Result<Option<Backend>> {
    let Some(path) = crate::env_vars::value(VARIABLE)? else {
        return Ok(None);
    };
    if path.is_empty() {
        bail!("{VARIABLE} is set and empty: name the node's secrets file, or unset it");
    }
    let path = PathBuf::from(path);
    if !path.is_absolute() {
        bail!("{VARIABLE} is an absolute path, not {}", path.display());
    }
    let text = owner_only(&path, FILE_MAX_BYTES, VARIABLE)?;
    Backend::parse(&text, &path).map(Some)
}

/// Reads and checks the node's secrets file, before the node starts: a file
/// that is wrong stops the process, saying why.
pub fn load() -> anyhow::Result<()> {
    if BACKEND.get().is_none() {
        let backend = from_env()?;
        // a racing first call read the same file; either value is it
        let _ = BACKEND.set(backend);
    }
    Ok(())
}

fn backend() -> Option<&'static Backend> {
    BACKEND.get_or_init(|| from_env().expect("validated CELLD_SECRETS_FILE")).as_ref()
}

/// Refuses the `vars` backend on a node that is not `celld dev`'s
/// (`dev`): a fleet's secrets are OpenBao's, never values in a file.
pub fn check_node(dev: bool) -> anyhow::Result<()> {
    check(backend(), dev)
}

fn check(backend: Option<&Backend>, dev: bool) -> anyhow::Result<()> {
    if let (Some(Backend::Vars { file, .. }), false) = (backend, dev) {
        bail!(
            "{VARIABLE} {} names the backend vars, which is `celld dev`'s alone (dev, tests): a node's secrets are OpenBao's",
            file.display()
        );
    }
    Ok(())
}

/// The secret named `name` from the node's backend, or why not.
pub async fn read(name: &str) -> Result<String, Failure> {
    match backend() {
        Some(backend) => backend.read(name).await,
        None => Err(failure("unbound", format!("this node has no secrets backend, so it has no secret {name}: name one in {VARIABLE}"))),
    }
}

#[cfg(test)]
mod tests;
