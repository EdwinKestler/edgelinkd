# Phase 4 Durable Execution History Report

- Status: closed locally; uncommitted
- Date: 2026-10-04 (America/Guatemala)
- Branch: `master`
- Base revision: `b93d052fc0b582f34bcd5da2bcbecf4a07d0b8e3`
- Version: unchanged at `0.3.0`

Phase 4 introduces optional, bounded, queryable operational execution history backed by SQLite behind
the `history_sqlite` feature flag. It captures lifecycle events for deployments, node status transitions,
throttled node errors, Flow Copilot draft states, fleet push/promote operations, and runtime startup/shutdown.

Flow execution, deploy, rollback, credential storage, and security audit remain completely independent:
database failure never blocks, delays, or corrupts active flows or credentials. Minimal builds remain 100%
free of SQLite linkage or dependencies.

## Schema & Failure Policy

### Schema Version 1

- SQLite Database Pragma:
  - `PRAGMA application_id = 0x454C4831;` (`"ELH1"`, integer `1162627121`)
  - `PRAGMA user_version = 1;`
  - `PRAGMA journal_mode = WAL;`
  - `PRAGMA synchronous = NORMAL;`
  - `PRAGMA auto_vacuum = INCREMENTAL;`
  - `PRAGMA busy_timeout = 1000;`
  - File permissions: mode `0600` on POSIX systems.
- Table: `events`
  ```sql
  CREATE TABLE IF NOT EXISTS events (
      seq      INTEGER PRIMARY KEY AUTOINCREMENT,
      at_ms    INTEGER NOT NULL,
      run_id   TEXT    NOT NULL,
      category TEXT    NOT NULL,
      kind     TEXT    NOT NULL,
      outcome  TEXT,
      actor    TEXT    NOT NULL,
      subject  TEXT,
      detail   TEXT    NOT NULL CHECK (json_valid(detail) AND length(detail) <= 1024)
  );
  CREATE INDEX IF NOT EXISTS events_at_ms ON events(at_ms);
  CREATE INDEX IF NOT EXISTS events_category_seq ON events(category, seq);
  CREATE INDEX IF NOT EXISTS events_kind_seq ON events(kind, seq);
  ```

### Failure & Degradation Policy

- **Fail-Open by Construction**:
  The history subsystem operates via a bounded in-memory `sync_channel` (default capacity: 1,024). Event
  production is non-blocking (`try_send`). If the channel is full, the event is immediately dropped and
  incremented on `dropped_queue_full` counter; the event producer never waits.
- **Fault Tolerance**:
  If the SQLite file cannot be opened, is read-only, has corrupt headers, is locked, or disk is full, the
  worker thread marks the subsystem `state = HistoryState::Failed` and sets `last_error`. In-flight and
  incoming events are dropped without blocking (`dropped_write_error`).
- **Health Reporting**:
  Subsystem state (`ok`, `starting`, `disabled`, `degraded`, `failed`, `stopped`), counters, drop metrics,
  and error details are reported in `/status` under the `history` object.
- **Boundary Separation**:
  `flows.json` and `flows_cred.json` remain the sole flow and credential source of truth. `audit.log`
  remains the authoritative security audit log. EdgeLinkd never reads flows or credentials from history.

### Redaction by Construction

- Message bodies, payloads, parameters, flow graphs, prompts, and provider completions are strictly excluded.
- Node error messages are sanitized and throttled (per-node interval 1,000 ms by default, reporting suppression count).
- Actor names are sanitized, control characters stripped, and bounded to 64 chars.
- Subject identifiers are bounded to 128 chars.
- Detail JSON is strictly validated and capped at 1,024 bytes.

## Files and Dependencies Changed

### Dependency Management

- Workspace root `Cargo.toml`: Added `rusqlite = { version = "0.40", default-features = false, features = ["bundled"] }`
  under `[workspace.dependencies]`.
