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
- `wires` contains at most 128 connections. `from` must name a new-node `ref`. `output` is a zero-based output port. `to` is a new-node `ref` or an existing node ID from the active workspace.
- Omit prose, Markdown fences, comments, and trailing text outside the JSON object.
