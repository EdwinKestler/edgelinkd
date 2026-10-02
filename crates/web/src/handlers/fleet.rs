//! Inventory of other EdgeLinkd devices and a push of one flows file.
//!
//! The routes stay installed when fleet is off and answer `not_supported`, so a caller cannot
//! mistake a missing route for a push that worked. A push reads the target's current `rev` and
//! sends that rev back. A 409 from the target is returned as-is.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::Extension;
use axum::extract::Path;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use edgelink_core::runtime::engine::Engine;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::RwLock;

use super::WebState;
use super::reply::api_error;

#[derive(Clone, Serialize, Deserialize)]
struct PushNote {
    at: i64,
    ok: bool,
    rev: Option<String>,
    status: u16,
}

#[derive(Clone, Serialize, Deserialize)]
struct Device {
    name: String,
    url: String,
    stage: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    token: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    last: Option<PushNote>,
}

#[derive(Serialize, Deserialize)]
struct FleetFile {
    devices: Vec<Device>,
}

pub struct Fleet {
    enabled: bool,
    home: RwLock<Option<PathBuf>>,
    devices: RwLock<Vec<Device>>,
    client: reqwest::Client,
}

impl Fleet {
    pub fn disabled() -> Self {
        Self {
            enabled: false,
            home: RwLock::new(None),
            devices: RwLock::new(Vec::new()),
            client: reqwest::Client::builder().timeout(Duration::from_secs(10)).build().expect("http client"),
        }
    }

    pub fn enabled(&self) -> bool {
        self.enabled
    }

    pub fn from_config(cfg: &config::Config) -> Result<Self, String> {
        let raw = match cfg.get::<RawFleet>("fleet") {
            Ok(raw) => raw,
            Err(config::ConfigError::NotFound(_)) => return Ok(Self::disabled()),
            Err(err) => return Err(err.to_string()),
        };
        if !raw.enabled {
            return Ok(Self::disabled());
        }
        for device in &raw.devices {
            check_device(&device.name, &device.url, &device.stage)?;
        }
        let mut fleet = Self::disabled();
        fleet.enabled = true;
        fleet.devices = RwLock::new(raw.devices);
        Ok(fleet)
    }

    pub async fn load_home(&self, home: PathBuf) -> Result<(), String> {
        *self.home.write().await = Some(home.clone());
        if !self.enabled {
            return Ok(());
        }
        let path = home.join("fleet.json");
        if !path.exists() {
            return Ok(());
        }
        let text = tokio::fs::read_to_string(&path).await.map_err(|err| err.to_string())?;
        let file: FleetFile = serde_json::from_str(&text).map_err(|err| format!("fleet.json is not valid: {err}"))?;
        for device in &file.devices {
            check_device(&device.name, &device.url, &device.stage)?;
        }
        *self.devices.write().await = file.devices;
        Ok(())
    }

    pub async fn push(&self, name: &str, flows: Vec<Value>) -> Result<(StatusCode, Value), String> {
        let device = self.device(name).await.ok_or_else(|| format!("device '{name}' was not found"))?;
        let rev = self.target_rev(&device).await?;
        let (status, body) = self.post_flows(&device, &flows, &rev).await?;
        let pushed = Engine::revision_of(&Value::Array(flows));
        self.note(&device.name, status.is_success(), Some(pushed), status.as_u16()).await?;
        Ok((status, body))
    }

    pub async fn promote(&self, from: &str, to: &str) -> Result<(StatusCode, Value), String> {
        let source = self.device(from).await.ok_or_else(|| format!("device '{from}' was not found"))?;
        let flows = self.target_flows(&source).await?;
        self.push(to, flows).await
    }

    async fn device(&self, name: &str) -> Option<Device> {
        self.devices.read().await.iter().find(|device| device.name == name).cloned()
    }

    async fn target_rev(&self, device: &Device) -> Result<String, String> {
        let body = self.get_flows(device).await?;
        body.get("rev")
            .and_then(Value::as_str)
            .filter(|rev| !rev.is_empty())
            .map(str::to_string)
            .ok_or_else(|| format!("device '{}' did not return a rev", device.name))
    }

