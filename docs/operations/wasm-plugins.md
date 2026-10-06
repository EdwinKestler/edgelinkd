# WASM plugins (experimental prototype)

Optional third-party flow nodes compiled to `wasm32-unknown-unknown` and run by the Wasmi
interpreter. Design: `adoption/phase7/ADR-0002-wasm-node-sdk.md` and
`adoption/phase7/DESIGN.md`. Status: `adoption/phase7/REPORT.md`.

## What works today

- The default binary does **not** include the interpreter. Build with `--features nodes_wasm`.
- Even then, plugins are off until the home overlay sets:

  ```toml
  [runtime.wasm]
  enabled = true
  ```

- Plugin types are `wasm-<publisher>-<name>`. A flow that names one fails deploy with
  `NotSupported` in every build — never the silent `unknown` node — and the message says why:
  not compiled, disabled by configuration, or plugin not active.
- Guests may import only `edgelink:node/v1` `{emit, log, status, fail}`. No WASI, filesystem,
  network, clock, randomness or credentials.
- Each message runs under fuel, a wall-clock deadline, a linear-memory cap and a global
  concurrency limit; repeated faults put the node in a failed state until redeploy.

## Installing a plugin (offline CLI)

A package is a `wasm32-unknown-unknown` module with its manifest (TOML, schema 1) embedded as
the `edgelink.manifest` custom section. Until the guest SDK ships, embed it with `pack`.

Stop edgelinkd first: when plugins are enabled the runtime holds `<home>/plugins/.lock` and
every command below refuses to run while it does.

```sh
edgelinkd plugin pack echo.wasm plugin.toml -o echo.pkg.wasm
edgelinkd plugin stage echo.pkg.wasm            # validate → quarantine → self-test; prints sha256
edgelinkd plugin activate acme/echo --sha256 <hex>
edgelinkd plugin list                           # active generations and quarantined packages
edgelinkd plugin rollback acme/echo --sha256 <previous hex>
edgelinkd plugin remove acme/echo               # refused while a flow node uses the type
edgelinkd plugin discard <hex>                  # delete a quarantined package
edgelinkd plugin verify                         # re-hash active generations; non-zero on problems
```

- `stage` rejects a package for any framing, manifest, import, compile or limit error and keeps
  nothing; a package that validates but fails its `[[selftest]]` stays in quarantine marked
  `rejected` and cannot be activated. At most 8 packages wait in quarantine.
- `activate` and `rollback` first build the flows edgelinkd would deploy (`flows.json` plus
  credentials, as if `enabled = true`) with the candidate plugin set. If that graph does not
  build, nothing on disk changes. Nothing is started.
- Each plugin keeps its current and one previous generation; the generation before that is
  deleted. `remove` moves both back to quarantine.
- The commands work whether or not `enabled` is set, so packages can be staged and activated
  before plugins are turned on. Nothing runs until edgelinkd starts with `enabled = true`.

Store layout under `<home>/<dir>` (default `plugins`), directories `0700`, files `0600`,
symlinks refused:

```text
.lock  staging/  quarantine/<sha>.wasm|.json  store/<sha>.wasm|.json  active.toml  active.toml.prev
```

At startup with `enabled = true`, leftover uploads and unreferenced store files are cleaned up,
an interrupted pointer write leaves the old or the new `active.toml` (never a partial one),
and a generation whose file is missing or fails its digest is left out and logged; flows that
use it fail to deploy naming the plugin, other plugins still run. With `enabled = false` the
directory is not opened.

## What does not exist yet

- Online install: `GET /wasm/plugins` still answers HTTP 501. Use the CLI with the runtime
  stopped.
- Plugin configuration (manifest `[[node.config]]`), generated editor nodes and Copilot catalog
  entries are not implemented; a plugin node with any property beyond the editor's own
  (`x`, `y`, `info`, `l`, `wasmPlugin`) fails deploy, and the editor shows plugin nodes as
  unknown types until PR 6.
- No guest SDK yet: `crates/wasm-guest` has the raw imports only.

## Settings (`[runtime.wasm]`)

| Key | Default | Range |
|---|---:|---|
| `enabled` | `false` | TOML boolean only |
| `max_concurrent` | 2 | 1–64 |
| `memory_budget_kib` | 8192 | 64–1048576; admission per deployed graph |
| `default_memory_pages` / `max_memory_pages` | 32 / 256 | 64 KiB pages |
| `default_fuel` / `max_fuel` / `fuel_slice` | 2·10⁷ / 10⁹ / 10⁶ | `fuel_slice ≤ default_fuel` |
| `default_deadline_ms` / `max_deadline_ms` | 250 / 5000 | ms |
| `max_input_kib` | 64 | 1–1024; encoded message size |
| `failure_threshold` / `failure_window_s` | 3 / 60 | faults before the failed state |
| `dir` | `plugins` | store directory, relative to the home directory or absolute |
| `max_plugins` | 16 | 1–256 active plugins |
| `max_module_kib` | 512 | 1–4096; package size cap |
| `require_signature` | `false` | `true` fails startup (not implemented) |

Unknown keys fail startup. A manifest may request `[limits]` up to the `max_*` ceilings; a
request above one fails `stage` naming both keys. Fuel and deadline defaults are
calibrated on a Raspberry Pi 5 (≈ 4.8·10⁵ fuel/ms: the default fuel is ≈ 42 ms of guest work
there); slower or 32-bit boards reach the 250 ms deadline sooner and are not yet measured.
