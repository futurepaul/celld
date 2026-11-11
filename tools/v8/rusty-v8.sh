#!/usr/bin/env bash
# The V8 that celld links: check it, rebuild it, publish it, pin it.
# The why and the steps are in docs/v8.md.
#
#   tools/v8/rusty-v8.sh check            name the newest V8 patch; exit 10 behind, 11 branch out of support
#   tools/v8/rusty-v8.sh build [V8]       rusty_v8 (the crate version in Cargo.lock) on V8 patch V8
#   tools/v8/rusty-v8.sh pin              write the build's archive URL, SHA-256 and version to .cargo/config.toml
#   tools/v8/rusty-v8.sh verify-local     build and test celld against the local archive
#   tools/v8/rusty-v8.sh publish          upload the build as a release on $CELLD_V8_REPO, check the round trip
#   tools/v8/rusty-v8.sh verify           build and test celld against the published archive
#   tools/v8/rusty-v8.sh update [V8]      all of the above, in that order
#
# Environment: CELLD_V8_WORK (default ~/.cache/celld-v8), CELLD_V8_JOBS
# (default 16; other work shares the box), CELLD_V8_REPO (default
# futurepaul/celld), CELLD_TARGET_DIR (celld's target dir for verify).
set -euo pipefail

ROOT=$(git -C "$(dirname "$0")" rev-parse --show-toplevel)
CONFIG="$ROOT/.cargo/config.toml"
REPO=${CELLD_V8_REPO:-futurepaul/celld}
WORK=${CELLD_V8_WORK:-${XDG_CACHE_HOME:-$HOME/.cache}/celld-v8}
JOBS=${CELLD_V8_JOBS:-16}
CHROMIUM_V8=https://chromium.googlesource.com/v8/v8
DENO_V8=https://github.com/denoland/v8
RUSTY_V8=https://github.com/denoland/rusty_v8
CHROMIUMDASH=https://chromiumdash.appspot.com/fetch_releases
TARGET=$(rustc -vV | sed -n 's/^host: //p')
ARCHIVE="librusty_v8_release_${TARGET}.a.gz"
BINDING="src_binding_release_${TARGET}.rs"

die() { echo "rusty-v8: $*" >&2; exit 1; }
say() { echo "== $*" >&2; }

# The v8 crate version celld locks; the rusty_v8 tag is v<this>.
crate_version() {
  awk '/^name = "v8"$/ { getline; gsub(/version = |"/, ""); print; exit }' "$ROOT/Cargo.lock"
}

# A value from .cargo/config.toml's [env] table, or empty.
pinned() {
  [[ -f $CONFIG ]] || return 0
  sed -n "s/^$1 = { value = \"\([^\"]*\)\".*/\1/p" "$CONFIG"
}

is_version() { [[ $1 =~ ^[0-9]+\.[0-9]+\.[0-9]+\.[0-9]+$ ]]; }

# V8's patch tags on one branch build (e.g. 15.2.124), oldest first.
patch_tags() {
  git ls-remote --tags "$CHROMIUM_V8" "refs/tags/$1.*" |
    sed -n 's|.*refs/tags/\([0-9.]*\)$|\1|p' | sort -V
}

# The V8 version a v8-version.h names.
header_version() {
  awk '/define V8_(MAJOR_VERSION|MINOR_VERSION|BUILD_NUMBER|PATCH_LEVEL) /{v = v (v ? "." : "") $3} END {print v}' "$1"
}

# The V8 version the v8 crate $1 embeds, from its source in the cargo registry.
release_v8_version() {
  local h
  h=$(find "${CARGO_HOME:-$HOME/.cargo}/registry/src" -path "*/v8-$1/v8/include/v8-version.h" | head -1)
  [[ -n $h ]] || die "no v8-$1 crate source in the cargo registry; run cargo fetch in $ROOT"
  header_version "$h"
}

