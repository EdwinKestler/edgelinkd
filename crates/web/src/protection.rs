//! Endpoint-class resource protection for the editor and administrative HTTP server.

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::body::{Body, to_bytes};
use axum::extract::{ConnectInfo, Request};
use axum::http::{HeaderMap, Method, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::{Extension, Json};
use edgelink_core::runtime::ingress::{
    EndpointClass, EndpointLimits, IngressProtectionConfig, ProtectionMode, TrustedProxySet,
};
use serde_json::json;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use crate::handlers::WebState;

#[derive(Debug, Clone, Hash, PartialEq, Eq)]
struct RateKey {
    class: EndpointClass,
    kind: RateKeyKind,
    value: String,
}

#[derive(Debug, Clone, Copy, Hash, PartialEq, Eq)]
enum RateKeyKind {
    Principal,
    Address,
}

#[derive(Debug, Clone)]
struct RateWindow {
    started: Instant,
    count: u32,
}

#[derive(Clone)]
pub struct ApiProtection {
    config: Arc<IngressProtectionConfig>,
    trusted_proxies: TrustedProxySet,
    global: Arc<Semaphore>,
    classes: Arc<HashMap<EndpointClass, Arc<Semaphore>>>,
    rate: Arc<Mutex<HashMap<RateKey, RateWindow>>>,
}

impl Default for ApiProtection {
    fn default() -> Self {
        Self::new(IngressProtectionConfig::default()).expect("default ingress limits must be valid")
    }
}

impl ApiProtection {
    pub fn load(cfg: Option<&config::Config>) -> Result<Self, String> {
        Self::new(IngressProtectionConfig::load(cfg)?)
    }

    pub fn new(config: IngressProtectionConfig) -> Result<Self, String> {
        config.validate()?;
        let trusted_proxies = TrustedProxySet::new(&config.trusted_proxies)?;
        let classes = [
            EndpointClass::Health,
            EndpointClass::EditorAdmin,
            EndpointClass::Authentication,
            EndpointClass::Websocket,
            EndpointClass::Webhook,
            EndpointClass::Copilot,
            EndpointClass::Fleet,
            EndpointClass::Static,
        ]
        .into_iter()
        .map(|class| (class, Arc::new(Semaphore::new(config.limits(class).max_concurrency))))
        .collect();
        Ok(Self {
            global: Arc::new(Semaphore::new(config.global_max_concurrency)),
            config: Arc::new(config),
            trusted_proxies,
            classes: Arc::new(classes),
            rate: Arc::new(Mutex::new(HashMap::new())),
        })
    }

    pub fn limits(&self, class: EndpointClass) -> &EndpointLimits {
        self.config.limits(class)
    }

    pub async fn acquire_websocket(&self) -> Result<Option<OwnedSemaphorePermit>, Box<Response>> {
        let limits = self.limits(EndpointClass::Websocket);
        if limits.mode != ProtectionMode::Enforce {
            return Ok(None);
        }
        self.acquire_class(EndpointClass::Websocket, limits).await.map(Some)
    }

    async fn acquire_class(
        &self,
        class: EndpointClass,
        limits: &EndpointLimits,
    ) -> Result<OwnedSemaphorePermit, Box<Response>> {
        let semaphore = Arc::clone(self.classes.get(&class).expect("every class has a semaphore"));
        match tokio::time::timeout(limits.queue_timeout(), semaphore.acquire_owned()).await {
            Ok(Ok(permit)) => Ok(permit),
            Ok(Err(_)) => {
                Err(Box::new(rejection(StatusCode::SERVICE_UNAVAILABLE, "shutting_down", "service is shutting down")))
            }
            Err(_) => Err(Box::new(rejection(StatusCode::TOO_MANY_REQUESTS, "busy", "endpoint is busy"))),
        }
    }

    fn client_address(&self, headers: &HeaderMap, peer: Option<SocketAddr>) -> Result<Option<IpAddr>, Box<Response>> {
        let Some(peer) = peer else {
            return Ok(None);
        };
        if !self.trusted_proxies.contains(peer.ip()) {
            return Ok(Some(peer.ip()));
        }
        if let Some(value) = headers.get("forwarded") {
            let value = value.to_str().map_err(|_| {
                Box::new(rejection(StatusCode::BAD_REQUEST, "invalid_forwarded", "invalid forwarded address"))
            })?;
            let first = value.split(',').next().unwrap_or(value);
            let address = first
                .split(';')
                .find_map(|part| part.trim().strip_prefix("for="))
                .map(|value| value.trim_matches('"').trim_matches(['[', ']']))
                .ok_or_else(|| {
                    Box::new(rejection(StatusCode::BAD_REQUEST, "invalid_forwarded", "invalid forwarded address"))
                })?;
            return forwarded_ip(address).map(Some).ok_or_else(|| {
                Box::new(rejection(StatusCode::BAD_REQUEST, "invalid_forwarded", "invalid forwarded address"))
            });
        }
        if let Some(value) = headers.get("x-forwarded-for") {
            let value = value.to_str().map_err(|_| {
                Box::new(rejection(StatusCode::BAD_REQUEST, "invalid_forwarded", "invalid forwarded address"))
            })?;
            return forwarded_ip(value.split(',').next().unwrap_or(value).trim()).map(Some).ok_or_else(|| {
                Box::new(rejection(StatusCode::BAD_REQUEST, "invalid_forwarded", "invalid forwarded address"))
            });
        }
        Ok(Some(peer.ip()))
    }

    fn check_rate(&self, class: EndpointClass, principal: &str, address: Option<IpAddr>) -> bool {
        let limit = self.limits(class).requests_per_minute;
        let now = Instant::now();
        let mut map = self.rate.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        map.retain(|_, window| now.duration_since(window.started) < Duration::from_secs(60));
        let mut keys = Vec::with_capacity(2);
        if principal != "anonymous" && !principal.is_empty() {
            keys.push(RateKey { class, kind: RateKeyKind::Principal, value: principal.to_string() });
        }
        if let Some(address) = address {
            keys.push(RateKey { class, kind: RateKeyKind::Address, value: address.to_string() });
        }
        if keys.iter().any(|key| map.get(key).is_some_and(|window| window.count >= limit)) {
            return false;
        }
        for key in keys {
            if !map.contains_key(&key) && map.len() >= self.config.max_rate_keys {
                return false;
            }
            let window = map.entry(key).or_insert(RateWindow { started: now, count: 0 });
            window.count += 1;
        }
        true
    }
}

fn forwarded_ip(value: &str) -> Option<IpAddr> {
    let value = value.trim().trim_matches('"');
    value.parse::<IpAddr>().ok().or_else(|| value.parse::<SocketAddr>().ok().map(|address| address.ip())).or_else(
        || {
            let close = value.find(']')?;
            value.get(1..close)?.parse::<IpAddr>().ok()
        },
    )
}

pub fn classify(path: &str) -> EndpointClass {
    if matches!(path, "/api/health" | "/api/info" | "/health" | "/info") {
        EndpointClass::Health
    } else if path.starts_with("/auth/") {
        EndpointClass::Authentication
    } else if path == "/comms" {
        EndpointClass::Websocket
    } else if path.starts_with("/assistant/") {
        EndpointClass::Copilot
    } else if path.starts_with("/fleet/") {
        EndpointClass::Fleet
    } else if [
        "/flows",
        "/flow",
        "/credentials",
        "/nodes",
        "/library",
        "/plugins",
        "/settings",
        "/status",
        "/audit",
        "/history",
        "/runtime",
        "/context",
        "/inject",
    ]
    .iter()
    .any(|root| path == *root || path.strip_prefix(root).is_some_and(|suffix| suffix.starts_with('/')))
        || (path.starts_with("/debug/") && !path.starts_with("/debug/view/"))
    {
        EndpointClass::EditorAdmin
    } else if path.starts_with("/icons")
        || path.starts_with("/locales")
        || path.starts_with("/core/")
        || path == "/theme"
        || path == "/debug.js"
        || path == "/debug-utils.js"
        || path.starts_with("/debug/view/")
        || path == "/"
        || path.rsplit('/').next().is_some_and(|name| name.contains('.'))
    {
        EndpointClass::Static
    } else {
        EndpointClass::EditorAdmin
    }
}

#[derive(Clone, Copy)]
struct ProtectionApplied;

pub async fn protect_static_request(
    Extension(state): Extension<Arc<WebState>>,
    mut request: Request,
    next: Next,
) -> Response {
    if classify(request.uri().path()) != EndpointClass::Static {
        return next.run(request).await;
    }
    request.extensions_mut().insert(ProtectionApplied);
    protect_impl(state, request, next).await
}

pub async fn protect_request(Extension(state): Extension<Arc<WebState>>, request: Request, next: Next) -> Response {
    if request.extensions().get::<ProtectionApplied>().is_some() {
        return next.run(request).await;
    }
    protect_impl(state, request, next).await
}

async fn protect_impl(state: Arc<WebState>, request: Request, next: Next) -> Response {
    let class = classify(request.uri().path());
    let limits = state.protection.limits(class).clone();
    if limits.mode == ProtectionMode::Off {
        return next.run(request).await;
    }

    let header_bytes = request
        .headers()
        .iter()
        .map(|(name, value)| name.as_str().len().saturating_add(value.as_bytes().len()).saturating_add(4))
        .sum::<usize>();
    if request.headers().len() > limits.max_headers || header_bytes > limits.max_header_bytes {
        return enforce_or_continue(
            limits.mode,
            class,
            "headers_too_large",
            request,
            next,
            StatusCode::REQUEST_HEADER_FIELDS_TOO_LARGE,
        )
        .await;
    }

    let peer = request.extensions().get::<ConnectInfo<SocketAddr>>().map(|ConnectInfo(peer)| *peer);
    let address = match state.protection.client_address(request.headers(), peer) {
        Ok(address) => address,
        Err(response) if limits.mode == ProtectionMode::Enforce => return *response,
        Err(_) => peer.map(|value| value.ip()),
    };
    let actor = state.auth.actor_from_headers(request.headers());
    if !state.protection.check_rate(class, &actor.username, address) {
        if limits.mode == ProtectionMode::Enforce {
            log_rejection(class, "rate_limited");
            return rejection(StatusCode::TOO_MANY_REQUESTS, "rate_limited", "request rate exceeded");
        }
        log_observation(class, "rate_limited");
    }

    let content_length = match request.headers().get(header::CONTENT_LENGTH) {
        Some(value) => match value.to_str().ok().and_then(|value| value.parse::<usize>().ok()) {
            Some(length) => Some(length),
            None if limits.mode == ProtectionMode::Enforce => {
                log_rejection(class, "invalid_content_length");
                return rejection(StatusCode::BAD_REQUEST, "invalid_content_length", "invalid content length");
            }
            None => None,
        },
        None => None,
    };
    if content_length.is_some_and(|length| length > limits.max_body_bytes) {
        return enforce_or_continue(limits.mode, class, "body_too_large", request, next, StatusCode::PAYLOAD_TOO_LARGE)
            .await;
    }

    let body_declared =
        content_length.is_some_and(|length| length > 0) || request.headers().contains_key(header::TRANSFER_ENCODING);
    if has_body(request.method()) && body_declared && !content_type_supported(class, request.headers()) {
        return enforce_or_continue(
            limits.mode,
            class,
            "unsupported_media_type",
            request,
            next,
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
        )
        .await;
    }

    // Acquire bounded work capacity before reading a potentially slow chunked body. Authentication
    // is an outer middleware, so unauthorized requests never occupy either permit.
    let _global = if limits.mode == ProtectionMode::Enforce {
        match tokio::time::timeout(limits.queue_timeout(), Arc::clone(&state.protection.global).acquire_owned()).await {
            Ok(Ok(permit)) => Some(permit),
            Ok(Err(_)) => {
                return rejection(StatusCode::SERVICE_UNAVAILABLE, "shutting_down", "service is shutting down");
            }
            Err(_) => return rejection(StatusCode::TOO_MANY_REQUESTS, "busy", "service is busy"),
        }
    } else {
        None
    };
    let _class = if limits.mode == ProtectionMode::Enforce && class != EndpointClass::Websocket {
        match state.protection.acquire_class(class, &limits).await {
            Ok(permit) => Some(permit),
            Err(response) => return *response,
        }
    } else {
        None
    };
    let deadline = tokio::time::Instant::now() + limits.request_timeout();

    let request = if limits.mode == ProtectionMode::Enforce && has_body(request.method()) {
        let (parts, body) = request.into_parts();
        let body = match tokio::time::timeout_at(deadline, to_bytes(body, limits.max_body_bytes)).await {
            Ok(Ok(body)) => body,
            Ok(Err(_)) => {
                log_rejection(class, "body_too_large");
                return rejection(StatusCode::PAYLOAD_TOO_LARGE, "body_too_large", "request body is too large");
            }
            Err(_) => {
                log_rejection(class, "body_timeout");
                return rejection(StatusCode::REQUEST_TIMEOUT, "body_timeout", "request body timed out");
            }
        };
        Request::from_parts(parts, Body::from(body))
    } else {
        request
    };

    let started = Instant::now();
    let response = if limits.mode == ProtectionMode::Enforce && class != EndpointClass::Websocket {
        let cancel = state.cancel_token.read().await.clone();
        tokio::select! {
            _ = async {
                if let Some(cancel) = cancel { cancel.cancelled().await } else { std::future::pending::<()>().await }
            } => rejection(StatusCode::SERVICE_UNAVAILABLE, "shutting_down", "service is shutting down"),
            result = tokio::time::timeout_at(deadline, next.run(request)) => match result {
                Ok(response) => response,
                Err(_) => {
                    log_rejection(class, "request_timeout");
                    rejection(StatusCode::GATEWAY_TIMEOUT, "request_timeout", "request timed out")
                }
            },
        }
    } else {
        next.run(request).await
    };
    if limits.mode == ProtectionMode::Observe && started.elapsed() > limits.request_timeout() {
        log_observation(class, "request_timeout");
    }
    bound_response(response, class, &limits, deadline).await
}

async fn bound_response(
    response: Response,
    class: EndpointClass,
    limits: &EndpointLimits,
    deadline: tokio::time::Instant,
) -> Response {
    if limits.mode != ProtectionMode::Enforce || class == EndpointClass::Websocket {
        return response;
    }
    if class == EndpointClass::Static {
        let too_large = response
            .headers()
            .get(header::CONTENT_LENGTH)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.parse::<usize>().ok())
            .is_some_and(|length| length > limits.max_response_bytes);
        if too_large {
            log_rejection(class, "response_too_large");
            return rejection(StatusCode::BAD_GATEWAY, "response_too_large", "response body is too large");
        }
        return response;
    }
    let (parts, body) = response.into_parts();
    match tokio::time::timeout_at(deadline, to_bytes(body, limits.max_response_bytes)).await {
        Ok(Ok(body)) => Response::from_parts(parts, Body::from(body)),
        Ok(Err(_)) => {
            log_rejection(class, "response_too_large");
            rejection(StatusCode::BAD_GATEWAY, "response_too_large", "response body is too large")
        }
        Err(_) => {
            log_rejection(class, "response_timeout");
            rejection(StatusCode::GATEWAY_TIMEOUT, "response_timeout", "response body timed out")
        }
    }
}

