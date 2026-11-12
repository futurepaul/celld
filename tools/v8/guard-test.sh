#!/usr/bin/env bash
# Proves the V8 pin's guard (crates/celld/build.rs, docs/v8.md) with real
# builds of celld:
#   - a first build, on an empty CARGO_HOME, passes;
#   - each variable that brings other V8 code in fails it, even where the v8
#     build script accepts it (a mirror or URL serving a binding with a marker
#     line added, as an attacker's would);
#   - pins from the environment are ignored;
#   - a poisoned binding in the extracted v8 crate (its registry source, or
#     its git checkout, which ships none) is never compiled: the v8 crate
#     compiles the repository's binding (.cargo/v8/);
#   - a changed repository binding fails the build, naming it, and passes
#     once restored.
#
# It runs against a private, empty CARGO_HOME, so it downloads what a first
# build on a new machine downloads and never touches your registry; and a
# private target dir, kept between runs.
#
#   tools/v8/guard-test.sh        (CELLD_GUARD_WORK, default target/guard-test;
#                                  CELLD_V8_JOBS, default 16)
set -euo pipefail

ROOT=$(git -C "$(dirname "$0")" rev-parse --show-toplevel)
JOBS=${CELLD_V8_JOBS:-16}
WORK=${CELLD_GUARD_WORK:-$ROOT/target/guard-test}
TARGET=$(rustc -vV | sed -n 's/^host: //p')
BINDING="src_binding_release_${TARGET}.rs"
PINNED="$ROOT/.cargo/v8/$BINDING"
home="$WORK/cargo-home"
attack="$WORK/attack"

rm -rf "$home" "$attack"
mkdir -p "$home" "$attack/mirror"
export CARGO_HOME="$home" CARGO_TARGET_DIR="$WORK/target"
cp "$PINNED" "$attack/pinned.rs"
# The repository's binding is changed below; put it back however this ends.
trap 'cp "$attack/pinned.rs" "$PINNED"' EXIT
{ cat "$PINNED"; echo "// guard-test: not the crate's binding"; } > "$attack/evil.rs"
cp "$attack/evil.rs" "$attack/mirror/$BINDING"
pinned_sha=$(sha256sum "$PINNED" | cut -d' ' -f1)
evil_sha=$(sha256sum "$attack/evil.rs" | cut -d' ' -f1)
echo "pinned binding $pinned_sha, attacker's $evil_sha"

build() {
  local package=$1
  shift
  (cd "$ROOT" && env "$@" cargo build --profile lab --locked -p "$package" -j "$JOBS") > "$WORK/log" 2>&1
}
# The binding the v8 crate compiled, as its build script handed it to rustc.
compiled() {
  local out
  out=$(find "$CARGO_TARGET_DIR/lab/build" -path '*/v8-*/output' -printf '%T@ %p\n' | sort -n | tail -1 | cut -d' ' -f2)
  sed -n 's/^cargo:rustc-env=RUSTY_V8_SRC_BINDING_PATH=//p' "$out"
}
# The copy in the extracted crate, which the v8 build script writes for a
# mirror: in its registry source, or in its git checkout when Cargo.toml
# patches v8 from git (a checkout ships no binding, so it may be absent).
extracted() {
  local manifest
  manifest=$(cd "$ROOT" && cargo metadata --locked --format-version 1 |
    python3 -I -c 'import json, sys; print(next(p["manifest_path"] for p in json.load(sys.stdin)["packages"] if p["name"] == "v8"))')
  echo "$(dirname "$manifest")/gen/$BINDING"
}
# A file's SHA-256, or "absent".
state() { if [[ -f $1 ]]; then sha256sum "$1" | cut -d' ' -f1; else echo absent; fi; }

