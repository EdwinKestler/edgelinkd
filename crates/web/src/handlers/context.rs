//! Context sidebar routes.
//!
//! The editor (`tab-context.js`) reads `{ "<store>": { "<key>": { "msg", "format" } } }` and
//! deletes with `DELETE .../{key}?store=`. A single key is `{ "msg", "format" }`, plus `store`
//! only when that store is not the default. A missing key is `format: "undefined"`: after a
//! delete the sidebar removes the row only when it sees that. A missing flow or node is an
//! error. An empty object here used to look like a live context with no keys.
//!
//! A held force adds `"forced": true` on that value. The stock sidebar ignores the extra field.
//! `POST` and `DELETE` `.../{key}/force` set and clear it. Deleting the key itself while the
//! force is held is `409` and leaves both the hold and the stored value in place.

use std::sync::Arc;

use axum::extract::{Extension, Path, Query};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Json, Response};
use n2link_core::N2linkError;
use n2link_core::runtime::context::{Context, ContextManager, ContextStoreHandle, ContextValue};
use n2link_core::runtime::debug_channel::format_message_for_display;
use n2link_core::runtime::engine::Engine;
use n2link_core::runtime::model::{ContextHolder, ElementId, Variant};
use serde::Deserialize;
use serde_json::{Map, Value, json};

use crate::handlers::WebState;

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct ContextQuery {
    store: Option<String>,
    #[serde(rename = "keysOnly")]
    keys_only: Option<String>,
}

impl ContextQuery {
    fn store_name(&self) -> Option<&str> {
        self.store.as_deref()
    }

    /// Node-RED treats any present `keysOnly` query, including an empty one, as true.
    fn keys_only(&self) -> bool {
        self.keys_only.is_some()
    }
}

pub enum ContextReply {
    Json(Value),
    Empty,
}

impl IntoResponse for ContextReply {
    fn into_response(self) -> Response {
        match self {
            Self::Json(value) => Json(value).into_response(),
            Self::Empty => StatusCode::NO_CONTENT.into_response(),
        }
    }
}

