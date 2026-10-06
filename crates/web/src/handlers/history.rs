//! HTTP query endpoint for operational history.
//!
//! Route: GET /history
//! Classified as `editor_admin`, protected by `history.read`.

use std::collections::HashMap;
use std::sync::Arc;

use axum::Extension;
use axum::extract::Query;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use edgelink_core::runtime::history::{HistoryQuery, HistoryQueryError};

use super::WebState;
use super::reply::api_error;

const VALID_CATEGORIES: &[&str] = &["deploy", "node", "copilot", "fleet", "runtime", "history", "plugin"];

pub async fn get_history(
    Extension(state): Extension<Arc<WebState>>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let mut filter = HistoryQuery {
        limit: 100,
        before: None,
        since_ms: None,
        until_ms: None,
        category: None,
        kind: None,
        subject: None,
    };

    for (k, v) in &params {
        match k.as_str() {
            "limit" => {
                let parsed = match v.parse::<usize>() {
                    Ok(n) if (1..=500).contains(&n) => n,
                    _ => {
                        return api_error(StatusCode::BAD_REQUEST, "invalid_filter", "limit must be between 1 and 500");
                    }
                };
                filter.limit = parsed;
            }
            "before" => {
                let parsed = match v.parse::<i64>() {
                    Ok(n) if n > 0 => n,
                    _ => {
                        return api_error(
                            StatusCode::BAD_REQUEST,
                            "invalid_filter",
                            "before must be a positive integer",
                        );
                    }
                };
                filter.before = Some(parsed);
            }
            "since_ms" => {
                let parsed = match v.parse::<i64>() {
                    Ok(n) => n,
                    _ => return api_error(StatusCode::BAD_REQUEST, "invalid_filter", "since_ms must be an integer"),
                };
                filter.since_ms = Some(parsed);
            }
            "until_ms" => {
                let parsed = match v.parse::<i64>() {
                    Ok(n) => n,
                    _ => return api_error(StatusCode::BAD_REQUEST, "invalid_filter", "until_ms must be an integer"),
                };
                filter.until_ms = Some(parsed);
            }
            "category" => {
                if !VALID_CATEGORIES.contains(&v.as_str()) {
                    return api_error(
                        StatusCode::BAD_REQUEST,
                        "invalid_filter",
                        "category must be one of: deploy, node, copilot, fleet, runtime, history",
                    );
                }
                filter.category = Some(v.clone());
            }
            "kind" => {
                let trimmed = v.trim();
                if trimmed.is_empty() || trimmed.len() > 64 {
                    return api_error(StatusCode::BAD_REQUEST, "invalid_filter", "kind must be 1..=64 characters");
                }
                let mut chars = trimmed.chars();
                let first = chars.next().unwrap();
                if !first.is_ascii_lowercase() || !chars.all(|c| c.is_ascii_lowercase() || c == '.' || c == '_') {
                    return api_error(
                        StatusCode::BAD_REQUEST,
                        "invalid_filter",
                        "kind must match ^[a-z][a-z._]{0,63}$",
                    );
                }
                filter.kind = Some(trimmed.to_string());
            }
            "subject" => {
                let trimmed = v.trim();
                if trimmed.is_empty() || trimmed.len() > 128 || trimmed.chars().any(|c| c.is_control()) {
                    return api_error(
                        StatusCode::BAD_REQUEST,
                        "invalid_filter",
                        "subject must be 1..=128 characters without control characters",
                    );
                }
                filter.subject = Some(trimmed.to_string());
            }
            unknown => {
                return api_error(
                    StatusCode::BAD_REQUEST,
                    "invalid_filter",
                    &format!("unknown query parameter: {unknown}"),
                );
            }
        }
    }

    if let (Some(since), Some(until)) = (filter.since_ms, filter.until_ms)
        && since > until
    {
        return api_error(StatusCode::BAD_REQUEST, "invalid_filter", "since_ms must be less than or equal to until_ms");
    }

    match state.history.query(filter) {
        Ok(result) => axum::Json(result).into_response(),
        Err(HistoryQueryError::Disabled) => api_error(StatusCode::NOT_FOUND, "not_supported", "history is not enabled"),
        Err(HistoryQueryError::Failed(err)) => api_error(StatusCode::SERVICE_UNAVAILABLE, "history_unavailable", &err),
        Err(HistoryQueryError::InvalidFilter(msg)) => api_error(StatusCode::BAD_REQUEST, "invalid_filter", &msg),
        Err(HistoryQueryError::Internal(err)) => api_error(StatusCode::INTERNAL_SERVER_ERROR, "unexpected_error", &err),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::create_all_routes;
    use crate::handlers::fleet::Fleet;
    use crate::handlers::web_state::WebRuntimeServices;
    use crate::models::RedSystemSettings;
    use axum::body::{Body, to_bytes};
    use axum::http::Request;
    use edgelink_core::runtime::history::{HistoryConfig, HistoryHandle};
    use tower::ServiceExt;

    #[tokio::test]
    async fn test_history_disabled_returns_404() {
        let state = WebState::new();
        let router = create_all_routes(&state).layer(Extension(state));
        let response = router.oneshot(Request::builder().uri("/history").body(Body::empty()).unwrap()).await.unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn test_history_query_and_validation() {
        let temp_dir = std::env::temp_dir().join(format!("edgelinkd-web-hist-{}", uuid::Uuid::new_v4()));
        let _ = std::fs::create_dir_all(&temp_dir);
        let db_path = temp_dir.join("history.sqlite3");

        let config = HistoryConfig {
            enabled: true,
            path: Some(db_path.to_str().unwrap().to_string()),
            queue_capacity: 100,
            batch_max: 20,
            retention_days: 1,
            max_db_bytes: 10_000_000,
            shutdown_drain_ms: 1000,
            node_error_interval_ms: 50,
            migrate: false,
        };

        let handle = HistoryHandle::init_with_config(config, Some(temp_dir.clone())).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(50));

        handle.record_deploy_proposed("admin", Some("1234abcd"), "full", 1);
        handle.record_deploy_accepted("admin", "1234abcd", "full", 1);
        handle.record_copilot_requested("admin");
        std::thread::sleep(std::time::Duration::from_millis(600));

        let state = WebState::assemble_with_egress(
            Arc::new(RedSystemSettings::default()),
            std::env::temp_dir(),
            None,
            super::super::auth::AdminAuth::open(),
            Fleet::disabled(),
            WebRuntimeServices { history: handle.clone(), ..Default::default() },
            false,
        );

        let router = create_all_routes(&state).layer(Extension(state.clone()));
        let response =
            router.clone().oneshot(Request::builder().uri("/history").body(Body::empty()).unwrap()).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["events"].as_array().unwrap().len(), 3);

        // Test pagination limit=2
        let response = router
            .clone()
            .oneshot(Request::builder().uri("/history?limit=2").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["events"].as_array().unwrap().len(), 2);
        assert!(json["next"].is_number());

        // Test filtering by category
        let response = router
            .clone()
            .oneshot(Request::builder().uri("/history?category=copilot").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["events"].as_array().unwrap().len(), 1);

        // Test invalid limits
        let response = router
            .clone()
            .oneshot(Request::builder().uri("/history?limit=0").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);

        let response = router
            .clone()
            .oneshot(Request::builder().uri("/history?limit=501").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);

        // Test invalid category
        let response = router
            .clone()
            .oneshot(Request::builder().uri("/history?category=invalid").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);

        // Test invalid kind format
        let response = router
            .clone()
            .oneshot(Request::builder().uri("/history?kind=123bad").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);

        // Test unknown parameter
        let response = router
            .clone()
            .oneshot(Request::builder().uri("/history?unknown=true").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);

        // Test since_ms > until_ms
        let response = router
            .clone()
            .oneshot(Request::builder().uri("/history?since_ms=200&until_ms=100").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);

        handle.shutdown();
        let _ = std::fs::remove_dir_all(&temp_dir);
    }
}
