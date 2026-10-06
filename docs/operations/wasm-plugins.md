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
  network, clock, randomness or credentials. Guests export `memory`, `el_abi_version`,
  `el_alloc` and `el_on_input`, and optionally `el_init` (receives the node configuration) and
  `el_close` (runs when the node stops). Missing or mistyped exports fail `stage`.
- Each message runs under fuel, a wall-clock deadline, a linear-memory cap and a global
  concurrency limit; repeated faults put the node in a failed state until redeploy.

## Node configuration

A manifest declares per-node options; the editor shows one input per field and the plugin
receives the resolved values as one object in `el_init` (a manifest with fields must export
`el_init`):

```toml
[[node.config]]
name = "delimiter"      # [a-z][a-zA-Z0-9]{0,31}; not id, type, z, g, x, y, l, d, name, wires, info, credentials, wasmPlugin
kind = "string"         # string | number | boolean | enum
label = "Delimiter"
default = ","
max_len = 1             # string only (≤ 4096)

[[node.config]]
name = "limit"
kind = "number"
integer = true          # number only, with min / max
min = 1.0
max = 100.0
default = 10

[[node.config]]
name = "mode"
kind = "enum"
values = ["fast", "safe"]   # enum only, 1-32 values
required = true             # with self-tests, a required field needs a default
```

Deploy fails when a node sets a property the plugin does not declare, a value of the wrong
kind or out of range, or omits a required field. Numbers typed into the editor as strings are
accepted; an empty string counts as unset for non-string kinds.

## Installing a plugin online (admin API)

With `enabled = true` the running edgelinkd owns the store and serves these routes. All of them
need the administrator role (viewers and deployers cannot read them either), are limited by
`[api_protection.plugins]` (1 MiB body, 6 requests/min, one at a time by default) and
serialise with flow deploys.

| Method and path | Body | Result |
|---|---|---|
| `GET /wasm/plugins` | — | `{active, packages}`: pointers and stage reports, no bytes |
| `POST /wasm/plugins/stage` | the package, `Content-Type: application/wasm` | stage report with `sha256` and `status` (`ready` or `rejected`) |
| `POST /wasm/plugins/{publisher}/{name}/activate` | `{"sha256": "…"}` | `{active, previous, activatedAt, editorReloadRequired: true}` |
| `POST /wasm/plugins/{publisher}/{name}/rollback` | `{"sha256": "<current previous>"}` | same |
| `DELETE /wasm/plugins/{publisher}/{name}` | — | 409 `in_use` while a deployed node uses the type |
| `DELETE /wasm/plugins/quarantine/{sha256}` | — | deletes a package that is not active |

Activation and rollback build the deployed flows (with credentials) using the candidate plugin
set; if that fails nothing changes (409 `invalid_flows`). Otherwise the pointer is written, the
registry swapped and the whole graph redeployed. If the redeploy fails, the pointer, the
registry and the graph are put back (409 `activation_failed`). Errors carry a stable `code`:
`invalid_id`, `invalid_digest`, `plugins_disabled`, `unsupported_media_type`, `too_large`,
`manifest_invalid`, `import_forbidden`, `selftest_failed`, `budget_exceeded`,
`digest_mismatch`, `not_found`, `conflict`, `invalid_flows`, `in_use`, `activation_failed`,
`store_error`. A publisher named `quarantine` cannot be removed through the API (the
`quarantine/{sha256}` route takes precedence); use the CLI.

After activation, reload the editor: plugin types then appear in the palette (generated from
the manifest, every plugin string escaped), in `/nodes` as module `wasm/<publisher>/<name>`,
and in the Copilot catalog (type, ports, output labels and config field names and kinds; never
the description or help). `/status` gains a `wasm` section (`state`, `plugins`, `engineLive`,
`permitsInUse`, `memoryReservedKib`). History (category `plugin`) and the audit log record
`plugin.staged`, `plugin.rejected`, `plugin.activated`, `plugin.rolled_back`, `plugin.removed`,
`plugin.discarded` and `plugin.failed` (three faults) with the id, version, the first 12 hex
digits of the digest and a reason code.

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

- No guest SDK yet: `crates/wasm-guest` has the raw imports only; packages are WAT or
  hand-written Rust plus `edgelinkd plugin pack`.
- The editor learns about new plugin types only on reload; there is no live palette push.
- Signatures (`require_signature`) are not implemented.

## Settings (`[runtime.wasm]`)

| Key | Default | Range |
|---|---:|---|
| `enabled` | `false` | TOML boolean only |
| `max_concurrent` | 2 | 1–64 |
| `memory_budget_kib` | 8192 | 64–1048576; admission per deployed graph |
| `default_memory_pages` / `max_memory_pages` | 8 / 256 | 64 KiB pages (512 KiB default) |
| `default_fuel` / `max_fuel` / `fuel_slice` | 2·10⁷ / 10⁹ / 10⁶ | `fuel_slice ≤ default_fuel` |
| `default_deadline_ms` / `max_deadline_ms` | 250 / 5000 | ms |
| `max_input_kib` | 64 | 1–1024; encoded message size |
| `failure_threshold` / `failure_window_s` | 3 / 60 | faults before the failed state |
| `dir` | `plugins` | store directory, relative to the home directory or absolute |
| `max_plugins` | 16 | 1–256 active plugins |
| `max_module_kib` | 512 | 1–4096; package size cap |
| `require_signature` | `false` | `true` fails startup (not implemented) |

Unknown keys fail startup. A manifest may request `[limits]` up to the `max_*` ceilings; a
request above one fails `stage` naming both keys.

The memory budget reserves each plugin node's cap (`memory_pages` × 64 KiB) plus about 8× its
module size for compiled code, for the whole deployed graph; real use is far lower (≈ 82 KiB per
instance on a Pi 5). With the defaults the budget admits about 15 nodes. On a board with RAM to
spare, raise it, for example on a Raspberry Pi 5:

```toml
[runtime.wasm]
memory_budget_kib = 65536   # 64 MiB
max_concurrent = 4
```

A module whose initial linear memory is larger than its limit is rejected at `stage` with the
fix in the message. Rust guests built for `wasm32-unknown-unknown` reserve a 1 MiB stack by
default (17+ pages): either request `[limits] memory_pages = 32` or link with
`-C link-arg=-zstack-size=65536`. Fuel and deadline defaults are
calibrated on a Raspberry Pi 5 (≈ 4.8·10⁵ fuel/ms: the default fuel is ≈ 42 ms of guest work
there); slower or 32-bit boards reach the 250 ms deadline sooner and are not yet measured.
