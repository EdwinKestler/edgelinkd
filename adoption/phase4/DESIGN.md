# Phase 4 Design - Optional Durable Execution History

- Status: implemented locally; uncommitted
- Base revision: `b93d052` (Phase 3 committed and pushed by the maintainer)
- Feature: `history_sqlite`, off by default and absent from `full`

## Boundaries

`flows.json` stays the flow source of truth and `audit.log` stays the security/operator audit
boundary. Every existing `audit.record` call is unchanged. History is a separate, deletable,
best-effort operational record: losing it never changes a valid flow, a credential, a deploy
outcome, or an audit line.

| Record | Required? | Store |
|---|---|---|
| Login, deploy, rollback, config, library, fleet, Copilot audit facts | Required (existing behaviour) | `audit.log` |
| Deploy proposed/accepted/rejected, rollback accepted/rejected | Best-effort | history |
| Node error and node status transitions | Best-effort | history |
| Copilot draft requested/produced/rejected | Best-effort | history |
| Fleet push/promote outcomes | Best-effort | history |
| Runtime started/stopped, history gap markers | Best-effort | history |

Flow execution, deploy, and rollback never wait for history. Applying a Copilot draft is an
ordinary editor deploy and appears as a deploy event; the server cannot observe a user
accepting a preview. Per-message execution summaries are not recorded in this phase.

## Feature gate and configuration

The root `history_sqlite` feature enables `edgelink-core/history_sqlite` and
`edgelink-web/history_sqlite`, which pull in `rusqlite` with the `bundled` SQLite amalgamation.
No other feature implies it. Without the feature there is no SQLite code, no `/history` route,
and no `history` field in `/status`. A configuration that sets `[history] enabled = true` on a
build without the feature fails startup with `EdgelinkError::NotSupported`.

```toml
[history]
enabled = false
# path = "history.sqlite3"     # relative paths resolve against the flows directory
queue_capacity = 1024          # 16..=65536 events
batch_max = 256                # 1..=queue_capacity events per transaction
retention_days = 30            # 1..=3650
max_db_bytes = 16777216        # 1 MiB..=4 GiB main database ceiling
shutdown_drain_ms = 2000       # 1..=30000
node_error_interval_ms = 1000  # 0..=3600000; per-node error coalescing window
migrate = false                # permit an explicit forward schema migration at startup
```

Unknown keys and out-of-range values fail startup. Database faults never fail startup.

## Event schema (schema version 1)

```sql
CREATE TABLE events (
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
CREATE INDEX events_at_ms ON events(at_ms);
CREATE INDEX events_category_seq ON events(category, seq);
CREATE INDEX events_kind_seq ON events(kind, seq);
```

- `PRAGMA application_id = 0x454C4831` (`ELH1`) identifies the file. A database with another
  application id, or with tables but no id, is refused as foreign.
- `PRAGMA user_version` is the schema version.
- `seq` is assigned by the single writer in queue order. It is the authoritative order and is
  never reused, including after pruning. Events from one producer keep their order; events
  from concurrent producers are ordered by the moment they entered the queue.
- `at_ms` is the producer's wall clock (Unix milliseconds). It can move backwards with clock
  changes and is only a display/filter value.
- `run_id` is a random UUID per process start, so restarts are visible.
- `actor` is the authenticated username, `anonymous` for open installs, or `runtime`.

| Category | Kind | Outcome | Subject | Detail fields |
|---|---|---|---|---|
| `deploy` | `deploy.proposed` | - | offered revision | `scope`, `nodes` |
| `deploy` | `deploy.accepted` | `ok` | revision | `scope`, `nodes` |
| `deploy` | `deploy.rejected` | `rejected` | offered revision | `scope`, `reason` |
| `deploy` | `deploy.rollback` | `ok`/`rejected` | revision | `reason` when rejected |
| `node` | `node.error` | `failed` | node id | `type`, `suppressed` |
| `node` | `node.status` | - | node id | `type`, `fill`, `shape` |
| `copilot` | `copilot.draft.requested` | - | - | - |
| `copilot` | `copilot.draft.produced` | `ok` | - | `nodes` |
| `copilot` | `copilot.draft.rejected` | `rejected` | - | `reason` |
| `fleet` | `fleet.push`/`fleet.promote` | `ok`/`failed` | device or `from->to` | `status`, `rev` |
| `runtime` | `runtime.started`/`runtime.stopped` | - | - | `version`, `schema` |
| `history` | `history.gap` | - | - | `queue_full`, `write_error` |

## Redaction policy

Redaction is by construction. Rows are built only from typed events with fixed fields:
identifiers, counts, revisions, enumerated reason codes, and enumerated status fill/shape.
There is no free-form text column.

- Message bodies, node error text, and status text are never stored. A function node can put
  `msg` data into either string, so only the node id/type and the coarse fill/shape are kept.
