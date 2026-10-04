# Phase 3 Inbound Resource Protection Design

Status: implementation contract, 2026-10-03.

## Boundaries

The Node-RED-compatible Axum server and `http in` are separate ingress stacks. Axum middleware
protects the editor/API server. The Rust `http in` node owns a raw TCP listener and applies the
same shared configuration model before producing a flow message. No reverse proxy, WAF, durable
history, AI node, or extension runtime is introduced.

Eight endpoint classes are stable configuration identities: health, editor/admin, authentication,
WebSocket, webhook, Copilot, fleet, and static assets. Each class has its own mode and budgets. A
global Axum semaphore provides the cross-class ceiling; `http in` listeners have finite per-node
connection capacity because they do not share the editor listener.

## Admission order

For protected Axum routes the order is:

1. route and method selection;
2. existing administrator authentication/authorization;
3. header and trusted-client validation;
4. per-principal and per-client rate accounting;
5. global and class capacity admission;
6. bounded body collection;
7. handler execution under the same deadline and shutdown cancellation;
8. bounded response collection where streaming/upgrade semantics permit it.

The static fallback is protected by an outer middleware because it is not part of the API router.
It retains streaming and rejects an asset whose declared length exceeds its class budget. A marker
prevents editor static routes registered inside the API router from being charged twice.

WebSocket admission uses the normal request checks, then transfers a separate class permit into
the upgraded socket lifetime. Axum's maximum incoming message size is set from the WebSocket body
budget. Existing session binding, expiry, revocation broadcasts, lag recovery, and shutdown close
paths remain unchanged.

## Client identity and safe telemetry

The TCP peer is authoritative unless its IP matches an administrator-supplied exact address or
CIDR. Only then may `Forwarded` or `X-Forwarded-For` select the client rate key. A malformed trusted
forwarding header is rejected; an untrusted forwarding header is ignored.

Rate state is a finite fixed-window map. Authenticated API work is charged to both username and
client address. Anonymous/authentication work is charged to the address. No bearer token is used
as a key. Ingress logs contain only mode, action, class, and reason.

## HTTP-in contract

An `http in` URL must be an absolute path without a query string. Method and path match exactly;
prefix and substring matching are removed. The listener bounds request-line/header reads, header
count, fixed and chunked bodies, concurrent connections, queue wait, response wait, response body,
and response headers. Conflicting or malformed lengths fail closed. Response headers containing
control characters are rejected.

An optional `api_protection.webhook_bearer_env` names an environment variable containing a shared
bearer token. Missing configured material prevents the node graph from loading. Authentication is
constant-time and happens after bounded headers but before body allocation or flow execution.

Every response-registry registration owns a drop guard. Normal completion cleans it synchronously;
timeout, disconnect, shutdown, or task cancellation schedules removal, so cancelled requests do
not accumulate response senders.

## Compatibility and rollback

`enforce` is the safe default. `observe` logs the same over-budget decision without rejecting and
`off` bypasses the class. The modes are independent. Existing configuration files need no rewrite
because absent tables deserialize to compiled defaults. Rollback modifies only the affected class
and restarts the process; no flow, credential, envelope, key, audit, or future history migration is
coupled to these limits.
