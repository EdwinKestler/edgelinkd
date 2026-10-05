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

## What does not exist yet

- **There is no way to install a plugin.** The package store (stage, quarantine, self-test,
  activate, rollback) is not implemented, so in a real deployment every `wasm-*` node reports
  that its plugin is not active. Plugins are activated only by tests.
- `GET /wasm/plugins` and `edgelinkd plugin list` answer `NotSupported` (HTTP 501 for the API)
  instead of an empty list.
- Plugin configuration (manifest `[[node.config]]`), generated editor nodes and Copilot catalog
  entries are not implemented; a plugin node with any property beyond the editor's own
  (`x`, `y`, `info`, `l`, `wasmPlugin`) fails deploy.

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
| `require_signature` | `false` | `true` fails startup (not implemented) |

Unknown keys fail startup. `dir`, `max_plugins` and `max_module_kib` belong to the plugin
store and fail startup with `NotSupported` until it exists. Fuel and deadline defaults are
provisional until the G1 Raspberry Pi-class measurements are recorded.
