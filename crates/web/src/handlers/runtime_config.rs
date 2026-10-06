//! Transactional editor for the environment-specific egress configuration overlay.
//!
//! Only the `[egress]` table is exposed. The rest of the TOML file can contain credentials and
//! is never returned to the browser or copied into logs.

use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::Arc;

use axum::extract::Extension;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Json, Response};
use n2link_core::runtime::egress::{EgressConfig, EgressPolicy};
use n2link_core::utils::atomic_file::{self, FileReplace};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use toml_edit::DocumentMut;

#[cfg(test)]
use super::WebRuntimeServices;
use super::WebState;
use super::reply::api_error;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConfigWrite {
    rev: String,
    config: EgressConfig,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RevisionRequest {
    rev: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ConfigView {
    config: EgressConfig,
    rev: String,
    applied_rev: String,
    dirty: bool,
    file: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ConfigResult {
    rev: String,
    applied_rev: String,
    dirty: bool,
}

#[derive(Deserialize)]
struct Overlay {
    #[serde(default)]
    egress: Option<EgressConfig>,
}

#[derive(Serialize)]
struct EgressOnly<'a> {
    egress: &'a EgressConfig,
}

pub fn revision(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

type ApiResult<T> = Result<T, Box<Response>>;

fn boxed_api_error(status: StatusCode, code: &str, message: &str) -> Box<Response> {
    Box::new(api_error(status, code, message))
}

fn previous_path(path: &Path) -> PathBuf {
    let name = path.file_name().and_then(|name| name.to_str()).unwrap_or("n2linkd.dev.toml");
    path.with_file_name(format!("{name}.prev"))
}

async fn editor_path(state: &WebState) -> ApiResult<PathBuf> {
    if !state.config_editor_enabled {
        return Err(boxed_api_error(StatusCode::NOT_FOUND, "not_supported", "configuration editor is not enabled"));
    }
    state.config_file_path.read().await.clone().ok_or_else(|| {
        boxed_api_error(StatusCode::INTERNAL_SERVER_ERROR, "not_configured", "configuration file is not configured")
    })
}

async fn read_bytes(path: &Path) -> ApiResult<Vec<u8>> {
    match tokio::fs::read(path).await {
        Ok(bytes) => Ok(bytes),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(_) => {
            Err(boxed_api_error(StatusCode::INTERNAL_SERVER_ERROR, "read_failed", "configuration could not be read"))
        }
    }
}

fn saved_config(bytes: &[u8], active: &EgressConfig) -> ApiResult<EgressConfig> {
    if bytes.iter().all(u8::is_ascii_whitespace) {
        return Ok(active.clone());
    }
    let text = std::str::from_utf8(bytes)
        .map_err(|_| boxed_api_error(StatusCode::BAD_REQUEST, "invalid_config", "configuration is not UTF-8"))?;
    let overlay: Overlay = toml_edit::de::from_str(text)
        .map_err(|_| boxed_api_error(StatusCode::BAD_REQUEST, "invalid_config", "configuration is not valid TOML"))?;
    Ok(overlay.egress.unwrap_or_else(|| active.clone()))
}

fn patch_egress(bytes: &[u8], config: &EgressConfig) -> ApiResult<Vec<u8>> {
    let text = std::str::from_utf8(bytes)
        .map_err(|_| boxed_api_error(StatusCode::BAD_REQUEST, "invalid_config", "configuration is not UTF-8"))?;
    let mut document = if text.trim().is_empty() {
        DocumentMut::new()
    } else {
        DocumentMut::from_str(text).map_err(|_| {
            boxed_api_error(StatusCode::BAD_REQUEST, "invalid_config", "configuration is not valid TOML")
        })?
    };
    let mut egress = toml_edit::ser::to_document(&EgressOnly { egress: config }).map_err(|_| {
        boxed_api_error(StatusCode::INTERNAL_SERVER_ERROR, "serialize_failed", "configuration could not be serialized")
    })?;
    let item = egress.remove("egress").ok_or_else(|| {
        boxed_api_error(StatusCode::INTERNAL_SERVER_ERROR, "serialize_failed", "egress configuration is missing")
    })?;
    document["egress"] = item;
    Ok(document.to_string().into_bytes())
}

fn validate(config: EgressConfig) -> ApiResult<EgressPolicy> {
    EgressPolicy::from_config(config)
        .map_err(|err| boxed_api_error(StatusCode::BAD_REQUEST, "invalid_config", &err.to_string()))
}

async fn check_revision(path: &Path, offered: &str) -> ApiResult<Vec<u8>> {
    let bytes = read_bytes(path).await?;
    if revision(&bytes) != offered {
        return Err(boxed_api_error(StatusCode::CONFLICT, "version_mismatch", "configuration version mismatch"));
    }
    Ok(bytes)
}

async fn swap_files(path: &Path, live: Vec<u8>, previous: Vec<u8>) -> ApiResult<()> {
    atomic_file::replace_files(&[
        FileReplace { path: previous_path(path), bytes: live, private: true },
        FileReplace { path: path.to_path_buf(), bytes: previous, private: true },
    ])
    .await
    .map_err(|_| {
        boxed_api_error(StatusCode::INTERNAL_SERVER_ERROR, "write_failed", "configuration could not be replaced")
    })
}

async fn restart(state: &WebState) -> Result<(), String> {
    let engine = state.engine.read().await.clone().ok_or_else(|| "runtime engine is not available".to_string())?;
    engine.restart().await.map_err(|err| err.to_string())
}

pub async fn get_egress_config(Extension(state): Extension<Arc<WebState>>) -> Response {
    let path = match editor_path(&state).await {
        Ok(path) => path,
        Err(response) => return *response,
    };
    let bytes = match read_bytes(&path).await {
        Ok(bytes) => bytes,
        Err(response) => return *response,
    };
    let active = state.egress.config();
    let config = match saved_config(&bytes, &active) {
        Ok(config) => config,
        Err(response) => return *response,
    };
    let rev = revision(&bytes);
    let applied_rev = state.applied_config_rev.read().await.clone();
    Json(ConfigView {
        dirty: config != active,
        config,
        rev,
        applied_rev,
        file: path.file_name().and_then(|name| name.to_str()).unwrap_or("configuration").to_string(),
    })
    .into_response()
}

pub async fn validate_egress_config(Json(payload): Json<EgressConfig>) -> Response {
    match validate(payload) {
        Ok(policy) => Json(serde_json::json!({ "valid": true, "config": policy.config() })).into_response(),
        Err(response) => *response,
    }
}

pub async fn put_egress_config(
    Extension(state): Extension<Arc<WebState>>,
    headers: HeaderMap,
    Json(payload): Json<ConfigWrite>,
) -> Response {
    let _transaction = state.config_apply.lock().await;
    let path = match editor_path(&state).await {
        Ok(path) => path,
        Err(response) => return *response,
    };
    if let Err(response) = validate(payload.config.clone()) {
        return *response;
    }
    let current = match check_revision(&path, &payload.rev).await {
        Ok(bytes) => bytes,
        Err(response) => return *response,
    };
    let live = match patch_egress(&current, &payload.config) {
        Ok(bytes) => bytes,
        Err(response) => return *response,
    };
    let previous = match patch_egress(&current, &state.egress.config()) {
        Ok(bytes) => bytes,
        Err(response) => return *response,
    };
    if let Err(response) = swap_files(&path, previous, live.clone()).await {
        return *response;
    }
    let rev = revision(&live);
    let actor = state.auth.actor_from_headers(&headers);
    let _ = state.audit.record(&actor.username, "config.save", Some(&rev)).await;
    let applied_rev = state.applied_config_rev.read().await.clone();
    Json(ConfigResult { dirty: payload.config != state.egress.config(), rev, applied_rev }).into_response()
}

pub async fn apply_egress_config(
    Extension(state): Extension<Arc<WebState>>,
    headers: HeaderMap,
    Json(payload): Json<RevisionRequest>,
) -> Response {
    let _transaction = state.config_apply.lock().await;
    let path = match editor_path(&state).await {
        Ok(path) => path,
        Err(response) => return *response,
    };
    let live = match check_revision(&path, &payload.rev).await {
        Ok(bytes) => bytes,
        Err(response) => return *response,
    };
    let previous_path = previous_path(&path);
    let previous = match tokio::fs::read(&previous_path).await {
        Ok(bytes) => bytes,
        Err(_) => return api_error(StatusCode::NOT_FOUND, "not_found", "no previous configuration"),
    };
    let candidate_config = match saved_config(&live, &state.egress.config()) {
        Ok(config) => config,
        Err(response) => return *response,
    };
    let candidate = match validate(candidate_config) {
        Ok(policy) => policy,
        Err(response) => return *response,
    };
    let old = state.egress.replace(candidate);
    if let Err(err) = restart(&state).await {
        state.egress.replace_arc(old);
        let restore_runtime = restart(&state).await;
        let restore_disk = swap_files(&path, live, previous).await;
        let restored = read_bytes(&path).await.unwrap_or_default();
        *state.applied_config_rev.write().await = revision(&restored);
        let actor = state.auth.actor_from_headers(&headers);
        let _ = state.audit.record(&actor.username, "config.rejected", Some(&payload.rev)).await;
        if let Err(restore) = restore_runtime {
            log::error!("configuration activation failed and runtime restore failed: {restore}");
        }
        if restore_disk.is_err() {
            log::error!("configuration activation failed and disk restore failed");
        }
        return api_error(StatusCode::INTERNAL_SERVER_ERROR, "activation_failed", &err);
    }
    *state.applied_config_rev.write().await = payload.rev.clone();
    let actor = state.auth.actor_from_headers(&headers);
    let _ = state.audit.record(&actor.username, "config.apply", Some(&payload.rev)).await;
    state.comms.send_notification("success", "Outbound configuration applied").await;
    Json(ConfigResult { dirty: false, rev: payload.rev.clone(), applied_rev: payload.rev }).into_response()
}

pub async fn rollback_egress_config(
    Extension(state): Extension<Arc<WebState>>,
    headers: HeaderMap,
    Json(payload): Json<RevisionRequest>,
) -> Response {
    let _transaction = state.config_apply.lock().await;
    let path = match editor_path(&state).await {
        Ok(path) => path,
        Err(response) => return *response,
    };
    let live = match check_revision(&path, &payload.rev).await {
        Ok(bytes) => bytes,
        Err(response) => return *response,
    };
    let previous = match tokio::fs::read(previous_path(&path)).await {
        Ok(bytes) => bytes,
        Err(_) => return api_error(StatusCode::NOT_FOUND, "not_found", "no previous configuration"),
    };
    let candidate_config = match saved_config(&previous, &state.egress.config()) {
        Ok(config) => config,
        Err(response) => return *response,
    };
    let candidate = match validate(candidate_config) {
        Ok(policy) => policy,
        Err(response) => return *response,
    };
    if let Err(response) = swap_files(&path, live.clone(), previous.clone()).await {
        return *response;
    }
    let old = state.egress.replace(candidate);
    if let Err(err) = restart(&state).await {
        state.egress.replace_arc(old);
        let _ = swap_files(&path, previous, live).await;
        let _ = restart(&state).await;
        return api_error(StatusCode::INTERNAL_SERVER_ERROR, "activation_failed", &err);
    }
    let rev = revision(&previous);
    *state.applied_config_rev.write().await = rev.clone();
    let actor = state.auth.actor_from_headers(&headers);
    let _ = state.audit.record(&actor.username, "config.rollback", Some(&rev)).await;
    state.comms.send_notification("success", "Outbound configuration rolled back").await;
    Json(ConfigResult { dirty: false, rev: rev.clone(), applied_rev: rev }).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::handlers::auth::AdminAuth;
    use crate::handlers::fleet::Fleet;
    use crate::models::RedSystemSettings;
    use axum::body::to_bytes;
    use n2link_core::runtime::egress::{EgressMode, EgressRule, NetworkProtocol};
    use n2link_core::runtime::engine::Engine;
    use n2link_core::runtime::registry::RegistryBuilder;
    use serde_json::json;

    struct TempDir(PathBuf);

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn temp() -> TempDir {
        let path = std::env::temp_dir().join(format!("n2link-config-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&path).unwrap();
        TempDir(path)
    }

    async fn body(response: Response) -> serde_json::Value {
        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    async fn state(path: &Path) -> Arc<WebState> {
        let registry = RegistryBuilder::default().build().unwrap();
        let engine =
            Engine::with_json(&registry, json!([{ "id": "100", "type": "tab", "label": "Flow 1" }]), None).unwrap();
        engine.start().await.unwrap();
        let egress = engine.egress_policy().clone();
        let state = WebState::assemble_with_egress(
            Arc::new(RedSystemSettings::default()),
            path.parent().unwrap().to_path_buf(),
            None,
            AdminAuth::open(),
            Fleet::disabled(),
            WebRuntimeServices {
                egress,
                credentials: n2link_core::runtime::credential_storage::CredentialStore::default(),
                protection: crate::protection::ApiProtection::default(),
                ..Default::default()
            },
            true,
        );
        state.set_registry(registry).await;
        state.set_engine(Arc::new(engine)).await;
        state.set_config_file_path(path.to_path_buf()).await;
        state
    }

    #[test]
    fn patch_changes_only_the_egress_table() {
        let source = br#"title = "keep"

[admin]
password = "secret-value"

[egress]
mode = "off"
"#;
        let config = EgressConfig {
            mode: EgressMode::Enforce,
            allow: vec![EgressRule {
                protocols: vec![NetworkProtocol::Mqtt],
                host: Some("127.0.0.1".to_string()),
                cidr: None,
                ports: vec![1883],
            }],
            ..EgressConfig::default()
        };

        let patched = patch_egress(source, &config).unwrap();
        let text = String::from_utf8(patched).unwrap();

        assert!(text.contains("title = \"keep\""));
        assert!(text.contains("password = \"secret-value\""));
        let parsed: Overlay = toml_edit::de::from_str(&text).unwrap();
        assert_eq!(parsed.egress.unwrap(), config);
    }

    #[test]
    fn invalid_policy_is_rejected_before_write() {
        let config = EgressConfig { connect_timeout_ms: 0, ..EgressConfig::default() };
        let response = validate(config).unwrap_err();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn save_apply_and_rollback_reinitialize_the_shared_policy() {
        let dir = temp();
        let path = dir.0.join("n2linkd.dev.toml");
        std::fs::write(&path, "title = \"keep\"\n").unwrap();
        let state = state(&path).await;
        let original = revision(&std::fs::read(&path).unwrap());
        let config = EgressConfig {
            mode: EgressMode::Enforce,
            allow: vec![EgressRule {
                protocols: vec![NetworkProtocol::Mqtt],
                host: Some("127.0.0.1".to_string()),
                cidr: None,
                ports: vec![1883],
            }],
            ..EgressConfig::default()
        };

        let saved =
            put_egress_config(Extension(state.clone()), HeaderMap::new(), Json(ConfigWrite { rev: original, config }))
                .await;
        assert_eq!(saved.status(), StatusCode::OK);
        let saved = body(saved).await;
        let saved_rev = saved["rev"].as_str().unwrap().to_string();
        assert_eq!(state.egress.mode(), EgressMode::Off);

        let applied = apply_egress_config(
            Extension(state.clone()),
            HeaderMap::new(),
            Json(RevisionRequest { rev: saved_rev.clone() }),
        )
        .await;
        assert_eq!(applied.status(), StatusCode::OK);
        assert_eq!(state.egress.mode(), EgressMode::Enforce);

        let rolled_back = rollback_egress_config(
            Extension(state.clone()),
            HeaderMap::new(),
            Json(RevisionRequest { rev: saved_rev }),
        )
        .await;
        assert_eq!(rolled_back.status(), StatusCode::OK);
        assert_eq!(state.egress.mode(), EgressMode::Off);
        assert!(std::fs::read_to_string(&path).unwrap().contains("title = \"keep\""));
        state.engine.read().await.as_ref().unwrap().stop().await.unwrap();
    }

    #[tokio::test]
    async fn an_unrelated_file_change_does_not_require_an_egress_restart() {
        let dir = temp();
        let path = dir.0.join("n2linkd.dev.toml");
        std::fs::write(&path, "title = \"before\"\n").unwrap();
        let state = state(&path).await;
        std::fs::write(&path, "title = \"after\"\n").unwrap();

        let response = get_egress_config(Extension(state)).await;
        assert_eq!(response.status(), StatusCode::OK);
        let view = body(response).await;
        assert_eq!(view["dirty"], false);
        assert_ne!(view["rev"], view["appliedRev"]);
    }

    #[tokio::test]
    async fn stale_save_is_rejected_without_writing() {
        let dir = temp();
        let path = dir.0.join("n2linkd.dev.toml");
        std::fs::write(&path, "title = \"keep\"\n").unwrap();
        let state = state(&path).await;

        let response = put_egress_config(
            Extension(state.clone()),
            HeaderMap::new(),
            Json(ConfigWrite { rev: "stale".to_string(), config: EgressConfig::default() }),
        )
        .await;

        assert_eq!(response.status(), StatusCode::CONFLICT);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "title = \"keep\"\n");
        state.engine.read().await.as_ref().unwrap().stop().await.unwrap();
    }
}
