//! celld links V8 only from the archive `.cargo/config.toml` pins by hash,
//! with the `v8` crate's own Rust binding, which Cargo.lock checksums
//! (docs/v8.md). The `v8` build script also honors variables that bring
//! other V8 code in unchecked: a mirror, or a URL or path, for the binding it
//! compiles as Rust source; the trybuild shortcut, which skips the archive;
//! and a build from source. When any of them is set, to any value, the build
//! fails here. That holds whichever build script runs first, because celld
//! is never linked. A target without a pinned archive fails here too.

use std::io::Write as _;

/// Variables that swap V8 code past the pin.
const REFUSED: [&str; 5] = [
    "RUSTY_V8_MIRROR",
    "RUSTY_V8_SRC_BINDING_URL",
    "RUSTY_V8_SRC_BINDING_PATH",
    "DENO_TRYBUILD",
    "V8_FROM_SOURCE",
];

/// The pin `.cargo/config.toml` sets.
const PINNED: [&str; 3] = ["RUSTY_V8_ARCHIVE", "RUSTY_V8_ARCHIVE_SHA256", "CELLD_V8_VERSION"];

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
    let sha = std::env::var("RUSTY_V8_ARCHIVE_SHA256").unwrap_or_default();
    assert!(
        sha.len() == 64 && sha.bytes().all(|b| b.is_ascii_hexdigit()),
        "RUSTY_V8_ARCHIVE_SHA256 must pin the archive (docs/v8.md), not {sha:?}"
    );
}
