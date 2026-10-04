# Phase 3 Agent Prompt - Webhook and API Resource Protection

You are implementing Phase 3 of the z8run adoption plan in EdgeLinkd.

Repository: `/media/kestl/andor/github/edgelinkd`

Read first: `AGENTS.md`, `adoption/z8adoptionplan.md`, the Phase 0 ADR, and evidence for phases
1-2. Inspect the Axum routers, dynamic node HTTP registry, authentication middleware,
WebSockets, Flow Copilot handler, and audit implementation.

## Operating rules

- Use Palimnex before edits and refresh/validate it afterward.
- Verify prior gates from source and tests. Preserve unrelated work and secrets.
- Use existing Axum/Tower patterns and add dependencies only with explicit footprint
  justification.
- Use `apply_patch`; never hand-edit generated files or Cargo.lock.
- Do not commit, push, tag, publish, release, or change the version without separate approval.

## Objective

Protect public and administrative HTTP surfaces from oversized, slow, unauthenticated, or
over-concurrent work without breaking the Node-RED editor, WebSockets, HTTP-in flows, or
legitimate AI requests.

## Required implementation

- Inventory routes and classify them as health, editor/admin, authentication, WebSocket,
  webhook/HTTP-in, AI/Copilot, fleet, or static asset.
- Create endpoint-class-specific configuration for:
  - body and header limits;
  - rate limits by authenticated principal and/or client address;
  - global and endpoint concurrency;
  - execution deadlines;
  - bounded responses where applicable;
  - cancellation on timeout, disconnect, or shutdown.
- Trust forwarded client addresses only from explicit trusted proxies.
- Authenticate/authorize before expensive parsing or execution.
- Require exact method and route matching for externally triggered flows.
- Keep WebSocket upgrade/revocation semantics intact.
- Log/audit only safe metadata; never bodies, auth tokens, credentials, or complete prompts.
- Permit independent compatibility fallback per endpoint class, not one global disable switch.

## Required tests

- Oversized fixed and chunked bodies.
- Slow request body and slow response behavior.
- Excess headers, malformed lengths, and unsupported media types.
- Per-user, per-IP, and global rate/concurrency limits.
- Forged forwarding headers with and without a configured trusted proxy.
- Queue saturation, fairness, recovery, and cancellation.
- Authentication before body parsing and AI work.
- Exact method/path mismatch.
- WebSocket upgrade, session expiry, revocation, and browser reconnect.
- Node-RED deploy, credentials, settings, library, palette, and debug/editor smoke tests.
- HTTP-in/webhook success and catchable failure behavior.
- Flow Copilot prompt and provider-response limits.
- Security logs contain no secrets or request bodies.
- Full mandatory gate and resource comparison to Phase 0.

## Rollback drill

- Enable strict limits and prove an oversized/over-concurrent request is rejected.
- Relax only that endpoint class and prove a legitimate request succeeds.
- Restore the prior generated limit configuration.
- Verify flows, credential files, encrypted envelopes, and history state are unchanged.
- Verify other endpoint classes remain protected during the targeted rollback.

## Non-goals

- No general reverse proxy or WAF implementation.
- No durable history, new AI nodes, or WASM.
- Do not weaken existing authentication, WebSocket revocation, deploy locking, or egress rules.

## Completion criteria

- Legitimate editor, MQTT, HTTP-in, and Copilot workflows pass.
- Resource-exhaustion tests remain bounded and cancellation is observable.
- Trusted-proxy behavior is explicit and fail-closed.
- Each endpoint class can be rolled back independently.

## Final response format

Report route classification, configured limits, files changed, exact tests/counts, browser and
live checks, cancellation evidence, resource delta, rollback drill, remaining risks, Git
status, and explicit no-commit/no-push/no-release confirmation.
