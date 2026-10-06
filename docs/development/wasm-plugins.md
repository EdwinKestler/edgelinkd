# Writing WASM plugins

A WASM plugin is a third-party flow node compiled to `wasm32-unknown-unknown`, installed into a
running EdgeLinkd without rebuilding it, and run in the Wasmi sandbox. This manual is for plugin
authors. Installing and operating plugins is in
[`docs/operations/wasm-plugins.md`](../operations/wasm-plugins.md); the design record is in
[`adoption/phase7/`](../../adoption/phase7/).

Status: experimental. The host is compiled only with `--features nodes_wasm` and runs plugins
only with `[runtime.wasm] enabled = true`.

## 1. What a plugin can and cannot do

A plugin receives a message, computes, and emits zero or more messages. It can log and set the
node status. That is all ABI v1 grants.

| Can | Cannot |
|---|---|
| Transform, validate, filter, aggregate messages | Open files, sockets, serial ports or devices |
| Keep state between messages (one instance per node) | Read the clock, randomness or environment |
| Read its node configuration (`[[node.config]]`) | See credentials, context stores or other nodes |
| Emit on several outputs, log, set status, fail a message | Deploy flows, start timers, run without a message |

Good fits: protocol decoders (sensor frames, LoRaWAN payloads, Modbus register maps), parsers,
unit conversion, filters and thresholds, checksums, small DSP, proprietary algorithms shipped
without source. Anything that needs I/O is split in two: a built-in node moves the bytes
(`tcp in`, `tcp request`, `mqtt in`, `http request`, `modbus`, …) and the plugin does the logic.
`examples/tofsense` is a complete case: a UART laser-ranging sensor exposed over TCP, read by
`tcp in`, decoded by the plugin.

## 2. Quick start (Rust SDK)

```text
my-plugin/
  Cargo.toml          [lib] crate-type = ["cdylib", "rlib"]; n2link-wasm-guest dependency
  .cargo/config.toml  64 KiB stack for wasm32-unknown-unknown
  plugin.toml         the manifest (section 3)
  src/lib.rs          impl Node, export_node!, manifest!
```

`Cargo.toml`:

```toml
[package]
name = "my-plugin"
version = "1.0.0"
edition = "2021"

[lib]
crate-type = ["cdylib", "rlib"]

[dependencies]
n2link-wasm-guest = { path = "../edgelinkd/crates/wasm-guest" }

[profile.release]
opt-level = "z"
lto = true
codegen-units = 1
panic = "abort"
```

`.cargo/config.toml`:

```toml
[target.wasm32-unknown-unknown]
rustflags = ["-C", "link-arg=-zstack-size=65536"]
```

`src/lib.rs`:

```rust
use n2link_wasm_guest::{export_node, manifest, Ctx, EveValue, Fill, Msg, Node, Shape};

manifest!("../plugin.toml");

struct Scale {
    factor: f64,
}

impl Node for Scale {
    fn init(config: &Msg) -> Result<Self, String> {
        Ok(Self { factor: config.get_f64("factor").unwrap_or(1.0) })
    }

    fn on_input(&mut self, ctx: &mut Ctx, mut msg: Msg) -> Result<(), String> {
        let value = msg.get_f64("payload").ok_or("payload must be a number")?;
        msg.set_payload(EveValue::F64(value * self.factor));
        ctx.status(Fill::Green, Shape::Dot, &format!("{:.2}", value * self.factor));
        ctx.emit(0, &msg)
    }
}

export_node!(Scale);
```

Build, test and install:

```sh
rustup target add wasm32-unknown-unknown
cargo test                                               # host-side logic tests
cargo build --release --target wasm32-unknown-unknown
edgelinkd plugin stage target/wasm32-unknown-unknown/release/my_plugin.wasm   # runtime stopped
edgelinkd plugin activate acme/scale --sha256 <hex from stage>
```

or, with the runtime running, `POST /wasm/plugins/stage` (`Content-Type: application/wasm`) and
`POST /wasm/plugins/acme/scale/activate`. Reload the editor; the node appears in the palette.

### SDK reference

| Item | Purpose |
|---|---|
| `trait Node { fn init(&Msg) -> Result<Self, String>; fn on_input(&mut self, &mut Ctx, Msg) -> Result<(), String>; fn close(&mut self) {} }` | the node |
| `Msg` | an object: `get`, `set` (keeps key order), `remove`, `payload`, `set_payload`, `get_str`, `get_i64`, `get_f64`, `get_bool`, `fields`, `from_eve`, `to_eve` |
| `EveValue` | `Null`, `Bool`, `I64`, `U64`, `F64`, `String`, `Bytes`, `Array`, `Object`, `Date`, `Regexp` |
| `Ctx::emit(port, &msg)` | queue an output; returns `Err` before the host would fault (size, count) |
| `Ctx::log(Level, text)`, `Ctx::status(Fill, Shape, text)` | logging (16 lines/message, 512 B each) and node status (128 B) |
| `export_node!(Type)` | generates the ABI exports; nothing off `wasm32` |
| `manifest!("../plugin.toml")` | embeds the manifest as the `edgelink.manifest` custom section |
| `ctx.record` (off `wasm32`) | outputs, logs and status recorded for unit tests |

