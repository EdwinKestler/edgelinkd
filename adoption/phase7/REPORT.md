# Phase 7 Report: Optional WASM Node SDK

Depends on: phases 0–6 and ADR-0002 (accepted 2026-10-05). Branch `phase7-design`.
Version remains `0.3.0`. Phase 7 is closed with this report.

## Decision

**Go, as an experimental opt-in feature; not for default builds.** The prototype satisfies the
ADR: a Wasmi sandbox with no ambient authority, fuel/deadline/memory/concurrency limits that
stopped every hostile guest, an atomic store with one previous generation, loud failure for
missing plugins, and a Rust guest SDK. It was measured in-tree on the host and on a Raspberry
Pi 5 (G1 and G2).

- Default, `--no-default-features` and `full` builds contain no `wasmi` and no plugin code
  path; idle memory is unchanged within noise. They grow by 16–17 KiB (0.1 %) for the reserved
  `wasm-` prefix, the `edgelinkd plugin` CLI surface that answers `NotSupported`,
  `EndpointClass::Plugins` and the history event type, above the 1 KiB target of PR 1.
- With `--features nodes_wasm`: +1.41 MiB binary (x86-64) / +1.19 MiB (arm64), within the
  1.5 MiB budget. Private memory (`RssAnon`) is +48–56 KiB with plugins disabled and
  +96–120 KiB enabled with no plugin node, within the 256 KiB budget. `VmRSS` is +0.9 MiB in
  both cases because the larger executable maps more clean, shared file pages; by the Phase 0
  metric (`VmRSS`) that row is over budget, which is why the feature stays opt-in.
- Default inclusion is not recommended: the measured `VmRSS` cost, ABI v1's narrow capability
  set and a Rust-only SDK do not justify it. Revisit after a 32-bit board run and real plugins.

## What is implemented