fn has_body(method: &Method) -> bool {
    matches!(*method, Method::POST | Method::PUT | Method::PATCH)
}

fn content_type_supported(class: EndpointClass, headers: &HeaderMap) -> bool {
    let Some(value) = headers.get(header::CONTENT_TYPE) else {
        return false;
    };
    let Ok(value) = value.to_str() else {
        return false;
    };
    let media = value.split(';').next().unwrap_or(value).trim().to_ascii_lowercase();
    if class == EndpointClass::Authentication {
        matches!(media.as_str(), "application/json" | "application/x-www-form-urlencoded")
    } else {
        matches!(media.as_str(), "application/json" | "text/plain" | "application/octet-stream")
    }
}

async fn enforce_or_continue(
    mode: ProtectionMode,
    class: EndpointClass,
    reason: &'static str,
    request: Request,
    next: Next,
    status: StatusCode,
) -> Response {
    if mode == ProtectionMode::Enforce {
        log_rejection(class, reason);
        rejection(status, reason, reason.replace('_', " ").as_str())
    } else {
        log_observation(class, reason);
        next.run(request).await
    }
}

fn log_rejection(class: EndpointClass, reason: &str) {
    log::warn!("ingress decision mode=enforce action=reject class={} reason={reason}", class.as_str());
}

