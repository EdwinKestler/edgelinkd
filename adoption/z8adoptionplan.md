# z8run Adoption Plan for EdgeLinkd

Status: Phases 0-5 complete; phase 6 is next. Evidence is in
`adoption/phase5/REPORT.md`, `adoption/phase4/REPORT.md`, `adoption/phase3/REPORT.md`,
`adoption/phase2/REPORT.md`, `adoption/phase1/REPORT.md` and `adoption/phase0/BASELINE.md`.

This plan evaluates patterns found in the sibling checkout at
`/media/kestl/andor/github/z8run`, reviewed at commit
`2e2cba6178d94e22906d37d6ac1af5ba23dcbbb0`. It does not authorize copying code,
committing, pushing, tagging, publishing, or changing the EdgeLinkd version.

EdgeLinkd remains a Node-RED-compatible, embedded-first runtime. z8run is an architectural
reference, not a replacement runtime or editor. Implement adopted ideas independently in
EdgeLinkd's existing style, preserve its Node-RED contract, and retain third-party notices if
substantial source is ever copied.

## Risk matrix

Probability ranges are pre-prototype engineering estimates, not measured failure rates.

| Order | Adoption | Expected gain | Adoption-risk probability | Maximum impact | Principal failure mode | Recommendation |
|---:|---|---:|---:|---|---|---|
| 1 | Central egress policy | Very high | Medium, 35-50% | High | Existing MQTT, Modbus, AI, OIDC, or HTTP connections are unexpectedly blocked | Adopt using observe then enforce |
| 2 | Encrypted credentials | Very high | Medium, 30-45% | Critical | Key loss, failed migration, or rollback makes credentials unreadable | Adopt with explicit migration and recovery tooling |
| 3 | Webhook/API protection | High | Medium, 25-40% | High | Node-RED clients or slow legitimate requests are rejected | Adopt with endpoint-specific limits |
| 4 | Durable execution history | Medium-high | Low-medium, 20-35% | Medium | Storage growth, disk exhaustion, or writer contention affects runtime | Adopt as an optional SQLite feature |
| 5 | Typed Copilot metadata | High | Low-medium, 20-30% | Medium | Incorrect schemas cause valid drafts to be rejected | Adopt as advisory metadata first |
| 6 | Selective AI nodes | Medium-high | Medium-high, 40-60% | High | Provider drift, cost loops, secret exposure, or tool misuse | Adopt incrementally behind subfeatures |
| 7 | WASM node SDK | Medium | High, 55-70% | Critical | Sandbox escape, resource exhaustion, ABI instability, or excessive footprint | Prototype last; default off |

## Required sequence

1. Phase 0: baseline and architecture contract.
2. Phase 1: central outbound egress policy.
3. Phase 2: encrypted credential storage.
4. Phase 3: webhook and API resource protection.
5. Phase 4: optional durable execution history.
6. Phase 5: typed registry metadata for Flow Copilot.
7. Phase 6: selective AI nodes.
8. Phase 7: optional WASM node SDK.

The phases are intentionally dependency ordered. In particular, AI nodes depend on the
egress and credential boundaries, and WASM depends on nearly every preceding security and
resource-control boundary.

## Phase 0 - Adoption baseline

Status: closed locally against `35e0546c68c6aed057e5ac246aac46b0844f457c`; see
`adoption/phase0/ADR-0001-adoption-baseline.md` and `adoption/phase0/BASELINE.md`.

Define the compatibility contract and capture reproducible measurements before behavior or
persisted formats change.

Deliverables:

- An ADR defining Node-RED compatibility, fail-open/fail-closed rules, feature gates,
  downgrade policy, and embedded resource budgets.
- Baselines for default, minimal, and full builds: binary size, idle RSS, startup time, and
  representative HTTP, MQTT, and Copilot latency.
- Sanitized deployment fixtures containing flows, current/previous credential sidecars, and
  MQTT/AI/HTTP configurations without secrets.
- A reproducible test report for deploy, rollback, credential placeholders, MQTT, and Flow
  Copilot.
- Confirmation that `crates/web/src/handlers/deploy.rs` remains the single transactional
  deploy and rollback writer.

