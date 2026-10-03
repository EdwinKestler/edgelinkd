use crate::handlers::WebState;
use crate::models::{AdminAuthSettings, RedSystemSettings, SettingsUser};
use axum::extract::{Path, Query};
use axum::{
    Extension,
    http::{HeaderMap, StatusCode},
    response::{Html, IntoResponse, Json},
};
use edgelink_core::runtime::paths;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;

// settings/user_settings/locale/icons related handlers
// ...existing code...

/// Editor view preferences. Node-RED returns an empty object until the editor saves some.
pub async fn get_user_settings(Extension(_state): Extension<Arc<WebState>>) -> Result<Json<Value>, StatusCode> {
    Ok(Json(serde_json::json!({})))
}

/// Update user settings
pub async fn update_user_settings(
    Extension(_state): Extension<Arc<WebState>>,
    Json(payload): Json<Value>,
) -> Result<Json<Value>, StatusCode> {
    log::debug!("Updating user settings: {payload:?}");

    // In actual implementation, this should save the settings
    Ok(Json(payload))
}
/// Get system settings.
///
/// The context block names the stores the running engine actually has. The editor sidebar uses
/// that list; a static `"default"` would not match the memory store the runtime creates.
pub async fn get_settings(
    Extension(state): Extension<Arc<WebState>>,
    headers: HeaderMap,
) -> Result<Json<RedSystemSettings>, StatusCode> {
    let mut settings = state.red_settings.as_ref().clone();
    if let Some(addr) = *state.listen.read().await {
        settings.ui_host = addr.ip().to_string();
        settings.ui_port = addr.port();
    }
    if let Some(engine) = state.engine.read().await.as_ref() {
        let manager = engine.get_context_manager();
        settings.context.default = manager.default_store_name();
        settings.context.stores = manager.store_names();
    }
    if state.auth.enabled() {
        let actor = state.auth.actor_from_headers(&headers);
        if actor.permissions.is_empty() {
            return Err(StatusCode::UNAUTHORIZED);
        }
        settings.user =
            Some(SettingsUser { username: actor.username, permissions: actor.permissions, anonymous: false });
        settings.admin_auth = Some(AdminAuthSettings { auth_type: state.auth.login_kind().to_string() });
        settings.editor_theme.user_menu = true;
    }
    Ok(Json(settings))
}

/// Get icon list
pub async fn get_icons() -> Result<Json<Value>, StatusCode> {
    // Return a simulated icon list
    let icons = serde_json::json!({
        "node-red": ["arrow-in.svg", "arrow-out.svg", "debug.svg", "inject.svg", "function.svg"],
        "edgelink": ["edge.svg", "link.svg"]
    });

    Ok(Json(icons))
}

/// Get icon file
pub async fn get_icon_file(
    Path((module, icon)): Path<(String, String)>,
) -> Result<axum::response::Response, StatusCode> {
    log::debug!("Requesting icon: {icon} from module: {module}");

    // Get static file directory
    let static_dir = paths::ui_static_dir();

    // Build icon file path
    let icon_path = static_dir.join("icons").join(&module).join(&icon);

    // Security check - ensure we do not escape the static directory
    if !icon_path.starts_with(&static_dir) {
        log::warn!("Attempted path traversal attack: {}", icon_path.display());
        return Err(StatusCode::FORBIDDEN);
    }

    // Try to read the icon file
    match tokio::fs::read(&icon_path).await {
        Ok(content) => {
            let mut headers = axum::http::HeaderMap::new();

            // Set correct Content-Type based on file extension
            if icon.ends_with(".svg") {
                headers.insert(axum::http::header::CONTENT_TYPE, "image/svg+xml".parse().unwrap());
            } else if icon.ends_with(".png") {
                headers.insert(axum::http::header::CONTENT_TYPE, "image/png".parse().unwrap());
            } else if icon.ends_with(".jpg") || icon.ends_with(".jpeg") {
                headers.insert(axum::http::header::CONTENT_TYPE, "image/jpeg".parse().unwrap());
            } else if icon.ends_with(".gif") {
                headers.insert(axum::http::header::CONTENT_TYPE, "image/gif".parse().unwrap());
            } else {
                // Default to SVG
                headers.insert(axum::http::header::CONTENT_TYPE, "image/svg+xml".parse().unwrap());
            }

            Ok((headers, content).into_response())
        }
        Err(_) => {
            log::warn!("Icon not found: {}", icon_path.display());
            Err(StatusCode::NOT_FOUND)
        }
    }
}

