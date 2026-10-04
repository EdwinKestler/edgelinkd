# Phase 2 Encrypted Credential Storage Report

- Status: complete
- Date: 2026-10-03 (America/Guatemala)
- Branch: `master`
- Base revision: `5a778f42eeb1fc890d9bd39aa94bd0cd110dca93`
- Version: unchanged at `0.3.0`

Phase 2 adds explicit authenticated encryption for Node-RED-compatible credential sidecars.
It does not migrate the user's installation automatically. Plaintext and encrypted sidecars are
both readable during this compatibility release, and deploy preserves the format already in use.

## Envelope and key contract

The version-1 JSON envelope uses XChaCha20-Poly1305 with a random 24-byte nonce, a 32-byte key,
and a detached 16-byte authentication tag. The format, version, algorithm, and key identifier are
authenticated as associated data. The key identifier is the first 128 bits of SHA-256 over the
key and is only a selector, not an authorization boundary. RustCrypto supplies the
[AEAD implementation](https://github.com/RustCrypto/AEADs/tree/master/chacha20poly1305), including
the [XChaCha20-Poly1305 type](https://docs.rs/chacha20poly1305/0.11.0/chacha20poly1305/type.XChaCha20Poly1305.html).
Key and plaintext buffers use [zeroize](https://docs.rs/zeroize/1.9.0/zeroize/struct.Zeroizing.html)
where their owned representation permits it.

Key precedence is exact:

1. A non-empty injected key from `credentials.key_env`, default
   `EDGELINK_CREDENTIAL_KEY`. Invalid input fails closed and never falls back.
2. A versioned local keyring at `credentials.key_file`, or `flows_cred.key` by default.

Local keyrings are created only by explicit migration or rotation and are required to be mode
`0600` on Unix. Sidecars, transaction journals, key backups, and plaintext exports are also
private. Startup does not generate a key or rewrite plaintext.

## User operations

The application now provides:

- `credentials status`
- `credentials migrate --dry-run`
- `credentials migrate --backup-dir PATH`
- `credentials rotate --backup-key PATH`
- `credentials recover --key-file PATH`
- `credentials export --output PATH`

Migration requires an explicit backup directory. Export creates a directly compatible plaintext
pair at `PATH` and `PATH.prev`; it refuses existing outputs and live-file conflicts. Recovery
validates the supplied key against both encrypted generations before installation. Injected keys
must be rotated in their external secret provider.

## Implementation scope

The new storage implementation is `crates/core/src/runtime/credential_storage.rs`. Core flow
loading, web deploy/rollback, credential GETs, Flow Copilot validation, and server state now use
the configured store. The application CLI, default configuration, README, features, dependencies,
and ignore rules were extended. The default application and web builds enable
`credential_encryption`; minimal builds can remove it and then read plaintext while rejecting
encrypted envelopes clearly.

No user flow, live credential sidecar, administrator configuration, version, generated spec
report, or release metadata was changed by Phase 2.

## Transaction and failure-injection matrix

All readers and writers use one advisory lock beside the sidecar. Web deploy retains its existing
in-process mutex as the first gate. An envelope is authenticated before any candidate can replace
it, so a missing/wrong key or corrupt input cannot be overwritten.

| Transaction | Injected boundary | Expected result | Evidence |
|---|---|---|---|
| Deploy | Before previous flows rename | All four old files byte-identical | passed |
| Deploy | Before previous credentials rename | All four old files byte-identical | passed |
| Deploy | Before live credentials rename | All four old files byte-identical | passed |
| Deploy | Before live flows rename | All four old files byte-identical | passed |
| Rollback | Before live credentials rename | All four old files byte-identical | passed |
| Rollback | Before live flows rename | All four old files byte-identical | passed |
| Rollback | Before previous credentials rename | All four old files byte-identical | passed |
| Rollback | Before previous flows rename | All four old files byte-identical | passed |
| Migration/rotation | Before current sidecar rename | Old sidecars/key restored | passed |
| Migration/rotation | Before previous sidecar rename | Old sidecars/key restored | passed |
| Migration/rotation | Before keyring rename | Old sidecars/key restored | passed |
| Activation | After persistence, before successful runtime activation | Live and prior rollback generations restored | passed |
| Abrupt stop | Journal exists after a partial replacement | Next credential operation restores the complete old generation | passed |

Migration journals reference and hash the administrator's explicit backup. They do not copy the
plaintext sidecar into an automatic backup. Rotation journals contain already-encrypted sidecars
and private key material, never decrypted credentials.

## Compatibility and recovery drill

A sanitized copied installation was exercised under `/tmp`:

1. `status` reported plaintext current/previous files and no key.
2. `migrate --dry-run` reported the planned local key without creating it or changing sidecars.
3. Real migration required an explicit backup and produced encrypted, decryptable current and
   previous generations plus a `0600` keyring.
4. The encrypted files contained neither sanitized credential value.
5. Rotation changed the key identifier and retained both credential generations.
6. Export produced a `0600` plaintext current/previous pair.
7. A build with `--no-default-features` read the exported pair as plaintext and decryptable.
8. Automated web coverage deployed a new encrypted credential, retained `__PWRD__`, cleared a
   blank password, rolled back, and recovered the matching earlier generation.
9. Automated lost-key recovery restored an old encrypted generation only after its saved keyring
   proved it could decrypt both files.
10. Failure injection restored original bytes and removed the transaction journal at every rename.

After the copied-fixture drill passed, the administrator explicitly migrated the live repository
installation. Both live generations report encrypted and decryptable, their files and local
keyring are mode `0600`, and the explicit backup directory is mode `0700`. The generated keyring
was copied into that backup at mode `0600`. A post-migration restart loaded the MQTT and AI global
nodes, started every flow, served an editor WebSocket, and shut down cleanly without a credential
or authentication-format error. Rotation and plaintext export were not run against the live
installation.

## Validation

| Command or check | Result |
|---|---|
| `cargo fmt --check` | passed |
| `cargo clippy --all-features --tests --all -- -D warnings` | passed |
| `cargo test --workspace --features full --no-fail-fast` | app unit 1; CLI integration 1; core 274 passed/1 ignored; web 61; doctests 2; no failures |
| Focused credential-storage tests | 10 passed |
| Credential CLI failure regression | 1 passed; visible stderr, exit code 1, fixture secret absent |
| Encrypted web credential/locking tests | 2 passed |
| Four-file deploy/rollback failure tests | 2 matrix tests, 8 injected boundaries passed |
| `cargo build --all` | passed |
| `.venv/bin/pytest ./tests -v` | 825 passed, 213 skipped in 117.03 s |
| Live RabbitMQ basic publish/subscribe | 1 passed in 2.84 s |
| ARMv7 workspace full build, excluding PyO3 | passed |
| `git diff --check` | passed after the report refresh |
| Palimnex `index --incremental` + `validate --deep` | 257 files; fresh; deep validation passed |

The live MQTT test used environment-only broker credentials and did not print or persist them in
tracked files. Deterministic Rust provider tests cover the existing AI adapters; no paid external
AI call was made. A diff scan found no local broker password in the product patch. Fixture-secret
assertions also prove that ciphertext, web responses, and audit logs omit their test values.

## Resource comparison

Stripped `ci` binaries were measured with `stat -c %s` after locked offline builds.

| Configuration | Phase 0 bytes | Phase 2 bytes | Delta from Phase 0 |
|---|---:|---:|---:|
| Default | 14,420,296 | 14,966,152 | +545,856 (+3.79%) |
| Root no-default | 14,259,480 | 14,719,832 | +460,352 (+3.23%) |
| Root `full` | 14,420,296 | 14,966,152 | +545,856 (+3.79%) |
| All features | 14,509,464 | 15,053,336 | +543,872 (+3.75%) |

The default Phase 2 result is +225,600 bytes (+1.53%) over the Phase 1.1 default of 14,740,552,
below the larger-of-5%-or-512-KiB per-phase budget. The identical default feature set built
without `credential_encryption` is 14,906,184 bytes, so the crypto feature itself contributes
59,968 bytes (+0.40%). Root no-default remains an imperfect minimal proxy for the reasons recorded
in Phase 0.

Warm copied-fixture measurements used the default stripped binary:

| Measurement | Phase 0 | Phase 2 | Delta |
|---|---:|---:|---:|
| Startup median, 5 samples | 7 ms | 16 ms (8-25 ms) | +9 ms |
| Idle RSS median, 5 samples | 13,404 KiB | 14,012 KiB (13,828-14,068 KiB) | +608 KiB (+4.54%) |
| Health median, 100 requests | 0.337 ms | 0.518 ms | +0.181 ms |
| Health p95, 100 requests | 0.466 ms | 0.918 ms | +0.452 ms |

These remain inside the Phase 0 resource budgets.

## Remaining platform limits

- Windows compiles this path only in CI; local Windows rename, permission, advisory-lock, and
  crash-recovery behavior was not executed.
- ARMv7 compilation passed, but ARM runtime encryption, filesystem crash behavior, RSS, and
  recovery were not executed on hardware or QEMU.
- Environment-injected key rotation is deliberately external; EdgeLinkd cannot back up a secret
  it does not own.
- The compatibility reader is intentionally transitional. Removing plaintext input requires a
  separately versioned migration decision after deployed installations have completed export and
  recovery drills.
- The post-migration editor WebSocket connected, but the captured terminal log does not by itself
  prove an MQTT message reached `debug 2` or an AI response reached `debug 3`; those remain browser
  observations unless separately recorded. Placeholder and clearing contracts are automated
  through the same web routes.
- Palimnex doctor remains `action_needed`: its coverage audit reports excluded code, and its
  manifest omits the three currently untracked Phase 2 Rust files despite matching include globs.
  The source tree and test results remain authoritative until those files are tracked and the
  index is refreshed.

## Git and release state

Phase 2 was prepared on top of `5a778f42eeb1fc890d9bd39aa94bd0cd110dca93` for publication on
`master`. The user's modified `edgelinkd.dev.toml`, live flow/credential files, and local agent
metadata remain outside the Phase 2 product scope. This phase does not create a tag, package
publication, release, or version change.
