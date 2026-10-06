//! Node credential API.
//!
//! The editor loads a config node's Security tab with `GET /credentials/{type}/{id}` and writes
//! changes inside the flow deploy body. Passwords are never returned. An unchanged password is
//! the sentinel `__PWRD__`, and an empty password deletes the stored value.
//!
//! Secrets are stored in the versioned credential sidecar beside `flows.json`. Plaintext is
//! accepted during migration; encrypted sidecars remain opaque to the editor. The flow file and
//! `GET /flows` do not contain credentials.

use crate::handlers::WebState;
use axum::{Extension, Json, extract::Path, http::StatusCode};
use n2link_core::runtime::flow_credentials::{self, sidecar_path};
use serde_json::{Map, Value, json};
use std::collections::HashSet;
use std::path::{Path as StdPath, PathBuf};
use std::sync::Arc;

#[derive(Clone, Copy, PartialEq, Eq)]
enum FieldKind {
    Text,
    Password,
}

const MQTT_BROKER: &[(&str, FieldKind)] = &[("user", FieldKind::Text), ("password", FieldKind::Password)];
const HTTP_REQUEST: &[(&str, FieldKind)] = &[("user", FieldKind::Text), ("password", FieldKind::Password)];
const TLS_CONFIG: &[(&str, FieldKind)] = &[
    ("certdata", FieldKind::Text),
    ("keydata", FieldKind::Text),
    ("cadata", FieldKind::Text),
    ("p12data", FieldKind::Text),
    ("passphrase", FieldKind::Password),
];
const HTTP_PROXY: &[(&str, FieldKind)] = &[("username", FieldKind::Text), ("password", FieldKind::Password)];
const AI_PROVIDER: &[(&str, FieldKind)] = &[("apiKey", FieldKind::Password)];
const POSTGRES_CONFIG: &[(&str, FieldKind)] = &[("user", FieldKind::Text), ("password", FieldKind::Password)];
const REDIS_CONFIG: &[(&str, FieldKind)] = &[("user", FieldKind::Text), ("password", FieldKind::Password)];

fn fields_for(node_type: &str) -> Option<&'static [(&'static str, FieldKind)]> {
    match node_type {
        "mqtt-broker" => Some(MQTT_BROKER),
        "http request" | "http-request" => Some(HTTP_REQUEST),
        "tls-config" => Some(TLS_CONFIG),
        "http proxy" | "http-proxy" => Some(HTTP_PROXY),
        "ai-provider" => Some(AI_PROVIDER),
        "postgres-config" => Some(POSTGRES_CONFIG),
        "redis-config" => Some(REDIS_CONFIG),
        _ => None,
    }
}

/// Remove `credentials` from every node and fold recognised updates into `stored`.
///
/// A node that omits `credentials` keeps its stored values. `__PWRD__` on a password field keeps
/// the stored password. A blank value deletes that key. A type with no credential definition is
/// not stored. Ids that are no longer in the flow are dropped.
pub fn separate(flows: &mut [Value], stored: &mut Map<String, Value>) {
    let live: HashSet<String> =
        flows.iter().filter_map(|node| node.get("id").and_then(Value::as_str).map(str::to_owned)).collect();
    for node in flows.iter_mut() {
        extract_one(node, stored);
    }
    stored.retain(|id, value| live.contains(id) && value.as_object().is_some_and(|object| !object.is_empty()));
}

fn extract_one(node: &mut Value, stored: &mut Map<String, Value>) {
    let Some(object) = node.as_object_mut() else {
        return;
    };
    let Some(incoming) = object.remove("credentials") else {
        return;
    };
    let Some(id) = object.get("id").and_then(Value::as_str).map(str::to_owned) else {
        return;
    };
    let node_type = object.get("type").and_then(Value::as_str).unwrap_or("");
    let Some(incoming) = incoming.as_object() else {
        return;
    };
    let mut saved = stored.get(&id).and_then(Value::as_object).cloned().unwrap_or_default();
    if node_type == "global-config" {
        apply_global_map(&mut saved, incoming);
    } else if let Some(fields) = fields_for(node_type) {
        for (key, kind) in fields {
            let Some(value) = incoming.get(*key) else {
                continue;
            };
            assign_field(&mut saved, key, *kind, value);
        }
    } else {
        log::warn!("credentials for node type '{node_type}' are not registered and were not stored");
        return;
    }
    if saved.is_empty() {
        stored.remove(&id);
    } else {
        stored.insert(id, Value::Object(saved));
    }
}

fn assign_field(saved: &mut Map<String, Value>, key: &str, kind: FieldKind, value: &Value) {
    let Some(text) = value.as_str() else {
        if value.is_null() {
            saved.remove(key);
        }
        return;
    };
    if kind == FieldKind::Password && text == "__PWRD__" {
        return;
    }
    if text.trim().is_empty() {
        saved.remove(key);
        return;
    }
    let next = Value::String(text.to_string());
    if saved.get(key) != Some(&next) {
        saved.insert(key.to_string(), next);
    }
}