pub enum ContextFailure {
    RuntimeDown,
    NotFound(&'static str),
    Forced,
    NotSupported(String),
    Unexpected(String),
}

impl IntoResponse for ContextFailure {
    fn into_response(self) -> Response {
        let (status, code, message) = match self {
            Self::RuntimeDown => {
                (StatusCode::SERVICE_UNAVAILABLE, "not_available", "runtime is not running".to_string())
            }
            Self::NotFound(what) => (StatusCode::NOT_FOUND, "not_found", format!("{what} not found")),
            Self::Forced => (StatusCode::CONFLICT, "forced", "context key is forced".to_string()),
            Self::NotSupported(message) => (StatusCode::BAD_REQUEST, "not_supported", message),
            Self::Unexpected(message) => {
                log::warn!("Context request failed: {message}");
                (StatusCode::BAD_REQUEST, "unexpected_error", message)
            }
        };
        (status, Json(json!({ "code": code, "message": message }))).into_response()
    }
}

enum Scope {
    Global,
    Flow(String),
    Node(String),
}

pub async fn get_global_context(
    Extension(state): Extension<Arc<WebState>>,
    Query(query): Query<ContextQuery>,
) -> Result<ContextReply, ContextFailure> {
    read_scope(&state, Scope::Global, None, &query).await
}

pub async fn get_global_context_key(
    Extension(state): Extension<Arc<WebState>>,
    Path(key): Path<String>,
    Query(query): Query<ContextQuery>,
) -> Result<ContextReply, ContextFailure> {
    read_scope(&state, Scope::Global, Some(key), &query).await
}

pub async fn delete_global_context_key(
    Extension(state): Extension<Arc<WebState>>,
    Path(key): Path<String>,
    Query(query): Query<ContextQuery>,
) -> Result<ContextReply, ContextFailure> {
    delete_scope(&state, Scope::Global, &key, query.store_name()).await
}

pub async fn get_flow_context(
    Extension(state): Extension<Arc<WebState>>,
    Path(flow_id): Path<String>,
    Query(query): Query<ContextQuery>,
) -> Result<ContextReply, ContextFailure> {
    read_scope(&state, Scope::Flow(flow_id), None, &query).await
}

pub async fn get_flow_context_key(
    Extension(state): Extension<Arc<WebState>>,
    Path((flow_id, key)): Path<(String, String)>,
    Query(query): Query<ContextQuery>,
) -> Result<ContextReply, ContextFailure> {
    read_scope(&state, Scope::Flow(flow_id), Some(key), &query).await
}

pub async fn delete_flow_context_key(
    Extension(state): Extension<Arc<WebState>>,
    Path((flow_id, key)): Path<(String, String)>,
    Query(query): Query<ContextQuery>,
) -> Result<ContextReply, ContextFailure> {
    delete_scope(&state, Scope::Flow(flow_id), &key, query.store_name()).await
}

pub async fn get_node_context(
    Extension(state): Extension<Arc<WebState>>,
    Path(node_id): Path<String>,
    Query(query): Query<ContextQuery>,
) -> Result<ContextReply, ContextFailure> {
    read_scope(&state, Scope::Node(node_id), None, &query).await
}

pub async fn get_node_context_key(
    Extension(state): Extension<Arc<WebState>>,
    Path((node_id, key)): Path<(String, String)>,
    Query(query): Query<ContextQuery>,
) -> Result<ContextReply, ContextFailure> {
    read_scope(&state, Scope::Node(node_id), Some(key), &query).await
}

pub async fn delete_node_context_key(
    Extension(state): Extension<Arc<WebState>>,
    Path((node_id, key)): Path<(String, String)>,
    Query(query): Query<ContextQuery>,
) -> Result<ContextReply, ContextFailure> {
    delete_scope(&state, Scope::Node(node_id), &key, query.store_name()).await
}

pub async fn force_global_context_key(
    Extension(state): Extension<Arc<WebState>>,
    Path(key): Path<String>,
    Query(query): Query<ContextQuery>,
    body: axum::extract::Json<Value>,
) -> Result<ContextReply, ContextFailure> {
    write_force(&state, Scope::Global, &key, query.store_name(), body.0).await
}

pub async fn clear_global_context_force(
    Extension(state): Extension<Arc<WebState>>,
    Path(key): Path<String>,
    Query(query): Query<ContextQuery>,
) -> Result<ContextReply, ContextFailure> {
    clear_force_scope(&state, Scope::Global, &key, query.store_name()).await
}

pub async fn force_flow_context_key(
    Extension(state): Extension<Arc<WebState>>,
    Path((flow_id, key)): Path<(String, String)>,
    Query(query): Query<ContextQuery>,
    body: axum::extract::Json<Value>,
) -> Result<ContextReply, ContextFailure> {
    write_force(&state, Scope::Flow(flow_id), &key, query.store_name(), body.0).await
}

pub async fn clear_flow_context_force(
    Extension(state): Extension<Arc<WebState>>,
    Path((flow_id, key)): Path<(String, String)>,
    Query(query): Query<ContextQuery>,
) -> Result<ContextReply, ContextFailure> {
    clear_force_scope(&state, Scope::Flow(flow_id), &key, query.store_name()).await
}

pub async fn force_node_context_key(
    Extension(state): Extension<Arc<WebState>>,
    Path((node_id, key)): Path<(String, String)>,
    Query(query): Query<ContextQuery>,
    body: axum::extract::Json<Value>,
) -> Result<ContextReply, ContextFailure> {
    write_force(&state, Scope::Node(node_id), &key, query.store_name(), body.0).await
}

pub async fn clear_node_context_force(
    Extension(state): Extension<Arc<WebState>>,
    Path((node_id, key)): Path<(String, String)>,
    Query(query): Query<ContextQuery>,
) -> Result<ContextReply, ContextFailure> {
    clear_force_scope(&state, Scope::Node(node_id), &key, query.store_name()).await
}

async fn read_scope(
    state: &WebState,
    scope: Scope,
    key: Option<String>,
    query: &ContextQuery,
) -> Result<ContextReply, ContextFailure> {
    let (ctx, manager) = open_scope(state, scope).await?;
    let body = match key {
        Some(key) => read_one(&ctx, &manager, &key, query).await?,
        None => read_all(&ctx, &manager, query).await?,
    };
    Ok(ContextReply::Json(body))
}

async fn delete_scope(
    state: &WebState,
    scope: Scope,
    key: &str,
    requested_store: Option<&str>,
) -> Result<ContextReply, ContextFailure> {
    let (ctx, manager) = open_scope(state, scope).await?;
    if let Some(name) = requested_store {
        manager.canonical_store_name(name).ok_or(ContextFailure::NotFound("context store"))?;
    }
    match ctx.set_one(requested_store, key, None, &[]).await {
        Ok(()) => Ok(ContextReply::Empty),
        Err(err) if err.is_out_of_range() => Ok(ContextReply::Empty),
        Err(err) if forced_hold(&err) => Err(ContextFailure::Forced),
        Err(err) => Err(ContextFailure::Unexpected(err.to_string())),
    }
}

async fn write_force(
    state: &WebState,
    scope: Scope,
    key: &str,
    requested_store: Option<&str>,
    body: Value,
) -> Result<ContextReply, ContextFailure> {
    let (ctx, manager) = open_scope(state, scope).await?;
    // An unknown store is a 404 before a nested key is a 400, and before anything is held.
    let (name, _, is_default) = selected_store(&manager, requested_store)?;
    let value: Variant = serde_json::from_value(body).map_err(|err| ContextFailure::Unexpected(err.to_string()))?;
    ctx.force_one(requested_store, key, value.clone()).map_err(map_force_error)?;
    let mut encoded = encode_present(&value, true, None)?;
    if !is_default {
        encoded["store"] = Value::String(name);
    }
    Ok(ContextReply::Json(encoded))
}

async fn clear_force_scope(
    state: &WebState,
    scope: Scope,
    key: &str,
    requested_store: Option<&str>,
) -> Result<ContextReply, ContextFailure> {
    let (ctx, manager) = open_scope(state, scope).await?;
    if let Some(name) = requested_store {
        manager.canonical_store_name(name).ok_or(ContextFailure::NotFound("context store"))?;
    }
    ctx.clear_force(requested_store, key).map_err(map_force_error)?;
    Ok(ContextReply::Empty)
}

async fn open_scope(state: &WebState, scope: Scope) -> Result<(Context, Arc<ContextManager>), ContextFailure> {
    let engine = state.engine.read().await.clone().ok_or(ContextFailure::RuntimeDown)?;
    let ctx = resolve_context(&engine, scope)?;
    let manager = Arc::clone(engine.get_context_manager());
    Ok((ctx, manager))
}

fn resolve_context(engine: &Engine, scope: Scope) -> Result<Context, ContextFailure> {
    match scope {
        Scope::Global => Ok(engine.context().clone()),
        Scope::Flow(id) => {
            let id = parse_id(&id).map_err(|_| ContextFailure::NotFound("flow"))?;
            let flow = engine.get_flow(&id).ok_or(ContextFailure::NotFound("flow"))?;
            Ok(flow.context().clone())
        }
        Scope::Node(id) => {
            let id = parse_id(&id).map_err(|_| ContextFailure::NotFound("node"))?;
            let node = engine.find_flow_node_by_id(&id).ok_or(ContextFailure::NotFound("node"))?;
            // This is the context a function node's `context.set` writes. The editor sidebar
            // reads it back through this route.
            Ok(node.get_base().context().clone())
        }
    }
}

fn parse_id(id: &str) -> Result<ElementId, ()> {
    id.parse().map_err(|_| ())
}

async fn read_all(ctx: &Context, manager: &ContextManager, query: &ContextQuery) -> Result<Value, ContextFailure> {
    let stores = selected_stores(manager, query.store_name())?;
    let mut body = Map::new();
    for (name, store) in stores {
        let entries = ctx.read_store(&name, store.as_ref()).await.map_err(unexpected)?;
        let value = if query.keys_only() {
            let keys = entries
                .into_iter()
                .map(|(key, entry)| {
                    let mut item = json!({ "key": key });
                    if entry.forced {
                        item["forced"] = json!(true);
                    }
                    item
                })
                .collect::<Vec<_>>();
            json!({ "keys": keys })
        } else {
            let mut store_body = Map::new();
            for (key, entry) in entries {
                store_body.insert(key, encode_present(&entry.value, entry.forced, entry.updated_at)?);
            }
            Value::Object(store_body)
        };
        body.insert(name, value);
    }
    Ok(Value::Object(body))
}

async fn read_one(
    ctx: &Context,
    manager: &ContextManager,
    key: &str,
    query: &ContextQuery,
) -> Result<Value, ContextFailure> {
    let (name, store, is_default) = selected_store(manager, query.store_name())?;
    let value = ctx.read_key(&name, store.as_ref(), key).await.map_err(unexpected)?;
    if query.keys_only() {
        return Ok(json!({ name: keys_only_value(value.as_ref()) }));
    }
    let mut encoded = match value.as_ref() {
        Some(entry) => encode_present(&entry.value, entry.forced, entry.updated_at)?,
        None => json!({ "msg": "(undefined)", "format": "undefined" }),
    };
    if !is_default {
        encoded["store"] = Value::String(name);
    }
    Ok(encoded)
}

fn selected_stores(
    manager: &ContextManager,
    requested: Option<&str>,
) -> Result<Vec<(String, ContextStoreHandle)>, ContextFailure> {
    let names = match requested {
        Some(name) => vec![manager.canonical_store_name(name).ok_or(ContextFailure::NotFound("context store"))?],
        None => manager.store_names(),
    };
    names
        .into_iter()
        .map(|name| {
            let store = manager
                .configured_store(&name)
                .cloned()
                .ok_or_else(|| ContextFailure::Unexpected(format!("context store '{name}' is not configured")))?;
            Ok((name, store))
        })
        .collect()
}

fn selected_store(
    manager: &ContextManager,
    requested: Option<&str>,
) -> Result<(String, ContextStoreHandle, bool), ContextFailure> {
    let default_name = manager.default_store_name();
    let name = match requested {
        Some(name) => manager.canonical_store_name(name).ok_or(ContextFailure::NotFound("context store"))?,
        None => default_name.clone(),
    };
    let store = manager
        .configured_store(&name)
        .cloned()
        .ok_or_else(|| ContextFailure::Unexpected(format!("context store '{name}' is not configured")))?;
    let is_default = name == default_name;
    Ok((name, store, is_default))
}

fn encode_present(value: &Variant, forced: bool, updated_at: Option<i64>) -> Result<Value, ContextFailure> {
    let json = serde_json::to_value(value).map_err(|err| ContextFailure::Unexpected(err.to_string()))?;
    let (msg, format) = format_message_for_display(&json);
    let mut encoded = json!({ "msg": msg, "format": format });
    if forced {
        encoded["forced"] = json!(true);
    }
    // Absent when this process has not stored the key. A force alone does not invent a time.
    if let Some(updated_at) = updated_at {
        encoded["updatedAt"] = json!(updated_at);
    }
    Ok(encoded)
}

/// `keysOnly` on one key is how the editor expands an object. The shape is the runtime's, not
/// the `{msg, format}` used for a normal read.
fn keys_only_value(entry: Option<&ContextValue>) -> Value {
    let Some(entry) = entry else {
        return json!({ "keys": [] });
    };
    let mut value = match &entry.value {
        Variant::Array(items) => json!({ "format": format!("array[{}]", items.len()) }),
        Variant::Object(map) => {
            let keys = map
                .iter()
                .map(|(key, child)| match child {
                    Variant::Array(items) => {
                        json!({ "key": key, "format": format!("array[{}]", items.len()), "length": items.len() })
                    }
                    Variant::Object(_) => json!({ "key": key, "format": "object" }),
                    _ => json!({ "key": key }),
                })
                .collect::<Vec<_>>();
            json!({ "format": "object", "keys": keys })
        }
        // `null` is an object in JavaScript. Expanding it has no keys; reporting an error would
        // blank the sidebar for a value that is really there.
        _ => json!({ "keys": [] }),
    };
    if entry.forced {
        value["forced"] = json!(true);
    }
    value
}

fn forced_hold(err: &N2linkError) -> bool {
    match err {
        N2linkError::InvalidOperation(message) => message.contains("is forced"),
        N2linkError::Other(inner) => inner.downcast_ref::<N2linkError>().is_some_and(forced_hold),
        _ => false,
    }
}

fn not_supported_text(err: &N2linkError) -> Option<String> {
    match err {
        N2linkError::NotSupported(message) => Some(message.clone()),
        N2linkError::Other(inner) => inner.downcast_ref::<N2linkError>().and_then(not_supported_text),
        _ => None,
    }
}

fn map_force_error(err: N2linkError) -> ContextFailure {
    if let Some(message) = not_supported_text(&err) {
        ContextFailure::NotSupported(message)
    } else {
        ContextFailure::Unexpected(err.to_string())
    }
}

fn unexpected(err: impl ToString) -> ContextFailure {
    ContextFailure::Unexpected(err.to_string())
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::api::create_all_routes;
    use crate::handlers::WebState;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use n2link_core::runtime::engine::Engine;
    use n2link_core::runtime::model::{ContextHolder, Variant};
    use n2link_core::runtime::registry::RegistryBuilder;
    use serde_json::{Value, json};
    use tower::ServiceExt;

    fn sample_flows() -> Value {
        json!([
            { "id": "100", "type": "tab", "label": "Flow 1" },
            {
                "id": "1", "type": "inject", "z": "100", "name": "tick",
                "props": [{ "p": "payload" }, { "p": "topic", "vt": "str" }],
                "once": false, "onceDelay": 0, "repeat": "", "topic": "",
                "payload": "", "payloadType": "str", "wires": [[]]
            }
        ])
    }

    fn variant(value: Value) -> Variant {
        serde_json::from_value(value).expect("variant")
    }

    async fn router_for(engine: Option<Arc<Engine>>) -> axum::Router {
        let state = WebState::new();
        if let Some(engine) = engine {
            state.set_engine(engine).await;
        }
        create_all_routes(&state).layer(Extension(state))
    }

    async fn call(router: &axum::Router, method: &str, uri: &str) -> (StatusCode, Value) {
        let response = router
            .clone()
            .oneshot(Request::builder().method(method).uri(uri).body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let body = if bytes.is_empty() { Value::Null } else { serde_json::from_slice(&bytes).unwrap() };
        (status, body)
    }

    async fn call_json(router: &axum::Router, method: &str, uri: &str, body: Value) -> (StatusCode, Value) {
        let response = router
            .clone()
            .oneshot(
                Request::builder()
                    .method(method)
                    .uri(uri)
                    .header("content-type", "application/json")
                    .body(Body::from(serde_json::to_vec(&body).unwrap()))
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let body = if bytes.is_empty() { Value::Null } else { serde_json::from_slice(&bytes).unwrap() };
        (status, body)
    }

    #[tokio::test]
    async fn context_http_reads_and_deletes_each_scope() {
        let registry = RegistryBuilder::default().build().unwrap();
        let engine = Arc::new(Engine::with_json(&registry, sample_flows(), None).unwrap());
        let router = router_for(Some(Arc::clone(&engine))).await;

        let (status, body) = call(&router, "GET", "/context/node/1").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, json!({ "memory": {} }));

        engine.context().set_one(None, "plant", Some(variant(json!("running"))), &[]).await.unwrap();
        engine
            .get_flow(&"100".parse().unwrap())
            .unwrap()
            .context()
            .set_one(None, "motor", Some(variant(json!(true))), &[])
            .await
            .unwrap();
        let node = engine.find_flow_node_by_id(&"1".parse().unwrap()).unwrap();
        node.get_base().context().set_one(None, "count", Some(variant(json!(3))), &[]).await.unwrap();
        node.get_base()
            .context()
            .set_one(None, "pose", Some(variant(json!({"x": 1, "y": [2, 3]}))), &[])
            .await
            .unwrap();

        let (status, body) = call(&router, "GET", "/context/global").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["memory"]["plant"]["msg"], "running");
        assert_eq!(body["memory"]["plant"]["format"], "string[7]");

        let (status, body) = call(&router, "GET", "/context/global/plant?store=default").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["msg"], "running");
        assert!(body.get("store").is_none());

        let (status, body) = call(&router, "GET", "/context/global?keysOnly").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["memory"]["keys"], json!([{ "key": "plant" }]));

