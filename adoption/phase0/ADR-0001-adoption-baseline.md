# ADR-0001: z8run Adoption Baseline and Architecture Contract

- Status: accepted for phases 1-7
- Date: 2026-10-03
- Decision scope: the adoption work described in `adoption/z8adoptionplan.md`
- Tested revision: `35e0546c68c6aed057e5ac246aac46b0844f457c`
- Published crate version at the baseline: `0.3.0`

## Context

EdgeLinkd is an embedded-first Rust flow runtime whose user interface is the Node-RED editor.
The z8run review identified useful security, persistence, history, metadata, AI, and extension
patterns, but adopting those patterns must not turn EdgeLinkd into a general desktop Node.js
runtime or silently weaken its Node-RED compatibility contract.

This ADR fixes the invariants and evidence gates that every later phase must preserve. It does
not change runtime behavior, persistent formats, dependencies, features, or the version.

## Decision

### Node-RED compatibility invariants

1. The bundled editor is an authoring surface for this runtime. Every node or option it exposes
   must work as covered by the ported upstream tests or fail immediately and clearly.
2. Unsupported behavior is absent from the editor or returns `EdgelinkError::NotSupported`, a
   deploy error, or a catchable node error/status. It is never accepted as a silent no-op and no
   plausible value is fabricated.
3. Supported node behavior and the exact upstream test titles remain the contract recorded by
   `tests/`, `scripts/specs_diff.json`, and the generated coverage report.
4. Flow JSON, node IDs, wires, dynamic `msg` values, credential placeholders, and the editor's
   deploy/revision behavior remain compatible within the explicitly supported surface.
5. New validation may reject configurations that are unsafe or impossible, but it must not
   reinterpret a valid flow. Structural Copilot checks may be strict; dynamic payload typing is
   advisory.
6. Every graph-changing API continues to use the same transactional deploy path. There must not
   be a second writer that bypasses prepare, revision checking, credential separation, audit, or
   rollback.

### Fail-open and fail-closed rules

| Subsystem | Required behavior |
|---|---|
| Deploy, single-flow edits, rollback | Fail closed. Prepare the complete candidate before persistence; any persistence or activation failure preserves or restores the complete prior four-file generation. |
| Credential read, migration, rotation | Fail closed. Missing keys, corrupt authentication, or mixed generations must never become empty credentials or trigger an implicit rewrite. |
| Authentication and authorization | When configured, fail closed. Unknown roles and incomplete identity-provider configuration prevent startup. Public/default-login behavior remains an explicit configuration choice, not an error fallback. |
| Central egress policy | `enforce` fails closed. `observe` and `off` intentionally allow the request and report the decision; malformed policy configuration fails startup. A policy-engine error in `enforce` denies the request. |
| MQTT, HTTP, TCP, WebSocket, UDP, Modbus, AI | Connection, protocol, timeout, or validation failures are surfaced as node/provider errors; no fake success or data. Other independent flows may continue. |
| Web/API limits | Fail closed for the affected request or socket. Rejection happens before expensive parsing or downstream effects where possible. |
| Flow Copilot | Provider, schema, catalog, wiring, credential, or `prepare_flows` failure produces no draft application. All accepted drafts remain previews requiring explicit user apply/deploy. |
| Operational history | Fail open for flow execution, with bounded loss counters and health/status reporting. It never replaces the security audit log or `flows.json`. |
| Security audit | Do not log bodies, tokens, credentials, or message contents. A write failure is visible through health/logging and never turns a failed security operation into success. Existing operations that already treat audit as best-effort retain that compatibility until separately changed. |
| Typed metadata | Registered types, ports, feature availability, and configuration references fail closed. Dynamic message payload types remain advisory. |
| WASM packages | Validate and quarantine before activation; missing capability, limit, ABI, or signature/manifest validation fails closed. A plugin failure cannot stop unrelated flows. |

### Feature gates and downgrade rules

- The application default features are `core`, `js`, `jsonata`, `nodes_network`,
  `nodes_storage`, and `nodes_ai`. `full` adds `rqjs_bindgen`. `runtime_scan` and
  `nodes_modbus` remain opt-in. Later optional families must be removable with a Cargo feature.
