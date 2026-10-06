# Credential Lifecycle Operations Manual

n2link stores Node-RED node credentials separately from `flows.json`. The current and rollback
generations are `flows_cred.json` and `flows_cred.json.prev`. Default builds can store both as
authenticated XChaCha20-Poly1305 envelopes while preserving Node-RED's `__PWRD__` placeholder,
deploy, and rollback behavior.

This procedure is explicit by design: startup reads plaintext sidecars but never migrates them.
All examples assume the repository or installation directory is the n2link home.

```bash
cd /path/to/n2linkd-home
export N2LINK_HOME="$PWD"
```

Do not run credential commands with `sudo`. Keep backups and exports outside the checkout, on
storage restricted to the service operator. Never commit a sidecar, keyring, plaintext export,
transaction journal, or backup directory.

## 1. Inspect without changing files

Build the binary, then inspect both generations:

```bash
cargo build
target/debug/n2linkd credentials status
```

The response reports each generation as `missing`, `plaintext`, `encrypted`, or `corrupt`, whether
it is decryptable, and the key source. It never returns credentials or key material. Stop if either
generation is corrupt or encrypted but not decryptable.

Preview migration:

```bash
target/debug/n2linkd credentials migrate --dry-run
```

Dry-run must not create `flows_cred.key`, a backup, or a transaction journal.

## 2. Migrate plaintext sidecars

Choose a new or empty backup directory outside the repository. The command refuses a non-empty
directory.

```bash
BACKUP="/secure/offline/n2link-credentials-$(date +%Y%m%d-%H%M%S)"
target/debug/n2linkd credentials migrate --backup-dir "$BACKUP"
```

With the default local-key provider, migration creates `flows_cred.key`. On Unix, sidecars and the
keyring must be mode `0600`; the backup directory must be mode `0700`.

```bash
target/debug/n2linkd credentials status
stat -c '%a %n' flows_cred.json flows_cred.json.prev flows_cred.key "$BACKUP"
```

Copy the generated keyring to protected offline storage. The migration backup contains the old
installation files but does not silently add a copy of a newly generated key.

```bash
install -m 600 flows_cred.key "$BACKUP/flows_cred.key"
```

Restart n2link and exercise one flow that uses a saved credential. For example, confirm an MQTT
publish/subscribe round trip or an AI-provider response. A clean startup alone proves that the
sidecars decrypt; it does not prove the external service accepted the credential.

## 3. Key providers

The default local keyring is beside the sidecar. These optional settings change key lookup:

```toml
[credentials]
# Relative paths are resolved beside flows.json.
key_file = "flows_cred.key"
key_env = "N2LINK_CREDENTIAL_KEY"
```

A non-empty environment value must be a 32-byte base64url key without padding. It takes precedence
over the local keyring. Invalid injected material fails closed and does not fall back to the file.
Avoid shell history and process-manager logs when provisioning it.

## 4. Rotate a local key

Rotation is only for a local keyring. The backup path must not exist. Both credential generations
must already be encrypted and decryptable.

```bash
target/debug/n2linkd credentials rotate \
  --backup-key /secure/offline/flows_cred.key.before-rotation
target/debug/n2linkd credentials status
```

After restart, re-run the credential-backed flow acceptance test. Keep the previous key until the
new generation and rollback generation have both been exercised. Environment-injected keys must
be rotated by their external secret provider.

## 5. Recover a lost or replaced local key

Test recovery against a copied installation first. `recover` proves the candidate keyring can
decrypt every encrypted generation before installing it.

```bash
target/debug/n2linkd credentials recover \
  --key-file /secure/offline/flows_cred.key
target/debug/n2linkd credentials status
```

If an injected key is present, remove it from the service environment before local-key recovery.
Do not replace `flows_cred.key` manually while n2link may be deploying.

## 6. Export for downgrade or disaster recovery

Older binaries cannot read encrypted envelopes. Export a new plaintext pair to protected storage:

```bash
target/debug/n2linkd credentials export \
  --output /secure/offline/export/flows_cred.json
```

The command also creates `flows_cred.json.prev`, refuses to overwrite either output, and writes
both as mode `0600` on Unix. These files contain plaintext secrets. Copy the entire installation to
a staging location, install the export there, and validate the older runtime before changing the
live installation. Retain the encrypted installation and key backup until downgrade validation is
complete.

## 7. Failure behavior and rollback

Credential operations use an advisory lock and a private transaction journal. An interrupted
migration or rotation is restored on the next credential operation or startup. Deploy and rollback
preserve the active sidecar format and update flows plus credentials as one write set.

All command failures return a non-zero exit code and print a concise message to stderr even when
logging has not initialized. Errors must not contain credential values, key bytes, ciphertext,
nonces, or tags.

If migration fails, leave the explicit backup untouched and run `credentials status` before any
manual recovery. If the new runtime cannot be accepted, use the tested export procedure; do not
replace an encrypted sidecar with a plaintext backup while a process is running.
