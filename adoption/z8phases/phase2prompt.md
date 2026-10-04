# Phase 2 Agent Prompt - Encrypted Credential Storage

You are implementing Phase 2 of the z8run adoption plan in EdgeLinkd.

Repository: `/media/kestl/andor/github/edgelinkd`

Read first: `AGENTS.md`, `adoption/z8adoptionplan.md`, the Phase 0 ADR/baseline, and Phase 1
evidence. Inspect `flow_credentials.rs`, web credential handlers, atomic-file utilities, and
the complete transactional deploy/rollback path before editing.

## Operating rules

- Use Palimnex before and after edits. Source and tests are authoritative.
- Verify the previous phase gates; do not assume a report means code is present.
- Work only on copied/sanitized credential fixtures. Never print, migrate, rotate, or back up
  the user's live secrets during development.
- Use audited Rust cryptography libraries; do not invent a cipher, nonce scheme, or KDF.
- Preserve unrelated work. Use `apply_patch`; do not hand-edit generated files or Cargo.lock.
- Do not commit, push, tag, publish, release, or change the version without separate approval.

## Objective

Add versioned authenticated encryption for the credential sidecar while preserving Node-RED
placeholder semantics and the existing four-file transactional deploy/rollback guarantees.

## Required design before implementation

Document and review:

- Envelope format, version, algorithm, nonce, key identifier, and authenticated metadata.
- Key-provider abstraction and exact key precedence.
- Local key-file creation/permission behavior and injected-secret behavior.
- Key backup, loss, rotation, and downgrade recovery.
- Plaintext/encrypted compatibility window.
- Crash consistency across live flows, live credentials, previous flows, and previous
  credentials.
- Behavior for missing, wrong, corrupt, or unsupported keys/formats.

## Required implementation

- Read both existing plaintext and the new encrypted envelope during the compatibility window.
- Never silently rewrite plaintext at startup.
- Add explicit status, dry-run migration, encryption migration, rotation, and recovery/export
  operations in the repository's existing CLI style.
- Refuse to overwrite unreadable credential data.
- Preserve `__PWRD__`, blank clearing, secret stripping, revision semantics, deploy locking,
  activation compensation, and `.prev` rollback.
- Use atomic same-directory writes and mode `0600` for credential and key material where the
  OS permits.
- Ensure errors, logs, API responses, history hooks, and audits never expose key material,
  plaintext, ciphertext, nonces, or full credential objects.

## Required tests

- Known-answer or library-appropriate encryption tests plus randomized round trips.
- Nonce uniqueness; wrong/missing key; corrupt header, ciphertext, and authentication tag.
- Unsupported envelope version/algorithm fails loudly.
- `0600` mode and atomic replacement.
- Failure injection before each rename in the complete four-file transaction.
- Deploy and rollback match flow and credential generations.
- Failed activation preserves the former rollback generation.
- Placeholder retention and blank clearing.
- Rotation covers live and previous sidecars atomically.
- Concurrent deploy and rotation serialize safely.
- Plaintext-to-encrypted migration dry run and real run on fixtures.
- Controlled export/downgrade on fixtures.
- Lost-key recovery drill.
- Secret scanners over logs, audit, errors, API output, diffs, and test artifacts.
- Existing credential, deploy, rollback, MQTT, and AI tests.
- Full mandatory gate and Phase 0 resource comparison.

## Rollback drill

1. Back up a sanitized four-file fixture and test key.
2. Migrate it to encrypted form.
3. Deploy a new credential generation and roll it back.
4. Export/decrypt through the supported downgrade path.
5. Start the compatible prior-format test path and verify the expected credentials.
6. Inject a migration failure and prove byte-for-byte restoration of all original files.

Do not leave an automatic plaintext backup beside a live encrypted sidecar.

## Non-goals

- No OS keyring/TPM implementation unless separately approved; design an interface only.
- No durable history, new AI nodes, or WASM.
- Do not change credential API response semantics except where an explicit migration/status API
  is approved.

## Completion criteria

- Crash injection always leaves a complete old or complete new generation.
- Wrong or lost keys cannot cause data overwrite.
- Existing editor credential behavior is unchanged.
- A documented and tested downgrade path exists for the compatibility window.

## Final response format

Report the envelope and key contract without secret values, files changed, test counts,
failure-injection matrix, migration and rollback drill, resource delta, remaining platform
limits, Git status, and explicit no-commit/no-push/no-release confirmation.