Tests and rollback:

- Run the full existing suite and supported cross-builds.
- Restore a copied test installation from the baseline backup.
- Phase 0 must not change runtime behavior or persisted production data. Its rollback is the
  removal of documentation, benchmark helpers, and sanitized fixtures.

Exit gate: evidence is reproducible on x86-64 and at least one supported ARM target.

## Phase 1 - Central egress policy

Status: closed locally; see `adoption/phase1/REPORT.md`.

Add one outbound policy used by HTTP, AI providers, OIDC, fleet, MQTT, WebSocket/TCP clients,
and future database or WASM capabilities.

Required behavior:

- Modes: `off`, `observe`, and `enforce`.
- Administrator allowlists for hostname, IP/CIDR, protocol, and port.
- Validation of every resolved IPv4 and IPv6 address.
- Default rejection of cloud metadata, link-local, unspecified, and other unsafe targets.
- Redirect revalidation and DNS-rebinding protection.
- Environment proxies disabled unless explicitly enabled.
- Connection/request/idle timeouts and bounded HTTP/AI response bodies.
- A distinction between administrator-configured industrial endpoints and message-controlled
  dynamic destinations.
- Secret-safe decision logs.

Rollout:

1. Compatibility baseline in `off` mode.
2. Inventory real destinations in `observe` mode.
3. Enforce dynamic/untrusted targets.
4. Enforce globally after administrators have explicit allowlists.

Tests:

- Address, hostname, CIDR, port, protocol, IPv4, and IPv6 matrices.
- Loopback/private/link-local/multicast/metadata rejection.
- Mixed DNS answers, DNS rebinding, and public-to-private redirects.
- Encoded-address and malformed-host bypass attempts.
- Proxy isolation, timeouts, and body limits.
- Local RabbitMQ succeeds only through an explicit test allowlist.
- Mock and opt-in live AI provider tests.
- Invalid policy configuration fails startup clearly.
- Default and minimal footprint comparisons.

Rollback:

- Switch to `observe` or `off` and restart.
- Preserve the previous configuration before enforcing.
- No flow or credential conversion is allowed in this phase.

Exit gate: existing network behavior passes with explicit compatibility rules, and bypass
tests fail closed.

## Phase 2 - Encrypted credential storage

Status: complete against `5a778f42eeb1fc890d9bd39aa94bd0cd110dca93`; see
`adoption/phase2/DESIGN.md` and `adoption/phase2/REPORT.md`.

Replace or supplement plaintext `flows_cred.json` with a versioned authenticated-encryption
envelope while keeping Node-RED credential semantics and transactional rollback.

Required behavior:

- Versioned envelope containing algorithm, key identifier, nonce, ciphertext, and tag.
- Key provider abstraction supporting an injected secret and a local random key file with
  mode `0600`; hardware-backed providers may be added later.
- Explicit `status`, dry-run migration, encryption migration, rotation, and recovery/export
  operations. Startup must not silently rewrite plaintext.
- One compatibility release that reads plaintext and encrypted sidecars.
- Refusal to overwrite corrupt or undecryptable credentials.
- Preservation of `__PWRD__`, blank-secret clearing, secret stripping, live/previous pairs,
  transactional restoration, and secret-free logging.

Tests:

- Encryption round trip, nonce uniqueness, wrong/missing key, and corrupt authentication tag.
- `0600` permissions and atomic replacement.
- Failure injection at every four-file deploy and rollback boundary.
- Placeholder retention and clearing.
- Transactional key rotation across current and previous generations.
- Concurrent rotation and deploy.
- Secret absence from logs, API responses, history, and errors.
- Plaintext upgrade, controlled downgrade, and lost-key recovery on a copied installation.

Rollback:

- Require an offline backup before migration.
- Provide a supported decrypt/export path before an older binary is installed.
- Do not leave automatic plaintext backups next to live data.
- Failed migration restores all four original files.
- Rotation completes only when current and previous generations decrypt with the new key.

Exit gate: crash injection always leaves a complete old or complete new generation, never a
mixed pair.

## Phase 3 - Webhook and API resource protection

