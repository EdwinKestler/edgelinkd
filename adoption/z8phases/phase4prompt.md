# Phase 4 Agent Prompt - Optional Durable Execution History

You are implementing Phase 4 of the z8run adoption plan in EdgeLinkd.

Repository: `/media/kestl/andor/github/edgelinkd`

Read first: `AGENTS.md`, `adoption/z8adoptionplan.md`, the Phase 0 ADR/baseline, and evidence
for phases 1-3. Inspect the existing audit log, status/error channels, Flow Copilot audit event,
fleet events, feature structure, and runtime shutdown behavior.

## Operating rules

- Use Palimnex before edits and refresh/deep-validate afterward.
- Verify prior phase gates. Preserve unrelated changes and secrets.
- `flows.json` remains the flow source of truth. Existing `audit.log` remains the security and
  operator audit boundary. History is not allowed to replace either one.
- Keep SQLite and its transitive dependencies behind an optional feature absent from minimal
  builds.
- Use `apply_patch`; do not hand-edit generated files or Cargo.lock.
- Do not commit, push, tag, publish, release, or change the version without separate approval.

## Objective

Add bounded, queryable operational history for deployments, node health, Copilot, and fleet
activity without making flow execution depend on the database.

## Required design before implementation

Define:

- Event schema, redaction policy, timestamps, sequence/order semantics, and actor identifiers.
- Which events are required audit events versus best-effort operational history.
- Database location, permissions, WAL/sync behavior, migration versioning, and backup policy.
- Writer queue capacity, overflow behavior, shutdown drain deadline, retention, and maximum
  database size.
- Health/status reporting for writer errors, drops, corruption, and disabled state.
- Query API authorization, pagination, upper bounds, and filtering.

## Required implementation

- Add a feature such as `history_sqlite`; keep it off in feature-minimal builds.
- Use a bounded asynchronous writer and prepared/typed operations.
- Store structured redacted metadata. Do not store full message bodies, credentials, keys,
  provider tokens, raw prompts, or provider responses by default.
- Record deploy proposed/accepted/rejected/rollback, node error/status transitions, Copilot
  draft lifecycle, and fleet outcomes where those events already exist.
- Add retention and maximum-size enforcement that cannot block flow tasks indefinitely.
- Expose bounded, authenticated reads only if the phase design includes a query API.
- History failure must not stop active flows or corrupt deploy/credential state.

## Required tests

- Schema creation, idempotent startup, forward migration, and restart persistence.
- Stable ordering under concurrent writers.
- Bounded queue saturation and declared overflow behavior.
- Retention by age and size.
- Disk-full, read-only, permission, corrupt database, interrupted migration, and shutdown cases.
- Query bounds, pagination, authorization, malformed filters, and large-result resistance.
- Secret/prompt/message-body exclusion from rows, logs, errors, and fixtures.
- Feature-disabled build has no SQLite linkage/dependency and no history API/catalog claims.
- Existing audit log and deployment behavior remain unchanged.
- Full mandatory gate, ARM check, and Phase 0 binary/RSS/startup comparison.

## Rollback drill

- Create history in a copied test home, disable the feature, and prove flows, deploy, rollback,
  MQTT, AI, and audit still work.
- Re-enable it and prove the database is readable without destructive downgrade.
- Inject a failed migration and restore the database copy.
- Fill or corrupt the database and prove active flows continue while health reports the fault.
- Preserve the database for export/forensics; do not delete it automatically during rollback.

## Non-goals

- Do not move flows, credentials, context, or security audit into SQLite.
- Do not add PostgreSQL or multi-tenant storage.
- No new AI nodes or WASM.

## Completion criteria

- History is optional, bounded, redacted, and operationally observable.
- Database failure cannot corrupt or stop flow deployment/execution.
- Minimal builds remain free of the database dependency.
- Migration and feature-disable rollback have been demonstrated.

## Final response format

Report schema and failure policy, files/dependencies changed, test counts, secret-scan evidence,
resource delta, migration and rollback drills, platform limits, Git status, and explicit
no-commit/no-push/no-release confirmation.
