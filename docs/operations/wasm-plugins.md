# WASM plugins (experimental)

Optional third-party flow nodes compiled to `wasm32-unknown-unknown` and run by the Wasmi
interpreter inside EdgeLinkd. This is the operator manual: enabling, installing, upgrading,
rolling back, observing and troubleshooting plugins. Writing plugins:
[`docs/development/wasm-plugins.md`](../development/wasm-plugins.md). Design and measurements:
[`adoption/phase7/`](../../adoption/phase7/).

## When to use plugins

Use a plugin for message logic you want to add or update without rebuilding EdgeLinkd: device
protocol decoders, parsers, filters, unit conversion, vendor algorithms shipped as binaries. A
plugin cannot do I/O. Devices, brokers and services are reached by the built-in nodes, and the
plugin processes what they carry (see [Connecting devices](#connecting-devices)). For a few lines
of ad-hoc logic, the `function` node is simpler.

## Enabling

1. Build with the host compiled in (the default binary does **not** include it):

   ```sh
   cargo build --release --features nodes_wasm
   ```

   It adds about 1.2–1.4 MiB to the binary and about 50–120 KiB of private memory while no
   plugin node runs (measurements: `adoption/phase7/REPORT.md`).

2. Turn plugins on in the home overlay (`n2linkd.toml` or `n2linkd.<env>.toml`):

   ```toml
   [runtime.wasm]
   enabled = true
   ```

   Without this, nothing under the plugin directory is read and no interpreter is created.

Plugin types are `wasm-<publisher>-<name>`. A flow that names one fails deploy with
`NotSupported` in every build — never the silent `unknown` node — and the message says why: not
compiled, disabled by configuration, or plugin not active.

## Security model

- Guests may import only `edgelink:node/v1` `{emit, log, status, fail}`. No WASI, filesystem,
  network, clock, randomness, environment, credentials, context or deploy access. Any other
  import, an imported memory, a start function, SIMD or threads fail `stage`.
- Every message runs under fuel, a wall-clock deadline, a linear-memory cap and a global
  concurrency limit. A fault (trap, limit, bad output) discards the instance; three faults in 60 s
  put the node in a red "plugin failed" state until the next deploy.
- Packages are identified by SHA-256 and checked again on every start; a tampered or missing
  file disables only that plugin. Signatures are not implemented: install only packages you
  trust, from people you trust. Plugin output is data — treat it like any other flow input.
- Only administrators can list, stage, activate, roll back or remove plugins.

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

With `enabled = true` the running n2linkd owns the store and serves these routes. All of them
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
and in the Copilot catalog (type, named output ports with advisory payload types from
`output_labels` / `output_payloads` / `input_payload`, and config field names and kinds; never
the description or help). `/status` gains a `wasm` section (`state`, `plugins`, `engineLive`,
`permitsInUse`, `memoryReservedKib`). History (category `plugin`) and the audit log record
`plugin.staged`, `plugin.rejected`, `plugin.activated`, `plugin.rolled_back`, `plugin.removed`,
`plugin.discarded` and `plugin.failed` (three faults) with the id, version, the first 12 hex
digits of the digest and a reason code.

## Installing a plugin (offline CLI)

A package is a `wasm32-unknown-unknown` module with its manifest (TOML, schema 1) embedded as
the `edgelink.manifest` custom section. Plugins built with the Rust guest SDK
(`crates/wasm-guest`, `manifest!`) already contain it; for other modules embed it with `pack`.

Stop n2linkd first: when plugins are enabled the runtime holds `<home>/plugins/.lock` and
every command below refuses to run while it does.

```sh
n2linkd plugin pack echo.wasm plugin.toml -o echo.pkg.wasm
n2linkd plugin stage echo.pkg.wasm            # validate → quarantine → self-test; prints sha256
n2linkd plugin activate acme/echo --sha256 <hex>
n2linkd plugin list                           # active generations and quarantined packages
n2linkd plugin rollback acme/echo --sha256 <previous hex>
n2linkd plugin remove acme/echo               # refused while a flow node uses the type
n2linkd plugin discard <hex>                  # delete a quarantined package
n2linkd plugin verify                         # re-hash active generations; non-zero on problems
```

- `stage` rejects a package for any framing, manifest, import, compile or limit error and keeps
  nothing; a package that validates but fails its `[[selftest]]` stays in quarantine marked
  `rejected` and cannot be activated. At most 8 packages wait in quarantine.
- `activate` and `rollback` first build the flows n2linkd would deploy (`flows.json` plus
  credentials, as if `enabled = true`) with the candidate plugin set. If that graph does not
  build, nothing on disk changes. Nothing is started.
- Each plugin keeps its current and one previous generation; the generation before that is
  deleted. `remove` moves both back to quarantine.
- The commands work whether or not `enabled` is set, so packages can be staged and activated
  before plugins are turned on. Nothing runs until n2linkd starts with `enabled = true`.

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

## Connecting devices

A plugin never opens a port. Put a built-in node in front of it:

| Device link | Built-in node | Plugin input |
|---|---|---|
| UART / RS-485 | expose the port over TCP (`socat`, `ser2net`), then `tcp in` (client, stream of Buffer) or `tcp request` | arrays of byte values |
| MQTT, LoRaWAN network server | `mqtt in` | string, Buffer or JSON |
| Modbus TCP | `modbus` | register values |
| HTTP device APIs | `http request` | body |

Example, a Nooploop TOFSense laser sensor on a USB-to-TTL adapter (full walkthrough in
`crates/wasm-guest/examples/tofsense/README.md`):

```sh
socat TCP-LISTEN:7000,reuseaddr FILE:/dev/ttyUSB0,b921600,raw,echo=0
```

`tcp in` (client `127.0.0.1:7000`, stream, Buffer) → `TOFSense` plugin → your flow.

EdgeLinkd rejects a wire loop at deploy (`Referenced node not found`). For request/response
devices, send with one plugin node and decode the reply with a second one.

## Observing

| Where | What |
|---|---|
| Node status | the plugin's own status; a red ring `plugin failed (3 faults within 60s)` after repeated faults |
| `catch` node | message failures and faults, text prefixed `wasm <publisher>/<name>:` |
| Log target `edgelink::wasm` | guest log lines (50/s per node, drops counted) and `el_close` problems |
| `GET /status` → `wasm` | `state` (`disabled`, `idle`, `active`), `plugins`, `engineLive`, `permitsInUse`, `memoryReservedKib`, `memoryBudgetKib` |
| `GET /history?category=plugin` | lifecycle events (needs `[history] enabled = true`) |
| `GET /audit` | who staged, activated, rolled back, removed or discarded what |

## Troubleshooting

| Message | Cause and fix |
|---|---|
| `… not compiled in this build (requires nodes_wasm)` | the binary was built without the feature; rebuild with `--features nodes_wasm` |
| `… disabled by configuration ([runtime.wasm] enabled = false)` | set `enabled = true` and restart |
| `… requires WASM plugin acme/x which is not active` | stage and activate the plugin, or check `GET /wasm/plugins` / startup log for a disabled generation |
| `… requires acme/x@2, active is acme/x@1.4.0` | the node was configured for another major version; activate a matching version or update the node |
| `property 'p' is not declared by WASM plugin …` / `must be within …` / `is required` | fix the node's settings in the editor (or the plugin's manifest) |
| `WASM memory budget exceeded …` | raise `memory_budget_kib`, lower the plugin's `memory_pages`, or use fewer plugin nodes |
| `module starts with N pages of linear memory but … may use M` | the package needs `[limits] memory_pages = N` (or a smaller stack, see the developer manual) |
| `import … is not granted` | the module uses WASI or another host API; it cannot run here |
| `fuel budget … exhausted` / `deadline exceeded` | the plugin is too slow for one message; raise its `[limits]` within the ceilings or fix the plugin |
| `concurrency limit ([runtime.wasm] max_concurrent)` | other plugin calls held every permit for this plugin's whole deadline; raise `max_concurrent` (up to the core count) |
| `plugin store … is in use by another n2linkd process` | the CLI needs the runtime stopped; use the admin API instead |
| `409 plugins_disabled` from `/wasm/plugins` | the runtime runs with `enabled = false` |
| `409 invalid_flows` on activate | the deployed flows do not build with that version; nothing changed |

## Limitations

- Rust is the only guest SDK (`crates/wasm-guest`; examples `uppercase`, `csvparse`,
  `tofsense`). No component model or other guest languages.
- The editor learns about new plugin types only on reload; there is no live palette push.
- Signatures (`require_signature`) are not implemented.
- Measured on a Raspberry Pi 5 (arm64) and x86-64; 32-bit ARM boards are build-only so far.

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
