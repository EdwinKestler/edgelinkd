# Known Issues

Known problems and limitations of n2link with their workarounds. n2link is alpha
software; deliberate scope decisions (for example, Node-RED features we do not offer) are in the
root [README](../README.md) and the spec coverage report
([`tests/REDNODES-SPECS-DIFF.md`](../tests/REDNODES-SPECS-DIFF.md)), not here. Measured evidence
and phase boundaries are under [`adoption/`](../adoption/).

Report new issues on GitHub: <https://github.com/n2link/n2link/issues>.

## Runtime and flows

### A wire loop fails deploy with "Referenced node not found"

**Symptom.** Deploying a flow where a node's output is wired, directly or through other nodes,
back to its own input fails with `Referenced node not found [this_node.id=… referenced_node.id=…]`.
Node-RED accepts such flows.

**Workaround.** Break the loop. For request/response patterns (send a query, decode the reply)
use two nodes: one that builds the request and one that handles the reply. The TOFSense plugin
example does exactly this for its query mode.

### `n2linkd run` ignores `[ui-host]` in the configuration

**Symptom.** The editor always listens on `127.0.0.1:1888` with `n2linkd run`, whatever
`ui-host` says in `n2linkd.toml`.

**Cause.** The `run` subcommand's `--bind` option has a default value, and it takes precedence
over the configuration file. Plain `n2linkd` (no subcommand) uses `ui-host`.

**Workaround.** Pass the address explicitly: `n2linkd run --bind 0.0.0.0:1888`.

### MQTT keeps its resolved broker address until redeploy

**Symptom.** After the broker's DNS name points to a new address, the `mqtt-broker` connection
keeps reconnecting to the old one.

**Cause.** With the egress policy, the broker's addresses are checked once and the approved
address is kept for the session and its reconnect loop.

**Workaround.** Redeploy (or restart) after a broker DNS change.

## Builds and platforms

### `--no-default-features` is not a minimal build

`cargo build --no-default-features` drops the app's optional features (AI nodes, history,
PostgreSQL, Redis, bcrypt admin passwords), but `n2link-web` depends on `n2link-core` with
core's own defaults, so the JavaScript engine, JSONata and the network, storage and parser nodes
stay in. Do not use it as evidence that one of those was removed. A truly minimal build needs a
feature-propagation change that has not been made.

### Windows and 32-bit ARM are compile-checked, not runtime-verified

Encrypted credentials (atomic replace, permissions, advisory locks, crash recovery), resource
measurements and the WASM plugin host have run on Linux x86-64 and on a Raspberry Pi 5 (arm64).
Windows msvc and gnu tests pass in GitHub Actions. QEMU ARM CI (armv7 and aarch64-on-x86) is
deferred; restore `build-and-test-on-arm-linux` in `.github/workflows/CICD.yml` when picking
that up. Report anything that behaves differently on those hosts.

## WASM plugins (`nodes_wasm`, experimental)

See the [operator manual](operations/wasm-plugins.md) for setup and its troubleshooting table.

- **The editor only shows new or changed plugin types after a reload.** There is no live
  palette update; the activation response says `editorReloadRequired: true`.
- **A publisher named `quarantine` cannot be removed through the admin API**, because the
  `DELETE /wasm/plugins/quarantine/{sha256}` route matches first. Use
  `n2linkd plugin remove quarantine/<name>` with the runtime stopped.
- **A crash between an online activation and its redeploy can leave startup failing** if the
  newly activated generation does not build with the deployed flows. Startup reports the plugin
  and stops. Recover with `n2linkd plugin rollback <id> --sha256 <previous>` while stopped.
- **The engine-restart fallback uses the startup plugin set.** It runs only when the web server
  has no engine, which the normal `run` path never does; after an online activation, restart the
  process instead of relying on it.
- **Rust plugins need a small stack.** Rust's default 1 MiB stack does not fit the default
  512 KiB plugin memory cap; build with `-C link-arg=-zstack-size=65536` or request
  `[limits] memory_pages` (see the [developer manual](development/wasm-plugins.md)).
- **Package signatures are not implemented.** `require_signature = true` fails startup; install
  only packages you trust.

## Flow Copilot

- Drafts can add and connect nodes only; update, delete and install tools are not implemented.
- Port payload types are advisory and describe a node's default configuration; they are never
  enforced. Nodes that declare no ports (most single-output nodes) appear in the catalog as
  `output 1` with payload `any`. Every node with more than one output, or a configurable number of
  outputs, names its ports, and a registry test keeps it that way.
- Live provider acceptance (OpenAI, Anthropic, xAI, Snowflake Cortex) is not part of the
  automated tests; the tests use a deterministic local provider. This includes `ai-agent`, which
  is in the default build but has not had its live OpenAI and Anthropic run
  (`adoption/phase6/LIVE.md`).
