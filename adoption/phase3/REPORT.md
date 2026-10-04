# Phase 3 Inbound Resource Protection Report

- Status: closed locally; uncommitted
- Date: 2026-10-03 (America/Guatemala)
- Branch: `master`
- Base revision: `29a27b3ee45d4a9bf80889a992df11e0bb4ee06f`
- Version: unchanged at `0.3.0`

Phase 3 adds bounded admission, execution, and response handling to EdgeLinkd's Axum editor/API
server and raw-TCP `http in` listeners. It does not add a reverse proxy, durable history, new AI
node, persistent migration, dependency, or version change.

## Route classification and limits

Eight stable endpoint classes select independent compatibility and resource budgets:

| Class | Surface |
|---|---|
| `health` | `/api/health`, `/api/info`, and compatibility aliases |
| `editor_admin` | Flows, credentials, nodes/palette, library, settings, status, audit, runtime configuration, context, inject/debug handlers, and other API routes |
| `authentication` | `/auth/*` |
| `websocket` | `/comms`, including the upgraded socket lifetime |
| `webhook` | Standalone raw-TCP listeners owned by `http in` nodes |
| `copilot` | `/assistant/*` |
| `fleet` | `/fleet/*` |
| `static_assets` | Editor HTML, JavaScript, CSS, icons, locales, theme, and debug-view assets |

The compiled defaults use `enforce` for every class, a global Axum concurrency ceiling of 128,
a maximum of 2,048 in-memory rate keys, and no trusted proxies. Each table can independently use
`enforce`, `observe`, or `off`.

| Class | Body | Headers | Rate/min | Concurrency | Queue | Deadline | Response |
|---|---:|---:|---:|---:|---:|---:|---:|
| Health | 0 | 32 KiB / 128 | 1,200 | 32 | 500 ms | 2 s | 64 KiB |
| Editor/admin | 4 MiB | 32 KiB / 128 | 600 | 32 | 500 ms | 120 s | 8 MiB |
| Authentication | 64 KiB | 32 KiB / 128 | 60 | 8 | 500 ms | 15 s | 64 KiB |
| WebSocket | 64 KiB/message | 32 KiB / 128 | 120 | 32 sockets | 500 ms | 10 s upgrade | 64 KiB |
| Webhook | 1 MiB | 32 KiB / 128 | 600 | 16/listener | 500 ms | 30 s | 2 MiB |
| Copilot | 1 MiB | 32 KiB / 128 | 30 | 2 | 1 s | 70 s | 2 MiB |
| Fleet | 1 MiB | 32 KiB / 128 | 60 | 4 | 500 ms | 60 s | 4 MiB |
| Static assets | 0 | 32 KiB / 128 | 1,200 | 64 | 500 ms | 30 s | 16 MiB |

Authenticated API work is charged to both principal and client address. Anonymous work is
charged to the client address. Fixed-window maps and every queue are finite. Forwarding headers
are ignored unless the direct TCP peer is an explicit trusted exact address or CIDR; malformed
forwarding from a trusted peer fails closed.

## Implementation

`crates/core/src/runtime/ingress.rs` owns strict configuration, defaults, validation, stable class
names, duration conversion, and trusted-proxy CIDR matching. `crates/web/src/protection.rs`
classifies routes and applies header, address, rate, global/class capacity, body, deadline,
shutdown, and response checks. Authentication remains the outer middleware and therefore rejects
unauthorized work before body collection. Static fallback protection uses an extension marker so
editor assets registered inside the API router are not charged twice.

WebSocket upgrade requests use the normal admission checks. A separate class permit then remains
owned for the socket lifetime, and the configured body budget becomes the incoming message-size
cap. Existing binding, expiry, revocation, lag recovery, and close behavior remain intact.

The raw `http in` listener now requires an exact absolute configured path and exact method. It
bounds request lines, aggregate headers, header count, fixed and chunked bodies, connections,
queue wait, one end-to-end request/flow-response deadline, response headers, and response bodies.
Conflicting or malformed lengths and unsafe response headers fail closed. An optional
`api_protection.webhook_bearer_env` enables one environment-supplied shared token; missing
configured material prevents graph load, and comparison happens before body allocation. A drop
guard cleans the response registry after timeout, disconnect, cancellation, or shutdown.

