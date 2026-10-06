#!/usr/bin/env bash
# Phase 7 G2: binary size, startup-to-healthy and idle RSS for the WASM plugin host, with the
# Phase 0 method (copied adoption/phase0/fixtures, `run --bind`, VmRSS one second after launch,
# median of 5). Runs only prebuilt binaries, so it works on a device without cargo.
#
#   scripts/wasm-measure.sh DEFAULT_BIN WASM_BIN UPPERCASE_WASM [FIXTURES_DIR] > results.jsonl
#
# DEFAULT_BIN    edgelinkd built without nodes_wasm (cargo build --profile ci --locked)
# WASM_BIN       edgelinkd built with nodes_wasm (cargo build --profile ci --locked --features nodes_wasm)
# UPPERCASE_WASM the example plugin from scripts/wasm-examples.sh
set -euo pipefail

DEFAULT_BIN=$(realpath "$1")
WASM_BIN=$(realpath "$2")
PLUGIN=$(realpath "$3")
FIXTURES=$(realpath "${4:-$(dirname "$0")/../adoption/phase0/fixtures}")
PORT=19888
SAMPLES=${SAMPLES:-5}
WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT

now_ms() { date +%s%3N; }
median() { sort -n | awk '{a[NR]=$1} END {print (NR%2 ? a[(NR+1)/2] : int((a[NR/2]+a[NR/2+1])/2))}'; }

# home NAME MODE NODES: copy the fixture; MODE off|disabled|enabled; NODES plugin nodes to add.
home() {
  local dir="$WORK/$1" mode=$2 nodes=$3
  rm -rf "$dir"
  cp -a "$FIXTURES" "$dir"
  case $mode in
    disabled) printf '[runtime.wasm]\nenabled = false\n' > "$dir/edgelinkd.dev.toml" ;;
    enabled)
      printf '[runtime.wasm]\nenabled = true\nmemory_budget_kib = 65536\n' > "$dir/edgelinkd.dev.toml"
      local sha
      sha=$("$WASM_BIN" -v 0 --home "$dir" plugin stage "$PLUGIN" | python3 -c 'import json,sys; print(json.load(sys.stdin)["sha256"])')
      "$WASM_BIN" -v 0 --home "$dir" plugin activate edgelink/uppercase --sha256 "$sha" > /dev/null
      ;;
  esac
  if [[ $nodes -gt 0 ]]; then
    python3 - "$dir/flows.json" "$nodes" <<'EOF'
import json, sys
path, count = sys.argv[1], int(sys.argv[2])
flows = json.load(open(path))
tab = "6d00000000000000"
flows.append({"id": tab, "type": "tab", "label": "g2-plugins"})
for i in range(count):
    node = f"6d0000000000{i + 1:04d}"
    inject = f"6d0000000001{i + 1:04d}"
    flows.append({"id": inject, "type": "inject", "z": tab, "props": [{"p": "payload"}],
                  "payload": "x" * 1024, "payloadType": "str", "repeat": "", "crontab": "",
                  "once": True, "onceDelay": 0.1, "topic": "", "x": 100, "y": 40 * (i + 1), "wires": [[node]]})
    flows.append({"id": node, "type": "wasm-edgelink-uppercase", "z": tab, "x": 300, "y": 40 * (i + 1), "wires": [[]]})
json.dump(flows, open(path, "w"))
EOF
  fi
  echo "$dir"
}

# sample BIN HOME -> "startup_ms rss_kib anon_kib file_kib"
sample() {
  local bin=$1 dir=$2 start pid ready=""
  start=$(now_ms)
  EDGELINK_HOME="$dir" "$bin" -v 0 run --bind "127.0.0.1:$PORT" > "$dir/run.log" 2>&1 &
  pid=$!
  for _ in $(seq 1 3000); do
    if curl -sf -o /dev/null "http://127.0.0.1:$PORT/api/health"; then ready=$(( $(now_ms) - start )); break; fi
    if ! kill -0 "$pid" 2>/dev/null; then break; fi
    sleep 0.001
  done
  if [[ -z $ready ]]; then
    echo "edgelinkd did not become healthy; log:" >&2
    tail -20 "$dir/run.log" >&2
    kill "$pid" 2>/dev/null || true
    exit 1
  fi
  sleep "$(awk -v s="$start" -v n="$(now_ms)" 'BEGIN { d = 1 - (n - s) / 1000; print (d > 0 ? d : 0) }')"
  local rss anon file
  rss=$(awk '/^VmRSS/ {print $2}' "/proc/$pid/status")
  anon=$(awk '/^RssAnon/ {print $2}' "/proc/$pid/status")
  file=$(awk '/^RssFile/ {print $2}' "/proc/$pid/status")
  kill -INT "$pid"
  wait "$pid" 2>/dev/null || true
  echo "$ready $rss $anon $file"
}

measure() {
  local name=$1 bin=$2 mode=$3 nodes=$4 dir startups=() rsses=() anons=() files=()
  dir=$(home "$name" "$mode" "$nodes")
  for _ in $(seq 1 $SAMPLES); do
    read -r ms kib anon file < <(sample "$bin" "$dir")
    startups+=("$ms")
    rsses+=("$kib")
    anons+=("$anon")
    files+=("$file")
  done
  printf '{"case":"%s","plugin_nodes":%d,"startup_ms_median":%s,"startup_ms":[%s],"rss_kib_median":%s,"rss_kib":[%s],"rss_anon_kib_median":%s,"rss_file_kib_median":%s}\n' \
    "$name" "$nodes" \
    "$(printf '%s\n' "${startups[@]}" | median)" "$(IFS=,; echo "${startups[*]}")" \
    "$(printf '%s\n' "${rsses[@]}" | median)" "$(IFS=,; echo "${rsses[*]}")" \
    "$(printf '%s\n' "${anons[@]}" | median)" "$(printf '%s\n' "${files[@]}" | median)"
}

printf '{"host":"%s","arch":"%s","default_bytes":%d,"wasm_bytes":%d,"plugin_bytes":%d}\n' \
  "$(uname -n)" "$(uname -m)" "$(stat -c %s "$DEFAULT_BIN")" "$(stat -c %s "$WASM_BIN")" "$(stat -c %s "$PLUGIN")"
measure default "$DEFAULT_BIN" off 0
measure wasm-disabled "$WASM_BIN" disabled 0
measure wasm-enabled-idle "$WASM_BIN" enabled 0
measure wasm-1-node "$WASM_BIN" enabled 1
measure wasm-8-nodes "$WASM_BIN" enabled 8
