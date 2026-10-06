# ADR-0002: Phase 7 WASM Node SDK — Runtime Decision and Sandbox Contract

- Status: **accepted** by the maintainer on 2026-10-05 (gate G0 passed); implementation design in
  `adoption/phase7/DESIGN.md`
- Date: 2026-10-05 (America/Guatemala)
- Base revision: `be2e155` (`Reject function extra modules at deploy`), version `0.3.0`, clean tree
- Decision scope: Phase 7 of `adoption/z8adoptionplan.md`, prompt `adoption/z8phases/phase7prompt.md`
- Evidence: `adoption/phase7/spike/` (measurement harness) and `adoption/phase7/spike/results/`
- Reference only: z8run `crates/z8run-runtime` at `2e2cba6178d94e22906d37d6ac1af5ba23dcbbb0`
  (Wasmtime 48, default features, 128 MiB / 2·10⁹ fuel / 5 s defaults). No z8run code is copied.

This ADR changes no runtime source, dependency, feature, generated file, `Cargo.lock`, or
version. The spike is a standalone package with its own `[workspace]`; it is not a workspace
member, is not built by `cargo build --all`, and writes its build products outside the repo.

## Decision

| Question | Decision |
|---|---|
| Wasmtime (any configuration) | **No-go.** It does not fit the embedded budget, the ARM target set, or the MSRV, and its only small configuration (runtime-only) requires trusting native artifacts. |
| Phase 7 overall | **Go for an opt-in prototype** on **Wasmi 2.0.x** (pure-Rust interpreter, MIT/Apache-2.0), behind `nodes_wasm`, absent from `default`, `full` and minimal builds. |
| Plugin model | Core WebAssembly module + EdgeLinkd ABI `edgelink:node/v1`. No WASI, no component model, no wasm-bindgen/JS glue, no third-party editor HTML/JS. |
| Authority | Pure compute. The only host imports are `emit`, `log`, `status`, `fail`. Network, filesystem, environment, clock, randomness, process, secrets, context and deploy are **not linkable** in ABI v1. |
| Install | Local, admin-authorized, single-file package. Stage → validate → quarantine → self-test → activate, one previous generation kept, atomic pointer swap. |
| Missing plugin | Reserved `wasm-` type namespace is an owned type (the `ai-agent` rule): deploy fails with `NotSupported` naming the plugin identity, whether `nodes_wasm` is off or the plugin is absent. |
| Gate before merge | One real ARM device result (§11). If Wasmi breaches the §10 budget on that device, Phase 7 stops with a no-go and this ADR is amended. |

The rest of this document is the evidence (§1–3), the rejected options (§4), and the contract the
prototype must implement (§5–12).

## 1. Constraints this decision is measured against

- **Budget (ADR-0001).** WASM enabled: at most 8 MiB stripped-binary growth; disabled: absent,
  zero linked code/dependency growth. Idle RSS for a default-off feature: below 4 MiB, except
  WASM, which "requires its own ADR and device result" — this ADR sets that number (§10) and
  does not raise the generic 4 MiB ceiling.
- **Where the process already is.** Default idle RSS was 13,404 KiB at Phase 0 and 14,956 KiB
  after Phase 4 (17,132 KiB with history enabled). Every MiB WASM adds is ~7% of the runtime.
- **Targets.** `x86_64-unknown-linux-gnu`, and the three Linux ARM CI targets:
  `aarch64-unknown-linux-gnu`, `armv7-unknown-linux-gnueabihf`, `armv7-unknown-linux-gnueabi`.
- **MSRV.** `rust-version = "1.88"` in the root manifest.
- **Non-goals (prompt).** No WASI with ambient capabilities, no npm/Node.js plugin layer, no
  default network/filesystem, no claim that sandbox tests prove the absence of runtime bugs.
- **Invariants (AGENTS.md, ADR-0001).** Never fake support; fail loudly; one transactional
  deploy writer; secrets never leave the credential service.

## 2. Measured evidence

### 2.1 Method

`adoption/phase7/spike/run.sh` builds one binary per runtime with the EdgeLinkd `ci`/`release`
size profile (`opt-level = "z"`, fat LTO, one codegen unit, stripped) and the same harness
skeleton. `none` is the skeleton without a runtime, so each delta is the cost of the runtime and
its glue, not of `std`. Each runtime gets the same embedded-style configuration:

- fuel metering per message, plus a 100 ms wall-clock deadline (Wasmi: fuel slices of 10⁶
  through resumable calls; Wasmtime: epoch interruption with a 10 ms ticker);
- linear memory capped at 1 MiB per store, `trap_on_grow_failure`, one memory/table/instance;
- Wasmtime without the 4 GiB reservation or guard region (explicit bounds checks), SIMD off;
- Wasmi with eager validation/translation, start functions disallowed, SIMD/memory64 compiled out;
- only `edgelink:node/v1::emit` linkable; any other import is rejected before linking.

Guests are WAT (`spike/guests/`), converted by `spike/guestgen`, which also generates
`bulk.wasm`: 1,500 functions, 138,547 bytes, standing in for a compiled Rust guest.

Modes: `idle` (engine constructed, nothing compiled; median of 5), `load` (compile `upper` and
`bulk`, 16 instances, one call each, RSS one second later), `bench` (2,000 calls with a 1 KiB
message), `hostile` (§2.4).

Host: `andorxps`, Intel Core i9-14900K, Linux 7.0, `rustc 1.99.0`. These are warm numbers on a
busy workstation. Sizes and RSS repeat within ±30 KiB across reruns; per-call latency varies by
up to 3× between runs and is indicative only (ranges shown where a runtime was run twice).

### 2.2 Size and memory (x86-64 host)

