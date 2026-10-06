//! Admin API for WASM plugins (DESIGN.md §10) and the editor, `/nodes` and Copilot entries
//! generated from active plugin manifests (§11).
//!
//! Every mutating route holds `WebState::deploy`, so plugin changes serialise with flow deploys.
//! Activation and rollback prepare the deployed flows with the candidate plugin set, write the
//! pointer, swap the registry and redeploy; a failed redeploy reverts the pointer and redeploys
//! the previous set.

use std::sync::Arc;

use axum::Extension;
use axum::Json;
use axum::body::Bytes;
use axum::extract::Path;
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use n2link_core::N2linkError;
use n2link_core::runtime::engine::Engine;
use n2link_core::runtime::flow_credentials;
use n2link_core::runtime::registry::RegistryHandle;
use n2link_core::runtime::wasm::{
    ActiveEntry, ActivePlugins, ConfigKind, PackageStatus, PendingChange, PluginStore, PluginView,
};
use serde::Deserialize;
use serde_json::{Value, json};

use super::WebState;
use super::deploy;
use super::reply::api_error;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DigestBody {
    sha256: String,
}

/// `[a-z][a-z0-9]{0,31}`, the manifest's id segment rule, checked before any path is built.
fn valid_segment(segment: &str) -> bool {
    let mut chars = segment.chars();
    matches!(chars.next(), Some('a'..='z'))
        && segment.len() <= 32
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
}

fn valid_digest(sha: &str) -> bool {
    sha.len() == 64 && sha.chars().all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c))
}

fn plugin_id(publisher: &str, name: &str) -> Result<String, Box<Response>> {
    if valid_segment(publisher) && valid_segment(name) {
        Ok(format!("{publisher}/{name}"))
    } else {
        Err(Box::new(api_error(StatusCode::BAD_REQUEST, "invalid_id", "plugin id must be <publisher>/<name>")))
    }
}

fn digest(sha: &str) -> Result<(), Box<Response>> {
    if valid_digest(sha) {
        Ok(())
    } else {
        Err(Box::new(api_error(StatusCode::BAD_REQUEST, "invalid_digest", "sha256 must be 64 lowercase hex digits")))
    }
}

const PREPARE_FAILED: &str = "deployed flows do not build with this plugin set";

/// Stable error codes for clients and history. Store, manifest and host errors are matched by
/// their wording; anything unrecognised is `invalid_package` (or `store_error` for I/O).
fn reason_code(err: &N2linkError) -> &'static str {
    let text = err.to_string();
    let has = |needle: &str| text.contains(needle);
    if has(PREPARE_FAILED) {
        "invalid_flows"
    } else if text.starts_with("plugin store ") || has("invalid operation: plugin store ") {
        "store_error"
    } else if has("selftest") {
        "selftest_failed"
    } else if has("is not granted") || has("must be a function (imported") {
        "import_forbidden"
    } else if has("memory budget") || has("above [runtime.wasm]") || has("max_plugins") {
        "budget_exceeded"
    } else if has("does not match its digest") || has("has sha256") {
        "digest_mismatch"
    } else if has("no staged package")
        || has("is not active")
        || has("no quarantined package")
        || has("no previous generation")
    {
        "not_found"
    } else if has("previous generation is") || has("was rejected") || has("is plugin ") || has("already quarantined") {
        "conflict"
    } else if has("manifest") || has("WASM package") || has("export") || has("ABI") {
        "manifest_invalid"
    } else {
        "invalid_package"
    }
}

fn status_for(code: &str) -> StatusCode {
    match code {
        "not_found" => StatusCode::NOT_FOUND,
        // `selftest_failed` as an error means activating a package whose self-test failed.
        "conflict" | "invalid_flows" | "in_use" | "selftest_failed" => StatusCode::CONFLICT,
        "store_error" | "activation_failed" => StatusCode::INTERNAL_SERVER_ERROR,
        "budget_exceeded" => StatusCode::UNPROCESSABLE_ENTITY,
        _ => StatusCode::BAD_REQUEST,
    }
}

fn store_error(err: &N2linkError) -> Response {
    let code = reason_code(err);
    if code == "store_error" {
        log::error!("[WASM] {err}");
    }
    api_error(status_for(code), code, &err.to_string())
}

fn join_error(err: tokio::task::JoinError) -> Response {
    log::error!("[WASM] plugin store task failed: {err}");
    api_error(StatusCode::INTERNAL_SERVER_ERROR, "store_error", "plugin store task failed")
}

