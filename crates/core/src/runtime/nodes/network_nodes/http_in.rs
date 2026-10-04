use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::{Arc, Mutex};
use std::time::Instant;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;

use serde::Deserialize;
use serde_json::{Map, Value};

use crate::runtime::flow::Flow;
use crate::runtime::http_registry::{HttpResponse, HttpResponseRegistry};
use crate::runtime::ingress::{EndpointLimits, IngressProtectionConfig, ProtectionMode, TrustedProxySet};
use crate::runtime::nodes::*;
use edgelink_macro::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
#[derive(Default)]
enum HttpMethod {
    #[default]
    Get,
    Post,
    Put,
    Delete,
    Patch,
    Options,
    Head,
}

impl HttpMethod {
    fn as_str(&self) -> &'static str {
        match self {
            HttpMethod::Get => "GET",
            HttpMethod::Post => "POST",
            HttpMethod::Put => "PUT",
            HttpMethod::Delete => "DELETE",
            HttpMethod::Patch => "PATCH",
            HttpMethod::Options => "OPTIONS",
            HttpMethod::Head => "HEAD",
        }
    }

    fn from_str(s: &str) -> Option<Self> {
        match s.to_uppercase().as_str() {
            "GET" => Some(HttpMethod::Get),
            "POST" => Some(HttpMethod::Post),
            "PUT" => Some(HttpMethod::Put),
            "DELETE" => Some(HttpMethod::Delete),
            "PATCH" => Some(HttpMethod::Patch),
            "OPTIONS" => Some(HttpMethod::Options),
            "HEAD" => Some(HttpMethod::Head),
            _ => None,
        }
    }
}

#[derive(Debug)]
#[flow_node("http in", red_name = "httpin")]
struct HttpInNode {
    base: BaseFlowNodeState,
    config: HttpInNodeConfig,
    limits: EndpointLimits,
    trusted_proxies: TrustedProxySet,
    concurrency: Arc<Semaphore>,
    rate: Mutex<HashMap<IpAddr, (Instant, u32)>>,
    max_rate_keys: usize,
    webhook_token: Option<String>,
}

struct ResponseRegistration {
    registry: Arc<HttpResponseRegistry>,
    id: Option<String>,
}

impl ResponseRegistration {
    async fn cleanup(&mut self) {
        if let Some(id) = self.id.take() {
            self.registry.cleanup_handler(&id).await;
        }
    }
}

impl Drop for ResponseRegistration {
    fn drop(&mut self) {
        let Some(id) = self.id.take() else {
            return;
        };
        let registry = Arc::clone(&self.registry);
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move {
                registry.cleanup_handler(&id).await;
            });
        }
    }
}

impl HttpInNode {
    fn build(
        _flow: &Flow,
        state: BaseFlowNodeState,
        config: &RedFlowNodeConfig,
        options: Option<&config::Config>,
    ) -> crate::Result<Box<dyn FlowNodeBehavior>> {
        let http_config = HttpInNodeConfig::deserialize(&config.rest)?;
        if !http_config.url.starts_with('/') || http_config.url.contains('?') {
            return Err(EdgelinkError::invalid_operation(
                "http in url must be an exact absolute path without a query string",
            ));
        }
        let ingress = IngressProtectionConfig::load(options).map_err(|err| EdgelinkError::invalid_operation(&err))?;
        let limits = ingress.webhook.clone();
        let trusted_proxies =
            TrustedProxySet::new(&ingress.trusted_proxies).map_err(|err| EdgelinkError::invalid_operation(&err))?;
        let webhook_token =
            match ingress.webhook_bearer_env.as_deref().map(str::trim).filter(|name| !name.is_empty()) {
                Some(name) => Some(std::env::var(name).ok().filter(|value| !value.is_empty()).ok_or_else(|| {
                    EdgelinkError::invalid_operation("configured webhook bearer environment is missing")
                })?),
                None => None,
            };
        let concurrency = Arc::new(Semaphore::new(limits.max_concurrency));
        let node = HttpInNode {
            base: state,
            config: http_config,
            limits,
            trusted_proxies,
            concurrency,
            rate: Mutex::new(HashMap::new()),
            max_rate_keys: ingress.max_rate_keys,
            webhook_token,
        };
        Ok(Box::new(node))
    }
}

#[allow(dead_code)]
#[derive(Deserialize, Debug, Clone)]
struct HttpInNodeConfig {
    /// URL path for the HTTP endpoint
    url: String,

    /// HTTP method
    #[serde(default)]
    method: HttpMethod,

    /// Whether to enable file upload support
    #[serde(default)]
    upload: bool,

    /// Swagger documentation reference
    #[serde(rename = "swaggerDoc")]
    swagger_doc: Option<String>,

    /// Server port (optional, defaults to 1880)
    #[serde(default = "default_port")]
    port: u16,

