# Phase 0 Agent Prompt - Baseline and Architecture Contract

You are implementing Phase 0 of the z8run adoption plan in the EdgeLinkd repository.

Repository: `/media/kestl/andor/github/edgelinkd`

Controlling plan: `adoption/z8adoptionplan.md`

Reference only: `/media/kestl/andor/github/z8run` at commit
`2e2cba6178d94e22906d37d6ac1af5ba23dcbbb0`

## Operating rules

1. Read `AGENTS.md`, `README.md`, `CONTRIBUTING.md`, and the controlling plan completely.
2. Start with Palimnex: check status, run focused searches, and open the actual sources. Source
   and Git evidence outrank cached results. Reindex and deep-validate after indexed-file edits.
   Never flush shared Redis.
3. Inspect `git status` and preserve all unrelated or pre-existing changes. Local agent/tooling
   paths are not product changes.
4. This phase must not change runtime behavior, persistent formats, secrets, dependencies, or
   the published version.
5. Use `apply_patch` for edits. Do not hand-edit generated files or `Cargo.lock`.
6. Do not commit, push, tag, publish, or release unless the user separately authorizes it.
7. Do not copy z8run code. Its design may inform the ADR; EdgeLinkd's compatibility and
   embedded constraints control the result.

## Objective

Create a reproducible baseline and architecture contract that later agents can use to measure
the effect and reversibility of phases 1-7.

## Required work

- Inspect the current default, minimal, and full feature sets in the root and crate
  `Cargo.toml` files. Record the actual current version without changing it.
- Confirm the single deploy/rollback path and four-file flow/credential write set.
- Identify every current outbound network call site: HTTP request, AI adapter, OIDC, fleet,
  MQTT, TCP, WebSocket, Modbus, and any others found.
- Identify current request middleware, authentication, audit, Flow Copilot validation, and
  feature-gating boundaries.
- Write an ADR defining:
  - Node-RED compatibility invariants.
  - fail-open versus fail-closed behavior by subsystem;
  - feature-gate and downgrade rules;
  - secret and persistent-data boundaries;
  - binary-size, idle-RSS, startup-time, and per-feature resource budgets;
  - supported host and ARM validation targets;
  - release and rollback evidence required for every later phase.
- Add safe benchmark/measurement instructions or helpers only if they are deterministic,
  non-privileged, and do not mutate live user state.
- Add sanitized fixtures for deploy/rollback, credentials, MQTT, AI, and HTTP only if no
  secret or machine-specific state is included.
- Produce a baseline report containing commands, results, skipped boundaries, and the exact
  Git revision tested.

Choose repository locations consistent with existing documentation conventions. Avoid adding
a new framework when a Markdown report and small existing-style helper are sufficient.

## Required validation

- `cargo fmt --check`
- `cargo clippy --all-features --tests --all -- -D warnings`
- `cargo test --workspace --features full`
- `cargo build --all`
- Relevant pytest suite after the Python extension build
- Default, minimal, and full build checks
- Existing deploy/rollback and credential tests
- Existing Flow Copilot tests
- Local MQTT acceptance only if the service is available; pass credentials through
  `EDGELINK_MQTT_USER` and `EDGELINK_MQTT_PASSWORD`, never print them
- At least one supported ARM cross-check if the toolchain is present; otherwise document the
  exact unverified boundary
- `git diff --check`
- Palimnex incremental index, deep validation, and fresh status

Record binary size, idle RSS, startup time, and representative latency using stable commands.
Do not fabricate measurements when a target or tool is unavailable.

## Rollback drill

Demonstrate that all Phase 0 additions can be removed without affecting runtime files. Restore
a copied, sanitized test installation from its baseline backup. Do not run a recovery drill on
the user's live credentials or flows.

## Completion criteria

- The ADR and baseline report are sufficient for a new agent to determine whether a later
  phase regressed compatibility, resources, or recovery.
- No runtime behavior or persisted format changed.
- No secret or local runtime state was added.
- Working-tree changes are limited to intentional Phase 0 documentation, fixtures, or helpers.

## Final response format

Report:

1. Files changed and why.
2. Baseline measurements and exact commands.
3. Test results and counts.
4. Rollback drill result.
5. Unverified host/ARM/live-service boundaries.
6. Git status and explicit confirmation that no commit, push, tag, release, or version change
   occurred.
