# Outbound Egress Policy Runbook

n2link's shared egress policy governs outbound HTTP, AI-provider, OIDC, fleet, MQTT, WebSocket,
TCP, UDP, and Modbus connections. It is intended to limit server-side request forgery, accidental
metadata access, DNS rebinding, unbounded responses, and undeclared proxy use without preventing
embedded installations from reaching explicitly approved private devices.

## Modes

- `off` preserves legacy outbound behavior.
- `observe` resolves and evaluates destinations, pins approved sockets, and logs decisions without
  blocking a destination solely because it is not allowlisted.
- `enforce` permits only destinations matching an exact protocol, port, host and, where required,
  address/CIDR rule.

Cloud metadata destinations remain denied. Wildcard hosts, URL-shaped hosts, invalid CIDRs, empty
rules, zero limits, and unknown fields fail configuration validation.

## Staged rollout

1. Back up the active configuration overlay.
2. Set `mode = "observe"` and restart or use **User Settings -> n2link -> Save and apply**.
3. Exercise every production flow and administrative integration.
4. Review `egress decision` records. Logs intentionally omit hosts, paths, queries, and secrets, so
   correlate them by purpose, protocol, port, action, and time.
5. Add the narrowest rules that cover the observed, intended destinations.
6. Validate the configuration, switch to `enforce`, restart, and repeat acceptance tests.

Example:

```toml
[egress]
mode = "enforce"
allow_environment_proxy = false
connect_timeout_ms = 10000
request_timeout_ms = 60000
idle_timeout_ms = 30000
max_response_bytes = 1048576
max_redirects = 5

[[egress.allow]]
protocols = ["mqtt"]
host = "127.0.0.1"
ports = [1883]

[[egress.allow]]
protocols = ["modbus"]
host = "plc.example.internal"
cidr = "192.168.20.0/24"
ports = [502]
```

Private DNS names need a CIDR constraint so a later resolution cannot escape the intended network.
An exact private IP literal is already address-bound. When DNS returns multiple addresses, every
answer is checked before a governed client connects.

## Proxies and redirects

Governed modes isolate ambient proxy variables. Do not enable `allow_environment_proxy` in
`observe` or `enforce`; configuration rejects it because the proxy cannot be pinned reliably.

If a proxy is required, configure a credential-free `proxy_url` and an exact allow rule for the
proxy origin. Treat the proxy as a separate trust boundary: n2link pins the proxy socket, but the
proxy may perform its own destination resolution. Redirect targets are resolved and evaluated on
every hop, and cross-origin credentials are removed.

## Editor administration

The editor pane is opt-in:

```toml
[config_editor]
enabled = true
```

It also requires configured administrator authentication. Only an `administrator` receives
`config.read`, `config.write`, and `runtime.restart`; `viewer` and `deployer` do not. The API reads
and writes only `[egress]`, never the rest of an overlay that may contain passwords or OIDC secrets.
File revision checks reject stale saves, and apply failures restore the previous file, policy, and
runtime.

## Acceptance checks

After enforcement:

- health and editor endpoints remain available;
- each approved MQTT, Modbus, HTTP, AI, OIDC, fleet, WebSocket, TCP, and UDP path still works;
- an unlisted loopback/private target is denied;
- metadata destinations are denied even if a rule attempts to include them;
- HTTP response-size, redirect, request, connect, and idle limits behave as configured;
- logs contain no URL, credential, token, or provider secret.

The `exec` node can start an administrator-configured child process. The parent cannot govern that
child's network activity; use operating-system sandboxing or network policy for that boundary.

## Rollback

Change `egress.mode` to `observe` to retain diagnostics without blocking, or to `off` for legacy
behavior, then restart/apply. Egress rollback does not migrate or rewrite flows, credentials, or
context data. Preserve the denied decision records and the rejected rule set for diagnosis.