    async fn target_flows(&self, device: &Device) -> Result<Vec<Value>, String> {
        let body = self.get_flows(device).await?;
        body.get("flows")
            .and_then(Value::as_array)
            .cloned()
            .ok_or_else(|| format!("device '{}' did not return flows", device.name))
    }

    async fn get_flows(&self, device: &Device) -> Result<Value, String> {
        let url = flows_url(device);
        let mut request = self.client.get(url);
        if let Some(token) = device.token.as_deref().filter(|token| !token.is_empty()) {
            request = request.bearer_auth(token);
        }
        let response = request.send().await.map_err(|err| err.to_string())?;
        let status = response.status();
        let text = response.text().await.map_err(|err| err.to_string())?;
        if !status.is_success() {
            return Err(format!("device '{}' returned {status}", device.name));
        }
        serde_json::from_str(&text).map_err(|err| err.to_string())
    }

    async fn post_flows(&self, device: &Device, flows: &[Value], rev: &str) -> Result<(StatusCode, Value), String> {
        let url = flows_url(device);
        let mut request = self.client.post(url).json(&json!({ "flows": flows, "rev": rev }));
        if let Some(token) = device.token.as_deref().filter(|token| !token.is_empty()) {
            request = request.bearer_auth(token);
        }
        let response = request.send().await.map_err(|err| err.to_string())?;
        let status = StatusCode::from_u16(response.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
        let text = response.text().await.map_err(|err| err.to_string())?;
        let body = serde_json::from_str(&text).unwrap_or_else(|_| json!({ "message": text }));
        Ok((status, body))
    }

    async fn note(&self, name: &str, ok: bool, rev: Option<String>, status: u16) -> Result<(), String> {
        {
            let mut devices = self.devices.write().await;
            let Some(device) = devices.iter_mut().find(|device| device.name == name) else {
                return Err(format!("device '{name}' was not found"));
            };
            device.last = Some(PushNote { at: unix_ms(), ok, rev, status });
        }
        self.store().await
    }

    async fn store(&self) -> Result<(), String> {
        let home = self.home.read().await.clone().ok_or_else(|| "fleet home is not configured".to_string())?;
        let path = home.join("fleet.json");
        let devices = self.devices.read().await.clone();
        let text = serde_json::to_string_pretty(&FleetFile { devices }).map_err(|err| err.to_string())?;
        edgelink_core::utils::atomic_file::write_bytes(&path, text.as_bytes(), true).await
    }
}

#[derive(Deserialize)]
struct RawFleet {
    #[serde(default)]
    enabled: bool,
    #[serde(default)]
    devices: Vec<Device>,
}

fn check_device(name: &str, url: &str, stage: &str) -> Result<(), String> {
    if name.trim().is_empty() {
        return Err("fleet device has no name".to_string());
    }
    if url.trim().is_empty() {
        return Err(format!("fleet device '{name}' has no url"));
    }
    if !matches!(stage, "development" | "production") {
        return Err(format!("fleet device '{name}' has stage '{stage}'"));
    }
    Ok(())
}

fn flows_url(device: &Device) -> String {
    format!("{}/flows", device.url.trim_end_matches('/'))
}

fn unix_ms() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|elapsed| elapsed.as_millis() as i64).unwrap_or(0)
}

fn disabled_response() -> Response {
    api_error(StatusCode::NOT_FOUND, "not_supported", "fleet is not enabled")
}

pub async fn get_devices(Extension(state): Extension<Arc<WebState>>) -> Response {
    if !state.fleet.enabled() {
        return disabled_response();
    }
    let devices = state.fleet.devices.read().await;
    let listed = devices
        .iter()
        .map(|device| {
            json!({
                "name": device.name,
                "url": device.url,
                "stage": device.stage,
                "last": device.last,
            })
        })
        .collect::<Vec<_>>();
    axum::Json(listed).into_response()
}

#[derive(Deserialize)]
struct DeviceInput {
    name: String,
    url: String,
    stage: String,
    #[serde(default)]
    token: Option<String>,
}

