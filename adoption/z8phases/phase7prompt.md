# Phase 7 Agent Prompt - Optional WASM Node SDK

You are implementing Phase 7 of the z8run adoption plan in EdgeLinkd.

Repository: `/media/kestl/andor/github/edgelinkd`

Read first: `AGENTS.md`, `adoption/z8adoptionplan.md`, the Phase 0 ADR/resource baseline, and
evidence for phases 1-6. Review z8run's pinned WASM design only as a reference. Perform current
license and dependency due diligence before adding a runtime.

## Operating rules

- Use Palimnex before edits and refresh/deep-validate afterward.
- Verify prior phase gates, especially egress, encrypted credentials, API/resource limits,
  typed metadata, and history redaction.
- This is an experimental, optional feature. It must be disabled by default unless the user
  separately approves default inclusion after target-device measurements.
- Preserve unrelated work and secrets. Use `apply_patch`; do not hand-edit generated files or
  Cargo.lock.
- Do not commit, push, tag, publish, release, or change the version without separate approval.

## Objective

Prototype a sandboxed third-party node interface that cannot silently gain network,
filesystem, environment, clock, random, process, credential, or deployment authority.

## Required design review before code

Produce an ADR or design update covering:

- Runtime/dependency choice and license.
- Versioned ABI/component model and support window.
- Manifest schema, node identity, configuration, typed metadata, and named ports.
- Message encoding and strict input/output size bounds.
- Capability model and default-deny host imports.
- Memory, fuel, epoch/time, concurrency, and cancellation limits.
- Package origin/integrity/signature policy and local installation authorization.
- Atomic quarantine, validation, self-test, activation, previous-generation rollback, and
  removal.
- Cross-platform/ARM support and exact embedded resource budget.
- Behavior when a required plugin or ABI is unavailable.

Do not proceed to a runtime dependency if the Phase 0 budget or target support makes the
proposal nonviable. A documented no-go result is a valid phase outcome.

## Required implementation if the design passes

- Feature such as `nodes_wasm`, absent from default/minimal builds.
- Versioned manifest and ABI with explicit capabilities.
- Default denial of filesystem, network, environment, clock, randomness, process, secrets,
  context writes, and deploy operations.
- Host calls only through narrow, validated, bounded interfaces.
- Low memory cap based on measured target hardware, fuel/instruction limit, wall deadline,
  global concurrency limit, and cancellation.
- Egress-policy and credential-service mediation for any future granted capability.
- Atomic package lifecycle: stage, validate, instantiate, self-test, activate, and retain one
  previous generation.
- Editor and metadata catalog updated only after successful activation.
- Loud deploy/start failure for unavailable required plugins; never silently use `unknown` as
  apparent support.

## Required hostile-plugin tests

- Valid minimal plugin and normal message round trip.
- Unknown ABI, malformed manifest, duplicate identity, and unsupported capability.
- Infinite loop/fuel exhaustion and ignored cancellation.
- Memory growth and allocation bombs.
- Oversized input, output, log, error, and metadata.
- Forbidden filesystem, network, environment, clock, random, process, and credential imports.
- Path traversal, symlink escape, package-name traversal, and partial extraction.
- Trap/panic isolation and repeated crash behavior.
- Global and per-plugin concurrency exhaustion.
- Shutdown and redeploy cancellation.
- Failed validation/self-test/rename preserves the active previous package.
- Restart ignores incomplete or quarantined packages.
- Feature-disabled builds contain no runtime dependency, editor node, metadata, API, or false
  support claim.
- Manifest and host-call decoder fuzz/property tests.
- Binary size, compile time, startup time, idle RSS, and per-instance peak memory on host and
  at least one supported ARM device/target.
- Full mandatory gate and all previous phase regression tests.

## Rollback drill

- Install generation A, upgrade to B, inject activation failure for C, and prove B remains
  active while A remains the rollback generation according to the selected contract.
- Roll back from B to A and restart successfully.
- Disable `nodes_wasm` and prove ordinary non-WASM flows still work with no plugin runtime
  initialized.
- Prove flows requiring a disabled/missing plugin fail loudly and identify the missing
  identity/version.
- Verify packages can be preserved for later re-enable without automatic execution.

## Stop conditions

Stop the phase and recommend no adoption if any of these holds:

- Disabled/default builds retain a material runtime dependency or footprint.
- Supported ARM targets cannot build or run it.
- Enforced memory and time limits cannot interrupt hostile modules reliably.
- Capability denial cannot be proven.
- Package installation cannot be made atomic.
- The measured footprint exceeds the Phase 0 budget.

## Non-goals

- No WASI preview exposing ambient system capabilities.
- No npm/Node.js plugin compatibility layer.
- No default network/filesystem access.
- No claim that sandbox tests prove freedom from all runtime vulnerabilities.

## Completion criteria

- Either a measured, hostile-tested optional prototype satisfies the ADR, or a documented
  no-go decision explains why it must not be adopted.
- Default/minimal builds remain essentially unchanged.
- Rollback and missing-plugin behavior are explicit and tested.

## Final response format

Report go/no-go decision, ADR conclusions, ABI/capability contract, files/dependencies,
hostile-test counts, resource measurements by target, atomic install/rollback drill, known
sandbox limits and advisories, Git status, and explicit no-commit/no-push/no-release
confirmation.