/// Get Plugins
pub async fn get_plugins(headers: HeaderMap) -> Result<axum::response::Response, StatusCode> {
    // Check Accept header to determine response format
    let accept_header = headers.get("accept").and_then(|h| h.to_str().ok()).unwrap_or("application/json");

    if accept_header.contains("text/html") {
        // Return HTML config for plugins
        let html_content = generate_plugins_html().await;
        Ok(Html(html_content).into_response())
    } else {
        // Return plugin list in JSON format
        #[cfg(feature = "nodes_ai")]
        let plugins = serde_json::json!([{
            "id": "edgelink-flow-copilot/flow-copilot",
            "name": "flow-copilot",
            "types": ["edgelink-flow-copilot"],
            "enabled": true,
            "local": true,
            "user": false,
            "module": "edgelink-flow-copilot",
            "version": env!("CARGO_PKG_VERSION")
        }]);
        #[cfg(not(feature = "nodes_ai"))]
        let plugins = serde_json::json!([]);
        Ok(Json(plugins).into_response())
    }
}

/// Generate HTML config for plugins
async fn generate_plugins_html() -> String {
    // Node-RED frontend expects plugin config in HTML format
    // Each plugin is wrapped with specific comment delimiters
    #[cfg(feature = "nodes_ai")]
    {
        format!(
            "\n<!-- --- [red-plugin:edgelink-flow-copilot/flow-copilot] --- -->\n{}",
            include_str!("../../flow-copilot/flow-copilot.html")
        )
    }
    #[cfg(not(feature = "nodes_ai"))]
    {
        String::new()
    }
}

pub async fn get_theme() -> Result<Json<Value>, StatusCode> {
    // Return Node-RED compatible theme list
    let jd = serde_json::json!({
        "page": {
            "title": "EdgeLinkd",
            "favicon": "favicon.ico",
            "tabicon": {
                "icon": "red/images/node-red-icon-black.svg",
                "colour": "#8f0000"
            }
        },
        "header": {
            "title": "EdgeLinkd",
            "image": "red/images/node-red.svg"
        },
        "asset": {
            "red": "red/red.min.js",
            "main": "red/main.min.js",
            "vendorMonaco": "vendor/monaco/monaco-bootstrap.js"
        },
        "themes": []
    });
    Ok(Json(jd))
}
/// Get plugin messages
pub async fn get_plugin_messages(Query(params): Query<HashMap<String, String>>) -> Result<Json<Value>, StatusCode> {
    let lang = params.get("lng").unwrap_or(&"en-US".to_string()).clone();

    log::debug!("Getting plugin messages for language: {lang}");

    // Return localized messages for plugins
    let messages = match lang.as_str() {
        "zh-CN" => serde_json::json!({
            "edgelink": {
                "plugin": {
                    "name": "EdgeLinkd 插件",
                    "description": "EdgeLinkd 核心插件",
                    "version": "版本"
                }
            }
        }),
        _ => serde_json::json!({
            "edgelink": {
                "plugin": {
                    "name": "EdgeLinkd Plugin",
                    "description": "EdgeLinkd core plugin",
                    "version": "Version"
                }
            }
        }),
    };

    Ok(Json(messages))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::create_all_routes;
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    #[tokio::test]
    async fn settings_report_the_bound_address_and_the_libraries() {
        let state = WebState::new();
        let router = create_all_routes(&state).layer(Extension(state.clone()));
        let response =
            router.clone().oneshot(Request::builder().uri("/settings").body(Body::empty()).unwrap()).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let body: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(body["uiHost"], "0.0.0.0");
        assert_eq!(body["uiPort"], 1880);
        let libraries = body["libraries"].as_array().expect("libraries");
        assert_eq!(libraries[0]["id"], "local");
        assert!(libraries[0].get("readOnly").is_none());
        assert_eq!(libraries[1]["id"], "examples");
        assert_eq!(libraries[1]["readOnly"], true);
        assert_eq!(libraries[1]["types"], serde_json::json!(["flows"]));

        state.record_listen("127.0.0.1:1888".parse().unwrap()).await;
        let response = router.oneshot(Request::builder().uri("/settings").body(Body::empty()).unwrap()).await.unwrap();
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let body: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(body["uiHost"], "127.0.0.1");
        assert_eq!(body["uiPort"], 1888);
    }

    #[tokio::test]
    async fn plugins_match_the_enabled_editor_features() {
        let state = WebState::new();
        let router = create_all_routes(&state).layer(Extension(state));

        let response = router
            .clone()
            .oneshot(
                Request::builder().uri("/plugins").header("accept", "application/json").body(Body::empty()).unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let plugins: Value = serde_json::from_slice(&bytes).unwrap();

        let response = router
            .oneshot(Request::builder().uri("/plugins").header("accept", "text/html").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let html = String::from_utf8(bytes.to_vec()).unwrap();

        #[cfg(feature = "nodes_ai")]
        {
            assert_eq!(plugins[0]["id"], "edgelink-flow-copilot/flow-copilot");
            assert!(html.contains("[red-plugin:edgelink-flow-copilot/flow-copilot]"));
            assert!(html.contains("RED.plugins.registerPlugin"));
            assert!(html.contains("RED.sidebar.addTab"));
            assert!(html.contains("RED.view.importNodes"));
        }
        #[cfg(not(feature = "nodes_ai"))]
        {
            assert_eq!(plugins, serde_json::json!([]));
            assert!(html.is_empty());
        }
    }
}