fn log_observation(class: EndpointClass, reason: &str) {
    log::info!("ingress decision mode=observe action=permit class={} reason={reason}", class.as_str());
}

fn rejection(status: StatusCode, code: &str, message: &str) -> Response {
    (status, Json(json!({ "code": code, "message": message }))).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::Router;
    use axum::http::Request as HttpRequest;
    use axum::routing::post;
    use edgelink_core::runtime::credential_storage::CredentialStore;
    use edgelink_core::runtime::egress::EgressPolicyHandle;
    use std::sync::atomic::{AtomicBool, Ordering};
    use tower::ServiceExt;

    use crate::handlers::auth::AdminAuth;
    use crate::handlers::fleet::Fleet;
    use crate::handlers::web_state::WebRuntimeServices;
    use crate::models::RedSystemSettings;

    fn state_with(config: &str) -> Arc<WebState> {
        let cfg = config::Config::builder()
            .add_source(config::File::from_str(config, config::FileFormat::Toml))
            .build()
            .unwrap();
        let state = WebState::new();
        let protection = ApiProtection::load(Some(&cfg)).unwrap();
        Arc::new(WebState { protection, ..Arc::try_unwrap(state).ok().unwrap() })
    }

    fn app(state: Arc<WebState>) -> Router {
        Router::new()
            .route("/flows", post(|| async { "ok" }))
            .layer(axum::middleware::from_fn(protect_request))
            .layer(Extension(state))
    }

    #[tokio::test]
    async fn oversized_fixed_and_chunked_bodies_are_rejected() {
        let state = state_with("[api_protection.editor_admin]\nmax_body_bytes = 4");
        for request in [
            HttpRequest::post("/flows")
                .header("content-type", "application/json")
                .header("content-length", "5")
                .body(Body::from("12345"))
                .unwrap(),
            HttpRequest::post("/flows").header("content-type", "application/json").body(Body::from("12345")).unwrap(),
        ] {
            assert_eq!(app(Arc::clone(&state)).oneshot(request).await.unwrap().status(), StatusCode::PAYLOAD_TOO_LARGE);
        }
    }

    #[tokio::test]
    async fn unsupported_media_type_is_rejected() {
        let response = app(WebState::new())
            .oneshot(
                HttpRequest::post("/flows")
                    .header("content-type", "image/png")
                    .header("content-length", "1")
                    .body(Body::from("x"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNSUPPORTED_MEDIA_TYPE);
    }

    #[tokio::test]
    async fn slow_chunked_bodies_and_large_responses_are_bounded() {
        let mut config = IngressProtectionConfig::default();
        config.editor_admin.request_timeout_ms = 5;
        config.editor_admin.max_response_bytes = 4;
        let state = state_with_protection(ApiProtection::new(config.clone()).unwrap(), AdminAuth::open());
        let router = Router::new()
            .route("/flows", post(|| async { "12345" }))
            .layer(axum::middleware::from_fn(protect_request))
            .layer(Extension(state));
        let stream = futures_util::stream::once(async {
            tokio::time::sleep(Duration::from_millis(25)).await;
            Ok::<_, std::io::Error>(axum::body::Bytes::from_static(b"{}"))
        });
        let slow = router
            .clone()
            .oneshot(
                HttpRequest::post("/flows")
                    .header("content-type", "application/json")
                    .header("transfer-encoding", "chunked")
                    .body(Body::from_stream(stream))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(slow.status(), StatusCode::REQUEST_TIMEOUT);

        let large = router.oneshot(HttpRequest::post("/flows").body(Body::empty()).unwrap()).await.unwrap();
        assert_eq!(large.status(), StatusCode::BAD_GATEWAY);

        let slow_response = Router::new()
            .route(
                "/flow",
                post(|| async {
                    let delayed = futures_util::stream::once(async {
                        tokio::time::sleep(Duration::from_millis(25)).await;
                        Ok::<_, std::io::Error>(axum::body::Bytes::from_static(b"ok"))
                    });
                    Response::new(Body::from_stream(delayed))
                }),
            )
            .layer(axum::middleware::from_fn(protect_request))
            .layer(Extension(state_with_protection(ApiProtection::new(config).unwrap(), AdminAuth::open())))
            .oneshot(HttpRequest::post("/flow").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(slow_response.status(), StatusCode::GATEWAY_TIMEOUT);
    }

    #[tokio::test]
    async fn excessive_headers_are_rejected() {
        let mut config = IngressProtectionConfig::default();
        config.editor_admin.max_headers = 1;
        let state = state_with_protection(ApiProtection::new(config).unwrap(), AdminAuth::open());
        let response = app(state)
            .oneshot(HttpRequest::post("/flows").header("x-one", "1").header("x-two", "2").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::REQUEST_HEADER_FIELDS_TOO_LARGE);
    }

    #[test]
    fn rate_limits_are_independent_by_principal_and_address() {
        let mut config = IngressProtectionConfig::default();
        config.health.requests_per_minute = 2;
        let protection = ApiProtection::new(config).unwrap();
        let address = "192.0.2.4".parse().unwrap();
        assert!(protection.check_rate(EndpointClass::Health, "alice", Some(address)));
        assert!(protection.check_rate(EndpointClass::Health, "alice", Some(address)));
        assert!(!protection.check_rate(EndpointClass::Health, "alice", Some(address)));
        assert!(!protection.check_rate(EndpointClass::Health, "bob", Some(address)));
        assert!(!protection.check_rate(EndpointClass::Health, "alice", Some("192.0.2.5".parse().unwrap())));
        assert!(protection.check_rate(EndpointClass::Health, "bob", Some("192.0.2.5".parse().unwrap())));
    }

    #[test]
    fn rate_state_refuses_unbounded_new_keys() {
        let config = IngressProtectionConfig { max_rate_keys: 1, ..IngressProtectionConfig::default() };
        let protection = ApiProtection::new(config).unwrap();
        assert!(protection.check_rate(EndpointClass::Health, "anonymous", Some("192.0.2.1".parse().unwrap())));
        assert!(!protection.check_rate(EndpointClass::Health, "anonymous", Some("192.0.2.2".parse().unwrap())));
        assert_eq!(protection.rate.lock().unwrap().len(), 1);
    }

    #[test]
    fn forwarded_addresses_are_used_only_for_explicit_trusted_proxies() {
        let mut headers = HeaderMap::new();
        headers.insert("x-forwarded-for", "198.51.100.9".parse().unwrap());
        let peer = "127.0.0.1:1234".parse().unwrap();

        let untrusted = ApiProtection::default();
        assert_eq!(untrusted.client_address(&headers, Some(peer)).unwrap(), Some(peer.ip()));

        let config = IngressProtectionConfig {
            trusted_proxies: vec!["127.0.0.1".to_string()],
            ..IngressProtectionConfig::default()
        };
        let trusted = ApiProtection::new(config).unwrap();
        assert_eq!(trusted.client_address(&headers, Some(peer)).unwrap(), Some("198.51.100.9".parse().unwrap()));
    }

    #[tokio::test]
    async fn saturated_class_recovers_after_the_permit_is_released() {
        let mut config = IngressProtectionConfig::default();
        config.copilot.max_concurrency = 1;
        config.copilot.queue_timeout_ms = 5;
        let protection = ApiProtection::new(config).unwrap();
        let limits = protection.limits(EndpointClass::Copilot).clone();
        let first = protection.acquire_class(EndpointClass::Copilot, &limits).await.unwrap();
        assert!(protection.acquire_class(EndpointClass::Copilot, &limits).await.is_err());
        drop(first);
        assert!(protection.acquire_class(EndpointClass::Copilot, &limits).await.is_ok());
    }

    #[tokio::test]
    async fn a_saturated_class_does_not_block_another_class() {
        let mut config = IngressProtectionConfig::default();
        config.copilot.max_concurrency = 1;
        config.copilot.queue_timeout_ms = 5;
        let protection = ApiProtection::new(config).unwrap();
        let copilot = protection.limits(EndpointClass::Copilot).clone();
        let health = protection.limits(EndpointClass::Health).clone();
        let _copilot_permit = protection.acquire_class(EndpointClass::Copilot, &copilot).await.unwrap();
        assert!(protection.acquire_class(EndpointClass::Copilot, &copilot).await.is_err());
        assert!(protection.acquire_class(EndpointClass::Health, &health).await.is_ok());
    }

    #[tokio::test]
    async fn saturated_global_capacity_recovers_after_release() {
        let config = IngressProtectionConfig { global_max_concurrency: 1, ..IngressProtectionConfig::default() };
        let protection = ApiProtection::new(config).unwrap();
        let first = Arc::clone(&protection.global).acquire_owned().await.unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(5), Arc::clone(&protection.global).acquire_owned())
                .await
                .is_err()
        );
        drop(first);
        assert!(Arc::clone(&protection.global).try_acquire_owned().is_ok());
    }

    struct CancelProbe(Arc<AtomicBool>);

    impl Drop for CancelProbe {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }

    #[tokio::test]
    async fn timed_out_work_is_cancelled_and_capacity_recovers() {
        let mut config = IngressProtectionConfig::default();
        config.editor_admin.request_timeout_ms = 5;
        config.editor_admin.max_concurrency = 1;
        let state = state_with_protection(ApiProtection::new(config).unwrap(), AdminAuth::open());
        let cancelled = Arc::new(AtomicBool::new(false));
        let observed = Arc::clone(&cancelled);
        let router = Router::new()
            .route(
                "/flows",
                post(move || {
                    let observed = Arc::clone(&observed);
                    async move {
                        let _probe = CancelProbe(observed);
                        tokio::time::sleep(Duration::from_secs(1)).await;
                        "late"
                    }
                }),
            )
            .layer(axum::middleware::from_fn(protect_request))
            .layer(Extension(Arc::clone(&state)));
        let response = router.clone().oneshot(HttpRequest::post("/flows").body(Body::empty()).unwrap()).await.unwrap();
        assert_eq!(response.status(), StatusCode::GATEWAY_TIMEOUT);
        assert!(cancelled.load(Ordering::SeqCst));
        let response = router.oneshot(HttpRequest::post("/flows").body(Body::empty()).unwrap()).await.unwrap();
        assert_eq!(response.status(), StatusCode::GATEWAY_TIMEOUT);
    }

    #[tokio::test]
    async fn authentication_runs_before_an_oversized_body_is_read() {
        let cfg = config::Config::builder()
            .add_source(config::File::from_str(
                "[admin]\npassword = \"test-only-password\"\n[api_protection.editor_admin]\nmax_body_bytes = 1",
                config::FileFormat::Toml,
            ))
            .build()
            .unwrap();
        let auth = AdminAuth::from_config(&cfg).unwrap();
        let state = state_with_protection(ApiProtection::load(Some(&cfg)).unwrap(), auth);
        let response = crate::api::create_all_routes(&state)
            .layer(Extension(state))
            .oneshot(
                HttpRequest::post("/flows")
                    .header("content-type", "application/json")
                    .header("content-length", "100")
                    .body(Body::from("{}"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    fn state_with_protection(protection: ApiProtection, auth: AdminAuth) -> Arc<WebState> {
        WebState::assemble_with_egress(
            Arc::new(RedSystemSettings::default()),
            std::env::temp_dir(),
            None,
            auth,
            Fleet::disabled(),
            WebRuntimeServices {
                egress: EgressPolicyHandle::default(),
                credentials: CredentialStore::default(),
                protection,
                ..Default::default()
            },
            false,
        )
    }

    #[test]
    fn routes_have_stable_resource_classes() {
        assert_eq!(classify("/api/health"), EndpointClass::Health);
        assert_eq!(classify("/auth/token"), EndpointClass::Authentication);
        assert_eq!(classify("/comms"), EndpointClass::Websocket);
        assert_eq!(classify("/assistant/draft"), EndpointClass::Copilot);
        assert_eq!(classify("/fleet/push"), EndpointClass::Fleet);
        assert_eq!(classify("/locales/editor"), EndpointClass::Static);
        assert_eq!(classify("/flows"), EndpointClass::EditorAdmin);
        assert_eq!(classify("/library/local/flows/example.json"), EndpointClass::EditorAdmin);
        assert_eq!(classify("/debug/node-id/enable"), EndpointClass::EditorAdmin);
        assert_eq!(classify("/debug/view/index.html"), EndpointClass::Static);
    }
}