pub async fn post_device(Extension(state): Extension<Arc<WebState>>, _headers: HeaderMap, body: String) -> Response {
    if !state.fleet.enabled() {
        return disabled_response();
    }
    let input: DeviceInput = match serde_json::from_str(&body) {
        Ok(input) => input,
        Err(_) => return api_error(StatusCode::BAD_REQUEST, "bad_request", "fleet device is not valid"),
    };
    if let Err(err) = check_device(&input.name, &input.url, &input.stage) {
        return api_error(StatusCode::BAD_REQUEST, "bad_request", &err);
    }
    {
        let mut devices = state.fleet.devices.write().await;
        if let Some(existing) = devices.iter_mut().find(|device| device.name == input.name) {
            existing.url = input.url;
            existing.stage = input.stage;
            existing.token = input.token.filter(|token| !token.is_empty());
        } else {
            devices.push(Device {
                name: input.name,
                url: input.url,
                stage: input.stage,
                token: input.token.filter(|token| !token.is_empty()),
                last: None,
            });
        }
    }
    if let Err(err) = state.fleet.store().await {
        return api_error(StatusCode::INTERNAL_SERVER_ERROR, "unexpected_error", &err);
    }
    StatusCode::NO_CONTENT.into_response()
}

pub async fn delete_device(Extension(state): Extension<Arc<WebState>>, Path(name): Path<String>) -> Response {
    if !state.fleet.enabled() {
        return disabled_response();
    }
    let removed = {
        let mut devices = state.fleet.devices.write().await;
        let before = devices.len();
        devices.retain(|device| device.name != name);
        devices.len() < before
    };
    if !removed {
        return api_error(StatusCode::NOT_FOUND, "not_found", &format!("device '{name}' was not found"));
    }
    if let Err(err) = state.fleet.store().await {
        return api_error(StatusCode::INTERNAL_SERVER_ERROR, "unexpected_error", &err);
    }
    StatusCode::NO_CONTENT.into_response()
}

#[derive(Deserialize)]
struct PushInput {
    device: String,
}

pub async fn post_push(Extension(state): Extension<Arc<WebState>>, headers: HeaderMap, body: String) -> Response {
    if !state.fleet.enabled() {
        return disabled_response();
    }
    let input: PushInput = match serde_json::from_str(&body) {
        Ok(input) => input,
        Err(_) => return api_error(StatusCode::BAD_REQUEST, "bad_request", "fleet push is not valid"),
    };
    let flows = match local_flows(&state).await {
        Ok(flows) => flows,
        Err(err) => return api_error(StatusCode::INTERNAL_SERVER_ERROR, "unexpected_error", &err),
    };
    finish_remote(&state, &headers, "fleet.push", state.fleet.push(&input.device, flows).await).await
}

#[derive(Deserialize)]
struct PromoteInput {
    from: String,
    to: String,
}

pub async fn post_promote(Extension(state): Extension<Arc<WebState>>, headers: HeaderMap, body: String) -> Response {
    if !state.fleet.enabled() {
        return disabled_response();
    }
    let input: PromoteInput = match serde_json::from_str(&body) {
        Ok(input) => input,
        Err(_) => return api_error(StatusCode::BAD_REQUEST, "bad_request", "fleet promote is not valid"),
    };
    finish_remote(&state, &headers, "fleet.promote", state.fleet.promote(&input.from, &input.to).await).await
}

async fn finish_remote(
    state: &WebState,
    headers: &HeaderMap,
    event: &str,
    result: Result<(StatusCode, Value), String>,
) -> Response {
    match result {
        Ok((status, body)) => {
            let actor = state.auth.actor_from_headers(headers);
            let rev = body.get("rev").and_then(Value::as_str).map(str::to_string);
            let _ = state.audit.record(&actor.username, event, rev.as_deref()).await;
            (status, axum::Json(body)).into_response()
        }
        Err(err) => {
            let code = if err.contains("was not found") { "not_found" } else { "not_available" };
            let status = if code == "not_found" { StatusCode::NOT_FOUND } else { StatusCode::BAD_GATEWAY };
            api_error(status, code, &err)
        }
    }
}