- Deploy and Copilot errors are stored as fixed reason codes, never as error messages, flow
  JSON, prompts, provider responses, or credentials.
- Identifiers are stripped of control characters and truncated to 128 bytes. Revisions must be
  1-64 hexadecimal characters or are stored as `invalid`.
- Health and logs report error classes such as `full`, `read_only`, `corrupt`, and
  `permission`, not SQLite messages or file paths.

## Storage

- Default path: `<flows directory>/history.sqlite3`; `history.path` overrides it.
- On Unix the file is created or reset to mode `0600` before SQLite opens it. SQLite creates
  `-wal` and `-shm` with the database file's mode.
- `journal_mode = WAL`, `synchronous = NORMAL`, `busy_timeout = 1000`,
  `auto_vacuum = INCREMENTAL` (set before the first table), and
  `journal_size_limit = min(4 MiB, max_db_bytes / 4)`. `NORMAL` can lose the most recent
  transactions on power loss but cannot corrupt the database; this matches best-effort
  history.
- `max_page_count` is set from `max_db_bytes`, so SQLite itself enforces the hard ceiling.
- Backup: operators copy the file while EdgeLinkd is stopped, or use `sqlite3 .backup` while
  running. The file is never deleted automatically, including when the feature is disabled.

## Migration policy

- A new or empty file is initialized directly to the current version in one transaction.
- Matching version: used as is.
- Newer version: refused without writing; history is `failed` with `newer_schema`. A
  downgraded binary therefore never touches newer data.
- Older version with `migrate = false`: refused without writing (`migration_required`).
- Older version with `migrate = true`: `VACUUM INTO <path>.v<old>.<unix-ms>.bak` (mode
  `0600`) first, then all pending migrations and the version update in one transaction. An
  interrupted or failed migration rolls back to the old version; the backup remains.

## Writer, queue, retention, and size

- Producers call a non-blocking `record` that formats a bounded row and `try_send`s it into a
  `std::sync::mpsc::sync_channel(queue_capacity)`. A full queue drops the new event and
  increments `dropped_queue_full`. Producers never await, lock the database, or panic.
- One dedicated OS thread owns the write connection and commits up to `batch_max` events per
  transaction.
- After rows were dropped, the writer inserts a `history.gap` row with the counts, so loss is
  visible in the data as well as in health.
- Node errors are coalesced per node: at most one row per `node_error_interval_ms`; the next
  recorded error carries a `suppressed` count. Status rows are written only when fill/shape
  changes. Both maps are capped at 4,096 nodes and reset on overflow and on redeploy.
- Retention runs at most once a minute and after a size warning: rows older than
  `retention_days` are deleted in bounded chunks. If used pages exceed 80% of the ceiling, the
  oldest rows are deleted in 512-row chunks until usage is at most 70%, and the freed pages are
  released with `incremental_vacuum`.
- If an insert reaches `SQLITE_FULL`, the writer prunes once and retries the batch; if that
  fails, the batch is dropped and counted as `dropped_write_error`.
- Shutdown: closing rejects new events (`dropped_shutdown`), the writer drains the queue for up
  to `shutdown_drain_ms`, drops and counts the remainder, and closes the connection.

## Health

`GET /status` gains a `history` object only in builds with the feature:

```json
{ "state": "ok", "schemaVersion": 1, "queueCapacity": 1024, "queued": 0,
  "accepted": 10, "written": 10, "droppedQueueFull": 0, "droppedWriteError": 0,
  "droppedShutdown": 0, "pruned": 0, "dbBytes": 32768,
  "lastError": null, "lastErrorAtMs": null }
```

States: `disabled`, `starting`, `ok`, `degraded` (last write failed but the writer is alive),
`failed` (cannot open, corrupt, foreign, newer schema, or migration required; events are
dropped and counted), and `stopped`. State changes are logged once with the error class.

## Query API

`GET /history` is registered only in feature builds. It is classified as `editor_admin` by
Phase 3 and requires the new `history.read` permission, which `read`/viewer scopes include,
the same exposure as `/audit`.

| Parameter | Rule |
|---|---|
| `limit` | 1..=500, default 100 |
| `before` | positive `seq`; returns rows with smaller `seq` |
| `since_ms`, `until_ms` | Unix milliseconds, `since_ms <= until_ms` |
| `category` | one of the six categories |
| `kind` | `^[a-z][a-z._]{0,63}$` |
| `subject` | 1..=128 bytes without control characters |

Unknown or malformed parameters return `400 invalid_filter`. A runtime-disabled store returns
`404 not_supported`; a failed store returns `503 history_unavailable`. Rows are newest first;
`next` is the cursor for the following page or `null`. Reads use a separate read-only
connection with a 2-second interrupt deadline. The maximum response is bounded by 500 rows of
at most about 1.5 KiB each.
