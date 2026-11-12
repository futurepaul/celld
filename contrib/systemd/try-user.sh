#!/usr/bin/env bash
# Runs celld dev under fragment-celld.service's own hardening, as a transient
# systemd --user unit, with no root: proof that celld works inside the
# sandbox before anyone installs the unit.
#
#   contrib/systemd/try-user.sh CELLD ESBUILD PROJECT PORT [VAR=VALUE ...]
#
# The [Service] lines are read from fragment-celld.service. A user manager
# cannot switch users or own system paths, so these differ, and only these:
#   User=, Group=          the caller's user instead
#   ProtectHome=yes        tmpfs, with CELLD, ESBUILD and PROJECT bound read-only
#   StateDirectory=        ~/.cache/celld-try/PORT, which PROJECT/.celld links to
#   EnvironmentFile=       VAR=VALUE arguments
# The unit is named celld-try-PORT; stop it with
#   systemctl --user stop celld-try-PORT
set -euo pipefail

[[ $# -ge 4 ]] || { sed -n '2,16p' "$0" | sed 's/^# \{0,1\}//'; exit 2; }
celld=$(readlink -f "$1") esbuild=$(readlink -f "$2") project=$(readlink -f "$3") port=$4
shift 4
unit_file=${UNIT:-"$(dirname "$0")/fragment-celld.service"}
state="${XDG_CACHE_HOME:-$HOME/.cache}/celld-try/$port"
mkdir -p "$state"
if [[ -e $project/.celld && ! -L $project/.celld ]]; then
  echo "try-user.sh: $project/.celld is a directory; move it aside" >&2
  exit 1
fi
ln -sfn "$state" "$project/.celld"

props=()
while IFS= read -r line; do
  case $line in
    ''|'#'*|'['*) continue ;;
    User=*|Group=*|EnvironmentFile=*|Environment=*|ExecStart=*|WorkingDirectory=*|StateDirectory*|ProtectHome=*|Restart=*|RestartSec=*) continue ;;
  esac
  props+=(-p "$line")
done < <(sed -n '/^\[Service\]/,/^\[/p' "$unit_file")

env=(-E HOME="$project/.celld" -E NO_COLOR=1 -E CELLD_ESBUILD="$esbuild")
for kv in "$@"; do env+=(-E "$kv"); done

exec systemd-run --user --unit="celld-try-$port" --collect --quiet \
  "${props[@]}" \
  -p ProtectHome=tmpfs \
  -p "BindReadOnlyPaths=$celld $esbuild $project" \
  -p "BindPaths=$state" \
  "${env[@]}" \
  -- "$celld" dev "$project" --host 127.0.0.1 --port "$port" --logs --no-watch