async fn store_of(state: &WebState) -> Result<Arc<PluginStore>, Box<Response>> {
    state.plugin_store.read().await.clone().ok_or_else(|| {
        Box::new(api_error(
            StatusCode::CONFLICT,
            "plugins_disabled",
            "WASM plugins are disabled by configuration ([runtime.wasm] enabled = false)",
        ))
    })
}

/// Run a blocking store operation (file I/O, compile, self-test) off the async runtime.
async fn blocking<T: Send + 'static>(
    store: &Arc<PluginStore>,
    op: impl FnOnce(&PluginStore) -> n2link_core::Result<T> + Send + 'static,
) -> Result<T, Box<Response>> {
    let store = store.clone();
    match tokio::task::spawn_blocking(move || op(&store)).await {
        Ok(Ok(value)) => Ok(value),
        Ok(Err(err)) => Err(Box::new(store_error(&err))),
        Err(err) => Err(Box::new(join_error(err))),
    }
}

/// The flows on disk with their credentials, as a deploy would load them.
async fn current_flows(state: &WebState) -> Result<Value, Box<Response>> {
    let Some(path) = state.flows_file_path.read().await.clone() else {
        return Ok(Value::Array(Vec::new()));
    };
    let internal = |what: &str, err: String| {
        log::error!("[WASM] {what}: {err}");
        Box::new(api_error(StatusCode::INTERNAL_SERVER_ERROR, "store_error", what))
    };
    let flows = deploy::load_flows_array(&path).await.map_err(|err| internal("flows are unreadable", err))?;
    let stored = flow_credentials::read_sidecar_with(&state.credentials, &path)
        .await
        .map_err(|err| internal("credentials are unavailable", err))?;
    Ok(deploy::with_credentials(flows, &stored))
}

/// Ids of flow nodes whose type is `type_name`.
fn nodes_using(flows: &Value, type_name: &str) -> Vec<String> {
    flows
        .as_array()
        .into_iter()
        .flatten()
        .filter(|node| node.get("type").and_then(Value::as_str) == Some(type_name))
        .map(|node| node.get("id").and_then(Value::as_str).unwrap_or("?").to_owned())
        .collect()
}

fn type_name_of(id: &str) -> String {
    format!("wasm-{}", id.replacen('/', "-", 1))
}

fn short(sha: &str) -> &str {
    &sha[..sha.len().min(12)]
}

/// `GET /wasm/plugins`: active generations and quarantined packages (reports only, no bytes).
pub async fn list_plugins(Extension(state): Extension<Arc<WebState>>) -> Response {
    let store = match store_of(&state).await {
        Ok(store) => store,
        Err(response) => return *response,
    };
    match blocking(&store, |store| store.list()).await {
        Ok(listing) => Json(json!({ "active": listing.active, "packages": listing.packages })).into_response(),
        Err(response) => *response,
    }
}

/// `POST /wasm/plugins/stage` with an `application/wasm` body: validate, quarantine, self-test.
pub async fn stage_plugin(Extension(state): Extension<Arc<WebState>>, headers: HeaderMap, body: Bytes) -> Response {
    let store = match store_of(&state).await {
        Ok(store) => store,
        Err(response) => return *response,
    };
    let content_type = headers.get(header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).unwrap_or("");
    if content_type.split(';').next().map(str::trim) != Some("application/wasm") {
        return api_error(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "unsupported_media_type",
            "send the package as application/wasm",
        );
    }
    if body.len() > store.max_package_bytes() {
        return api_error(
            StatusCode::PAYLOAD_TOO_LARGE,
            "too_large",
            &format!("package is larger than {} bytes ([runtime.wasm] max_module_kib)", store.max_package_bytes()),
        );
    }
    let actor = state.auth.actor_from_headers(&headers).username;
    let _deploy = state.deploy.lock().await;
    let bytes = body.to_vec();
    match blocking(&store, move |store| store.stage(&bytes)).await {
        Ok(report) => {
            let (kind, reason) = match report.status {
                PackageStatus::Ready => ("plugin.staged", None),
                PackageStatus::Rejected => ("plugin.rejected", Some("selftest_failed")),
            };
            state.history.record_plugin(&actor, kind, &report.id, Some(&report.version), Some(&report.sha256), reason);
            let detail = format!("{} {} {}", report.id, report.version, short(&report.sha256));
            let _ = state.audit.record(&actor, kind, Some(&detail)).await;
            Json(report).into_response()
        }
        Err(response) => {
            state.history.record_plugin(&actor, "plugin.rejected", "unknown", None, None, Some("invalid_package"));
            let _ = state.audit.record(&actor, "plugin.rejected", None).await;
            *response
        }
    }
}