cmd_check() {
  local crate ours branch build newest stable extended lag status=0
  crate=$(crate_version)
  ours=$(pinned CELLD_V8_VERSION)
  [[ -n $ours ]] || ours=$(release_v8_version "$crate")
  is_version "$ours" || die "can't tell which V8 celld links (got '$ours')"
  branch=${ours%.*.*}
  build=${ours%.*}
  newest=$(patch_tags "$build" | tail -1)
  is_version "$newest" || die "no V8 tags found for $build"
  stable=$(curl -fsS "$CHROMIUMDASH?channel=Stable&platform=Linux&num=1" | python3 -I -c 'import json,sys; print(json.load(sys.stdin)[0]["milestone"])')
  extended=$(curl -fsS "$CHROMIUMDASH?channel=Extended&platform=Windows&num=1" | python3 -I -c 'import json,sys; print(json.load(sys.stdin)[0]["milestone"])')
  # Chrome milestone M ships V8 branch M/10 . M%10 (Chrome 152 is V8 15.2).
  local mine=$(( ${branch%.*} * 10 + ${branch#*.} ))
  echo "celld links V8 $ours (rusty_v8 $crate, ${RUSTY_V8#https://github.com/})."
  echo "Newest patch on V8 $branch: $newest."
  echo "Chrome stable is $stable (V8 $((stable / 10)).$((stable % 10))); extended stable is $extended (V8 $((extended / 10)).$((extended % 10)))."
  if [[ $newest != "$ours" ]]; then
    lag=$(patch_tags "$build" | awk -v o="$ours" 'f {n++} $0 == o {f = 1} END {print n + 0}')
    echo "BEHIND: $lag patch(es) on $branch since ours. Run: tools/v8/rusty-v8.sh update $newest"
    status=10
  fi
  if (( mine < extended )); then
    echo "OUT OF SUPPORT: Chrome no longer merges security fixes to V8 $branch (oldest supported is $((extended / 10)).$((extended % 10))). Moving needs a newer rusty_v8: see docs/v8.md."
    (( status == 0 )) && status=11
  fi
  (( status == 0 )) && echo "Current."
  return $status
}

# rusty_v8's submodules, less tools/win (Windows-only; update = none upstream).
submodule_paths() {
  git -C "$1" config -f .gitmodules --get-regexp '^submodule\..*\.path$' | awk '$2 != "tools/win" {print $2}'
}

# Every git dependency V8's DEPS file at revision $2 pins, as "path rev".
deps_at() {
  git -C "$1" show "$2:DEPS" | python3 -I -c '
import sys
src = sys.stdin.read()
scope = {"Str": str, "Var": lambda name: scope["vars"][name]}
exec(src, scope)
for path, dep in scope["deps"].items():
    url = dep if isinstance(dep, str) else dep.get("url")
    if url and "@" in url:
        print(path, url.rsplit("@", 1)[1])
'
}

cmd_build() {
  local crate want src v8 pin base base_version patches branch build
  crate=$(crate_version)
  [[ -n $crate ]] || die "no v8 crate in Cargo.lock"
  want=${1:-}
  src="$WORK/src/rusty_v8-v$crate"
  v8="$src/v8"

  if [[ ! -d $src/.git ]]; then
    say "cloning rusty_v8 v$crate"
    mkdir -p "$WORK/src"
    git clone -q --depth 1 --branch "v$crate" "$RUSTY_V8" "$src.part"
    # v8 needs its patches' history down to the upstream version commit.
    git -C "$src.part" submodule update -q --init --depth 30 v8
    submodule_paths "$src.part" | grep -vx v8 |
      xargs git -C "$src.part" submodule update -q --init --recursive --depth 1 --jobs 8 --
    mv "$src.part" "$src"
  fi
  [[ $(git -C "$src" describe --tags --exact-match HEAD) == "v$crate" ]] || die "$src is not at v$crate"
  [[ -z $(git -C "$src" status --porcelain --untracked-files=no --ignore-submodules=all) ]] || die "$src has local changes"
  # Submodules move only where they must, so a second run rebuilds nothing.
  local sub
  for sub in $(submodule_paths "$src"); do
    [[ -z $(git -C "$src/$sub" status --porcelain --untracked-files=no) ]] || die "$src/$sub has local changes"
  done

  # The release's V8 is an upstream "Version x.y.z.w" commit plus Deno's
  # patches; carry the same patches onto the upstream tag we want.
  pin=$(git -C "$src" ls-tree HEAD v8 | awk '{print $3}')
  git -C "$v8" cat-file -e "$pin^{commit}" 2>/dev/null || git -C "$v8" fetch -q --depth 30 origin "$pin"
  read -r base base_version < <(git -C "$v8" log --format='%H %s' "$pin" |
    awk '$2 == "Version" && $3 ~ /^[0-9]+\.[0-9]+\.[0-9]+\.[0-9]+$/ && NF == 3 {print $1, $3; exit}')
  is_version "${base_version:-}" || die "no upstream Version commit under the release's V8 $pin"
  mapfile -t patches < <(git -C "$v8" rev-list --reverse "$base..$pin")
  (( ${#patches[@]} > 0 && ${#patches[@]} < 20 )) || die "unexpected Deno patch count ${#patches[@]} on $base_version"
  [[ -n $want ]] || want=$(patch_tags "${base_version%.*}" | tail -1)
  is_version "$want" || die "not a V8 version: '$want'"
  branch=${want%.*.*}
  build=${want%.*}
  [[ ${base_version%.*} == "$build" ]] || die "V8 $want is not on the release's branch build ${base_version%.*}; a different branch needs a different rusty_v8 (docs/v8.md)"
  say "rusty_v8 v$crate: V8 $base_version + ${#patches[@]} Deno patches -> $want"
  git -C "$v8" fetch -q --depth 1 "$CHROMIUM_V8" "+refs/tags/$want:refs/tags/$want"
  git -C "$v8" fetch -q --depth 1 "$CHROMIUM_V8" "+refs/tags/$base_version:refs/tags/$base_version"
  [[ $(git -C "$v8" rev-parse "$base_version^{commit}") == "$base" ]] || die "the release's base is not upstream's $base_version"
  # Patch in a side worktree, with a fixed identity and date so the same
  # inputs give the same commit, then check out only what differs.
  local side="$WORK/v8-patching" patched
  git -C "$v8" worktree remove --force "$side" 2>/dev/null || rm -rf "$side"
  git -C "$v8" worktree add -q --detach "$side" "$want"
  GIT_COMMITTER_NAME="celld rusty-v8.sh" GIT_COMMITTER_EMAIL="celld@localhost" \
    GIT_COMMITTER_DATE="$(git -C "$v8" log -1 --format=%cI "$want")" \
    git -C "$side" cherry-pick --allow-empty "${patches[@]}" >/dev/null
  patched=$(git -C "$side" rev-parse HEAD)
  git -C "$v8" worktree remove --force "$side"
  git -C "$v8" checkout -q --detach "$patched"
  local header
  header=$(header_version "$v8/include/v8-version.h")
  [[ $header == "$want" ]] || die "v8-version.h says $header, wanted $want"

  # Follow V8's DEPS, like rusty_v8's tools/update_deps.py. build carries
  # rusty_v8's own patches, so a moved build revision stops here.
  local -A old new
  local path rev pinned_rev
  while read -r path rev; do old[$path]=$rev; done < <(deps_at "$v8" "$base")
  while read -r path rev; do new[$path]=$rev; done < <(deps_at "$v8" "$want")
  [[ ${old[build]:-} == "${new[build]:-}" ]] || die "V8 $want moves //build (${old[build]:-} -> ${new[build]:-}); denoland/chromium_build needs a rebase first"
  local moved=()
  for path in $(submodule_paths "$src"); do
    [[ $path == v8 ]] && continue
    pinned_rev=$(git -C "$src" ls-tree HEAD "$path" | awk '{print $3}')
    rev=$pinned_rev
    [[ $path == build || -z ${new[$path]:-} ]] || rev=${new[$path]}
    [[ $rev == "$pinned_rev" ]] || moved+=("$path@$rev")
    if [[ $(git -C "$src/$path" rev-parse HEAD) != "$rev" ]]; then
      git -C "$src/$path" cat-file -e "$rev^{commit}" 2>/dev/null || git -C "$src/$path" fetch -q --depth 1 origin "$rev"
      git -C "$src/$path" checkout -q --detach "$rev"
    fi
  done
  say "dependencies moved to V8 $want's DEPS: ${moved[*]:-none}"

  # Cross-check: denoland's own branch for this V8, when it has the same tree.
  local deno_tree="" deno_head=""
  deno_head=$(git ls-remote "$DENO_V8" "refs/heads/$branch-lkgr-denoland" | cut -f1)
  if [[ -n $deno_head ]]; then
    git -C "$v8" fetch -q --depth 1 "$DENO_V8" "$deno_head" &&
      deno_tree=$(git -C "$v8" rev-parse "$deno_head^{tree}")
  fi
  local ours_tree
  ours_tree=$(git -C "$v8" rev-parse "HEAD^{tree}")
  if [[ $deno_tree == "$ours_tree" ]]; then
    say "the patched V8 tree matches denoland/v8 $branch-lkgr-denoland ($deno_head)"
  else
    say "note: the patched V8 tree differs from denoland/v8 $branch-lkgr-denoland (${deno_head:-absent}); review before publishing"
  fi

  # rusty_v8's own build: V8_FROM_SOURCE, its gn args, and Chromium's pinned
  # clang and Rust. bindgen needs a libclang of 21.1 or later; Chromium ships
  # none for Linux, so it is the host's (CELLD_V8_LIBCLANG), and the binding
  # it generates is checked against the crate's below.
  local libclang=${CELLD_V8_LIBCLANG:-$(llvm-config --libdir 2>/dev/null || true)}
  [[ -e $libclang/libclang.so ]] || die "no libclang.so in '$libclang'; set CELLD_V8_LIBCLANG to a directory with libclang 21.1 or later"
  say "building with $JOBS jobs (load: $(cut -d' ' -f1-3 /proc/loadavg))"
  (cd "$src" && env -u CARGO_TARGET_DIR -u RUSTFLAGS -u RUSTY_V8_ARCHIVE -u RUSTY_V8_MIRROR \
      V8_FROM_SOURCE=1 LIBCLANG_PATH="$libclang" \
      cargo build --release --locked --lib -j "$JOBS" --target "$TARGET")

  local out="$src/target/$TARGET/release/gn_out" dist="$WORK/dist/v$crate-$want"
  rm -rf "$dist" && mkdir -p "$dist"
  gzip -9 -n -c "$out/obj/librusty_v8.a" > "$dist/$ARCHIVE"
  cp "$out/src_binding.rs" "$dist/$BINDING"
  grep -aq "$want" "$out/obj/librusty_v8.a" || die "the archive does not carry the string $want"

  # celld compiles the crate's own (checksummed) binding. bindgen makes it
  # from V8's public headers and rusty_v8's sources, so it fits this archive
  # when those are the release's but for the patch level. (The generated
  # file varies with the host's libclang, so it is published, not compared.)
  local headers
  headers=$(git -C "$v8" diff --name-only "$pin" HEAD -- include/)
  [[ -z $headers || $headers == include/v8-version.h ]] || die "V8's public headers changed: $(echo $headers); the crate's binding may not fit"
  if git -C "$v8" diff "$pin" HEAD -- include/v8-version.h | grep '^[-+]#define' | grep -qv ' V8_PATCH_LEVEL '; then
    die "v8-version.h changed beyond the patch level"
  fi
  say "V8's public headers are the release's but for V8_PATCH_LEVEL"

  {
    echo "rusty_v8: v$crate ($RUSTY_V8)"
    echo "V8: $want (upstream $CHROMIUM_V8 tag $want, $(git -C "$v8" rev-parse "$want^{commit}"))"
    echo "V8 with Deno's patches: $(git -C "$v8" rev-parse HEAD) (tree $ours_tree)"
    echo "Deno patches, carried from the release's V8 $pin on $base_version:"
    git -C "$v8" log --format='  %h %s' "$want..HEAD"
    echo "denoland/v8 $branch-lkgr-denoland: ${deno_head:-absent} ($( [[ $deno_tree == "$ours_tree" ]] && echo same tree || echo different tree))"
    echo "Dependencies moved to V8's DEPS: ${moved[*]:-none}"
    echo "Target: $TARGET; profile: release; features: default (use_custom_libcxx)"
    echo "Clang: $(sed -n "s/^CLANG_REVISION = '\(.*\)'/\1/p" "$src/tools/clang/scripts/update.py") (Chromium's); bindgen's libclang: $(basename "$(readlink -f "$libclang/libclang.so")")"
    echo "Rust (rusty_v8's toolchain): $(sed -n 's/^channel = "\(.*\)"/\1/p' "$src/rust-toolchain.toml")"
    echo "gn args:"
    sed 's/^/  /' "$out/args.gn"
    echo "Built: $(date -u +%FT%TZ) on $(uname -srm) by tools/v8/rusty-v8.sh at celld $(git -C "$ROOT" rev-parse HEAD)"
  } > "$dist/BUILDINFO"
  (cd "$dist" && sha256sum "$ARCHIVE" "$BINDING" > SHA256SUMS)
  ln -sfn "$dist" "$WORK/dist/latest"
  say "built $dist"
  cat "$dist/SHA256SUMS" >&2
}

dist_dir() {
  local d="$WORK/dist/latest"
  [[ -f $d/BUILDINFO ]] || die "no build in $d; run build first"
  readlink -f "$d"
}
dist_v8() { sed -n 's/^V8: \([0-9.]*\) .*/\1/p' "$1/BUILDINFO"; }
dist_crate() { sed -n 's/^rusty_v8: v\([0-9.]*\) .*/\1/p' "$1/BUILDINFO"; }
tag_for() { echo "rusty_v8-v$1-v8-$2"; }

cmd_pin() {
  local dist crate v8 sha url
  dist=$(dist_dir)
  crate=$(dist_crate "$dist")
  v8=$(dist_v8 "$dist")
  [[ $crate == "$(crate_version)" ]] || die "the build is rusty_v8 $crate, Cargo.lock has $(crate_version)"
  sha=$(sha256sum "$dist/$ARCHIVE" | cut -d' ' -f1)
  url="https://github.com/$REPO/releases/download/$(tag_for "$crate" "$v8")/$ARCHIVE"
  mkdir -p "$(dirname "$CONFIG")"
  cat > "$CONFIG" <<EOF
# celld links V8 from our own rusty_v8 build, not denoland's prebuilt
# archive. tools/v8/rusty-v8.sh pin writes this file; docs/v8.md says why.
#
# The archive is fetched from RUSTY_V8_ARCHIVE and checked against
# RUSTY_V8_ARCHIVE_SHA256 before use. Only the URL may be overridden from
# the environment (to try a local file with the same hash); the hash and the
# version are forced, and building V8 from source or skipping the download
# are turned off. Only $TARGET has an archive; other targets fail the hash.
[env]
CELLD_V8_VERSION = { value = "$v8", force = true }
RUSTY_V8_ARCHIVE = { value = "$url" }
RUSTY_V8_ARCHIVE_SHA256 = { value = "$sha", force = true }
V8_FROM_SOURCE = { value = "0", force = true }
RUSTY_V8_SKIP_DOWNLOAD = { value = "0", force = true }
EOF
  say "pinned V8 $v8: $url sha256 $sha"
}

# Build celld and run its tests; $1 is an archive override or empty.
celld_test() {
  local target_dir=${CELLD_TARGET_DIR:-$ROOT/target} override=()
  [[ -z $1 ]] || override=("RUSTY_V8_ARCHIVE=$1")
  (cd "$ROOT" && env "${override[@]}" CARGO_TARGET_DIR="$target_dir" \
      cargo test --profile lab --locked -j "$JOBS")
  local v
  v=$("$target_dir/lab/celld" --version)
  echo "$v"
  [[ $v == *"(V8 $(pinned CELLD_V8_VERSION)-rusty)" ]] || die "celld --version does not report V8 $(pinned CELLD_V8_VERSION)"
}

cmd_verify_local() {
  local dist
  dist=$(dist_dir)
  [[ $(pinned RUSTY_V8_ARCHIVE_SHA256) == "$(sha256sum "$dist/$ARCHIVE" | cut -d' ' -f1)" ]] || die "pin this build first"
  celld_test "$dist/$ARCHIVE"
}

cmd_publish() {
  local dist crate v8 tag head
  dist=$(dist_dir)
  crate=$(dist_crate "$dist")
  v8=$(dist_v8 "$dist")
  tag=$(tag_for "$crate" "$v8")
  head=$(git -C "$ROOT" rev-parse HEAD)
  git -C "$ROOT" fetch -q origin
  [[ -n $(git -C "$ROOT" branch -r --contains "$head") ]] || die "push $head first: the release tag points at it"
  if gh release view "$tag" -R "$REPO" >/dev/null 2>&1; then
    die "release $tag exists; a published archive is never replaced (bump the V8 patch or delete it deliberately)"
  fi
  gh release create "$tag" -R "$REPO" --target "$head" --latest=false \
    --title "rusty_v8 $crate on V8 $v8 ($TARGET)" --notes-file "$dist/BUILDINFO" \
    "$dist/$ARCHIVE" "$dist/$BINDING" "$dist/SHA256SUMS" "$dist/BUILDINFO"
  # Round trip: what GitHub serves is what we built.
  local check
  check=$(mktemp -d)
  gh release download "$tag" -R "$REPO" -D "$check" -p "$ARCHIVE" -p SHA256SUMS
  (cd "$check" && grep " $ARCHIVE\$" SHA256SUMS | sha256sum -c -) || die "the published archive does not match"
  cmp -s "$check/$ARCHIVE" "$dist/$ARCHIVE" || die "the published archive differs from the build"
  rm -rf "$check"
  say "published $tag"
}

cmd_verify() {
  [[ -n $(pinned RUSTY_V8_ARCHIVE) ]] || die "nothing pinned"
  curl -fsSIL -o /dev/null "$(pinned RUSTY_V8_ARCHIVE)" || die "$(pinned RUSTY_V8_ARCHIVE) is not reachable"
  celld_test ""
}

case ${1:-} in
  check) cmd_check ;;
  build) cmd_build "${2:-}" ;;
  pin) cmd_pin ;;
  verify-local) cmd_verify_local ;;
  publish) cmd_publish ;;
  verify) cmd_verify ;;
  update)
    cmd_build "${2:-}"
    cmd_pin
    cmd_verify_local
    cmd_publish
    cmd_verify
    say "done: commit .cargo/config.toml and open a PR with $(dist_dir)/BUILDINFO"
    ;;
  *) sed -n '2,17p' "$0" | sed 's/^# \{0,1\}//'; exit 2 ;;
esac
