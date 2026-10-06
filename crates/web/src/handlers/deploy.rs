//! One deployment path for `/flows`, `/flows/rollback`, and `/flow`.
//!
//! Flows and credentials are one write-set. The candidate graph is prepared before any live file
//! is replaced. Activation failure restores the live pair and the previous rollback pair from a
//! snapshot taken before persistence.

use std::path::{Path as StdPath, PathBuf};

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use n2link_core::N2linkError;
use n2link_core::runtime::credential_storage::CredentialStore;
use n2link_core::runtime::engine::Engine;
use n2link_core::runtime::flow_credentials::{self, sidecar_path};
use n2link_core::utils::atomic_file::{self, FileReplace};
use serde_json::{Map, Value};

use super::WebState;
use super::credentials;
use super::reply::api_error;

#[derive(Debug)]
pub enum DeployErr {
    NotFound,
    MissingFlow,
    Invalid(N2linkError),
    Internal(String),
}

impl IntoResponse for DeployErr {
    fn into_response(self) -> Response {
        match self {
            Self::NotFound => api_error(StatusCode::NOT_FOUND, "not_found", "no previous flows"),
            Self::MissingFlow => api_error(StatusCode::NOT_FOUND, "not_found", "flow not found"),
            Self::Invalid(err) => match err {
                N2linkError::NotSupported(_)
                | N2linkError::BadFlowsJson(_)
                | N2linkError::InvalidOperation(_)
                | N2linkError::UnsupportedFlowsJsonFormat(_) => {
                    api_error(StatusCode::BAD_REQUEST, "invalid_flows", &err.to_string())
                }
                _ => {
                    log::error!("Failed to activate flows: {err}");
                    StatusCode::INTERNAL_SERVER_ERROR.into_response()
                }
            },
            Self::Internal(message) => {
                log::error!("{message}");
                StatusCode::INTERNAL_SERVER_ERROR.into_response()
            }
        }
    }
}

pub fn previous_flows_path(path: &StdPath) -> PathBuf {
    let name = path.file_name().and_then(|name| name.to_str()).unwrap_or("flows.json");
    path.with_file_name(format!("{name}.prev"))
}

fn previous_creds_path(flows: &StdPath) -> PathBuf {
    let cred = sidecar_path(flows);
    let name = cred.file_name().and_then(|name| name.to_str()).unwrap_or("flows_cred.json");
    cred.with_file_name(format!("{name}.prev"))
}

async fn read_or(path: &StdPath, empty: &[u8]) -> Result<Vec<u8>, String> {
    if path.exists() { tokio::fs::read(path).await.map_err(|err| err.to_string()) } else { Ok(empty.to_vec()) }
}

struct DiskSnapshot {
    live_flows: Option<Vec<u8>>,
    live_creds: Option<Vec<u8>>,
    prev_flows: Option<Vec<u8>>,
    prev_creds: Option<Vec<u8>>,
}

async fn read_optional(path: &StdPath) -> Result<Option<Vec<u8>>, String> {
    if path.exists() { tokio::fs::read(path).await.map_err(|err| err.to_string()).map(Some) } else { Ok(None) }
}

async fn snapshot_all(flows_path: &StdPath) -> Result<DiskSnapshot, String> {
    Ok(DiskSnapshot {
        live_flows: read_optional(flows_path).await?,
        live_creds: read_optional(&sidecar_path(flows_path)).await?,
        prev_flows: read_optional(&previous_flows_path(flows_path)).await?,
        prev_creds: read_optional(&previous_creds_path(flows_path)).await?,
    })
}

async fn put_file(path: PathBuf, bytes: Option<Vec<u8>>, private: bool) -> Result<(), String> {
    match bytes {
        Some(bytes) => atomic_file::write_bytes(&path, &bytes, private).await,
        None => match tokio::fs::remove_file(&path).await {
            Ok(()) => Ok(()),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(err) => Err(err.to_string()),
        },
    }
}

async fn restore_all(flows_path: &StdPath, snap: DiskSnapshot) -> Result<(), String> {
    put_file(sidecar_path(flows_path), snap.live_creds, true).await?;
    put_file(flows_path.to_path_buf(), snap.live_flows, false).await?;
    put_file(previous_creds_path(flows_path), snap.prev_creds, true).await?;
    put_file(previous_flows_path(flows_path), snap.prev_flows, false).await?;
    Ok(())
}

fn pair_replaces(
    flows_path: &StdPath,
    live_flows: Vec<u8>,
    live_creds: Vec<u8>,
    flows_bytes: Vec<u8>,
    cred_bytes: Vec<u8>,
) -> [FileReplace; 4] {
    [
        FileReplace { path: previous_flows_path(flows_path), bytes: live_flows, private: false },
        FileReplace { path: previous_creds_path(flows_path), bytes: live_creds, private: true },
        FileReplace { path: sidecar_path(flows_path), bytes: cred_bytes, private: true },
        FileReplace { path: flows_path.to_path_buf(), bytes: flows_bytes, private: false },
    ]
}

