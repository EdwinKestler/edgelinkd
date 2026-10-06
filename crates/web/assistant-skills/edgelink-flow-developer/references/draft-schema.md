# Flow draft schema

Return exactly one JSON object with this shape:

```json
{
  "version": 1,
  "summary": "Short description of the proposed change",
  "assumptions": ["Defaults selected because the prompt omitted them"],
  "warnings": ["Missing configuration or unsupported behavior"],
  "nodes": [
    {
      "ref": "unique_symbolic_name",
      "type": "registered node type",
      "name": "short editor label",
      "x": 320,
      "y": 240,
      "config": {}
    }
  ],
  "wires": [
    {"from": "source_ref", "output": 0, "to": "target_ref_or_existing_node_id"}
  ]
}
```

Constraints:

- `version` is `1`.
- `summary` is required and must describe the draft, not claim it was applied.
- `nodes` contains at most 64 new flow nodes.
- `ref` is unique and uses only letters, digits, `_`, and `-`.
- `type` must exactly match a supplied runtime-catalog type.
- `config` contains ordinary Node-RED node properties. It must not contain `id`, `type`, `z`, `x`, `y`, `wires`, or `credentials`.
- `wires` contains at most 128 connections. `from` must name a new-node `ref`. `output` is a zero-based output port and must be less than that type's `outputs` (unless `dynamicOutputs`). `to` is a new-node `ref` or an existing node ID from the active workspace.
- Choose `output` from the catalog's `outputPorts`: use the `index` of the port whose `name` matches the branch you want (for `exec`, `stderr` is output 1). A port with `"repeats": true` (`switch` rules, `function` outputs) describes every output of that node; `{n}` in its name is the 1-based output number, so `rule {n}` output 0 is the first rule.
- `inputPayload` and each port's `payload` are advisory `msg.payload` types for the node's default configuration (`any`, `string`, `number`, `boolean`, `object`, `array`, `buffer`, `null`, joined with `|`). `buffer` appears in messages as an array of byte values. When an output's payload type does not match the next node's `inputPayload`, add a converter such as `json`, `csv`, or `change`, or state the assumption. Payload types are never enforced.
- Required `configRefs` (for example `broker` → `mqtt-broker`) must reuse an existing configuration node id from the live catalog. Do not invent credentials or new config nodes.
- Omit prose, Markdown fences, comments, and trailing text outside the JSON object.
