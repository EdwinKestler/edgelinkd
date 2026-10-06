//! One document a poller can read. This process does not grow a dashboard.
//!
//! `uptimeMs` is how long this process has been up. A redeploy does not reset it.
//! `errors` and each link's `errors` count node errors since the current engine started.
//! Context ages are the last write in this process. A restart clears them.

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use axum::Extension;
use axum::Json;
use axum::response::{IntoResponse, Response};
use n2link_core::runtime::engine::Engine;
use serde_json::{Value, json};

use super::WebState;

pub async fn get_status(Extension(state): Extension<Arc<WebState>>) -> Response {
    let uptime_ms = state.started_at.elapsed().as_millis() as u64;
    let rev = match state.flows_file_path.read().await.as_ref() {
        Some(path) => match tokio::fs::read_to_string(path).await {
            Ok(text) if text.trim().is_empty() => Engine::revision_of(&json!([])),
            Ok(text) => match serde_json::from_str::<Vec<Value>>(&text) {
                Ok(flows) => Engine::revision_of(&Value::Array(flows)),
                Err(err) => {
                    return super::reply::api_error(
                        axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                        "unexpected_error",
                        &format!("flows file is not valid: {err}"),
                    );
                }
            },
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Engine::revision_of(&json!([])),
            Err(err) => {
                return super::reply::api_error(
                    axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                    "unexpected_error",
                    &format!("flows file was not read: {err}"),
                );
            }
        },
        None => Engine::revision_of(&json!([])),
    };

    let engine = state.engine.read().await.clone();
    let (errors, links, context_ages) = if let Some(engine) = engine.as_ref() {
        let now = unix_ms();
        let ages = engine
            .get_context_manager()
            .ages()
            .into_iter()
            .map(|age| {
                json!({
                    "scope": age.scope,
                    "store": age.store,
                    "key": age.key,
                    "updatedAt": age.updated_at,
                    "ageMs": (now - age.updated_at).max(0),
                })
            })
            .collect::<Vec<_>>();
        let links = engine
            .link_states()
            .into_iter()
            .map(|link| {
                json!({
                    "id": link.id,
                    "type": link.type_name,
                    "text": link.text,
                    "errors": link.errors,
                })
            })
            .collect::<Vec<_>>();
        (engine.error_count(), links, ages)
    } else {
        (0, Vec::new(), Vec::new())
    };

    #[allow(unused_mut)]
    let mut status_json = json!({
        "rev": rev,
        "uptimeMs": uptime_ms,
        "errors": errors,
        "contextAges": context_ages,
        "links": links,
    });

    #[cfg(feature = "history_sqlite")]
    {
        status_json["history"] = serde_json::to_value(state.history.health()).unwrap_or(Value::Null);
    }

    #[cfg(feature = "nodes_wasm")]
    if let Some(engine) = engine.as_ref() {
        status_json["wasm"] = serde_json::to_value(engine.wasm_status()).unwrap_or(Value::Null);
    }

    Json(status_json).into_response()
}

fn unix_ms() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|elapsed| elapsed.as_millis() as i64).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::create_all_routes;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    #[tokio::test]
    async fn status_reports_the_revision_and_empty_links() {
        let dir = std::env::temp_dir().join(format!("n2linkd-status-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let flows = dir.join("flows.json");
        std::fs::write(&flows, b"[]").unwrap();
        let state = WebState::new();
        state.set_flows_file_path(flows.clone()).await;
        let router = create_all_routes(&state).layer(Extension(state));
        let response =
            router.clone().oneshot(Request::builder().uri("/status").body(Body::empty()).unwrap()).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let body: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(body["rev"], Engine::revision_of(&json!([])));
        assert!(body["uptimeMs"].as_u64().is_some());
        assert_eq!(body["errors"], 0);
        assert_eq!(body["contextAges"], json!([]));
        assert_eq!(body["links"], json!([]));

        std::fs::write(&flows, b"{").unwrap();
        let response = router.oneshot(Request::builder().uri("/status").body(Body::empty()).unwrap()).await.unwrap();
        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let body: Value = serde_json::from_slice(&bytes).unwrap();
        assert!(body["message"].as_str().unwrap().contains("flows file is not valid"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn status_drops_links_for_removed_mqtt_nodes() {
        let dir = std::env::temp_dir().join(format!("n2linkd-status-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let flows = dir.join("flows.json");
        std::fs::write(&flows, br#"[{"id":"100","type":"tab"}]"#).unwrap();
        let state = WebState::new();
        state.set_flows_file_path(flows.clone()).await;
        let registry = n2link_core::runtime::registry::RegistryBuilder::default().build().unwrap();
        let engine = Engine::with_json(&registry, json!([{ "id": "100", "type": "tab" }]), None).unwrap();
        engine.start().await.unwrap();
        state.set_registry(registry).await;
        state.set_engine(std::sync::Arc::new(engine.clone())).await;
        crate::handlers::deploy::commit(
            &state,
            &flows,
            vec![
                json!({ "id": "100", "type": "tab" }),
                json!({ "id": "b1", "type": "mqtt-broker", "broker": "127.0.0.1", "autoConnect": false }),
                json!({
                    "id": "2",
                    "z": "100",
                    "type": "mqtt in",
                    "broker": "b1",
                    "topic": "n2linkd/status-link",
                    "qos": 0,
                    "datatype": "utf8",
                    "wires": []
                }),
            ],
        )
        .await
        .unwrap();
        let mqtt_id: n2link_core::runtime::model::ElementId = "2".parse().unwrap();
        engine.report_node_status(
            mqtt_id,
            n2link_core::runtime::nodes::StatusObject {
                fill: Some(n2link_core::runtime::nodes::StatusFill::Green),
                shape: Some(n2link_core::runtime::nodes::StatusShape::Dot),
                text: Some("connected".to_owned()),
            },
        );
        let mqtt_id_text = mqtt_id.to_string();
        let router = create_all_routes(&state).layer(Extension(state.clone()));
        let response =
            router.clone().oneshot(Request::builder().uri("/status").body(Body::empty()).unwrap()).await.unwrap();
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let body: Value = serde_json::from_slice(&bytes).unwrap();
        let links = body["links"].as_array().unwrap();
        assert!(links.iter().any(|link| link["id"] == mqtt_id_text), "{body}");

        crate::handlers::deploy::commit(&state, &flows, vec![json!({ "id": "100", "type": "tab" })]).await.unwrap();
        let response = router.oneshot(Request::builder().uri("/status").body(Body::empty()).unwrap()).await.unwrap();
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let body: Value = serde_json::from_slice(&bytes).unwrap();
        let links = body["links"].as_array().unwrap();
        assert!(links.iter().all(|link| link["id"] != mqtt_id_text), "stale mqtt link remained: {body}");
        engine.stop().await.unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }
}
