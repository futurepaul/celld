//! celld links V8 only from the archive `.cargo/config.toml` pins by hash,
//! with the `v8` crate's own Rust binding, pinned by hash too (docs/v8.md).
//!
//! Two checks:
//!
//! - Variables the `v8` build script honors that bring other V8 code in are
//!   refused, whatever their value: a mirror, or a URL or path, for the
//!   binding it compiles as Rust source; the trybuild shortcut, which skips
//!   the archive; a build from source; a debug build, which selects another
//!   binding and archive.
//! - The binding the `v8` crate compiles is hashed against the pin. That
//!   binding is a file in the crate's extracted source, which the `v8` build
//!   script itself overwrites when a mirror or binding URL is set, and which
//!   Cargo does not check again once extracted. A refused build can leave it
//!   poisoned for the next one, so the file is checked on every build that
//!   could compile it.
//!
//! Either failure stops celld from linking, whichever build script runs
//! first. A target without a pinned archive fails too.

use sha2::{Digest as _, Sha256};
use std::io::Write as _;
use std::path::{Path, PathBuf};

/// Variables that swap V8 code past the pin.
const REFUSED: [&str; 6] = [
    "RUSTY_V8_MIRROR",
    "RUSTY_V8_SRC_BINDING_URL",
    "RUSTY_V8_SRC_BINDING_PATH",
    "DENO_TRYBUILD",
    "V8_FROM_SOURCE",
    "V8_FORCE_DEBUG",
];

/// The pin `.cargo/config.toml` sets.
const PINNED: [&str; 4] = [
    "RUSTY_V8_ARCHIVE",
    "RUSTY_V8_ARCHIVE_SHA256",
    "CELLD_V8_BINDING_SHA256",
    "CELLD_V8_VERSION",
];

/// `v8` features that select another archive and binding than the pinned
/// ones (the `_ptrcomp` and `_sandbox` builds).
const OTHER_BUILDS: [&str; 3] = [
    "v8_enable_pointer_compression",
    "v8_enable_sandbox",
    "v8_enable_multi_cage",
];

const CRATES_IO: &str = "registry+https://github.com/rust-lang/crates.io-index";

fn main() {
    let mut out = std::io::stdout();
    for name in REFUSED.iter().chain(PINNED.iter()) {
        writeln!(out, "cargo:rerun-if-env-changed={name}").expect("build script stdout");
    }

    let present: Vec<&str> = REFUSED
        .into_iter()
        .filter(|name| std::env::var_os(name).is_some())
        .collect();
    assert!(
        present.is_empty(),
        "celld links only the pinned V8 archive and the v8 crate's own binding \
         (docs/v8.md); unset {}",
        present.join(", ")
    );

    let target = std::env::var("TARGET").expect("cargo sets TARGET");
    let archive = std::env::var("RUSTY_V8_ARCHIVE").unwrap_or_default();
    assert!(
        archive.ends_with(&format!("/librusty_v8_release_{target}.a.gz")),
        "no pinned V8 archive for {target}: RUSTY_V8_ARCHIVE is {archive:?}; \
         build from the repository so .cargo/config.toml applies (docs/v8.md)"
    );
    let archive_sha = std::env::var("RUSTY_V8_ARCHIVE_SHA256").unwrap_or_default();
    assert!(
        is_sha256(&archive_sha),
        "RUSTY_V8_ARCHIVE_SHA256 must pin the archive (docs/v8.md), not {archive_sha:?}"
    );
    let binding_sha = std::env::var("CELLD_V8_BINDING_SHA256").unwrap_or_default();
    assert!(
        is_sha256(&binding_sha),
        "CELLD_V8_BINDING_SHA256 must pin the v8 crate's binding (docs/v8.md), not {binding_sha:?}"
    );

    // With nothing above set, the v8 build script hands rustc
    // `<its manifest dir>/gen/src_binding_release_<target>.rs`
    // (`RUSTY_V8_SRC_BINDING_PATH`, included by its src/binding.rs).
    let binding = v8_manifest_dir(&target)
        .join("gen")
        .join(format!("src_binding_release_{target}.rs"));
    // Again whenever the file changes, so a binding poisoned after this build
    // is caught by the next one, as the v8 crate recompiles.
    writeln!(out, "cargo:rerun-if-changed={}", binding.display()).expect("build script stdout");
    let actual = sha256_of(&binding);
    assert!(
        actual.eq_ignore_ascii_case(&binding_sha),
        "the v8 crate's binding {} is not the pinned one (SHA-256 {actual}, pinned {binding_sha}). \
         A build with a binding mirror or URL set overwrites it in the extracted crate. \
         Delete {} so cargo extracts it again from the .crate that Cargo.lock checks.",
        binding.display(),
        binding.parent().and_then(Path::parent).unwrap_or(&binding).display()
    );
}