/// `POST /wasm/plugins/{publisher}/{name}/activate` with `{"sha256": "…"}`.
pub async fn activate_plugin(
    Extension(state): Extension<Arc<WebState>>,
    headers: HeaderMap,
    Path((publisher, name)): Path<(String, String)>,
    Json(body): Json<DigestBody>,
) -> Response {
    change(&state, &headers, &publisher, &name, &body.sha256, Change::Activate).await
}

/// `POST /wasm/plugins/{publisher}/{name}/rollback` with `{"sha256": "<expected previous>"}`.
pub async fn rollback_plugin(
    Extension(state): Extension<Arc<WebState>>,
    headers: HeaderMap,
    Path((publisher, name)): Path<(String, String)>,
    Json(body): Json<DigestBody>,
) -> Response {
    change(&state, &headers, &publisher, &name, &body.sha256, Change::Rollback).await
}

#[derive(Clone, Copy, PartialEq)]
enum Change {
    Activate,
    Rollback,
}

impl Change {
    fn kind(self) -> &'static str {
        match self {
            Change::Activate => "plugin.activated",
            Change::Rollback => "plugin.rolled_back",
        }
    }
}

async fn change(
    state: &Arc<WebState>,
    headers: &HeaderMap,
    publisher: &str,
    name: &str,
    sha: &str,
    change: Change,
) -> Response {
    let id = match plugin_id(publisher, name) {
        Ok(id) => id,
        Err(response) => return *response,
    };
    if let Err(response) = digest(sha) {
        return *response;
    }
    let store = match store_of(state).await {
        Ok(store) => store,
        Err(response) => return *response,
    };
    let actor = state.auth.actor_from_headers(headers).username;
    let _deploy = state.deploy.lock().await;
    let Some(current) = state.registry.read().await.clone() else {
        return api_error(StatusCode::SERVICE_UNAVAILABLE, "runtime_unavailable", "node registry is not available");
    };
    let engine = state.engine.read().await.clone();
    let attached = match current_flows(state).await {
        Ok(flows) => flows,
        Err(response) => return *response,
    };

    let (registry, flows, cfg) = (current.clone(), attached.clone(), engine.as_ref().and_then(|e| e.config().cloned()));
    let (id_for_op, sha_for_op) = (id.clone(), sha.to_owned());
    let pointer = blocking(&store, move |store| {
        let prepare = move |set: Arc<ActivePlugins>| {
            Engine::prepare_flows(&flows, &registry.with_wasm(set), cfg.clone())
                .map_err(|err| N2linkError::invalid_operation(&format!("{PREPARE_FAILED}: {err}")))
        };
        let (entry, pending) = match change {
            Change::Activate => store.activate_pending(&id_for_op, &sha_for_op, &prepare)?,
            Change::Rollback => store.rollback_pending(&id_for_op, &sha_for_op, &prepare)?,
        };
        match store.active_plugins() {
            Ok((set, problems)) => Ok((entry, pending, set, problems)),
            Err(err) => {
                store.revert(pending)?;
                Err(err)
            }
        }
    })
    .await;
    let (entry, pending, set, problems) = match pointer {
        Ok(value) => value,
        Err(response) => {
            let reason = code_of(&response);
            state.history.record_plugin(&actor, change.kind(), &id, None, Some(sha), Some(reason));
            return *response;
        }
    };
    for problem in &problems {
        log::error!("[WASM] plugin left out: {problem}");
    }

    let next = current.with_wasm(set);
    state.set_registry(next.clone()).await;
    if let Some(engine) = engine.as_ref()
        && let Err(err) = engine.redeploy_flows(attached.clone(), &next, None).await
    {
        return undo(state, &store, pending, engine, &current, attached, &actor, &id, sha, change, err).await;
    }
    if let Err(response) = blocking(&store, move |store| store.finish(pending)).await {
        log::error!("[WASM] superseded generation was not deleted; it returns to quarantine on the next start");
        drop(response);
    }
    let version = version_of(&next, &id);
    state.history.record_plugin(&actor, change.kind(), &id, version.as_deref(), Some(&entry.current), None);
    let detail = format!("{id} {}", short(&entry.current));
    let _ = state.audit.record(&actor, change.kind(), Some(&detail)).await;
    Json(activation_json(&id, &entry)).into_response()
}

