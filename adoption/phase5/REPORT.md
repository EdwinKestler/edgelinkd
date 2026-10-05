# Phase 5 Report: Typed Registry Metadata for Flow Copilot

Schema version: **1** (`NODE_METADATA_VERSION`).

## Authoritative vs advisory

Authoritative: registered type, flow vs global kind, input/output cardinality,
`dynamicOutputs`, required config-node refs, reserved properties, secret **field names**.

Advisory: capabilities, workspace config-node list (`id`/`type`/`name` only). Dynamic
`msg` types are never rejected by metadata.

## Files

- `adoption/phase5/DESIGN.md`, this report
- `crates/macro/src/lib.rs`: `inputs`, `outputs`, `dynamic_outputs` on node macros
- `crates/core/src/runtime/nodes/mod.rs`: `NodePorts`, `NodeHints`, `node_hints!`
- Per-node port overrides and hints (inject, debug, mqtt, ai, file, exec, …)
- `registry.rs`: hint index and metadata validity test
- `assistant.rs`: `GET /assistant/catalog`, strict `materialize_draft`, config-ref fill
- `web_state.rs` / `server.rs` / `src/defaults.rs`: `[copilot] strict_metadata`

## Tests (this machine)

- `cargo fmt --check`: passed
- `cargo clippy --all-features --tests --all -- -D warnings`: passed
- `cargo test -p edgelink-core --all-features --lib`: 301 passed, 1 ignored
- `cargo test -p edgelink-web --all-features --lib`: 79 passed
- Golden timestamp → MQTT + CSV still `prepare_flows`
- Unique existing `mqtt-broker` is filled; missing broker is rejected; `strict_metadata = false` skips the fill

## Rollback

`[copilot] strict_metadata = false` restores type-only draft checks. No flow conversion.

## Open coverage

Default 1-in/1-out remains for ordinary transform nodes. Named ports and advisory payload
types are not yet populated. Switch uses `dynamic_outputs`.

## Git

This bundle is the Phase 5 commit. Version remains `0.3.0`. No tag or release unless
separately approved.