- Feature propagation: `history_sqlite` feature flag added to root workspace, `edgelink-core`, and `edgelink-web`.
  In feature-minimal and default builds, `rusqlite` is completely absent.
- `cargo tree -p edgelink-core --no-default-features` confirms **zero** SQLite dependencies in minimal builds.

### Modified Files

- `Cargo.toml`, `Cargo.lock`
- `crates/core/Cargo.toml`
- `crates/core/src/runtime/history.rs` (new, history subsystem implementation)
- `crates/core/src/runtime/mod.rs`
- `crates/core/src/runtime/engine.rs` (integrated history recording for lifecycle, redeploy, node error, status)
- `crates/core/src/runtime/nodes/mod.rs` (node status reporting)
- `crates/core/src/runtime/credential_storage.rs` (cfg-gated test module imports)
- `crates/web/Cargo.toml`
- `crates/web/src/handlers/history.rs` (new, `GET /history` query API)
- `crates/web/src/handlers/mod.rs`, `crates/web/src/handlers/web_state.rs`
- `crates/web/src/handlers/status.rs` (added `history` field to status response)
- `crates/web/src/handlers/auth.rs` (assigned `history.read` permission)
- `crates/web/src/handlers/flows.rs` (recorded deploy proposed/accepted/rejected/rollback)
- `crates/web/src/handlers/assistant.rs` (recorded copilot draft lifecycle)
- `crates/web/src/handlers/fleet.rs` (recorded fleet push and promote)
- `crates/web/src/protection.rs` (classified `/history` as `EndpointClass::EditorAdmin`)
- `crates/web/src/api.rs` (registered `/history` route when `history_sqlite` is active)
- `crates/web/src/server.rs` (initialized history from config and hooked graceful shutdown)
- `src/defaults.rs` (documented `[history]` configuration section)
- `adoption/phase4/DESIGN.md` (architecture design)
- `adoption/phase4/REPORT.md` (this report)
- `adoption/phase4/fixtures/*` (test configurations and flows)

## Test & Validation Evidence

| Check | Result |
|---|---|
| `cargo fmt --check` | Passed cleanly |
| `cargo clippy --all-features --tests --all -- -D warnings` | Passed with 0 warnings |
| `cargo test -p edgelink-core --features history_sqlite` | 264 passed, 0 failed, 1 ignored, 2 doctests passed |
| `cargo test -p edgelink-web --features history_sqlite` | 71 passed, 0 failed |
| `cargo test --workspace --features full` | Passed (283 core + 75 web + 2 doctests) |
| `.venv/bin/pytest ./tests -v` | 825 passed, 213 skipped in 119.78 s |
| ARMv7 cross-compilation (`armv7-unknown-linux-gnueabihf`) | Passed with code 0 |
| Minimal build dependency audit | Passed (0 sqlite dependencies found in `--no-default-features`) |
| Secret scanner check | 0 sensitive tokens, prompts, or message bodies found in Phase 4 files |
| Palimnex index & deep validation | Passed (`fresh`, `validation: passed`, 267 files, 1357 chunks, 6426 symbols) |

## Rollback Drill

The rollback drill was executed against `target/ci/edgelinkd-history` in a fresh copied test home (`/tmp/edgelinkd-p4-drill`):

1. **Step 1 - Start with History Enabled**:
   - Runtime started cleanly; `/status` reported `history.state = "ok"`, `schemaVersion = 1`.
   - `history.db` was created with POSIX mode `0600`.
   - PRAGMA checks verified `application_id = 0x454c4831` and `user_version = 1`.
   - `POST /flows` deployed successfully (rev `df845255a4422c006b1f16b9327d6d1af74db94dc9fa6e6a877ace6a722f2605`).
   - `GET /history` returned status 200 with recorded events (`deploy.proposed`, `deploy.accepted`, `runtime.started`, `runtime.stopped`).
   - Database rows checked directly; no secrets, passwords, or raw message bodies were present.
