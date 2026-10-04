# Phase 0 Sanitized Installation Fixture

These files model one complete current/previous deploy generation:

- `flows.json` and `flows_cred.json` are the live pair.
- `flows.json.prev` and `flows_cred.json.prev` are the rollback pair.
- `edgelinkd.toml` requests loopback port 19888 and uses an in-memory context store. The current
  `run` command's default `--bind` value takes precedence over the file, so isolated probes must
  also pass `run --bind 127.0.0.1:19888` explicitly.

The graph includes dormant MQTT, AI, and HTTP configurations so later migrations can exercise
all three credential shapes. MQTT auto-connect is disabled, and no inject node triggers the HTTP
or AI paths. `example.invalid` is reserved for examples. Every credential value starts with
`fixture-` and is intentionally invalid; none came from the environment or a user installation.

Copy this directory to a temporary directory before a runtime, migration, or rollback drill.
Never point a drill at the repository's live `flows.json` or `flows_cred.json`.
