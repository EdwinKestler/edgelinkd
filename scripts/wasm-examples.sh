#!/usr/bin/env bash
# Build the example WASM plugins (crates/wasm-guest/examples/*) for wasm32-unknown-unknown,
# run their host-side unit tests, and optionally the end-to-end install test.
#
#   scripts/wasm-examples.sh          build + unit tests; packages in target/wasm-examples/
#   scripts/wasm-examples.sh --e2e    also stage, activate and run them in n2link-core
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
OUT="$ROOT/target/wasm-examples"
TARGET=wasm32-unknown-unknown

if ! rustup target list --installed | grep -qx "$TARGET"; then
  echo "missing Rust target: rustup target add $TARGET" >&2
  exit 1
fi

mkdir -p "$OUT"
cargo test -q --manifest-path "$ROOT/crates/wasm-guest/Cargo.toml"
for example in uppercase csvparse tofsense; do
  dir="$ROOT/crates/wasm-guest/examples/$example"
  # Host-side logic tests (the ABI exports compile only for wasm32).
  cargo test -q --manifest-path "$dir/Cargo.toml"
  (cd "$dir" && CARGO_TARGET_DIR="$ROOT/target/wasm-examples-build" cargo build -q --release --target "$TARGET")
  wasm="$ROOT/target/wasm-examples-build/$TARGET/release/n2link_plugin_${example}.wasm"
  cp "$wasm" "$OUT/$example.wasm"
  if ! grep -aq "n2link.manifest" "$OUT/$example.wasm"; then
    echo "$example.wasm has no n2link.manifest section" >&2
    exit 1
  fi
  printf '%-10s %8d bytes  sha256 %s\n' "$example" "$(stat -c %s "$OUT/$example.wasm")" \
    "$(sha256sum "$OUT/$example.wasm" | cut -d' ' -f1)"
done

if [[ "${1:-}" == "--e2e" ]]; then
  N2LINK_WASM_EXAMPLES="$OUT" cargo test -q -p n2link-core --features nodes_wasm --lib -- \
    --ignored --exact runtime::wasm::plugin_node::tests::example_plugins_install_and_run
fi
