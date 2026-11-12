#!/usr/bin/env bash
# The V8 that celld links: check it, rebuild it, publish it, pin it.
# The why and the steps are in docs/v8.md.
#
#   tools/v8/rusty-v8.sh check            name the newest V8 patch; exit 10 behind (update can build it),
#                                         12 behind (denoland/v8 has not carried it yet), 11 branch out
#                                         of support
#   tools/v8/rusty-v8.sh build [V8]       rusty_v8 (the crate Cargo.lock locks: a release, or a commit
#                                         pinned by rev) on V8 patch V8, from source in rusty_v8's own
#                                         checkout (celld's build refuses that)
#   tools/v8/rusty-v8.sh verify-local     build and test celld against the local archive (the pin unchanged)
#   tools/v8/rusty-v8.sh publish          upload the build as a release on $CELLD_V8_REPO, download it, compare
#   tools/v8/rusty-v8.sh pin              check that the public URL serves the build, then write .cargo/config.toml
#   tools/v8/rusty-v8.sh verify           build and test celld against the pin
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

# The denoland/rusty_v8 commit celld locks, when Cargo.toml's [patch.crates-io]
# takes the v8 crate from git by rev (no release has its V8 yet); empty for a
# crates.io release. A git source with anything but a full commit is refused.
crate_rev() {
  local source
  source=$(awk '/^name = "v8"$/ { getline; getline; print; exit }' "$ROOT/Cargo.lock")
  case $source in
    'source = "registry+'*) ;;
    *) [[ $source =~ ^'source = "git+'"$RUSTY_V8"'?rev='([0-9a-f]{40})#([0-9a-f]{40})'"'$ &&
          ${BASH_REMATCH[1]} == "${BASH_REMATCH[2]}" ]] ||
         die "Cargo.lock's v8 is neither a crates.io release nor $RUSTY_V8 pinned by rev: $source"
       echo "${BASH_REMATCH[1]}" ;;
  esac
}

