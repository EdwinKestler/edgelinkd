//! Optional editor login.
//!
//! Unset admin configuration leaves every route open, which is how a default install behaves.
//! A configured password, user list, or OIDC issuer requires a bearer token. Viewer is `read`.
//! Deployer is `*`. An OIDC role claim of `deployer` is deployer; every other claim, including a
//! missing one, is viewer.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::sync::broadcast;
use tokio::time::Instant as TokioInstant;

use axum::Extension;
use axum::extract::Query;
use axum::http::{HeaderMap, Method, StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use serde_json::{Value, json};

use super::WebState;
use super::reply::api_error;

const SESSION_SECS: u64 = 7 * 24 * 60 * 60;
const ATTEMPT_LIMIT: usize = 5;
const ATTEMPT_WINDOW: Duration = Duration::from_secs(10 * 60);
const PENDING_TTL: Duration = Duration::from_secs(10 * 60);
const DUMMY_PASSWORD: &str = "xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx";

struct LocalUser {
    password: String,
    permissions: String,
}

#[derive(Clone)]
struct OidcConfig {
    issuer: String,
    client_id: String,
    client_secret: String,
    role_claim: String,
    redirect_url: String,
}

#[derive(Clone, Deserialize)]
struct Discovery {
    authorization_endpoint: String,
    token_endpoint: String,
    userinfo_endpoint: String,
}

struct Session {
    username: String,
    permissions: String,
    expires: TokioInstant,
}

struct Pending {
    username: String,
    permissions: String,
    expires: Instant,
}

pub struct Actor {
    pub username: String,
    pub permissions: String,
}

pub enum Access {
    Allowed,
    Missing,
    Denied,
}

pub(crate) enum LoginFail {
    Rejected,
    Locked,
}

pub(crate) struct Issued {
    token: String,
    expires_in: u64,
}

impl Issued {
    pub(crate) fn token(&self) -> &str {
        &self.token
    }
}

pub struct AdminAuth {
    enabled: bool,
    users: HashMap<String, LocalUser>,
    oidc: Option<OidcConfig>,
    sessions: Mutex<HashMap<String, Session>>,
    attempts: Mutex<HashMap<String, Vec<Instant>>>,
    states: Mutex<HashMap<String, Instant>>,
    exchanges: Mutex<HashMap<String, Pending>>,
    revocations: broadcast::Sender<String>,
    client: reqwest::Client,
}

impl AdminAuth {
    pub fn open() -> Self {
        Self {
            enabled: false,
            users: HashMap::new(),
            oidc: None,
            sessions: Mutex::new(HashMap::new()),
            attempts: Mutex::new(HashMap::new()),
            states: Mutex::new(HashMap::new()),
            exchanges: Mutex::new(HashMap::new()),
            revocations: broadcast::channel(32).0,
            client: http_client().expect("http client"),
        }
    }

    pub fn enabled(&self) -> bool {
        self.enabled
    }

    pub fn login_kind(&self) -> &'static str {
        if self.oidc.is_some() && self.users.is_empty() { "strategy" } else { "credentials" }
    }

    pub fn from_config(cfg: &config::Config) -> Result<Self, String> {
        let password = optional::<String>(cfg, "admin.password")?;
        let listed = optional::<Vec<RawUser>>(cfg, "admin.users")?.unwrap_or_default();
        let oidc = oidc_from(cfg)?;
        let mut users = HashMap::new();
        for user in listed {
            let username = user.username.trim();
            if username.is_empty() {
                return Err("admin user has no username".to_string());
            }
            if users.contains_key(username) {
                return Err(format!("admin user '{username}' is listed more than once"));
            }
            if user.password.trim().is_empty() {
                return Err(format!("admin user '{username}' has no password"));
            }
            let permissions = local_permissions(&user.role)?;
            users.insert(
                username.to_string(),
                LocalUser { password: user.password, permissions: permissions.to_string() },
            );
        }
        if let Some(password) = password.filter(|value| !value.trim().is_empty())
            && !users.contains_key("admin")
        {
            users.insert("admin".to_string(), LocalUser { password, permissions: "*".to_string() });
        }
        let mut auth = Self::open();
        auth.enabled = !users.is_empty() || oidc.is_some();
        auth.users = users;
        auth.oidc = oidc;
        Ok(auth)
    }

    pub fn authorize(&self, headers: &HeaderMap, permission: &str) -> Access {
        let Some(token) = bearer(headers) else {
            return Access::Missing;
        };
        let Some(actor) = self.actor_for_token(token) else {
            return Access::Missing;
        };
        if allows(&actor.permissions, permission) { Access::Allowed } else { Access::Denied }
    }

    pub fn actor_from_headers(&self, headers: &HeaderMap) -> Actor {
        if !self.enabled {
            return Actor { username: "anonymous".to_string(), permissions: "*".to_string() };
        }
        bearer(headers)
            .and_then(|token| self.actor_for_token(token))
            .unwrap_or(Actor { username: "anonymous".to_string(), permissions: String::new() })
    }

    pub(crate) fn login(&self, username: &str, password: &str) -> Result<Issued, LoginFail> {
        if self.locked(username) {
            return Err(LoginFail::Locked);
        }
        let (configured, permissions) = match self.users.get(username) {
            Some(user) => (user.password.clone(), Some(user.permissions.clone())),
            None => (DUMMY_PASSWORD.to_string(), None),
        };
        if passwords_match(&configured, password)
            && let Some(permissions) = permissions
        {
            self.clear_attempts(username);
            return Ok(self.issue(username, &permissions));
        }
        self.note_failure(username);
        Err(LoginFail::Rejected)
    }

    pub(crate) fn issue(&self, username: &str, permissions: &str) -> Issued {
        let token = format!("{}{}", uuid::Uuid::new_v4().simple(), uuid::Uuid::new_v4().simple());
        let mut sessions = self.sessions.lock().expect("sessions");
        sessions.insert(
            token.clone(),
            Session {
                username: username.to_string(),
                permissions: permissions.to_string(),
                expires: TokioInstant::now() + Duration::from_secs(SESSION_SECS),
            },
        );
        Issued { token, expires_in: SESSION_SECS }
    }

    #[cfg(test)]
    pub(crate) fn issue_with_ttl(&self, username: &str, permissions: &str, ttl: Duration) -> Issued {
        let token = format!("{}{}", uuid::Uuid::new_v4().simple(), uuid::Uuid::new_v4().simple());
        let mut sessions = self.sessions.lock().expect("sessions");
        sessions.insert(
            token.clone(),
            Session {
                username: username.to_string(),
                permissions: permissions.to_string(),
                expires: TokioInstant::now() + ttl,
            },
        );
        Issued { token, expires_in: ttl.as_secs() }
    }

    pub fn revoke(&self, token: &str) {
        self.sessions.lock().expect("sessions").remove(token);
        let _ = self.revocations.send(token.to_string());
    }

    pub fn session_valid(&self, token: &str) -> bool {
        self.actor_for_token(token).is_some()
    }

    pub fn subscribe_revocations(&self) -> broadcast::Receiver<String> {
        self.revocations.subscribe()
    }

    pub fn session_expires(&self, token: &str) -> Option<TokioInstant> {
        let sessions = self.sessions.lock().expect("sessions");
        let session = sessions.get(token)?;
        if session.expires <= TokioInstant::now() {
            return None;
        }
        Some(session.expires)
    }

    pub fn issue_state(&self) -> String {
        let state = uuid::Uuid::new_v4().simple().to_string();
        self.states.lock().expect("oidc state").insert(state.clone(), Instant::now() + PENDING_TTL);
        state
    }

    pub fn take_state(&self, state: &str) -> bool {
        let mut states = self.states.lock().expect("oidc state");
        let Some(expires) = states.remove(state) else {
            return false;
        };
        expires > Instant::now()
    }

    pub fn issue_exchange(&self, username: &str, permissions: &str) -> String {
        let code = uuid::Uuid::new_v4().simple().to_string();
        self.exchanges.lock().expect("exchange codes").insert(
            code.clone(),
            Pending {
                username: username.to_string(),
                permissions: permissions.to_string(),
                expires: Instant::now() + PENDING_TTL,
            },
        );
        code
    }

    pub fn take_exchange(&self, code: &str) -> Option<(String, String)> {
        let mut exchanges = self.exchanges.lock().expect("exchange codes");
        let pending = exchanges.remove(code)?;
        if pending.expires <= Instant::now() {
            return None;
        }
        Some((pending.username, pending.permissions))
    }

    fn actor_for_token(&self, token: &str) -> Option<Actor> {
        let mut sessions = self.sessions.lock().expect("sessions");
        let session = sessions.get(token)?;
        if session.expires <= TokioInstant::now() {
            sessions.remove(token);
            return None;
        }
        Some(Actor { username: session.username.clone(), permissions: session.permissions.clone() })
    }

    fn locked(&self, username: &str) -> bool {
        let now = Instant::now();
        let attempts = self.attempts.lock().expect("login attempts");
        attempts.get(username).is_some_and(|times| {
            times.iter().filter(|at| now.duration_since(**at) < ATTEMPT_WINDOW).count() >= ATTEMPT_LIMIT
        })
    }

    fn note_failure(&self, username: &str) {
        let now = Instant::now();
        let mut attempts = self.attempts.lock().expect("login attempts");
        let times = attempts.entry(username.to_string()).or_default();
        times.retain(|at| now.duration_since(*at) < ATTEMPT_WINDOW);
        times.push(now);
    }

    fn clear_attempts(&self, username: &str) {
        self.attempts.lock().expect("login attempts").remove(username);
    }

    async fn discovery(&self, oidc: &OidcConfig) -> Result<Discovery, String> {
        let url = format!("{}/.well-known/openid-configuration", oidc.issuer.trim_end_matches('/'));
        let response = self.client.get(url).send().await.map_err(|err| err.to_string())?;
        if !response.status().is_success() {
            return Err(format!("status {}", response.status()));
        }
        response.json().await.map_err(|err| err.to_string())
    }

    async fn exchange_code(&self, oidc: &OidcConfig, discovery: &Discovery, code: &str) -> Result<Value, String> {
        let response = self
            .client
            .post(&discovery.token_endpoint)
            .form(&[
                ("grant_type", "authorization_code"),
                ("code", code),
                ("redirect_uri", oidc.redirect_url.as_str()),
                ("client_id", oidc.client_id.as_str()),
                ("client_secret", oidc.client_secret.as_str()),
            ])
            .send()
            .await
            .map_err(|err| err.to_string())?;
        if !response.status().is_success() {
            return Err(format!("status {}", response.status()));
        }
        response.json().await.map_err(|err| err.to_string())
    }

    async fn userinfo(&self, endpoint: &str, access_token: &str) -> Result<Value, String> {
        let response =
            self.client.get(endpoint).bearer_auth(access_token).send().await.map_err(|err| err.to_string())?;
        if !response.status().is_success() {
            return Err(format!("status {}", response.status()));
        }
        response.json().await.map_err(|err| err.to_string())
    }
}