- A disabled node family is absent from both the runtime JSON catalog and editor HTML. A flow
  that still refers to a disabled type fails loudly; it never degrades to a functional-looking
  no-op.
- Feature-disabled builds must not retain that feature's heavy dependency or bundled asset.
  Phase reports must prove this using dependency/build evidence and size comparison.
- Persistent-format changes require a versioned reader, an explicit forward migration, an
  offline backup, and a documented export or downgrade path before an older binary is used.
  Startup must not silently migrate durable state.
- `flows.json` remains the compatibility source of truth. Optional history, metadata, or plugin
  indexes may be deleted or disabled without changing valid flows.
- Downgrade tests run against copied fixtures. Never test downgrade or recovery on live user
  flows, credentials, keys, context, or fleet inventory.

### Secrets and persistent-data boundaries

The current deploy write-set is:

1. `flows.json` - credential-free current graph;
2. `flows_cred.json` - current plaintext credential sidecar, mode `0600` where supported;
3. `flows.json.prev` - one previous credential-free graph;
4. `flows_cred.json.prev` - the matching previous credential sidecar.

`crates/web/src/handlers/deploy.rs` is the single writer for `/flows`, `/flows/rollback`,
`/flow`, and `/flow/{id}`. It prepares before persistence, replaces the four files as one
logical write-set, activates, and compensates on failure. `crates/core/src/runtime/
flow_credentials.rs` merges the sidecar only in memory and strips credentials from revisions.

Other persistent data includes `audit.log`, configured context stores, local libraries,
`fleet.json` when fleet is enabled, and configuration files. Later history databases, key files,
or plugin packages are separate durable domains and must define ownership, permissions,
retention, backup, and crash recovery.

Secret rules:

- Secrets never enter `flows.json`, revisions, API responses, logs, audit details, Copilot
  prompts, history, test output, or error messages.
- Node-RED `__PWRD__` retains the stored password and an intentional blank clears it.
- Environment variables and injected test credentials are process inputs, not fixture content.
- Fixtures use conspicuously invalid values on reserved example domains. They are not reusable
  credentials.
- Phase 2 encryption supplements file permissions; it does not remove atomic replacement,
  rollback pairing, or secret-redaction requirements.

### Resource budgets

Measurements use the stripped `ci` profile on the same host and toolchain, with the Phase 0
commands in `BASELINE.md`. A threshold breach blocks the phase unless the report explains and
approves a new budget. Small-host noise floors are included to avoid false precision.

| Resource | Compatibility gate |
|---|---|
| Default stripped binary | Per phase: no more than the larger of 5% or 512 KiB growth. Cumulative phases 1-6: no more than 20% without a separate ADR. |
| Minimal stripped binary | A default-off feature contributes 0 linked code/dependency growth when disabled. Other changes: no more than the larger of 2% or 256 KiB per phase. The current root `--no-default-features` build is not a true minimal baseline because `edgelink-web` pulls `edgelink-core` defaults; fixing that requires a separately reviewed change. |
| Full stripped binary | Per phase: no more than the larger of 7% or 1 MiB. WASM is separately capped at 8 MiB growth while enabled and must remain absent when disabled. |
| Idle RSS | Default: no more than the larger of 10% or 2 MiB. Default-off feature enabled: document steady-state delta and keep it below 4 MiB, except WASM which requires its own ADR and device result. |
| Startup to healthy | No more than the larger of 20% or 100 ms on the same warm/cold classification. |
| Local health/settings latency | Median no more than the larger of 20% or 2 ms and p95 no more than the larger of 25% or 5 ms over at least 100 requests. |
| Queues and bodies | Every new queue, response, prompt, request, history batch, and plugin value has a configured finite bound. Saturation behavior is tested. |

Per-feature allocation is: egress policy 512 KiB; credential encryption 768 KiB; API protection
512 KiB; typed metadata 768 KiB; each selective AI subfeature 1 MiB. SQLite history and WASM
must be default-off and therefore contribute zero to the disabled binary; enabled allocations
are 2 MiB and 8 MiB respectively. These are maximum stripped-binary deltas, not targets.

### Validation targets

- Primary host: `x86_64-unknown-linux-gnu`, full Rust and pytest suites plus live HTTP probes.
- Required ARM build gate: at least one of `aarch64-unknown-linux-gnu`,
  `armv7-unknown-linux-gnueabihf`, or `armv7-unknown-linux-gnueabi` for every phase.