failures=0
fail() { echo "FAIL  $*"; failures=$((failures + 1)); }
in_guard() { grep -q "failed to run custom build command for \`celld" "$WORK/log"; }
# refused NAME VAR=VALUE...: celld's guard fails the build, naming NAME.
refused() {
  local name=$1
  shift
  if build celld "$@"; then
    fail "$* : the build passed"
  elif in_guard && grep -q "unset .*$name" "$WORK/log"; then
    echo "ok    $* : refused ($(grep -o 'unset .*' "$WORK/log" | head -1))"
  else
    fail "$* : failed, but not in celld's guard:"; grep -E '^error|panicked' "$WORK/log" | head -5
  fi
}
# passes VAR=VALUE...: the build passes, compiling the pinned binding.
passes() {
  if ! build celld "$@"; then
    fail "${*:-(nothing set)} : the build failed:"; grep -E '^error|panicked|not the pinned' "$WORK/log" | head -5
  elif [[ $(compiled) != "$PINNED" ]]; then
    fail "${*:-(nothing set)} : the v8 crate compiled $(compiled), not $PINNED"
  else
    echo "ok    ${*:-(nothing set)} : passes, compiling $(compiled)"
  fi
}

echo "== a first build, on an empty CARGO_HOME"
passes
"$CARGO_TARGET_DIR/lab/celld" --version

echo "== each variable, refused"
refused RUSTY_V8_MIRROR RUSTY_V8_MIRROR="$attack/mirror"
refused RUSTY_V8_SRC_BINDING_URL RUSTY_V8_SRC_BINDING_URL="$attack/evil.rs"
refused DENO_TRYBUILD DENO_TRYBUILD=1
# Present but empty: the v8 build script reads these as off.
refused V8_FROM_SOURCE V8_FROM_SOURCE=
refused V8_FORCE_DEBUG V8_FORCE_DEBUG=

echo "== pins from the environment are ignored"
passes RUSTY_V8_SRC_BINDING_PATH="$attack/evil.rs" CELLD_V8_BINDING_SHA256="$evil_sha" \
  RUSTY_V8_ARCHIVE="$attack/mirror/librusty_v8_release_${TARGET}.a.gz" \
  RUSTY_V8_ARCHIVE_SHA256=0000000000000000000000000000000000000000000000000000000000000000

echo "== a poisoned extracted crate is never compiled"
# A build of v8 alone with a mirror set (any build sharing this cache can do
# that): the pinned binding path wins, so nothing is downloaded or written.
target_binding=$(extracted)
before=$(state "$target_binding")
if build v8 RUSTY_V8_MIRROR="$attack/mirror" && [[ $(state "$target_binding") == "$before" ]]; then
  echo "ok    v8 built with a mirror set left the extracted binding alone ($before)"
else
  fail "v8 built with a mirror set: $(tail -1 "$WORK/log"); extracted binding $before, now $(state "$target_binding")"
fi
# Poison it by hand, as another tool could (planting one in a git checkout).
if [[ ! -f $target_binding ]]; then
  mkdir -p "$(dirname "$target_binding")"
  cp "$attack/pinned.rs" "$target_binding"
fi
echo "// guard-test: tampered" >> "$target_binding"
echo "      extracted binding $target_binding now $(state "$target_binding")"
passes

echo "== a changed repository binding fails, naming it"
echo "// guard-test: tampered" >> "$PINNED"
if build celld; then
  fail "a changed $PINNED : the build passed"
elif in_guard && grep -q "the v8 binding $PINNED is not the pinned one" "$WORK/log"; then
  echo "ok    a changed binding : refused ($(grep -o 'the v8 binding .*pinned [0-9a-f]*)' "$WORK/log" | head -1))"
else
  fail "a changed binding : failed, but not in celld's guard:"; grep -E '^error|panicked' "$WORK/log" | head -5
fi
cp "$attack/pinned.rs" "$PINNED"
passes
"$CARGO_TARGET_DIR/lab/celld" --version

if ((failures)); then
  echo "guard-test: $failures failed"
  exit 1
fi
echo "guard-test: all passed"