# The v8 crate's own binding for $TARGET, on stdout, read from the .crate
# archive whose hash Cargo.lock records, never from the extracted source,
# which the v8 build script overwrites when a binding mirror is set.
crate_binding() {
  local crate=$1 want file
  want=$(awk -v v="version = \"$1\"" '/^name = "v8"$/ { getline; if ($0 == v) { getline; getline; gsub(/checksum = |"/, ""); print; exit } }' "$ROOT/Cargo.lock")
  [[ ${#want} == 64 ]] || die "no checksum for v8 $crate in Cargo.lock"
  (cd "$ROOT" && cargo fetch --locked >/dev/null)
  for file in "${CARGO_HOME:-$HOME/.cargo}"/registry/cache/*/"v8-$crate.crate"; do
    [[ -f $file && $(sha256sum "$file" | cut -d' ' -f1) == "$want" ]] || continue
    tar -xzOf "$file" "v8-$crate/gen/$BINDING"
    return
  done
  die "no v8-$crate.crate with Cargo.lock's checksum $want in the cargo cache"
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

# The V8 version denoland/v8's branch for V8 branch $1 carries, or empty
# when denoland has no such branch.
deno_carries() {
  local header code
  header=$(mktemp)
  code=$(curl -sS -o "$header" -w '%{http_code}' \
    "${DENO_V8/github.com/raw.githubusercontent.com}/$1-lkgr-denoland/include/v8-version.h") || code=failed
  case $code in
    200) header_version "$header" ;;
    404) ;;
    *) rm -f "$header"; die "can't read denoland/v8 $1-lkgr-denoland (HTTP $code)" ;;
  esac
  rm -f "$header"
}

cmd_check() {
  local crate rev ours branch build newest deno stable extended lag status=0
  crate=$(crate_version)
  rev=$(crate_rev)
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
  echo "celld links V8 $ours (rusty_v8 $crate${rev:+ at commit $rev}, ${RUSTY_V8#https://github.com/})."
  echo "Newest patch on V8 $branch: $newest."
  echo "Chrome stable is $stable (V8 $((stable / 10)).$((stable % 10))); extended stable is $extended (V8 $((extended / 10)).$((extended % 10)))."
  # build carries Deno's patches only onto a V8 whose patched tree
  # denoland/v8 has, so the patch denoland's branch carries is the newest it
  # can build today.
  deno=$(deno_carries "$branch")
  echo "denoland/v8 $branch-lkgr-denoland carries ${deno:-no $branch}."
  if [[ $newest != "$ours" ]]; then
    lag=$(patch_tags "$build" | awk -v o="$ours" 'f {n++} $0 == o {f = 1} END {print n + 0}')
    if [[ -n $deno && $deno != "$ours" && $(printf '%s\n' "$ours" "$deno" | sort -V | tail -1) == "$deno" ]]; then
      echo "BEHIND: $lag patch(es) on $branch since ours. Run: tools/v8/rusty-v8.sh update $deno"
      status=10
    else
      echo "BEHIND: $lag patch(es) on $branch since ours, none of them on denoland/v8 yet; update builds once it carries them (docs/v8.md)."
      status=12
    fi
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
  local crate commit want src v8 pin base base_version patches branch build
  crate=$(crate_version)
  [[ -n $crate ]] || die "no v8 crate in Cargo.lock"
  commit=$(crate_rev)
  want=${1:-}
  src="$WORK/src/rusty_v8-${commit:-v$crate}"
  v8="$src/v8"

  if [[ ! -d $src/.git ]]; then
    say "cloning rusty_v8 ${commit:-v$crate}"
    mkdir -p "$WORK/src"
    rm -rf "$src.part"
    if [[ -n $commit ]]; then
      git init -q "$src.part"
      git -C "$src.part" remote add origin "$RUSTY_V8"
      git -C "$src.part" fetch -q --depth 1 origin "$commit"
      git -C "$src.part" checkout -q --detach "$commit"
    else
      git clone -q --depth 1 --branch "v$crate" "$RUSTY_V8" "$src.part"
    fi
    # v8 needs its patches' history down to the upstream version commit.
    git -C "$src.part" submodule update -q --init --depth 30 v8
    submodule_paths "$src.part" | grep -vx v8 |
      xargs git -C "$src.part" submodule update -q --init --recursive --depth 1 --jobs 8 --
    mv "$src.part" "$src"
  fi
  if [[ -n $commit ]]; then
    [[ $(git -C "$src" rev-parse HEAD) == "$commit" ]] || die "$src is not at $commit"
    [[ $(git -C "$src" show HEAD:Cargo.toml | sed -n 's/^version = "\(.*\)"$/\1/p' | head -1) == "$crate" ]] ||
      die "rusty_v8 $commit is not the v8 crate $crate"
  else
    [[ $(git -C "$src" describe --tags --exact-match HEAD) == "v$crate" ]] || die "$src is not at v$crate"
  fi
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
  say "rusty_v8 v$crate${commit:+ at $commit}: V8 $base_version + ${#patches[@]} Deno patches -> $want"
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
  # Cross-check: denoland's own branch for this V8 must have the same tree.
  # A different tree means different patches or a branch denoland has not
  # moved yet: a person decides, so the build stops before it touches the
  # build tree.
  local deno_tree="" deno_head="" ours_tree
  ours_tree=$(git -C "$v8" rev-parse "$patched^{tree}")
  deno_head=$(git ls-remote "$DENO_V8" "refs/heads/$branch-lkgr-denoland" | cut -f1)
  if [[ -n $deno_head ]]; then
    git -C "$v8" fetch -q --depth 1 "$DENO_V8" "$deno_head" &&
      deno_tree=$(git -C "$v8" rev-parse "$deno_head^{tree}")
  fi
  [[ -n $deno_tree && $deno_tree == "$ours_tree" ]] ||
    die "the patched V8 tree ($ours_tree) is not denoland/v8 $branch-lkgr-denoland's (${deno_head:-absent}, ${deno_tree:-no tree})"
  say "the patched V8 tree matches denoland/v8 $branch-lkgr-denoland ($deno_head)"
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

  # rusty_v8's own build: V8_FROM_SOURCE, its gn args, and Chromium's pinned
  # clang and Rust. bindgen needs a libclang of 21.1 or later; Chromium ships
  # none for Linux, so it is the host's (CELLD_V8_LIBCLANG), and the binding
  # it generates is checked against the crate's below.
  local libclang=${CELLD_V8_LIBCLANG:-$(llvm-config --libdir 2>/dev/null || true)}
  [[ -e $libclang/libclang.so ]] || die "no libclang.so in '$libclang'; set CELLD_V8_LIBCLANG to a directory with libclang 21.1 or later"
  say "building with $JOBS jobs (load: $(cut -d' ' -f1-3 /proc/loadavg))"
  (cd "$src" && env -u CARGO_TARGET_DIR -u RUSTFLAGS -u RUSTY_V8_ARCHIVE -u RUSTY_V8_ARCHIVE_SHA256 \
      -u RUSTY_V8_MIRROR -u RUSTY_V8_SRC_BINDING_URL -u RUSTY_V8_SRC_BINDING_PATH -u DENO_TRYBUILD \
      -u RUSTY_V8_SKIP_DOWNLOAD V8_FROM_SOURCE=1 LIBCLANG_PATH="$libclang" \
      cargo build --release --locked --lib -j "$JOBS" --target "$TARGET")

  local out="$src/target/$TARGET/release/gn_out" dist="$WORK/dist/v$crate-$want"
  rm -rf "$dist" && mkdir -p "$dist"
  gzip -9 -n -c "$out/obj/librusty_v8.a" > "$dist/$ARCHIVE"
  cp "$out/src_binding.rs" "$dist/$BINDING"
  grep -aq "$want" "$out/obj/librusty_v8.a" || die "the archive does not carry the string $want"

  # For a release, celld compiles the crate's own (checksummed) binding; for
  # a commit, which has no .crate, the one generated here. bindgen makes it
  # from V8's public headers and rusty_v8's sources, and the crate's Rust
  # code was written against those headers, so both fit this archive when
  # the headers are the crate's but for the patch level. (The generated file
  # varies with the host's libclang, so it is published, not compared.)
  local headers
  headers=$(git -C "$v8" diff --name-only "$pin" HEAD -- include/)
  [[ -z $headers || $headers == include/v8-version.h ]] || die "V8's public headers changed: ${headers//$'\n'/ }; the crate's binding may not fit"
  if git -C "$v8" diff "$pin" HEAD -- include/v8-version.h | grep '^[-+]#define' | grep -qv ' V8_PATCH_LEVEL '; then
    die "v8-version.h changed beyond the patch level"
  fi
  say "V8's public headers are the release's but for V8_PATCH_LEVEL"

  {
    if [[ -n $commit ]]; then
      echo "rusty_v8: v$crate ($RUSTY_V8 commit $commit, unreleased)"
    else
      echo "rusty_v8: v$crate ($RUSTY_V8)"
    fi
    echo "V8: $want (upstream $CHROMIUM_V8 tag $want, $(git -C "$v8" rev-parse "$want^{commit}"))"
    echo "V8 with Deno's patches: $(git -C "$v8" rev-parse HEAD) (tree $ours_tree)"
    echo "Deno patches, carried from the release's V8 $pin on $base_version:"
    git -C "$v8" log --format='  %h %s' "$want..HEAD"
    echo "denoland/v8 $branch-lkgr-denoland: $deno_head (same tree)"
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
dist_rev() { sed -n 's/^rusty_v8: v[0-9.]* (.* commit \([0-9a-f]\{40\}\), unreleased)$/\1/p' "$1/BUILDINFO"; }
tag_for() { echo "rusty_v8-v$1-v8-$2"; }

cmd_pin() {
  local dist crate rev built v8 sha url check binding_sha
  dist=$(dist_dir)
  crate=$(dist_crate "$dist")
  rev=$(crate_rev)
  built=$(dist_rev "$dist")
  v8=$(dist_v8 "$dist")
  [[ $crate == "$(crate_version)" ]] || die "the build is rusty_v8 $crate, Cargo.lock has $(crate_version)"
  [[ $built == "$rev" ]] || die "the build is rusty_v8 ${built:-v$crate}, Cargo.lock has ${rev:-v$crate}"
  sha=$(sha256sum "$dist/$ARCHIVE" | cut -d' ' -f1)
  url="https://github.com/$REPO/releases/download/$(tag_for "$crate" "$v8")/$ARCHIVE"
  # Only what the public URL serves is pinned, so the round trip comes first.
  check=$(mktemp -d)
  curl -fsSL --retry 3 -o "$check/$ARCHIVE" "$url" || die "$url is not published; run publish first"
  [[ $(sha256sum "$check/$ARCHIVE" | cut -d' ' -f1) == "$sha" ]] || die "$url does not serve this build"
  mkdir -p "$(dirname "$CONFIG")/v8"
  if [[ -n $rev ]]; then
    # A commit has no .crate, so its binding is the one this build generated,
    # as the release publishes it.
    curl -fsSL --retry 3 -o "$check/$BINDING" "${url%/*}/$BINDING" || die "${url%/*}/$BINDING is not published"
    cmp -s "$check/$BINDING" "$dist/$BINDING" || die "${url%/*}/$BINDING is not this build's binding"
    cp "$check/$BINDING" "$(dirname "$CONFIG")/v8/$BINDING.part"
  else
    crate_binding "$crate" > "$(dirname "$CONFIG")/v8/$BINDING.part"
  fi
  rm -rf "$check"
  mv "$(dirname "$CONFIG")/v8/$BINDING.part" "$(dirname "$CONFIG")/v8/$BINDING"
  binding_sha=$(sha256sum "$(dirname "$CONFIG")/v8/$BINDING" | cut -d' ' -f1)
  cat > "$CONFIG" <<EOF
# celld links V8 from our own rusty_v8 build, not denoland's prebuilt
# archive. tools/v8/rusty-v8.sh pin writes this file; docs/v8.md says why.
#
# The v8 build script fetches RUSTY_V8_ARCHIVE and checks it against
# RUSTY_V8_ARCHIVE_SHA256 before use. It compiles the binding at
# RUSTY_V8_SRC_BINDING_PATH, kept here in v8/ (the crate's own, from the
# .crate Cargo.lock checks; for a crate pinned by git rev, which has none,
# the one the archive's build generated), and neither downloads nor writes one.
# crates/celld/build.rs checks that file against CELLD_V8_BINDING_SHA256,
# refuses the variables that would bring other V8 code in (a binding mirror
# or URL; trybuild; a source or debug build), and any target but $TARGET,
# the only one with an archive. Every value is forced over the environment.
[env]
CELLD_V8_VERSION = { value = "$v8", force = true }
RUSTY_V8_ARCHIVE = { value = "$url", force = true }
RUSTY_V8_ARCHIVE_SHA256 = { value = "$sha", force = true }
RUSTY_V8_SRC_BINDING_PATH = { value = ".cargo/v8/$BINDING", relative = true, force = true }
CELLD_V8_BINDING_SHA256 = { value = "$binding_sha", force = true }
RUSTY_V8_SKIP_DOWNLOAD = { value = "0", force = true }
EOF
  say "pinned V8 $v8: $url sha256 $sha; the crate's binding sha256 $binding_sha"
}

# Build celld and run its tests: against the pin, or against an archive,
# its SHA-256 and its V8 version ($1..$3), given as cargo --config
# overrides, which outrank the forced pin without changing it.
celld_test() {
  local target_dir=${CELLD_TARGET_DIR:-$ROOT/target} over=() version v
  version=$(pinned CELLD_V8_VERSION)
  if (( $# == 3 )); then
    over=(--config "env.RUSTY_V8_ARCHIVE.value=\"$1\"" \
      --config "env.RUSTY_V8_ARCHIVE_SHA256.value=\"$2\"" \
      --config "env.CELLD_V8_VERSION.value=\"$3\"")
    version=$3
  fi
  (cd "$ROOT" && CARGO_TARGET_DIR="$target_dir" cargo "${over[@]}" test --profile lab --locked -j "$JOBS")
  v=$("$target_dir/lab/celld" --version)
  echo "$v"
  [[ $v == *"(V8 $version-rusty)" ]] || die "celld --version does not report V8 $version"
}

cmd_verify_local() {
  local dist
  dist=$(dist_dir)
  celld_test "$dist/$ARCHIVE" "$(sha256sum "$dist/$ARCHIVE" | cut -d' ' -f1)" "$(dist_v8 "$dist")"
}

cmd_publish() {
  local dist crate v8 tag head check
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
  check=$(mktemp -d)
  gh release download "$tag" -R "$REPO" -D "$check" -p "$ARCHIVE" -p SHA256SUMS
  (cd "$check" && grep " $ARCHIVE\$" SHA256SUMS | sha256sum -c -) || die "the published archive does not match"
  cmp -s "$check/$ARCHIVE" "$dist/$ARCHIVE" || die "the published archive differs from the build"
  rm -rf "$check"
  say "published $tag"
}

cmd_verify() {
  [[ -n $(pinned RUSTY_V8_ARCHIVE) ]] || die "nothing pinned"
  celld_test
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
    cmd_verify_local
    cmd_publish
    cmd_pin
    cmd_verify
    say "done: commit .cargo/config.toml and open a PR with $(dist_dir)/BUILDINFO"
    ;;
  *) sed -n '2,/^set -euo/p' "$0" | sed '$d; s/^# \{0,1\}//'; exit 2 ;;
esac
