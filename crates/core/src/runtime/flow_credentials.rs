//! Credential sidecar that sits next to `flows.json`.
//!
//! Node-RED names it by stripping `.json` and appending `_cred.json`, so `flows.json` becomes
//! `flows_cred.json`. The compatibility reader accepts plaintext or the versioned encrypted
//! envelope. Nothing in this module logs its contents.
//! The editor stores secrets here and the engine merges them back onto node objects at load,
//! because config nodes read `credentials` while the flow file itself must not contain them.

use serde_json::{Map, Value};
use std::path::{Path, PathBuf};

use super::credential_storage::CredentialStore;

/// `flows.json` → `flows_cred.json`.
pub fn sidecar_path(flows_file: &Path) -> PathBuf {
    let name = flows_file.file_name().and_then(|name| name.to_str()).unwrap_or("flows.json");
    let stem = name.strip_suffix(".json").unwrap_or(name);
    flows_file.with_file_name(format!("{stem}_cred.json"))
}

/// Drop `credentials` so a revision matches the flow file the editor saved.
pub fn for_revision(json: &Value) -> Value {
    let mut copy = json.clone();
    if let Some(nodes) = copy.as_array_mut() {
        for node in nodes {
            if let Some(object) = node.as_object_mut() {
                object.remove("credentials");
            }
        }
    }
    copy
}

/// Attach stored secrets onto nodes that do not already carry a `credentials` object.
pub fn merge_into(flows: &mut [Value], stored: &Map<String, Value>) {
    for node in flows {
        let Some(object) = node.as_object_mut() else {
            continue;
        };
        if object.contains_key("credentials") {
            continue;
        }
        let Some(id) = object.get("id").and_then(Value::as_str) else {
            continue;
        };
        if let Some(creds) = stored.get(id) {
            object.insert("credentials".to_string(), creds.clone());
        }
    }
}

pub fn parse_sidecar(text: &str) -> Result<Map<String, Value>, String> {
    if text.trim().is_empty() {
        return Ok(Map::new());
    }
    match serde_json::from_str::<Value>(text) {
        Ok(Value::Object(map)) => Ok(map),
        Ok(_) => Err("credential file is not an object".to_string()),
        Err(_) => Err("credential file is not valid json".to_string()),
    }
}

pub async fn read_sidecar(flows_file: &Path) -> Result<Map<String, Value>, String> {
    CredentialStore::default().read_sidecar(flows_file).await
}

pub async fn read_sidecar_with(store: &CredentialStore, flows_file: &Path) -> Result<Map<String, Value>, String> {
    store.read_sidecar(flows_file).await
}

/// Read `flows.json` and merge `flows_cred.json` when that file exists.
pub async fn flows_value_with_credentials(flows_file: &Path, cfg: Option<&config::Config>) -> crate::Result<Value> {
    let json_str = tokio::fs::read_to_string(flows_file).await?;
    let mut json: Value = serde_json::from_str(&json_str)?;
    let store = CredentialStore::from_config(cfg).map_err(crate::N2linkError::BadFlowsJson)?;
    let stored = store.read_sidecar(flows_file).await.map_err(crate::N2linkError::BadFlowsJson)?;
    if let Some(flows) = json.as_array_mut() {
        merge_into(flows, &stored);
    }
    Ok(json)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn sidecar_name_strips_only_the_json_suffix() {
        let path = Path::new("/tmp/home/flows.json");
        assert_eq!(sidecar_path(path), PathBuf::from("/tmp/home/flows_cred.json"));
    }

    #[test]
    fn revision_ignores_credentials_and_merge_puts_them_back() {
        let bare = json!([{ "id": "b", "type": "mqtt-broker", "broker": "localhost" }]);
        let mut with = bare.clone();
        let stored = json!({ "b": { "user": "operator", "password": "secret-value" } });
        let map = stored.as_object().unwrap();
        merge_into(with.as_array_mut().unwrap(), map);
        assert_eq!(with[0]["credentials"]["password"], "secret-value");
        assert_eq!(for_revision(&with), bare);
        let again = json!([{ "id": "b", "type": "mqtt-broker", "credentials": { "user": "keep" } }]);
        let mut kept = again.clone();
        merge_into(kept.as_array_mut().unwrap(), map);
        assert_eq!(kept[0]["credentials"]["user"], "keep");
    }

    #[tokio::test]
    async fn a_missing_sidecar_leaves_the_flows_unchanged() {
        let dir = std::env::temp_dir().join(format!("n2linkd-cred-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let flows = dir.join("flows.json");
        std::fs::write(&flows, r#"[{"id":"t","type":"tab"}]"#).unwrap();
        let loaded = flows_value_with_credentials(&flows, None).await.unwrap();
        assert_eq!(loaded, json!([{ "id": "t", "type": "tab" }]));
        std::fs::write(sidecar_path(&flows), "[]").unwrap();
        let err = flows_value_with_credentials(&flows, None).await.unwrap_err();
        assert!(err.to_string().contains("credential file"));
        assert!(!err.to_string().contains("secret"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(feature = "credential_encryption")]
    #[tokio::test]
    async fn encrypted_sidecar_is_merged_on_runtime_load() {
        use crate::runtime::credential_storage::{CredentialStore, previous_credential_path};

        let dir = std::env::temp_dir().join(format!("n2link-encrypted-load-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let flows = dir.join("flows.json");
        std::fs::write(&flows, br#"[{"id":"b","type":"mqtt-broker","broker":"localhost"}]"#).unwrap();
        std::fs::write(sidecar_path(&flows), br#"{"b":{"user":"fixture-user","password":"fixture-secret"}}"#).unwrap();
        std::fs::write(flows.with_file_name("flows.json.prev"), b"[]").unwrap();
        std::fs::write(previous_credential_path(&flows), b"{}").unwrap();
        let store = CredentialStore::default();
        store.migrate(&flows, false, Some(&dir.join("backup"))).await.unwrap();

        let loaded = flows_value_with_credentials(&flows, None).await.unwrap();
        assert_eq!(loaded[0]["credentials"]["user"], "fixture-user");
        assert_eq!(loaded[0]["credentials"]["password"], "fixture-secret");
        assert!(!std::fs::read_to_string(sidecar_path(&flows)).unwrap().contains("fixture-secret"));
        std::fs::remove_dir_all(dir).unwrap();
    }
}