#[derive(Deserialize)]
struct RawUser {
    username: String,
    password: String,
    role: String,
}

fn local_permissions(role: &str) -> Result<&'static str, String> {
    match role {
        "viewer" => Ok("read"),
        "deployer" => Ok("*"),
        other => Err(format!("admin role '{other}' is not supported")),
    }
}

fn oidc_from(cfg: &config::Config) -> Result<Option<OidcConfig>, String> {
    let Some(_) = optional::<config::Value>(cfg, "admin.oidc")? else {
        return Ok(None);
    };
    let issuer = required(cfg, "admin.oidc.issuer")?;
    let client_id = required(cfg, "admin.oidc.client_id")?;
    let client_secret = required(cfg, "admin.oidc.client_secret")?;
    let role_claim = required(cfg, "admin.oidc.role_claim")?;
    let redirect_url = required(cfg, "admin.oidc.redirect_url")?;
    if [issuer.as_str(), client_id.as_str(), client_secret.as_str(), role_claim.as_str(), redirect_url.as_str()]
        .iter()
        .any(|value| value.trim().is_empty())
    {
        return Err("admin.oidc is not complete".to_string());
    }
    Ok(Some(OidcConfig { issuer, client_id, client_secret, role_claim, redirect_url }))
}

fn required(cfg: &config::Config, key: &str) -> Result<String, String> {
    match cfg.get::<String>(key) {
        Ok(value) => Ok(value),
        Err(_) => Err(format!("{key} is required")),
    }
}