fn is_sha256(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|b| b.is_ascii_hexdigit())
}

/// The `v8` package's source directory, as Cargo resolves it for this
/// workspace (registry, vendored or otherwise). It must be crates.io's, with
/// none of the features that select another build.
///
/// `cargo metadata` is the one interface that names the directory Cargo
/// compiles a package from, whatever the source replacement or CARGO_HOME;
/// Cargo exports neither to build scripts, and `v8` has no `links` key to
/// carry it. It reads every package's manifest, more than `cargo build`
/// downloads, so it may download too: never `--offline` here, or a first
/// build fails. An offline build (`CARGO_NET_OFFLINE`, or `net.offline` in
/// config, both of which it inherits) needs `cargo fetch` first.
fn v8_manifest_dir(target: &str) -> PathBuf {
    let cargo = std::env::var_os("CARGO").expect("cargo sets CARGO");
    let manifest = PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").expect("cargo sets CARGO_MANIFEST_DIR"))
        .join("Cargo.toml");
    let output = std::process::Command::new(cargo)
        .args(["metadata", "--format-version", "1", "--locked", "--filter-platform", target])
        .arg("--manifest-path")
        .arg(&manifest)
        .output()
        .expect("run cargo metadata");
    assert!(
        output.status.success(),
        "cargo metadata failed; it reads every package's manifest, so an offline build \
         needs `cargo fetch` first (docs/v8.md): {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let metadata: serde_json::Value = serde_json::from_slice(&output.stdout).expect("cargo metadata's JSON");

    let packages: Vec<&serde_json::Value> = metadata["packages"]
        .as_array()
        .expect("metadata packages")
        .iter()
        .filter(|package| package["name"] == "v8")
        .collect();
    assert!(packages.len() == 1, "expected one v8 package, found {}", packages.len());
    let v8 = packages[0];
    assert!(
        v8["source"] == CRATES_IO,
        "celld links crates.io's v8, not {}",
        v8["source"]
    );
    let features = metadata["resolve"]["nodes"]
        .as_array()
        .expect("metadata resolve nodes")
        .iter()
        .find(|node| node["id"] == v8["id"])
        .and_then(|node| node["features"].as_array())
        .expect("v8's resolved features");
    let other: Vec<&str> = features
        .iter()
        .filter_map(serde_json::Value::as_str)
        .filter(|feature| OTHER_BUILDS.contains(feature))
        .collect();
    assert!(
        other.is_empty(),
        "v8 is built with {other:?}, which no pinned archive matches (docs/v8.md)"
    );

    Path::new(v8["manifest_path"].as_str().expect("v8's manifest path"))
        .parent()
        .expect("a manifest has a directory")
        .to_path_buf()
}

#[allow(clippy::disallowed_methods)] // a source file of a dependency, not node storage
fn sha256_of(path: &Path) -> String {
    let bytes = std::fs::read(path).unwrap_or_else(|error| {
        panic!(
            "cannot read the v8 crate's binding {}: {error}. Delete {} so cargo extracts it \
             again from the .crate that Cargo.lock checks.",
            path.display(),
            path.parent().and_then(Path::parent).unwrap_or(path).display()
        )
    });
    format!("{:x}", Sha256::digest(&bytes))
}
