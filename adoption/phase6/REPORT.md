# Phase 6 Report: Selective AI Nodes

Depends on: phases 0–5 (`43b3dbf`). Version remains `0.3.0`.

## Families

| Feature | Nodes | App default | Notes |
|---|---|---|---|
| `nodes_ai` | `ai-provider`, `ai-chat` | on | Unchanged contract plus Responses `"store": false` |
| `nodes_ai_text` | `ai-split`, `ai-structured` | on | Local, no `reqwest` |
| `nodes_ai_embeddings` | `ai-embed` | on in binary | README experimental until live OpenAI and xAI |
| `nodes_ai_agent` | `ai-agent` | **off** (on since 2026-10-06, see below) | `--features nodes_ai_agent`; two concurrent loops |

## Provider matrix

| Kind | Chat | Embed | Agent tools |
|---|---|---|---|
| OpenAI | yes | yes | yes |
| xAI | yes | yes | yes |
| Anthropic | yes | NotSupported | yes |
| Cortex | yes | NotSupported | NotSupported |

## Tests

- Unit: splitter table, schema subset, owned-type loud fail, `nodered-foo` still unknown.
- Adapter: Responses `store: false`; embeddings and tool-chat under their features.
- Pytest: `tests/nodes/function/test_ai_split_node.py` (EdgeLinkd-specific, not Node-RED mocha).
- CI: `cargo check -p edgelink-core --no-default-features --features core,nodes_ai_text`; extra `cargo test -p edgelink-core --all-features --lib` on Linux/Windows jobs.

Live procedures: `adoption/phase6/LIVE.md`. Not run in this close-out.

## Rollback

- `--no-default-features` drops the families.
- A graph with `ai-agent` on a binary without `nodes_ai_agent` fails deploy: `node type 'ai-agent' is not compiled in this build`.
- `ai-provider` credentials stay in the sidecar.

## Resource

- Text family: no new crates.
- Embeddings/agent: reuse `reqwest` already in `nodes_ai`.
- Agent semaphore: 2 permits on `Engine`, `cfg(nodes_ai_agent)` only.

## Git

This bundle is the Phase 6 commit. Version remains `0.3.0`. No tag or release unless
separately approved.

## Update 2026-10-06: `nodes_ai_agent` in the default build

At the owner's request `nodes_ai_agent` joined the app's default features (and therefore `full`).
The precondition in `LIVE.md` (a live OpenAI and Anthropic agent run) has not been met, so the
README keeps `ai-agent` experimental. Nothing else changes: two concurrent loops per process,
the memory-context requirement, and deploy-time validation. `--no-default-features` (plus the
features you still need) builds without it. `nodes_ai_agent` now also enables `nodes_ai_text`
(the agent uses its JSON Schema subset, `CompiledSchema`); before, an agent-only build
did not compile. CI checks `core,nodes_ai_agent`.