fn optional<T: serde::de::DeserializeOwned>(cfg: &config::Config, key: &str) -> Result<Option<T>, String> {
    match cfg.get::<T>(key) {
        Ok(value) => Ok(Some(value)),
        Err(config::ConfigError::NotFound(_)) => Ok(None),
        Err(err) => Err(err.to_string()),
    }
}

fn http_client() -> Result<reqwest::Client, String> {
    reqwest::Client::builder().timeout(Duration::from_secs(10)).build().map_err(|err| err.to_string())
}

pub fn allows(scope: &str, permission: &str) -> bool {
    if permission.is_empty() {
        return true;
    }
    scope.split(',').map(str::trim).any(|item| {
        item == "*" || item == permission || ((item == "read" || item == "*.read") && is_read_permission(permission))
    })
}

fn is_read_permission(permission: &str) -> bool {
    permission == "read" || permission.strip_suffix(".read").is_some_and(|head| !head.is_empty())
}

pub fn permission_for(method: &Method, path: &str) -> Option<&'static str> {
    if *method == Method::OPTIONS {
        return None;
    }
    let path = path.split('?').next().unwrap_or(path);
    if is_public(path) {
        return None;
    }
    if path.starts_with("/settings") {
        return Some("settings.read");
    }
    if path.starts_with("/status") {
        return Some("status.read");
    }
    if path.starts_with("/credentials") {
        let write = !matches!(*method, Method::GET | Method::HEAD);
        return Some(if write { "credentials.write" } else { "credentials.read" });
    }
    let write = !matches!(*method, Method::GET | Method::HEAD);
    let resource = if path.starts_with("/nodes") {
        "nodes"
    } else if path.starts_with("/context") {
        "context"
    } else if path.starts_with("/plugins") {
        "plugins"
    } else {
        "flows"
    };
    Some(if write { write_permission(resource) } else { read_permission(resource) })
}