async fn local_flows(state: &WebState) -> Result<Vec<Value>, String> {
    let path = state.flows_file_path.read().await.clone().ok_or_else(|| "flows file is not configured".to_string())?;
    if !path.exists() {
        return Ok(Vec::new());
    }
    let text = tokio::fs::read_to_string(path).await.map_err(|err| err.to_string())?;
    if text.trim().is_empty() {
        return Ok(Vec::new());
    }
    serde_json::from_str(&text).map_err(|err| err.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::create_all_routes;
    use crate::handlers::auth::AdminAuth;
    use crate::models::RedSystemSettings;
    use axum::body::Body;
    use axum::http::Request;
    use std::sync::Mutex;
    use tower::ServiceExt;

    fn enabled_fleet() -> Fleet {
        let mut fleet = Fleet::disabled();
        fleet.enabled = true;
        fleet
    }

    async fn serve(router: axum::Router) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, router).await;
        });
        format!("http://{address}")
    }

    #[tokio::test]
    async fn a_disabled_fleet_does_not_pretend_to_push() {
        let state = WebState::new();
        let router = create_all_routes(&state).layer(Extension(state));
        let response = router
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/fleet/pipelines/promote")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"from":"bench","to":"line"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let body: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(body["code"], "not_supported");
        assert_eq!(body["message"], "fleet is not enabled");
    }

    #[tokio::test]
    async fn promote_posts_the_target_revision() {
        let seen = Arc::new(Mutex::new(Value::Null));
        let seen_post = Arc::clone(&seen);
        let development = axum::Router::new().route(
            "/flows",
            axum::routing::get(|| async {
                axum::Json(json!({ "flows": [{ "id": "dev", "type": "tab" }], "rev": "source-rev" }))
            }),
        );
        let production = axum::Router::new().route(
            "/flows",
            axum::routing::get(|| async {
                axum::Json(json!({ "flows": [{ "id": "prod", "type": "tab" }], "rev": "target-rev" }))
            })
            .post(move |body: String| {
                let seen_post = Arc::clone(&seen_post);
                async move {
                    *seen_post.lock().expect("posted flows") = serde_json::from_str(&body).unwrap();
                    axum::Json(json!({ "rev": "accepted" }))
                }
            }),
        );
        let dev_url = serve(development).await;
        let prod_url = serve(production).await;

        let state = WebState::assemble(
            Arc::new(RedSystemSettings::default()),
            std::env::temp_dir(),
            None,
            AdminAuth::open(),
            enabled_fleet(),
        );
        let dir = std::env::temp_dir().join(format!("edgelinkd-fleet-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        state.set_flows_file_path(dir.join("flows.json")).await;
        let router = create_all_routes(&state).layer(Extension(state.clone()));

        for (name, url, stage) in [("bench", dev_url, "development"), ("line", prod_url, "production")] {
            let response = router
                .clone()
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri("/fleet/devices")
                        .header("content-type", "application/json")
                        .body(Body::from(json!({ "name": name, "url": url, "stage": stage }).to_string()))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::NO_CONTENT);
        }

        let response = router
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/fleet/pipelines/promote")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"from":"bench","to":"line"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let posted = seen.lock().expect("posted flows").clone();
        assert_eq!(posted["rev"], "target-rev");
        assert_eq!(posted["flows"][0]["id"], "dev");
        let log = std::fs::read_to_string(dir.join("audit.log")).unwrap();
        assert!(log.contains("fleet.promote"));
        assert!(log.contains("anonymous"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_unknown_stage_is_rejected() {
        let built = config::Config::builder()
            .add_source(config::File::from_str(
                r#"
                [fleet]
                enabled = true
                [[fleet.devices]]
                name = "line"
                url = "http://127.0.0.1:9"
                stage = "lab"
                "#,
                config::FileFormat::Toml,
            ))
            .build()
            .unwrap();
        let Err(message) = Fleet::from_config(&built) else {
            panic!("expected an error");
        };
        assert!(message.contains("stage"));
    }
}
