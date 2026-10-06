use crate::handlers::WebState;
use crate::handlers::deploy;
use crate::handlers::reply::api_error;
use crate::models::*;
use axum::http::HeaderMap;
use axum::response::{IntoResponse, Response};
use axum::{Extension, extract::Path, http::StatusCode, response::Json};
use n2link_core::runtime::engine::Engine;
use serde_json::Value;
use std::sync::Arc;

/// Get all flows (Node-RED compatible)
pub async fn get_flows(Extension(state): Extension<Arc<WebState>>) -> Result<Json<Value>, StatusCode> {
    let flows_path_guard = state.flows_file_path.read().await;
    let flows = if let Some(flows_path) = flows_path_guard.as_ref() {
        match deploy::load_flows_array(flows_path).await {
            Ok(flows) => flows,
            Err(e) => {
                log::error!("Failed to load flows from file: {e}");
                return Err(StatusCode::INTERNAL_SERVER_ERROR);
            }
        }
    } else {
        log::warn!("No flows file path configured");
        vec![]
    };

    let flows_value = serde_json::Value::Array(flows);
    let revision = Engine::revision_of(&flows_value);
    let response = serde_json::json!({
        "flows": flows_value,
        "rev": revision,
    });

    Ok(Json(response))
}

/// Deploy/update all flows (Node-RED compatible)
pub async fn post_flows(
    Extension(state): Extension<Arc<WebState>>,
    headers: HeaderMap,
    payload: String,
) -> Result<Response, StatusCode> {
    // The body can contain node passwords. Do not log it.
    let parsed_payload: FlowsPayload = match serde_json::from_str(&payload) {
        Ok(p) => p,
        Err(e) => {
            log::error!("Failed to parse flows payload: {e}");
            return Err(StatusCode::BAD_REQUEST);
        }
    };

    log::debug!("Received deployment request with rev: {:?}", parsed_payload.rev);
    log::debug!("Received deployment request with {} flows", parsed_payload.flows.len());

    let flows_path = {
        let flows_path_guard = state.flows_file_path.read().await;
        flows_path_guard.clone()
    };
    let Some(flows_path) = flows_path else {
        log::error!("No flows file path configured, cannot save flows");
        return Err(StatusCode::INTERNAL_SERVER_ERROR);
    };

    let _deploy = state.deploy.lock().await;
    let actor = state.auth.actor_from_headers(&headers);
    let deployed_count = parsed_payload.flows.len();
    state.history.record_deploy_proposed(&actor.username, parsed_payload.rev.as_deref(), "full", deployed_count);
    if let Some(offered) = parsed_payload.rev.as_deref() {
        let current = deploy::revision_on_disk(&flows_path).await.map_err(|err| {
            log::error!("Failed to read the current flows revision: {err}");
            StatusCode::INTERNAL_SERVER_ERROR
        })?;
        if offered != current {
            let _ = state.audit.record(&actor.username, "flows.deploy.rejected", Some(offered)).await;
            state.history.record_deploy_rejected(&actor.username, Some(offered), "full", "version_mismatch");
            return Ok(api_error(StatusCode::CONFLICT, "version_mismatch", "version mismatch"));
        }
    }

    match deploy::commit(&state, &flows_path, parsed_payload.flows).await {
        Ok((_, revision)) => {
            log::info!("Flows saved to file: {}", flows_path.display());
            state.comms.send_deploy_notification(true, Some(&revision)).await;
            state.comms.send_notification("success", &format!("Successfully deployed {deployed_count} flows")).await;
            let _ = state.audit.record(&actor.username, "flows.deploy", Some(&revision)).await;
            state.history.record_deploy_accepted(&actor.username, &revision, "full", deployed_count);
            Ok(Json(serde_json::json!({ "rev": revision })).into_response())
        }
        Err(err) => {
            state.comms.send_deploy_notification(false, Some("0")).await;
            state.comms.send_notification("error", "Failed to deploy flows").await;
            let _ = state.audit.record(&actor.username, "flows.deploy.rejected", None).await;
            state.history.record_deploy_rejected(
                &actor.username,
                parsed_payload.rev.as_deref(),
                "full",
                "commit_failed",
            );
            Ok(err.into_response())
        }
    }
}

