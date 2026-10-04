# Phase 6 Agent Prompt - Selective AI Nodes

You are implementing Phase 6 of the z8run adoption plan in EdgeLinkd.

Repository: `/media/kestl/andor/github/edgelinkd`

Read first: `AGENTS.md`, `adoption/z8adoptionplan.md`, the Phase 0 ADR, and evidence for phases
1-5. Also read `.agents/skills/port-node-red-node/SKILL.md` if porting any upstream node and
follow it fully. Inspect the existing `ai-provider`, `ai-chat`, provider adapter, editor HTML,
credential handling, egress policy, typed metadata, and Flow Copilot boundaries.

## Operating rules

- Use Palimnex before edits and refresh/deep-validate afterward.
- Verify prior egress, credential, API-limit, and metadata gates; AI work must not bypass them.
- Preserve unrelated work and secrets. Live provider credentials stay in environment/local
  secret storage and must never be printed or committed.
- Implement one node family at a time in the stated order. Stop and report if a prerequisite
  or provider contract is uncertain rather than fabricating support.
- Use `apply_patch`; do not hand-edit generated files or Cargo.lock.
- Do not commit, push, tag, publish, release, or change the version without separate approval.

## Objective and implementation order

1. Local deterministic text splitter.
2. Structured-output validator/parser.
3. Embeddings.
4. Bounded agent/tool loop.
5. Media/provider-specific nodes only if separately justified and approved.

Use focused feature gates where practical, such as text, embeddings, and agents, while
preserving the existing default AI behavior approved by the product.

## Required behavior for all AI nodes

- Explicit provider/model capability validation.
- Provider-specific request construction; omit unsupported optional parameters.
- Egress policy on every outbound request and redirect.
- Credentials obtained through the credential service only.
- Input/output/token/time/concurrency limits and cancellation.
- Bounded retries with idempotence rules and no duplicate side effects.
- Secret-safe logs, status, catchable node errors, audit, and optional history.
- Typed registry metadata and editor configuration.
- Loud deploy-time rejection of recognized but unsupported configuration.

Agent/tool nodes additionally require:

- Maximum turns and tool calls.
- Explicit tool allowlist and argument validation.
- Capability checks for network, filesystem, process, context, and flow operations.
- No autonomous flow deployment; changes remain previewed and user-approved.
- Bounded accumulated context and tool output.
- Loop, recursion, and repeated-side-effect prevention.

## Required tests

- Deterministic mock-provider contract suites for every node and provider claimed.
- OpenAI, Claude, Grok, and Cortex request-shape fixtures where claimed supported.
- Model with omitted/default model configuration.
- Unsupported temperature/model/parameter behavior.
- Provider 4xx/5xx, timeout, disconnect, malformed/truncated JSON, and oversized responses.
- Cancellation, token/output limits, maximum turns, and maximum tools.
- Tool denial, malformed arguments, egress denial, and capability denial.
- Retry idempotence and duplicate-side-effect prevention.
- Secret absence from flows, metadata, prompts where excluded, logs, errors, audit, and history.
- Editor credential placeholder, deploy, reopen, and clear-secret behavior.
- Catch/status behavior and concurrent load/backpressure.
- Focused pytest behavior tests for each shipped node.
- Opt-in live-provider tests only with explicit local environment credentials; clearly separate
  mocked success from live acceptance.
- Full mandatory gate, feature combinations, ARM check, and Phase 0 resource comparison.

For EdgeLinkd-specific nodes, document that they are not upstream Node-RED specs. If any node
ports an upstream Node-RED node, port and register the matching spec tests exactly as required
by `AGENTS.md`.

## Rollback drill

- Disable each new node-family feature independently and verify it disappears from the editor
  and metadata catalog.
- A flow containing a disabled node must fail loudly at deploy/start; it must not act as a
  successful no-op.
- Export/remove unsupported nodes from a copied flow and demonstrate downgrade.
- Re-enable the feature and prove provider configuration and encrypted credentials remain
  usable.
- Cancel a running agent/tool loop and prove no further tool side effects occur.

## Non-goals

- Do not implement every z8run SaaS/AI node.
- Do not create an unrestricted general-purpose autonomous agent.
- Do not expose arbitrary filesystem, shell, network, or deployment tools.
- Do not claim a provider production-supported based only on mock tests.

## Completion criteria

- Each enabled node is bounded, cancellable, secret-safe, and testable without live providers.
- Every production-supported provider has separate opt-in live acceptance evidence.
- Feature-disabled builds make no false catalog/support claims.
- Rollback does not orphan credentials or silently change flow behavior.

## Final response format

Report nodes/features completed separately, provider capability matrix, files/dependencies,
mock and live test counts, limits and cancellation evidence, editor checks, secret scan,
resource delta, rollback drill, unsupported features, Git status, and explicit
no-commit/no-push/no-release confirmation.