| Piece | Where | Status |
|---|---|---|
| Reserved `wasm-` prefix in every build (flow and config nodes) | `runtime/nodes/mod.rs`, `flow.rs`, `engine.rs` | `NotSupported`, never `unknown` |
| `[runtime.wasm] enabled = true` without `nodes_wasm` | `runtime/wasm/mod.rs` | startup `NotSupported` |
| `enabled` defaults to `false`, strict TOML boolean | `runtime/wasm/settings.rs` | `"yes"`/`1` rejected |
| Settings validation | `settings.rs` | unknown keys fail; ranges and cross-checks; store keys `dir`, `max_plugins`, `max_module_kib` |
| Package framing | `runtime/wasm/section.rs` | LEB128 section walker: exactly one `edgelink.manifest` ≤ 16 KiB; missing, duplicate, truncated or oversize sections fail with the byte offset; `append_manifest` for packing |
| Manifest schema 1 | `runtime/wasm/manifest.rs` | TOML, `deny_unknown_fields`; id `<publisher>/<name>` segments, semver, `abi = 1`, license, `inputs = 1`, `outputs ≤ 16`, ≤ 4 self-tests with object inputs |
| `[[node.config]]` | `manifest.rs`, `plugin_node.rs` | ≤ 32 fields of kind `string` (`max_len`), `number` (`min`/`max`/`integer`), `boolean`, `enum` (`values`); names not reserved and unique; defaults checked; per node: undeclared property, wrong kind, out of range or missing required value fails deploy; resolved object passed to `el_init` |
| ABI exports | `exec.rs` | `memory`, `el_abi_version`, `el_alloc`, `el_on_input` required with exact signatures; `el_init`/`el_close` optional (required `el_init` when config is declared); imported memories/tables/globals and mistyped imports rejected at `stage` |
| `el_init` / `el_close` | `exec.rs`, `plugin_node.rs` | `el_init` on every new instance under the message budget (rejection is a fault, counted toward the failed state); `el_close` for an idle instance at stop, under a permit and the plugin budget; `emit` refused in both |
| Per-plugin limits | `host.rs` | `[limits]` `memory_pages`, `fuel_per_message`, `deadline_ms` requested up to the `max_*` ceilings; above → error naming both keys |
| Plugin store | `runtime/wasm/store.rs` | stage → validate → quarantine → self-test (with the default configuration); activate/rollback guarded by `prepare_flows`; one previous generation, older ones deleted; remove back to quarantine; discard; verify; quarantine cap 8; fs2 exclusive lock; two-phase `*_pending` → `finish`/`revert` for online use |
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
| Node properties | `plugin_node.rs` | anything beyond `x`, `y`, `info`, `l`, `wasmPlugin` and the declared config fields fails deploy; `wasmPlugin` pin must match the active major |
| Interned `MetaNode` | `runtime/wasm/plugin_set.rs` | one leak per (type, outputs, version), bounded |
| Admin API | `crates/web/src/handlers/wasm_plugins.rs` | `GET /wasm/plugins`, `POST …/stage` (`application/wasm`), `POST …/{publisher}/{name}/activate`, `…/rollback`, `DELETE …/{publisher}/{name}` (409 `in_use`), `DELETE …/quarantine/{sha256}`; administrator only (`wasm.read`/`wasm.write`); `EndpointClass::Plugins` (1 MiB, 6/min, concurrency 1); stable error codes; `409 plugins_disabled` when `enabled = false` |
| Online activation | `wasm_plugins.rs` | under `WebState::deploy`: prepare deployed flows + credentials with the candidate set → pointer → registry swap → whole-graph redeploy → `finish`; a failed redeploy reverts pointer, registry and graph |
| Engine plugin set | `runtime/engine.rs` | swapped on each redeploy; deploy and Copilot validation prepare with the live engine's configuration (`Engine::config`), so plugin nodes validate as they run |
| Editor, `/nodes`, Copilot | `wasm_plugins.rs`, `nodes.rs`, `assistant.rs` | one generated `registerType` + form + help per plugin, every plugin string JSON- or HTML-escaped (`</script>`, U+2028/9); `/nodes` module `wasm/<id>`; catalog lists type, ports, output labels and config names/kinds (no description/help); drafts may use plugin types |
| History, audit, status | `history.rs`, `wasm_plugins.rs`, `status.rs` | category `plugin`: `staged`, `rejected`, `activated`, `rolled_back`, `removed`, `discarded`, `failed` with id, version, 12-hex digest prefix and reason code (no bytes or config values); audit lines for API actions; `/status.wasm` `{state, plugins, engineLive, permitsInUse, memoryReservedKib, …}` |
| Guest SDK | `crates/wasm-guest` | `Node` trait (`init`/`on_input`/`close`), `Msg`, `Ctx` (emit/log/status, host bounds checked before the call), `export_node!` (ABI v1 exports), `manifest!` (embeds `plugin.toml` as `edgelink.manifest`); off `wasm32`, `Ctx` records calls so plugin logic is unit-tested on the host |
| Example plugins | `crates/wasm-guest/examples/{uppercase,csvparse}` | standalone crates for `wasm32-unknown-unknown`, 64 KiB stack; 55,859 and 62,870 bytes; `csvparse` has `delimiter`/`header` config and quoted fields |
| CI | `.github/workflows/CICD.yml` `wasm-plugins` | examples fmt + clippy for `wasm32`, core/web `nodes_wasm` tests, `scripts/wasm-examples.sh --e2e` |
| Measurement | `scripts/wasm-measure.sh` | Phase 0 method on prebuilt binaries (works on a device without cargo) |
| Workspace pin | root `Cargo.toml` | `wasmi = "=2.0.0"` in `[workspace.dependencies]` |

## Not done and known limits

| Item | Consequence |
|---|---|
| 32-bit ARM board (armhf/armel), Pi 3/4/Zero 2 class | Measured on a Pi 5 (arm64) only; armv7 is build-only. Defaults are calibrated for the Pi 5. |
| Startup restore of `active.toml.prev` after a crash between an online activation's pointer write and its redeploy, when the new graph then fails | Startup fails loudly; `edgelinkd plugin rollback` while stopped recovers. |
| A redeploy that fails after its prepare passed, end to end | `revert` is unit-tested; the API undo path is code-reviewed, not exercised by a test. |
| `App::restart_engine` after an online activation | Used only when the web state has no engine; it would build with the startup plugin set. |
| History/audit for offline CLI actions | CLI output is the only record. |
| Signatures (`require_signature`) | `true` fails startup; packages are trusted by the administrator who installs them. |
| Capabilities beyond ABI v1 (clock, randomness, HTTP through the egress policy, credentials) | Not offered; a manifest that asks for any is refused. Granting them later must go through the egress policy and the credential service. |
| Guest languages other than Rust, component model | Not provided. |

