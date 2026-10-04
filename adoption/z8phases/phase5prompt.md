# Phase 5 Agent Prompt - Typed Metadata for Flow Copilot

You are implementing Phase 5 of the z8run adoption plan in EdgeLinkd.

Repository: `/media/kestl/andor/github/edgelinkd`

Read first: `AGENTS.md`, `adoption/z8adoptionplan.md`, the Phase 0 ADR, and evidence for phases
1-4. Inspect runtime node registration, node-set JSON/HTML generation, Flow Copilot's skill and
draft schema, `materialize_draft`, `Engine::prepare_flows`, and editor catalog tests.

## Operating rules

- Use Palimnex before edits and refresh/deep-validate afterward.
- Verify prior gates. Preserve unrelated work and secrets.
- Node-RED messages remain dynamically typed. Metadata may guide the editor/Copilot but must
  not silently reinterpret or over-constrain runtime messages.
- A supported Node-RED node behavior still requires the upstream-compatible tests described in
  `AGENTS.md`. EdgeLinkd-specific metadata tests do not replace them.
- Use `apply_patch`; do not hand-edit generated files or Cargo.lock.
- Do not commit, push, tag, publish, release, or change the version without separate approval.

## Objective

Give Flow Copilot and custom UI code a versioned, live registry description of node structure
so impossible nodes, references, and wires are rejected before a draft reaches the canvas.

## Required design

Define a versioned metadata schema for:

- Node-RED type/module/set identity and enabled feature.
- Input/output cardinality and named-port hints where meaningful.
- Required and optional configuration properties.
- Configuration-node references.
- Secret fields, without secret values.
- Capabilities such as network, filesystem, AI, database, or external process.
- Advisory input/output property types.
- Metadata provenance and schema version.

Specify which fields are authoritative deploy constraints and which are advisory Copilot hints.

## Required implementation

- Extend self-registration or an adjacent inventory mechanism in the closest existing style.
- Avoid a second handwritten catalog that can drift from the actual registry.
- Expose metadata through a bounded authenticated endpoint or existing node catalog contract
  without breaking Node-RED clients.
- Teach Flow Copilot to validate registered type, enabled feature, output port, required config
  reference, reserved properties, and any other proven structural invariant.
- Continue running `Engine::prepare_flows` on the complete candidate graph.
- Keep user preview/approval before deployment.
- Provide a type-only fallback for older metadata or compatibility rollback.
- Exclude credentials, secret values, internal paths, and irrelevant configuration from AI
  prompts.

## Required tests

- Every registered node produces schema-valid metadata.
- Duplicate/missing metadata and registry/catalog drift fail tests.
- Default and feature builds expose exactly their live node types.
- Unknown type, disabled feature, nonexistent output, missing config node, duplicate reference,
  and reserved property are rejected.
- Dynamic payload/property types that Node-RED permits are not rejected.
- Existing MQTT brokers and AI providers are reused rather than duplicated.
- Secret fields are labeled but values never enter metadata or model prompts.
- Property/fuzz tests for references, IDs, ports, positions, and wires.
- Golden prompts including the timestamp-to-MQTT-and-CSV scenario.
- Old client/type-only fallback behavior.
- Editor palette and node-set HTML/JSON smoke tests.
- Full mandatory gate and Phase 0 resource comparison.

## Rollback drill

- Disable strict metadata validation while retaining current type-only draft validation.
- Serve or consume the previous schema version and prove Copilot still produces a preview.
- Prove no flow-file transformation is needed.
- Confirm runtime wiring/execution is identical before and after metadata fallback.

## Non-goals

- Do not replace the Node-RED editor with React Flow.
- Do not introduce a static type system for runtime messages.
- Do not auto-deploy an AI-generated flow.
- No new AI nodes or WASM in this phase.

## Completion criteria

- The live registry is the source of metadata truth.
- Golden drafts cannot invent unavailable nodes or ports.
- Existing valid dynamic Node-RED flows remain valid.
- Metadata can be rolled back without persisted-data migration.

## Final response format

Report schema/version, authoritative versus advisory fields, files changed, test counts and
golden prompts, editor smoke results, fallback/rollback drill, resource delta, open metadata
coverage, Git status, and explicit no-commit/no-push/no-release confirmation.
