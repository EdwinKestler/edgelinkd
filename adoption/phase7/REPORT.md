# Phase 7 Report: Optional WASM Node SDK (partial prototype)

Depends on: phases 0–6 and ADR-0002 (accepted 2026-10-05). Branch `phase7-design`.
Version remains `0.3.0`.

This is a **partial prototype**. The Wasmi host, the plugin flow node and every per-call limit
from DESIGN.md §6–8 are implemented and tested, and so are PR 3 (section walker, manifest
schema 1) and PR 4 (plugin store, lifecycle, offline CLI, crash-injection tests). A plugin can
now be installed with the runtime stopped; online install, editor integration and the guest
SDK (PR 5–7 leftovers) are not done. The G1 device gate passed on a Raspberry Pi 5 (arm64);
see "G1 device run" below.

## What is implemented

| Piece | Where | Status |
|---|---|---|
| Reserved `wasm-` prefix in every build (flow and config nodes) | `runtime/nodes/mod.rs`, `flow.rs`, `engine.rs` | `NotSupported`, never `unknown` |
| `[runtime.wasm] enabled = true` without `nodes_wasm` | `runtime/wasm/mod.rs` | startup `NotSupported` |
| `enabled` defaults to `false`, strict TOML boolean | `runtime/wasm/settings.rs` | `"yes"`/`1` rejected |
| Settings validation | `settings.rs` | unknown keys fail; ranges and cross-checks; store keys `dir`, `max_plugins`, `max_module_kib` |
| Package framing | `runtime/wasm/section.rs` | LEB128 section walker: exactly one `edgelink.manifest` ≤ 16 KiB; missing, duplicate, truncated or oversize sections fail with the byte offset; `append_manifest` for packing |
| Manifest schema 1 | `runtime/wasm/manifest.rs` | TOML, `deny_unknown_fields`; id `<publisher>/<name>` segments, semver, `abi = 1`, license, `inputs = 1`, `outputs ≤ 16`, ≤ 4 self-tests with object inputs; `[[node.config]]` refused `NotSupported` |
| Per-plugin limits | `host.rs` | `[limits]` `memory_pages`, `fuel_per_message`, `deadline_ms` requested up to the `max_*` ceilings; above → error naming both keys |
| Plugin store | `runtime/wasm/store.rs` | stage → validate → quarantine → self-test; activate/rollback guarded by `prepare_flows`; one previous generation, older ones deleted; remove back to quarantine; discard; verify; quarantine cap 8; fs2 exclusive lock |
| Store hardening | `store.rs` | dirs `0700`, files `0600`, `O_NOFOLLOW`, symlinked root/children refused, ids and digests validated before any path is built; tmp → fsync → rename → fsync dir |
| Crash recovery | `store.rs` | on open: `.part` deleted, `active.toml.tmp` dropped, unreferenced `store/` files back to quarantine; a missing or tampered generation disables only that plugin |
| Startup wiring | `runtime/wasm/mod.rs` `attach_store`, `src/app.rs` | with `enabled = true` the runtime opens the store, holds its lock for its lifetime and deploys with the active set; disabled → directory not touched |
| Offline CLI | `src/commands/plugin.rs` | `list`, `stage`, `activate --sha256`, `rollback --sha256`, `remove`, `discard`, `verify`, `pack`; `remove` refused while a flow node uses the type |
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
| `GET /wasm/plugins` | `crates/web` | answers `NotSupported` (HTTP 501), not an empty list |
| Guest SDK | `crates/wasm-guest` | EVE/1 re-export; safe `emit_bytes`/`log`/`status`/`fail` wrappers on `wasm32` only |
| Workspace pin | root `Cargo.toml` | `wasmi = "=2.0.0"` in `[workspace.dependencies]` |

## Not done

| Item | Consequence |
|---|---|
| Device runs beyond G1 (32-bit ARM board, Pi 3/4 class, in-tree binary) | Defaults are calibrated on a Pi 5 only; armv7 remains build-only. |
| Online install (admin API under the deploy lock, registry swap, redeploy, startup restore of `.prev` after an online activation whose graph fails) | Install only with the runtime stopped. |
| `[[node.config]]` | No plugin configuration; a manifest that declares any is refused. |
| History/audit events for store operations | CLI output is the only record. |
| `el_init` / `el_close` | Not called; ABI v1 guests in this prototype export `el_abi_version`, `el_alloc`, `el_on_input`. |
| Generated editor HTML, `/nodes` entries, Copilot catalog, history/audit events, `/status` section, `EndpointClass::Plugins` | Plugins are invisible to the editor and Copilot. |
| `wasm32-unknown-unknown` CI job and an example Rust plugin | The guest SDK is compiled for the host only. |
| In-tree size/RSS measurements (G2) | ADR-0002 spike numbers stand. |

## G1 device run (passed)

Raspberry Pi 5 Model B Rev 1.0 (BCM2712, Cortex-A76), Raspberry Pi OS 64-bit (`arm64`), kernel
`6.18.50+rpt-rpi-2712`, 47.7 °C, `throttled=0x0`. Spike built natively on the board with
`adoption/phase7/spike/run.sh none wasmi`. Raw lines: `spike/results/pi-arm64.jsonl`,
`spike/results/pi-device.txt`.

| ADR-0002 §11 criterion | Pi 5 result | Pass |
|---|---:|---|
| Wasmi idle RSS Δ ≤ 1 MiB | 1,776 → 2,128 KiB: **+352 KiB** | yes |
| All hostile guests contained | 6/6 (`spin`, `grow`, `bigout`, `wasi`, `bindgen`, `startspin`) | yes |
| Deadline overshoot ≤ 25 ms | `spin-deadline` 100.3 ms for a 100 ms deadline: **0.3 ms** | yes |
| Compile `bulk.wasm` (138 KiB) ≤ 500 ms | **30.4 ms** | yes |

