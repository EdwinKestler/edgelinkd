# n2link-wasm-guest

Guest SDK for EdgeLinkd WASM plugins (ABI `n2link:node/v1`). Experimental; the host is the
optional `nodes_wasm` feature. Full author manual (manifest schema, ABI, encoding, versioning):
`docs/development/wasm-plugins.md`. Operator manual: `docs/operations/wasm-plugins.md`.

## A plugin in four files

```text
my-plugin/
  Cargo.toml          crate-type = ["cdylib"]; depends on n2link-wasm-guest
  .cargo/config.toml  -C link-arg=-zstack-size=65536 for wasm32-unknown-unknown
  plugin.toml         manifest schema 1 (identity, node, config fields, self-tests)
  src/lib.rs          impl Node + export_node! + manifest!
```

```rust
use n2link_wasm_guest::{export_node, manifest, Ctx, EveValue, Msg, Node};

manifest!("../plugin.toml");

struct Upper;

impl Node for Upper {
    fn init(_config: &Msg) -> Result<Self, String> {
        Ok(Upper)
    }

    fn on_input(&mut self, ctx: &mut Ctx, mut msg: Msg) -> Result<(), String> {
        let Some(EveValue::String(text)) = msg.payload() else {
            return Err("payload must be a string".to_string());
        };
        let upper = text.to_uppercase();
        msg.set_payload(EveValue::String(upper));
        ctx.emit(0, &msg)
    }
}

export_node!(Upper);
```

```sh
rustup target add wasm32-unknown-unknown
cargo build --release --target wasm32-unknown-unknown
n2linkd plugin stage target/wasm32-unknown-unknown/release/my_plugin.wasm   # or POST /wasm/plugins/stage
```

The `.wasm` is the package: `manifest!` embeds `plugin.toml` as the `n2link.manifest` custom
section, so no `n2linkd plugin pack` step and no wasm-bindgen or wasm-pack are involved.

## What a plugin gets

- `Node::init(config)` receives the node's resolved `[[node.config]]` values; `Err` rejects
  them (the message fails and counts as a fault).
- `Node::on_input(ctx, msg)` gets the whole message body, including `_msgid`. Emit zero or more
  messages with `ctx.emit(port, &msg)`; they reach the flow only if `on_input` returns `Ok`.
  `Err(text)` fails the message with that text (catchable with a `catch` node).
- `ctx.log(level, text)`, `ctx.status(fill, shape, text)`.
- `Node::close()` when the node stops.

Nothing else: no clock, randomness, filesystem, network, environment or credentials. A Rust
standard-library call that needs one of those traps on `wasm32-unknown-unknown` (and counts as a
fault). Do not change `_msgid`.

## Limits

The SDK checks the host bounds first and returns `Err` instead of letting the host fault the
call: one output ≤ 64 KiB, ≤ 16 outputs and ≤ 256 KiB per message; log lines ≤ 512 bytes (16 per
message; more are dropped); status ≤ 128 bytes; failure text ≤ 1 KiB. Fuel, deadline and memory
come from the manifest's `[limits]` within the operator's ceilings. With the 64 KiB stack these
examples start in 2–3 pages, well inside the 512 KiB default; without it a Rust module starts at
17+ pages and `stage` rejects it unless `[limits] memory_pages` asks for more.

## Testing

Off `wasm32` the host imports do not exist and `Ctx` records outputs, logs and status in
`ctx.record`, so plugin logic is tested with a plain `cargo test`. See `examples/uppercase` and
`examples/csvparse` (configurable delimiter and header row) and `examples/tofsense` (a UART
laser-ranging sensor protocol: stream reassembly, checksums, two outputs, query frames); `scripts/wasm-examples.sh --e2e`
builds both, stages them (manifest + self-tests) and runs them in a flow.
