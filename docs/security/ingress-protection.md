# Inbound API and Webhook Protection

EdgeLinkd applies finite budgets to the editor API, authentication, WebSockets, Flow Copilot,
fleet operations, health checks, static assets, and each `http in` listener. Limits are grouped by
endpoint class so an administrator can relax one compatibility boundary without disabling the
others.

## Route classes

| Class | Routes or listener |
|---|---|
| `health` | `/api/health`, `/api/info` |
| `editor_admin` | Flow, credential, settings, library, palette, context, audit, runtime-config, and registered node routes |
| `authentication` | `/auth/*` |
| `websocket` | `/comms`, including its complete upgraded lifetime |
| `webhook` | Standalone listeners owned by `http in` nodes |
| `copilot` | `/assistant/*` |
| `fleet` | `/fleet/*` |
| `plugins` | `/wasm/*` (WASM plugin admin API; only routed with `nodes_wasm`) |
| `static_assets` | Editor HTML, JavaScript, CSS, icons, locales, and debug-view assets |

Authentication and authorization run before an API request body is collected or a handler is
started. `http in` matches its configured method and absolute path exactly. It no longer treats a
substring or child path as the configured endpoint.

## Configuration

New installations contain a complete `[api_protection]` section. Existing installations that do
not have the section use the same compiled defaults. Every class accepts:

- `mode`: `enforce`, `observe`, or `off`;
- `max_body_bytes`, `max_header_bytes`, and `max_headers`;
- `requests_per_minute` per authenticated principal and client address where available;
- `max_concurrency` and `queue_timeout_ms`;
- `request_timeout_ms`; and
- `max_response_bytes`.

`global_max_concurrency` bounds work across the editor server. `max_rate_keys` bounds rate-limiter
state. Invalid modes, unknown properties, malformed trusted networks, or zero active limits stop
startup. `observe` records a safe metadata-only decision but permits the request; `off` is the
class-specific legacy fallback. Do not disable all classes to solve one compatibility issue.

The default limits are documented in the generated `n2linkd.toml`. The larger defaults are
intentional for Node-RED flow deploys and editor assets. Flow Copilot has only two concurrent
requests and 30 requests per minute because each accepted request may start paid remote work.
WASM plugin administration (`plugins`) accepts one request at a time, six per minute, bodies up to
1 MiB (one package) and responses up to 64 KiB.

## Client addresses and proxies

EdgeLinkd uses the TCP peer address by default and ignores `Forwarded` and `X-Forwarded-For` from
all other clients. Add only reverse-proxy addresses or CIDRs you administer:

```toml
[api_protection]
trusted_proxies = ["127.0.0.1", "192.168.20.0/24"]
```

When the direct peer is trusted, malformed forwarded addresses fail with HTTP 400. A broad private
network is not a safe default: any host in that network could otherwise choose its rate-limit key.

## Optional `http in` authentication

Node-RED-compatible `http in` endpoints remain unauthenticated unless an administrator explicitly
requires one shared bearer token. Store the token only in an environment variable:

```toml
[api_protection]
webhook_bearer_env = "N2LINK_WEBHOOK_TOKEN"
```

```bash
read -rsp 'Webhook token: ' N2LINK_WEBHOOK_TOKEN
export N2LINK_WEBHOOK_TOKEN
N2LINK_HOME="$PWD" target/debug/n2linkd run
```

If the configured variable is missing or empty, deploying an `http in` node fails closed. The
authorization header is checked before its body is allocated or sent into the flow. Tokens,
request bodies, prompts, client addresses, hostnames, and paths are not written into ingress
decision logs.

## Response, cancellation, and WebSocket behavior

API bodies are collected only up to the class budget. Fixed-length and chunked bodies use the same
cap. A deadline covers body collection and handler execution; dropping the timed-out future
cancels that work and releases its permits. Shutdown cancels HTTP work and uses Axum graceful
shutdown. `http in` cancels connection tasks on flow shutdown and removes response-registry entries
even when a request future is dropped.

API responses are bounded before delivery. Static files retain streaming behavior; their declared
`Content-Length` is checked without buffering the complete asset. WebSocket upgrades retain the
existing token expiry and revocation behavior, add a finite upgraded-connection count, and cap
each incoming message.

## Rollout and rollback

1. Back up the active configuration overlay. Do not copy or alter flow or credential files.
2. Start with the compiled defaults and exercise editor deploy, credentials, palette, library,
   debug messages, WebSocket reconnect, `http in`, and Copilot if enabled.
3. Review only `ingress decision` metadata. Rejections use 400, 401, 408, 413, 429, 431, 502, 503,
   or 504 as appropriate.
4. If one legitimate workload exceeds a budget, first raise only that class's value. If its exact
   requirement is not yet known, temporarily change only that class to `observe`.
5. Restart EdgeLinkd and repeat the rejected request. Confirm unrelated classes still reject the
   same over-budget probes.
6. Restore `enforce` after measuring a safe bound.

This rollback changes configuration only. It does not rewrite `flows.json`, either credential
generation, encrypted envelopes, the keyring, or future durable-history data.
