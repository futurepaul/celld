// A test of the backend drives real sockets, files and clocks.
#![allow(clippy::disallowed_methods)]

use super::*;
use serde_json::json;
use std::io::{BufRead as _, BufReader, Write as _};
use std::net::{TcpListener, TcpStream};
use std::os::unix::fs::PermissionsExt as _;
use std::sync::{Arc, Mutex};
use std::time::Instant;

/// What one request to the scripted server carried.
#[derive(Debug, Clone)]
struct Hit {
    path: String,
    token: Option<String>,
    namespace: Option<String>,
}

/// What a scripted server answers a path: its status, body and extra
/// headers, or, for `None`, nothing: it holds the connection.
type Answer = fn(&str) -> Option<(u16, String, Vec<(String, String)>)>;

struct Scripted {
    addr: String,
    hits: Arc<Mutex<Vec<Hit>>>,
}

fn scripted(answer: Answer) -> Scripted {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = format!("http://{}", listener.local_addr().unwrap());
    let hits: Arc<Mutex<Vec<Hit>>> = Arc::default();
    let log = Arc::clone(&hits);
    std::thread::spawn(move || {
        // bounded: the test's requests, then the test process ends
        for stream in listener.incoming() {
            let Ok(stream) = stream else { continue };
            let log = Arc::clone(&log);
            std::thread::spawn(move || serve(stream, answer, log));
        }
    });
    Scripted { addr, hits }
}

fn serve(mut stream: TcpStream, answer: Answer, log: Arc<Mutex<Vec<Hit>>>) {
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let mut first = String::new();
    if reader.read_line(&mut first).unwrap_or(0) == 0 {
        return;
    }
    let path = first.split_whitespace().nth(1).unwrap_or("").to_string();
    let (mut token, mut namespace) = (None, None);
    // bounded: a request's head, at most 100 lines
    for _ in 0..100 {
        let mut line = String::new();
        if reader.read_line(&mut line).unwrap_or(0) == 0 || line.trim().is_empty() {
            break;
        }
        let (k, v) = line.split_once(':').unwrap_or(("", ""));
        match k.trim().to_ascii_lowercase().as_str() {
            "x-vault-token" => token = Some(v.trim().to_string()),
            "x-vault-namespace" => namespace = Some(v.trim().to_string()),
            _ => {}
        }
    }
    log.lock().unwrap().push(Hit { path: path.clone(), token, namespace });
    match answer(&path) {
        Some((status, body, extra)) => {
            let extra: String = extra.iter().map(|(k, v)| format!("{k}: {v}\r\n")).collect();
            let head = format!(
                "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n{extra}\r\n",
                body.len()
            );
            let _ = stream.write_all(head.as_bytes());
            let _ = stream.write_all(body.as_bytes());
        }
        // a service that hangs: the connection held, no answer
        None => std::thread::sleep(Duration::from_secs(30)),
    }
}

/// Where the scripted OpenBao's redirect points: another server, which
/// must see nothing.
static ELSEWHERE: OnceLock<String> = OnceLock::new();

fn openbao(addr: &str, extra: serde_json::Value) -> Backend {
    let mut config = json!({ "backend": "openbao", "addr": addr, "mount": "fragment", "token": "s.test-token" });
    for (k, v) in extra.as_object().into_iter().flatten() {
        if v.is_null() {
            config.as_object_mut().unwrap().remove(k);
        } else {
            config[k] = v.clone();
        }
    }
    Backend::parse(config.to_string().as_bytes(), Path::new("/etc/celld/secrets.json")).unwrap()
}

/// A port nothing listens on.
fn dead_addr() -> String {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    format!("http://{}", l.local_addr().unwrap())
}

fn failed(r: &Result<String, Failure>, code: &str, says: &[&str]) {
    let f = r.as_ref().expect_err("a failure");
    assert_eq!(f.code, code, "{f:?}");
    for s in says {
        assert!(f.message.contains(s), "{s:?} in {:?}", f.message);
    }
}

fn scratch() -> tempfile::TempDir {
    tempfile::tempdir().unwrap()
}

/// `contents` at `path`, mode `mode`.
fn file(path: &Path, contents: &str, mode: u32) {
    std::fs::write(path, contents).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
}

