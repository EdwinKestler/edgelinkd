# Phase 7 Report: Optional WASM Node SDK (partial prototype)

Depends on: phases 0–6 and ADR-0002 (accepted 2026-10-05). Branch `phase7-design`.
Version remains `0.3.0`.

This is a **partial prototype**. The Wasmi host, the plugin flow node and every per-call limit
from DESIGN.md §6–8 are implemented and tested. The plugin store, so any way to install a
plugin, is not: in a real deployment every `wasm-*` node reports that its plugin is not active.
The G1 Raspberry Pi-class run has not been done.

## What is implemented

| Piece | Where | Status |
|---|---|---|
| Reserved `wasm-` prefix in every build (flow and config nodes) | `runtime/nodes/mod.rs`, `flow.rs`, `engine.rs` | `NotSupported`, never `unknown` |
| `[runtime.wasm] enabled = true` without `nodes_wasm` | `runtime/wasm/mod.rs` | startup `NotSupported` |
| `enabled` defaults to `false`, strict TOML boolean | `runtime/wasm/settings.rs` | `"yes"`/`1` rejected |
| Settings validation | `settings.rs` | unknown keys fail; store keys (`dir`, `max_plugins`, `max_module_kib`) fail `NotSupported`; ranges and cross-checks |
| EVE/1 codec | `crates/eve` | bounded decoder (depth, value count, lengths checked before allocation) |
| `Variant` ↔ EVE/1 | `runtime/wasm/convert.rs` | dates before 1970 round-trip; non-finite numbers and unrepresentable dates are errors; guest regexps compiled with a 1 MiB size limit |
| Wasmi 2.0 host | `runtime/wasm/exec.rs` | four ABI imports only; any other import (WASI, `wbg`, unknown `edgelink:node/v1` name) rejected before linking; start functions rejected; floats allowed; SIMD not compiled in |
| Fuel-sliced calls | `exec.rs` | fuel budget, wall-clock deadline and cancellation checked every slice |
| Host bounds | `exec.rs` | emit ≤ 64 KiB × 16, Σ ≤ 256 KiB, port < outputs; log ≤ 512 B × 16 (levels 0–3); status fill 0–4 / shape 0–1, ≤ 128 B; fail ≤ 1 KiB |
| Lazy shared engine, module cache | `runtime/wasm/host.rs` | engine created on the first plugin message and dropped with its last user; one compile per module (SHA-256) |
| Per-graph memory admission | `host.rs` | Σ instance memory + 8 × module size per distinct module ≤ `memory_budget_kib`; reset on redeploy |
| Plugin flow node | `runtime/wasm/plugin_node.rs` | one reused instance per node; global permits (`max_concurrent`); outputs delivered only on success, in emit order; output must be an object; a changed `_msgid` is a fault; missing `_msgid` copied from the input |
| Failure state | `plugin_node.rs` | `failure_threshold` faults within `failure_window_s` → red ring status, every message errors until redeploy |
| Guest `log` / `status` / `fail` | `plugin_node.rs` | logs to target `edgelink::wasm` (50 lines/s per node, drops counted); status reported; fail text becomes a catchable node error |
| Node properties | `plugin_node.rs` | anything beyond `x`, `y`, `info`, `l`, `wasmPlugin` fails deploy (plugin configuration is not implemented); `wasmPlugin` pin must match the active major |
| Interned `MetaNode` | `runtime/wasm/plugin_set.rs` | one leak per (type, outputs, version), bounded |
| `GET /wasm/plugins`, `edgelinkd plugin list` | `crates/web`, `src/commands/plugin.rs` | answer `NotSupported` (HTTP 501), not an empty list |
| Guest SDK | `crates/wasm-guest` | EVE/1 re-export; safe `emit_bytes`/`log`/`status`/`fail` wrappers on `wasm32` only |
| Workspace pin | root `Cargo.toml` | `wasmi = "=2.0.0"` in `[workspace.dependencies]` |

