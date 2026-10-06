# Phase 6 live acceptance

CI never runs these. Set keys in the environment; do not commit them.

```bash
export EDGELINK_AI_LIVE=1
export EDGELINK_OPENAI_API_KEY=...
export EDGELINK_XAI_API_KEY=...
export EDGELINK_ANTHROPIC_API_KEY=...
```

## Embeddings

Requires OpenAI **and** xAI before README marks `ai-embed` production-supported.

```bash
EDGELINK_AI_LIVE=1 cargo test -p edgelink-core --features nodes_ai_embeddings --lib embed_live -- --nocapture
```

Use an embedding model id from the provider. Do not use a chat `defaultModel`. List xAI models with a live `GET /v1/embedding-models` and record the id here after a successful run. Do not hardcode a SKU that 404s.

## Agent

Opt-in binary: `--features nodes_ai_agent`. Cap: two concurrent loops per process.

```bash
EDGELINK_AI_LIVE=1 cargo test -p edgelink-core --features nodes_ai_agent --lib agent_live -- --nocapture
```

Requires a live OpenAI **and** Anthropic run before considering `nodes_ai_agent` for app default (not in 0.3.x).
Update 2026-10-06: the owner moved `nodes_ai_agent` into the default build before that run; `ai-agent` stays experimental until it passes.
