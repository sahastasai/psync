#!/usr/bin/env bash
# tauri-bundler `prepare_tools` (AppImage tools), sequential vs psync, against a local mirror of
# GitHub releases behind toxiproxy.
#
#   nix-shell -p toxiproxy --run './run.sh'
#
# bin/bundler-{seq,par,spec,hinted} are tauri-bundler's test binaries with patch.py's modes (spec is
# built with `cargo psync build`, the others with `cargo test --no-run`). The files
# have the sizes of the real assets (AppRun 18 KB, linuxdeploy 12 MB, the AppImage plugin 9 MB).
# Every response gets LATENCY ms one way; with BANDWIDTH (KB/s), each connection is capped at it.
set -euo pipefail
H=$(cd "$(dirname "$0")" && pwd)
B=${BIN:-$H/bin}
W=${WORK:-$H/work}
RUNS=${RUNS:-11}
CASES=${CASES:-"0:0 20:0 100:0 100:20000 100:5000"}
mkdir -p "$W/files"
cleanup() { kill "${MIRROR:-}" "${TOXI:-}" 2>/dev/null || true; }
trap cleanup EXIT

[ -f "$W/files/AppRun-x86_64" ] || {
  head -c 18000 /dev/urandom >"$W/files/AppRun-x86_64"
  head -c 12000000 /dev/urandom >"$W/files/linuxdeploy-x86_64.AppImage"
  head -c 9000000 /dev/urandom >"$W/files/linuxdeploy-plugin-appimage-x86_64.AppImage"
}
python3 "$H/mirror.py" "$W/files" 18100 &
MIRROR=$!
toxiproxy-server -port 18474 >"$W/toxi.log" 2>&1 &
TOXI=$!
sleep 1
tp() { curl -sf -X "$1" "http://127.0.0.1:18474$2" ${3:+-d "$3"} >/dev/null; }
tp POST /proxies '{"name":"gh","listen":"127.0.0.1:18101","upstream":"127.0.0.1:18100"}'
export TAURI_BUNDLER_TOOLS_GITHUB_MIRROR=http://127.0.0.1:18101

TEST=bundle::linux::appimage::linuxdeploy::psync_bench::prepare_tools_bench
run() { PSYNC_RUNS=$RUNS "$B/bundler-$1" --exact "$TEST" --nocapture --test-threads 1 2>/dev/null; }

printf '%-8s %-10s %8s %8s %8s %8s %8s %8s %8s\n' latency bandwidth seq par spec hinted par× spec× hinted×
for c in $CASES; do
  lat=${c%%:*} bw=${c##*:}
  tp DELETE /proxies/gh/toxics/lat 2>/dev/null || true
  tp DELETE /proxies/gh/toxics/bw 2>/dev/null || true
  [ "$lat" = 0 ] || tp POST /proxies/gh/toxics "{\"name\":\"lat\",\"type\":\"latency\",\"stream\":\"downstream\",\"attributes\":{\"latency\":$lat}}"
  [ "$bw" = 0 ] || tp POST /proxies/gh/toxics "{\"name\":\"bw\",\"type\":\"bandwidth\",\"stream\":\"downstream\",\"attributes\":{\"rate\":$bw}}"
  declare -A ms files
  for m in seq par spec hinted; do
    out=$(run $m)
    ms[$m]=$(sed -n 's/^MEDIAN_MS //p' <<<"$out")
    files[$m]=$(command grep '^FILE' <<<"$out")
  done
  for m in par spec hinted; do [ "${files[seq]}" = "${files[$m]}" ] || { echo "files differ: $m"; exit 1; }; done
  awk -v l="$lat" -v b="$bw" -v s="${ms[seq]}" -v p="${ms[par]}" -v x="${ms[spec]}" -v h="${ms[hinted]}" \
    'BEGIN { printf "%-8s %-10s %8s %8s %8s %8s %8.2f %8.2f %8.2f\n", l, b, s, p, x, h, s / p, s / x, s / h }'
done