/// Put the previous flows file back and redeploy it. A second call swaps the two files again.
pub async fn post_flows_rollback(
    Extension(state): Extension<Arc<WebState>>,
    headers: HeaderMap,
) -> Result<Response, StatusCode> {
    let flows_path = {
        let flows_path_guard = state.flows_file_path.read().await;
        flows_path_guard.clone()
    };
    let Some(flows_path) = flows_path else {
        log::error!("No flows file path configured, cannot roll back");
        return Err(StatusCode::INTERNAL_SERVER_ERROR);
    };
    let _deploy = state.deploy.lock().await;
    let actor = state.auth.actor_from_headers(&headers);
    match deploy::rollback_pair(&state, &flows_path).await {
        Ok(revision) => {
            let _ = state.audit.record(&actor.username, "flows.rollback", Some(&revision)).await;
            state.history.record_deploy_rollback(&actor.username, Some(&revision), true, None);
            state.comms.send_deploy_notification(true, Some(&revision)).await;
            Ok(Json(serde_json::json!({ "rev": revision })).into_response())
        }
        Err(err) => {
            state.history.record_deploy_rollback(&actor.username, None, false, Some("rollback_failed"));
            Ok(err.into_response())
        }
    }
}

/// Get flows state
pub async fn get_flows_state(Extension(state): Extension<Arc<WebState>>) -> Result<Json<Value>, StatusCode> {
    // Check if engine is available and its running state
    let engine_guard = state.engine.read().await;
    let (started, state_str) = if let Some(engine) = engine_guard.as_ref() {
        let is_running = engine.is_running();
        if is_running { (true, "started") } else { (false, "stopped") }
    } else {
        // If no engine instance, return stopped state
        (false, "stopped")
    };

    let response = serde_json::json!({
        "started": started,
        "state": state_str
    });

    Ok(Json(response))
}

/// Set flows state
pub async fn post_flows_state(
    Extension(state): Extension<Arc<WebState>>,
    Json(payload): Json<FlowState>,
) -> Result<Json<Value>, StatusCode> {
    log::info!("Setting flows state to: {}", payload.state);

    // Check if state value is valid
    let engine_guard = state.engine.read().await;
    let (started, state_str) = match payload.state.as_str() {
        "start" => {
            // Start flows
            if let Some(engine) = engine_guard.as_ref() {
                match engine.start().await {
                    Ok(_) => {
                        log::info!("Engine started successfully");
                        state.comms.send_notification("success", "Flow engine started").await;
                        (true, "started")
                    }
                    Err(e) => {
                        log::error!("Failed to start engine: {e}");
                        state.comms.send_notification("error", &format!("Failed to start engine: {e}")).await;
                        return Err(StatusCode::INTERNAL_SERVER_ERROR);
                    }
                }
            } else {
                log::warn!("No engine available to start");
                state.comms.send_notification("warning", "No engine available to start").await;
                (false, "stopped")
            }
        }
        "stop" => {
            // Stop flows
            if let Some(engine) = engine_guard.as_ref() {
                match engine.stop().await {
                    Ok(_) => {
                        log::info!("Engine stopped successfully");
                        state.comms.send_notification("success", "Flow engine stopped").await;
                        (false, "stopped")
                    }
                    Err(e) => {
                        log::error!("Failed to stop engine: {e}");
                        state.comms.send_notification("error", &format!("Failed to stop engine: {e}")).await;
                        return Err(StatusCode::INTERNAL_SERVER_ERROR);
                    }
                }
            } else {
                log::warn!("No engine available to stop");
                (false, "stopped")
            }
        }
        _ => {
            log::error!("Invalid state value: {}", payload.state);
            return Err(StatusCode::BAD_REQUEST);
        }
    };

    let response = serde_json::json!({
        "started": started,
        "state": state_str
    });

    Ok(Json(response))
}

/// Get single flow
pub async fn get_flow(
    Extension(state): Extension<Arc<WebState>>,
    Path(id): Path<String>,
) -> Result<Json<Value>, StatusCode> {
    let flows_path_guard = state.flows_file_path.read().await;
    let flows = if let Some(flows_path) = flows_path_guard.as_ref() {
        match deploy::load_flows_array(flows_path).await {
            Ok(flows) => flows,
            Err(e) => {
                log::error!("Failed to load flows from file: {e}");
                return Err(StatusCode::INTERNAL_SERVER_ERROR);
            }
        }
    } else {
        log::warn!("No flows file path configured");
        return Err(StatusCode::INTERNAL_SERVER_ERROR);
    };

    // Find the specified flow
    for flow in &flows {
        if let Some(flow_id) = flow.get("id").and_then(|v| v.as_str())
            && let Some(flow_type) = flow.get("type").and_then(|v| v.as_str())
            && flow_id == id
            && flow_type == "tab"
        {
            return Ok(Json(flow.clone()));
        }
    }

    Err(StatusCode::NOT_FOUND)
}

