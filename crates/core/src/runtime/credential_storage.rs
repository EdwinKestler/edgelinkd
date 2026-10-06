//! Versioned authenticated storage for the Node-RED credential sidecar.
//!
//! Plaintext JSON remains readable during the compatibility window. Encryption, migration,
//! rotation, recovery, and export are explicit; runtime startup never rewrites a sidecar.

use std::path::{Path, PathBuf};

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

use crate::utils::atomic_file::{self, FileReplace};

const ENVELOPE_FORMAT: &str = "n2link-credentials";
#[cfg(feature = "credential_encryption")]
const ENVELOPE_VERSION: u8 = 1;
#[cfg(feature = "credential_encryption")]
const ENVELOPE_ALGORITHM: &str = "XChaCha20-Poly1305";
#[cfg(feature = "credential_encryption")]
const KEYRING_FORMAT: &str = "n2link-credential-keyring";
#[cfg(feature = "credential_encryption")]
const KEYRING_VERSION: u8 = 1;
const JOURNAL_FORMAT: &str = "n2link-credential-transaction";
/// Formats written by EdgeLinkd before the rename. n2link does not read them: the rename was a clean
/// break for encrypted credentials (adoption/rebrand/PLAN.md), so they fail with a clear message.
#[cfg_attr(not(feature = "credential_encryption"), allow(dead_code))]
const LEGACY_FORMAT_PREFIX: &str = "edgelink-credential";
#[cfg_attr(not(feature = "credential_encryption"), allow(dead_code))]
const LEGACY_FORMAT_ERROR: &str = "credentials were encrypted by EdgeLinkd, which n2link cannot read; \
     remove flows_cred.json and flows_cred.key, then re-enter the credentials";
const JOURNAL_VERSION: u8 = 1;
const DEFAULT_KEY_ENV: &str = "N2LINK_CREDENTIAL_KEY";

#[derive(Clone, Debug)]
pub struct CredentialStore {
    key_env: String,
    key_file: Option<PathBuf>,
}