fn read_permission(resource: &str) -> &'static str {
    match resource {
        "nodes" => "nodes.read",
        "context" => "context.read",
        "plugins" => "plugins.read",
        _ => "flows.read",
    }
}

fn write_permission(resource: &str) -> &'static str {
    match resource {
        "nodes" => "nodes.write",
        "context" => "context.write",
        "plugins" => "plugins.write",
        _ => "flows.write",
    }
}

fn is_public(path: &str) -> bool {
    path.starts_with("/auth")
        || path == "/comms"
        || path.starts_with("/api/")
        || path.starts_with("/icons")
        || path == "/theme"
        || path.starts_with("/locales")
        || path.starts_with("/core/")
}

fn bearer(headers: &HeaderMap) -> Option<&str> {
    let value = headers.get(header::AUTHORIZATION)?.to_str().ok()?;
    let token = value.strip_prefix("Bearer ").or_else(|| value.strip_prefix("bearer "))?;
    let token = token.trim();
    if token.is_empty() { None } else { Some(token) }
}

fn passwords_match(configured: &str, given: &str) -> bool {
    let left = configured.as_bytes();
    let right = given.as_bytes();
    let width = left.len().max(right.len());
    let mut diff = left.len() ^ right.len();
    for index in 0..width {
        let a = left.get(index).copied().unwrap_or(0);
        let b = right.get(index).copied().unwrap_or(0);
        diff |= usize::from(a ^ b);
    }
    diff == 0
}

pub fn authorize_url(endpoint: &str, client_id: &str, redirect_url: &str, state: &str) -> String {
    let query = format!(
        "response_type=code&client_id={}&redirect_uri={}&scope=openid&state={}",
        form_encode(client_id),
        form_encode(redirect_url),
        form_encode(state)
    );
    if endpoint.contains('?') { format!("{endpoint}&{query}") } else { format!("{endpoint}?{query}") }
}

fn claim_permissions(claim: Option<&str>) -> &'static str {
    if claim == Some("deployer") { "*" } else { "read" }
}

fn actor_from_userinfo(oidc: &OidcConfig, info: &Value) -> (String, String) {
    let username = info
        .get("preferred_username")
        .and_then(Value::as_str)
        .or_else(|| info.get("sub").and_then(Value::as_str))
        .unwrap_or("oidc");
    let permissions = claim_permissions(info.get(&oidc.role_claim).and_then(Value::as_str));
    (username.to_string(), permissions.to_string())
}

#[derive(Deserialize, Default)]
struct TokenBody {
    #[serde(default)]
    grant_type: Option<String>,
    #[serde(default)]
    username: Option<String>,
    #[serde(default)]
    password: Option<String>,
    #[serde(default)]
    code: Option<String>,
    #[serde(default)]
    client_id: Option<String>,
    #[serde(default)]
    token: Option<String>,
}

fn parse_token_body(content_type: Option<&str>, body: &str) -> Result<TokenBody, ()> {
    if content_type.is_some_and(|value| value.contains("json")) || body.trim_start().starts_with('{') {
        return serde_json::from_str(body).map_err(|_| ());
    }
    let mut parsed = TokenBody::default();
    for pair in body.split('&') {
        if pair.is_empty() {
            continue;
        }
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        let value = decode_form(value);
        match decode_form(key).as_str() {
            "grant_type" => parsed.grant_type = Some(value),
            "username" => parsed.username = Some(value),
            "password" => parsed.password = Some(value),
            "code" => parsed.code = Some(value),
            "client_id" => parsed.client_id = Some(value),
            "token" => parsed.token = Some(value),
            _ => {}
        }
    }
    Ok(parsed)
}