- Before release, scheduled/manual CI must build all three Linux ARM targets already declared by
  the project. Any target-specific filesystem, crypto, SQLite, or WASM behavior needs execution
  on representative hardware or an emulator; a cross-build alone is labeled build-only.
- Windows remains a compile/test gate for atomic replacement and path behavior. Linux tests do
  not prove the non-Unix replacement branch.

### Release and rollback evidence for every later phase

Each phase report must contain:

1. exact base and tested Git revisions, version, dirty-tree scope, toolchain, target, feature
   flags, and configuration;
2. compatibility tests, phase-specific failure injection, full host gates, and at least one ARM
   cross-build;
3. default/minimal/full binary sizes, idle RSS, startup time, representative latency, and an
   explanation for every budget delta;
4. secret scanning/redaction evidence and confirmation that no fixture, log, history row, error,
   or API response contains a live secret;
5. a recovery drill on a copied installation, including pre/post hashes and proof that the prior
   binary or disabled feature can read the restored state;
6. explicit unverified boundaries such as unavailable hardware, Windows runtime, external
   provider, MQTT broker, or fault mode;
7. `git diff --check` and fresh Palimnex deep validation;
8. separate statements for implemented, committed, pushed, tagged, released, deployed, and
   live-verified status.

No phase is released merely because its unit tests pass. Commit, push, tag, package publication,
deployment, and live acceptance are distinct approvals and evidence states.

## Current architecture inventory

### Outbound call sites

| Capability | Current source boundary |
|---|---|
| HTTP request node | `crates/core/src/runtime/nodes/network_nodes/http_request.rs` (`reqwest`) |
| AI providers and Flow Copilot completion | `crates/core/src/runtime/nodes/ai_nodes/adapter.rs` and provider client construction |
| OIDC discovery/token/userinfo | `crates/web/src/handlers/auth.rs` |
| Fleet health/push/promote | `crates/web/src/handlers/fleet.rs` |
| MQTT | `crates/core/src/runtime/nodes/network_nodes/mqtt_broker.rs` (`rumqttc`) |
| TCP in/out/get client paths | `crates/core/src/runtime/nodes/network_nodes/tcp_in.rs`, `tcp_out.rs`, and `tcp_get.rs` |
| WebSocket client/in/out | `crates/core/src/runtime/nodes/network_nodes/websocket_client.rs`, `websocket_in.rs`, and `websocket_out.rs` |
| Modbus TCP | `crates/core/src/runtime/nodes/network_nodes/modbus.rs` |
| UDP output | `crates/core/src/runtime/nodes/network_nodes/udp_out.rs` |
| Process execution with possible child egress | `crates/core/src/runtime/nodes/function_nodes/exec.rs`; this is a separate process capability and cannot be secured by an HTTP-only policy |

Incoming HTTP, WebSocket listener, TCP-in listener, and UDP-in listener paths are ingress and
must use Phase 3 resource controls; they are not outbound-policy substitutes.

### Request, audit, Copilot, and gating boundaries

- `crates/web/src/api.rs` builds the Node-RED/editor/health routers, wraps registered node routes
  with `require_admin`, and currently permits any CORS origin, method, and header.
- `crates/web/src/handlers/auth.rs` owns local/OIDC login, roles, lockout, tokens, revoke, and the
  middleware's public-route exceptions.
- `crates/web/src/handlers/audit.rs` appends bounded structured event facts to `audit.log`; it is
  not an execution historian.
- `crates/web/src/handlers/assistant.rs` bounds prompt/flow/draft sizes, redacts flows before the
  model call, restricts proposed types to the live registry, materializes add-only drafts, merges
  credentials only for local validation, calls `Engine::prepare_flows`, and never deploys.
- Cargo features in the root and crate manifests control runtime and editor availability. The
  `/nodes` JSON and HTML catalog must continue to match the live registry.

## Consequences

Later phases gain explicit security, compatibility, resource, and rollback gates. The budgets
will reject some attractive dependencies or require feature gating. The current plaintext
credential sidecar and imperfect minimal feature wiring are recorded facts, not silently fixed
inside Phase 0.