// Goal: OpenBao is read at `<addr>/v1/<mount>/data/[<prefix>/]<name>` with
// the token (and the namespace when set), answering the value, and says why
// when it cannot, each kind its own: missing (404), refused (403), sealed
// (503), malformed (no `value`), failed (500, and a redirect, which it never
// follows, so the token goes nowhere else), and down: nothing listening at
// once, a server that hangs within its bound. No message holds the token.
// Method: a scripted server, as fragment's tests of its cell's secrets shim
// had it (this backend was that shim's).
#[tokio::test(flavor = "multi_thread")]
async fn openbao_is_read_through_the_kv_api_and_says_why_it_cannot() {
    let elsewhere = scripted(|_| Some((200, "{}".into(), vec![])));
    ELSEWHERE.set(elsewhere.addr.clone()).expect("set once");
    let bao = scripted(|path| {
        let ok = |v: &str| Some((200, json!({ "data": { "data": { "value": v }, "metadata": { "version": 3 } } }).to_string(), vec![]));
        match path.rsplit('/').next().unwrap_or("") {
            "kept" => ok("the-value"),
            "missing" => Some((404, "{\"errors\":[]}".into(), vec![])),
            "refused" => Some((403, "{\"errors\":[\"1 error occurred:\\n\\t* permission denied\\n\\n\"]}".into(), vec![])),
            "sealed" => Some((503, "{\"errors\":[\"Vault is sealed\"]}".into(), vec![])),
            "odd" => Some((200, json!({ "data": { "data": { "other": "x" } } }).to_string(), vec![])),
            "broken" => Some((500, "{\"errors\":[\"internal error\"]}".into(), vec![])),
            "huge" => Some((200, "x".repeat(ANSWER_BYTES_MAX + 1), vec![])),
            "moved" => Some((307, "{}".into(), vec![("location".into(), format!("{}/v1/fragment/data/moved", ELSEWHERE.get().expect("set")))])),
            "hang" => None,
            _ => Some((404, "{\"errors\":[]}".into(), vec![])),
        }
    });
    let a = bao.addr.as_str();
    let plain = openbao(a, json!({}));
    let placed = openbao(a, json!({ "prefix": "team/fragment", "namespace": "corp/eng" }));
    let mut results = vec![
        plain.read("kept").await,
        placed.read("kept").await,
        plain.read("missing").await,
        plain.read("refused").await,
        plain.read("sealed").await,
        plain.read("odd").await,
        plain.read("broken").await,
        plain.read("huge").await,
        plain.read("moved").await,
    ];
    assert_eq!(results[0], Ok("the-value".to_string()));
    assert_eq!(results[1], Ok("the-value".to_string()));
    failed(&results[2], "missing", &["fragment/missing is not in OpenBao at", "bao kv put -mount=fragment missing value="]);
    failed(&results[3], "refused", &["refused the node's token for fragment/refused", "permission denied"]);
    failed(&results[4], "sealed", &["is sealed", "Vault is sealed"]);
    failed(&results[5], "malformed", &["has no \"value\" field"]);
    failed(&results[6], "failed", &["answered 500", "internal error"]);
    failed(&results[7], "failed", &["answered 200", "over 262144 bytes"]);
    failed(&results[8], "failed", &["answered 307"]);

    let t0 = Instant::now();
    let down = openbao(&dead_addr(), json!({})).read("kept").await;
    failed(&down, "down", &["is not answering"]);
    assert!(t0.elapsed() < Duration::from_secs(2), "nothing listening is an error at once: {:?}", t0.elapsed());
    let t0 = Instant::now();
    let hung = plain.read("hang").await;
    let took = t0.elapsed();
    failed(&hung, "down", &["did not answer for fragment/hang within 5 s"]);
    assert!((Duration::from_millis(4_900)..Duration::from_secs(8)).contains(&took), "a hang is cut at its bound: {took:?}");
    results.extend([down, hung]);
    for (i, r) in results.iter().enumerate() {
        assert!(!format!("{r:?}").contains("s.test-token"), "no answer or message holds the token: case {i}: {r:?}");
    }
    assert!(!format!("{plain:?}").contains("s.test-token"), "nor does the backend's Debug");

    let hits = bao.hits.lock().unwrap().clone();
    let kept: Vec<&Hit> = hits.iter().filter(|h| h.path.ends_with("/kept")).collect();
    let paths: Vec<&str> = kept.iter().map(|h| h.path.as_str()).collect();
    assert!(paths.contains(&"/v1/fragment/data/kept") && paths.contains(&"/v1/fragment/data/team/fragment/kept"), "{paths:?}");
    assert!(hits.iter().all(|h| h.token.as_deref() == Some("s.test-token")), "every read carries the token: {hits:?}");
    assert!(
        kept.iter().any(|h| h.namespace.as_deref() == Some("corp/eng")) && kept.iter().any(|h| h.namespace.is_none()),
        "the namespace when set, alone"
    );
    assert!(elsewhere.hits.lock().unwrap().is_empty(), "a redirect is never followed: the token goes nowhere else");
}