fn decode_form(value: &str) -> String {
    let value = value.replace('+', " ");
    let bytes = value.as_bytes();
    let mut out = Vec::new();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            let hex = std::str::from_utf8(&bytes[index + 1..index + 3]).unwrap_or("");
            if let Ok(byte) = u8::from_str_radix(hex, 16) {
                out.push(byte);
                index += 3;
                continue;
            }
        }
        out.push(bytes[index]);
        index += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn form_encode(value: &str) -> String {
    let mut out = String::new();
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(byte as char),
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

fn client_allowed(client_id: Option<&str>) -> bool {
    matches!(client_id, None | Some("") | Some("node-red-editor") | Some("node-red-admin"))
}

fn token_response(issued: &Issued) -> Response {
    Json(json!({
        "access_token": issued.token(),
        "accessToken": issued.token(),
        "token_type": "Bearer",
        "expires_in": issued.expires_in,
    }))
    .into_response()
}

use axum::Json;

fn strategy_path(root: &str) -> String {
    let root = root.trim_end_matches('/');
    if root.is_empty() { "auth/strategy".to_string() } else { format!("{root}/auth/strategy") }
}

fn redirect_with(root: &str, query: &str) -> String {
    let root = root.trim_end_matches('/');
    if root.is_empty() { format!("/?{query}") } else { format!("{root}/?{query}") }
}

/// 302, the status the editor follows after an Express `res.redirect`.
fn found(location: &str) -> Response {
    (StatusCode::FOUND, [(header::LOCATION, location.to_string())]).into_response()
}

pub async fn get_login(Extension(state): Extension<Arc<WebState>>) -> Response {
    if !state.auth.enabled() {
        return Json(json!({})).into_response();
    }
    if state.auth.login_kind() == "strategy" {
        return Json(json!({
            "type": "strategy",
            "prompts": [{
                "type": "button",
                "label": "Sign in",
                "url": strategy_path(&state.red_settings.http_admin_root),
            }]
        }))
        .into_response();
    }
    Json(json!({
        "type": "credentials",
        "prompts": [
            {"id": "username", "type": "text", "label": "user.username"},
            {"id": "password", "type": "password", "label": "user.password"}
        ]
    }))
    .into_response()
}

pub async fn post_token(Extension(state): Extension<Arc<WebState>>, headers: HeaderMap, body: String) -> Response {
    if !state.auth.enabled() {
        return api_error(StatusCode::UNAUTHORIZED, "unauthorized", "admin password is not configured");
    }
    let content_type = headers.get(header::CONTENT_TYPE).and_then(|value| value.to_str().ok());
    let Ok(request) = parse_token_body(content_type, &body) else {
        return api_error(StatusCode::BAD_REQUEST, "bad_request", "token request is not valid");
    };
    if !client_allowed(request.client_id.as_deref()) {
        return api_error(StatusCode::UNAUTHORIZED, "unauthorized", "credentials required");
    }
    if let Some(code) = request.code.as_deref().filter(|code| !code.is_empty()) {
        let Some((username, permissions)) = state.auth.take_exchange(code) else {
            return api_error(StatusCode::UNAUTHORIZED, "unauthorized", "credentials required");
        };
        let issued = state.auth.issue(&username, &permissions);
        return token_response(&issued);
    }
    let username = request.username.unwrap_or_default();
    let password = request.password.unwrap_or_default();
    if username.is_empty() {
        return api_error(StatusCode::UNAUTHORIZED, "unauthorized", "credentials required");
    }
    match state.auth.login(&username, &password) {
        Ok(issued) => {
            let _ = state.audit.record(&username, "auth.login", None).await;
            token_response(&issued)
        }
        Err(LoginFail::Locked) => {
            let _ = state.audit.record(&username, "auth.login.fail", None).await;
            api_error(StatusCode::UNAUTHORIZED, "unauthorized", "too many attempts")
        }
        Err(LoginFail::Rejected) => {
            let _ = state.audit.record(&username, "auth.login.fail", None).await;
            api_error(StatusCode::UNAUTHORIZED, "unauthorized", "credentials required")
        }
    }
}

pub async fn post_revoke(Extension(state): Extension<Arc<WebState>>, headers: HeaderMap, body: String) -> Response {
    let content_type = headers.get(header::CONTENT_TYPE).and_then(|value| value.to_str().ok());
    if let Ok(request) = parse_token_body(content_type, &body)
        && let Some(token) = request.token.filter(|token| !token.is_empty())
    {
        state.auth.revoke(&token);
    }
    StatusCode::OK.into_response()
}

pub async fn start_strategy(Extension(state): Extension<Arc<WebState>>) -> Response {
    let Some(oidc) = state.auth.oidc.clone() else {
        return api_error(StatusCode::NOT_FOUND, "not_supported", "oidc is not configured");
    };
    let discovery = match state.auth.discovery(&oidc).await {
        Ok(discovery) => discovery,
        Err(err) => {
            return api_error(StatusCode::BAD_GATEWAY, "not_available", &format!("oidc discovery failed: {err}"));
        }
    };
    let state_token = state.auth.issue_state();
    let url = authorize_url(&discovery.authorization_endpoint, &oidc.client_id, &oidc.redirect_url, &state_token);
    found(&url)
}

#[derive(Deserialize)]
pub(crate) struct CallbackQuery {
    code: Option<String>,
    state: Option<String>,
}

pub(crate) async fn strategy_callback(
    Extension(state): Extension<Arc<WebState>>,
    Query(query): Query<CallbackQuery>,
) -> Response {
    let root = state.red_settings.http_admin_root.clone();
    let Some(oidc) = state.auth.oidc.clone() else {
        return api_error(StatusCode::NOT_FOUND, "not_supported", "oidc is not configured");
    };
    let Some(_nonce) = query.state.filter(|value| state.auth.take_state(value)) else {
        return found(&redirect_with(&root, "session_message=sign-in%20state%20was%20not%20recognised"));
    };
    let Some(code) = query.code.filter(|code| !code.is_empty()) else {
        return found(&redirect_with(&root, "session_message=the%20identity%20provider%20returned%20no%20code"));
    };
    let discovery = match state.auth.discovery(&oidc).await {
        Ok(discovery) => discovery,
        Err(_) => return found(&redirect_with(&root, "session_message=oidc%20discovery%20failed")),
    };
    let token = match state.auth.exchange_code(&oidc, &discovery, &code).await {
        Ok(token) => token,
        Err(_) => return found(&redirect_with(&root, "session_message=oidc%20token%20exchange%20failed")),
    };
    let Some(access) = token.get("access_token").and_then(Value::as_str).filter(|value| !value.is_empty()) else {
        return found(&redirect_with(
            &root,
            "session_message=oidc%20token%20exchange%20returned%20no%20access%20token",
        ));
    };
    let info = match state.auth.userinfo(&discovery.userinfo_endpoint, access).await {
        Ok(info) => info,
        Err(_) => return found(&redirect_with(&root, "session_message=oidc%20user%20info%20failed")),
    };
    let (username, permissions) = actor_from_userinfo(&oidc, &info);
    let exchange = state.auth.issue_exchange(&username, &permissions);
    let _ = state.audit.record(&username, "auth.login", None).await;
    found(&redirect_with(&root, &format!("code={exchange}")))
}

pub async fn require_admin(request: axum::extract::Request, next: axum::middleware::Next) -> Response {
    let state = request.extensions().get::<Arc<WebState>>().cloned();
    let Some(state) = state else {
        return api_error(StatusCode::INTERNAL_SERVER_ERROR, "unauthorized", "admin state is missing");
    };
    if state.auth.enabled()
        && let Some(permission) = permission_for(request.method(), request.uri().path())
    {
        match state.auth.authorize(request.headers(), permission) {
            Access::Allowed => {}
            Access::Missing => {
                return api_error(StatusCode::UNAUTHORIZED, "unauthorized", "credentials required");
            }
            Access::Denied => {
                return api_error(StatusCode::UNAUTHORIZED, "unauthorized", "permission denied");
            }
        }
    }
    next.run(request).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::create_all_routes;
    use crate::handlers::WebState;
    use crate::models::RedSystemSettings;
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    fn cfg(text: &str) -> config::Config {
        config::Config::builder().add_source(config::File::from_str(text, config::FileFormat::Toml)).build().unwrap()
    }

    fn router(auth: AdminAuth) -> (axum::Router, Arc<WebState>) {
        let settings = RedSystemSettings { http_admin_root: "/".to_string(), ..Default::default() };
        let state = WebState::assemble(
            Arc::new(settings),
            std::env::temp_dir(),
            None,
            auth,
            crate::handlers::fleet::Fleet::disabled(),
        );
        let router = create_all_routes(&state).layer(Extension(state.clone()));
        (router, state)
    }

    async fn call(
        router: &axum::Router,
        method: &str,
        uri: &str,
        body: Option<&str>,
        token: Option<&str>,
        content_type: &str,
    ) -> (StatusCode, Value, HeaderMap) {
        let mut builder = Request::builder().method(method).uri(uri).header(header::CONTENT_TYPE, content_type);
        if let Some(token) = token {
            builder = builder.header(header::AUTHORIZATION, format!("Bearer {token}"));
        }
        let response =
            router.clone().oneshot(builder.body(Body::from(body.unwrap_or("").to_string())).unwrap()).await.unwrap();
        let status = response.status();
        let headers = response.headers().clone();
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let parsed = if bytes.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&bytes).unwrap_or(Value::String(String::from_utf8_lossy(&bytes).into_owned()))
        };
        (status, parsed, headers)
    }

    #[test]
    fn read_scope_allows_only_read_permissions() {
        assert!(allows("read", "flows.read"));
        assert!(allows("read", "status.read"));
        assert!(!allows("read", "flows.write"));
        assert!(allows("*", "flows.write"));
        assert!(allows("*.read", "context.read"));
        assert!(!allows("flows.read", "nodes.read"));
        assert_eq!(claim_permissions(Some("deployer")), "*");
        assert_eq!(claim_permissions(Some("viewer")), "read");
        assert_eq!(claim_permissions(None), "read");
    }

    #[test]
    fn an_unknown_role_is_rejected() {
        let Err(message) = AdminAuth::from_config(&cfg(r#"
            [[admin.users]]
            username = "ada"
            password = "secret"
            role = "owner"
            "#))
        else {
            panic!("expected an error");
        };
        assert!(message.contains("not supported"));
    }

    #[test]
    fn users_replace_the_shorthand_password_for_the_same_name() {
        let auth = AdminAuth::from_config(&cfg(r#"
            [admin]
            password = "shorthand-secret"
            [[admin.users]]
            username = "admin"
            password = "listed-secret"
            role = "viewer"
            "#))
        .unwrap();
        assert!(auth.login("admin", "shorthand-secret").is_err());
        assert!(auth.login("admin", "listed-secret").is_ok());
        assert_eq!(auth.users.get("admin").unwrap().permissions, "read");
    }

    #[test]
    fn five_failures_lock_the_account() {
        let auth = AdminAuth::from_config(&cfg(r#"
            [admin]
            password = "plant-secret"
            "#))
        .unwrap();
        for _ in 0..5 {
            assert!(auth.login("admin", "wrong").is_err());
        }
        assert!(matches!(auth.login("admin", "plant-secret"), Err(LoginFail::Locked)));
    }

    #[tokio::test]
    async fn open_install_does_not_ask_for_a_token() {
        let (router, _) = router(AdminAuth::open());
        let (status, body, _) = call(&router, "GET", "/settings", None, None, "application/json").await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.get("user").is_none());
    }

    #[tokio::test]
    async fn viewer_cannot_deploy_and_deployer_can() {
        let auth = AdminAuth::from_config(&cfg(r#"
            [[admin.users]]
            username = "viewer"
            password = "view-secret"
            role = "viewer"
            [[admin.users]]
            username = "deployer"
            password = "deploy-secret"
            role = "deployer"
            "#))
        .unwrap();
        let (router, state) = router(auth);
        let dir = std::env::temp_dir().join(format!("edgelinkd-auth-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let flows = dir.join("flows.json");
        std::fs::write(&flows, b"[]").unwrap();
        state.set_flows_file_path(flows).await;

        let (status, body, _) = call(&router, "GET", "/settings", None, None, "application/json").await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(body["message"], "credentials required");

        let (status, _, _) = call(
            &router,
            "POST",
            "/auth/token",
            Some("client_id=node-red-editor&grant_type=password&scope=&username=viewer&password=wrong"),
            None,
            "application/x-www-form-urlencoded",
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);

        let (status, body, _) = call(
            &router,
            "POST",
            "/auth/token",
            Some("client_id=node-red-editor&grant_type=password&scope=&username=viewer&password=view-secret"),
            None,
            "application/x-www-form-urlencoded",
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let viewer = body["access_token"].as_str().unwrap().to_string();
        let (status, body, _) = call(&router, "GET", "/settings", None, Some(&viewer), "application/json").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["user"]["permissions"], "read");
        assert_eq!(body["editorTheme"]["userMenu"], true);

        let (status, body, _) =
            call(&router, "POST", "/flows", Some(r#"{"flows":[]}"#), Some(&viewer), "application/json").await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(body["message"], "permission denied");

        let (_status, body, _) = call(
            &router,
            "POST",
            "/auth/token",
            Some("client_id=node-red-admin&grant_type=password&username=deployer&password=deploy-secret"),
            None,
            "application/x-www-form-urlencoded",
        )
        .await;
        let deployer = body["access_token"].as_str().unwrap().to_string();
        let (status, body, _) = call(
            &router,
            "POST",
            "/flows",
            Some(r#"{"flows":[{"id":"1","type":"tab"}]}"#),
            Some(&deployer),
            "application/json",
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let rev = body["rev"].as_str().unwrap().to_string();
        let log = std::fs::read_to_string(dir.join("audit.log")).unwrap();
        assert!(log.contains("deployer"));
        assert!(log.contains(&rev));
        assert!(log.contains("flows.deploy"));
        assert!(!log.contains("deploy-secret"));
        assert!(!log.contains("view-secret"));
        let _ = std::fs::remove_dir_all(&dir);
        let _ = status;
    }

    #[test]
    fn authorize_url_carries_the_code_request() {
        let url = authorize_url(
            "http://idp.example/auth",
            "edgelinkd",
            "http://127.0.0.1:1888/auth/strategy/callback",
            "abc",
        );
        assert!(url.contains("response_type=code"));
        assert!(url.contains("client_id=edgelinkd"));
        assert!(url.contains("scope=openid"));
        assert!(url.contains("state=abc"));
        assert!(url.contains("redirect_uri=http%3A%2F%2F127.0.0.1%3A1888%2Fauth%2Fstrategy%2Fcallback"));
    }

    #[tokio::test]
    async fn oidc_code_exchange_uses_the_role_claim() {
        let idp = axum::Router::new()
            .route("/.well-known/openid-configuration", axum::routing::get(discovery_doc))
            .route("/token", axum::routing::post(idp_token))
            .route("/userinfo", axum::routing::get(idp_userinfo));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, idp).await;
        });
        let issuer = format!("http://{address}");
        let text = format!(
            r#"
            [admin.oidc]
            issuer = "{issuer}"
            client_id = "edgelinkd"
            client_secret = "idp-secret"
            role_claim = "edgelink_role"
            redirect_url = "http://127.0.0.1:9/auth/strategy/callback"
            "#
        );
        let auth = AdminAuth::from_config(&cfg(&text)).unwrap();
        assert_eq!(auth.login_kind(), "strategy");
        let (router, _) = router(auth);
        let (status, _, headers) = call(&router, "GET", "/auth/strategy", None, None, "application/json").await;
        assert_eq!(status, StatusCode::FOUND);
        let location = headers.get(header::LOCATION).unwrap().to_str().unwrap();
        let state_token = location.split("state=").nth(1).unwrap().split('&').next().unwrap();
        let (status, _, headers) = call(
            &router,
            "GET",
            &format!("/auth/strategy/callback?code=from-idp&state={state_token}"),
            None,
            None,
            "application/json",
        )
        .await;
        assert_eq!(status, StatusCode::FOUND);
        let location = headers.get(header::LOCATION).unwrap().to_str().unwrap();
        let exchange = location.split("code=").nth(1).unwrap();
        let (status, body, _) = call(
            &router,
            "POST",
            "/auth/token",
            Some(&format!("code={exchange}")),
            None,
            "application/x-www-form-urlencoded",
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert!(body["accessToken"].as_str().is_some());
        let token = body["accessToken"].as_str().unwrap();
        let (status, body, _) = call(&router, "GET", "/settings", None, Some(token), "application/json").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["user"]["username"], "ada");
        assert_eq!(body["user"]["permissions"], "*");
    }

    async fn discovery_doc(request: axum::extract::Request) -> Json<Value> {
        let host = request.headers().get(header::HOST).and_then(|value| value.to_str().ok()).unwrap_or("127.0.0.1");
        Json(json!({
            "authorization_endpoint": format!("http://{host}/authorize"),
            "token_endpoint": format!("http://{host}/token"),
            "userinfo_endpoint": format!("http://{host}/userinfo"),
        }))
    }

    async fn idp_token() -> Json<Value> {
        Json(json!({ "access_token": "idp-access" }))
    }

    async fn idp_userinfo() -> Json<Value> {
        Json(json!({ "preferred_username": "ada", "edgelink_role": "deployer" }))
    }
}