fn apply_global_map(saved: &mut Map<String, Value>, incoming: &Map<String, Value>) {
    let mut map = saved.get("map").and_then(Value::as_object).cloned().unwrap_or_default();
    let new_map = incoming.get("map").and_then(Value::as_object);
    let existing: Vec<String> = map.keys().cloned().collect();
    for key in existing {
        let still_present = new_map.and_then(|object| object.get(&key)).is_some_and(|value| match value {
            Value::String(text) => !text.is_empty(),
            Value::Null => false,
            _ => true,
        });
        if !still_present {
            let flag = format!("has_{key}");
            map.remove(&key);
            map.remove(&flag);
        }
    }
    if let Some(new_map) = new_map {
        let keys: Vec<String> = new_map.keys().cloned().collect();
        for key in keys {
            if key.starts_with("has_") {
                continue;
            }
            let value = &new_map[&key];
            if value.as_str().is_some_and(|text| text.trim().is_empty()) {
                continue;
            }
            let unchanged = value.as_str() == Some("__PWRD__") && map.contains_key(&key);
            if unchanged {
                continue;
            }
            map.insert(key.clone(), value.clone());
            let flag = format!("has_{key}");
            if let Some(marker) = new_map.get(&flag) {
                map.insert(flag, marker.clone());
            }
        }
    }
    if map.is_empty() {
        saved.remove("map");
    } else {
        saved.insert("map".to_string(), Value::Object(map));
    }
}

/// What the editor is allowed to see. Password fields become `has_<name>` and the secret stays out.
pub fn public_view(node_type: &str, entry: Option<&Value>) -> Value {
    let Some(entry) = entry else {
        return json!({});
    };
    if node_type == "global-config" {
        return match entry.get("map").filter(|value| value.is_object()) {
            Some(map) => json!({ "map": map }),
            None => json!({}),
        };
    }
    if let Some(fields) = fields_for(node_type) {
        let mut out = Map::new();
        for (key, kind) in fields {
            match kind {
                FieldKind::Password => {
                    let present = entry.get(*key).is_some_and(|value| match value {
                        Value::String(text) => !text.trim().is_empty(),
                        Value::Null => false,
                        _ => true,
                    });
                    out.insert(format!("has_{key}"), Value::Bool(present));
                }
                FieldKind::Text => {
                    let text = entry.get(*key).and_then(Value::as_str).unwrap_or("");
                    out.insert((*key).to_string(), Value::String(text.to_string()));
                }
            }
        }
        return Value::Object(out);
    }
    unknown_view(entry)
}

fn unknown_view(entry: &Value) -> Value {
    let Some(object) = entry.as_object() else {
        return json!({});
    };
    let mut out = Map::new();
    for (key, value) in object {
        if key == "user" || key == "username" {
            if let Some(text) = value.as_str() {
                out.insert(key.clone(), Value::String(text.to_string()));
            }
            continue;
        }
        let secret = key == "password" || key == "passphrase" || key.starts_with("has_");
        if secret {
            let present = match value {
                Value::Bool(flag) => *flag,
                Value::String(text) => !text.trim().is_empty(),
                Value::Null => false,
                _ => true,
            };
            let name = if key.starts_with("has_") { key.clone() } else { format!("has_{key}") };
            out.insert(name, Value::Bool(present));
        }
    }
    Value::Object(out)
}

fn previous_path(path: &StdPath) -> PathBuf {
    let name = path.file_name().and_then(|name| name.to_str()).unwrap_or("flows_cred.json");
    path.with_file_name(format!("{name}.prev"))
}

pub async fn snapshot_sidecar(flows_file: &StdPath) -> Result<(), String> {
    let path = sidecar_path(flows_file);
    let previous = previous_path(&path);
    if let Some(parent) = previous.parent() {
        tokio::fs::create_dir_all(parent).await.map_err(|err| err.to_string())?;
    }
    if path.exists() {
        tokio::fs::copy(&path, &previous).await.map_err(|err| err.to_string())?;
    } else {
        tokio::fs::write(&previous, b"{}").await.map_err(|err| err.to_string())?;
    }
    Ok(())
}

pub async fn write_sidecar(flows_file: &StdPath, stored: &Map<String, Value>) -> Result<(), String> {
    let store = n2link_core::runtime::credential_storage::CredentialStore::default();
    let _lock = store.lock(flows_file).await?;
    let path = sidecar_path(flows_file);
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await.map_err(|err| err.to_string())?;
    }
    let current =
        if path.exists() { tokio::fs::read(&path).await.map_err(|err| err.to_string())? } else { b"{}".to_vec() };
    let bytes = store.encode_for_write(flows_file, stored, &current).await?;
    n2link_core::utils::atomic_file::write_bytes(&path, &bytes, true).await
}

pub async fn swap_sidecar(flows_file: &StdPath) -> Result<(), String> {
    let path = sidecar_path(flows_file);
    let previous = previous_path(&path);
    let saved = if previous.exists() {
        tokio::fs::read(&previous).await.map_err(|err| err.to_string())?
    } else {
        b"{}".to_vec()
    };
    let current =
        if path.exists() { tokio::fs::read(&path).await.map_err(|err| err.to_string())? } else { b"{}".to_vec() };
    n2link_core::utils::atomic_file::write_bytes(&path, &saved, true).await?;
    n2link_core::utils::atomic_file::write_bytes(&previous, &current, true).await?;
    Ok(())
}

/// Safe credentials for one node. A missing id is `{}` so the editor can open a new config node.
pub async fn get_node_credentials(
    Extension(state): Extension<Arc<WebState>>,
    Path((node_type, id)): Path<(String, String)>,
) -> Result<Json<Value>, StatusCode> {
    let flows_path = state.flows_file_path.read().await.clone();
    let Some(flows_path) = flows_path else {
        return Ok(Json(json!({})));
    };
    let stored = flow_credentials::read_sidecar_with(&state.credentials, &flows_path).await.map_err(|err| {
        log::error!("Failed to read node credentials: {err}");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;
    Ok(Json(public_view(&node_type, stored.get(&id))))
}