/// Create new flow
pub async fn post_flow(
    Extension(state): Extension<Arc<WebState>>,
    Json(payload): Json<serde_json::Value>,
) -> Result<Json<Value>, deploy::DeployErr> {
    let flows_path = state.flows_file_path.read().await.clone();
    let Some(flows_path) = flows_path else {
        log::warn!("No flows file path configured");
        return Err(deploy::DeployErr::Internal("no flows file path configured".to_string()));
    };
    let _deploy = state.deploy.lock().await;
    let mut flows = match deploy::load_flows_array(&flows_path).await {
        Ok(flows) => flows,
        Err(e) => {
            log::error!("Failed to load flows from file: {e}");
            return Err(deploy::DeployErr::Internal(e));
        }
    };

    let created = payload.clone();
    flows.push(payload);
    deploy::commit(&state, &flows_path, flows).await?;
    Ok(Json(created))
}

/// Update flow
pub async fn put_flow(
    Extension(state): Extension<Arc<WebState>>,
    Path(id): Path<String>,
    Json(payload): Json<serde_json::Value>,
) -> Result<Json<Value>, deploy::DeployErr> {
    let flows_path = state.flows_file_path.read().await.clone();
    let Some(flows_path) = flows_path else {
        log::warn!("No flows file path configured");
        return Err(deploy::DeployErr::Internal("no flows file path configured".to_string()));
    };
    let _deploy = state.deploy.lock().await;
    let mut flows = match deploy::load_flows_array(&flows_path).await {
        Ok(flows) => flows,
        Err(e) => {
            log::error!("Failed to load flows from file: {e}");
            return Err(deploy::DeployErr::Internal(e));
        }
    };

    let mut found = false;
    for flow in &mut flows {
        if let Some(flow_id) = flow.get("id").and_then(|v| v.as_str())
            && flow_id == id
        {
            *flow = payload.clone();
            found = true;
            break;
        }
    }

    if !found {
        return Err(deploy::DeployErr::MissingFlow);
    }

    deploy::commit(&state, &flows_path, flows).await?;
    Ok(Json(payload))
}