Raw lines: `spike/results/host-x86_64.jsonl` (three runs, concatenated in order). Two harness
fixes landed between runs and touch only the runtime-only variant: run 2 added `std` to it
(without `std` Wasmtime expects embedder-provided platform symbols), and run 3 precompiled every
guest (run 2 stopped after `bench` for lack of `spike.cwasm`). The checked-in sources were then
`cargo fmt`-ed and are clippy-clean with `-D warnings` for every feature.

| Runtime (features) | Stripped bytes | Δ vs `none` | Idle RSS (median) | Δ idle | RSS after compiling `upper`+`bulk` | 1 KiB call, median / p95 |
|---|---:|---:|---:|---:|---:|---:|
| `none` (skeleton) | 290,320 | — | 1,932 KiB | — | — | — |
| **Wasmi 2.0.0** (`std`,`validate`,`auto-dispatch`) | 1,298,536 | **+1,008,216 (0.96 MiB)** | 2,568 KiB | **+636 KiB** | **+892 KiB** | 9.3 µs / 11.1 µs |
| Wasmtime 36.0.17 LTS (`runtime`,`cranelift`) | 4,952,152 | +4,661,832 (4.45 MiB) | 4,384 KiB | +2,452 KiB | **+10,956 KiB** (HWM 15.1 MiB) | 5.5–5.9 µs / 6.0–6.6 µs |
| Wasmtime 36.0.17 + Pulley (interpreter) | 5,021,896 | +4,731,576 (4.51 MiB) | 4,396 KiB | +2,464 KiB | +9,784 KiB | 41.9 µs / 69.9 µs |
| Wasmtime 36.0.17 runtime-only (`runtime`,`std`; loads `.cwasm`) | 831,000 | +540,680 (0.52 MiB) | 2,484–2,508 KiB | +552 KiB | +1,236 KiB | 2.1–7.1 µs / 7.1–8.4 µs |
| Wasmtime 48.0.5 LTS (`runtime`,`cranelift`) | 5,764,256 | +5,473,936 (5.22 MiB) | 4,840 KiB | +2,908 KiB | +11,012 KiB (HWM 16.3 MiB) | 2.4 µs / 5.6 µs |

Other load figures:

| Runtime | Compile `bulk.wasm` (138 KiB) | Instantiate | Per-instance RSS (1 page touched) |
|---|---:|---:|---:|
| Wasmi 2.0.0 | 11.6 ms | 56 µs | 83 KiB (64 KiB page is heap-allocated) |
| Wasmtime 36 Cranelift | 190–201 ms | 3 µs | 4 KiB (page is mmap'd lazily) |
| Wasmtime 36 Pulley | 189 ms | 5 µs | 7 KiB |
| Wasmtime 36 runtime-only | 2.7–3.5 ms (deserialize) | 18–40 µs | 8 KiB |

Reading the table:

1. Cranelift-based Wasmtime fits the 8 MiB *binary* cap, but its **memory** does not fit this
   device class: +2.4 MiB with only an engine, then +10.7 MiB retained after compiling one
   realistic plugin — +13.1 MiB in total, which would grow the Phase 4 default process
   (14,956 KiB) by ~90% for a single plugin. Phases 1–4 together added 1,552 KiB. Wasmtime 48
   is larger still (+5.2 MiB binary, +2.8 MiB idle, +10.8 MiB after compile).
2. Pulley is no rescue: it still carries Cranelift on the device (same size and compile memory)
   and, on this host, ran the message path **4.5× slower than Wasmi**.
3. Wasmtime runtime-only is the smallest option and fast, but it cannot compile: it only loads
   precompiled artifacts, which is the trust problem in §3.2.
4. Wasmi costs under 1 MiB of binary and ~0.6 MiB of idle RSS (+1.5 MiB in total with the
   138 KiB plugin compiled, ~10% of the default process), compiles 16× faster than Cranelift
   with ~12× less retained memory, and runs this message-transform workload 1.6–3.8× slower
   than Cranelift code (Wasmtime 36 / 48). That trade is the right one for message-transform
   nodes on embedded hardware; heavy numeric workloads are not the target of this SDK.

### 2.3 ARM (build-only)

Raw lines: `spike/results/armv7-gnueabihf.jsonl`. Cross-built on the same host with
`arm-linux-gnueabihf-gcc`. No ARM device or emulator was available, so these are **sizes only**.

| Runtime (features) | Stripped bytes | Δ vs `none` |
|---|---:|---:|
| `none` (skeleton) | 285,808 | — |
| **Wasmi 2.0.0** | 1,137,776 | **+851,968 (832 KiB)** |
| Wasmtime 36.0.17 + Cranelift + `pulley` | 3,411,304 | +3,125,496 (2.98 MiB) |
| Wasmtime 36.0.17 + Cranelift | 3,407,208 | +3,121,400 (2.98 MiB) |

Both Wasmtime builds link for armv7, but Cranelift has no ARM32 backend (§3.1): on the device
they can only compile to Pulley bytecode and interpret it, which was the slowest configuration
measured on the host. Neither was executed. aarch64 and soft-float armv7 were not built here
(no cross linkers installed on the host); Wasmi is pure Rust with no C build step, so no
target-specific obstacle is expected, but that is G1/CI evidence still to collect.

### 2.4 Hostile-guest containment (x86-64 host)

Every runtime contained every guest. Elapsed times are wall-clock from compile to failure.

| Guest | What it does | Wasmi 2.0.0 | Wasmtime 36 (Cranelift) | Wasmtime 36 (Pulley) |
|---|---|---|---|---|
| `spin` | infinite loop in `el_on_input` | fuel budget (5·10⁷) exhausted, 39 ms | fuel exhausted, 29–38 ms | deadline (interrupt), 101 ms |
| `spin`, fuel 10¹¹ | same, fuel effectively unlimited | **deadline** after 1.36·10⁸ fuel, 100.5 ms | epoch interrupt, 101 ms | epoch interrupt, 101 ms |
| `grow` | `memory.grow` until refused | trap at 1 MiB cap, 0.5 ms | trap at 1 MiB cap, 1 ms | trap at 1 MiB cap, 1 ms |
| `bigout` | `emit` of 1 MiB, then length −16 | rejected (> 64 KiB cap) | rejected | rejected |
| `wasi` | imports `wasi_snapshot_preview1::fd_write` | rejected before link | rejected before link | rejected before link |
| `bindgen` | imports `wbg::*` (wasm-pack output) | rejected before link | rejected before link | rejected before link |
| `startspin` | infinite loop in a start function | **rejected at compile** (start functions disallowed) | fuel exhausted during instantiation, 14–16 ms | interrupt during instantiation, 98 ms |

Wasmi's resumable out-of-fuel calls (`TypedResumableCall::OutOfFuel`, available since 0.45)
are what make a wall-clock deadline and cooperative cancellation possible in an interpreter
without epochs: the host regains control every fuel slice, checks the deadline and the
cancellation token, then grants another slice or abandons the call.

## 3. Sourced facts (checked 2026-10-05)

### 3.1 Wasmtime

- **Tiers** ([stability tiers](https://docs.wasmtime.dev/stability-tiers.html)):
  `x86_64-unknown-linux-gnu` Tier 1; `aarch64-unknown-linux-gnu` **Tier 2** (missing:
  continuous fuzzing); `armv7-unknown-linux-gnueabihf` **Tier 3** (missing: CI testing,
  full-time maintainer); `armv7-unknown-linux-gnueabi` not listed. Pulley is a Tier 2 execution
  backend.
- **Security scope** ([policy](https://docs.wasmtime.dev/security-what-is-considered-a-security-vulnerability.html)):
  "Bugs must affect a tier 1 platform or feature to be considered a security vulnerability."
  None of EdgeLinkd's ARM targets is Tier 1. (In practice advisories have been issued for
  aarch64, e.g. GHSA-jhxm-h53p-jm7w / CVE-2026-34971, Critical 9.0: a Cranelift miscompile on
  aarch64 that escapes the sandbox when 64-bit memories run with signals-based traps disabled —
  the no-virtual-memory setting embedded hosts reach for. Fixed in 36.0.7.)
- **No 32-bit native backend.** Cranelift has no ARM32 backend; armv7 runs only through
  Pulley (`pulley32`), and Pulley bytecode is produced by Cranelift, so the device needs
  Cranelift or a trusted precompiler ([platform support](https://docs.wasmtime.dev/stability-platform-support.html),
  [Pulley](https://docs.wasmtime.dev/examples-pulley.html)). Pulley bytecode is not portable
  across Wasmtime versions.
- **Precompiled artifacts are trusted code.** `Module::deserialize`/`deserialize_file` are
  `unsafe`; the docs warn that arbitrary input "could… replace valid compiled code with any
  other valid compiled code" ([docs.rs](https://docs.rs/wasmtime/49.0.2/wasmtime/struct.Module.html)).
- **Release/MSRV policy** ([release](https://docs.wasmtime.dev/stability-release.html)): a major
  release monthly; every 12th is LTS with 24 months of fixes, others 2 months. Supported LTS:
  36 (MSRV 1.86, ~Aug 2027) and 48 (MSRV 1.95). Only **36** fits MSRV 1.88; 48+ forces a
  project-wide MSRV bump. Latest stable is 49.0.2 (MSRV 1.96).
- **Advisory cadence.** 2025–2026 GHSAs include Critical sandbox escapes (Cranelift aarch64,
  Winch), component-model memory-safety bugs, WASI permission bypasses, and many DoS/fuel
  accounting issues. Each is fixed promptly; the consequence for a fleet is a monthly-or-better
  patch obligation.

### 3.2 Why runtime-only Wasmtime is not a sandbox for third-party code

The only Wasmtime build that fits the budget loads `.cwasm` produced elsewhere. Because loading
is `unsafe` and the artifact is native code (or version-locked Pulley bytecode), the sandbox
boundary moves from the runtime to whoever produced the artifact. EdgeLinkd would need a
trusted build service per Wasmtime version and target, artifact signing, and key management —
infrastructure that does not exist and that this phase must not invent. Rejected.

### 3.3 Wasmi

- **Release.** 2.0.0, 2026-09-01: new IR and executor (~2.2× faster than 1.0), fuel tied to
  input Wasm operators, `validate` feature. MSRV **1.86** (fits 1.88). License **MIT/Apache-2.0**.
  Runtime dependency tree with the selected features (`cargo tree`): `wasmi`, `wasmi_core`,
  `wasmi_ir`, `wasmi_collections`, `wasmparser 0.228` (Apache-2.0 WITH LLVM-exception OR
  Apache-2.0 OR MIT), `spin` (MIT), `libm` (MIT), `bitflags` (MIT OR Apache-2.0). No C code,
  no build-time native compiler, pure Rust on every target.
- **Limits API.** `Config::consume_fuel`, `Store::set_fuel`, `call_resumable` →
  `OutOfFuel(..).resume(..)`; `StoreLimitsBuilder::{memory_size, table_elements, instances,
  tables, memories, trap_on_grow_failure}`; `Config::{set_max_recursion_depth,
  set_max_stack_height, enforced_limits, allow_start_fn, compilation_mode, floats}`.
- **Audits.** SRLabs (v0.31.0, 2023-12) and Runtime Verification (v0.36–0.38, 2024-11). **2.0
  is a rewrite not covered by either audit.**
- **Advisories.** GHSA-75jp-vq8x-h4cq / CVE-2024-28123 (Critical, host calling Wasm with >128
  params, fixed 0.31.1 — EdgeLinkd never does this); GHSA-g4v2-cjqp-rfmq / CVE-2025-66627
  ([High 8.4](https://github.com/wasmi-labs/wasmi/security/advisories/GHSA-g4v2-cjqp-rfmq),
  use-after-free in linear memory growth, 0.41.0–1.0.0, fixed in 1.0.1). 2.0.0 is unaffected.
- **Maintenance risk.** Effectively one maintainer; the 2.0 announcement states the Stellar
  sponsorship ends in October 2026 and the author intends to continue. This is the main risk of
  the decision and is mitigated in §12.
- **No component model.** That is acceptable: ABI v1 is a plain core-module interface (§6),
  which also keeps the guest side runtime-neutral (a future host could swap runtimes without
  breaking packages).

### 3.4 Others

- **WAMR** (Apache-2.0 WITH LLVM-exception) is the smallest (≈56–59 KiB interpreter on
  Cortex-M4F), but it is C: a memory-unsafe sandbox core plus a cross C toolchain on three ARM
  targets, and `wamr-rust-sdk` is not published on crates.io (git dependency, last tag 2024).
- **wasm3**: the C project is active again (v0.9.x, 2026), but the Rust crate is frozen at
  0.3.1 (2021).

## 4. Options considered

| Option | Binary Δ | Idle Δ | ARM coverage | MSRV 1.88 | Untrusted `.wasm` on device | Verdict |
|---|---:|---:|---|---|---|---|
| Wasmtime 36 LTS + Cranelift | 4.45 MiB (armv7: 2.98) | 2.4 MiB (+10.7 after one compile) | aarch64 T2, armv7 via Pulley T3, armel none | yes, until ~2027-08 | yes | **Reject** (memory, ARM, security scope) |
| Wasmtime 48 LTS + Cranelift | 5.22 MiB | 2.8 MiB (+10.8 after one compile) | same | **no** (1.95) | yes | **Reject** |
| Wasmtime + Pulley | 4.51 MiB | 2.4 MiB | same, slower than Wasmi | as above | yes | **Reject** |
| Wasmtime runtime-only + `.cwasm` | 0.52 MiB | 0.5 MiB | needs per-target trusted precompiler | yes | **no** (trusted artifacts) | **Reject** (§3.2) |
| Winch baseline compiler | not measured | not measured | x86-64/aarch64 only; incompatible with epochs and signals-off | as above | yes | **Reject** |
| **Wasmi 2.0** | **0.96 MiB** (armv7: 0.81) | **0.6 MiB** | pure Rust, no C; armv7hf build verified (§2.3) | **yes** | **yes** | **Adopt for prototype** |
| WAMR via `wamr-rust-sdk` | small | small | C cross toolchain ×3 | n/a | yes | Reject (C core, unpublished binding) |
| wasm3 crate | small | small | unmaintained binding | n/a | yes | Reject |
| No plugin SDK | 0 | 0 | — | — | — | Valid fallback if §11 fails |

## 5. Guest toolchain (relation to MDN *Compiling from Rust to WebAssembly*)

The [MDN guide](https://developer.mozilla.org/en-US/docs/WebAssembly/Guides/Rust_to_Wasm) builds
a `cdylib` with `wasm-pack` and `#[wasm_bindgen]`, producing a `.wasm` plus generated JS glue,
TypeScript definitions and an npm `package.json`, for browsers and bundlers. Only the first half
of that pipeline applies here:

| MDN step | EdgeLinkd guest |
|---|---|
| `crate-type = ["cdylib"]` | **Same.** |
| `cargo` → `wasm32-unknown-unknown` | **Same**, `cargo build --release --target wasm32-unknown-unknown` (or `wasm32v1-none` for MVP-only `no_std` guests). |
| `wasm-bindgen` / `#[wasm_bindgen]` | **Not used.** Its imports (`wbg`, `__wbindgen_*`) need JS glue that EdgeLinkd does not run; installation rejects them with a message naming wasm-bindgen (spike guest `bindgen.wat`). |
| `extern "C" { fn alert(..) }` from JS | Replaced by `#[link(wasm_import_module = "edgelink:node/v1")] extern "C" { fn emit(..) -> i32; }` |
| `wasm-pack build --target web/bundler`, npm publish | **Not used.** No npm layer (prompt non-goal). The package is the single `.wasm` file (§8). |

Guest guidance: Rust's default 1 MiB shadow stack needs ≥17 pages, so a default Rust guest
needs the 2 MiB default memory cap (§7); set `-C link-arg=-zstack-size=65536` to run in less.
A small `edgelink-wasm-guest` crate (`no_std` + `alloc`, value codec, typed imports) is part of
the prototype, not of the runtime build.

## 6. ABI `edgelink:node/v1`

Experimental. The host supports the current ABI and **one previous** ABI version; a removal
requires a release note and an explicit migration path, matching the plan's rollback rule.

**Module requirements** (validated at install, rejected otherwise):

- Core Wasm 32-bit; one exported `memory`, no imported memory/table/global, no shared memory,
  no start function.
- Allowed proposals: MVP, mutable globals, sign-extension, saturating float-to-int,
  multi-value, bulk memory, reference types — the set current Rust `wasm32-unknown-unknown`
  emits by default. Rejected: SIMD, threads, memory64, multi-memory, exceptions, tail calls,
  GC, component model.
- Module ≤ 512 KiB; Wasmi `EnforcedLimits::strict()` (≤ 10,000 functions, ≤ 1,000 globals,
  1 memory, ≤ 32 params/results, ≥ 40 bytes average per function body); recursion depth ≤ 256.
  `strict()` was not enabled in the spike and must be checked against real Rust guests.
- Imports only from `edgelink:node/v1`, only names listed below, exact signatures.

**Exports**

| Export | Signature | Contract |
|---|---|---|
| `el_abi_version` | `() -> i32` | Returns `1`; must match manifest `abi`. |
| `el_alloc` | `(len: i32) -> i32` | Returns a guest pointer for `len` bytes; host bounds-checks `[ptr, ptr+len)`. |
| `el_init` | `(ptr: i32, len: i32) -> i32` | Validated node configuration (value encoding below); `0` = ready. |
| `el_on_input` | `(ptr: i32, len: i32) -> i32` | One message; `0` = done, non-zero = failure (see `fail`). |
| `el_close` | `() -> ()` (optional) | Called on stop under the same fuel/deadline. |

**Imports** (the entire host surface of v1)

| Import | Signature | Bounds |
|---|---|---|
| `emit` | `(port: i32, ptr: i32, len: i32) -> i32` | `port < outputs`; ≤ 64 KiB each; ≤ 16 per message; ≤ 256 KiB total. |
| `log` | `(level: i32, ptr: i32, len: i32)` | levels debug/info/warn/error; ≤ 512 B; ≤ 16 per message; control characters stripped. |
| `status` | `(fill: i32, shape: i32, ptr: i32, len: i32)` | Node-RED fills/shapes enum; text ≤ 128 B. |
| `fail` | `(ptr: i32, len: i32)` | Error text ≤ 1 KiB; becomes a catchable node error for the input `msg`. |

Any bound violation traps the call with a host error; the instance is discarded (§7).

**Value encoding (EVE/1).** EdgeLinkd's `Variant` serde is lossy (Buffer → number array, Date →
millis, RegExp → string), so v1 uses a small tagged binary encoding instead of JSON: null,
bool, i64, u64, f64 (finite only), UTF-8 string, bytes, array, object (unique keys), date (ms),
regexp (source). Decoding is bounded before allocation: depth ≤ 32, every count ≤ remaining
bytes, total ≤ the message cap. Input message cap: 64 KiB by default, configurable to 1 MiB.

**Message ownership.** The guest receives the message body. `_msgid` is host-owned: an emitted
object without `_msgid` gets the input's; a different `_msgid` is rejected. `link_call_stack`
is never shown to the guest and is restored by the host. Values that cannot be encoded (live
HTTP `req`/`res` handles) make the call fail loudly; they are never silently dropped.

## 7. Limits, scheduling and failure isolation

| Limit | Default | Admin ceiling | Enforcement |
|---|---:|---:|---|
| Linear memory per instance | 2 MiB (32 pages) | 16 MiB | `StoreLimits` + `trap_on_grow_failure`; manifest may request less or up to the ceiling. |
| Tables / elements | 1 / 10,000 | same | `StoreLimits` |
| Fuel per message | 2·10⁷ | 10⁹ | Wasmi fuel, granted in slices of 10⁶ |
| Wall-clock per message | 250 ms | 5 s | checked between fuel slices |
| Fuel / time for `el_init`, `el_close`, self-test | same as a message | — | same |
| Concurrency | 2 global permits; 1 per node | `cores` | `tokio::sync::Semaphore`; calls run on `spawn_blocking` while holding a permit |
| Global WASM memory budget | 8 MiB | admin | admission at flow start: Σ(max linear memory) + Σ(translated-code estimate, 8× module bytes) |

Defaults are **provisional**: fuel and deadline are calibrated on the §11 device. On the host,
Wasmi executed 1.36·10⁸ fuel per 100 ms of `spin`, so one 10⁶ slice ≈ 0.7 ms here; ARM boards
are expected to be several times slower, which keeps cancellation latency at a few ms.

**Cancellation.** Each flow owns a cancellation token. Stop/redeploy sets it; a running call is
abandoned at the next slice boundary, its instance dropped, its permit released. Node stop waits
at most deadline + one slice. Host imports never block.

**Isolation.** Each node owns one store/instance; instances share nothing. A trap, a limit
violation, or a host-side panic in the call (`spawn_blocking` `JoinError`) fails that message
with a node error and discards the instance; the next message re-instantiates (56 µs on the
host). Three failures within 60 s put the node in a failed state (red status, every message
errors without executing) until redeploy, so a crashing plugin cannot burn CPU in a loop.
Other nodes and flows are unaffected.

Guest state persists across messages within one instance (like function-node memory) and is
lost on redeploy, restart, or trap.

## 8. Packages, integrity and authorization

**Format: one `.wasm` file.** The manifest is a custom section named `edgelink.manifest`
(TOML, ≤ 16 KiB). There is no archive, so archive path traversal, symlink entries and partial
extraction do not exist by construction. The host walks the section headers itself (a few dozen
lines; no extra crate).

**Manifest (schema 1)**

```toml
[plugin]
id = "acme/csvparse"        # publisher/name, each [a-z][a-z0-9]{0,31}; no dashes
version = "1.2.0"           # semver
abi = 1
license = "MIT"             # SPDX expression, shown to the admin
description = "Parse CSV"   # plain text, ≤ 256 B
capabilities = []           # v1: must be empty, see §9

[limits]                    # requests; install fails above the admin ceiling
memory_pages = 16
fuel_per_message = 5000000
deadline_ms = 100

[node]                      # v1: exactly one node per package
label = "csv parse"
category = "plugins"
color = "#C0DEED"           # validated hex
icon = "parser-csv.svg"     # from the bundled Node-RED icon set only
inputs = 1
outputs = 2
output_labels = ["rows", "errors"]
help = "Plain text, rendered as text."

[[node.config]]
name = "delimiter"          # [a-z][a-zA-Z0-9]{0,31}; not a reserved Node-RED property
kind = "string"             # string | number | boolean | enum
default = ","
max_len = 4
required = true

[[selftest]]                # optional, ≤ 4 vectors, run at install
input = { payload = "a,b" }
expect_outputs = [1, 0]     # emitted message count per port
```

**Node identity.** Type name = `wasm-<publisher>-<name>` (e.g. `wasm-acme-csvparse`). Segments
contain no dashes, so the mapping is injective; no built-in type starts with `wasm-` (drift
test). The editor writes `wasmPlugin: "acme/csvparse@1"` into new nodes; if present, the active
plugin's major version must match or deploy fails with `NotSupported`. Config fields named
`credentials`, or of a secret kind, are rejected: ABI v1 has no secrets.

**Integrity and authorization.**

- Identity of a generation = SHA-256 of the exact `.wasm` bytes (`sha2` is already a workspace
  dependency).
- Only an authenticated **admin** can install (Phase 3 route class `plugins`: body ≤ 1 MiB,
  concurrency 1, 6/min, 30 s), or someone with shell access to the device via
  `edgelinkd plugin install <file>` (same code path, same validation).
- Two-step consent: `stage` returns `{id, version, sha256, license, requested limits, selftest
  result}`; `activate` must echo the same `sha256`, closing the time-of-check/time-of-use gap.
- v1 trust model is **local-admin**: no remote fetch, no registry, no fleet distribution of
  plugins. Publisher signatures (Ed25519 over the SHA-256, trusted keys in configuration) are a
  prerequisite for any remote distribution and are out of v1; a `require_signature = true`
  setting fails startup with `NotSupported` rather than being accepted and ignored.
- Audit log and operational history (Phase 4) record `plugin.staged/rejected/activated/
  rolled_back/removed` with id, version, digest prefix and reason code — never bytes or config
  values.

## 9. Capability model

Default deny is **by construction**: the `Linker` defines only the four v1 imports, and the
installer rejects any other import with a message naming it before linking (spike:
`wasi`, `bindgen`). There is no WASI crate in the build.

| Authority | ABI v1 | Future grant (separate ADR amendment each) |
|---|---|---|
| Compute, emit, log, status, fail | always | — |
| Filesystem | **never linkable** | none planned |
| Network | **never linkable** | `net:http`, host-executed through the Phase 1 egress policy (`enforce`, new purpose `WasmPlugin`, per-plugin allowlist ∩ admin allowlist, bounded response) |
| Environment variables | **never** | none |
| Clock | **not linkable** (also denies timing measurement) | `clock:coarse` (ms, monotonic) |
| Randomness | **not linkable** | `random`, host CSPRNG, bounded bytes |
| Process / exec | **never** | none |
| Secrets / credentials | **never in guest memory** | host-side injection only (e.g. the host adds an `Authorization` header for `net:http`); the guest never sees the secret |
| Context | **not linkable** | `context:node-read` / `context:node-write`, memory store only (the `ai-agent` rule) |
| Deploy, flows, admin API, other nodes | **never** | none |

A manifest listing any capability is rejected in v1 with `unsupported capability '<name>' in
ABI 1`. Grants, when they exist, come only from admin configuration; a manifest can request,
never grant.

## 10. Exact resource budget for `nodes_wasm`

| Quantity | Budget | Basis |
|---|---:|---|
| Default / minimal / `full` build (feature off) | **0 dependencies**; ≤ 1 KiB code | Only the reserved-prefix arm in `edgelink_owned_node_type` remains, as for `ai-agent` in Phase 6. Proven by `cargo tree` and stripped size. |
| Stripped binary growth, feature on | ≤ **1.5 MiB** | Wasmi measured +0.96 MiB; ≤ 0.5 MiB for host glue. Well inside ADR-0001's 8 MiB. |
| Idle RSS, feature on, no plugin nodes deployed | ≤ **256 KiB** | Engine is created lazily on the first plugin node start and dropped with the last. |
| Idle RSS, engine live, no instances | ≤ **1 MiB** | measured +636 KiB |
| Translated code | ≤ 8× module bytes | measured ≈ 6.6× (+892 KiB for 135 KiB of Wasm) |
| Per instance | linear-memory cap + ≤ 32 KiB | measured 83 KiB with one 64 KiB page |
| All plugins together | ≤ 8 MiB (configurable) | admission control, §7 |
| Startup to healthy, feature on, no plugins | ≤ +5 ms | no engine at startup |

These apply to the x86-64 gate and to the §11 ARM device. A breach on the device stops the phase.

## 11. Gates

1. **G0 — this ADR accepted** by the maintainer. Passed 2026-10-05.
2. **G1 — ARM device run of the spike (before the prototype merges).** Copy
   `$SPIKE_OUT/target-armv7-unknown-linux-gnueabihf-wasmi/armv7-unknown-linux-gnueabihf/release/edgelink-wasm-spike`
   (or an aarch64 build) and `$SPIKE_OUT/guests/*.wasm` to a Raspberry Pi-class board and run
   `idle`, `load <guests> 16`, `bench <guests> 2000`, `hostile <guests>`, and `hostile` again
   with `SPIKE_FUEL_PER_CALL=100000000000`. Pass: Wasmi idle Δ ≤ 1 MiB, all hostile guests
   contained, deadline overshoot ≤ 25 ms, compile of `bulk.wasm` ≤ 500 ms (an install/start-time
   cost only). Record fuel-per-ms to set the §7 defaults. Fail → no-go, amend this ADR.
3. **G2 — prototype exit**: every hostile test in §12, ADR-0001 phase evidence, `nodes_wasm`
   off in all shipped feature sets, and the in-tree budget of §10 measured on the real
   `edgelinkd` binary (the spike measures the runtime, not EdgeLinkd's glue).

## 12. Prototype contract (what implementation must deliver once G0 passes)

**Feature gating.** Root `nodes_wasm = ["edgelink-core/nodes_wasm", "edgelink-web/nodes_wasm"]`;
core `nodes_wasm = ["dep:wasmi"]` with `wasmi = { version = "=2.0.x", default-features = false,
features = ["std", "stable", "validate", "auto-dispatch"] }` pinned exactly in the workspace.
Not in `default`, not in `full`, not in pymod. CI adds `cargo check --features nodes_wasm` on
Linux and the ARM cross job, and a `cargo tree --no-default-features` / default /
`--features full` assertion that `wasmi` is absent.

**Registry.** Lookup order: static inventory → active plugin set (feature on) → owned check
(`wasm-` prefix → `NotSupported`) → `unknown` backstop. Messages:

- feature off: `node type 'wasm-acme-csvparse' is not compiled in this build (requires nodes_wasm)`
- feature on, not installed/active: `node type 'wasm-acme-csvparse' requires WASM plugin acme/csvparse which is not active`
- major mismatch: `... requires acme/csvparse@1, active is 2.0.1`
- integrity failure at startup: `... plugin acme/csvparse sha256 mismatch; plugin disabled`

**Storage** (`$EDGELINK_HOME/plugins/`, directory 0700, files 0600, opened with `O_NOFOLLOW`;
a symlinked `plugins/` or child refuses to load):

```text
plugins/
  staging/<random>.part      upload in progress; deleted at startup
  quarantine/<sha256>.wasm   validated, never executed except by self-test; ≤ 8 files, ≤ 8 MiB
  store/<sha256>.wasm        immutable generations referenced by active.toml
  active.toml                { "acme/csvparse" = { current = "<sha256>", previous = "<sha256>" } }
```

**Lifecycle** (single writer, holding the same lock as `deploy.rs` so plugin activation and flow
deploys serialize; `flows.json` and the four-file write-set are never touched):

1. **Stage**: stream to `staging/*.part` with the size cap; fsync.
2. **Validate**: magic/version, section walk, manifest schema, identity regex, limits ≤ ceilings,
   empty capabilities, import allowlist, Wasm feature set, Wasmi eager compile.
3. **Quarantine**: rename to `quarantine/<sha256>.wasm`.
4. **Self-test**: instantiate under the strict limits, `el_abi_version`, `el_init` with manifest
   defaults, run each `[[selftest]]` vector, `el_close`. Any failure → `rejected`, file stays in
   quarantine for inspection.
5. **Activate** (`sha256` echoed by the admin): `Engine::prepare_flows` on the *current* graph
   with the candidate plugin set; rename `quarantine → store`; write `active.toml.tmp`, fsync,
   rename, fsync directory (current ← new, previous ← old current); restart the flows that use
   the type through the existing engine redeploy of the unchanged graph. Restart failure →
   restore the previous `active.toml`, restart again, report the error. The editor catalog,
   `/nodes` HTML/JSON and Copilot metadata change only after success.
6. **Rollback**: swap `current`/`previous` through step 5. **Remove**: refused (409) while any
   deployed node uses the type; otherwise the entry leaves `active.toml` and the file moves back
   to quarantine, preserved for re-enable but never executed automatically.
7. **Restart**: read `active.toml`, verify each referenced digest, delete `staging/`, never
   promote quarantine. With `nodes_wasm` off, `plugins/` is neither read nor modified.

**Editor and Copilot.** The host generates each plugin node's editor definition from the
manifest (`RED.nodes.registerType` with defaults from `[[node.config]]`, help rendered as text,
icon from the bundled set). **No third-party HTML or JavaScript reaches the editor**: plugin JS
would run with the admin's session and could deploy an `exec` node, defeating the sandbox.
Copilot metadata (schema v1) includes type, ports, labels and config field names/kinds only;
plugin free text (description, help) is excluded from prompts as a prompt-injection channel.

**Hostile tests** (all from the prompt, mapped to where they run):

| Area | Tests |
|---|---|
| Valid path | minimal plugin round trip; multi-port emit; config init; state across messages |
| ABI/manifest | unknown ABI, ABI/manifest mismatch, malformed/oversized/missing manifest, duplicate identity, digest re-stage no-op, unsupported capability, forbidden Wasm features, start function, imported memory |
| CPU | infinite loop (fuel), loop with huge fuel (deadline), loop during `el_init`/`el_close`, cancellation on stop/redeploy within deadline + slice |
| Memory | `memory.grow` bomb, allocation bomb through `el_alloc`, table growth, deep recursion, module over 512 KiB, global budget admission |
| Values | oversized input, output, emit count, log, status, fail text; malformed EVE/1 (bad tags, counts > remaining, depth 33, invalid UTF-8, NaN, duplicate keys); `_msgid` forgery |
| Authority | imports for each forbidden class (WASI fs/env/clock/random/proc, `wbg`, unknown `edgelink:*` names, wrong signatures) |
| Packaging | id/publisher traversal (`../`, `/`, uppercase, dashes), symlinked `plugins/` and children, truncated upload, crash at every rename, restart ignoring `staging/` and quarantine |
| Isolation | trap and host panic isolation; three-strike failed state; concurrency exhaustion (global and per node) with back-pressure |
| Lifecycle | A → B → failed C keeps B current and A previous; rollback B → A and restart; disable feature → non-WASM flows run with no engine created; flow needing missing/disabled plugin fails naming id and version; packages preserved for re-enable |
| Build | default/minimal/full contain no `wasmi`, no editor entries, no API routes, no catalog entries |
| Fuzz/property | manifest parser, section walker, EVE/1 decoder, host-import argument validation |
| Resources | binary size, compile time, startup, idle RSS, per-instance peak on host and the G1 device |

**Wasmi risk mitigations.** Exact version pin; `cargo audit`/GHSA watch for `wasmi` and
`wasmparser`; fuzz the host-facing surface in CI; keep ABI v1 runtime-neutral so the host can
move to another runtime (including a future Wasmtime Tier 1 ARM story) without breaking
packages; if Wasmi becomes unmaintained or a 2.x advisory is unfixable, disable `nodes_wasm`
(the feature is off by default, so shipped binaries are unaffected).

## 13. Consequences

- The default, minimal and `full` binaries are unchanged; operators opt in at build time.
- Plugins are pure functions over messages with node-local state. Anything needing I/O is
  still a native node or a future, separately reviewed capability.
- Interpreted execution is 1.6–3.8× slower than Wasmtime's compiled code on the measured
  workload and will be slower still for compute-heavy code. That is accepted for message
  transforms on embedded hardware.
- EdgeLinkd owns a small hand-written ABI and value codec instead of WIT bindings. That costs a
  guest SDK crate and fuzzing, and buys runtime independence and a much smaller host.
- If the G1 device run fails, the phase ends as a documented no-go and nothing ships.

## 14. Unverified boundaries

- ARM execution: G1 passed on a Raspberry Pi 5 (arm64), see Amendments. A 32-bit ARM board
  (armhf/armel) and slower boards are still unmeasured; armv7 numbers are build-only.
- Wasmi 2.0 has no third-party audit; the 2023/2024 audits cover older code.
- Host latency figures come from a busy workstation and are indicative only; RSS and sizes
  were stable across reruns.
- The spike measures runtimes inside a minimal skeleton. In-tree deltas will differ slightly
  (shared dependencies, EdgeLinkd glue) and must be re-measured at G2.
- Windows was not built; the runtime is pure Rust, but plugin storage `O_NOFOLLOW`/rename
  semantics need the Windows compile/test gate at implementation time.

## 15. Reproduction

```sh
# x86-64 host: build and measure every candidate (outputs JSON lines; products in /tmp)
adoption/phase7/spike/run.sh | tee /tmp/phase7-host.jsonl

# ARM cross-build sizes (build-only)
SPIKE_TARGET=armv7-unknown-linux-gnueabihf adoption/phase7/spike/run.sh none wasmi wt36-pulley wt36

# One runtime only
adoption/phase7/spike/run.sh none wasmi
```

`SPIKE_OUT` relocates build products (default `/tmp/edgelinkd-phase7-spike`). The spike
package has its own `[workspace]` and `Cargo.lock`; it is never built by the main workspace.

## 16. Prompt checklist

| `phase7prompt.md` design-review item | Section |
|---|---|
| Runtime/dependency choice and license | §3, §4, §12 (feature gating) |
| Versioned ABI/component model and support window | §6 (core-module ABI v1, current + one previous) |
| Manifest schema, node identity, configuration, typed metadata, named ports | §8, §12 (editor and Copilot) |
| Message encoding and strict input/output size bounds | §6 (EVE/1, import bounds) |
| Capability model and default-deny host imports | §9 |
| Memory, fuel, epoch/time, concurrency and cancellation limits | §7 |
| Package origin/integrity/signature policy and local installation authorization | §8 |
| Atomic quarantine, validation, self-test, activation, rollback, removal | §12 (storage, lifecycle) |
| Cross-platform/ARM support and exact embedded resource budget | §2.3, §3.1, §10, §11 |
| Behavior when a required plugin or ABI is unavailable | §12 (registry messages) |
| Stop conditions (prompt) | Wasmtime fails the §10 memory budget and reaches armv7 only through a Tier 3 interpreter path outside its security policy. Wasmi is re-tested against every stop condition at G1/G2. |

## Amendments

- 2026-10-05, with acceptance (details in `DESIGN.md`): the admin API lives under
  `/wasm/plugins…`; activation redeploys the whole graph rather than only affected flows (§12
  step 5); `[runtime.wasm] enabled` defaults to `false` even when `nodes_wasm` is compiled, and
  while off every `wasm-*` type fails deploy with `NotSupported`; float instructions are allowed
  in ABI v1 and SIMD stays rejected.
- 2026-10-05, G1 (§11) passed on a Raspberry Pi 5 Model B (arm64): Wasmi idle Δ +352 KiB,
  6/6 hostile guests contained, deadline overshoot 0.3 ms, `bulk.wasm` compile 30.4 ms; fuel
  throughput 4.8·10⁵/ms. §7 defaults are kept. Evidence: `adoption/phase7/REPORT.md`,
  `spike/results/pi-arm64.jsonl`.
- 2026-10-06, after the live test: the default linear-memory cap per instance (§7) is 512 KiB
  (8 pages) instead of 2 MiB, so the 8 MiB default budget admits about 15 plugin nodes rather
  than 3. The 16 MiB ceiling is unchanged. A module whose initial memory exceeds its limit is
  rejected at `stage` with the remedy (request `[limits] memory_pages`, or link Rust guests with
  `-zstack-size=65536`, §8); boards with spare RAM raise `memory_budget_kib`.

## Git and release state

Documentation and a standalone measurement spike only. No runtime source, dependency,
workspace `Cargo.lock`, feature, version, live flow, credential or configuration was changed.
Nothing was committed, pushed, tagged, published, released or deployed.
