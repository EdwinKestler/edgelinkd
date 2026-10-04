# Phase 0 Baseline Report

- Status: closed locally; uncommitted
- Date: 2026-10-03 (America/Guatemala)
- Branch: `master`
- Tested revision: `35e0546c68c6aed057e5ac246aac46b0844f457c`
- Revision subject: `feat: enable AI flow construction by default`
- Revision date: `2026-10-03T09:52:52-06:00`
- Version: `0.3.0` in the root, core, and web manifests
- Host: `x86_64-unknown-linux-gnu`
- Rust: `rustc 1.99.0 (b940084d7 2026-09-28)`, Cargo `1.99.0`
- Python: `3.12.3`; pytest `8.4.1`
- Node.js: `v24.15.0`
- Node-RED editor submodule: `v5.0.7`

This report closes Phase 0 of `adoption/z8adoptionplan.md`. It records a reference point; it
does not change product code, dependencies, persistent formats, runtime defaults, or the
published version.

## Architecture confirmation

`crates/web/src/handlers/deploy.rs` remains the single transactional writer used by `POST
/flows`, `POST /flows/rollback`, `POST /flow`, `PUT /flow/{id}`, and `DELETE /flow/{id}`. The
write-set is `flows.json`, `flows_cred.json`, `flows.json.prev`, and
`flows_cred.json.prev`. Candidate preparation precedes persistence. Activation failure restores
all four snapshots, while rollback prepares the previous graph before swapping and swaps back if
activation fails.

The outbound call-site, middleware, authentication, audit, Copilot, feature-gate, secret, and
persistence inventories are in `ADR-0001-adoption-baseline.md`.

## Build configurations and binary sizes

All sizes are the stripped `ci` profile executable at `target/ci/edgelinkd`, measured with
`stat -c %s`. Each row was built separately with the listed command on the same checkout.

| Configuration | Command | Bytes | Delta from default |
|---|---|---:|---:|
| Default | `cargo build --profile ci --locked` | 14,420,296 | baseline |
| Root no-default | `cargo build --profile ci --locked --no-default-features` | 14,259,480 | -160,816 (-1.12%) |
| Root `full` | `cargo build --profile ci --locked --features full` | 14,420,296 | 0 |
| All features | `cargo build --profile ci --locked --all-features` | 14,509,464 | +89,168 (+0.62%) |

The root no-default result is not a true minimal build: `edgelink-web` is a dependency with its
defaults enabled, and that crate depends on `edgelink-core` with core defaults. The small delta
therefore must not be used to claim that optional networking, parser, or storage code has been
removed. Correcting feature propagation is a separate product change, not Phase 0 work.

The root `full` feature adds `rqjs_bindgen` to the default set; the existing optimized binary was
the same measured size. `--all-features` additionally enables `nodes_modbus` and
`runtime_scan`.

## Runtime measurements

Measurements used a copied `adoption/phase0/fixtures` installation and the default stripped
binary. Each process was invoked as:

```sh
EDGELINK_HOME="$TEMP_INSTALL" target/ci/edgelinkd run --bind 127.0.0.1:19888
```

The explicit `--bind` is required because the current CLI default overrides the configuration
file's `ui-host` value. Startup is measured from process launch until `/api/health` first returns
success. Idle RSS is `VmRSS` after one second. These are warm filesystem-cache measurements.

| Measurement | Samples | Result |
|---|---:|---:|
| Startup to healthy | 5 | median 7 ms; range 6-8 ms |
| Idle RSS | 5 | median 13,404 KiB; range 13,056-13,484 KiB |
| `GET /api/health` | 100 sequential | median 0.337 ms; p95 0.466 ms; mean 0.331 ms |
| Flow Copilot draft with local deterministic provider | 20 sequential | median 1.216 ms; p95 1.727 ms; mean 1.260 ms |

The health response was `{"status":"healthy","service":"edgelink-web","version":"0.3.0"}`.
`GET /settings` reported `uiHost=127.0.0.1` and `uiPort=19888`. The Copilot sample traversed
the real `/assistant/draft` route, provider adapter, schema materialization, registry checks, and
`Engine::prepare_flows`; only the provider was a loopback deterministic mock. It returned an
empty add-only draft and never deployed.

### MQTT acceptance

The opt-in basic send/receive spec ran against the local RabbitMQ MQTT plugin. Existing
credentials were loaded into `EDGELINK_MQTT_USER` and `EDGELINK_MQTT_PASSWORD` without being
printed:

```sh
NR_MQTT_TESTS=1 \
  .venv/bin/pytest \
  tests/nodes/network/test_mqtt_node.py::TestMqttNodes::test_0004 -v
```

Result: `1 passed in 2.84s`; wall time `3.30s`. This proves an EdgeLinkd publish/subscribe
round trip through the broker. It is not a broker packet-latency metric because the upstream-style
harness intentionally schedules injection after 800 ms and observes the flow for two seconds.
No MQTT latency percentile is claimed.