// Goal: a token file is read at every read, so the token its owner renews
// or replaces there is the one sent; one others may read is refused, at the
// start and at a read. Replay: the same read twice sends the same token.
#[tokio::test(flavor = "multi_thread")]
async fn a_token_file_is_read_again_at_every_read() {
    let bao = scripted(|_| Some((200, json!({ "data": { "data": { "value": "v" } } }).to_string(), vec![])));
    let dir = scratch();
    let token = dir.path().join("cell.token");
    file(&token, "s.first\n", 0o600);
    let backend = openbao(&bao.addr, json!({ "token": null, "token_file": token.display().to_string() }));
    assert_eq!(backend.read("x").await, Ok("v".into()));
    assert_eq!(backend.read("x").await, Ok("v".into()));
    file(&token, "s.second", 0o600);
    assert_eq!(backend.read("x").await, Ok("v".into()));
    let sent: Vec<Option<String>> = bao.hits.lock().unwrap().iter().map(|h| h.token.clone()).collect();
    assert_eq!(sent, [Some("s.first".into()), Some("s.first".into()), Some("s.second".into())]);

    std::fs::set_permissions(&token, std::fs::Permissions::from_mode(0o644)).unwrap();
    let r = backend.read("x").await;
    failed(&r, "refused", &["is readable by others (mode 644)"]);
    assert!(!format!("{r:?}").contains("s.second"));
    assert_eq!(bao.hits.lock().unwrap().len(), 3, "nothing is sent without the token");
    let e = Backend::parse(
        json!({ "backend": "openbao", "addr": bao.addr, "mount": "fragment", "token_file": token.display().to_string() }).to_string().as_bytes(),
        Path::new("/s.json"),
    )
    .unwrap_err();
    assert!(e.to_string().contains("readable by others"), "refused at the start too: {e}");
}

// Goal: a config that is wrong is refused, saying what; nothing falls back to
// a backend it does not name. Each case once, and again: the same answer.
#[test]
fn a_wrong_config_is_refused_saying_what() {
    let a = "http://127.0.0.1:9";
    let bao = |extra: serde_json::Value| {
        let mut c = json!({ "backend": "openbao", "addr": a, "mount": "fragment", "token": "s.tok" });
        for (k, v) in extra.as_object().unwrap() {
            if v.is_null() {
                c.as_object_mut().unwrap().remove(k);
            } else {
                c[k] = v.clone();
            }
        }
        c
    };
    let cases = [
        (bao(json!({ "addr": "http://127.0.0.1:9/vault" })), "addr is OpenBao's origin"),
        (bao(json!({ "addr": "ftp://bao" })), "addr is OpenBao's origin"),
        (bao(json!({ "addr": "http://user@bao" })), "addr is OpenBao's origin"),
        (bao(json!({ "mount": "../sys" })), "mount is a KV v2 mount's path"),
        (bao(json!({ "prefix": "a/../b" })), "prefix, when set, is a path"),
        (bao(json!({ "namespace": "" })), "namespace, when set, is a path"),
        (bao(json!({ "adress": a })), "not adress"),
        (bao(json!({ "token": null })), "reads with a token"),
        (bao(json!({ "token": "has space" })), "is not a token"),
        (bao(json!({ "token_file": "/x" })), "not both"),
        (bao(json!({ "token": null, "token_file": "relative/cell.token" })), "is an absolute path"),
        (bao(json!({ "token": null, "token_file": "/nonexistent/cell.token" })), "the OpenBao token file /nonexistent/cell.token"),
        (json!({ "backend": "vars", "addr": a, "values": {} }), "takes backend, values, not addr"),
        (json!({ "backend": "vars" }), "holds its `values`"),
        (json!({ "backend": "vars", "values": { "a b": "x" } }), "names a secret \"a b\""),
        (json!({ "backend": "vars", "values": { "x": "" } }), "value of x is 1 to 65536 bytes"),
        (json!({ "backend": "vars", "values": { "x": 3 } }), "value of x is a string"),
        (json!({ "backend": "file" }), "openbao or vars, not \"file\""),
        (json!({ "values": {} }), "names its backend"),
        (json!([]), "holds an object"),
    ];
    for (config, says) in &cases {
        for _ in 0..2 {
            let e = Backend::parse(config.to_string().as_bytes(), Path::new("/s.json")).expect_err(says).to_string();
            assert!(e.contains(says), "{says:?} in {e:?}");
            assert!(!e.contains("s.tok\""), "no token in {e:?}");
        }
    }
    assert!(Backend::parse(b"not json", Path::new("/s.json")).unwrap_err().to_string().contains("is not JSON"));
}