/// Delete flow
pub async fn delete_flow(
    Extension(state): Extension<Arc<WebState>>,
    Path(id): Path<String>,
) -> Result<StatusCode, deploy::DeployErr> {
    let flows_path = state.flows_file_path.read().await.clone();
    let Some(flows_path) = flows_path else {
        log::warn!("No flows file path configured");
        return Err(deploy::DeployErr::Internal("no flows file path configured".to_string()));
    };
    let _deploy = state.deploy.lock().await;
    let mut flows = match deploy::load_flows_array(&flows_path).await {
        Ok(flows) => flows,
        Err(e) => {
            log::error!("Failed to load flows from file: {e}");
            return Err(deploy::DeployErr::Internal(e));
        }
    };

    let initial_len = flows.len();
    flows.retain(|flow| flow.get("id").and_then(|v| v.as_str()).is_none_or(|flow_id| flow_id != id));

    if flows.len() < initial_len {
        deploy::commit(&state, &flows_path, flows).await?;
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(deploy::DeployErr::MissingFlow)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::create_all_routes;
    use axum::body::Body;
    use axum::http::Request;
    use serde_json::json;
    use std::path::Path as StdPath;
    use tower::ServiceExt;

    struct TempDir(std::path::PathBuf);
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    async fn router() -> (axum::Router, TempDir, std::path::PathBuf) {
        let dir = TempDir(std::env::temp_dir().join(format!("edgelinkd-flows-{}", uuid::Uuid::new_v4())));
        std::fs::create_dir_all(&dir.0).unwrap();
        let flows = dir.0.join("flows.json");
        std::fs::write(&flows, b"[]").unwrap();
        let state = WebState::new();
        state.set_flows_file_path(flows.clone()).await;
        let router = create_all_routes(&state).layer(Extension(state));
        (router, dir, flows)
    }

    #[cfg(feature = "credential_encryption")]
    async fn encrypted_router()
    -> (axum::Router, TempDir, std::path::PathBuf, n2link_core::runtime::credential_storage::CredentialStore) {
        use n2link_core::runtime::credential_storage::{CredentialStore, previous_credential_path};
        use n2link_core::runtime::flow_credentials::sidecar_path;

        let dir = TempDir(std::env::temp_dir().join(format!("edgelinkd-encrypted-flows-{}", uuid::Uuid::new_v4())));
        std::fs::create_dir_all(&dir.0).unwrap();
        let flows = dir.0.join("flows.json");
        std::fs::write(&flows, b"[]").unwrap();
        std::fs::write(deploy::previous_flows_path(&flows), b"[]").unwrap();
        std::fs::write(sidecar_path(&flows), b"{}").unwrap();
        std::fs::write(previous_credential_path(&flows), b"{}").unwrap();
        let store = CredentialStore::default();
        store.migrate(&flows, false, Some(&dir.0.join("backup"))).await.unwrap();
        let state = WebState::new();
        state.set_flows_file_path(flows.clone()).await;
        let router = create_all_routes(&state).layer(Extension(state));
        (router, dir, flows, store)
    }

    async fn call(router: &axum::Router, method: &str, uri: &str, body: Option<Value>) -> (StatusCode, Value) {
        let mut builder = Request::builder().method(method).uri(uri);
        let payload = if let Some(body) = body {
            builder = builder.header("content-type", "application/json");
            serde_json::to_vec(&body).unwrap()
        } else {
            Vec::new()
        };
        let response = router.clone().oneshot(builder.body(Body::from(payload)).unwrap()).await.unwrap();
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let parsed = if bytes.is_empty() { Value::Null } else { serde_json::from_slice(&bytes).unwrap() };
        (status, parsed)
    }

    fn on_disk(path: &StdPath) -> Value {
        serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
    }

    #[tokio::test]
    async fn a_stale_rev_is_rejected_and_rollback_swaps_back() {
        let (router, _dir, flows) = router().await;
        let first = json!([{ "id": "a", "type": "tab" }]);
        let (status, _) = call(&router, "POST", "/flows", Some(json!({ "flows": first }))).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(on_disk(&flows), json!([{ "id": "a", "type": "tab" }]));

        let second = json!([{ "id": "b", "type": "tab" }]);
        let (status, body) = call(&router, "POST", "/flows", Some(json!({ "flows": second, "rev": "stale" }))).await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(body["code"], "version_mismatch");
        assert_eq!(on_disk(&flows), json!([{ "id": "a", "type": "tab" }]));

        let (status, body) = call(&router, "GET", "/flows", None).await;
        assert_eq!(status, StatusCode::OK);
        let rev = body["rev"].as_str().unwrap().to_string();
        let (status, _) = call(&router, "POST", "/flows", Some(json!({ "flows": second, "rev": rev }))).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(on_disk(&flows), json!([{ "id": "b", "type": "tab" }]));

        let (status, _) = call(&router, "POST", "/flows/rollback", None).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(on_disk(&flows), json!([{ "id": "a", "type": "tab" }]));
        let (status, _) = call(&router, "POST", "/flows/rollback", None).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(on_disk(&flows), json!([{ "id": "b", "type": "tab" }]));

        std::fs::remove_file(deploy::previous_flows_path(&flows)).unwrap();
        let (status, body) = call(&router, "POST", "/flows/rollback", None).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(body["message"], "no previous flows");
        assert_eq!(on_disk(&flows), json!([{ "id": "b", "type": "tab" }]));

        let log = std::fs::read_to_string(_dir.0.join("audit.log")).unwrap();
        assert!(log.contains("flows.deploy.rejected"));
        assert!(log.contains("anonymous"));
        assert!(log.contains("stale"));
    }

    #[tokio::test]
    async fn mqtt_broker_credentials_stay_out_of_the_flow_file() {
        let (router, dir, flows) = router().await;
        let (status, body) = call(&router, "GET", "/credentials/mqtt-broker/missing", None).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, json!({}));

        let broker = json!({
            "id": "b",
            "type": "mqtt-broker",
            "broker": "localhost",
            "credentials": { "user": "operator", "password": "secret-value" }
        });
        let (status, _) = call(&router, "POST", "/flows", Some(json!({ "flows": [broker] }))).await;
        assert_eq!(status, StatusCode::OK);
        let saved = std::fs::read_to_string(&flows).unwrap();
        assert!(!saved.contains("secret-value"));
        assert!(!saved.contains("credentials"));
        let (status, body) = call(&router, "GET", "/flows", None).await;
        assert_eq!(status, StatusCode::OK);
        assert!(body["flows"][0].get("credentials").is_none());
        let rev = body["rev"].as_str().unwrap().to_string();

        let (status, body) = call(&router, "GET", "/credentials/mqtt-broker/b", None).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["user"], "operator");
        assert_eq!(body["has_password"], true);
        assert!(body.get("password").is_none());
        let sidecar = std::fs::read_to_string(dir.0.join("flows_cred.json")).unwrap();
        assert!(sidecar.contains("secret-value"));
        assert!(!body.to_string().contains("secret-value"));

        let kept = json!({
            "flows": [{
                "id": "b",
                "type": "mqtt-broker",
                "broker": "localhost",
                "credentials": { "user": "operator", "password": "__PWRD__" }
            }],
            "rev": rev
        });
        let (status, _) = call(&router, "POST", "/flows", Some(kept)).await;
        assert_eq!(status, StatusCode::OK);
        let sidecar = std::fs::read_to_string(dir.0.join("flows_cred.json")).unwrap();
        assert!(sidecar.contains("secret-value"));

        let changed = json!({
            "id": "b",
            "type": "mqtt-broker",
            "broker": "localhost",
            "credentials": { "user": "operator", "password": "other-secret" }
        });
        let (status, _) = call(&router, "POST", "/flows", Some(json!({ "flows": [changed] }))).await;
        assert_eq!(status, StatusCode::OK);
        let (status, _) = call(&router, "POST", "/flows/rollback", None).await;
        assert_eq!(status, StatusCode::OK);
        let sidecar = std::fs::read_to_string(dir.0.join("flows_cred.json")).unwrap();
        assert!(sidecar.contains("secret-value"));
        assert!(!sidecar.contains("other-secret"));

        let cleared = json!({
            "id": "b",
            "type": "mqtt-broker",
            "broker": "localhost",
            "credentials": { "user": "operator", "password": "" }
        });
        let (status, _) = call(&router, "POST", "/flows", Some(json!({ "flows": [cleared] }))).await;
        assert_eq!(status, StatusCode::OK);
        let (status, body) = call(&router, "GET", "/credentials/mqtt-broker/b", None).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["user"], "operator");
        assert_eq!(body["has_password"], false);
        let sidecar = std::fs::read_to_string(dir.0.join("flows_cred.json")).unwrap();
        assert!(!sidecar.contains("secret-value"));
        assert!(!sidecar.contains("other-secret"));
        let log = std::fs::read_to_string(dir.0.join("audit.log")).unwrap();
        assert!(!log.contains("secret-value"));
        assert!(!log.contains("other-secret"));
    }

    #[cfg(feature = "credential_encryption")]
    #[tokio::test]
    async fn encrypted_credentials_keep_placeholder_clear_and_rollback_semantics() {
        let (router, dir, flows, store) = encrypted_router().await;
        let initial = json!({
            "id": "b",
            "type": "mqtt-broker",
            "broker": "localhost",
            "credentials": { "user": "operator", "password": "fixture-secret" }
        });
        let (status, _) = call(&router, "POST", "/flows", Some(json!({ "flows": [initial] }))).await;
        assert_eq!(status, StatusCode::OK);
        let raw = std::fs::read_to_string(dir.0.join("flows_cred.json")).unwrap();
        assert!(raw.contains("edgelink-credentials"));
        assert!(!raw.contains("fixture-secret"));
        let (status, view) = call(&router, "GET", "/credentials/mqtt-broker/b", None).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(view, json!({ "user": "operator", "has_password": true }));
        let (_, current) = call(&router, "GET", "/flows", None).await;

        let keep = json!({
            "flows": [{
                "id": "b",
                "type": "mqtt-broker",
                "broker": "localhost",
                "credentials": { "user": "operator", "password": "__PWRD__" }
            }],
            "rev": current["rev"]
        });
        assert_eq!(call(&router, "POST", "/flows", Some(keep)).await.0, StatusCode::OK);
        assert_eq!(store.read_sidecar(&flows).await.unwrap()["b"]["password"], "fixture-secret");

        let changed = json!({
            "id": "b",
            "type": "mqtt-broker",
            "broker": "localhost",
            "credentials": { "user": "operator", "password": "fixture-changed" }
        });
        assert_eq!(call(&router, "POST", "/flows", Some(json!({ "flows": [changed] }))).await.0, StatusCode::OK);
        assert_eq!(call(&router, "POST", "/flows/rollback", None).await.0, StatusCode::OK);
        assert_eq!(store.read_sidecar(&flows).await.unwrap()["b"]["password"], "fixture-secret");

        let cleared = json!({
            "id": "b",
            "type": "mqtt-broker",
            "broker": "localhost",
            "credentials": { "user": "operator", "password": "" }
        });
        assert_eq!(call(&router, "POST", "/flows", Some(json!({ "flows": [cleared] }))).await.0, StatusCode::OK);
        assert!(store.read_sidecar(&flows).await.unwrap()["b"].get("password").is_none());
        let audit = std::fs::read_to_string(dir.0.join("audit.log")).unwrap();
        assert!(!audit.contains("fixture-secret"));
        assert!(!audit.contains("fixture-changed"));
    }

    #[cfg(feature = "credential_encryption")]
    #[tokio::test]
    async fn encrypted_deploy_waits_for_the_credential_transaction_lock() {
        let (router, _dir, flows, store) = encrypted_router().await;
        let lock = store.lock(&flows).await.unwrap();
        let mut deploy = tokio::spawn(async move {
            call(
                &router,
                "POST",
                "/flows",
                Some(json!({
                    "flows": [{
                        "id": "b",
                        "type": "mqtt-broker",
                        "broker": "localhost",
                        "credentials": { "password": "fixture-secret" }
                    }]
                })),
            )
            .await
        });
        assert!(tokio::time::timeout(std::time::Duration::from_millis(50), &mut deploy).await.is_err());
        drop(lock);
        let (status, body) = tokio::time::timeout(std::time::Duration::from_secs(2), deploy).await.unwrap().unwrap();
        assert_eq!(status, StatusCode::OK, "{body}");
    }

    #[tokio::test]
    async fn credentials_without_a_flows_file_are_empty() {
        let state = WebState::new();
        let router = create_all_routes(&state).layer(Extension(state));
        let (status, body) = call(&router, "GET", "/credentials/mqtt-broker/new", None).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, json!({}));
    }

    #[tokio::test]
    async fn two_deploys_with_the_same_revision_serialize() {
        let (router, _dir, flows) = router().await;
        let first = json!([{ "id": "a", "type": "tab" }]);
        let (status, body) = call(&router, "POST", "/flows", Some(json!({ "flows": first }))).await;
        assert_eq!(status, StatusCode::OK);
        let rev = body["rev"].as_str().unwrap().to_string();
        let left = json!({ "flows": [{ "id": "l", "type": "tab" }], "rev": rev });
        let right = json!({ "flows": [{ "id": "r", "type": "tab" }], "rev": rev });
        let left_call = call(&router, "POST", "/flows", Some(left));
        let right_call = call(&router, "POST", "/flows", Some(right));
        let ((s1, b1), (s2, b2)) = tokio::join!(left_call, right_call);
        let statuses = [s1, s2];
        assert!(statuses.contains(&StatusCode::OK), "{statuses:?}");
        assert!(statuses.contains(&StatusCode::CONFLICT), "{statuses:?} {b1} {b2}");
        let winner = if s1 == StatusCode::OK { "l" } else { "r" };
        assert_eq!(on_disk(&flows)[0]["id"], winner);
    }

    #[tokio::test]
    async fn an_invalid_node_does_not_change_disk_or_runtime() {
        let dir = TempDir(std::env::temp_dir().join(format!("edgelinkd-flows-{}", uuid::Uuid::new_v4())));
        std::fs::create_dir_all(&dir.0).unwrap();
        let flows = dir.0.join("flows.json");
        std::fs::write(&flows, b"[]").unwrap();
        let state = WebState::new();
        state.set_flows_file_path(flows.clone()).await;
        let registry = n2link_core::runtime::registry::RegistryBuilder::default().build().unwrap();
        let engine = Engine::with_json(&registry, json!([{ "id": "a", "type": "tab" }]), None).unwrap();
        engine.start().await.unwrap();
        state.set_registry(registry).await;
        state.set_engine(std::sync::Arc::new(engine.clone())).await;
        let router = create_all_routes(&state).layer(Extension(state));
        let (status, body) = call(
            &router,
            "POST",
            "/flows",
            Some(json!({
                "flows": [{ "id": "b", "type": "mqtt-broker", "broker": "localhost", "usetls": true, "credentials": { "password": "secret-value" } }]
            })),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        assert_eq!(body["code"], "invalid_flows");
        assert!(!body.to_string().contains("secret-value"));
        assert_eq!(on_disk(&flows), json!([]));
        assert!(engine.is_running());
        assert!(engine.get_flow(&"a".parse().unwrap()).is_some());
        let log = std::fs::read_to_string(dir.0.join("audit.log")).unwrap_or_default();
        assert!(!log.contains("secret-value"));
        engine.stop().await.unwrap();
    }

    #[tokio::test]
    async fn an_invalid_single_flow_does_not_change_disk_or_runtime() {
        let dir = TempDir(std::env::temp_dir().join(format!("edgelinkd-flows-{}", uuid::Uuid::new_v4())));
        std::fs::create_dir_all(&dir.0).unwrap();
        let flows = dir.0.join("flows.json");
        std::fs::write(&flows, br#"[{"id":"a","type":"tab"}]"#).unwrap();
        let state = WebState::new();
        state.set_flows_file_path(flows.clone()).await;
        let registry = n2link_core::runtime::registry::RegistryBuilder::default().build().unwrap();
        let engine = Engine::with_json(&registry, json!([{ "id": "a", "type": "tab" }]), None).unwrap();
        engine.start().await.unwrap();
        state.set_registry(registry).await;
        state.set_engine(std::sync::Arc::new(engine.clone())).await;
        let router = create_all_routes(&state).layer(Extension(state));
        let (status, body) = call(
            &router,
            "POST",
            "/flow",
            Some(json!({
                "id": "b",
                "type": "mqtt-broker",
                "broker": "localhost",
                "usetls": true,
                "credentials": { "password": "secret-value" }
            })),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        assert_eq!(body["code"], "invalid_flows");
        assert!(!body.to_string().contains("secret-value"));
        assert_eq!(on_disk(&flows), json!([{ "id": "a", "type": "tab" }]));
        assert!(engine.is_running());
        assert!(engine.get_flow(&"a".parse().unwrap()).is_some());
        engine.stop().await.unwrap();
    }

    #[tokio::test]
    async fn two_deploys_leave_the_engine_on_the_disk_winner() {
        let dir = TempDir(std::env::temp_dir().join(format!("edgelinkd-flows-{}", uuid::Uuid::new_v4())));
        std::fs::create_dir_all(&dir.0).unwrap();
        let flows = dir.0.join("flows.json");
        std::fs::write(&flows, br#"[{"id":"a","type":"tab"}]"#).unwrap();
        let state = WebState::new();
        state.set_flows_file_path(flows.clone()).await;
        let registry = n2link_core::runtime::registry::RegistryBuilder::default().build().unwrap();
        let engine = Engine::with_json(&registry, json!([{ "id": "a", "type": "tab" }]), None).unwrap();
        engine.start().await.unwrap();
        state.set_registry(registry).await;
        state.set_engine(std::sync::Arc::new(engine.clone())).await;
        let router = create_all_routes(&state).layer(Extension(state));
        let (status, body) = call(&router, "GET", "/flows", None).await;
        assert_eq!(status, StatusCode::OK);
        let rev = body["rev"].as_str().unwrap().to_string();
        let left = json!({ "flows": [{ "id": "1", "type": "tab" }], "rev": rev });
        let right = json!({ "flows": [{ "id": "2", "type": "tab" }], "rev": rev });
        let ((s1, _), (s2, _)) =
            tokio::join!(call(&router, "POST", "/flows", Some(left)), call(&router, "POST", "/flows", Some(right)));
        let statuses = [s1, s2];
        assert!(statuses.contains(&StatusCode::OK), "{statuses:?}");
        assert!(statuses.contains(&StatusCode::CONFLICT), "{statuses:?}");
        let winner = on_disk(&flows)[0]["id"].as_str().unwrap().to_string();
        assert!(engine.is_running());
        assert!(engine.get_flow(&winner.parse().unwrap()).is_some(), "runtime missing winner {winner}");
        assert!(engine.get_flow(&"a".parse().unwrap()).is_none());
        engine.stop().await.unwrap();
    }
}
