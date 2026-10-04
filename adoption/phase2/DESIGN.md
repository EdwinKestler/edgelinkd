# Phase 2 Encrypted Credential Storage Design

Status: implementation contract, 2026-10-03.

## Scope and compatibility

EdgeLinkd continues to use `flows_cred.json` and `flows_cred.json.prev`. The reader detects the
format from the file contents. During this compatibility release it accepts either the existing
plaintext JSON object or the encrypted envelope below. Startup never migrates or rewrites a
plaintext sidecar. A deploy preserves the current sidecar format: plaintext remains plaintext;
once explicitly migrated, subsequent credential generations remain encrypted.

The `credential_encryption` Cargo feature is enabled by the application default feature set and
can be removed from minimal builds. A build without it reads plaintext and rejects an encrypted
envelope clearly instead of treating it as node credentials.

## Envelope contract

The UTF-8 JSON envelope is:

```json
{
  "format": "edgelink-credentials",
  "version": 1,
  "algorithm": "XChaCha20-Poly1305",
  "keyId": "sha256:<32 lowercase hex characters>",
  "nonce": "<base64url without padding, 24 bytes>",
  "ciphertext": "<base64url without padding>",
  "tag": "<base64url without padding, 16 bytes>"
}
```

The plaintext is the canonical compact JSON encoding of the complete credential object. The
format, version, algorithm, and key identifier are authenticated as associated data. The nonce is
also bound by the AEAD operation. A fresh operating-system-backed random 192-bit nonce is generated
for every encryption. The key identifier is derived from the first 128 bits of SHA-256 over the
256-bit key; it selects a key and is not an authorization or secrecy boundary.

RustCrypto `chacha20poly1305` supplies the audited AEAD implementation. EdgeLinkd does not define a
cipher, nonce construction, tag, or KDF. Keys and decrypted byte buffers are held in zeroizing
containers where the library and data model permit it.

## Key provider and precedence

The provider interface returns an active key for encryption and resolves a key by identifier for
decryption. Phase 2 implements two providers:

1. An injected 32-byte base64url key from the environment variable named by
   `credentials.key_env` (default `EDGELINK_CREDENTIAL_KEY`).
2. A private versioned local keyring at `credentials.key_file`, or beside the flow file as
   `flows_cred.key` when unset.

If the configured environment variable exists and is non-empty, it is the only provider used. A
malformed key or key-identifier mismatch fails closed; EdgeLinkd does not silently fall back to the
local keyring. If the variable is absent, the local keyring is used. Missing key material is an
error only when encrypted data must be read or an encryption operation is requested.

Local key material is generated only by an explicit migration or rotation command, using 32 random
bytes. It is written atomically with mode `0600` on Unix. No key is generated during startup.
Hardware-backed providers are outside Phase 2, but the provider boundary does not expose file or
environment details to envelope code.

## Operations

`edgelinkd credentials` adds these explicit operations:

- `status`: report plaintext/encrypted/missing/corrupt state and decryptability without returning
  credentials, keys, nonces, tags, or ciphertext.
- `migrate --dry-run`: validate both generations and state what would change without writing a key,
  backup, sidecar, or journal.
- `migrate --backup-dir PATH`: create an explicit private backup of the four flow/credential files,
  then encrypt current and previous sidecars. It refuses a non-empty backup directory.
- `rotate --backup-key PATH`: prove both encrypted generations decrypt with the current local
  keyring, make the explicit private key backup, and atomically move both generations to a fresh
  key. Injected environment keys are rotated outside EdgeLinkd and are rejected by this command.
- `recover --key-file PATH`: validate a copied keyring against every encrypted generation before
  atomically installing it as the configured local keyring.
- `export --output PATH`: explicitly decrypt the live and previous sidecars to a directly
  compatible private pair at `PATH` and `PATH.prev`. It refuses to overwrite either output or
  write onto live/key paths.

Migration and rotation refuse unreadable, unauthenticated, unsupported, or structurally invalid
input. They never reinterpret corruption as an empty credential set.

## Transactions and concurrency

Deploy, rollback, migration, rotation, recovery, and export take an advisory cross-process lock
beside the credential sidecar. Web deploy serialization remains in place as the in-process first
gate.

Migration and rotation write a private, checksummed transaction journal before replacing any
destination. During migration, recovery entries reference and hash the administrator's explicit
backup instead of copying plaintext into the journal. During rotation, original sidecars are
already encrypted; their bytes and the private keyring can be journaled directly. Normal errors
restore originals and remove the journal. If the process stops between renames, the next
credential operation or runtime startup verifies the journal and restores the complete old
generation before reading credentials. A stop after all renames but before journal removal
therefore safely rolls back to the old generation. No automatic plaintext backup is left beside
an encrypted sidecar.

The existing deploy write-set remains: previous flows, previous credentials, live credentials,
then live flows. Encryption happens before the first rename. Failure injection at any rename
restores all already replaced destinations. Rollback decrypts and prepares the previous generation
before swapping raw encrypted bytes, so the live and `.prev` envelopes retain their exact nonces
and key identifiers.

## Failure and recovery behavior

- Missing or wrong keys: fail startup/read without modifying any file.
- Corrupt header, nonce, ciphertext, or tag: fail authentication/format validation without
  revealing which credential or byte failed.
- Unsupported version or algorithm: fail loudly and do not overwrite.
- Lost local key: copy a saved keyring to offline media, then use `recover --key-file` against a
  copied installation first. Installation occurs only after both generations validate.
- Downgrade: use `export --output` before installing the older binary, move the explicit export to
  the older installation as directed, and retain the encrypted installation plus key backup until
  validation completes.
- Failed migration/rotation: originals are restored byte-for-byte. The explicit backup remains
  because it belongs to the administrator, not to automatic rollback.

Errors, logs, web APIs, audit records, and status output may contain a format/state label and key
identifier, but never key bytes, environment values, plaintext, ciphertext, nonce, tag, or full
credential objects.

## Rollback boundary

Before migration, rollback is simply continuing to use plaintext. After migration, rollback to an
older binary requires the explicit export path; changing binaries without export is unsupported.
The encrypted files and offline key backup remain authoritative until the older runtime has loaded
and exercised the exported copied installation successfully.
