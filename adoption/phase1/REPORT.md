# Phase 1 Central Egress Policy Report

- Status: complete; committed as `b092696`
- Date: 2026-10-03 (America/Guatemala)
- Branch: `master`
- Base revision: `35e0546c68c6aed057e5ac246aac46b0844f457c`
- Version: unchanged at `0.3.0`

## Configuration contract

`[egress] mode` is `off`, `observe`, or `enforce`. `off` preserves the legacy connection
paths. `observe` resolves and pins targets and records allow/deny decisions without blocking.
`enforce` requires every resolved address to match an exact protocol/port rule. An unsafe
address reached through DNS also requires a CIDR-bound rule; an exact IP literal is itself an
address-bound rule. Cloud metadata hostnames and addresses cannot be allowlisted.

Rules are `[[egress.allow]]` tables containing non-empty `protocols` and `ports`, plus `host`,
`cidr`, or both. Hosts are exact: wildcards and URL-like host strings are rejected. IPv4 and
IPv6 CIDRs are supported. Port zero, empty rules, invalid prefixes, unknown fields, zero
timeouts, and invalid modes fail before the runtime starts.

The policy also sets connect, request, and idle timeouts, response-byte and redirect limits,
and an optional credential-free `proxy_url`. Governed modes isolate ambient proxy variables.
`allow_environment_proxy = true` is rejected in `observe` and `enforce`, because an ambient
proxy cannot be resolved and pinned reliably. An explicit proxy is separately resolved,
allowlisted, and pinned; it becomes a documented trust boundary for destination resolution.

The generated default configuration and `README.md` document migration from `off` to
`observe` to `enforce`, including credential-free MQTT, private Modbus, and proxy examples.

## Governed call sites

| Call site | Actual connection boundary |
|---|---|
| HTTP request node | Every initial URL and redirect is resolved and revalidated; reqwest is pinned; cross-origin credentials are stripped; bodies and idle periods are bounded |
| AI providers and Flow Copilot | Provider URL uses a pinned reqwest client; request/idle limits and response cap apply; provider keys and URLs are absent from governed errors |
| Admin OIDC | Discovery, token, and userinfo endpoints use pinned clients and bounded JSON reads |
| Fleet | Source and target flow calls use pinned clients and bounded responses |
| MQTT | Every broker DNS result is checked; the shared rumqttc session connects and reconnects to the selected approved address |
| WebSocket client/in/out | The WebSocket handshake runs over the approved TCP stream while retaining the original hostname for TLS/SNI |
| TCP in-client/out/request | Client sockets are opened only by `EgressPolicy::connect_tcp` |
| UDP out | The message/config destination IP and port are checked before `send_to` |
| Modbus TCP | The retained stream is opened only by `EgressPolicy::connect_tcp` |

Server listeners are inbound and outside this phase. The `exec` node can start an arbitrary
administrator-configured child process, so the parent process cannot claim to govern that
child's network calls. No database client currently opens an outbound socket. Future adapters
must consume this policy before being described as protected.

## Security behavior and tests

The unit matrix covers exact hosts, IPv4/IPv6 CIDRs, protocol and port mismatches, wildcard and
empty-rule rejection, loopback/private/carrier-NAT/link-local/multicast/unspecified classes,
metadata aliases and IPs, mixed DNS answers, URL user-info, invalid configuration, proxy
isolation, and the `enforce -> observe -> explicit enforce rule` rollback sequence. Approved
addresses are carried into the actual socket/client, preventing a second unvalidated DNS lookup.

Enforce-mode integration coverage includes:

- local deterministic OpenAI-compatible responses;
- AI request timeout and response-size rejection;
- OIDC discovery/code/userinfo;
- fleet source/target promotion;
- local RabbitMQ MQTT v4 and v5 publish/subscribe with the explicit
  `mqtt/127.0.0.1:1883` rule.

The RabbitMQ credentials were read from the ignored credential sidecar into
`EDGELINK_MQTT_USER` and `EDGELINK_MQTT_PASSWORD`. They were not printed or written to a tracked
file.

## Validation

| Command or check | Result |
|---|---|
| `cargo fmt --check` | passed |
| `cargo clippy --all-features --tests --all -- -D warnings` | passed |
| `cargo test --workspace --features full --no-fail-fast` | app 1 passed; core 262 passed/1 ignored; web 46 passed; 2 doctests passed |
| `cargo test -p edgelink-core --lib --all-features -- --test-threads=8` | 274 passed, 1 ignored |
| `cargo build --all` | passed |
| `.venv/bin/pytest ./tests -v` | 825 passed, 213 skipped |
| ARMv7 full workspace build, excluding PyO3 | passed |
| Live RabbitMQ enforce-mode test | 1 passed; v4 and v5 round trips |
| Palimnex incremental index and deep validation | fresh and passed; final counts recorded in the handoff |
| Palimnex ledger | ready; integrity ok; no pending projection |
| `git diff --check` | passed |

The first web-test build attempt ended in a transient rustc SIGSEGV. The identical test suite
passed with `CARGO_BUILD_JOBS=2`; this was a compiler-process failure, not a test failure.
`palimnex doctor` reports `action_needed` only for intentionally excluded third-party files and
two crate files outside the current include patterns; Redis, cache, configuration, ignore rules,
and ledger checks are healthy. Redis was not flushed.