/// The redeploy with the new set failed: put the pointer, the registry and the graph back.
#[allow(clippy::too_many_arguments)]
async fn undo(
    state: &WebState,
    store: &Arc<PluginStore>,
    pending: PendingChange,
    engine: &Engine,
    current: &RegistryHandle,
    attached: Value,
    actor: &str,
    id: &str,
    sha: &str,
    change: Change,
    err: N2linkError,
) -> Response {
    let reverted = blocking(store, move |store| store.revert(pending)).await;
    state.set_registry(current.clone()).await;
    let restored = engine.redeploy_flows(attached, current, None).await;
    state.history.record_plugin(actor, change.kind(), id, None, Some(sha), Some("activation_failed"));
    match (reverted, restored) {
        (Ok(()), Ok(())) => api_error(
            StatusCode::CONFLICT,
            "activation_failed",
            &format!("redeploy with the new plugin set failed and was undone: {err}"),
        ),
        (reverted, restored) => {
            log::error!(
                "[WASM] activation of {id} failed ({err}); pointer restored: {}, previous graph restored: {}",
                reverted.is_ok(),
                restored.is_ok()
            );
            api_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "activation_failed",
                &format!("redeploy failed ({err}) and the previous state was not fully restored; check the logs"),
            )
        }
    }
}

fn code_of(response: &Response) -> &'static str {
    match response.status() {
        StatusCode::NOT_FOUND => "not_found",
        StatusCode::CONFLICT => "conflict",
        StatusCode::UNPROCESSABLE_ENTITY => "budget_exceeded",
        StatusCode::INTERNAL_SERVER_ERROR => "store_error",
        _ => "invalid",
    }
}

fn version_of(registry: &RegistryHandle, id: &str) -> Option<String> {
    let set = registry.wasm()?;
    set.views().into_iter().find(|view| view.id == id).map(|view| view.version.to_string())
}

fn activation_json(id: &str, entry: &ActiveEntry) -> Value {
    json!({
        "id": id,
        "active": entry.current,
        "previous": entry.previous,
        "activatedAt": entry.activated_at,
        // The editor learns about new or changed plugin types on reload.
        "editorReloadRequired": true,
    })
}

/// `DELETE /wasm/plugins/{publisher}/{name}`: refused while a deployed node uses the type.
pub async fn remove_plugin(
    Extension(state): Extension<Arc<WebState>>,
    headers: HeaderMap,
    Path((publisher, name)): Path<(String, String)>,
) -> Response {
    let id = match plugin_id(&publisher, &name) {
        Ok(id) => id,
        Err(response) => return *response,
    };
    let store = match store_of(&state).await {
        Ok(store) => store,
        Err(response) => return *response,
    };
    let actor = state.auth.actor_from_headers(&headers).username;
    let _deploy = state.deploy.lock().await;
    let flows = match current_flows(&state).await {
        Ok(flows) => flows,
        Err(response) => return *response,
    };
    let users = nodes_using(&flows, &type_name_of(&id));
    if !users.is_empty() {
        return api_error(
            StatusCode::CONFLICT,
            "in_use",
            &format!("plugin {id} is used by deployed node(s) {}; remove them first", users.join(", ")),
        );
    }
    let remove_id = id.clone();
    let set = match blocking(&store, move |store| {
        store.remove(&remove_id)?;
        store.active_plugins()
    })
    .await
    {
        Ok((set, _problems)) => set,
        Err(response) => return *response,
    };
    // No deployed node uses the type, so no redeploy: the next deploy uses the new set.
    if let Some(current) = state.registry.read().await.clone() {
        state.set_registry(current.with_wasm(set)).await;
    }
    state.history.record_plugin(&actor, "plugin.removed", &id, None, None, None);
    let _ = state.audit.record(&actor, "plugin.removed", Some(&id)).await;
    Json(json!({ "removed": id, "editorReloadRequired": true })).into_response()
}

/// `DELETE /wasm/plugins/quarantine/{sha256}`: delete a package that is not active.
pub async fn discard_package(
    Extension(state): Extension<Arc<WebState>>,
    headers: HeaderMap,
    Path(sha): Path<String>,
) -> Response {
    if let Err(response) = digest(&sha) {
        return *response;
    }
    let store = match store_of(&state).await {
        Ok(store) => store,
        Err(response) => return *response,
    };
    let actor = state.auth.actor_from_headers(&headers).username;
    let _deploy = state.deploy.lock().await;
    let target = sha.clone();
    if let Err(response) = blocking(&store, move |store| store.discard(&target)).await {
        return *response;
    }
    state.history.record_plugin(&actor, "plugin.discarded", "quarantine", None, Some(&sha), None);
    let _ = state.audit.record(&actor, "plugin.discarded", Some(short(&sha))).await;
    Json(json!({ "discarded": sha })).into_response()
}

// ---------------------------------------------------------------------------------------------
// Editor, `/nodes` and Copilot entries
// ---------------------------------------------------------------------------------------------