async fn replace_generation(files: &[FileReplace], fail_before: Option<usize>) -> Result<(), String> {
    match fail_before {
        Some(index) => atomic_file::replace_files_failing_before(files, index).await,
        None => atomic_file::replace_files(files).await,
    }
}

/// Snapshot the live pair and replace it as one write-set. A later rename restores every file
/// already replaced in this set.
pub async fn persist_pair(flows_path: &StdPath, flows: &[Value], stored: &Map<String, Value>) -> Result<(), String> {
    let store = CredentialStore::default();
    let _lock = store.lock(flows_path).await?;
    persist_pair_with_store(&store, flows_path, flows, stored).await
}

async fn persist_pair_with_store(
    store: &CredentialStore,
    flows_path: &StdPath,
    flows: &[Value],
    stored: &Map<String, Value>,
) -> Result<(), String> {
    persist_pair_inner(store, flows_path, flows, stored, None).await
}

async fn persist_pair_inner(
    store: &CredentialStore,
    flows_path: &StdPath,
    flows: &[Value],
    stored: &Map<String, Value>,
    fail_before: Option<usize>,
) -> Result<(), String> {
    let flows_bytes = serde_json::to_string_pretty(flows).map_err(|err| err.to_string())?.into_bytes();
    let live_flows = read_or(flows_path, b"[]").await?;
    let live_creds = read_or(&sidecar_path(flows_path), b"{}").await?;
    let cred_bytes = store.encode_for_write(flows_path, stored, &live_creds).await?;
    let files = pair_replaces(flows_path, live_flows, live_creds, flows_bytes, cred_bytes);
    replace_generation(&files, fail_before).await
}

pub fn with_credentials(flows: Vec<Value>, stored: &Map<String, Value>) -> Value {
    let mut attached = flows;
    flow_credentials::merge_into(&mut attached, stored);
    Value::Array(attached)
}

pub async fn load_flows_array(path: &StdPath) -> Result<Vec<Value>, String> {
    if !path.exists() {
        return Ok(vec![]);
    }
    let content = tokio::fs::read_to_string(path).await.map_err(|err| err.to_string())?;
    if content.trim().is_empty() {
        return Ok(vec![]);
    }
    serde_json::from_str(&content).map_err(|err| err.to_string())
}

pub async fn revision_on_disk(path: &StdPath) -> Result<String, String> {
    let flows = load_flows_array(path).await?;
    Ok(Engine::revision_of(&Value::Array(flows)))
}

/// Prepare, persist, and activate a stripped flow array plus its sidecar.
pub async fn commit(
    state: &WebState,
    flows_path: &StdPath,
    mut flows: Vec<Value>,
) -> Result<(Vec<Value>, String), DeployErr> {
    let _credential_lock = state.credentials.lock(flows_path).await.map_err(DeployErr::Internal)?;
    let mut stored = state
        .credentials
        .read_sidecar_while_locked(flows_path, &_credential_lock)
        .await
        .map_err(DeployErr::Internal)?;
    credentials::separate(&mut flows, &mut stored);
    let attached = with_credentials(flows.clone(), &stored);
    prepare(state, &attached).await?;
    let snapshot = snapshot_all(flows_path).await.map_err(DeployErr::Internal)?;
    persist_pair_with_store(&state.credentials, flows_path, &flows, &stored).await.map_err(DeployErr::Internal)?;
    let revision = Engine::revision_of(&Value::Array(flows.clone()));
    if let Err(err) = activate(state, attached).await {
        if let Err(restore) = restore_all(flows_path, snapshot).await {
            return Err(DeployErr::Internal(format!("activation failed; files were not restored: {restore}")));
        }
        return Err(err);
    }
    Ok((flows, revision))
}

fn parse_flows(bytes: &[u8]) -> Result<Vec<Value>, DeployErr> {
    if bytes.is_empty() || bytes == b"[]" {
        return Ok(vec![]);
    }
    serde_json::from_slice(bytes).map_err(|err| DeployErr::Internal(err.to_string()))
}