## 3. Manifest schema 1 (`plugin.toml`)

Every table rejects unknown keys. The node type is derived, never declared:
`wasm-<publisher>-<name>`.

### `[plugin]`

| Key | Rule |
|---|---|
| `id` | `<publisher>/<name>`, each `[a-z][a-z0-9]{0,31}` (no dashes) |
| `version` | semver, no build metadata; the major version is the editor pin (`wasmPlugin = "id@1"`) |
| `abi` | `1` |
| `license` | 1–64 printable ASCII characters; shown to administrators |
| `description` | optional, ≤ 256 bytes, no control characters |
| `capabilities` | must be empty in ABI 1 |

### `[limits]` (optional)

| Key | Default and ceiling (operator settings) |
|---|---|
| `memory_pages` | `default_memory_pages` (8 = 512 KiB) up to `max_memory_pages` (256) |
| `fuel_per_message` | `default_fuel` (2·10⁷ ≈ 42 ms of guest work on a Pi 5) up to `max_fuel` |
| `deadline_ms` | `default_deadline_ms` (250) up to `max_deadline_ms` (5000) |

A request above a ceiling fails `stage`. A module whose initial memory exceeds `memory_pages`
fails `stage` with the remedy.

### `[node]`

| Key | Rule |
|---|---|
| `label` | 1–64 bytes; palette label |
| `category` | `[a-z][a-z0-9 _-]{0,31}` |
| `color` | `#RRGGBB` |
| `icon` | a Node-RED icon file name such as `function.svg` |
| `inputs` | `1` (ABI 1 has no trigger for input-less nodes) |
| `outputs` | 0–16 |
| `output_labels` | empty or one label per output, ≤ 64 bytes each; the editor and Flow Copilot use them as port names |
| `output_payloads` | empty or one advisory `msg.payload` type per output for Flow Copilot: `any`, `string`, `number`, `boolean`, `object`, `array`, `buffer`, `null`, or several joined with `\|` (`"string\|buffer"`); empty means `any` |
| `input_payload` | optional advisory payload type the node expects, same vocabulary; absent means `any` |
| `help` | ≤ 4 KiB, shown escaped as plain text in the editor help |

### `[[node.config]]` (≤ 32)

| Key | Rule |
|---|---|
| `name` | `[a-z][a-zA-Z0-9]{0,31}`, unique, not `id type z g x y l d name wires info credentials wasmPlugin` |
| `kind` | `string`, `number`, `boolean`, `enum` |
| `label` | optional, ≤ 64 bytes |
| `required` | default `false`; with `[[selftest]]`, a required field needs a `default` |
| `default` | must satisfy the field's own rules |
| `max_len` | `string` only, 1–4096 (default 4096) |
| `min`, `max`, `integer` | `number` only |
| `values` | `enum` only, 1–32 unique strings |

At deploy each node's value (or the default) is validated and normalised (numeric strings from
the editor become numbers; an empty string counts as unset for non-string kinds) and the whole
object is passed to `Node::init`. Undeclared properties, wrong kinds, out-of-range values and
missing required values fail the deploy, naming the property.

### `[[selftest]]` (≤ 4)

```toml
[[selftest]]
input = { payload = "abc" }   # a TOML table = the message body
expect_outputs = [1]          # messages expected on each output
```

`stage` runs every vector with the default configuration under the plugin's limits. A failed
vector leaves the package in quarantine marked `rejected`; it cannot be activated. TOML has no
byte strings, so test byte-oriented plugins through another input (the TOFSense example tests
its query path).

## 4. Messages

The whole message body crosses the boundary as one EVE/1 object, including `_msgid`.

| Flow value | `EveValue` | Note |
|---|---|---|
| `null`, boolean, string | `Null`, `Bool`, `String` | |
| number | `I64`, `U64` (> `i64::MAX`) or `F64` | NaN and ±∞ cannot be sent |
| Buffer | `Bytes` | `tcp in`/`tcp request` emit **arrays of byte values** instead; accept both |
| array, object | `Array`, `Object` | object keys unique, order kept |
| Date | `Date` | milliseconds since 1970, negative allowed |
| RegExp | `Regexp` | source only |

Rules on output: every output is an object; a missing `_msgid` is copied from the input; a
**changed** `_msgid` is a fault. `link call` return information is kept by the host.

## 5. ABI v1 reference

For authors not using the SDK (WAT, C, Zig, …). Import module `edgelink:node/v1`; any other import
fails `stage`.