Status: closed locally against `29a27b3ee45d4a9bf80889a992df11e0bb4ee06f`; see
`adoption/phase3/DESIGN.md` and `adoption/phase3/REPORT.md`.

Apply shared, endpoint-specific Axum/Tower controls to admin/editor, authentication, webhook,
HTTP-in, Copilot, WebSocket, and health routes.

Required behavior:

- Exact method/path matching and correct authentication/role enforcement.
- Body, header, concurrency, rate, execution-time, and response limits.
- Cancellation propagated to downstream work.
- Forwarded client addresses trusted only through explicit trusted-proxy configuration.
- Audit entries that exclude request bodies, credentials, and tokens.
- Independent compatibility controls per endpoint class.

Tests:

- Oversized, chunked, and slowly delivered requests.
- Excess headers and malformed lengths.
- Per-user and per-IP limiting, including forged forwarding headers.
- Concurrency exhaustion, recovery, timeout, and client-disconnect cancellation.
- Authentication before expensive parsing.
- WebSocket upgrade/revocation and Node-RED editor compatibility.
- Bounded Copilot prompt and response processing.

Rollback:

- Disable or relax one endpoint class without removing protections elsewhere.
- Keep a generated snapshot of prior limits.
- Do not change persisted flows or credentials.

Exit gate: legitimate editor and MQTT workflows pass while resource-exhaustion tests stay
bounded.

## Phase 4 - Optional durable execution history

Add a feature-gated SQLite operational history without replacing `flows.json` or the security
audit log.

Suggested events:

- Deploy proposed, accepted, rejected, and rolled back.
- Node error and status transitions.
- Copilot draft requested, accepted, and rejected.
- Fleet push and promotion outcomes.
- Optional execution summaries, excluding full message bodies by default.

Required behavior:

- Feature such as `history_sqlite`, in the default app build and droppable in `--no-default-features` minimal builds.
- Versioned migrations, bounded asynchronous writer, retention, and database-size limits.
- Redacted structured data and health metrics for failed/dropped events.
- Flow execution must not depend on history availability.

Tests:

- Schema creation, forward migration, restart persistence, and concurrent ordering.
- Retention, size limits, disk full, read-only database, corruption, and queue saturation.
- Credential, prompt, and message-body exclusion.
- Feature-disabled build has no SQLite dependency.
- ARM and runtime footprint checks.

Rollback:

- Disable the feature while preserving the database for export or forensics.
- Do not require database downgrade for binary rollback.
- Restore a copied database if a migration fails.
- History failures are fail-open with health reporting; security-audit policy remains separate.

Exit gate: a corrupt or full history database cannot stop active flows or corrupt deployment
files.

## Phase 5 - Typed metadata for Flow Copilot

Status: closed locally; see `adoption/phase5/DESIGN.md` and `adoption/phase5/REPORT.md`.

Extend the runtime registry with versioned metadata for ports, configuration, secrets,
capabilities, feature availability, and advisory message types.

Initial enforcement is limited to structural facts:

- Registered node type and enabled feature.
- Input/output cardinality and valid output port.
- Required configuration-node references.
- Reserved configuration properties.

Payload types remain advisory because Node-RED messages are dynamic. Copilot must still run
`Engine::prepare_flows`, show a preview, and require user approval before deployment.

Tests:

- Every registered node produces valid metadata.
- Editor HTML and runtime registry remain synchronized.
- Unknown nodes, nonexistent ports, missing references, and disabled features are rejected.
- Valid dynamic message types are not rejected.
- Existing MQTT configurations are reused and secrets never enter metadata or prompts.
- Property/fuzz tests for draft references and wiring.
- Golden natural-language prompt tests and old-client compatibility.

Rollback:

- Version the metadata schema.
- Fall back to the existing type-only catalog.
- Allow strict metadata validation to be disabled independently.
- Never change runtime wire semantics or require flow conversion.

Exit gate: golden prompts generate valid drafts without inventing nodes, ports, or credentials.

## Phase 6 - Selective AI nodes

Implement in increasing order of external and operational risk:

1. Local deterministic text splitter.
2. Structured-output validator/parser.
3. Embeddings.
4. Bounded agent/tool loop.
5. Media or provider-specific nodes only after demonstrated demand.

Use focused feature gates where practical. Every agent loop requires turn, tool, token, time,
and concurrency limits; cancellation; explicit tools; egress enforcement; credential-service
access; and manual approval for flow deployment.

Tests:

- Deterministic mock-provider contracts for every node.
- OpenAI, Claude, Grok, and Cortex request-shape fixtures when claimed supported.
- Provider errors, timeouts, malformed/truncated output, and unsupported parameters.
- Cancellation, maximum turns, tool denial, egress denial, and retry idempotence.
- Secret redaction, editor credential round trip, cost/token caps, and concurrent load.
- Focused pytest behavior tests and opt-in live-provider acceptance.

Rollback:

- Feature gate each node family and hide it from the editor when unavailable.
- A deployed flow requiring a disabled feature fails loudly; it must not become a silent no-op.
- Export or remove unsupported nodes before downgrading the binary.
- Provider configurations and credentials remain readable while a node family is disabled.

Exit gate: ordinary CI uses mocks, and every production-supported provider has a documented
opt-in live acceptance procedure.

## Phase 7 - Optional WASM node SDK

Prototype a versioned component interface behind `nodes_wasm`, disabled by default and absent
from minimal builds.

Required behavior:

- Versioned manifest and ABI with named ports, bounded values, configuration schema, and
  requested capabilities.
- Default denial of filesystem, network, environment, clock, and randomness.
- Explicit capabilities, low memory ceiling, fuel limit, timeout, global concurrency limit,
  and bounded input/output.
- Atomic validate, quarantine, self-test, activate, and one-generation rollback process.
- Editor catalog changes only after activation.

Tests:

- Minimal valid plugin and unknown/malformed ABI.
- Infinite loop, memory growth, oversized values, forbidden imports, traversal, traps,
  concurrency, cancellation, and shutdown.
- Failed install preserves the previous package; restart ignores partial packages.
- Disabled build has no Wasmtime dependency or editor entries.
- Fuzzed manifests/host calls and measured binary, startup, and per-instance memory costs.

Rollback:

- Disable `nodes_wasm` and retain the previous package generation.
- Flows that require an unavailable plugin fail loudly.
- Maintain one previous experimental ABI or provide an explicit migration tool.
- Stop after the prototype if the embedded resource budget is exceeded.

Exit gate: hostile-plugin tests pass, default builds remain essentially unchanged, and actual
target devices remain within the Phase 0 budget.

## Commit and release strategy

Each phase must be independently reviewable and contain its architecture/configuration
contract, implementation, failure tests, integration/editor tests, migration/rollback docs,
and live acceptance evidence where applicable.

Do not combine encrypted credential migration, egress enforcement, and WASM in one release.
Egress enforcement and encrypted persisted data warrant a pre-1.0 minor release rather than a
patch. Choose the exact version from the current checkout only when that release is prepared.

Suggested release groups:

1. Security: phases 1-3.
2. Operations and Copilot: phases 4-5.
3. AI: phase 6.
4. Experimental extensions: phase 7.

## Mandatory gate after every phase

- `cargo fmt --check`
- `cargo clippy --all-features --tests --all -- -D warnings`
- `cargo test --workspace --features full`
- `cargo build --all`
- Relevant pytest suites after rebuilding the Python extension
- Default, minimal, and full feature builds
- Node-RED editor smoke test
- Focused live MQTT test with credentials only in environment variables
- ARM cross-build
- Binary-size and RSS comparison against Phase 0
- Failure-injection rollback drill
- Secret scan of logs, fixtures, diffs, and history
- `git diff --check`
- Palimnex incremental index, deep validation, and fresh status

Never commit `.palimnex/`, `*.pmem`, `*.key`, live credentials, runtime databases, generated
secrets, or local agent/tooling directories.

## Agent handoffs

The executable prompts live in `adoption/z8phases/`. Run them in numeric order. A later agent
must verify that the previous phase's exit gate is present in the checkout; it must not assume
that an earlier agent completed or committed its work.