pub async fn rollback_pair(state: &WebState, flows_path: &StdPath) -> Result<String, DeployErr> {
    let _credential_lock = state.credentials.lock(flows_path).await.map_err(DeployErr::Internal)?;
    let prev = previous_flows_path(flows_path);
    if !prev.exists() {
        return Err(DeployErr::NotFound);
    }
    let live_flows = read_or(flows_path, b"[]").await.map_err(DeployErr::Internal)?;
    let live_creds = read_or(&sidecar_path(flows_path), b"{}").await.map_err(DeployErr::Internal)?;
    let prev_flows = tokio::fs::read(&prev).await.map_err(|err| DeployErr::Internal(err.to_string()))?;
    let prev_creds = read_or(&previous_creds_path(flows_path), b"{}").await.map_err(DeployErr::Internal)?;
    let decoded_prev = state.credentials.decode_bytes(flows_path, &prev_creds).await.map_err(DeployErr::Internal)?;
    let attached = with_credentials(parse_flows(&prev_flows)?, &decoded_prev);
    prepare(state, &attached).await?;
    replace_generation(
        &[
            FileReplace { path: sidecar_path(flows_path), bytes: prev_creds.clone(), private: true },
            FileReplace { path: flows_path.to_path_buf(), bytes: prev_flows.clone(), private: false },
            FileReplace { path: previous_creds_path(flows_path), bytes: live_creds.clone(), private: true },
            FileReplace { path: previous_flows_path(flows_path), bytes: live_flows.clone(), private: false },
        ],
        None,
    )
    .await
    .map_err(DeployErr::Internal)?;
    let flows = parse_flows(&prev_flows)?;
    let revision = Engine::revision_of(&Value::Array(flows));
    if let Err(err) = activate(state, attached).await {
        if let Err(restore) = atomic_file::replace_files(&[
            FileReplace { path: sidecar_path(flows_path), bytes: live_creds, private: true },
            FileReplace { path: flows_path.to_path_buf(), bytes: live_flows, private: false },
            FileReplace { path: previous_creds_path(flows_path), bytes: prev_creds, private: true },
            FileReplace { path: previous_flows_path(flows_path), bytes: prev_flows, private: false },
        ])
        .await
        {
            return Err(DeployErr::Internal(format!(
                "rollback activation failed; live files were not restored: {restore}"
            )));
        }
        return Err(err);
    }
    Ok(revision)
}

async fn prepare(state: &WebState, attached: &Value) -> Result<(), DeployErr> {
    let Some(engine) = state.engine.read().await.clone() else {
        return Ok(());
    };
    let registry = state.registry.read().await.clone();
    let Some(registry) = registry else {
        return Err(DeployErr::Internal("engine is not available".to_string()));
    };
    // The candidate sees the settings the live engine runs with (plugins, egress, ...), the same
    // ones `redeploy_flows` falls back to.
    Engine::prepare_flows(attached, &registry, engine.config().cloned()).map_err(DeployErr::Invalid)
}

async fn activate(state: &WebState, attached: Value) -> Result<(), DeployErr> {
    let engine_present = state.engine.read().await.is_some();
    if engine_present {
        state.redeploy_flows(attached).await.map_err(DeployErr::Invalid)
    } else {
        restart_engine(state).await;
        Ok(())
    }
}