Copilot provider responses now have a 512 KiB pre-JSON cap in addition to the route budgets.
Ingress logs contain only mode, action, endpoint class, and reason. Flow bodies, prompts,
credentials, tokens, paths, addresses, and forwarded values are not logged.

User documentation is in `docs/security/ingress-protection.md`. Generated starter configuration
in `src/defaults.rs` lists every default. `adoption/phase3/fixtures` contains strict and
editor-only-observe rollback overlays; it contains no flow or credential copy.

## Test evidence

| Command or check | Result |
|---|---|
| `cargo fmt --check` | passed |
| `cargo clippy --all-features --tests --all -- -D warnings` | passed |
| `cargo test --workspace --features full --no-fail-fast` | app 1; CLI integration 1; core 280 passed/1 ignored; web 75 passed; doctests 2; no failures |
| Focused Axum protection tests | 13 passed |
| Shared ingress-configuration tests | 3 passed |
| Live raw-TCP HTTP-in protection test | 1 passed |
| `.venv/bin/pytest ./tests -v` | 825 passed, 213 skipped in 116.21 s; run before the final raw HTTP-in fallback refinement, which does not touch the Python harness and is covered by the final Rust suites |
| `cargo build --all` | passed |
| ARMv7 workspace full build, excluding PyO3 | passed |
| Default/no-default/full/all-feature locked `ci` builds | passed; sizes below |
| `git diff --check` | passed after the final report refresh |
| Palimnex incremental index and deep validation | 264 files, 1,323 chunks, 6,324 symbols; fresh; passed |
| Palimnex ledger | ready; integrity and scanner policy checks passed |

The new tests cover fixed and streamed/chunked over-limit bodies, a slow body, a delayed response
stream, response caps, excessive headers, malformed/conflicting lengths, unsupported media types,
rate identity, finite rate state, class/global saturation and recovery, deadline cancellation,
authentication before body parsing, trusted and forged forwarding, stable route classes, exact
webhook path/method behavior, invalid webhook paths, missing configured bearer material, and
oversized Copilot provider responses. Existing upgraded-socket tests prove revoke closure, expiry,
lag recovery, and token-free logs. Existing web tests cover deploy, encrypted credentials,
settings, examples/local library, palette catalog, status/debug links, Flow Copilot, OIDC, fleet,
and transactional failure recovery.

The full Rust suite also passed `a_live_broker_round_trip_on_localhost`. It confirms that the
ingress work did not break the available local MQTT path; it is not a packet capture or a complete
credentialed broker matrix. The Python MQTT acceptance and live encrypted-credential/editor
restart were established in the earlier phase evidence and were not repeated with a secret copied
into a command or tracked file.

## Cancellation evidence

The Axum timeout test holds a one-slot endpoint permit, times out a handler owning a drop probe,
observes the probe drop, and repeats the request to prove capacity recovery. Slow request and
response streams terminate with 408 and 504 respectively. The webhook listener shares one
deadline across headers, body, flow delivery, and response wait; its stop token cancels all
connection tasks, while the response-registration guard prevents orphaned senders. WebSocket
permits end when the upgraded task exits.

## Live and compatibility checks

The rollback process served the copied Phase 0 editor fixture over loopback. `GET /` and health
returned 200. Strict editor limits rejected the over-limit request; changing only editor/admin to
`observe` allowed the same request to reach the handler, while the health class continued to
reject a body. Metadata-only logs contained the expected class/reason decisions and no probe body.

No browser automation was available, so a manual editor click-through and browser reconnect were
not newly recorded. Editor route contracts, real upgraded WebSocket behavior, palette/library,
settings, deploy, debug/status, and Flow Copilot are covered by the Rust/web and pytest suites. No
paid external AI call was made.

## Resource comparison

Stripped `ci` binaries were measured with `stat -c %s` after separate locked builds.