Sandbox advisories: Wasmi is an interpreter with a strong validation and fuel model, but the
tests show containment for the cases listed, not the absence of runtime bugs; keep `wasmi` pinned
(`=2.0.0`) and rerun the hostile tests on every bump. Plugins share the process: a memory-safety
bug in Wasmi would be a process-level bug. Plugin output is data in flows; downstream nodes must
treat it as untrusted input.

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
- `default_memory_pages` lowered from 32 (2 MiB) to 8 (512 KiB) after the live test: with
  2 MiB caps the 8 MiB budget admitted only 3 plugin nodes. Measured live: 3 + 12 nodes admit
  7,704 KiB; one more is refused with `memory budget exceeded` and the running graph is kept.
  Modules whose initial memory exceeds their limit are rejected at `stage` with the remedy.
  Boards with spare RAM raise `memory_budget_kib` (e.g. 64 MiB on a Pi 5).
- `fuel_slice = 10⁶` ≈ 2 ms on a Pi 5, which is the cancellation latency on stop/redeploy.
- With the spike's 5·10⁷ fuel budget the Pi 5 hit the 100 ms deadline first (4.8·10⁷ fuel),
  so both limits were exercised on the device.

Open observation: per-message latency is 10× the host while raw fuel throughput is only 2.8×
slower, and the Pi 5 distribution is unusually tight (median ≈ p95). The fixed per-call cost
(alloc, memory copy, `emit` read) dominates on the Pi; it is not a gate criterion, but it should
be profiled before any throughput claim.

Not covered by G1: a 32-bit (`armhf`/`armel`) board, older or slower boards (Pi 3/4, Zero 2),
and the in-tree `edgelinkd` binary (G2). The armv7 build remains build-only.

## G2 in-tree measurements (passed with one noted row)

`scripts/wasm-measure.sh` with the Phase 0 method: `ci` profile, copied `adoption/phase0/fixtures`,
`run --bind`, startup = launch to first `/api/health`, RSS one second after launch, medians
(host 15 samples, Pi 5 samples). Plugin cases add `edgelink/uppercase` (the SDK example) with an
inject that sends one 1 KiB message to each node. Raw data: `adoption/phase7/g2/`.

Binary sizes (`stat -c %s`, stripped `ci`), host x86-64; base = `79a4364` (before any Phase 7 code):

| Build | Base | Now | Δ |
|---|---:|---:|---:|
| default | 17,004,920 | 17,021,688 | +16,768 (+0.10 %) |
| `--no-default-features` | 14,889,528 | 14,905,944 | +16,416 |
| `--features full` | 17,003,960 | 17,021,688 | +17,728 |
| `--features nodes_wasm` | — | 18,504,408 | +1,482,720 vs default (1.41 MiB) |

Raspberry Pi 5 (arm64, built natively, 51–58 °C, `throttled=0x0`): default 14,517,768,
`nodes_wasm` 15,763,016 (+1,245,248, 1.19 MiB).

| Case | Host startup (min/median ms) | Host VmRSS / RssAnon (KiB) | Pi 5 startup median (ms) | Pi 5 VmRSS / RssAnon (KiB) |
|---|---:|---:|---:|---:|
| base default (`79a4364`) | 7 / 15 | 16,044 / 3,000 | — | — |
| default | 6 / 17 | 15,532 / 3,008 | 16 | 11,824 / 2,544 |
| `nodes_wasm`, `enabled = false` | 6 / 13 | 16,492 / 3,064 | 15 | 12,736 / 2,592 |
| `nodes_wasm`, enabled, no plugin node | 6 / 14 | 16,704 / 3,128 | 15 | 12,848 / 2,640 |
| one plugin node (engine live) | 6 / 11 | 17,872 / 3,844 | 15 | 13,584 / 3,184 |
| eight plugin nodes | 6 / 20 | 20,632 / 6,612 | 16 | 15,760 / 5,360 |

The host startup distribution is bimodal (6–9 ms or 17–23 ms, from the 1 ms health-poll loop);
minima are identical for every case. Against the DESIGN budgets:

