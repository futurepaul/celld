#!/usr/bin/env bash
# Proves the V8 pin's guard (crates/celld/build.rs, docs/v8.md) with real
# builds. celld's build must fail:
#   - when a variable that brings other V8 code in is set, even where the v8
#     build script accepts it (a mirror, URL or path serving a binding with a
#     marker line added, as an attacker's would);
#   - when the binding the v8 crate compiles is not the pinned one, as after a
#     build with a mirror poisoned the extracted crate (Codex round 2);
# and pass once the crate is extracted again, and with nothing set.
#
# It runs against a private CARGO_HOME (your registry's index and .crate
# files copied in, the sources extracted afresh) and a private target dir,
# so your own registry is never poisoned.
#
#   tools/v8/guard-test.sh        (CELLD_GUARD_WORK, default target/guard-test;
#                                  CELLD_V8_JOBS, default 16)
set -euo pipefail

ROOT=$(git -C "$(dirname "$0")" rev-parse --show-toplevel)
JOBS=${CELLD_V8_JOBS:-16}
WORK=${CELLD_GUARD_WORK:-$ROOT/target/guard-test}
TARGET=$(rustc -vV | sed -n 's/^host: //p')
BINDING="src_binding_release_${TARGET}.rs"
crate=$(awk '/^name = "v8"$/ { getline; gsub(/version = |"/, ""); print; exit }' "$ROOT/Cargo.lock")
checksum=$(awk -v v="version = \"$crate\"" '/^name = "v8"$/ { getline; if ($0 == v) { getline; getline; gsub(/checksum = |"/, ""); print; exit } }' "$ROOT/Cargo.lock")
real_home=${CARGO_HOME:-$HOME/.cargo}
home="$WORK/cargo-home"
attack="$WORK/attack"

rm -rf "$home" "$attack"
mkdir -p "$home/registry" "$attack/mirror"
cp -a --reflink=auto "$real_home/registry/index" "$real_home/registry/cache" "$home/registry/"
export CARGO_HOME="$home" CARGO_TARGET_DIR="$WORK/target"

# The crate's own binding, from the .crate whose hash Cargo.lock records.
crate_file=
for file in "$home"/registry/cache/*/"v8-$crate.crate"; do
  [[ -f $file && $(sha256sum "$file" | cut -d' ' -f1) == "$checksum" ]] && crate_file=$file
done
[[ -n $crate_file ]] || { echo "guard-test: no v8-$crate.crate matching Cargo.lock; run cargo fetch" >&2; exit 1; }
tar -xzOf "$crate_file" "v8-$crate/gen/$BINDING" > "$attack/pristine.rs"
{ cat "$attack/pristine.rs"; echo "// guard-test: not the crate's binding"; } > "$attack/evil.rs"
cp "$attack/evil.rs" "$attack/mirror/$BINDING"
pristine_sha=$(sha256sum "$attack/pristine.rs" | cut -d' ' -f1)
evil_sha=$(sha256sum "$attack/evil.rs" | cut -d' ' -f1)
echo "pristine binding $pristine_sha, attacker's $evil_sha"

build() {
  local package=$1
  shift
  (cd "$ROOT" && env "$@" cargo build --profile lab --locked --offline -p "$package" -j "$JOBS") > "$WORK/log" 2>&1
}
# The binding the v8 crate compiles: the file in the extracted crate.
extracted() { find "$home/registry/src" -path "*/v8-$crate/gen/$BINDING" 2>/dev/null | head -1; }
extracted_sha() { local f; f=$(extracted); [[ -n $f ]] && sha256sum "$f" | cut -d' ' -f1; }

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
# passes VAR=VALUE...: the build passes.
passes() {
  if build celld "$@"; then
    echo "ok    ${*:-(nothing set)} : passes"
  else
    fail "${*:-(nothing set)} : the build failed:"; grep -E '^error|panicked|not the pinned' "$WORK/log" | head -5
  fi
}

echo "== a clean build"
passes

echo "== each variable, refused"
refused RUSTY_V8_MIRROR RUSTY_V8_MIRROR="$attack/mirror"
refused RUSTY_V8_SRC_BINDING_URL RUSTY_V8_SRC_BINDING_URL="$attack/evil.rs"
refused RUSTY_V8_SRC_BINDING_PATH RUSTY_V8_SRC_BINDING_PATH="$attack/evil.rs"
# Alone, DENO_TRYBUILD stops the v8 build script itself; with a binding path
# it skips the archive and its hash, and only the guard stops it.
refused DENO_TRYBUILD DENO_TRYBUILD=1 RUSTY_V8_SRC_BINDING_PATH="$attack/pristine.rs"
# Present but empty: the v8 build script reads these as off.
refused V8_FROM_SOURCE V8_FROM_SOURCE=
refused V8_FORCE_DEBUG V8_FORCE_DEBUG=

echo "== a refused build's poisoned binding, then nothing set"
# The v8 build script writes a mirror's binding into the extracted crate. A
# refused celld build may or may not get that far, so poison it the way any
# build sharing this cache can: v8 alone, with the mirror set.
if build v8 RUSTY_V8_MIRROR="$attack/mirror" && [[ $(extracted_sha) == "$evil_sha" ]]; then
  echo "ok    the extracted binding is now the attacker's ($(extracted_sha))"
else
  fail "could not poison the extracted binding (it is $(extracted_sha))"
fi
if build celld; then
  fail "nothing set, poisoned binding $(extracted_sha) : the build passed"
elif in_guard && grep -q "is not the pinned one" "$WORK/log"; then
  echo "ok    nothing set, poisoned binding : refused ($(grep -o "the v8 crate's binding .*pinned [0-9a-f]*)" "$WORK/log" | head -1))"
else
  fail "nothing set, poisoned binding : failed, but not in celld's guard:"; grep -E '^error|panicked' "$WORK/log" | head -5
fi

echo "== the crate extracted again"
rm -rf "$(dirname "$(dirname "$(extracted)")")"
passes
if [[ $(extracted_sha) == "$pristine_sha" ]]; then
  echo "ok    the extracted binding is the crate's ($(extracted_sha))"
else
  fail "the extracted binding is $(extracted_sha), not the crate's"
fi
"$CARGO_TARGET_DIR/lab/celld" --version

echo "== the pin is forced"
# An archive and hash from the environment are ignored (honored, this one
# would fail the hash).
passes RUSTY_V8_ARCHIVE="$attack/mirror/librusty_v8_release_${TARGET}.a.gz" \
  RUSTY_V8_ARCHIVE_SHA256=0000000000000000000000000000000000000000000000000000000000000000 \
  CELLD_V8_BINDING_SHA256="$evil_sha"

if ((failures)); then
  echo "guard-test: $failures failed"
  exit 1
fi
echo "guard-test: all passed"