Other Pi 5 figures, with the x86-64 host for comparison:

| Measure | Pi 5 | i9-14900K host |
|---|---:|---:|
| Stripped binary Δ (`wasmi` − `none`) | +917,504 B (896 KiB) | +1,008,216 B |
| RSS after compiling `upper` + `bulk` | +880 KiB | +892 KiB |
| Per instance (one 64 KiB page touched) | 82 KiB | 83 KiB |
| Instantiate | 59 µs | 56 µs |
| Fuel throughput (`spin`) | 4.8·10⁵ fuel/ms | 1.36·10⁶ fuel/ms |
| 1 KiB message, median / p95 | 95.0 µs / 95.0 µs | 9.3 µs / 11.1 µs |

Consequences for the defaults (unchanged, now calibrated):

- `default_fuel = 2·10⁷` ≈ 42 ms of guest work on a Pi 5, so fuel binds well before the 250 ms
  deadline; the deadline is the backstop for slower boards.
- `fuel_slice = 10⁶` ≈ 2 ms on a Pi 5, which is the cancellation latency on stop/redeploy.
- With the spike's 5·10⁷ fuel budget the Pi 5 hit the 100 ms deadline first (4.8·10⁷ fuel),
  so both limits were exercised on the device.

Open observation: per-message latency is 10× the host while raw fuel throughput is only 2.8×
slower, and the Pi 5 distribution is unusually tight (median ≈ p95). The fixed per-call cost
(alloc, memory copy, `emit` read) dominates on the Pi; it is not a gate criterion, but it should
be profiled before any throughput claim.

Not covered by G1: a 32-bit (`armhf`/`armel`) board, older or slower boards (Pi 3/4, Zero 2),
and the in-tree `edgelinkd` binary (G2). The armv7 build remains build-only.

## Tests

`cargo test -p edgelink-core --features nodes_wasm --lib wasm` — 44 tests (29 from the first
prototype, 15 new):

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
- Section and manifest: packed manifest found, precise framing errors, duplicate section
  rejected, sample manifest parses, every manifest rule fails loudly.
- Store: stage/activate/upgrade/rollback; failed activation keeps B current and A previous;
  **every crash point** (`staged`, `quarantined`, `prepared`, `promoted-wasm`,
  `pointer-prev-written`, `pointer-tmp-written`, `pointer-written`) reopens to a complete old or
  new pointer; crash during stage leaves nothing runnable; invalid packages rejected and kept
  nowhere; failing self-test quarantined as `rejected` and not activatable; remove preserves
  packages and the lock is exclusive; tampering disables only that plugin; symlinked store
  refused; traversal ids and digests rejected.

`cargo test --features nodes_wasm --bin edgelinkd plugin` — `nodes_using_matches_only_the_plugin_type`.

CLI smoke run (andorxps, debug build, `identity.wat` fixture packed twice as `acme/echo` 1.0.0
and 1.1.0): pack refuses to overwrite; an unpacked module is rejected at stage; both versions
stage `ready`; activate 1.0.0 then 1.1.0 keeps 1.0.0 as previous; rollback with the wrong
expected digest is refused, with the right one swaps; `verify` is clean; `remove` is refused
while `flows.json` uses `wasm-acme-echo`; `edgelinkd run` with `enabled = true` logs
`plugin store … open, 1 plugin(s) active` and deploys the node, and a CLI command during the
run fails with "in use by another edgelinkd process"; activation is refused when the flows use
an inactive plugin; after the flow is gone `remove` returns both generations to quarantine and
`discard` deletes one. Files are `0600`, directories `0700`.

## Gates (andorxps, Rust 1.99)

| Command | Result |
|---|---|
| `cargo fmt --check` | passed |
| `cargo clippy --all-features --tests --all -- -D warnings` | passed |
| `cargo test --workspace --features full --no-fail-fast` | passed (no `nodes_wasm`; core 318 passed / 1 ignored, web 79 passed) |
| `cargo test -p edgelink-core --features nodes_wasm --lib` | 311 passed, 1 ignored (44 WASM tests) |
| `cargo test -p edgelink-web --features nodes_wasm --lib` | 71 passed |
| `cargo test --features nodes_wasm --bin edgelinkd` | 2 passed |
| `cargo build --features nodes_wasm`, `cargo build` | passed |
| `cargo tree -e normal -i wasmi` (default and `--features full`) | no match: `wasmi` absent |
| `cargo tree -p edgelink-core --no-default-features -i toml_edit` | nothing: the store adds no dependency to a minimal core |
| CLI smoke run (above) and `run` with plugins disabled | passed; disabled run fails the `wasm-*` node loudly and creates no `plugins/` directory |
| `git diff --check` | passed |

Not run in this close-out: `pytest ./tests -v`, ARM cross builds of `edgelinkd`, in-tree size/RSS
measurements (G2). G1 is recorded above.

## Rollback

- Default build: no `wasmi` in the dependency graph.
- `[runtime.wasm] enabled = false` (the default): plugins never run; `wasm-*` types fail loud.
- Removing `nodes_wasm` from a build: same loud failure. The only persisted state is
  `<home>/plugins/` (or `[runtime.wasm] dir`); it is never read with plugins disabled and can be
  deleted. `flows.json` and credentials are read, never written, by the store and the CLI.

## Git

Committed on `phase7-design` only. Not pushed, tagged, released or merged to `master`.
