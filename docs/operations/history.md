# Operational History

n2link can keep a bounded SQLite record of deploys, node status and throttled node errors,
Flow Copilot draft outcomes, fleet push/promote, and runtime start/stop. It is fail-open.
The default app build includes `history_sqlite`. Recording stays off until `[history] enabled = true`.

`flows.json` and `flows_cred.json` remain the flow and credential source of truth. `audit.log`
remains the security and operator audit log. History never stores message payloads, flow
bodies, credentials, tokens, prompts, or provider completions.

## Enable

The default `cargo run -- run` binary already includes SQLite history. Set `[history] enabled = true` in the home overlay:

```bash
N2LINK_HOME="$PWD" cargo run -- run
```

A minimal binary can omit it with `--no-default-features` (and the features you still need).

```toml
[history]
enabled = true
# path = "history.sqlite3"
# queue_capacity = 1024
# batch_max = 256
# retention_days = 30
# max_db_bytes = 16777216
# shutdown_drain_ms = 2000
# node_error_interval_ms = 1000
# migrate = false
```

Relative `path` values resolve against the flows directory. Unknown keys and out-of-range
values fail startup. A build without `history_sqlite` that sets `enabled = true` fails
startup with `NotSupported`. Database faults do not fail startup: `/status` reports
`history.state` as `failed` and flows continue.

The file is created with mode `0600` on Unix. WAL sidecar files (`-wal`, `-shm`) appear
beside it. Do not commit them; the repository ignores `/history.sqlite3` and `/history.db`.

## Query

`GET /history` is an editor/admin route and requires `history.read` (included in the
viewer `read` scope). Query parameters: `limit` (1..=500), `before`, `since_ms`,
`until_ms`, `category`, `kind`, `subject`.

`/status` includes a `history` object with state, schema version, queue counters, and
drop metrics.

## Disable and rollback

Set `enabled = false` and restart, or run a binary built without `history_sqlite`
(`--no-default-features` plus the features you still need). The database
file is left in place. Deleting it only removes history; it does not change flows,
credentials, or audit logs.

Forward schema migration is explicit (`migrate = true`). A file with a newer
`user_version` than this build marks history `failed` (`newer_schema`) and does not
rewrite the file.

## Events

| Category | Kinds |
|---|---|
| `deploy` | `deploy.proposed`, `deploy.accepted`, `deploy.rejected`, `deploy.rollback` |
| `node` | `node.error`, `node.status` |
| `copilot` | `copilot.draft.requested`, `copilot.draft.produced`, `copilot.draft.rejected` |
| `fleet` | fleet push and promote outcomes |
| `runtime` | `runtime.started`, `runtime.stopped` |
| `history` | gap markers when the queue drops events |

Recording uses a bounded channel (`try_send`). A full queue increments `dropped_queue_full`
and never blocks a flow task.
