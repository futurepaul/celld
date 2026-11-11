#!/usr/bin/env bash
# Proves the V8 pin's guard (crates/celld/build.rs, docs/v8.md): celld's
# build fails when any variable that brings other V8 code in is set, even
# where the v8 build script accepts it, and passes when none is. Each case
# is a real `cargo build`; a mirror, URL or path serves a binding that is
# not the crate's (a marker line added), as an attacker's would.
#
#   tools/v8/guard-test.sh        (CELLD_TARGET_DIR, CELLD_V8_JOBS as for rusty-v8.sh)
set -euo pipefail

ROOT=$(git -C "$(dirname "$0")" rev-parse --show-toplevel)
JOBS=${CELLD_V8_JOBS:-16}
TARGET_DIR=${CELLD_TARGET_DIR:-$ROOT/target}
TARGET=$(rustc -vV | sed -n 's/^host: //p')
BINDING="src_binding_release_${TARGET}.rs"
crate=$(awk '/^name = "v8"$/ { getline; gsub(/version = |"/, ""); print; exit }' "$ROOT/Cargo.lock")
genuine=$(find "${CARGO_HOME:-$HOME/.cargo}/registry/src" -path "*/v8-$crate/gen/$BINDING" | head -1)
[[ -n $genuine ]] || { echo "guard-test: no v8-$crate source; run cargo fetch" >&2; exit 1; }

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
mkdir -p "$work/mirror"
{ cat "$genuine"; echo "// guard-test: not the crate's binding"; } > "$work/evil.rs"
cp "$work/evil.rs" "$work/mirror/$BINDING"

build() {
  (cd "$ROOT" && env "$@" CARGO_TARGET_DIR="$TARGET_DIR" \
    cargo build --profile lab --locked -p celld -j "$JOBS") > "$work/log" 2>&1
}

failures=0
# refused NAME VAR=VALUE...: the build fails in celld's guard, naming NAME.
refused() {
  local name=$1
  shift
  if build "$@"; then
    echo "FAIL  $* : the build passed"
    failures=$((failures + 1))
  elif grep -q "failed to run custom build command for \`celld" "$work/log" &&
       grep -q "unset .*$name" "$work/log"; then
    echo "ok    $* : refused ($(grep -o 'unset .*' "$work/log" | head -1))"
  else
    echo "FAIL  $* : failed, but not in celld's guard:"
    grep -E '^error|panicked' "$work/log" | head -5
    failures=$((failures + 1))
  fi
}
# passes VAR=VALUE...: the build passes.
passes() {
  if build "$@"; then
    echo "ok    ${*:-(none set)} : passes"
  else
    echo "FAIL  ${*:-(none set)} : the build failed:"
    grep -E '^error|panicked' "$work/log" | head -5
    failures=$((failures + 1))
  fi
}

refused RUSTY_V8_MIRROR RUSTY_V8_MIRROR="$work/mirror"
refused RUSTY_V8_SRC_BINDING_URL RUSTY_V8_SRC_BINDING_URL="$work/evil.rs"
refused RUSTY_V8_SRC_BINDING_PATH RUSTY_V8_SRC_BINDING_PATH="$work/evil.rs"
# Alone, DENO_TRYBUILD stops the v8 build script itself; with a binding path
# it skips the archive and its hash, and only the guard stops it.
refused DENO_TRYBUILD DENO_TRYBUILD=1 RUSTY_V8_SRC_BINDING_PATH="$genuine"
# Present but empty: the v8 build script reads it as off; the guard refuses
# it all the same.
refused V8_FROM_SOURCE V8_FROM_SOURCE=
# The pin is forced: an archive and hash from the environment are ignored
# (honored, this one would fail the hash).
passes RUSTY_V8_ARCHIVE="$work/mirror/librusty_v8_release_${TARGET}.a.gz" \
  RUSTY_V8_ARCHIVE_SHA256=0000000000000000000000000000000000000000000000000000000000000000
passes

if ((failures)); then
  echo "guard-test: $failures failed"
  exit 1
fi
echo "guard-test: all passed"