## Not done

| Item | Consequence |
|---|---|
| G1 Raspberry Pi-class device run | Fuel/deadline defaults are provisional; the execution code must not merge to `master` (DESIGN PR 0 gate). |
| Plugin store and lifecycle (stage, quarantine, self-test, activate, rollback, remove, crash recovery) and the CLI/API that drive it | No install path; plugins exist only in tests. |
| Manifest schema 1 (custom section, `[[node.config]]`, limits requests, self-test vectors) | No plugin configuration; per-plugin limits use the global defaults. |
| `el_init` / `el_close` | Not called; ABI v1 guests in this prototype export `el_abi_version`, `el_alloc`, `el_on_input`. |
| Generated editor HTML, `/nodes` entries, Copilot catalog, history/audit events, `/status` section, `EndpointClass::Plugins` | Plugins are invisible to the editor and Copilot. |
| `wasm32-unknown-unknown` CI job and an example Rust plugin | The guest SDK is compiled for the host only. |
| In-tree size/RSS measurements (G2) | ADR-0002 spike numbers stand. |

## Tests

`cargo test -p edgelink-core --features nodes_wasm --lib wasm` — 29 tests:

- Prefix and configuration: `wasm_prefix_fails_loud_when_the_feature_is_off` (exact message per
  build), `a_wasm_config_node_is_never_unknown`, `enabled_true_without_the_feature_is_not_supported`,
  `enabled_must_be_a_boolean`, `plugin_ids_come_from_type_names`,
  `settings::defaults_keep_plugins_off`, `settings::invalid_values_fail_loudly`.
- Host (`exec`): uppercase round trip, WASI import rejected, unknown ABI import rejected, float
  guest accepted, start function rejected, infinite loop → fuel then deadline (< 500 ms),
  cancellation at the next slice, memory growth capped, oversized emit and bad port are faults,
  log/status/fail recorded.
- Engine (`host`): engine created lazily and dropped with its last user.
- Codec (`convert`, `edgelink-eve`): pre-1970 dates, buffers keep their type, scalar round trips,
  NaN, trailing bytes, unknown tag.
- Plugin set: injective type names, invalid ids rejected, `MetaNode` interned once.
- Flow node: identity round trip keeps payload and `_msgid`; disabled by default even with the
  feature; missing plugin names `acme/csvparse`; undeclared property and wrong pin rejected;
  memory budget admission; forged `_msgid` caught by a `catch` node; three faults → failed state.

## Gates (andorxps, Rust 1.99)

| Command | Result |
|---|---|
| `cargo fmt --check` | passed |
| `cargo clippy --all-features --tests --all -- -D warnings` | passed |
| `cargo test --workspace --features full --no-fail-fast` | passed (no `nodes_wasm`; core 318 passed / 1 ignored, web 79 passed) |
| `cargo test -p edgelink-core --features nodes_wasm --lib` | 296 passed, 1 ignored (29 WASM tests) |
| `cargo test -p edgelink-web --features nodes_wasm --lib` | 71 passed |
| `cargo build --features nodes_wasm` | passed |
| `cargo tree -e normal -i wasmi` (default and `--features full`) | no match: `wasmi` absent |
| `edgelinkd plugin list` (with `nodes_wasm`) | exits 1: `not supported: the WASM plugin store is not implemented in this prototype` |
| `git diff --check` | passed |

Not run in this close-out: `pytest ./tests -v`, ARM cross builds, size/RSS measurements, G1.

## Rollback

- Default build: no `wasmi` in the dependency graph.
- `[runtime.wasm] enabled = false` (the default): plugins never run; `wasm-*` types fail loud.
- Removing `nodes_wasm` from a build: same loud failure; no persisted state exists to migrate.

## Git

Uncommitted at the time of writing; committed on `phase7-design` only with separate approval.
Not pushed, tagged, released or merged to `master`.