2. **Step 2 - Disable History Feature (`[history] enabled = false`)**:
   - Runtime started cleanly; `/status` reported `history.state = "disabled"`.
   - Active flows, `POST /flows`, and `POST /flows/rollback` executed with 200 OK.
   - `GET /history` returned 404.
   - SHA-256 of `history.db` remained identical (`f947c5663179b01af1cf562af71a8d111db0491b13dc6c635fdac1b5fbec497f`).
   - Database was preserved untouched for forensic export; never deleted or modified.
3. **Step 3 - Re-enable History**:
   - Runtime started with `[history] enabled = true`.
   - Previous events remained intact and queryable via `GET /history` without destructive downgrade or data loss.
4. **Step 4 - Corrupted Database Injection**:
   - Injected corrupt non-SQLite header data into `history.db`.
   - Runtime started successfully; active flows continued executing without interruption.
   - `/status` reported `history.state = "failed"` with `lastError = "corrupt"`.
   - Flow deployments succeeded normally (fail-open verified!).
5. **Step 5 - Failed Migration Injection (Future Schema Version 999)**:
   - Configured `history.db` with `PRAGMA user_version = 999;`.
   - Runtime started successfully; flows executed normally.
   - `/status` reported `history.state = "failed"` with `lastError = "newer_schema"`.
   - Database restored from backup and verified `state = "ok"`.

## Resource Delta

### Executable Binary Size (Stripped `ci` profile, `stat -c %s`)

| Configuration | Phase 0 | Phase 2 | Phase 3 | Phase 4 | Delta vs Phase 3 |
|---|---:|---:|---:|---:|---:|
| Default (no history) | 14,420,296 | 14,966,152 | 15,088,776 | 15,125,896 | +37,120 (+0.25%) |
| Root no-default | 14,259,480 | 14,719,832 | 14,837,784 | 14,876,568 | +38,784 (+0.26%) |
| Root `full` | 14,420,296 | 14,966,152 | 15,088,712 | 15,125,896 | +37,184 (+0.25%) |
| With `history_sqlite` | N/A | N/A | N/A | 16,274,808 | +1,186,032 (+7.86%) |
| All features | 14,509,464 | 15,053,336 | 15,180,056 | 16,362,200 | +1,182,144 (+7.79%) |

When the `history_sqlite` feature is disabled, the binary footprint delta is only ~37 KiB (representing core handler and trait structures). The bundled SQLite C engine and rusqlite bindings only exist in binaries where `history_sqlite` is explicitly compiled in.

### Runtime Metrics (Warm Fixture)

| Metric | Phase 0 Baseline | Phase 3 | Phase 4 (Default) | Phase 4 (History Enabled) |
|---|---:|---:|---:|---:|
| Startup median (5 samples) | 7.000 ms | 12.005 ms | 13.452 ms | 13.636 ms |
| Idle RSS median (5 samples) | 13,404 KiB | 14,156 KiB | 14,956 KiB | 17,132 KiB |
| `GET /api/health` median (100 req) | 0.337 ms | 0.214 ms | 0.082 ms | 0.077 ms |
| `GET /api/health` p95 (100 req) | 0.466 ms | 0.440 ms | 0.160 ms | 0.152 ms |

Enabling SQLite history adds only ~2.1 MiB of idle RSS (for the SQLite WAL connection, page cache, bounded writer channel, and worker thread).

## Platform Limits & Operational Notes

- SQLite concurrency: WAL mode allows single-writer with concurrent read-only queries.
- Permissions: Database file created with `0600` permissions.
- In-memory limits: Bounded writer queue defaults to 1,024 elements; events drop on full queue to protect latency.
- Retention: Incremental vacuum with automatic periodic cleanup based on `retention_days` and `max_db_bytes`.

## Git & Release Status

Phase 4 implementation is completely uncommitted in the local working tree on `master`.
- No git commits have been made.
- No git pushes have been executed.
- No git tags have been created.
- Package version remains unchanged at `0.3.0`.