/// JSON safe to place inside `<script>`: `<`, `>`, `&`, U+2028 and U+2029 are escaped so no
/// manifest string can close the element or break the JavaScript.
fn script_json(value: &Value) -> String {
    serde_json::to_string(value)
        .unwrap_or_else(|_| "null".to_owned())
        .replace('<', "\\u003c")
        .replace('>', "\\u003e")
        .replace('&', "\\u0026")
        .replace('\u{2028}', "\\u2028")
        .replace('\u{2029}', "\\u2029")
}

fn html_text(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            '\u{2028}' => out.push_str("&#x2028;"),
            '\u{2029}' => out.push_str("&#x2029;"),
            _ => out.push(c),
        }
    }
    out
}

fn kind_str(kind: ConfigKind) -> &'static str {
    match kind {
        ConfigKind::String => "string",
        ConfigKind::Number => "number",
        ConfigKind::Boolean => "boolean",
        ConfigKind::Enum => "enum",
    }
}

fn pin(view: &PluginView<'_>) -> String {
    format!("{}@{}", view.id, view.version.major)
}

/// One editor definition per active plugin. Built from the manifest in Rust; every plugin
/// string is JSON-escaped (script) or HTML-escaped (template, help), never emitted as markup.
pub fn editor_html(plugins: &ActivePlugins) -> String {
    let mut html = String::new();
    for view in plugins.views() {
        let node = &view.manifest.node;
        let config: Vec<Value> = node
            .config
            .iter()
            .map(|field| {
                json!({
                    "name": field.name,
                    "kind": kind_str(field.kind),
                    "required": field.required,
                    "default": field.default.clone().unwrap_or(match field.kind {
                        ConfigKind::Boolean => Value::Bool(false),
                        _ => Value::String(String::new()),
                    }),
                })
            })
            .collect();
        let def = json!({
            "type": view.type_name,
            "category": node.category,
            "color": node.color,
            "icon": node.icon,
            "label": node.label,
            "outputs": node.outputs,
            "outputLabels": node.output_labels,
            "pin": pin(&view),
            "config": config,
        });
        html.push_str("<script type=\"text/javascript\">\n(function(){\nvar def = ");
        html.push_str(&script_json(&def));
        html.push_str(
            ";\nvar defaults = { name: { value: \"\" }, wasmPlugin: { value: def.pin } };\n\
             def.config.forEach(function (f) { defaults[f.name] = { value: f.default, required: f.required }; });\n\
             RED.nodes.registerType(def.type, {\n\
             category: def.category, color: def.color, defaults: defaults, inputs: 1, outputs: def.outputs,\n\
             icon: def.icon, paletteLabel: def.label, outputLabels: def.outputLabels,\n\
             label: function () { return this.name || def.label; }\n\
             });\n})();\n</script>\n",
        );
        html.push_str(&format!("<script type=\"text/html\" data-template-name=\"{}\">\n", view.type_name));
        html.push_str(
            "<div class=\"form-row\"><label for=\"node-input-name\"><i class=\"fa fa-tag\"></i> Name</label>\
             <input type=\"text\" id=\"node-input-name\"></div>\n",
        );
        for field in &node.config {
            let label = html_text(if field.label.is_empty() { &field.name } else { &field.label });
            let id = format!("node-input-{}", field.name);
            let input = match field.kind {
                ConfigKind::Boolean => {
                    format!("<input type=\"checkbox\" id=\"{id}\" style=\"width:auto;vertical-align:top\">")
                }
                ConfigKind::Enum => {
                    let options: String = field
                        .values
                        .iter()
                        .map(|v| format!("<option value=\"{0}\">{0}</option>", html_text(v)))
                        .collect();
                    format!("<select id=\"{id}\">{options}</select>")
                }
                ConfigKind::Number | ConfigKind::String => format!("<input type=\"text\" id=\"{id}\">"),
            };
            html.push_str(&format!("<div class=\"form-row\"><label for=\"{id}\">{label}</label>{input}</div>\n"));
        }
        html.push_str("</script>\n");
        html.push_str(&format!(
            "<script type=\"text/html\" data-help-name=\"{}\"><p style=\"white-space:pre-wrap\">{}</p>\
             <p>WASM plugin {} {} ({})</p></script>\n",
            view.type_name,
            html_text(&node.help),
            html_text(view.id),
            view.version,
            html_text(&view.manifest.plugin.license)
        ));
    }
    html
}

/// `/nodes` JSON entries: one node set per plugin, module `wasm/<publisher>/<name>`.
pub fn node_sets(plugins: &ActivePlugins) -> Vec<Value> {
    plugins
        .views()
        .into_iter()
        .map(|view| {
            let module = format!("wasm/{}", view.id);
            json!({
                "id": format!("{module}/{}", view.type_name),
                "name": view.type_name,
                "types": [view.type_name],
                "enabled": true,
                "local": false,
                "user": true,
                "module": module,
                "version": view.version.to_string(),
            })
        })
        .collect()
}