## Validation evidence

| Command | Result |
|---|---|
| `cargo fmt --check` | passed |
| `cargo clippy --all-features --tests --all -- -D warnings` | passed |
| `cargo test --workspace --features full --no-fail-fast` | passed: core 248 passed/1 ignored; web 46 passed; 2 doctests passed; no failures |
| `cargo build --all` | passed |
| `.venv/bin/pytest ./tests -v` | 825 passed, 213 skipped in 114.23s |
| `cargo test -p edgelink-web --lib --features nodes_ai handlers::deploy::tests:: -- --test-threads=8` | 2 passed |
| `cargo test -p edgelink-web --lib --features nodes_ai handlers::flows::tests:: -- --test-threads=8` | 7 passed |
| `cargo test -p edgelink-web --lib --features nodes_ai handlers::assistant::tests:: -- --test-threads=8` | 5 passed |
| Default/no-default/full/all-feature `ci` builds | passed; sizes above |
| `CCACHE_DISABLE=1 cargo build --workspace --target armv7-unknown-linux-gnueabihf --features full --exclude edgelink-pymod` | passed |
| Live local MQTT basic send/receive | 1 passed |
| `git diff --check` | passed after final documentation refresh |
| `palimnex index --incremental` and `palimnex validate --deep` | 251 indexed files; deep validation passed and cache fresh |
| `palimnex ledger-status` | ready; integrity and scanner policy checks passed |

The first full Rust test attempt was constrained by the workspace sandbox's loopback-bind policy;
the identical command passed when rerun with local binding enabled. This was an execution
environment restriction, not a product failure.

`palimnex doctor` reports `action_needed` only for the existing intentional corpus boundary
(`3rd-party/`, two crate files, and the 50,000-file coverage-scan cap) and the optional retention
migration notice. Redis answered, the v3 cache was fresh, and the ledger was ready. Redis was not
flushed.

## ARM boundary

`armv7-unknown-linux-gnueabihf` was installed and the full workspace cross-build passed with the
Python extension excluded. The cross linker was `arm-linux-gnueabihf-gcc`; ccache was disabled
because its configured cache directory is outside the writable workspace.

The produced debug ARM executable was 357,154,752 bytes and identified as 32-bit ARM EABI5 with
debug symbols. It is not comparable with the stripped `ci` host sizes. No QEMU user/system binary
was available, so ARM runtime tests, RSS, startup, rollback, crypto/filesystem semantics, and
latency remain unverified. Scheduled/manual CI and representative hardware are still required
before release.

## Sanitized fixtures

`adoption/phase0/fixtures` contains a dormant MQTT broker, AI provider, and HTTP request plus the
matching current/previous credential sidecars. `autoConnect=false`, no injection is scheduled,
all external URLs use the reserved `example.invalid` domain, and every credential begins with
`fixture-` and is intentionally invalid.

Fixture hashes at closure:

| File | SHA-256 |
|---|---|
| `flows.json` | `69f55aed909634a7ae647c66c8eee0c02d292c865aba9278def772919fac0d39` |
| `flows_cred.json` | `202cb124b051fa335fb9af7a6513a7c69d2b59c2a9d5788a588f45a3bb81d569` |
| `flows.json.prev` | `5c538b2472ab16bbd35efcb79bb67fb8ac77a7d9a29114d635eb32b33c0ee3e8` |
| `flows_cred.json.prev` | `6d2b7b2d18407e9d1ef53f79bed6ccaf8a0599659873263536fd4c0391224f55` |

## Rollback drill

The fixture was copied to `/tmp`; all four files were copied to a baseline directory; the
current pair was replaced by the previous pair; then the saved current and previous pairs were
restored. SHA-256 comparison of all four restored files matched the baseline exactly.

Result: mutation observed, four-file restoration passed, no repository or live runtime file was
used or modified. Phase 0 itself rolls back by removing only `adoption/phase0` and reverting the
Phase 0 status lines in the controlling plan; it has no product or persistent-state migration.

## Unverified boundaries

- ARM execution and measurements; the local result is build-only.
- Windows runtime and non-Unix atomic replacement behavior.
- Browser/editor interaction for these fixtures.
- External OpenAI, Anthropic, xAI, or Cortex latency and provider drift; Copilot used a mock.
- Full live MQTT matrix; Phase 0 reran only the basic publish/subscribe acceptance. Known
  broker-specific QoS 2 and expiry behavior remains outside this result.
- Cold-cache and constrained-device resource measurements.
- A true root minimal build, because current feature propagation retains core defaults.

## Git and release state

Phase 0 is implemented only as documentation and sanitized fixtures in the working tree. No
runtime source, dependency, generated file, `Cargo.lock`, version, live flow, live credential,
or service configuration was changed. No commit, push, tag, package publication, release, or
deployment was performed.