    /// Server host (optional, defaults to "0.0.0.0")
    #[serde(default = "default_host")]
    host: String,
}

fn default_port() -> u16 {
    1880
}

fn default_host() -> String {
    "0.0.0.0".to_string()
}

impl HttpInNode {
    #[allow(clippy::too_many_arguments)]
    async fn create_request_message(
        &self,
        method: &str,
        path: &str,
        query: Option<Map<String, Value>>,
        headers: Map<String, Value>,
        body: Option<Vec<u8>>,
        remote_addr: Option<String>,
        msg_id: String,
    ) -> MsgHandle {
        let mut msg_body = std::collections::BTreeMap::new();

        // Generate message ID
        msg_body.insert("_msgid".to_string(), Variant::String(msg_id.clone()));

        // Create request object
        let mut req = std::collections::BTreeMap::new();

        // Add method and URL
        req.insert("method".to_string(), Variant::String(method.to_string()));
        req.insert("url".to_string(), Variant::String(path.to_string()));
        req.insert("originalUrl".to_string(), Variant::String(path.to_string()));
        req.insert("path".to_string(), Variant::String(path.to_string()));

        // Add headers
        let headers_variant = Self::json_value_to_variant(Value::Object(headers.clone()));
        req.insert("headers".to_string(), headers_variant);

        // Add query parameters or body based on method
        if matches!(self.config.method, HttpMethod::Get) {
            if let Some(query_params) = query {
                let query_variant = Self::json_value_to_variant(Value::Object(query_params));
                msg_body.insert("payload".to_string(), query_variant.clone());
                req.insert("query".to_string(), query_variant);
            } else {
                msg_body.insert("payload".to_string(), Variant::Object(std::collections::BTreeMap::new()));
                req.insert("query".to_string(), Variant::Object(std::collections::BTreeMap::new()));
            }
        } else {
            // For POST, PUT, PATCH, DELETE - use body as payload
            let payload = if let Some(body_bytes) = body {
                if let Ok(json_str) = String::from_utf8(body_bytes.clone()) {
                    // Try to parse as JSON
                    match serde_json::from_str::<Value>(&json_str) {
                        Ok(json_val) => Self::json_value_to_variant(json_val),
                        Err(_) => {
                            // If not JSON, check content type
                            if self.is_text_content(&headers) {
                                Variant::String(json_str)
                            } else {
                                // Return as buffer (array of bytes)
                                let bytes: Vec<Variant> =
                                    body_bytes.iter().map(|&b| Variant::Number(serde_json::Number::from(b))).collect();
                                Variant::Array(bytes)
                            }
                        }
                    }
                } else {
                    // Binary data - return as buffer
                    let bytes: Vec<Variant> =
                        body_bytes.iter().map(|&b| Variant::Number(serde_json::Number::from(b))).collect();
                    Variant::Array(bytes)
                }
            } else {
                Variant::String(String::new())
            };
            msg_body.insert("payload".to_string(), payload);
        }

        // Add remote address if available
        if let Some(addr) = remote_addr {
            req.insert("ip".to_string(), Variant::String(addr.clone()));
            req.insert("hostname".to_string(), Variant::String(addr));
        }

        req.insert("protocol".to_string(), Variant::String("http".to_string()));

        msg_body.insert("req".to_string(), Variant::Object(req));

        // Create response placeholder that will be used by http out node
        let mut res = std::collections::BTreeMap::new();
        res.insert("_msgid".to_string(), Variant::String(msg_id));
        res.insert("_node_id".to_string(), Variant::String(self.base.id.to_string()));
        res.insert("statusCode".to_string(), Variant::Number(serde_json::Number::from(200)));
        msg_body.insert("res".to_string(), Variant::Object(res));

        MsgHandle::with_properties(msg_body)
    }

    fn is_text_content(&self, headers: &Map<String, Value>) -> bool {
        if let Some(content_type) = headers.get("content-type")
            && let Some(ct_str) = content_type.as_str()
        {
            let ct_lower = ct_str.to_lowercase();
            return ct_lower.starts_with("text/")
                || ct_lower.starts_with("application/json")
                || ct_lower.starts_with("application/xml")
                || ct_lower.contains("charset");
        }
        false
    }

    fn json_value_to_variant(value: Value) -> Variant {
        match value {
            Value::Null => Variant::Null,
            Value::Bool(b) => Variant::Bool(b),
            Value::Number(n) => Variant::Number(n),
            Value::String(s) => Variant::String(s),
            Value::Array(arr) => {
                let variants: Vec<Variant> = arr.into_iter().map(Self::json_value_to_variant).collect();
                Variant::Array(variants)
            }
            Value::Object(obj) => {
                let mut map = std::collections::BTreeMap::new();
                for (k, v) in obj {
                    map.insert(k, Self::json_value_to_variant(v));
                }
                Variant::Object(map)
            }
        }
    }