| Import | Signature | Effect and bounds |
|---|---|---|
| `emit` | `(port i32, ptr i32, len i32) -> i32` | queue an EVE/1 object on `port` (< outputs); ≤ 64 KiB, ≤ 16 per message, ≤ 256 KiB total |
| `log` | `(level i32, ptr i32, len i32) -> i32` | level 0 debug, 1 info, 2 warn, 3 error; ≤ 512 B; ≤ 16 per message |
| `status` | `(fill i32, shape i32, ptr i32, len i32) -> i32` | fill 0 red, 1 green, 2 yellow, 3 blue, 4 grey; shape 0 ring, 1 dot; ≤ 128 B |
| `fail` | `(ptr i32, len i32) -> i32` | failure text for this message, ≤ 1 KiB |

| Export | Signature | Required |
|---|---|---|
| `memory` | linear memory (not imported) | yes |
| `el_abi_version` | `() -> i32`, returns 1 | yes |
| `el_alloc` | `(len i32) -> i32`, a buffer the host writes the input into | yes |
| `el_on_input` | `(ptr i32, len i32) -> i32` | yes |
| `el_init` | `(ptr i32, len i32) -> i32`, the configuration object | if `[[node.config]]` is declared |
| `el_close` | `() -> ()` | no |

Call sequence per node: compile once per package → instantiate on the first message →
`el_abi_version` → `el_alloc` + `el_init(config)` → for each message `el_alloc` +
`el_on_input` → `el_close` when the node stops with an idle instance. Return 0 for success.

| Outcome | Effect |
|---|---|
| return 0 | queued outputs are delivered in emit order; logs and status applied |
| non-zero return or `fail` | message error (catchable with `catch`); outputs discarded; instance kept |
| trap, fuel or deadline exhausted, host bound exceeded, bad port, changed `_msgid` | fault: instance discarded and recreated (and `el_init` rerun) on the next message; 3 faults in 60 s put the node in a failed state until redeploy |
| `el_init` returns non-zero or fails | fault |

Rejected at `stage`: start functions, imported memories/tables/globals, mistyped exports or
imports, SIMD, threads, memory64, and any import outside the four above. Floats are allowed.

Without the SDK, embed the manifest with `edgelinkd plugin pack module.wasm plugin.toml -o
package.wasm`. A minimal WAT plugin is `crates/core/src/runtime/wasm/fixtures/identity.wat`.

### EVE/1 encoding

`0xEE 0x01`, then one value; little-endian; lengths and counts are `u32`.

| Tag | Value | Payload |
|---:|---|---|
| `0x00` / `0x01` / `0x02` | null / false / true | — |
| `0x03` / `0x04` / `0x05` | i64 / u64 / f64 (finite) | 8 bytes |
| `0x06` / `0x07` / `0x0B` | string / bytes / regexp source | length + bytes |
| `0x08` | array | count + values |
| `0x09` | object | count + (key length + UTF-8 key, value) |
| `0x0A` | date | i64 milliseconds |

The host decodes with depth ≤ 32, ≤ 65,536 values and the `max_input_kib` size cap.

## 6. Testing

1. **Logic on the host:** off `wasm32`, `Ctx` records instead of calling the host, so `cargo test`
   covers the plugin without a runtime (see every example's tests).
2. **Device simulators:** test the decoder against the manufacturer's sample frames, and run a
   fake device for the flow (`examples/tofsense/tools/fake_tofsense.py`).
3. **Install path:** `stage` validates the module and runs the self-tests;
   `scripts/wasm-examples.sh --e2e` builds the in-repo examples, stages them and runs a flow.

## 7. Versioning

- Bump the **patch/minor** version for compatible changes; editors keep their
  `wasmPlugin = "<id>@<major>"` pin and deployed nodes keep working.
- Bump the **major** version when configuration fields, outputs or behaviour change
  incompatibly. Nodes pinned to the old major then fail deploy with a message naming both
  versions instead of running with the wrong assumptions.
- Activation keeps one previous generation; `rollback` swaps back in one call.
- ABI 1 is the only ABI. A future ABI 2 host will keep accepting ABI 1 packages.

## 8. Gotchas

- Link with a 64 KiB stack (or request `[limits] memory_pages`): Rust's default 1 MiB stack does
  not fit the 512 KiB default cap, and `stage` rejects the module.
- `std::time`, randomness and file APIs trap on `wasm32-unknown-unknown`; a trap is a fault.
- A panic aborts the call (fault). Return `Err` for bad input instead.
- One instance serves one node, one message at a time. Plugins share `max_concurrent` permits.
- Do not wire a node's output back into its own input: EdgeLinkd rejects wire loops at deploy.
  Use a second node (as the TOFSense query example does).
- After activating a new version, reload the editor to see palette changes.

## Examples

| Example | Shows |
|---|---|
| [`uppercase`](../../crates/wasm-guest/examples/uppercase) | the smallest useful node, status |
| [`csvparse`](../../crates/wasm-guest/examples/csvparse) | configuration, errors, arrays of objects |
| [`tofsense`](../../crates/wasm-guest/examples/tofsense) | binary protocol, stream reassembly, checksums, two outputs, a device simulator |
