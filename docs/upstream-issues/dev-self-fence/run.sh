#!/bin/bash
# run.sh CELLD GB [PROJECT_DIR]: an idle `celld dev` over PROJECT_DIR (default:
# this directory, so its state is on this disk) while GB gigabytes are written
# to this directory's disk and fsynced. Prints the slowest lease renewal and
# whether the node fenced itself.
here="$(cd "$(dirname "$0")" && pwd)"
project="${3:-$here}"
[ "$project" = "$here" ] || cp "$here/index.js" "$here/wrangler.jsonc" "$project/"
cd "$project"
"$1" dev . --port 18935 --no-watch --clean --logs > celld.log 2>&1 &
node=$!
for _ in $(seq 100); do curl -sf http://127.0.0.1:18935/.well-known/celld/health >/dev/null && break; sleep 0.2; done
sleep 4
dd if=/dev/urandom of="$here/load.bin" bs=4M count=$(( $2 * 256 )) conv=fsync status=none
rm -f "$here/load.bin"
sleep 6
kill -0 $node 2>/dev/null && echo "node: up" || { wait $node; echo "node: exited $?"; }
kill -TERM $node 2>/dev/null; wait $node 2>/dev/null
grep -o 'attempt="renew" outcome="[a-z_]*" elapsed_ms=[0-9]*' celld.log |
  awk -F'elapsed_ms=' '{n++; if ($2+0>m) m=$2+0} END {print n" renewals, the slowest "m" ms"}'
grep -c 'SELF-FENCE' celld.log | sed 's/^/self-fence lines: /'