    fn parse_query_string(query: &str) -> Map<String, Value> {
        let mut params = Map::new();
        for pair in query.split('&') {
            if let Some((key, value)) = pair.split_once('=') {
                let key = urlencoding::decode(key).unwrap_or_else(|_| key.into());
                let value = urlencoding::decode(value).unwrap_or_else(|_| value.into());
                params.insert(key.to_string(), Value::String(value.to_string()));
            } else if !pair.is_empty() {
                let key = urlencoding::decode(pair).unwrap_or_else(|_| pair.into());
                params.insert(key.to_string(), Value::String(String::new()));
            }
        }
        params
    }

    async fn handle_http_request(&self, mut stream: TcpStream, stop_token: CancellationToken) {
        let started = Instant::now();
        let remote_addr = stream.peer_addr().ok().map(|addr| addr.to_string());
        let mut buf_reader = BufReader::new(&mut stream);
        let deadline = (self.limits.mode == ProtectionMode::Enforce)
            .then(|| tokio::time::Instant::now() + self.limits.request_timeout());

        let request_line =
            match self.read_line(&mut buf_reader, self.limits.max_header_bytes.min(8_192), deadline).await {
                Ok(Some(line)) => line,
                Ok(None) => return,
                Err((status, message)) => {
                    drop(buf_reader);
                    Self::send_error(&mut stream, status, message).await;
                    return;
                }
            };

        let request_line = request_line.trim();
        let parts: Vec<&str> = request_line.split_whitespace().collect();

        if parts.len() != 3 || !matches!(parts[2], "HTTP/1.0" | "HTTP/1.1") {
            drop(buf_reader);
            Self::send_error(&mut stream, 400, "Bad Request").await;
            return;
        }

        let method = parts[0];
        let full_path = parts[1];
        let _version = parts[2];

        // Parse path and query string
        let (path, query) = if let Some((p, q)) = full_path.split_once('?') {
            (p.to_string(), Some(Self::parse_query_string(q)))
        } else {
            (full_path.to_string(), None)
        };

        // Check if this request matches our endpoint
        if !self.path_matches(&path) || !self.method_matches(method) {
            drop(buf_reader);
            Self::send_error(&mut stream, 404, "Not Found").await;
            return;
        }

        let mut headers = Map::new();
        let mut content_length = None;
        let mut transfer_encoding = None;
        let mut header_bytes = request_line.len();
        let mut header_count = 0usize;

        loop {
            let header_line = match self.read_line(&mut buf_reader, self.limits.max_header_bytes, deadline).await {
                Ok(Some(line)) => line,
                Ok(None) => {
                    drop(buf_reader);
                    Self::send_error(&mut stream, 400, "Bad Request").await;
                    return;
                }
                Err((status, message)) => {
                    drop(buf_reader);
                    Self::send_error(&mut stream, status, message).await;
                    return;
                }
            };
            header_bytes = header_bytes.saturating_add(header_line.len());
            let header_line = header_line.trim();
            if header_line.is_empty() {
                break;
            }
            header_count += 1;
            if header_bytes > self.limits.max_header_bytes || header_count > self.limits.max_headers {
                if self.limits.mode == ProtectionMode::Enforce {
                    drop(buf_reader);
                    Self::send_error(&mut stream, 431, "Request Header Fields Too Large").await;
                    return;
                }
                Self::log_decision(self.limits.mode, "headers_too_large");
            }
            let Some((key, value)) = header_line.split_once(':') else {
                drop(buf_reader);
                Self::send_error(&mut stream, 400, "Bad Request").await;
                return;
            };
            let key = key.trim().to_ascii_lowercase();
            let value = value.trim();
            if key.is_empty() || key.chars().any(char::is_whitespace) {
                drop(buf_reader);
                Self::send_error(&mut stream, 400, "Bad Request").await;
                return;
            }
            if key == "content-length" {
                let parsed = match value.parse::<usize>() {
                    Ok(value) => value,
                    Err(_) => {
                        drop(buf_reader);
                        Self::send_error(&mut stream, 400, "Bad Request").await;
                        return;
                    }
                };
                if content_length.is_some_and(|existing| existing != parsed) {
                    drop(buf_reader);
                    Self::send_error(&mut stream, 400, "Bad Request").await;
                    return;
                }
                content_length = Some(parsed);
            } else if key == "transfer-encoding" {
                transfer_encoding = Some(value.to_ascii_lowercase());
            }
            headers.insert(key, Value::String(value.to_string()));
        }

        if transfer_encoding.is_some() && content_length.is_some() {
            drop(buf_reader);
            Self::send_error(&mut stream, 400, "Bad Request").await;
            return;
        }

        if let Some(expected) = &self.webhook_token {
            let supplied = headers
                .get("authorization")
                .and_then(Value::as_str)
                .and_then(|value| value.strip_prefix("Bearer ").or_else(|| value.strip_prefix("bearer ")))
                .map(str::trim)
                .unwrap_or("");
            if !constant_time_equal(expected, supplied) {
                drop(buf_reader);
                Self::log_decision(self.limits.mode, "unauthorized");
                Self::send_error(&mut stream, 401, "Unauthorized").await;
                return;
            }
        }

        let client_addr = match self.effective_client(remote_addr.as_deref(), &headers) {
            Ok(address) => address,
            Err(()) if self.limits.mode == ProtectionMode::Enforce => {
                drop(buf_reader);
                Self::log_decision(self.limits.mode, "invalid_forwarded");
                Self::send_error(&mut stream, 400, "Bad Request").await;
                return;
            }
            Err(()) => remote_addr.clone(),
        };
        if let Some(address) = client_addr.as_deref().and_then(|value| value.parse::<IpAddr>().ok())
            && !self.check_rate(address)
        {
            if self.limits.mode == ProtectionMode::Enforce {
                drop(buf_reader);
                Self::log_decision(self.limits.mode, "rate_limited");
                Self::send_error(&mut stream, 429, "Too Many Requests").await;
                return;
            }
            Self::log_decision(self.limits.mode, "rate_limited");
        }

        let body = match transfer_encoding.as_deref() {
            Some("chunked") => match self.read_chunked_body(&mut buf_reader, deadline).await {
                Ok(body) => Some(body),
                Err((status, message)) => {
                    drop(buf_reader);
                    Self::send_error(&mut stream, status, message).await;
                    return;
                }
            },
            Some(_) => {
                drop(buf_reader);
                Self::send_error(&mut stream, 400, "Unsupported Transfer Encoding").await;
                return;
            }
            None => {
                let length = content_length.unwrap_or(0);
                if length > self.limits.max_body_bytes {
                    if self.limits.mode == ProtectionMode::Enforce {
                        drop(buf_reader);
                        Self::log_decision(self.limits.mode, "body_too_large");
                        Self::send_error(&mut stream, 413, "Payload Too Large").await;
                        return;
                    }
                    Self::log_decision(self.limits.mode, "body_too_large");
                }
                if length == 0 {
                    None
                } else {
                    let mut bytes = vec![0u8; length];
                    let read_result = match deadline {
                        Some(deadline) => {
                            match tokio::time::timeout_at(deadline, buf_reader.read_exact(&mut bytes)).await {
                                Ok(result) => result,
                                Err(_) => {
                                    drop(buf_reader);
                                    Self::log_decision(self.limits.mode, "body_timeout");
                                    Self::send_error(&mut stream, 408, "Request Timeout").await;
                                    return;
                                }
                            }
                        }
                        None => buf_reader.read_exact(&mut bytes).await,
                    };
                    match read_result {
                        Ok(_) => Some(bytes),
                        Err(_) => {
                            drop(buf_reader);
                            Self::send_error(&mut stream, 400, "Bad Request").await;
                            return;
                        }
                    }
                }
            }
        };
        drop(buf_reader);

        // Generate unique message ID for this request
        let msg_id = format!(
            "{}_{}",
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos(),
            std::ptr::addr_of!(self) as usize
        );

        // Create response channel and register with global registry
        let (response_tx, response_rx) = tokio::sync::oneshot::channel();
        let mut registration = if let Some(engine) = self.engine() {
            let registry = Arc::clone(engine.http_response_registry());
            registry.register_handler(msg_id.clone(), response_tx).await;
            ResponseRegistration { registry, id: Some(msg_id.clone()) }
        } else {
            log::error!("HTTP in: No engine available for response registry");
            Self::send_error(&mut stream, 500, "Internal Server Error").await;
            return;
        };

        // Create and send message
        let msg = self.create_request_message(method, &path, query, headers, body, client_addr, msg_id.clone()).await;

        log::info!("HTTP in request method={method} class=webhook");

        // Send message to flow
        if let Err(e) = self.fan_out_one(Envelope { port: 0, msg }, stop_token.clone()).await {
            log::error!("HTTP in: Failed to send message: {e}");
            registration.cleanup().await;
            Self::send_error(&mut stream, 500, "Internal Server Error").await;
            return;
        }

        // Wait for response with timeout
        let response_deadline =
            deadline.unwrap_or_else(|| tokio::time::Instant::now() + std::time::Duration::from_secs(30));
        let response = tokio::select! {
            _ = stop_token.cancelled() => {
                log::debug!("HTTP in: Request cancelled");
                return;
            }
            result = response_rx => {
                match result {
                    Ok(response) => response,
                    Err(_) => {
                        // No response received, send default
                        HttpResponse {
                            status_code: 200,
                            headers: HashMap::new(),
                            body: b"OK".to_vec(),
                        }
                    }
                }
            }
            _ = tokio::time::sleep_until(response_deadline) => {
                // Timeout
                log::warn!("HTTP in: Response timeout for {msg_id}");
                HttpResponse {
                    status_code: 504,
                    headers: HashMap::new(),
                    body: b"Gateway Timeout".to_vec(),
                }
            }
        };

        // Clean up response handler from global registry
        registration.cleanup().await;

        // Send HTTP response
        if response.body.len() > self.limits.max_response_bytes && self.limits.mode == ProtectionMode::Enforce {
            Self::log_decision(self.limits.mode, "response_too_large");
            Self::send_error(&mut stream, 502, "Bad Gateway").await;
        } else {
            self.send_http_response(&mut stream, response).await;
        }
        if self.limits.mode == ProtectionMode::Observe && started.elapsed() > self.limits.request_timeout() {
            Self::log_decision(self.limits.mode, "request_timeout");
        }
    }

