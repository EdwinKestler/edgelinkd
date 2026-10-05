# Phase 5 Design: Typed Registry Metadata for Flow Copilot

Status: closed locally  
Schema: `NodeMetadata` version **1**  
Depends on: phases 0–4 on `master` (`7c34b8e` and earlier)

## Problem

Flow Copilot currently receives a comma-separated list of **flow** node type names and
`materialize_draft` only checks registration, reserved keys, a global output cap of 15, and
workspace membership. It cannot tell that `inject` has no input, `debug` has no output, or
`mqtt in` needs an existing `mqtt-broker`. Payload typing is not a Node-RED invariant and
must stay advisory.

## Source of truth

The live `inventory` registry remains the only node catalog.

- **Ports** (`inputs`, `outputs`, `dynamic_outputs`) live on `MetaNode` and are emitted by
  `#[flow_node]` / `#[global_node]`. Defaults: flow nodes `1/1`, global nodes `0/0`.
  Overrides are attributes on the same registration that creates the type.
- **Hints** (config-node references, secret **field names**, capabilities) are submitted
  beside the node via `node_hints!` in the node's source file. Missing hints mean no
  required config refs and no secret labels.
- There is no second handwritten type list. Tests fail if a registered type has invalid
  ports, if HTML `registerType` names are absent from the registry, or if a known
  0-input/0-output/config-ref node still uses the 1/1 default.

`Engine::prepare_flows` still validates the complete candidate graph. Metadata never
replaces that step and never writes `flows.json`.

## Schema version 1

```text
schemaVersion: 1
type, kind (flow | global), module, redId, feature (optional)
inputs: u8
outputs: u8
dynamicOutputs: bool          # switch-style; ports 0..=15 still bounded
configRefs: [{ property, type }]
secretFields: [name]          # names only
capabilities: [network | filesystem | ai | process]
```

### Authoritative (draft rejected)

| Check | Rule |
|---|---|
| Type | Must be in this build's registry |
| Kind | Draft `nodes` may only add **flow** types |
| Output port | `output < outputs`, unless `dynamicOutputs` (then `output <= 15`) |
| Config ref | Required property must name an existing node of that type, or there is exactly one such node in the editor graph (then Copilot fills the id) |
| Reserved keys | `id`, `type`, `z`, `x`, `y`, `wires`, `credentials` |
| Secrets | `config` must not contain secret field names or a `credentials` object |
| Disabled feature | Type absent from registry → same as unknown |

### Advisory (prompt only)

- Capabilities
- Typical `msg` properties
- Named ports
- Workspace list of existing config nodes (`id`, `type`, `name`) **without** credentials

Dynamic `msg.payload` types are never rejected by metadata.

## Copilot contract

1. `GET /assistant/catalog` (Copilot class, authenticated like `/assistant/draft`) returns
   schema v1 plus workspace-independent type metadata.
2. `POST /assistant/draft` still redacts the editor flow, then appends compact type
   metadata and existing config-node ids to the system prompt.
3. `materialize_draft` applies authoritative checks. When `copilot.strict_metadata` is
   `false`, only type, reserved keys, and the previous global port cap apply (type-only
   fallback).
4. User preview and revision-protected `POST /flows` remain the only deploy path.

## Configuration

```toml
[copilot]
strict_metadata = true
```

Unknown keys fail startup. Default is `true` when the table is absent.

## Rollback

- Set `strict_metadata = false` and restart. No flow conversion.
- Older binaries ignore `[copilot]` if they lack the table and `deny_unknown_fields` is
  not applied to the whole file; new keys live under `[copilot]` only.
- Runtime wire semantics are unchanged.

## Non-goals

New AI nodes, WASM, auto-deploy, React Flow, and a static type system for runtime
messages are out of scope (phases 6–7).

## Tests

- Every registry type produces schema-valid metadata.
- Default vs featured builds expose exactly live types.
- Reject unknown type, global type in a draft, overflow output, missing/ambiguous config
  ref, reserved keys, secret fields in `config`.
- Auto-fill a unique existing `mqtt-broker` / `ai-provider`.
- Do not reject valid dynamic payloads.
- Golden timestamp → MQTT + CSV draft still `prepare_flows`.
- Type-only fallback still materializes the golden draft.
- `/nodes` HTML `registerType` ⊆ JSON catalog (existing test).