async fn restart_engine(state: &WebState) {
    let restart_callback_guard = state.restart_callback.read().await;
    let flows_path_guard = state.flows_file_path.read().await;
    if let (Some(restart_callback), Some(flows_path)) = (restart_callback_guard.as_ref(), flows_path_guard.as_ref()) {
        log::info!("Triggering flow engine restart...");
        restart_callback(flows_path.clone());
        state.comms.send_notification("info", "Flow engine restart initiated").await;
    } else {
        log::warn!("No restart callback or flows path available");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use n2link_core::runtime::registry::RegistryBuilder;
    use serde_json::json;
    use std::sync::Arc;

    struct TempDir(std::path::PathBuf);
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn read(path: &StdPath) -> String {
        std::fs::read_to_string(path).unwrap_or_default()
    }

    #[tokio::test]
    async fn every_failed_deploy_rename_restores_the_four_file_generation() {
        for fail_before in 0..4 {
            let dir = TempDir(std::env::temp_dir().join(format!("edgelinkd-pair-{}", uuid::Uuid::new_v4())));
            std::fs::create_dir_all(&dir.0).unwrap();
            let flows = dir.0.join("flows.json");
            let paths = [flows.clone(), sidecar_path(&flows), previous_flows_path(&flows), previous_creds_path(&flows)];
            let originals = [
                br#"[{"id":"live","type":"tab"}]"#.to_vec(),
                br#"{"live":{"user":"original"}}"#.to_vec(),
                br#"[{"id":"previous","type":"tab"}]"#.to_vec(),
                br#"{"previous":{"user":"original"}}"#.to_vec(),
            ];
            for (path, bytes) in paths.iter().zip(&originals) {
                std::fs::write(path, bytes).unwrap();
            }
            let stored = json!({ "candidate": { "user": "operator" } }).as_object().unwrap().clone();
            let err = persist_pair_inner(
                &CredentialStore::default(),
                &flows,
                &[json!({ "id": "candidate", "type": "tab" })],
                &stored,
                Some(fail_before),
            )
            .await
            .unwrap_err();
            assert!(!err.contains("operator"));
            for (path, original) in paths.iter().zip(&originals) {
                assert_eq!(&std::fs::read(path).unwrap(), original, "failure before rename {fail_before}");
            }
        }
    }

    #[tokio::test]
    async fn every_failed_rollback_rename_restores_the_four_file_generation() {
        for fail_before in 0..4 {
            let dir = TempDir(std::env::temp_dir().join(format!("edgelinkd-rollback-{}", uuid::Uuid::new_v4())));
            std::fs::create_dir_all(&dir.0).unwrap();
            let flows = dir.0.join("flows.json");
            let paths = [sidecar_path(&flows), flows.clone(), previous_creds_path(&flows), previous_flows_path(&flows)];
            let originals = [
                br#"{"live":{"user":"original"}}"#.to_vec(),
                br#"[{"id":"live","type":"tab"}]"#.to_vec(),
                br#"{"previous":{"user":"original"}}"#.to_vec(),
                br#"[{"id":"previous","type":"tab"}]"#.to_vec(),
            ];
            for (path, bytes) in paths.iter().zip(&originals) {
                std::fs::write(path, bytes).unwrap();
            }
            let replacements = [
                FileReplace { path: paths[0].clone(), bytes: originals[2].clone(), private: true },
                FileReplace { path: paths[1].clone(), bytes: originals[3].clone(), private: false },
                FileReplace { path: paths[2].clone(), bytes: originals[0].clone(), private: true },
                FileReplace { path: paths[3].clone(), bytes: originals[1].clone(), private: false },
            ];
            let err = replace_generation(&replacements, Some(fail_before)).await.unwrap_err();
            assert!(err.contains("injected rename failure"), "{err}");
            for (path, original) in paths.iter().zip(&originals) {
                assert_eq!(&std::fs::read(path).unwrap(), original, "failure before rename {fail_before}");
            }
        }
    }

    #[tokio::test]
    async fn a_failed_activation_keeps_the_previous_rollback() {
        let dir = TempDir(std::env::temp_dir().join(format!("edgelinkd-pair-{}", uuid::Uuid::new_v4())));
        std::fs::create_dir_all(&dir.0).unwrap();
        let flows = dir.0.join("flows.json");
        let prev = previous_flows_path(&flows);
        let cred = sidecar_path(&flows);
        let prev_cred = previous_creds_path(&flows);
        let gen_a = json!([{ "id": "a", "type": "tab" }]);
        let gen_b = json!([{ "id": "b", "type": "tab" }]);
        std::fs::write(&prev, serde_json::to_vec_pretty(&gen_a).unwrap()).unwrap();
        std::fs::write(&flows, serde_json::to_vec_pretty(&gen_b).unwrap()).unwrap();
        std::fs::write(&prev_cred, br#"{"a":{"user":"old"}}"#).unwrap();
        std::fs::write(&cred, br#"{"b":{"user":"cur"}}"#).unwrap();

        let state = WebState::new();
        state.set_flows_file_path(flows.clone()).await;
        let registry = RegistryBuilder::default().build().unwrap();
        let engine = Engine::with_json(&registry, gen_b.clone(), None).unwrap();
        engine.start().await.unwrap();
        state.set_registry(registry).await;
        state.set_engine(Arc::new(engine.clone())).await;

        let err = commit(&state, &flows, vec![]).await.unwrap_err();
        assert!(matches!(err, DeployErr::Invalid(_) | DeployErr::Internal(_)), "{err:?}");
        assert_eq!(serde_json::from_str::<Value>(&read(&flows)).unwrap(), gen_b);
        assert_eq!(serde_json::from_str::<Value>(&read(&prev)).unwrap(), gen_a);
        assert!(read(&cred).contains("cur"), "{}", read(&cred));
        assert!(read(&prev_cred).contains("old"), "{}", read(&prev_cred));
        assert!(engine.is_running());
        assert!(engine.get_flow(&"b".parse().unwrap()).is_some());

        rollback_pair(&state, &flows).await.unwrap();
        assert_eq!(serde_json::from_str::<Value>(&read(&flows)).unwrap(), gen_a);
        assert_eq!(serde_json::from_str::<Value>(&read(&prev)).unwrap(), gen_b);
        assert!(read(&cred).contains("old"), "{}", read(&cred));
        assert!(read(&prev_cred).contains("cur"), "{}", read(&prev_cred));
        engine.stop().await.unwrap();
    }
}