| Configuration | Phase 0 | Phase 2 | Phase 3 | Phase 3 vs Phase 2 | Phase 3 vs Phase 0 |
|---|---:|---:|---:|---:|---:|
| Default | 14,420,296 | 14,966,152 | 15,088,776 | +122,624 (+0.82%) | +668,480 (+4.64%) |
| Root no-default | 14,259,480 | 14,719,832 | 14,837,784 | +117,952 (+0.80%) | +578,304 (+4.06%) |
| Root `full` | 14,420,296 | 14,966,152 | 15,088,712 | +122,560 (+0.82%) | +668,416 (+4.64%) |
| All features | 14,509,464 | 15,053,336 | 15,180,056 | +126,720 (+0.84%) | +670,592 (+4.62%) |

The default Phase 3 increment is below its 512 KiB allocation. Root no-default remains the
imperfect minimal proxy recorded in Phase 0; Phase 3 is built into the web server and therefore is
not a default-off feature.

Warm copied-fixture measurements used the default stripped binary:

| Measurement | Phase 0 | Phase 2 | Phase 3 | Phase 3 delta from Phase 2 |
|---|---:|---:|---:|---:|
| Startup median, 5 samples | 7 ms | 16 ms | 12.005 ms (7.544-14.957) | -3.995 ms |
| Idle RSS median, 5 samples | 13,404 KiB | 14,012 KiB | 14,156 KiB (14,120-14,248) | +144 KiB (+1.03%) |
| Health median, 100 requests | 0.337 ms | 0.518 ms | 0.214 ms | -0.304 ms |
| Health p95, 100 requests | 0.466 ms | 0.918 ms | 0.440 ms | -0.478 ms |
| Health mean, 100 requests | 0.331 ms | not recorded | 0.241 ms | not comparable |

All Phase 0 resource gates remain satisfied. These are warm host measurements, not
resource-constrained device results.

## Rollback drill

A copied Phase 0 installation was exercised under `/tmp` with the Phase 3 fixtures:

1. The strict overlay rejected an editor/admin request larger than 16 bytes with 413.
2. The editor-only `observe` overlay allowed the identical request to reach `/settings` and
   return 200.
3. A request body to the health class remained rejected with 413 under both overlays.
4. Static editor `/` remained available with 200.
5. The strict generated overlay was restored byte-for-byte.
6. SHA-256 checks of `flows.json`, `flows.json.prev`, `flows_cred.json`, and
   `flows_cred.json.prev` matched before and after. No history store exists in Phase 3.

The drill changed only copied configuration and process state. It did not touch the repository's
live encrypted sidecars, keyring, flows, or running service.

## Remaining boundaries

- Axum applies the configured header limits after Hyper has parsed headers. This prevents
  application work but is not a replacement for a reverse proxy's pre-parser socket limits.
- Static responses retain streaming; known `Content-Length` is bounded, but the class permit is
  released when the response is handed to the server rather than after the client consumes every
  byte.
- `http in` connection limits are finite per listener, not one cross-listener process semaphore.
- Rate limits are in-memory fixed windows. They reset on restart and are not coordinated across
  multiple EdgeLinkd processes.
- The optional webhook bearer is shared across HTTP-in listeners, not a per-route identity or
  authorization policy.
- A trusted reverse proxy must overwrite untrusted forwarding headers before relaying them.
- ARMv7 compilation passed, but ARM runtime load, latency, and cancellation were not run on
  hardware or QEMU. Windows runtime behavior was not executed locally.
- Browser interaction, external slow proxies, paid AI providers, and a complete live MQTT matrix
  remain outside this local closure.
- Palimnex doctor remains `action_needed` for the existing intentional corpus exclusions and
  50,000-file coverage-scan cap; Redis, the v3 cache, and the ledger are healthy.
- `observe` and `off` intentionally restore compatibility for one class and therefore remove
  enforcement for that class until the administrator returns it to `enforce`.

## Git and release state

Phase 3 is uncommitted working-tree work on top of
`29a27b3ee45d4a9bf80889a992df11e0bb4ee06f`. The user's modified
`edgelinkd.dev.toml`, live flows, encrypted credential files, keyring, and local agent metadata are
outside this phase. No commit, push, tag, release, package publication, deployment, persistent
migration, or version change was performed.
