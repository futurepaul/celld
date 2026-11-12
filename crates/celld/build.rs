//! celld links V8 only from the archive `.cargo/config.toml` pins by hash,
//! with the `v8` crate's own Rust binding, kept in this repository and pinned
//! by hash too (docs/v8.md).
//!
//! The binding is a file in the repository (`.cargo/v8/`), not the copy in
//! the crate's extracted registry source. The `v8` build script overwrites
//! that copy when a binding mirror or URL is set, Cargo never checks it again,
//! and Cargo does not notice a change to it. `RUSTY_V8_SRC_BINDING_PATH`
//! (forced in `.cargo/config.toml`) makes the `v8` build script hand rustc the
//! repository's file instead, without downloading or writing any binding.
//! This script hashes the file that variable names, so it checks exactly what
//! the `v8` crate compiles, on every run and whenever the file changes.
//!
//! It also refuses, whatever their value, the variables that would bring
//! other V8 code in: a mirror or URL for the binding, the trybuild shortcut
//! (which skips the archive), a build from source, and a debug build (which
//! selects another archive). Any failure stops celld from linking, whichever
//! build script runs first. A target without a pinned archive fails too.
//!
//! Which `v8` features are on is not checked here: only this repository's
//! manifests can turn them on (Cargo.lock shows celld alone depends on v8),
//! and changing those is a reviewed code change.

use sha2::{Digest as _, Sha256};
use std::io::Write as _;
use std::path::Path;

/// Variables that swap V8 code past the pin.
const REFUSED: [&str; 5] = [
    "RUSTY_V8_MIRROR",
    "RUSTY_V8_SRC_BINDING_URL",
    "DENO_TRYBUILD",
    "V8_FROM_SOURCE",
    "V8_FORCE_DEBUG",
];

/// The pin `.cargo/config.toml` sets.
const PINNED: [&str; 5] = [
    "RUSTY_V8_ARCHIVE",
    "RUSTY_V8_ARCHIVE_SHA256",
    "RUSTY_V8_SRC_BINDING_PATH",
    "CELLD_V8_BINDING_SHA256",
    "CELLD_V8_VERSION",
];

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
        "celld links only the pinned V8 archive and binding (docs/v8.md); unset {}",
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
        "CELLD_V8_BINDING_SHA256 must pin the v8 binding (docs/v8.md), not {binding_sha:?}"
    );

    // The file the v8 build script passes to rustc as-is (its
    // `BindingSource::Path`), included by its src/binding.rs.
    let binding = std::env::var("RUSTY_V8_SRC_BINDING_PATH").unwrap_or_default();
    assert!(
        binding.ends_with(&format!("/src_binding_release_{target}.rs")),
        "RUSTY_V8_SRC_BINDING_PATH must name the pinned binding for {target}, not {binding:?} (docs/v8.md)"
    );
    let binding = Path::new(&binding);
    writeln!(out, "cargo:rerun-if-changed={}", binding.display()).expect("build script stdout");
    let actual = sha256_of(binding);
    assert!(
        actual.eq_ignore_ascii_case(&binding_sha),
        "the v8 binding {} is not the pinned one (SHA-256 {actual}, pinned {binding_sha}); \
         restore it from git, or from the v8 crate's .crate with tools/v8/rusty-v8.sh pin",
        binding.display()
    );
}

fn is_sha256(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|b| b.is_ascii_hexdigit())
}

#[allow(clippy::disallowed_methods)] // a build input, not node storage
fn sha256_of(path: &Path) -> String {
    let bytes = std::fs::read(path)
        .unwrap_or_else(|error| panic!("cannot read the v8 binding {}: {error}", path.display()));
    format!("{:x}", Sha256::digest(&bytes))
}