impl Default for CredentialStore {
    fn default() -> Self {
        Self { key_env: DEFAULT_KEY_ENV.to_string(), key_file: None }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum SidecarFormat {
    Missing,
    Plaintext,
    Encrypted,
    Corrupt,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SidecarStatus {
    pub format: SidecarFormat,
    pub decryptable: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub key_id: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CredentialStatus {
    pub current: SidecarStatus,
    pub previous: SidecarStatus,
    pub key_source: &'static str,
    pub pending_transaction_recovered: bool,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MigrationResult {
    pub dry_run: bool,
    pub changed: bool,
    pub current: SidecarFormat,
    pub previous: SidecarFormat,
    pub key_source: &'static str,
    pub would_create_local_key: bool,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RotationResult {
    pub changed: bool,
    pub current_key_id: String,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ExportResult {
    pub current_output: PathBuf,
    pub previous_output: PathBuf,
}

#[cfg(feature = "credential_encryption")]
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Envelope {
    format: String,
    version: u8,
    algorithm: String,
    key_id: String,
    nonce: String,
    ciphertext: String,
    tag: String,
}

#[cfg(feature = "credential_encryption")]
#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Keyring {
    format: String,
    version: u8,
    active_key_id: String,
    keys: Vec<KeyEntry>,
}

#[cfg(feature = "credential_encryption")]
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct KeyEntry {
    id: String,
    key: String,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct TransactionJournal {
    format: String,
    version: u8,
    #[serde(skip_serializing_if = "Option::is_none")]
    backup_root: Option<String>,
    entries: Vec<TransactionEntry>,
    checksum: String,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct TransactionEntry {
    path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    original: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    original_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    original_sha256: Option<String>,
    candidate: String,
    private: bool,
}

impl CredentialStore {
    pub fn from_config(cfg: Option<&config::Config>) -> Result<Self, String> {
        let mut store = Self::default();
        if let Some(cfg) = cfg {
            if let Ok(value) = cfg.get_string("credentials.key_env") {
                let value = value.trim();
                if value.is_empty()
                    || !value.chars().all(|ch| ch == '_' || ch.is_ascii_alphanumeric())
                    || value.as_bytes()[0].is_ascii_digit()
                {
                    return Err("credential key environment variable name is invalid".to_string());
                }
                store.key_env = value.to_string();
            }
            if let Ok(value) = cfg.get_string("credentials.key_file") {
                let value = value.trim();
                if value.is_empty() {
                    return Err("credential key file path is empty".to_string());
                }
                store.key_file = Some(PathBuf::from(value));
            }
        }
        Ok(store)
    }

    pub fn key_file_path(&self, flows_file: &Path) -> PathBuf {
        match &self.key_file {
            Some(path) if path.is_absolute() => path.clone(),
            Some(path) => flows_file.parent().unwrap_or_else(|| Path::new(".")).join(path),
            None => credential_path(flows_file).with_extension("key"),
        }
    }

    pub async fn lock(&self, flows_file: &Path) -> Result<CredentialFileLock, String> {
        CredentialFileLock::acquire(&lock_path(flows_file)).await
    }

    /// Restore the complete old generation when a prior key/sidecar transaction was interrupted.
    pub async fn recover_pending(&self, flows_file: &Path) -> Result<bool, String> {
        let _lock = self.lock(flows_file).await?;
        self.recover_pending_unlocked(flows_file).await
    }

    async fn recover_pending_unlocked(&self, flows_file: &Path) -> Result<bool, String> {
        let path = journal_path(flows_file);
        if !path.exists() {
            return Ok(false);
        }
        let bytes = tokio::fs::read(&path).await.map_err(|_| "credential transaction journal cannot be read")?;
        let journal: TransactionJournal =
            serde_json::from_slice(&bytes).map_err(|_| "credential transaction journal is corrupt")?;
        validate_journal(&journal, flows_file, &self.key_file_path(flows_file))?;
        restore_entries(&journal.entries).await?;
        tokio::fs::remove_file(&path).await.map_err(|_| "credential transaction journal cannot be removed")?;
        Ok(true)
    }

    pub async fn read_sidecar(&self, flows_file: &Path) -> Result<Map<String, Value>, String> {
        let lock = self.lock(flows_file).await?;
        self.read_sidecar_while_locked(flows_file, &lock).await
    }

    pub async fn read_sidecar_while_locked(
        &self,
        flows_file: &Path,
        _lock: &CredentialFileLock,
    ) -> Result<Map<String, Value>, String> {
        self.recover_pending_unlocked(flows_file).await?;
        let path = credential_path(flows_file);
        if !path.exists() {
            return Ok(Map::new());
        }
        let bytes = tokio::fs::read(&path).await.map_err(|_| "credential file cannot be read")?;
        self.decode_bytes(flows_file, &bytes).await
    }

    pub async fn decode_bytes(&self, flows_file: &Path, bytes: &[u8]) -> Result<Map<String, Value>, String> {
        let parsed = parse_value(bytes)?;
        if is_envelope(&parsed) { self.decrypt_value(flows_file, parsed).await } else { object_from_value(parsed) }
    }

    /// Encode a credential generation without changing its storage format.
    pub async fn encode_for_write(
        &self,
        flows_file: &Path,
        stored: &Map<String, Value>,
        current_bytes: &[u8],
    ) -> Result<Vec<u8>, String> {
        let current = parse_value(current_bytes)?;
        if is_envelope(&current) {
            // Never let a valid candidate overwrite a sidecar that the configured key cannot
            // authenticate. This also protects callers that bypass `read_sidecar`.
            let _ = self.decrypt_value(flows_file, current).await?;
            let keys = self.resolve_keys(flows_file).await?;
            encrypt_map(stored, &keys)
        } else {
            serde_json::to_vec_pretty(stored).map_err(|_| "credential data cannot be encoded".to_string())
        }
    }

    pub async fn status(&self, flows_file: &Path) -> Result<CredentialStatus, String> {
        let _lock = self.lock(flows_file).await?;
        let recovered = self.recover_pending_unlocked(flows_file).await?;
        let source = self.key_source(flows_file).await;
        Ok(CredentialStatus {
            current: self.inspect_file(flows_file, &credential_path(flows_file)).await?,
            previous: self.inspect_file(flows_file, &previous_credential_path(flows_file)).await?,
            key_source: source,
            pending_transaction_recovered: recovered,
        })
    }

    pub async fn migrate(
        &self,
        flows_file: &Path,
        dry_run: bool,
        backup_dir: Option<&Path>,
    ) -> Result<MigrationResult, String> {
        let _lock = self.lock(flows_file).await?;
        self.recover_pending_unlocked(flows_file).await?;
        let current_path = credential_path(flows_file);
        let previous_path = previous_credential_path(flows_file);
        let current_bytes = read_or(&current_path, b"{}").await?;
        let previous_bytes = read_or(&previous_path, b"{}").await?;
        let current_value = parse_value(&current_bytes)?;
        let previous_value = parse_value(&previous_bytes)?;
        let current_format = format_of(&current_value);
        let previous_format = format_of(&previous_value);

        // Validate all existing material before planning any write.
        let current_map = self.decode_value(flows_file, current_value).await?;
        let previous_map = self.decode_value(flows_file, previous_value).await?;
        let changed = current_format != SidecarFormat::Encrypted || previous_format != SidecarFormat::Encrypted;
        let key_file = self.key_file_path(flows_file);
        let injected = self.injected_key()?;
        let would_create_local_key = injected.is_none() && !key_file.exists();
        let key_source = if injected.is_some() { "environment" } else { "local-keyring" };
        if dry_run {
            if !would_create_local_key {
                let _ = self.resolve_keys(flows_file).await?;
            }
            return Ok(MigrationResult {
                dry_run: true,
                changed,
                current: current_format,
                previous: previous_format,
                key_source,
                would_create_local_key,
            });
        }
        let backup_dir = backup_dir.ok_or_else(|| "migration requires --backup-dir".to_string())?;
        backup_installation(flows_file, &key_file, backup_dir).await?;
        if !changed {
            return Ok(MigrationResult {
                dry_run: false,
                changed: false,
                current: current_format,
                previous: previous_format,
                key_source,
                would_create_local_key: false,
            });
        }

        let (keys, new_keyring) = if let Some(key) = injected {
            (KeySet::injected(key), None)
        } else if key_file.exists() {
            (read_keyring(&key_file).await?, None)
        } else {
            let keys = KeySet::generated()?;
            let bytes = keys.keyring_bytes()?;
            (keys, Some(bytes))
        };
        let encrypted_current = encrypt_map(&current_map, &keys)?;
        let encrypted_previous = encrypt_map(&previous_map, &keys)?;
        let mut files = vec![
            FileReplace { path: current_path, bytes: encrypted_current, private: true },
            FileReplace { path: previous_path, bytes: encrypted_previous, private: true },
        ];
        if let Some(bytes) = new_keyring {
            files.push(FileReplace { path: key_file, bytes, private: true });
        }
        self.transactional_replace(flows_file, &files, Some(backup_dir), None).await?;
        Ok(MigrationResult {
            dry_run: false,
            changed: true,
            current: current_format,
            previous: previous_format,
            key_source,
            would_create_local_key,
        })
    }

    pub async fn rotate(&self, flows_file: &Path, backup_key: &Path) -> Result<RotationResult, String> {
        let _lock = self.lock(flows_file).await?;
        self.recover_pending_unlocked(flows_file).await?;
        if self.injected_key()?.is_some() {
            return Err("injected credential keys must be rotated by the secret provider".to_string());
        }
        let key_file = self.key_file_path(flows_file);
        let old_keyring_bytes = tokio::fs::read(&key_file).await.map_err(|_| "local credential keyring is missing")?;
        let old_keys = KeySet::from_keyring_bytes(&old_keyring_bytes)?;
        let current_path = credential_path(flows_file);
        let previous_path = previous_credential_path(flows_file);
        let current_bytes = read_or(&current_path, b"{}").await?;
        let previous_bytes = read_or(&previous_path, b"{}").await?;
        if format_of(&parse_value(&current_bytes)?) != SidecarFormat::Encrypted
            || format_of(&parse_value(&previous_bytes)?) != SidecarFormat::Encrypted
        {
            return Err("credential rotation requires encrypted current and previous sidecars".to_string());
        }
        let current = decrypt_with_keys(parse_value(&current_bytes)?, &old_keys)?;
        let previous = decrypt_with_keys(parse_value(&previous_bytes)?, &old_keys)?;
        write_new_private(backup_key, &old_keyring_bytes).await?;
        let new_keys = KeySet::generated()?;
        let key_id = new_keys.active_id().to_string();
        let files = [
            FileReplace { path: current_path, bytes: encrypt_map(&current, &new_keys)?, private: true },
            FileReplace { path: previous_path, bytes: encrypt_map(&previous, &new_keys)?, private: true },
            FileReplace { path: key_file, bytes: new_keys.keyring_bytes()?, private: true },
        ];
        self.transactional_replace(flows_file, &files, None, None).await?;
        Ok(RotationResult { changed: true, current_key_id: key_id })
    }

    pub async fn recover_key(&self, flows_file: &Path, source: &Path) -> Result<String, String> {
        let _lock = self.lock(flows_file).await?;
        self.recover_pending_unlocked(flows_file).await?;
        if self.injected_key()?.is_some() {
            return Err("remove the injected credential key before local-key recovery".to_string());
        }
        let bytes = tokio::fs::read(source).await.map_err(|_| "recovery keyring cannot be read")?;
        let keys = KeySet::from_keyring_bytes(&bytes)?;
        for path in [credential_path(flows_file), previous_credential_path(flows_file)] {
            if path.exists() {
                let value = parse_value(&tokio::fs::read(&path).await.map_err(|_| "credential file cannot be read")?)?;
                if is_envelope(&value) {
                    let _ = decrypt_with_keys(value, &keys)?;
                }
            }
        }
        let active = keys.active_id().to_string();
        atomic_file::write_bytes(&self.key_file_path(flows_file), &bytes, true).await?;
        Ok(active)
    }

    pub async fn export(&self, flows_file: &Path, output: &Path) -> Result<ExportResult, String> {
        let _lock = self.lock(flows_file).await?;
        self.recover_pending_unlocked(flows_file).await?;
        let previous_output = previous_export_path(output);
        let forbidden = [
            flows_file.to_path_buf(),
            credential_path(flows_file),
            previous_credential_path(flows_file),
            self.key_file_path(flows_file),
        ];
        if forbidden.iter().any(|path| path == output || path == &previous_output) {
            return Err("credential export path conflicts with a live installation file".to_string());
        }
        let current = self.decode_path(flows_file, &credential_path(flows_file)).await?;
        let previous = self.decode_path(flows_file, &previous_credential_path(flows_file)).await?;
        let current =
            serde_json::to_vec_pretty(&current).map_err(|_| "credential export cannot be encoded".to_string())?;
        let previous =
            serde_json::to_vec_pretty(&previous).map_err(|_| "credential export cannot be encoded".to_string())?;
        if output.exists() || previous_output.exists() {
            return Err("credential export output already exists".to_string());
        }
        write_new_private(output, &current).await?;
        if let Err(err) = write_new_private(&previous_output, &previous).await {
            let _ = tokio::fs::remove_file(output).await;
            return Err(err);
        }
        Ok(ExportResult { current_output: output.to_path_buf(), previous_output })
    }

    async fn inspect_file(&self, flows_file: &Path, path: &Path) -> Result<SidecarStatus, String> {
        if !path.exists() {
            return Ok(SidecarStatus { format: SidecarFormat::Missing, decryptable: true, key_id: None });
        }
        let bytes = tokio::fs::read(path).await.map_err(|_| "credential file cannot be read")?;
        let value = match parse_value(&bytes) {
            Ok(value) => value,
            Err(_) => return Ok(SidecarStatus { format: SidecarFormat::Corrupt, decryptable: false, key_id: None }),
        };
        let format = format_of(&value);
        let key_id = if format == SidecarFormat::Encrypted {
            value.get("keyId").and_then(Value::as_str).map(str::to_string)
        } else {
            None
        };
        let decryptable = self.decode_value(flows_file, value).await.is_ok();
        Ok(SidecarStatus { format, decryptable, key_id })
    }

    async fn decode_path(&self, flows_file: &Path, path: &Path) -> Result<Map<String, Value>, String> {
        if !path.exists() {
            return Ok(Map::new());
        }
        let bytes = tokio::fs::read(path).await.map_err(|_| "credential file cannot be read")?;
        self.decode_bytes(flows_file, &bytes).await
    }

    async fn decode_value(&self, flows_file: &Path, value: Value) -> Result<Map<String, Value>, String> {
        if is_envelope(&value) { self.decrypt_value(flows_file, value).await } else { object_from_value(value) }
    }

    async fn decrypt_value(&self, flows_file: &Path, value: Value) -> Result<Map<String, Value>, String> {
        #[cfg(feature = "credential_encryption")]
        {
            let keys = self.resolve_keys(flows_file).await?;
            decrypt_with_keys(value, &keys)
        }
        #[cfg(not(feature = "credential_encryption"))]
        {
            let _ = (flows_file, value);
            Err("encrypted credential storage is not available in this build".to_string())
        }
    }

    #[cfg(feature = "credential_encryption")]
    async fn resolve_keys(&self, flows_file: &Path) -> Result<KeySet, String> {
        if let Some(key) = self.injected_key()? {
            Ok(KeySet::injected(key))
        } else {
            read_keyring(&self.key_file_path(flows_file)).await
        }
    }

    #[cfg(not(feature = "credential_encryption"))]
    async fn resolve_keys(&self, _flows_file: &Path) -> Result<KeySet, String> {
        Err("encrypted credential storage is not available in this build".to_string())
    }

    #[cfg(feature = "credential_encryption")]
    fn injected_key(&self) -> Result<Option<[u8; 32]>, String> {
        let Some(raw) = crate::compat::named_env_var_os(&self.key_env)? else {
            return Ok(None);
        };
        let raw = raw.into_string().map_err(|_| "injected credential key is not UTF-8")?;
        if raw.trim().is_empty() {
            return Ok(None);
        }
        decode_key(raw.trim()).map(Some)
    }

    #[cfg(not(feature = "credential_encryption"))]
    fn injected_key(&self) -> Result<Option<[u8; 32]>, String> {
        Ok(None)
    }

    async fn key_source(&self, flows_file: &Path) -> &'static str {
        match self.injected_key() {
            Ok(Some(_)) => "environment",
            Ok(None) if self.key_file_path(flows_file).exists() => "local-keyring",
            _ => "missing",
        }
    }

    async fn transactional_replace(
        &self,
        flows_file: &Path,
        files: &[FileReplace],
        recovery_backup: Option<&Path>,
        fail_before: Option<usize>,
    ) -> Result<(), String> {
        let journal = build_journal(files, recovery_backup).await?;
        let journal_bytes = serde_json::to_vec_pretty(&journal)
            .map_err(|_| "credential transaction journal cannot be encoded".to_string())?;
        let path = journal_path(flows_file);
        atomic_file::write_bytes(&path, &journal_bytes, true).await?;
        let result = match fail_before {
            Some(index) => atomic_file::replace_files_failing_before(files, index).await,
            None => atomic_file::replace_files(files).await,
        };
        if let Err(err) = result {
            let recovery = self.recover_pending_unlocked(flows_file).await;
            return match recovery {
                Ok(_) => Err(err),
                Err(recovery) => Err(format!("credential transaction failed and recovery failed: {recovery}")),
            };
        }
        tokio::fs::remove_file(path).await.map_err(|_| "credential transaction journal cannot be removed")?;
        Ok(())
    }
}

fn credential_path(flows_file: &Path) -> PathBuf {
    super::flow_credentials::sidecar_path(flows_file)
}

pub fn previous_credential_path(flows_file: &Path) -> PathBuf {
    let path = credential_path(flows_file);
    let name = path.file_name().and_then(|name| name.to_str()).unwrap_or("flows_cred.json");
    path.with_file_name(format!("{name}.prev"))
}

fn journal_path(flows_file: &Path) -> PathBuf {
    let path = credential_path(flows_file);
    let name = path.file_name().and_then(|name| name.to_str()).unwrap_or("flows_cred.json");
    path.with_file_name(format!("{name}.txn"))
}

fn lock_path(flows_file: &Path) -> PathBuf {
    let path = credential_path(flows_file);
    let name = path.file_name().and_then(|name| name.to_str()).unwrap_or("flows_cred.json");
    path.with_file_name(format!("{name}.lock"))
}

fn previous_export_path(output: &Path) -> PathBuf {
    let name = output.file_name().and_then(|name| name.to_str()).unwrap_or("flows_cred.json");
    output.with_file_name(format!("{name}.prev"))
}

fn parse_value(bytes: &[u8]) -> Result<Value, String> {
    if bytes.iter().all(u8::is_ascii_whitespace) {
        return Ok(Value::Object(Map::new()));
    }
    serde_json::from_slice(bytes).map_err(|_| "credential file is not valid JSON".to_string())
}

fn object_from_value(value: Value) -> Result<Map<String, Value>, String> {
    match value {
        Value::Object(map) => Ok(map),
        _ => Err("credential file is not an object".to_string()),
    }
}

fn is_envelope(value: &Value) -> bool {
    let Some(object) = value.as_object() else {
        return false;
    };
    object.get("format").and_then(Value::as_str) == Some(ENVELOPE_FORMAT)
        || ["algorithm", "keyId", "nonce", "ciphertext", "tag"].iter().all(|field| object.contains_key(*field))
}

fn format_of(value: &Value) -> SidecarFormat {
    if is_envelope(value) {
        SidecarFormat::Encrypted
    } else if value.is_object() {
        SidecarFormat::Plaintext
    } else {
        SidecarFormat::Corrupt
    }
}

#[cfg(feature = "credential_encryption")]
#[derive(Clone)]
struct KeySet {
    active: String,
    keys: Vec<(String, zeroize::Zeroizing<[u8; 32]>)>,
    keyring: Option<Keyring>,
}

#[cfg(not(feature = "credential_encryption"))]
struct KeySet;

#[cfg(not(feature = "credential_encryption"))]
impl KeySet {
    fn injected(_key: [u8; 32]) -> Self {
        Self
    }

    fn generated() -> Result<Self, String> {
        Err("encrypted credential storage is not available in this build".to_string())
    }

    fn from_keyring_bytes(_bytes: &[u8]) -> Result<Self, String> {
        Err("encrypted credential storage is not available in this build".to_string())
    }

    fn active_id(&self) -> &str {
        "unavailable"
    }

    fn keyring_bytes(&self) -> Result<Vec<u8>, String> {
        Err("encrypted credential storage is not available in this build".to_string())
    }
}

#[cfg(feature = "credential_encryption")]
impl KeySet {
    fn injected(key: [u8; 32]) -> Self {
        let id = key_id(&key);
        Self { active: id.clone(), keys: vec![(id, zeroize::Zeroizing::new(key))], keyring: None }
    }

    fn generated() -> Result<Self, String> {
        let key = rand::random::<[u8; 32]>();
        let id = key_id(&key);
        let keyring = Keyring {
            format: KEYRING_FORMAT.to_string(),
            version: KEYRING_VERSION,
            active_key_id: id.clone(),
            keys: vec![KeyEntry { id: id.clone(), key: URL_SAFE_NO_PAD.encode(key) }],
        };
        Ok(Self { active: id.clone(), keys: vec![(id, zeroize::Zeroizing::new(key))], keyring: Some(keyring) })
    }

    fn from_keyring_bytes(bytes: &[u8]) -> Result<Self, String> {
        let keyring: Keyring = serde_json::from_slice(bytes).map_err(|_| "credential keyring is not valid JSON")?;
        if keyring.format.starts_with(LEGACY_FORMAT_PREFIX) {
            return Err(LEGACY_FORMAT_ERROR.to_string());
        }
        if keyring.format != KEYRING_FORMAT || keyring.version != KEYRING_VERSION || keyring.keys.is_empty() {
            return Err("credential keyring format is not supported".to_string());
        }
        let mut keys = Vec::with_capacity(keyring.keys.len());
        for entry in &keyring.keys {
            let key = decode_key(&entry.key)?;
            if key_id(&key) != entry.id {
                return Err("credential keyring integrity check failed".to_string());
            }
            keys.push((entry.id.clone(), zeroize::Zeroizing::new(key)));
        }
        if !keys.iter().any(|(id, _)| id == &keyring.active_key_id) {
            return Err("credential keyring active key is missing".to_string());
        }
        Ok(Self { active: keyring.active_key_id.clone(), keys, keyring: Some(keyring) })
    }

    fn active_id(&self) -> &str {
        &self.active
    }

    fn active_key(&self) -> Result<&[u8; 32], String> {
        self.key(&self.active)
    }

    fn key(&self, id: &str) -> Result<&[u8; 32], String> {
        self.keys
            .iter()
            .find(|(candidate, _)| candidate == id)
            .map(|(_, key)| &**key)
            .ok_or_else(|| "credential key is unavailable".to_string())
    }

    fn keyring_bytes(&self) -> Result<Vec<u8>, String> {
        let keyring = self.keyring.as_ref().ok_or_else(|| "injected keys do not have a local keyring".to_string())?;
        serde_json::to_vec_pretty(keyring).map_err(|_| "credential keyring cannot be encoded".to_string())
    }
}

#[cfg(feature = "credential_encryption")]
fn decode_key(raw: &str) -> Result<[u8; 32], String> {
    let bytes = URL_SAFE_NO_PAD.decode(raw).map_err(|_| "credential key is not valid base64url")?;
    bytes.try_into().map_err(|_| "credential key must contain exactly 32 bytes".to_string())
}

#[cfg(feature = "credential_encryption")]
fn key_id(key: &[u8; 32]) -> String {
    let digest = Sha256::digest(key);
    format!("sha256:{}", hex::encode(&digest[..16]))
}

#[cfg(feature = "credential_encryption")]
fn aad(key_id: &str) -> Vec<u8> {
    format!("{ENVELOPE_FORMAT}\0{ENVELOPE_VERSION}\0{ENVELOPE_ALGORITHM}\0{key_id}").into_bytes()
}

#[cfg(feature = "credential_encryption")]
fn encrypt_map(stored: &Map<String, Value>, keys: &KeySet) -> Result<Vec<u8>, String> {
    use chacha20poly1305::aead::{Aead, KeyInit, Payload};
    use chacha20poly1305::{Key, XChaCha20Poly1305, XNonce};
    use zeroize::Zeroizing;

    let plaintext =
        Zeroizing::new(serde_json::to_vec(stored).map_err(|_| "credential data cannot be encoded".to_string())?);
    let nonce = rand::random::<[u8; 24]>();
    let key = Key::from(*keys.active_key()?);
    let cipher = XChaCha20Poly1305::new(&key);
    let nonce = XNonce::from(nonce);
    let mut combined = cipher
        .encrypt(&nonce, Payload { msg: &plaintext, aad: &aad(keys.active_id()) })
        .map_err(|_| "credential encryption failed".to_string())?;
    if combined.len() < 16 {
        return Err("credential encryption failed".to_string());
    }
    let tag = combined.split_off(combined.len() - 16);
    let envelope = Envelope {
        format: ENVELOPE_FORMAT.to_string(),
        version: ENVELOPE_VERSION,
        algorithm: ENVELOPE_ALGORITHM.to_string(),
        key_id: keys.active_id().to_string(),
        nonce: URL_SAFE_NO_PAD.encode(nonce),
        ciphertext: URL_SAFE_NO_PAD.encode(combined),
        tag: URL_SAFE_NO_PAD.encode(tag),
    };
    serde_json::to_vec_pretty(&envelope).map_err(|_| "credential envelope cannot be encoded".to_string())
}

#[cfg(not(feature = "credential_encryption"))]
fn encrypt_map(_stored: &Map<String, Value>, _keys: &KeySet) -> Result<Vec<u8>, String> {
    Err("encrypted credential storage is not available in this build".to_string())
}

#[cfg(feature = "credential_encryption")]
fn decrypt_with_keys(value: Value, keys: &KeySet) -> Result<Map<String, Value>, String> {
    use chacha20poly1305::aead::{Aead, KeyInit, Payload};
    use chacha20poly1305::{Key, XChaCha20Poly1305, XNonce};
    use zeroize::Zeroizing;

    let envelope: Envelope = serde_json::from_value(value).map_err(|_| "credential envelope is invalid")?;
    if envelope.format.starts_with(LEGACY_FORMAT_PREFIX) {
        return Err(LEGACY_FORMAT_ERROR.to_string());
    }
    if envelope.format != ENVELOPE_FORMAT || envelope.version != ENVELOPE_VERSION {
        return Err("credential envelope version is not supported".to_string());
    }
    if envelope.algorithm != ENVELOPE_ALGORITHM {
        return Err("credential envelope algorithm is not supported".to_string());
    }
    let nonce = URL_SAFE_NO_PAD.decode(&envelope.nonce).map_err(|_| "credential envelope nonce is invalid")?;
    let nonce: [u8; 24] = nonce.try_into().map_err(|_| "credential envelope nonce is invalid")?;
    let mut ciphertext =
        URL_SAFE_NO_PAD.decode(&envelope.ciphertext).map_err(|_| "credential envelope ciphertext is invalid")?;
    let tag = URL_SAFE_NO_PAD.decode(&envelope.tag).map_err(|_| "credential envelope tag is invalid")?;
    if tag.len() != 16 {
        return Err("credential envelope tag is invalid".to_string());
    }
    ciphertext.extend_from_slice(&tag);
    let key = Key::from(*keys.key(&envelope.key_id)?);
    let cipher = XChaCha20Poly1305::new(&key);
    let nonce = XNonce::from(nonce);
    let plaintext = Zeroizing::new(
        cipher
            .decrypt(&nonce, Payload { msg: &ciphertext, aad: &aad(&envelope.key_id) })
            .map_err(|_| "credential authentication failed".to_string())?,
    );
    let value: Value = serde_json::from_slice(&plaintext).map_err(|_| "decrypted credential data is invalid")?;
    object_from_value(value)
}

#[cfg(not(feature = "credential_encryption"))]
fn decrypt_with_keys(_value: Value, _keys: &KeySet) -> Result<Map<String, Value>, String> {
    Err("encrypted credential storage is not available in this build".to_string())
}

#[cfg(feature = "credential_encryption")]
async fn read_keyring(path: &Path) -> Result<KeySet, String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode =
            tokio::fs::metadata(path).await.map_err(|_| "local credential keyring is missing")?.permissions().mode()
                & 0o777;
        if mode != 0o600 {
            return Err("local credential keyring permissions must be 0600".to_string());
        }
    }
    let bytes = tokio::fs::read(path).await.map_err(|_| "local credential keyring is missing")?;
    KeySet::from_keyring_bytes(&bytes)
}

#[cfg(not(feature = "credential_encryption"))]
async fn read_keyring(_path: &Path) -> Result<KeySet, String> {
    Err("encrypted credential storage is not available in this build".to_string())
}

async fn read_or(path: &Path, empty: &[u8]) -> Result<Vec<u8>, String> {
    if path.exists() {
        tokio::fs::read(path).await.map_err(|_| "credential file cannot be read".to_string())
    } else {
        Ok(empty.to_vec())
    }
}

async fn build_journal(files: &[FileReplace], recovery_backup: Option<&Path>) -> Result<TransactionJournal, String> {
    let backup_root = match recovery_backup {
        Some(path) => {
            Some(tokio::fs::canonicalize(path).await.map_err(|_| "credential recovery backup cannot be resolved")?)
        }
        None => None,
    };
    let mut entries = Vec::with_capacity(files.len());
    for file in files {
        let (original, original_path, original_sha256) = if file.path.exists() {
            let bytes = tokio::fs::read(&file.path).await.map_err(|_| "credential transaction input cannot be read")?;
            if let Some(root) = &backup_root {
                let name =
                    file.path.file_name().ok_or_else(|| "credential transaction path has no name".to_string())?;
                let source = root.join(name);
                let backup = tokio::fs::read(&source).await.map_err(|_| "credential recovery backup is incomplete")?;
                if backup != bytes {
                    return Err("credential recovery backup does not match the live input".to_string());
                }
                (None, Some(source.to_string_lossy().into_owned()), Some(hex::encode(Sha256::digest(&bytes))))
            } else {
                (Some(URL_SAFE_NO_PAD.encode(bytes)), None, None)
            }
        } else {
            (None, None, None)
        };
        entries.push(TransactionEntry {
            path: file.path.to_string_lossy().into_owned(),
            original,
            original_path,
            original_sha256,
            candidate: URL_SAFE_NO_PAD.encode(&file.bytes),
            private: file.private,
        });
    }
    let backup_root = backup_root.map(|path| path.to_string_lossy().into_owned());
    let checksum = journal_checksum(backup_root.as_deref(), &entries)?;
    Ok(TransactionJournal {
        format: JOURNAL_FORMAT.to_string(),
        version: JOURNAL_VERSION,
        backup_root,
        entries,
        checksum,
    })
}

fn journal_checksum(backup_root: Option<&str>, entries: &[TransactionEntry]) -> Result<String, String> {
    let bytes =
        serde_json::to_vec(&(backup_root, entries)).map_err(|_| "credential transaction journal cannot be encoded")?;
    Ok(hex::encode(Sha256::digest(bytes)))
}

fn validate_journal(journal: &TransactionJournal, flows_file: &Path, key_file: &Path) -> Result<(), String> {
    if journal.format != JOURNAL_FORMAT
        || journal.version != JOURNAL_VERSION
        || journal.entries.is_empty()
        || journal.checksum != journal_checksum(journal.backup_root.as_deref(), &journal.entries)?
    {
        return Err("credential transaction journal integrity check failed".to_string());
    }
    let allowed = [credential_path(flows_file), previous_credential_path(flows_file), key_file.to_path_buf()];
    for entry in &journal.entries {
        if !allowed.iter().any(|path| path == Path::new(&entry.path)) {
            return Err("credential transaction journal contains an invalid path".to_string());
        }
        let _ = URL_SAFE_NO_PAD
            .decode(&entry.candidate)
            .map_err(|_| "credential transaction journal is corrupt".to_string())?;
        if let Some(original) = &entry.original {
            let _ = URL_SAFE_NO_PAD
                .decode(original)
                .map_err(|_| "credential transaction journal is corrupt".to_string())?;
        }
        if entry.original.is_some() && entry.original_path.is_some() {
            return Err("credential transaction journal has conflicting recovery sources".to_string());
        }
        if let Some(original_path) = &entry.original_path {
            let root = journal
                .backup_root
                .as_deref()
                .ok_or_else(|| "credential transaction journal has no backup root".to_string())?;
            let original_path = Path::new(original_path);
            if original_path.parent() != Some(Path::new(root))
                || original_path.file_name() != Path::new(&entry.path).file_name()
                || entry.original_sha256.as_deref().is_none_or(|hash| hash.len() != 64)
            {
                return Err("credential transaction journal contains an invalid recovery path".to_string());
            }
        } else if entry.original_sha256.is_some() {
            return Err("credential transaction journal has an orphaned checksum".to_string());
        }
    }
    Ok(())
}

async fn restore_entries(entries: &[TransactionEntry]) -> Result<(), String> {
    let mut files = Vec::new();
    let mut missing = Vec::new();
    for entry in entries {
        match (&entry.original, &entry.original_path) {
            (Some(original), None) => files.push(FileReplace {
                path: PathBuf::from(&entry.path),
                bytes: URL_SAFE_NO_PAD
                    .decode(original)
                    .map_err(|_| "credential transaction journal is corrupt".to_string())?,
                private: entry.private,
            }),
            (None, Some(original_path)) => {
                let bytes =
                    tokio::fs::read(original_path).await.map_err(|_| "credential recovery backup cannot be read")?;
                if Some(hex::encode(Sha256::digest(&bytes)).as_str()) != entry.original_sha256.as_deref() {
                    return Err("credential recovery backup integrity check failed".to_string());
                }
                files.push(FileReplace { path: PathBuf::from(&entry.path), bytes, private: entry.private });
            }
            (None, None) => missing.push(PathBuf::from(&entry.path)),
            (Some(_), Some(_)) => return Err("credential transaction journal is corrupt".to_string()),
        }
    }
    atomic_file::replace_files(&files).await?;
    for path in missing {
        match tokio::fs::remove_file(path).await {
            Ok(()) => {}
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => return Err("credential transaction recovery could not remove a new file".to_string()),
        }
    }
    Ok(())
}

async fn backup_installation(flows_file: &Path, key_file: &Path, backup_dir: &Path) -> Result<(), String> {
    if backup_dir.exists() {
        let mut entries = tokio::fs::read_dir(backup_dir).await.map_err(|_| "backup directory cannot be read")?;
        if entries.next_entry().await.map_err(|_| "backup directory cannot be read")?.is_some() {
            return Err("backup directory is not empty".to_string());
        }
    } else {
        tokio::fs::create_dir_all(backup_dir).await.map_err(|_| "backup directory cannot be created")?;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        tokio::fs::set_permissions(backup_dir, std::fs::Permissions::from_mode(0o700))
            .await
            .map_err(|_| "backup directory permissions cannot be set")?;
    }
    let paths = [
        (flows_file.to_path_buf(), false),
        (credential_path(flows_file), true),
        (previous_flow_path(flows_file), false),
        (previous_credential_path(flows_file), true),
        (key_file.to_path_buf(), true),
    ];
    for (source, private) in paths {
        if !source.exists() {
            continue;
        }
        let name = source.file_name().ok_or_else(|| "backup source has no file name".to_string())?;
        let destination = backup_dir.join(name);
        let bytes = tokio::fs::read(&source).await.map_err(|_| "backup source cannot be read")?;
        write_new(&destination, &bytes, private).await?;
    }
    Ok(())
}

fn previous_flow_path(flows_file: &Path) -> PathBuf {
    let name = flows_file.file_name().and_then(|name| name.to_str()).unwrap_or("flows.json");
    flows_file.with_file_name(format!("{name}.prev"))
}

async fn write_new_private(path: &Path, bytes: &[u8]) -> Result<(), String> {
    write_new(path, bytes, true).await
}

async fn write_new(path: &Path, bytes: &[u8], private: bool) -> Result<(), String> {
    use tokio::io::AsyncWriteExt;
    let mut options = tokio::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    if private {
        options.mode(0o600);
    }
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await.map_err(|_| "output directory cannot be created")?;
    }
    let mut file = options.open(path).await.map_err(|_| "output file already exists or cannot be created")?;
    file.write_all(bytes).await.map_err(|_| "output file cannot be written")?;
    file.flush().await.map_err(|_| "output file cannot be flushed")?;
    file.sync_all().await.map_err(|_| "output file cannot be synchronized")?;
    Ok(())
}

pub struct CredentialFileLock {
    #[cfg(feature = "credential_encryption")]
    file: std::fs::File,
}

impl CredentialFileLock {
    async fn acquire(path: &Path) -> Result<Self, String> {
        #[cfg(feature = "credential_encryption")]
        {
            let path = path.to_path_buf();
            tokio::task::spawn_blocking(move || {
                let mut options = std::fs::OpenOptions::new();
                options.read(true).write(true).create(true);
                #[cfg(unix)]
                {
                    use std::os::unix::fs::OpenOptionsExt;
                    options.mode(0o600);
                }
                let file = options.open(path).map_err(|_| "credential lock file cannot be opened")?;
                fs2::FileExt::lock_exclusive(&file).map_err(|_| "credential lock cannot be acquired")?;
                Ok(Self { file })
            })
            .await
            .map_err(|_| "credential lock task failed".to_string())?
        }
        #[cfg(not(feature = "credential_encryption"))]
        {
            let _ = path;
            Ok(Self {})
        }
    }
}

#[cfg(feature = "credential_encryption")]
impl Drop for CredentialFileLock {
    fn drop(&mut self) {
        let _ = fs2::FileExt::unlock(&self.file);
    }
}

#[cfg(test)]
mod tests {
    #[cfg(feature = "credential_encryption")]
    use super::*;
    #[cfg(feature = "credential_encryption")]
    use serde_json::json;

    #[cfg(feature = "credential_encryption")]
    struct TempDir(PathBuf);

    #[cfg(feature = "credential_encryption")]
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[cfg(feature = "credential_encryption")]
    fn temp() -> TempDir {
        let path = std::env::temp_dir().join(format!("n2link-credential-storage-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&path).unwrap();
        TempDir(path)
    }

    #[cfg(feature = "credential_encryption")]
    #[test]
    fn encryption_round_trips_and_uses_unique_nonces() {
        let keys = KeySet::generated().unwrap();
        let stored = json!({"node":{"password":"fixture-secret"}}).as_object().unwrap().clone();
        let first = encrypt_map(&stored, &keys).unwrap();
        let second = encrypt_map(&stored, &keys).unwrap();
        assert_ne!(first, second);
        assert_eq!(decrypt_with_keys(parse_value(&first).unwrap(), &keys).unwrap(), stored);
        assert_eq!(decrypt_with_keys(parse_value(&second).unwrap(), &keys).unwrap(), stored);
        assert!(!String::from_utf8(first).unwrap().contains("fixture-secret"));
    }

    #[cfg(feature = "credential_encryption")]
    #[test]
    fn randomized_credential_maps_round_trip() {
        let keys = KeySet::generated().unwrap();
        for _ in 0..64 {
            let node_id = hex::encode(rand::random::<[u8; 8]>());
            let value = hex::encode(rand::random::<[u8; 32]>());
            let stored = json!({(node_id):{"password":value}}).as_object().unwrap().clone();
            let encrypted = encrypt_map(&stored, &keys).unwrap();
            assert_eq!(decrypt_with_keys(parse_value(&encrypted).unwrap(), &keys).unwrap(), stored);
        }
    }

    #[cfg(feature = "credential_encryption")]
    #[test]
    fn wrong_key_and_corrupt_tag_fail_authentication() {
        let keys = KeySet::generated().unwrap();
        let other = KeySet::generated().unwrap();
        let stored = json!({"node":{"password":"fixture-secret"}}).as_object().unwrap().clone();
        let bytes = encrypt_map(&stored, &keys).unwrap();
        let value = parse_value(&bytes).unwrap();
        assert!(decrypt_with_keys(value.clone(), &other).is_err());
        let mut envelope: Envelope = serde_json::from_value(value).unwrap();
        envelope.tag.replace_range(..2, "AA");
        let err = decrypt_with_keys(serde_json::to_value(envelope).unwrap(), &keys).unwrap_err();
        assert_eq!(err, "credential authentication failed");
        assert!(!err.contains("fixture-secret"));
    }

    #[cfg(feature = "credential_encryption")]
    #[test]
    fn edgelinkd_credentials_and_keyrings_fail_loudly() {
        let keys = KeySet::generated().unwrap();
        let stored = json!({"node":{"password":"fixture-secret"}}).as_object().unwrap().clone();
        let value = parse_value(&encrypt_map(&stored, &keys).unwrap()).unwrap();
        let mut envelope: Envelope = serde_json::from_value(value).unwrap();
        envelope.format = "edgelink-credentials".to_string();
        let legacy = serde_json::to_value(envelope).unwrap();
        assert!(matches!(format_of(&legacy), SidecarFormat::Encrypted));
        let err = decrypt_with_keys(legacy, &keys).unwrap_err();
        assert!(err.contains("n2link"), "{err}");
        assert!(!err.contains("fixture-secret"));

        let keyring = String::from_utf8(keys.keyring_bytes().unwrap()).unwrap();
        let legacy_keyring = keyring.replace(KEYRING_FORMAT, "edgelink-credential-keyring");
        assert_ne!(keyring, legacy_keyring);
        let err = KeySet::from_keyring_bytes(legacy_keyring.as_bytes()).err().unwrap();
        assert!(err.contains("n2link"), "{err}");
    }

    #[cfg(feature = "credential_encryption")]
    #[test]
    fn unsupported_or_corrupt_envelopes_fail_loudly() {
        let keys = KeySet::generated().unwrap();
        let stored = json!({"node":{"password":"fixture-secret"}}).as_object().unwrap().clone();
        let value = parse_value(&encrypt_map(&stored, &keys).unwrap()).unwrap();

        let mut version: Envelope = serde_json::from_value(value.clone()).unwrap();
        version.version = 2;
        assert_eq!(
            decrypt_with_keys(serde_json::to_value(version).unwrap(), &keys).unwrap_err(),
            "credential envelope version is not supported"
        );

        let mut algorithm: Envelope = serde_json::from_value(value.clone()).unwrap();
        algorithm.algorithm = "unsupported".to_string();
        assert_eq!(
            decrypt_with_keys(serde_json::to_value(algorithm).unwrap(), &keys).unwrap_err(),
            "credential envelope algorithm is not supported"
        );

        let mut header: Envelope = serde_json::from_value(value).unwrap();
        header.format = "corrupt-format".to_string();
        let header = serde_json::to_value(header).unwrap();
        assert!(is_envelope(&header));
        assert_eq!(decrypt_with_keys(header, &keys).unwrap_err(), "credential envelope version is not supported");
    }

    #[cfg(feature = "credential_encryption")]
    #[tokio::test]
    async fn missing_or_wrong_local_keys_cannot_overwrite_an_envelope() {
        let dir = temp();
        let flows = dir.0.join("flows.json");
        std::fs::write(&flows, "[]").unwrap();
        let expected = json!({"node":{"password":"fixture-secret"}}).as_object().unwrap().clone();
        let keys = KeySet::generated().unwrap();
        let encrypted = encrypt_map(&expected, &keys).unwrap();
        std::fs::write(credential_path(&flows), &encrypted).unwrap();
        let store = CredentialStore::default();
        assert_eq!(store.read_sidecar(&flows).await.unwrap_err(), "local credential keyring is missing");

        let wrong = KeySet::generated().unwrap();
        std::fs::write(store.key_file_path(&flows), wrong.keyring_bytes().unwrap()).unwrap();
        let replacement = json!({"node":{"password":"replacement"}}).as_object().unwrap().clone();
        assert!(store.encode_for_write(&flows, &replacement, &encrypted).await.is_err());
        assert_eq!(std::fs::read(credential_path(&flows)).unwrap(), encrypted);
    }

    #[cfg(feature = "credential_encryption")]
    #[tokio::test]
    async fn migration_and_export_are_explicit() {
        let dir = temp();
        let flows = dir.0.join("flows.json");
        std::fs::write(&flows, "[]").unwrap();
        std::fs::write(credential_path(&flows), r#"{"node":{"password":"fixture-secret"}}"#).unwrap();
        std::fs::write(previous_flow_path(&flows), "[]").unwrap();
        std::fs::write(previous_credential_path(&flows), r#"{"node":{"password":"fixture-old"}}"#).unwrap();
        let store = CredentialStore::default();
        let dry = store.migrate(&flows, true, None).await.unwrap();
        assert!(dry.changed);
        assert!(!store.key_file_path(&flows).exists());
        let backup = dir.0.join("offline-backup");
        let result = store.migrate(&flows, false, Some(&backup)).await.unwrap();
        assert!(result.changed);
        assert_eq!(store.read_sidecar(&flows).await.unwrap()["node"]["password"], "fixture-secret");
        let encrypted = std::fs::read_to_string(credential_path(&flows)).unwrap();
        assert!(!encrypted.contains("fixture-secret"));
        let export = dir.0.join("downgrade.json");
        let result = store.export(&flows, &export).await.unwrap();
        assert_eq!(result.current_output, export);
        assert_eq!(result.previous_output, previous_export_path(&export));
        let exported = std::fs::read_to_string(&export).unwrap();
        let exported_previous = std::fs::read_to_string(previous_export_path(&export)).unwrap();
        assert!(exported.contains("fixture-secret"));
        assert!(exported_previous.contains("fixture-old"));
        #[cfg(unix)]
        for path in
            [store.key_file_path(&flows), credential_path(&flows), export.clone(), previous_export_path(&export)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(path).unwrap().permissions().mode() & 0o777, 0o600);
        }
    }

    #[cfg(feature = "credential_encryption")]
    #[tokio::test]
    async fn rotation_and_lost_key_recovery_cover_both_generations() {
        let dir = temp();
        let flows = dir.0.join("flows.json");
        std::fs::write(&flows, "[]").unwrap();
        std::fs::write(credential_path(&flows), r#"{"node":{"password":"fixture-current"}}"#).unwrap();
        std::fs::write(previous_flow_path(&flows), "[]").unwrap();
        std::fs::write(previous_credential_path(&flows), r#"{"node":{"password":"fixture-previous"}}"#).unwrap();
        let store = CredentialStore::default();
        store.migrate(&flows, false, Some(&dir.0.join("migration-backup"))).await.unwrap();
        let old_current = std::fs::read(credential_path(&flows)).unwrap();
        let old_previous = std::fs::read(previous_credential_path(&flows)).unwrap();
        let old_key = dir.0.join("old.key");
        let old_id = store.status(&flows).await.unwrap().current.key_id.unwrap();
        let rotated = store.rotate(&flows, &old_key).await.unwrap();
        assert_ne!(rotated.current_key_id, old_id);
        assert_eq!(store.read_sidecar(&flows).await.unwrap()["node"]["password"], "fixture-current");
        assert_eq!(
            store.decode_path(&flows, &previous_credential_path(&flows)).await.unwrap()["node"]["password"],
            "fixture-previous"
        );

        std::fs::write(credential_path(&flows), old_current).unwrap();
        std::fs::write(previous_credential_path(&flows), old_previous).unwrap();
        assert!(store.read_sidecar(&flows).await.is_err());
        assert_eq!(store.recover_key(&flows, &old_key).await.unwrap(), old_id);
        assert_eq!(store.read_sidecar(&flows).await.unwrap()["node"]["password"], "fixture-current");
    }

    #[cfg(feature = "credential_encryption")]
    #[tokio::test]
    async fn transaction_failure_at_every_rename_restores_all_inputs() {
        for fail_before in 0..3 {
            let dir = temp();
            let flows = dir.0.join("flows.json");
            std::fs::write(&flows, "[]").unwrap();
            let store = CredentialStore::default();
            let paths = [credential_path(&flows), previous_credential_path(&flows), store.key_file_path(&flows)];
            for (index, path) in paths.iter().enumerate() {
                std::fs::write(path, format!("old-{index}")).unwrap();
            }
            let files: Vec<FileReplace> = paths
                .iter()
                .enumerate()
                .map(|(index, path)| FileReplace {
                    path: path.clone(),
                    bytes: format!("new-{index}").into_bytes(),
                    private: true,
                })
                .collect();
            let err = store.transactional_replace(&flows, &files, None, Some(fail_before)).await.unwrap_err();
            assert!(err.contains("injected rename failure"), "{err}");
            for (index, path) in paths.iter().enumerate() {
                assert_eq!(std::fs::read_to_string(path).unwrap(), format!("old-{index}"));
            }
            assert!(!journal_path(&flows).exists());
        }
    }

    #[cfg(feature = "credential_encryption")]
    #[tokio::test]
    async fn migration_journal_references_the_explicit_backup_without_copying_plaintext() {
        let dir = temp();
        let flows = dir.0.join("flows.json");
        std::fs::write(&flows, "[]").unwrap();
        std::fs::write(previous_flow_path(&flows), "[]").unwrap();
        let current = credential_path(&flows);
        let previous = previous_credential_path(&flows);
        std::fs::write(&current, br#"{"node":{"password":"fixture-current"}}"#).unwrap();
        std::fs::write(&previous, br#"{"node":{"password":"fixture-previous"}}"#).unwrap();
        let store = CredentialStore::default();
        let backup = dir.0.join("explicit-backup");
        backup_installation(&flows, &store.key_file_path(&flows), &backup).await.unwrap();
        let files = [
            FileReplace { path: current.clone(), bytes: b"candidate-current".to_vec(), private: true },
            FileReplace { path: previous.clone(), bytes: b"candidate-previous".to_vec(), private: true },
            FileReplace { path: store.key_file_path(&flows), bytes: b"candidate-key".to_vec(), private: true },
        ];
        let journal = build_journal(&files, Some(&backup)).await.unwrap();
        let journal_bytes = serde_json::to_vec(&journal).unwrap();
        let journal_text = String::from_utf8(journal_bytes.clone()).unwrap();
        assert!(!journal_text.contains("fixture-current"));
        assert!(!journal_text.contains("fixture-previous"));
        assert!(!journal_text.contains(&URL_SAFE_NO_PAD.encode(std::fs::read(&current).unwrap())));
        atomic_file::write_bytes(&journal_path(&flows), &journal_bytes, true).await.unwrap();
        atomic_file::write_bytes(&current, b"candidate-current", true).await.unwrap();
        atomic_file::write_bytes(&previous, b"candidate-previous", true).await.unwrap();
        atomic_file::write_bytes(&store.key_file_path(&flows), b"candidate-key", true).await.unwrap();
        assert!(store.recover_pending(&flows).await.unwrap());
        assert!(std::fs::read_to_string(current).unwrap().contains("fixture-current"));
        assert!(std::fs::read_to_string(previous).unwrap().contains("fixture-previous"));
        assert!(!store.key_file_path(&flows).exists());
    }

    #[cfg(feature = "credential_encryption")]
    #[tokio::test]
    async fn pending_transaction_restores_the_old_generation() {
        let dir = temp();
        let flows = dir.0.join("flows.json");
        std::fs::write(&flows, "[]").unwrap();
        let current = credential_path(&flows);
        let previous = previous_credential_path(&flows);
        std::fs::write(&current, b"old-current").unwrap();
        std::fs::write(&previous, b"old-previous").unwrap();
        let files = [
            FileReplace { path: current.clone(), bytes: b"new-current".to_vec(), private: true },
            FileReplace { path: previous.clone(), bytes: b"new-previous".to_vec(), private: true },
        ];
        let journal = build_journal(&files, None).await.unwrap();
        atomic_file::write_bytes(&journal_path(&flows), &serde_json::to_vec(&journal).unwrap(), true).await.unwrap();
        atomic_file::write_bytes(&current, b"new-current", true).await.unwrap();
        let store = CredentialStore::default();
        assert!(store.recover_pending(&flows).await.unwrap());
        assert_eq!(std::fs::read(current).unwrap(), b"old-current");
        assert_eq!(std::fs::read(previous).unwrap(), b"old-previous");
    }
}