    fn path_matches(&self, request_path: &str) -> bool {
        request_path == self.config.url
    }

    async fn read_line<R>(
        &self,
        reader: &mut R,
        maximum: usize,
        deadline: Option<tokio::time::Instant>,
    ) -> Result<Option<String>, (u16, &'static str)>
    where
        R: tokio::io::AsyncBufRead + Unpin,
    {
        let mut bytes = Vec::new();
        let count = match deadline {
            Some(deadline) => {
                match tokio::time::timeout_at(deadline, reader.take((maximum + 1) as u64).read_until(b'\n', &mut bytes))
                    .await
                {
                    Ok(Ok(count)) => count,
                    Ok(Err(_)) => return Err((400, "Bad Request")),
                    Err(_) => return Err((408, "Request Timeout")),
                }
            }
            None => reader.read_until(b'\n', &mut bytes).await.map_err(|_| (400, "Bad Request"))?,
        };
        if count == 0 {
            return Ok(None);
        }
        if count > maximum || !bytes.ends_with(b"\n") {
            if self.limits.mode == ProtectionMode::Enforce {
                return Err((431, "Request Header Fields Too Large"));
            }
            Self::log_decision(self.limits.mode, "headers_too_large");
        }
        String::from_utf8(bytes).map(Some).map_err(|_| (400, "Bad Request"))
    }

    async fn read_chunked_body<R>(
        &self,
        reader: &mut R,
        deadline: Option<tokio::time::Instant>,
    ) -> Result<Vec<u8>, (u16, &'static str)>
    where
        R: tokio::io::AsyncBufRead + Unpin,
    {
        let mut body = Vec::new();
        loop {
            let line = self.read_line(reader, 128, deadline).await?.ok_or((400, "Bad Request"))?;
            let size = usize::from_str_radix(line.trim().split(';').next().unwrap_or(""), 16)
                .map_err(|_| (400, "Bad Request"))?;
            if size == 0 {
                let trailer = self.read_line(reader, self.limits.max_header_bytes, deadline).await?;
                if trailer.as_deref().is_some_and(|line| !line.trim().is_empty()) {
                    return Err((400, "Bad Request"));
                }
                return Ok(body);
            }
            if body.len().saturating_add(size) > self.limits.max_body_bytes {
                Self::log_decision(self.limits.mode, "body_too_large");
                if self.limits.mode == ProtectionMode::Enforce {
                    return Err((413, "Payload Too Large"));
                }
            }
            let start = body.len();
            body.resize(start + size, 0);
            match deadline {
                Some(deadline) => tokio::time::timeout_at(deadline, reader.read_exact(&mut body[start..]))
                    .await
                    .map_err(|_| (408, "Request Timeout"))?
                    .map_err(|_| (400, "Bad Request"))?,
                None => reader.read_exact(&mut body[start..]).await.map_err(|_| (400, "Bad Request"))?,
            };
            let mut ending = [0u8; 2];
            match deadline {
                Some(deadline) => tokio::time::timeout_at(deadline, reader.read_exact(&mut ending))
                    .await
                    .map_err(|_| (408, "Request Timeout"))?
                    .map_err(|_| (400, "Bad Request"))?,
                None => reader.read_exact(&mut ending).await.map_err(|_| (400, "Bad Request"))?,
            };
            if ending != *b"\r\n" {
                return Err((400, "Bad Request"));
            }
        }
    }

    fn effective_client(&self, peer: Option<&str>, headers: &Map<String, Value>) -> Result<Option<String>, ()> {
        let peer_ip = peer.and_then(|peer| peer.parse::<std::net::SocketAddr>().ok()).map(|peer| peer.ip());
        let Some(peer_ip) = peer_ip else {
            return Ok(peer.map(str::to_owned));
        };
        if !self.trusted_proxies.contains(peer_ip) {
            return Ok(Some(peer_ip.to_string()));
        }
        let forwarded = headers
            .get("forwarded")
            .and_then(Value::as_str)
            .and_then(|value| value.split(',').next())
            .and_then(|value| value.split(';').find_map(|part| part.trim().strip_prefix("for=")))
            .map(|value| value.trim_matches('"').trim_matches(['[', ']']).to_string())
            .or_else(|| {
                headers
                    .get("x-forwarded-for")
                    .and_then(Value::as_str)
                    .and_then(|value| value.split(',').next())
                    .map(|value| value.trim().to_string())
            });
        match forwarded {
            Some(value) => parse_forwarded_ip(&value).map(|address| Some(address.to_string())).ok_or(()),
            None => Ok(Some(peer_ip.to_string())),
        }
    }

    fn check_rate(&self, address: IpAddr) -> bool {
        let now = Instant::now();
        let mut rate = self.rate.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        rate.retain(|_, (started, _)| now.duration_since(*started) < std::time::Duration::from_secs(60));
        if !rate.contains_key(&address) && rate.len() >= self.max_rate_keys {
            return false;
        }
        let entry = rate.entry(address).or_insert((now, 0));
        if entry.1 >= self.limits.requests_per_minute {
            return false;
        }
        entry.1 += 1;
        true
    }

    async fn send_error(stream: &mut TcpStream, status: u16, message: &'static str) {
        let response = format!(
            "HTTP/1.1 {status} {message}\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{message}",
            message.len()
        );
        let _ = stream.write_all(response.as_bytes()).await;
    }

    fn log_decision(mode: ProtectionMode, reason: &str) {
        let action = if mode == ProtectionMode::Enforce { "reject" } else { "permit" };
        log::warn!("ingress decision mode={mode:?} action={action} class=webhook reason={reason}");
    }

    fn method_matches(&self, request_method: &str) -> bool {
        HttpMethod::from_str(request_method) == Some(self.config.method)
    }

    async fn send_http_response(&self, stream: &mut TcpStream, response: HttpResponse) {
        let header_bytes = response
            .headers
            .iter()
            .map(|(key, value)| key.len().saturating_add(value.len()).saturating_add(4))
            .sum::<usize>();
        let unsafe_headers = response.headers.iter().any(|(key, value)| {
            key.is_empty()
                || key.contains(['\r', '\n', ':'])
                || value.contains(['\r', '\n'])
                || key.chars().any(char::is_whitespace)
        });
        if self.limits.mode == ProtectionMode::Enforce
            && (response.headers.len() > self.limits.max_headers
                || header_bytes > self.limits.max_header_bytes
                || unsafe_headers)
        {
            Self::log_decision(self.limits.mode, "invalid_response_headers");
            Self::send_error(stream, 502, "Bad Gateway").await;
            return;
        }
        let mut response_text =
            format!("HTTP/1.1 {} {}\r\n", response.status_code, self.status_text(response.status_code));

        // Add headers
        for (key, value) in &response.headers {
            response_text.push_str(&format!("{key}: {value}\r\n"));
        }

        // Add content length
        response_text.push_str(&format!("Content-Length: {}\r\n", response.body.len()));

        // End headers
        response_text.push_str("\r\n");

        // Send response headers
        if let Err(e) = stream.write_all(response_text.as_bytes()).await {
            log::error!("HTTP in: Failed to send response headers: {e}");
            return;
        }

        // Send response body
        if !response.body.is_empty()
            && let Err(e) = stream.write_all(&response.body).await
        {
            log::error!("HTTP in: Failed to send response body: {e}");
        }

        let _ = stream.flush().await;
    }

    fn status_text(&self, code: u16) -> &'static str {
        match code {
            200 => "OK",
            201 => "Created",
            204 => "No Content",
            400 => "Bad Request",
            408 => "Request Timeout",
            413 => "Payload Too Large",
            429 => "Too Many Requests",
            431 => "Request Header Fields Too Large",
            401 => "Unauthorized",
            403 => "Forbidden",
            404 => "Not Found",
            405 => "Method Not Allowed",
            500 => "Internal Server Error",
            502 => "Bad Gateway",
            503 => "Service Unavailable",
            504 => "Gateway Timeout",
            _ => "Unknown",
        }
    }
}

#[async_trait::async_trait]
impl FlowNodeBehavior for HttpInNode {
    fn get_base(&self) -> &BaseFlowNodeState {
        &self.base
    }

