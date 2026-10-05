#!/usr/bin/env bash
# Phase 7 spike: stripped size, idle RSS, load/bench numbers and hostile-guest containment for
# each candidate runtime. Evidence for adoption/phase7/ADR-0002-wasm-node-sdk.md.
#
#   adoption/phase7/spike/run.sh                       # host: all runtimes, build + measure
#   adoption/phase7/spike/run.sh none wasmi            # host: selected runtimes
#   SPIKE_TARGET=armv7-unknown-linux-gnueabihf adoption/phase7/spike/run.sh none wasmi wt36-pulley
#                                                      # cross: build-only sizes
#
# Outputs JSON lines on stdout. Build products go to $SPIKE_OUT (default /tmp), never the repo.
set -euo pipefail
HERE=$(cd "$(dirname "$0")" && pwd)
OUT=${SPIKE_OUT:-/tmp/edgelinkd-phase7-spike}
TARGET=${SPIKE_TARGET:-}
export CCACHE_DISABLE=1
export CARGO_TARGET_ARMV7_UNKNOWN_LINUX_GNUEABIHF_LINKER=${CARGO_TARGET_ARMV7_UNKNOWN_LINUX_GNUEABIHF_LINKER:-arm-linux-gnueabihf-gcc}
export CARGO_TARGET_ARMV7_UNKNOWN_LINUX_GNUEABI_LINKER=${CARGO_TARGET_ARMV7_UNKNOWN_LINUX_GNUEABI_LINKER:-arm-linux-gnueabi-gcc}
export CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER=${CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER:-aarch64-linux-gnu-gcc}
[ $# -eq 0 ] && set -- none wasmi wt36 wt36-pulley wt36-runtime-only wt48
mkdir -p "$OUT"
GUESTS="$OUT/guests"
(cd "$HERE/guestgen" && CARGO_TARGET_DIR="$OUT/target-guestgen" cargo run -q --release -- "$HERE/guests" "$GUESTS") >&2

median_idle() {
  for _ in 1 2 3 4 5; do "$1" idle | sed -E 's/.*"rss_kib":([0-9]+).*/\1/'; done | sort -n | sed -n 3p
}

for f in "$@"; do
  dir="$OUT/target-${TARGET:-host}-$f"
  start=$(date +%s)
  (cd "$HERE" && CARGO_TARGET_DIR="$dir" cargo build -q --release $( [ -f Cargo.lock ] && echo --locked ) --features "$f" ${TARGET:+--target "$TARGET"}) >&2
  secs=$(( $(date +%s) - start ))
  bin="$dir/${TARGET:+$TARGET/}release/edgelink-wasm-spike"
  echo "{\"feature\":\"$f\",\"target\":\"${TARGET:-host}\",\"stripped_bytes\":$(stat -c %s "$bin"),\"build_s\":$secs}"
  [ -n "$TARGET" ] && continue
  [ "$f" = wt36 ] && "$bin" precompile "$GUESTS"
  echo "{\"feature\":\"$f\",\"idle_rss_median_kib\":$(median_idle "$bin")}"
  [ "$f" = none ] && continue
  "$bin" load "$GUESTS" 16
  "$bin" bench "$GUESTS" 2000
  "$bin" hostile "$GUESTS"
  SPIKE_FUEL_PER_CALL=100000000000 "$bin" hostile "$GUESTS" | grep '"guest":"spin"' | sed 's/"guest":"spin"/"guest":"spin-deadline"/'
done
