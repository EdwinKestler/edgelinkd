//! One request/response contract. Each provider maps it onto its HTTP API.

use std::time::Duration;

use reqwest::Client;
use serde_json::{Value, json};

use crate::EdgelinkError;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ProviderKind {
    Openai,
    Xai,
    Anthropic,
    Cortex,
}

impl ProviderKind {
    pub(crate) fn parse(name: &str) -> crate::Result<Self> {
        match name.trim().to_ascii_lowercase().as_str() {
            "openai" => Ok(Self::Openai),
            "xai" | "grok" => Ok(Self::Xai),
            "anthropic" | "claude" => Ok(Self::Anthropic),
            "cortex" | "snowflake" => Ok(Self::Cortex),
            other => Err(EdgelinkError::NotSupported(format!("AI provider '{other}' is not supported"))),
        }
    }

    pub(crate) fn default_base(self) -> Option<&'static str> {
        match self {
            Self::Openai => Some("https://api.openai.com/v1"),
            Self::Xai => Some("https://api.x.ai/v1"),
            Self::Anthropic => Some("https://api.anthropic.com"),
            Self::Cortex => None,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Openai => "openai",
            Self::Xai => "xai",
            Self::Anthropic => "anthropic",
            Self::Cortex => "cortex",
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct ChatMessage {
    pub role: String,
    pub content: String,
}

#[derive(Clone, Debug)]
pub(crate) struct ChatRequest {
    pub model: String,
    pub messages: Vec<ChatMessage>,
    pub system: Option<String>,
    pub temperature: Option<f64>,
    pub max_tokens: Option<u32>,
    pub timeout: Duration,
}

#[derive(Clone, Debug)]
pub(crate) struct ChatResponse {
    pub text: String,
    pub model: String,
    pub provider: String,
}

#[derive(Clone, Debug)]
pub(crate) struct ProviderSettings {
    pub kind: ProviderKind,
    pub base_url: String,
    pub api_key: String,
    pub organization: Option<String>,
}

pub(crate) async fn complete(
    client: &Client,
    settings: &ProviderSettings,
    request: &ChatRequest,
) -> crate::Result<ChatResponse> {
    match settings.kind {
        ProviderKind::Openai | ProviderKind::Xai => responses_complete(client, settings, request).await,
        ProviderKind::Anthropic => anthropic_complete(client, settings, request).await,
        ProviderKind::Cortex => chat_completions_complete(client, settings, request).await,
    }
}

fn join_url(base: &str, path: &str) -> String {
    format!("{}/{}", base.trim_end_matches('/'), path.trim_start_matches('/'))
}

fn hide_secret(text: &str, secret: &str) -> String {
    if secret.is_empty() { text.to_owned() } else { text.replace(secret, "***") }
}

fn fail(status: reqwest::StatusCode, body: &str, secret: &str) -> EdgelinkError {
    let detail = serde_json::from_str::<Value>(body)
        .ok()
        .and_then(|value| {
            value
                .pointer("/error/message")
                .or_else(|| value.pointer("/message"))
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
        .unwrap_or_default();
    let text = if detail.is_empty() {
        format!("ai provider returned {status}")
    } else {
        format!("ai provider returned {status}: {detail}")
    };
    EdgelinkError::invalid_operation(&hide_secret(&text, secret))
}

async fn send_json(
    client: &Client,
    settings: &ProviderSettings,
    url: &str,
    headers: &[(&str, String)],
    body: Value,
    timeout: Duration,
) -> crate::Result<Value> {
    let mut req = client.post(url).timeout(timeout).header("content-type", "application/json");
    for (name, value) in headers {
        req = req.header(*name, value);
    }
    let response = req.json(&body).send().await.map_err(|err| {
        EdgelinkError::invalid_operation(&hide_secret(&format!("ai request failed: {err}"), &settings.api_key))
    })?;
    let status = response.status();
    let text = response
        .text()
        .await
        .map_err(|err| EdgelinkError::invalid_operation(&hide_secret(&err.to_string(), &settings.api_key)))?;
    if !status.is_success() {
        return Err(fail(status, &text, &settings.api_key));
    }
    serde_json::from_str(&text).map_err(|err| {
        EdgelinkError::invalid_operation(&hide_secret(&format!("ai response is not JSON: {err}"), &settings.api_key))
    })
}

fn user_messages_as_input(request: &ChatRequest) -> Value {
    let mut items = Vec::new();
    if let Some(system) = request.system.as_ref().filter(|text| !text.is_empty()) {
        items.push(json!({ "role": "system", "content": system }));
    }
    for message in &request.messages {
        items.push(json!({ "role": message.role, "content": message.content }));
    }
    Value::Array(items)
}

async fn responses_complete(
    client: &Client,
    settings: &ProviderSettings,
    request: &ChatRequest,
) -> crate::Result<ChatResponse> {
    let url = join_url(&settings.base_url, "responses");
    let mut body = json!({
        "model": request.model,
        "input": user_messages_as_input(request),
    });
    if let Some(temperature) = request.temperature {
        body["temperature"] = json!(temperature);
    }
    if let Some(max_tokens) = request.max_tokens {
        body["max_output_tokens"] = json!(max_tokens);
    }
    let mut headers = vec![("authorization", format!("Bearer {}", settings.api_key))];
    if let Some(org) = &settings.organization {
        headers.push(("openai-organization", org.clone()));
    }
    let value = send_json(client, settings, &url, &headers, body, request.timeout).await?;
    let text = responses_text(&value)?;
    Ok(ChatResponse {
        text,
        model: value.get("model").and_then(Value::as_str).unwrap_or(&request.model).to_owned(),
        provider: settings.kind.as_str().to_owned(),
    })
}

fn responses_text(value: &Value) -> crate::Result<String> {
    if let Some(text) = value.get("output_text").and_then(Value::as_str).filter(|text| !text.is_empty()) {
        return Ok(text.to_owned());
    }
    let mut collected = String::new();
    if let Some(output) = value.get("output").and_then(Value::as_array) {
        for item in output {
            if let Some(content) = item.get("content").and_then(Value::as_array) {
                for part in content {
                    if let Some(text) = part.get("text").and_then(Value::as_str) {
                        collected.push_str(text);
                    }
                }
            }
        }
    }
    if collected.is_empty() {
        return Err(EdgelinkError::invalid_operation("ai provider returned no text"));
    }
    Ok(collected)
}

async fn anthropic_complete(
    client: &Client,
    settings: &ProviderSettings,
    request: &ChatRequest,
) -> crate::Result<ChatResponse> {
    let url = join_url(&settings.base_url, "v1/messages");
    let mut body = json!({
        "model": request.model,
        "max_tokens": request.max_tokens.unwrap_or(1024),
        "messages": request.messages.iter().map(|message| json!({
            "role": message.role,
            "content": message.content,
        })).collect::<Vec<_>>(),
    });
    if let Some(system) = request.system.as_ref().filter(|text| !text.is_empty()) {
        body["system"] = json!(system);
    }
    if let Some(temperature) = request.temperature {
        body["temperature"] = json!(temperature);
    }
    let headers = [("x-api-key", settings.api_key.clone()), ("anthropic-version", "2023-06-01".to_owned())];
    let headers: Vec<(&str, String)> = headers.iter().map(|(k, v)| (*k, v.clone())).collect();
    let value = send_json(client, settings, &url, &headers, body, request.timeout).await?;
    let mut collected = String::new();
    if let Some(content) = value.get("content").and_then(Value::as_array) {
        for part in content {
            if part.get("type").and_then(Value::as_str) == Some("text")
                && let Some(text) = part.get("text").and_then(Value::as_str)
            {
                collected.push_str(text);
            }
        }
    }
    if collected.is_empty() {
        return Err(EdgelinkError::invalid_operation("ai provider returned no text"));
    }
    Ok(ChatResponse {
        text: collected,
        model: value.get("model").and_then(Value::as_str).unwrap_or(&request.model).to_owned(),
        provider: settings.kind.as_str().to_owned(),
    })
}

async fn chat_completions_complete(
    client: &Client,
    settings: &ProviderSettings,
    request: &ChatRequest,
) -> crate::Result<ChatResponse> {
    let url = join_url(&settings.base_url, "chat/completions");
    let mut messages = Vec::new();
    if let Some(system) = request.system.as_ref().filter(|text| !text.is_empty()) {
        messages.push(json!({ "role": "system", "content": system }));
    }
    for message in &request.messages {
        messages.push(json!({ "role": message.role, "content": message.content }));
    }
    let mut body = json!({
        "model": request.model,
        "messages": messages,
    });
    if let Some(temperature) = request.temperature {
        body["temperature"] = json!(temperature);
    }
    if let Some(max_tokens) = request.max_tokens {
        body["max_tokens"] = json!(max_tokens);
    }
    let headers = vec![("authorization", format!("Bearer {}", settings.api_key))];
    let value = send_json(client, settings, &url, &headers, body, request.timeout).await?;
    let text = value
        .pointer("/choices/0/message/content")
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty())
        .ok_or_else(|| EdgelinkError::invalid_operation("ai provider returned no text"))?;
    Ok(ChatResponse {
        text: text.to_owned(),
        model: value.get("model").and_then(Value::as_str).unwrap_or(&request.model).to_owned(),
        provider: settings.kind.as_str().to_owned(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::Json;
    use axum::Router;
    use axum::http::HeaderMap;
    use axum::routing::post;
    use serde_json::json;
    use tokio::net::TcpListener;

    async fn serve(app: Router) -> (String, tokio::task::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        (format!("http://{addr}"), task)
    }

    fn client() -> Client {
        Client::builder().build().unwrap()
    }

    fn request() -> ChatRequest {
        ChatRequest {
            model: "test-model".into(),
            messages: vec![ChatMessage { role: "user".into(), content: "hello".into() }],
            system: None,
            temperature: Some(0.2),
            max_tokens: Some(32),
            timeout: Duration::from_secs(2),
        }
    }

    #[tokio::test]
    async fn openai_responses_returns_output_text() {
        let app = Router::new().route(
            "/v1/responses",
            post(|headers: HeaderMap, Json(body): Json<Value>| async move {
                assert!(headers.get("authorization").unwrap().to_str().unwrap().starts_with("Bearer "));
                assert_eq!(body["model"], "test-model");
                Json(json!({ "output_text": "hello-openai", "model": "test-model" }))
            }),
        );
        let (base, task) = serve(app).await;
        let settings = ProviderSettings {
            kind: ProviderKind::Openai,
            base_url: format!("{base}/v1"),
            api_key: "k".into(),
            organization: None,
        };
        let reply = complete(&client(), &settings, &request()).await.unwrap();
        assert_eq!(reply.text, "hello-openai");
        assert_eq!(reply.provider, "openai");
        task.abort();
    }

    #[tokio::test]
    async fn xai_uses_the_responses_path() {
        let app = Router::new().route(
            "/v1/responses",
            post(|| async {
                Json(json!({
                    "output": [{ "content": [{ "text": "hello-grok" }] }]
                }))
            }),
        );
        let (base, task) = serve(app).await;
        let settings = ProviderSettings {
            kind: ProviderKind::Xai,
            base_url: format!("{base}/v1"),
            api_key: "k".into(),
            organization: None,
        };
        let reply = complete(&client(), &settings, &request()).await.unwrap();
        assert_eq!(reply.text, "hello-grok");
        assert_eq!(reply.provider, "xai");
        task.abort();
    }

    #[tokio::test]
    async fn anthropic_reads_text_blocks() {
        let app = Router::new().route(
            "/v1/messages",
            post(|headers: HeaderMap, Json(body): Json<Value>| async move {
                assert_eq!(headers.get("anthropic-version").unwrap(), "2023-06-01");
                assert_eq!(body["max_tokens"], 32);
                Json(json!({
                    "model": "claude-test",
                    "content": [{ "type": "text", "text": "hello-claude" }]
                }))
            }),
        );
        let (base, task) = serve(app).await;
        let settings =
            ProviderSettings { kind: ProviderKind::Anthropic, base_url: base, api_key: "k".into(), organization: None };
        let reply = complete(&client(), &settings, &request()).await.unwrap();
        assert_eq!(reply.text, "hello-claude");
        assert_eq!(reply.model, "claude-test");
        task.abort();
    }

    #[tokio::test]
    async fn cortex_reads_chat_completions() {
        let app = Router::new().route(
            "/chat/completions",
            post(|Json(body): Json<Value>| async move {
                assert_eq!(body["messages"][0]["role"], "user");
                Json(json!({
                    "model": "llama3",
                    "choices": [{ "message": { "content": "hello-cortex" } }]
                }))
            }),
        );
        let (base, task) = serve(app).await;
        let settings =
            ProviderSettings { kind: ProviderKind::Cortex, base_url: base, api_key: "k".into(), organization: None };
        let reply = complete(&client(), &settings, &request()).await.unwrap();
        assert_eq!(reply.text, "hello-cortex");
        assert_eq!(reply.provider, "cortex");
        task.abort();
    }

    #[tokio::test]
    async fn provider_errors_do_not_echo_the_key() {
        let app =
            Router::new().route("/v1/responses", post(|| async { (axum::http::StatusCode::UNAUTHORIZED, "nope") }));
        let (base, task) = serve(app).await;
        let settings = ProviderSettings {
            kind: ProviderKind::Openai,
            base_url: format!("{base}/v1"),
            api_key: "super-secret-key".into(),
            organization: None,
        };
        let err = complete(&client(), &settings, &request()).await.unwrap_err();
        assert!(!err.to_string().contains("super-secret-key"), "{err}");
        task.abort();
    }
}