| Budget | Result |
|---|---|
| default/minimal/full unchanged within noise; ≤ 1 KiB from PR 1 | RSS unchanged (RssAnon +8 KiB vs base); size +16–17 KiB, **above the 1 KiB target** (no WASM code; see Decision) |
| `nodes_wasm` binary ≤ +1.5 MiB | +1.41 MiB host, +1.19 MiB Pi 5 — pass |
| idle RSS, feature on, `enabled = false` ≤ +256 KiB | RssAnon +56 / +48 KiB — pass; VmRSS +960 / +912 KiB — **over** (file-backed text) |
| idle RSS, `enabled = true`, no plugin node ≤ +256 KiB | RssAnon +120 / +96 KiB — pass; VmRSS over as above |
| one plugin node ≤ +1 MiB + instance | +1,168 / +736 KiB over enabled-idle (instance cap 512 KiB) — pass |
| 8 plugin nodes within `memory_budget_kib` | 4,160 KiB admitted of 8,192; RssAnon +3.5 / +2.7 MiB over idle — pass |
| startup, feature on, no plugins ≤ +5 ms | equal minima; medians within noise — pass |
| per-message latency, 1 KiB `uppercase` | in-tree node path (encode, permit, blocking call, decode, delivery; 2,000 messages, engine start included): **68.8 µs** host, **137.2 µs** Pi 5 |

The in-tree Pi/host ratio (2.0×) matches the fuel-throughput ratio; the 10× gap seen in the G1
spike bench was specific to that harness. The end-to-end example test also passed natively on
the Pi 5.

## Tests

`cargo test -p edgelink-core --features nodes_wasm --lib wasm` — 55 tests (29 first prototype,
15 PR 3/4, 11 PR 5/6):

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
  refused; traversal ids and digests rejected; a reverted activation restores the pointer and
  re-quarantines the candidate, which stays activatable.
- ABI and configuration (PR 5): missing, mistyped and imported ABI items rejected at compile;
  `el_abi_version` 2 is `NotSupported`; `el_init` receives the configuration, `emit` during
  init is a fault, `el_close` runs and a spinning `el_close` stops at its fuel budget;
  config kinds validate and normalise (numeric strings, integer, range, enum); configuration
  reaches `el_init` with defaults and node overrides; invalid, out-of-range and missing
  required values fail deploy.
- Node behaviour (PR 5): outputs on two ports arrive in emit order; `link call` → plugin →
  `link out` (return) comes back to the caller; with the only permit held elsewhere a message
  fails with "concurrency limit" after its own deadline; `engine.stop()` releases a spinning
  guest's permit within 300 ms (the guest had ≈ 0.7 s of fuel left).

`cargo test -p edgelink-web --features nodes_wasm --lib` — 5 new tests:
`online_lifecycle_drill` (stage A, B, C with a failing self-test and D 2.0.0 over HTTP; activate
A; deploy a flow using the plugin through `POST /flows`; activate B; C → 409 `selftest_failed`;
D → 409 `invalid_flows` (pin `@1`); B current, A previous; wrong rollback expectation → 409;
rollback to A; remove while used → 409 `in_use`; discard C; bad id → 400; `/status.wasm`;
then a fresh `PluginStore::open` sees A active and the flows prepare with it),
`hostile_manifest_strings_are_escaped_in_editor_html`, `plugin_routes_are_administrator_only`,
`plugin_routes_answer_disabled_without_a_store`, and
`plugin_packages_are_accepted_as_application_wasm_only_on_plugin_routes` (protection layer).

`cargo test --features nodes_wasm --bin edgelinkd plugin` — `nodes_using_matches_only_the_plugin_type`.

PR 7: `edgelink-wasm-guest` 2 unit tests (EVE round trip, host bounds enforced by `Ctx`);
`uppercase` 1 and `csvparse` 2 host-side tests; `example_plugins_install_and_run` (ignored unless
built) stages both Rust examples (validation + self-tests), activates them and runs
`inject → uppercase → csvparse(delimiter ";")` to `[{"NAME":"BOLT","QTY":"4"},{"NAME":"NUT","QTY":"7"}]`
on x86-64 and natively on the Pi 5.

