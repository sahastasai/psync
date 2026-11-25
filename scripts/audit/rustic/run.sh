#!/usr/bin/env bash
# rustic `repoinfo` and `diff`, sequential vs psync, against a REST repository with added latency.
#
#   nix-shell -p toxiproxy restic-rest-server --run './run.sh'
#
# bin/rustic-seq is rustic as released; bin/rustic-psync has `#[psync::graph]` and `#[psync::par]`
# on RepoInfoCmd::inner_run and DiffCmd::inner_run (patch.py). Latencies are one-way, added by
# toxiproxy to every response; 0 is the REST server on the same machine.
set -euo pipefail
H=$(cd "$(dirname "$0")" && pwd)
W=$H/work
RUNS=${RUNS:-11}
LATENCIES=${LATENCIES:-"0 5 20 50"}
mkdir -p "$W"
export RUSTIC_PASSWORD=psync

cleanup() { kill "${REST:-}" "${TOXI:-}" 2>/dev/null || true; }
trap cleanup EXIT

rest-server --path "$W/repo" --listen 127.0.0.1:18000 --no-auth >"$W/rest.log" 2>&1 &
REST=$!
toxiproxy-server -port 18474 >"$W/toxi.log" 2>&1 &
TOXI=$!
sleep 1
tp() { curl -sf -X "$1" "http://127.0.0.1:18474$2" ${3:+-d "$3"} >/dev/null; }
tp POST /proxies '{"name":"rest","listen":"127.0.0.1:18001","upstream":"127.0.0.1:18000"}'

DIRECT=rest:http://127.0.0.1:18000/
VIA=rest:http://127.0.0.1:18001/

# A repository with 20 snapshots of a changing tree: about 20 index files and their packs.
if [ ! -f "$W/.ready" ]; then
  rm -rf "$W/repo" "$W/data"
  mkdir -p "$W/data"
  "$H/bin/rustic-seq" -r "$DIRECT" init >/dev/null 2>&1
  for i in $(seq 1 20); do
    for j in $(seq 1 200); do head -c 2048 /dev/urandom >"$W/data/f$j-$i"; done
    "$H/bin/rustic-seq" -r "$DIRECT" backup "$W/data" >/dev/null 2>&1
  done
  touch "$W/.ready"
fi
SNAPS=$(ls -tr "$W/repo/snapshots" | sed -n '1p;$p' | cut -c1-8 | tr '\n' ' ')
read -r FIRST LAST <<<"$SNAPS"

median() { sort -n | awk '{a[NR]=$1} END {print a[int((NR+1)/2)]}'; }
measure() { # binary, args...: median wall time in ms over $RUNS runs
  local bin=$1; shift
  for _ in $(seq "$RUNS"); do
    local s e
    s=$(date +%s%N)
    "$H/bin/$bin" -r "$VIA" --no-cache "$@" >/dev/null 2>&1
    e=$(date +%s%N)
    echo $(((e - s) / 1000000))
  done | median
}
same() { # the two binaries print the same thing
  diff <("$H/bin/rustic-seq" -r "$VIA" --no-cache "$@" 2>/dev/null) \
       <("$H/bin/rustic-psync" -r "$VIA" --no-cache "$@" 2>/dev/null) >/dev/null
}

printf '%-10s %-24s %10s %10s %8s\n' latency command sequential psync speedup
for lat in $LATENCIES; do
  tp DELETE /proxies/rest/toxics/latency || true
  [ "$lat" -gt 0 ] && tp POST /proxies/rest/toxics \
    "{\"name\":\"latency\",\"type\":\"latency\",\"stream\":\"downstream\",\"attributes\":{\"latency\":$lat}}"
  for cmd in "repoinfo" "diff $FIRST $LAST"; do
    # shellcheck disable=SC2086
    same $cmd || { echo "OUTPUT DIFFERS: $cmd at ${lat}ms"; exit 1; }
    # shellcheck disable=SC2086
    s=$(measure rustic-seq $cmd); p=$(measure rustic-psync $cmd)
    printf '%-10s %-24s %8sms %8sms %7.2fx\n' "${lat}ms" "${cmd%% *}" "$s" "$p" "$(echo "$s / $p" | bc -l)"
  done
done