        let (status, body) = call(&router, "GET", "/context/flow/100").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["memory"]["motor"]["msg"], "true");
        assert_eq!(body["memory"]["motor"]["format"], "boolean");
        assert!(body["memory"]["motor"]["updatedAt"].as_i64().is_some());

        let (status, body) = call(&router, "GET", "/context/node/1").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["memory"]["count"]["msg"], "3");
        assert_eq!(body["memory"]["count"]["format"], "number");
        assert!(body["memory"]["count"]["updatedAt"].as_i64().is_some());
        assert_eq!(body["memory"]["pose"]["format"], "Object");
        let pose: Value = serde_json::from_str(body["memory"]["pose"]["msg"].as_str().unwrap()).unwrap();
        assert_eq!(pose["x"], 1);
        assert_eq!(pose["y"], json!([2, 3]));

        let (status, body) = call(&router, "GET", "/context/node/1/pose?keysOnly=1").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["memory"]["format"], "object");
        assert_eq!(body["memory"]["keys"], json!([{ "key": "x" }, { "key": "y", "format": "array[2]", "length": 2 }]));

        let (status, settings) = call(&router, "GET", "/settings").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(settings["context"]["default"], "memory");
        assert_eq!(settings["context"]["stores"], json!(["memory"]));

        let (status, body) = call(&router, "DELETE", "/context/global/plant?store=disk").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(body["code"], "not_found");
        assert!(engine.context().get_one(None, "plant", &[]).await.is_some());

        let (status, _) = call(&router, "DELETE", "/context/global/plant?store=memory").await;
        assert_eq!(status, StatusCode::NO_CONTENT);
        assert!(engine.context().get_one(None, "plant", &[]).await.is_none());

        let (status, body) = call(&router, "GET", "/context/global/plant").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["format"], "undefined");

        let (status, _) = call(&router, "DELETE", "/context/flow/100/motor?store=default").await;
        assert_eq!(status, StatusCode::NO_CONTENT);
        let flow = engine.get_flow(&"100".parse().unwrap()).unwrap();
        assert!(flow.context().get_one(None, "motor", &[]).await.is_none());

        let (status, _) = call(&router, "DELETE", "/context/node/1/count").await;
        assert_eq!(status, StatusCode::NO_CONTENT);
        assert!(node.get_base().context().get_one(None, "count", &[]).await.is_none());
        assert!(node.get_base().context().get_one(None, "pose", &[]).await.is_some());

        let (status, _) = call(&router, "DELETE", "/context/global/missing").await;
        assert_eq!(status, StatusCode::NO_CONTENT);
    }

    #[tokio::test]
    async fn missing_flow_node_and_store_are_errors() {
        let registry = RegistryBuilder::default().build().unwrap();
        let engine = Arc::new(Engine::with_json(&registry, sample_flows(), None).unwrap());
        let router = router_for(Some(engine)).await;

        for uri in ["/context/flow/999", "/context/flow/not-hex", "/context/node/2", "/context/node/zz"] {
            let (status, body) = call(&router, "GET", uri).await;
            assert_eq!(status, StatusCode::NOT_FOUND, "{uri}");
            assert_eq!(body["code"], "not_found", "{uri}");
            assert_ne!(body, json!({}), "{uri}");
        }

        let (status, body) = call(&router, "GET", "/context/global?store=disk").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(body["code"], "not_found");
    }

    #[tokio::test]
    async fn context_routes_fail_when_the_runtime_is_down() {
        let router = router_for(None).await;
        let (status, body) = call(&router, "GET", "/context/global").await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(body["code"], "not_available");
    }

    #[tokio::test]
    async fn force_on_flow_context_holds_until_cleared() {
        let registry = RegistryBuilder::default().build().unwrap();
        let engine = Arc::new(Engine::with_json(&registry, sample_flows(), None).unwrap());
        let router = router_for(Some(Arc::clone(&engine))).await;
        let flow = engine.get_flow(&"100".parse().unwrap()).unwrap();
        flow.context().set_one(None, "motor", Some(variant(json!(true))), &[]).await.unwrap();

        let (status, body) = call(&router, "GET", "/context/flow/100/motor").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["msg"], "true");
        assert_eq!(body["format"], "boolean");
        assert!(body.get("forced").is_none());

        let (status, body) = call_json(&router, "POST", "/context/flow/100/motor/force?store=disk", json!(false)).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(body["code"], "not_found");

        let (status, body) = call_json(&router, "POST", "/context/flow/100/motor.speed/force", json!(1)).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["code"], "not_supported");
        assert!(body["message"].as_str().unwrap().contains("motor.speed"));

        let (status, body) = call(&router, "DELETE", "/context/flow/100/motor.speed/force").await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["code"], "not_supported");

        let (status, body) = call_json(&router, "POST", "/context/flow/100/motor/force", json!(false)).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["msg"], "false");
        assert_eq!(body["format"], "boolean");
        assert_eq!(body["forced"], true);
        assert!(body.get("store").is_none());

        let (status, body) = call(&router, "GET", "/context/flow/100/motor").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["msg"], "false");
        assert_eq!(body["format"], "boolean");
        assert_eq!(body["forced"], true);
        assert!(body["updatedAt"].as_i64().is_some());

        let (status, body) = call(&router, "GET", "/context/flow/100").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["memory"]["motor"]["forced"], true);

        let (status, body) = call(&router, "GET", "/context/flow/100?keysOnly").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["memory"]["keys"], json!([{ "key": "motor", "forced": true }]));

        // The same write a function node makes. The held value stays.
        flow.context().set_one(None, "motor", Some(variant(json!(true))), &[]).await.unwrap();
        let (status, body) = call(&router, "GET", "/context/flow/100/motor").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["msg"], "false");
        assert_eq!(body["forced"], true);

        let (status, body) = call(&router, "DELETE", "/context/flow/100/motor").await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(body["code"], "forced");
        assert_eq!(body["message"], "context key is forced");
        let (status, body) = call(&router, "GET", "/context/flow/100/motor").await;
        assert_eq!(body["forced"], true);
        assert_eq!(status, StatusCode::OK);

        let (status, body) = call(&router, "DELETE", "/context/flow/100/motor/force").await;
        assert_eq!(status, StatusCode::NO_CONTENT);
        assert_eq!(body, Value::Null);
        let (status, body) = call(&router, "GET", "/context/flow/100/motor").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["msg"], "true");
        assert_eq!(body["format"], "boolean");
        assert!(body.get("forced").is_none());
        assert!(body["updatedAt"].as_i64().is_some());

        let (status, body) =
            call_json(&router, "POST", "/context/flow/100/motor/force?store=default", json!(false)).await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.get("store").is_none());
        let (status, body) = call(&router, "GET", "/context/flow/100/motor").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["msg"], "false");
        assert_eq!(body["forced"], true);
        let (status, _) = call(&router, "DELETE", "/context/flow/100/motor/force?store=default").await;
        assert_eq!(status, StatusCode::NO_CONTENT);

        flow.context().set_one(None, "motor", Some(variant(json!("run"))), &[]).await.unwrap();
        let (status, body) = call(&router, "GET", "/context/flow/100/motor").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["msg"], "run");
        assert_eq!(body["format"], "string[3]");
        assert!(body.get("forced").is_none());

        // A force of a key the store does not hold is visible, and clearing it leaves the key absent.
        let (status, body) = call_json(&router, "POST", "/context/global/plant/force", json!("held")).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["forced"], true);
        assert_eq!(body["format"], "string[4]");
        let (status, body) = call(&router, "GET", "/context/global/plant").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["forced"], true);
        assert!(body.get("updatedAt").is_none());
        let (status, _) = call(&router, "DELETE", "/context/global/plant/force").await;
        assert_eq!(status, StatusCode::NO_CONTENT);
        let (status, _) = call(&router, "DELETE", "/context/global/plant/force").await;
        assert_eq!(status, StatusCode::NO_CONTENT);
        let (status, body) = call(&router, "DELETE", "/context/global/plant/force?store=disk").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(body["code"], "not_found");
        let (status, body) = call(&router, "GET", "/context/global/plant").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["format"], "undefined");

        let node = engine.find_flow_node_by_id(&"1".parse().unwrap()).unwrap();
        node.get_base().context().set_one(None, "count", Some(variant(json!(3))), &[]).await.unwrap();
        let (status, body) = call_json(&router, "POST", "/context/node/1/count/force", json!(9)).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["msg"], "9");
        assert_eq!(body["format"], "number");
        assert_eq!(body["forced"], true);
        node.get_base().context().set_one(None, "count", Some(variant(json!(4))), &[]).await.unwrap();
        let (status, body) = call(&router, "GET", "/context/node/1/count").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["msg"], "9");
        assert_eq!(body["forced"], true);
        let (status, _) = call(&router, "DELETE", "/context/node/1/count/force").await;
        assert_eq!(status, StatusCode::NO_CONTENT);
        let (status, body) = call(&router, "GET", "/context/node/1/count").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["msg"], "3");
        assert_eq!(body["format"], "number");
        assert!(body["updatedAt"].as_i64().is_some());
    }
}