Hostile-test coverage against the phase prompt: unknown ABI, malformed manifest, unsupported
capability, duplicate staging (idempotent by digest), infinite loop, cancellation, memory growth,
oversized input/output/log/status/fail, forbidden WASI/`wbg`/unknown imports, imported memory,
traversal ids and digests, symlinked store, crash at every store write, trap and three-strike
failure state, global concurrency exhaustion, stop during a call, failed self-test/activation
keeping the previous generation, restart ignoring quarantine, feature-off loud failure. Not
covered by a fuzz target: the manifest parser and host-call decoders are covered by rule tests
and the EVE decoder's bounded-input tests only.

Online smoke run against the real binary (`edgelinkd run`, `enabled = true`): stage over HTTP →
`ready`; activate → `editorReloadRequired`; `POST /flows` with a `wasm-acme-echo` node deploys;
`/nodes` HTML has the generated template and help, `/nodes` JSON has module `wasm/acme/echo`;
`/status.wasm` shows 1 plugin and 264 KiB admitted; remove → 409 `in_use`; the CLI is refused
while the runtime holds the lock. After a restart on the same home the store opens with 1
plugin active and the flow deploys; the generated editor script, evaluated in Node.js against a
stub `RED.nodes.registerType`, registers `wasm-acme-echo` with `wasmPlugin: "acme/echo@1"`.

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
| `cargo test --workspace --features full --no-fail-fast` | passed (no `nodes_wasm`; core 318 passed / 1 ignored, web 80 passed) |
| `cargo test -p edgelink-core --features nodes_wasm --lib` | 323 passed, 3 ignored (56 WASM tests; the 2 example tests need built plugins) |
| `cargo test -p edgelink-web --features nodes_wasm --lib` | 76 passed |
| `cargo test --features nodes_wasm --bin edgelinkd` | 2 passed |
| `scripts/wasm-examples.sh --e2e` | SDK 2, `csvparse` 2, `uppercase` 1 passed; both build for `wasm32`; end-to-end passed (host and Pi 5) |
| Example crates: `cargo fmt --check`, `cargo clippy --target wasm32-unknown-unknown -D warnings`; `edgelink-wasm-guest` clippy for `wasm32` | passed |
| `cargo build --features nodes_wasm`, `cargo build` | passed |
| `cargo tree -e normal -i wasmi` (default and `--features full`) | no match: `wasmi` absent |
| `cargo tree -p edgelink-core --no-default-features -i toml_edit` | nothing: the store adds no dependency to a minimal core |
| `pytest ./tests` (default build) | 825 passed, 217 skipped, 1 failed: `test_ai_split_node.py::test_splits_overlap_zero_window` passes `[["1", {...}]]` where a message object is expected (`invalid type: sequence, expected struct Msg`); test added in Phase 6 (`af56df0`), unchanged by Phase 7 — pre-existing, not fixed here |
| Offline CLI, online API and live editor runs; `run` with plugins disabled | passed |
| G2 (`scripts/wasm-measure.sh`) on host and Raspberry Pi 5 | see "G2 in-tree measurements" |
| `git diff --check` | passed |

Not run: a 32-bit ARM device run, and the Windows/ARM QEMU CI jobs (they run on schedule).

## Rollback drill

Exercised by `online_lifecycle_drill` (crates/web), the store tests and the live runs: install A →
upgrade B → C fails its self-test (rejected) → D fails `prepare_flows` (pin `@1`) with B current
and A previous → rollback B → A → restart (fresh store open) runs A. With plugins disabled,
ordinary flows run and no engine is created; a flow that needs a plugin fails deploy naming it
(`… requires WASM plugin acme/csvparse which is not active`, `… disabled by configuration`,
`… not compiled in this build`). Packages stay in `<home>/plugins` untouched while disabled and
are used again on re-enable.

## Rollback

- Default build: no `wasmi` in the dependency graph.
- `[runtime.wasm] enabled = false` (the default): plugins never run; `wasm-*` types fail loud.
- Removing `nodes_wasm` from a build: same loud failure. The only persisted state is
  `<home>/plugins/` (or `[runtime.wasm] dir`); it is never read with plugins disabled and can be
  deleted. `flows.json` and credentials are read, never written, by the store and the CLI.

## Git

Committed on `phase7-design`: `79a4364` (design + ADR), `8eb7546` (host prototype), `56d1d05`
(G1), `ef086bd` (store, manifest, CLI), `16f7b44` (config, admin API, editor), and the PR 7/8
close-out commit. Nothing pushed, tagged, released or merged to `master`; version unchanged.