    async fn run(self: Arc<Self>, stop_token: CancellationToken) {
        let bind_addr = format!("{}:{}", self.config.host, self.config.port);

        match TcpListener::bind(&bind_addr).await {
            Ok(listener) => {
                let actual_addr = listener.local_addr().unwrap_or_else(|_| bind_addr.parse().unwrap());
                log::info!(
                    "HTTP in: Server listening on {} for {} {}",
                    actual_addr,
                    self.config.method.as_str(),
                    self.config.url
                );

                loop {
                    tokio::select! {
                        _ = stop_token.cancelled() => {
                            log::debug!("HTTP in: Server stop token cancelled");
                            break;
                        }
                        accept_result = listener.accept() => {
                            match accept_result {
                                Ok((mut stream, _remote_addr)) => {
                                    let self_clone = self.clone();
                                    let stop_token_clone = stop_token.clone();
                                    let permit = if self.limits.mode == ProtectionMode::Enforce {
                                        match tokio::time::timeout(
                                            self.limits.queue_timeout(),
                                            Arc::clone(&self.concurrency).acquire_owned(),
                                        )
                                        .await
                                        {
                                            Ok(Ok(permit)) => Some(permit),
                                            Ok(Err(_)) => break,
                                            Err(_) => {
                                                Self::log_decision(self.limits.mode, "busy");
                                                Self::send_error(&mut stream, 429, "Too Many Requests").await;
                                                continue;
                                            }
                                        }
                                    } else {
                                        None
                                    };

                                    tokio::spawn(async move {
                                        let _permit = permit;
                                        let request_stop = stop_token_clone.clone();
                                        tokio::select! {
                                            _ = stop_token_clone.cancelled() => {}
                                            _ = self_clone.handle_http_request(stream, request_stop) => {}
                                        }
                                    });
                                }
                                Err(e) => {
                                    log::error!("HTTP in: Failed to accept connection: {e}");
                                    break;
                                }
                            }
                        }
                    }
                }

                log::info!("HTTP in: Server stopped listening on {actual_addr}");
            }
            Err(e) => {
                log::error!("HTTP in: Cannot bind to {bind_addr}: {e}");
            }
        }
    }
}

fn parse_forwarded_ip(value: &str) -> Option<IpAddr> {
    let value = value.trim().trim_matches('"');
    value
        .parse::<IpAddr>()
        .ok()
        .or_else(|| value.parse::<std::net::SocketAddr>().ok().map(|address| address.ip()))
        .or_else(|| {
            let close = value.find(']')?;
            value.get(1..close)?.parse::<IpAddr>().ok()
        })
}

fn constant_time_equal(expected: &str, supplied: &str) -> bool {
    let left = expected.as_bytes();
    let right = supplied.as_bytes();
    let width = left.len().max(right.len());
    let mut difference = left.len() ^ right.len();
    for index in 0..width {
        difference |= usize::from(left.get(index).copied().unwrap_or(0) ^ right.get(index).copied().unwrap_or(0));
    }
    difference == 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::engine::Engine;
    use crate::runtime::registry::RegistryBuilder;
    use serde_json::json;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    async fn unused_port() -> u16 {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        listener.local_addr().unwrap().port()
    }

    async fn request(port: u16, bytes: &[u8]) -> String {
        let mut stream = loop {
            match TcpStream::connect(("127.0.0.1", port)).await {
                Ok(stream) => break stream,
                Err(_) => tokio::time::sleep(std::time::Duration::from_millis(10)).await,
            }
        };
        stream.write_all(bytes).await.unwrap();
        stream.shutdown().await.unwrap();
        let mut response = Vec::new();
        stream.read_to_end(&mut response).await.unwrap();
        String::from_utf8(response).unwrap()
    }

    #[tokio::test]
    async fn webhook_is_exact_and_rejects_oversized_fixed_and_chunked_bodies() {
        let port = unused_port().await;
        let cfg = config::Config::builder()
            .add_source(config::File::from_str(
                r#"
                [runtime.context]
                default = "memory"
                [runtime.context.stores]
                memory = { provider = "memory" }
                [api_protection.webhook]
                max_body_bytes = 4
                max_headers = 4
                request_timeout_ms = 100
                "#,
                config::FileFormat::Toml,
            ))
            .build()
            .unwrap();
        let flows = json!([
            { "id": "100", "type": "tab", "label": "HTTP" },
            {
                "id": "101", "z": "100", "type": "http in", "url": "/hook", "method": "post",
                "host": "127.0.0.1", "port": port, "wires": [["102"]]
            },
            { "id": "102", "z": "100", "type": "http response", "wires": [] }
        ]);
        let registry = RegistryBuilder::default().build().unwrap();
        let engine = Engine::with_json(&registry, flows, Some(cfg)).unwrap();
        engine.start().await.unwrap();

        let ok = request(port, b"POST /hook HTTP/1.1\r\nHost: local\r\nContent-Length: 4\r\n\r\nping").await;
        assert!(ok.starts_with("HTTP/1.1 200"), "{ok}");
        assert!(ok.ends_with("ping"), "{ok}");

        let prefix = request(port, b"POST /hook/child HTTP/1.1\r\nHost: local\r\nContent-Length: 0\r\n\r\n").await;
        assert!(prefix.starts_with("HTTP/1.1 404"), "{prefix}");

        let fixed = request(port, b"POST /hook HTTP/1.1\r\nHost: local\r\nContent-Length: 5\r\n\r\n12345").await;
        assert!(fixed.starts_with("HTTP/1.1 413"), "{fixed}");

        let chunked = request(
            port,
            b"POST /hook HTTP/1.1\r\nHost: local\r\nTransfer-Encoding: chunked\r\n\r\n5\r\n12345\r\n0\r\n\r\n",
        )
        .await;
        assert!(chunked.starts_with("HTTP/1.1 413"), "{chunked}");

        let malformed = request(port, b"POST /hook HTTP/1.1\r\nHost: local\r\nContent-Length: nope\r\n\r\n").await;
        assert!(malformed.starts_with("HTTP/1.1 400"), "{malformed}");

        let headers = request(
            port,
            b"POST /hook HTTP/1.1\r\nHost: local\r\nX-One: 1\r\nX-Two: 2\r\nX-Three: 3\r\nContent-Length: 0\r\n\r\n",
        )
        .await;
        assert!(headers.starts_with("HTTP/1.1 431"), "{headers}");

        let wrong_method = request(port, b"GET /hook HTTP/1.1\r\nHost: local\r\n\r\n").await;
        assert!(wrong_method.starts_with("HTTP/1.1 404"), "{wrong_method}");

        let mut slow = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        slow.write_all(b"POST /hook HTTP/1.1\r\nHost: local\r\nContent-Length: 4\r\n\r\np").await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        let mut response = Vec::new();
        slow.read_to_end(&mut response).await.unwrap();
        assert!(String::from_utf8(response).unwrap().starts_with("HTTP/1.1 408"));

        engine.stop().await.unwrap();

        let observe_port = unused_port().await;
        let observe_config = config::Config::builder()
            .add_source(config::File::from_str(
                r#"
                [runtime.context]
                default = "memory"
                [runtime.context.stores]
                memory = { provider = "memory" }
                [api_protection.webhook]
                mode = "observe"
                max_body_bytes = 4
                max_headers = 2
                request_timeout_ms = 100
                "#,
                config::FileFormat::Toml,
            ))
            .build()
            .unwrap();
        let observe_flows = json!([
            { "id": "200", "type": "tab", "label": "HTTP observe" },
            {
                "id": "201", "z": "200", "type": "http in", "url": "/hook", "method": "post",
                "host": "127.0.0.1", "port": observe_port, "wires": [["202"]]
            },
            { "id": "202", "z": "200", "type": "http response", "wires": [] }
        ]);
        let observe_engine = Engine::with_json(&registry, observe_flows, Some(observe_config)).unwrap();
        observe_engine.start().await.unwrap();
        let observed = request(
            observe_port,
            b"POST /hook HTTP/1.1\r\nHost: local\r\nX-Probe: safe\r\nContent-Length: 5\r\n\r\n12345",
        )
        .await;
        assert!(observed.starts_with("HTTP/1.1 200"), "{observed}");
        assert!(observed.ends_with("12345"), "{observed}");
        observe_engine.stop().await.unwrap();
    }

    #[test]
    fn webhook_requires_an_exact_absolute_path() {
        let registry = RegistryBuilder::default().build().unwrap();
        let flows = json!([
            { "id": "100", "type": "tab", "label": "HTTP" },
            {
                "id": "101", "z": "100", "type": "http in", "url": "contains", "method": "get",
                "host": "127.0.0.1", "port": 1880, "wires": [[]]
            }
        ]);
        let error = Engine::with_json(&registry, flows, None).unwrap_err().to_string();
        assert!(error.contains("exact absolute path"), "{error}");
    }

    #[test]
    fn configured_webhook_auth_fails_closed_when_its_environment_is_missing() {
        let registry = RegistryBuilder::default().build().unwrap();
        let cfg = config::Config::builder()
            .add_source(config::File::from_str(
                r#"
                [runtime.context]
                default = "memory"
                [runtime.context.stores]
                memory = { provider = "memory" }
                [api_protection]
                webhook_bearer_env = "EDGELINK_PHASE3_TEST_ENV_THAT_MUST_NOT_EXIST"
                "#,
                config::FileFormat::Toml,
            ))
            .build()
            .unwrap();
        let flows = json!([
            { "id": "100", "type": "tab", "label": "HTTP" },
            {
                "id": "101", "z": "100", "type": "http in", "url": "/hook", "method": "get",
                "host": "127.0.0.1", "port": 1880, "wires": [[]]
            }
        ]);
        let result = Engine::with_json(&registry, flows, Some(cfg));
        let error = match result {
            Ok(_) => panic!("missing webhook bearer token was accepted"),
            Err(error) => error.to_string(),
        };
        assert!(error.contains("webhook bearer environment is missing"), "{error}");
        assert!(constant_time_equal("test-token", "test-token"));
        assert!(!constant_time_equal("test-token", "wrong-token"));
    }
}