// Goal: a vars backend answers its values by name; an unknown secret
// rejects (missing), as workerd's binding does; vars is `celld dev`'s alone.
#[tokio::test(flavor = "multi_thread")]
async fn vars_answer_their_values_on_a_dev_node_alone() {
    let vars = Backend::parse(
        json!({ "backend": "vars", "values": { "fragment-host-secret": "hs", "other_one": "o" } }).to_string().as_bytes(),
        Path::new("/dev/secrets.json"),
    )
    .unwrap();
    assert_eq!(vars.read("fragment-host-secret").await, Ok("hs".into()));
    assert_eq!(vars.read("other_one").await, Ok("o".into()));
    failed(&vars.read("nope").await, "missing", &["nope is not among this node's secrets", "CELLD_SECRETS_FILE /dev/secrets.json"]);
    failed(&vars.read("../x").await, "failed", &["is no secret's name"]);
    assert!(!format!("{vars:?}").contains("hs\""), "Debug names, never values: {vars:?}");

    assert!(check(Some(&vars), true).is_ok());
    let e = check(Some(&vars), false).unwrap_err().to_string();
    assert!(e.contains("`celld dev`'s alone") && e.contains("OpenBao's"), "{e}");
    let bao = openbao("http://127.0.0.1:9", json!({}));
    assert!(check(Some(&bao), false).is_ok() && check(Some(&bao), true).is_ok());
    assert!(check(None, false).is_ok() && check(None, true).is_ok());
}

// Goal: the secrets file is its owner's alone, bounded, and read whole.
#[test]
fn the_secrets_file_is_its_owners_alone() {
    let dir = scratch();
    let path = dir.path().join("secrets.json");
    file(&path, r#"{"backend":"vars","values":{"a":"b"}}"#, 0o640);
    let e = owner_only(&path, FILE_MAX_BYTES, VARIABLE).unwrap_err().to_string();
    assert!(e.contains("readable by others (mode 640)") && e.contains("chmod 600"), "{e}");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    let bytes = owner_only(&path, FILE_MAX_BYTES, VARIABLE).unwrap();
    assert!(Backend::parse(&bytes, &path).is_ok());
    let e = owner_only(&path, 4, VARIABLE).unwrap_err().to_string();
    assert!(e.contains("at most 4 are read"), "{e}");
    let e = owner_only(dir.path(), FILE_MAX_BYTES, VARIABLE).unwrap_err().to_string();
    assert!(e.contains("is not a file") || e.contains("readable by others"), "{e}");
}

#[test]
fn names_and_origins() {
    for name in ["fragment-host-secret", "A_b-9", &"x".repeat(NAME_MAX_BYTES)] {
        assert!(valid_secret_name(name), "{name}");
    }
    for name in ["", "a b", "a/b", "a.b", "é", &"x".repeat(NAME_MAX_BYTES + 1)] {
        assert!(!valid_secret_name(name), "{name}");
    }
    for addr in ["http://127.0.0.1:8200", "https://vault.example", "https://vault.example:8200", "http://[::1]:8200", "http://bao"] {
        assert!(origin(addr), "{addr}");
    }
    for addr in ["", "127.0.0.1:8200", "http://", "http://a:", "http://a:99999", "http://a/b", "http://[::1", "http://[zz]:1", "http://a b", "https://u@h"] {
        assert!(!origin(addr), "{addr}");
    }
}