/// Copilot catalog entries. Description and help are left out: third-party text in a model
/// prompt is an injection channel.
pub fn catalog_entries(plugins: &ActivePlugins) -> Vec<Value> {
    plugins
        .views()
        .into_iter()
        .map(|view| {
            let node = &view.manifest.node;
            // The manifest checks both lists are empty or one entry per output.
            let declared: Vec<(&str, &str)> = (0..usize::from(node.outputs))
                .map(|i| {
                    (
                        node.output_labels.get(i).map_or("", String::as_str),
                        node.output_payloads.get(i).map_or("any", String::as_str),
                    )
                })
                .collect();
            let (input_payload, output_ports) = n2link_core::runtime::nodes::catalog_ports_json(
                node.inputs,
                node.outputs,
                false,
                node.input_payload.as_deref(),
                &declared,
            );
            json!({
                "type": view.type_name,
                "kind": "flow",
                "module": format!("wasm/{}", view.id),
                "redId": view.type_name,
                "inputs": node.inputs,
                "outputs": node.outputs,
                "dynamicOutputs": false,
                "inputPayload": input_payload,
                "outputPorts": output_ports,
                "config": node.config.iter().map(|f| json!({
                    "name": f.name,
                    "kind": kind_str(f.kind),
                    "required": f.required,
                })).collect::<Vec<_>>(),
                "configRefs": [],
                "secretFields": [],
                "capabilities": [],
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::create_all_routes;
    use axum::body::Body;
    use axum::http::Request;
    use n2link_core::runtime::registry::RegistryBuilder;
    use n2link_core::runtime::wasm::append_manifest;
    use tower::ServiceExt;

    const IDENTITY: &str = r#"(module
      (import "edgelink:node/v1" "emit" (func $emit (param i32 i32 i32) (result i32)))
      (memory (export "memory") 1 1)
      (func (export "el_abi_version") (result i32) i32.const 1)
      (func (export "el_alloc") (param i32) (result i32) i32.const 1024)
      (func (export "el_on_input") (param $ptr i32) (param $len i32) (result i32)
        (call $emit (i32.const 0) (local.get $ptr) (local.get $len))))"#;

    fn manifest(version: &str, expect: u32) -> String {
        format!(
            r##"[plugin]
id = "acme/echo"
version = "{version}"
abi = 1
license = "MIT"

[node]
label = "echo"
category = "plugins"
color = "#C0DEED"
icon = "function.svg"
inputs = 1
outputs = 1

[[selftest]]
input = {{ payload = "a" }}
expect_outputs = [{expect}]
"##
        )
    }

    fn package(version: &str, expect: u32) -> Vec<u8> {
        append_manifest(&wat::parse_str(IDENTITY).unwrap(), &manifest(version, expect)).unwrap()
    }

    #[test]
    fn hostile_manifest_strings_are_escaped_in_editor_html() {
        let hostile = manifest("1.0.0", 1)
            .replace("label = \"echo\"", "label = \"</script><script>alert(1)</script>\"")
            .replace("outputs = 1\n", "outputs = 1\nhelp = \"</script><img src=x onerror=alert(2)>\u{2028}\"\n");
        let package = append_manifest(&wat::parse_str(IDENTITY).unwrap(), &hostile).unwrap();
        let set = ActivePlugins::from_packages(vec![package]).unwrap();
        let html = editor_html(&set);
        assert!(!html.contains("alert(1)</script>"), "{html}");
        assert!(!html.contains("<img"), "{html}");
        assert!(!html.contains('\u{2028}'), "{html}");
        assert!(html.contains("\\u003c/script\\u003e"), "{html}");
        assert!(html.contains("&lt;img src=x"), "{html}");
        assert!(html.contains("acme/echo@1"));
        let catalog = catalog_entries(&set);
        assert_eq!(catalog[0]["type"], "wasm-acme-echo");
        assert!(!catalog[0].to_string().contains("alert"), "{}", catalog[0]);
        assert_eq!(node_sets(&set)[0]["module"], "wasm/acme/echo");
    }

    #[test]
    fn catalog_entries_name_ports_and_payload_types() {
        let unlabelled = append_manifest(&wat::parse_str(IDENTITY).unwrap(), &manifest("1.0.0", 1)).unwrap();
        let set = ActivePlugins::from_packages(vec![unlabelled]).unwrap();
        let entry = &catalog_entries(&set)[0];
        assert_eq!(entry["inputPayload"], "any");
        assert_eq!(entry["outputPorts"], json!([{"index": 0, "name": "output 1", "payload": "any"}]));

        let labelled = manifest("1.0.0", 1).replace(
            "outputs = 1\n",
            "outputs = 1\noutput_labels = [\"echoed\"]\noutput_payloads = [\"string\"]\ninput_payload = \"string|buffer\"\n",
        );
        let package = append_manifest(&wat::parse_str(IDENTITY).unwrap(), &labelled).unwrap();
        let set = ActivePlugins::from_packages(vec![package]).unwrap();
        let entry = &catalog_entries(&set)[0];
        assert_eq!(entry["inputPayload"], "string|buffer");
        assert_eq!(entry["outputPorts"], json!([{"index": 0, "name": "echoed", "payload": "string"}]));
        assert!(entry.get("outputLabels").is_none());
    }

    struct Home(std::path::PathBuf);
    impl Drop for Home {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn settings(home: &std::path::Path) -> config::Config {
        let toml = format!(
            "home_dir = \"{}\"\n[runtime.context]\ndefault = \"memory\"\n[runtime.context.stores]\nmemory = {{ provider = \"memory\" }}\n[runtime.wasm]\nenabled = true\n",
            home.display()
        );
        config::Config::builder().add_source(config::File::from_str(&toml, config::FileFormat::Toml)).build().unwrap()
    }

    async fn call(
        router: &axum::Router,
        method: &str,
        uri: &str,
        body: Body,
        content_type: &str,
    ) -> (StatusCode, Value) {
        let request =
            Request::builder().method(method).uri(uri).header(header::CONTENT_TYPE, content_type).body(body).unwrap();
        let response = router.clone().oneshot(request).await.unwrap();
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
        (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
    }

    async fn stage(router: &axum::Router, bytes: Vec<u8>) -> (StatusCode, Value) {
        call(router, "POST", "/wasm/plugins/stage", Body::from(bytes), "application/wasm").await
    }

    async fn post_json(router: &axum::Router, uri: &str, body: Value) -> (StatusCode, Value) {
        call(router, "POST", uri, Body::from(body.to_string()), "application/json").await
    }

    /// DESIGN.md test plan "Lifecycle drill", online: install A → upgrade B → C fails self-test
    /// → D fails `prepare_flows` → rollback B→A → restart → A active.
    #[tokio::test]
    async fn online_lifecycle_drill() {
        let home = Home(std::env::temp_dir().join(format!("n2linkd-wasm-api-{}", uuid::Uuid::new_v4())));
        std::fs::create_dir_all(&home.0).unwrap();
        let flows_path = home.0.join("flows.json");
        std::fs::write(&flows_path, br#"[{"id":"100","type":"tab"}]"#).unwrap();
        let cfg = settings(&home.0);
        let store = Arc::new(PluginStore::open(&cfg).unwrap());
        let registry =
            RegistryBuilder::default().build().unwrap().with_wasm(ActivePlugins::from_packages(vec![]).unwrap());
        let engine = Engine::with_json(&registry, json!([{ "id": "100", "type": "tab" }]), Some(cfg.clone())).unwrap();
        engine.start().await.unwrap();
        let state = WebState::new();
        state.set_flows_file_path(flows_path.clone()).await;
        state.set_registry(registry).await;
        state.set_engine(Arc::new(engine.clone())).await;
        state.set_plugin_store(store.clone()).await;
        let router = create_all_routes(&state).layer(Extension(state.clone()));

        let (status, _) =
            call(&router, "POST", "/wasm/plugins/stage", Body::from(package("1.0.0", 1)), "text/plain").await;
        assert_eq!(status, StatusCode::UNSUPPORTED_MEDIA_TYPE);
        let (status, a) = stage(&router, package("1.0.0", 1)).await;
        assert_eq!(status, StatusCode::OK, "{a}");
        assert_eq!(a["status"], "ready");
        let a = a["sha256"].as_str().unwrap().to_owned();
        let (_, b) = stage(&router, package("1.1.0", 1)).await;
        let b = b["sha256"].as_str().unwrap().to_owned();
        let (_, c) = stage(&router, package("1.2.0", 0)).await;
        assert_eq!(c["status"], "rejected", "{c}");
        let c = c["sha256"].as_str().unwrap().to_owned();
        let (_, d) = stage(&router, package("2.0.0", 1)).await;
        let d = d["sha256"].as_str().unwrap().to_owned();

        let (status, body) = post_json(&router, "/wasm/plugins/acme/echo/activate", json!({ "sha256": a })).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["editorReloadRequired"], true);

        // Deploy a flow that uses the plugin through the ordinary deploy path.
        let flows = json!({ "flows": [
            { "id": "100", "type": "tab" },
            { "id": "1", "z": "100", "type": "wasm-acme-echo", "wasmPlugin": "acme/echo@1", "x": 1, "y": 1, "wires": [[]] }
        ]});
        let (status, body) = post_json(&router, "/flows", flows).await;
        assert_eq!(status, StatusCode::OK, "{body}");

        let (status, body) = post_json(&router, "/wasm/plugins/acme/echo/activate", json!({ "sha256": b })).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["previous"], a.as_str());

        let (status, body) = post_json(&router, "/wasm/plugins/acme/echo/activate", json!({ "sha256": c })).await;
        assert_eq!((status, body["code"].as_str()), (StatusCode::CONFLICT, Some("selftest_failed")), "{body}");
        let (status, body) = post_json(&router, "/wasm/plugins/acme/echo/activate", json!({ "sha256": d })).await;
        assert_eq!((status, body["code"].as_str()), (StatusCode::CONFLICT, Some("invalid_flows")), "{body}");

        let (_, listing) = call(&router, "GET", "/wasm/plugins", Body::empty(), "application/json").await;
        assert_eq!(listing["active"]["acme/echo"]["current"], b.as_str(), "{listing}");
        assert_eq!(listing["active"]["acme/echo"]["previous"], a.as_str(), "{listing}");

        let (status, body) = post_json(&router, "/wasm/plugins/acme/echo/rollback", json!({ "sha256": b })).await;
        assert_eq!(status, StatusCode::CONFLICT, "{body}");
        let (status, body) = post_json(&router, "/wasm/plugins/acme/echo/rollback", json!({ "sha256": a })).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["active"], a.as_str());

        let (status, body) =
            call(&router, "DELETE", "/wasm/plugins/acme/echo", Body::empty(), "application/json").await;
        assert_eq!((status, body["code"].as_str()), (StatusCode::CONFLICT, Some("in_use")), "{body}");
        let (status, _) = call(&router, "DELETE", &format!("/wasm/plugins/quarantine/{c}"), Body::empty(), "").await;
        assert_eq!(status, StatusCode::OK);
        let (status, body) = post_json(&router, "/wasm/plugins/Acme/echo/activate", json!({ "sha256": a })).await;
        assert_eq!((status, body["code"].as_str()), (StatusCode::BAD_REQUEST, Some("invalid_id")));

        // The editor and catalog describe the active generation.
        let html = editor_html(state.registry.read().await.as_ref().unwrap().wasm().unwrap());
        assert!(html.contains("wasm-acme-echo"));
        let status_doc = call(&router, "GET", "/status", Body::empty(), "").await.1;
        assert_eq!(status_doc["wasm"]["plugins"], 1, "{status_doc}");

        // Restart: a new process opens the store and runs A.
        engine.stop().await.unwrap();
        *state.plugin_store.write().await = None;
        drop(router);
        drop(store);
        let reopened = PluginStore::open(&cfg).unwrap();
        let (set, problems) = reopened.active_plugins().unwrap();
        assert!(problems.is_empty(), "{problems:?}");
        assert_eq!(set.views()[0].version.to_string(), "1.0.0");
        let registry = RegistryBuilder::default().build().unwrap().with_wasm(set);
        let flows = flow_credentials::flows_value_with_credentials(&flows_path, Some(&cfg)).await.unwrap();
        Engine::prepare_flows(&flows, &registry, Some(cfg.clone())).unwrap();
        let _ = d;
    }

    #[test]
    fn plugin_routes_are_administrator_only() {
        use crate::handlers::auth::{allows, permission_for};
        use axum::http::Method;
        let read = permission_for(&Method::GET, "/wasm/plugins").unwrap();
        let write = permission_for(&Method::POST, "/wasm/plugins/stage").unwrap();
        assert_eq!((read, write), ("wasm.read", "wasm.write"));
        for scope in ["read", "*.read", "read,settings.write,flows.write,credentials.write"] {
            assert!(!allows(scope, read) && !allows(scope, write), "{scope}");
        }
        assert!(allows("*", read) && allows("*", write));
    }

    #[tokio::test]
    async fn plugin_routes_answer_disabled_without_a_store() {
        let state = WebState::new();
        state.set_registry(RegistryBuilder::default().build().unwrap()).await;
        let router = create_all_routes(&state).layer(Extension(state));
        let (status, body) = call(&router, "GET", "/wasm/plugins", Body::empty(), "").await;
        assert_eq!((status, body["code"].as_str()), (StatusCode::CONFLICT, Some("plugins_disabled")));
    }
}
