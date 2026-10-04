# Phase 1 Agent Prompt - Central Egress Policy

You are implementing Phase 1 of the z8run adoption plan in EdgeLinkd.

Repository: `/media/kestl/andor/github/edgelinkd`

Read first: `AGENTS.md`, `adoption/z8adoptionplan.md`, and the completed Phase 0 ADR/baseline.
Reference only: z8run's egress design at the pinned sibling checkout; do not transplant code.

## Operating rules

- Use Palimnex before edits and refresh it after edits. Open authoritative source files.
- Verify Phase 0's exit evidence. If absent or materially stale, stop and report the missing
  prerequisite instead of inventing a baseline.
- Preserve unrelated work and secrets. Use `apply_patch`; do not hand-edit generated files or
  `Cargo.lock`.
- Follow the closest existing Rust patterns and keep clippy clean.
- Do not commit, push, tag, publish, release, or change the version without separate approval.

## Objective

Introduce one shared outbound-network policy without breaking legitimate embedded/private
network use. It must protect dynamic/untrusted destinations, support explicit administrator
allowlists, and be adoptable through `off`, `observe`, and `enforce` modes.

## Required implementation

1. Inventory and classify all outbound call sites, including HTTP request, AI, OIDC, fleet,
   MQTT, WebSocket, TCP, Modbus, and future adapter boundaries.
2. Design a small reusable core policy and configuration model for:
   - hostname, IPv4/IPv6, CIDR, protocol, and port allowlists;
   - unsafe address classes and cloud metadata targets;
   - every DNS result, not just the first;
   - redirect revalidation;
   - DNS-rebinding-resistant connection behavior;
   - explicit proxy policy;
   - connect/request/idle timeouts;
   - HTTP/AI response-size caps;
   - secret-safe policy telemetry.
3. Preserve private industrial endpoints through explicit administrator rules. Do not silently
   allow every private address and do not make a default change that strands current users
   without an observe-mode migration path.
4. Integrate in bounded slices. A call site must not claim protection until its actual socket
   or HTTP connection is governed by the shared decision.
5. Invalid policy configuration must fail clearly at startup. Unsupported combinations must
   use a real error, never a warning-only no-op.
6. Document the migration from `off` to `observe` to `enforce` and provide a safe example for
   local MQTT without embedding credentials.

## Required tests

- Unit matrix for hostnames, IPv4, IPv6, CIDRs, ports, protocols, wildcard rejection, and
  configuration parsing.
- Loopback/private/link-local/multicast/unspecified/metadata cases.
- Mixed allowed/disallowed DNS answers.
- Simulated DNS rebinding and public-to-private redirect.
- Encoded IPs, URL user-info, malformed hosts, and other parser differentials.
- Environment-proxy isolation.
- Connect/request/idle timeout and body-limit tests.
- Audit/observe logs contain decisions but no credentials, tokens, or sensitive URL data.
- Existing HTTP, AI, OIDC, fleet, MQTT, WebSocket, TCP, and Modbus tests as applicable.
- Live local RabbitMQ publish/subscribe with an explicit allowlist if available; credentials
  only through environment variables.
- Mock AI and opt-in live-provider acceptance.
- Invalid configuration fails startup.
- Phase 0 default/minimal/full size and RSS comparison.

Run the mandatory gate from the controlling plan, including full workspace tests, pytest,
clippy, formatting, cross-build, diff check, and Palimnex validation.

## Rollback drill

- Start in `enforce`, prove a denied connection, switch to `observe`, and prove the same
  legitimate configured connection is no longer blocked.
- Restore the pre-enforcement configuration snapshot.
- Verify no flow or credential file changes are required to roll back this phase.
- Prove that reverting the policy implementation does not require a data migration.

## Non-goals

- Do not rewrite the runtime scheduler.
- Do not add database history, credential encryption, AI nodes, or WASM.
- Do not silently weaken OIDC or admin authentication to make tests pass.

## Completion criteria

- Every claimed call site is demonstrably governed by the common policy.
- Existing private-device workflows work through explicit rules.
- Bypass and redirection tests fail closed in enforce mode.
- Resource budgets remain within the Phase 0 limits or the phase is held for review.

## Final response format

Report files, configuration contract, protected and still-unprotected call sites, exact tests
and counts, live checks, resource deltas, rollback drill, open risks, Git status, and explicit
no-commit/no-push/no-release confirmation.