## Resource comparison

The final post-hardening stripped `ci` measurements were:

| Configuration | Phase 0 bytes | Phase 1 bytes | Delta |
|---|---:|---:|---:|
| Default | 14,420,296 | 14,527,080 | +106,784 (+0.74%) |
| Root no-default | 14,259,480 | 14,357,048 | +97,568 (+0.68%) |
| Root `full` | 14,420,296 | 14,527,016 | +106,720 (+0.74%) |
| All features | 14,509,464 | 14,614,296 | +104,832 (+0.72%) |

These are below the Phase 0 per-phase limits.

Using the same copied fixture and warm-cache classification as Phase 0:

| Measurement | Phase 0 | Phase 1 | Delta |
|---|---:|---:|---:|
| Startup median, 5 samples | 7 ms | 21 ms | +14 ms |
| Idle RSS median, 5 samples | 13,404 KiB | 13,388 KiB | -16 KiB |
| Health median, 100 requests | 0.337 ms | 0.605 ms | +0.268 ms |
| Health p95, 100 requests | 0.466 ms | 0.892 ms | +0.426 ms |

All remain inside the ADR noise-floor budgets. The first benchmark attempt is excluded: it
omitted the sanitized credential fixture, all candidates exited before binding, and the script
continued timing failed curl calls. The corrected run used strict shell failure handling and the
complete copied fixture.

## Rollback drill

`enforce` without a rule denied a loopback TCP connection. Switching the same policy to
`observe` allowed it. Restoring `enforce` with an exact host/protocol/port rule allowed it. This
is automated by `enforce_denies_then_observe_and_an_explicit_rule_connect`.

Rollback requires only changing `egress.mode` to `observe` or `off` and restarting. This phase
does not alter `flows.json`, `flows_cred.json`, either `.prev` file, or any schema, so no data
migration or credential rewrite is needed.

## Residual boundaries

- An explicitly configured proxy is a trust boundary: EdgeLinkd pins the proxy socket and
  validates the requested origin locally, but the proxy may perform its own destination DNS.
- MQTT selects the first approved address after checking every initial answer and retains that
  address for the session/reconnect loop; a redeploy is required to adopt a DNS change.
- Environment-proxy behavior is isolated and configuration-validated, but a packet-capture
  proxy test was not added; the reqwest client construction is covered by unit/config tests.
- Opt-in live public AI-provider acceptance was not run because no external provider call was
  authorized. The enforce-mode deterministic provider exercises the same HTTP adapter contract.
- Windows was compile-checked through existing CI-oriented code paths only; this local host did
  not execute a Windows socket test.

## Git and release state

Phase 1 was committed as `b092696` on top of the Phase 0/adoption baseline and later pushed to
`origin/master`. It did not create a tag, package publication, or release, and it did not rewrite
flows or credentials.

## Phase 1.1 editor adjustment

The central policy can now be administered from **User Settings -> EdgeLinkd** without exposing
the rest of the active environment overlay. This adjustment is included in `b092696`.

- The pane is opt-in through `[config_editor] enabled = true` and EdgeLinkd refuses to enable it
  without configured admin authentication.
- The `administrator` role has `config.read`, `config.write`, and `runtime.restart` through its
  full administrative scope. `deployer` and `viewer` do not inherit process-configuration access.
- Read, validate, save, save-and-apply, and rollback-and-apply operate only on `[egress]`.
- Whole-file SHA-256 revisions prevent overwriting concurrent external edits, while the
  restart-required indicator compares only the saved and active egress policies.
- Saves atomically update the active overlay and a private `0600` `.prev` copy. Applying replaces
  the shared policy used by every governed client and restarts the flow runtime. Failed activation
  restores the prior policy, runtime, and disk state.
- Audit records contain the actor, action, and revision, never configuration bodies or secrets.
  Bootstrap and recovery remain file-based, so the pane cannot grant itself administrator access.

Phase 1.1 validation:

| Command or check | Result |
|---|---|
| `cargo fmt --check` | passed |
| `cargo clippy --all-features --tests --all -- -D warnings` | passed |
| `cargo test --workspace --features full --no-fail-fast` | passed: app 1; core 263/1 ignored; web 53; doctests 2 |
| `cargo test -p edgelink-web --lib --all-features -- --test-threads=8` | 54 passed |
| `cargo test -p edgelink-core --lib --all-features -- --test-threads=8` | 275 passed, 1 ignored |
| `.venv/bin/pytest ./tests -v` after `cargo build --all` | 825 passed, 213 skipped |
| Live RabbitMQ enforce-mode test | 1 passed; MQTT v4 and v5 round trips |
| ARMv7 full workspace build, excluding PyO3 | passed |

The default stripped `ci` binary is 14,740,552 bytes: +213,472 bytes (+1.47%) over the Phase 1
measurement and +320,256 bytes (+2.22%) over Phase 0. No browser automation was available, so the
final visual click-through of the new tab remains a user acceptance check; the exact editor plugin,
settings, permission, and backend contracts are covered by the web tests.
