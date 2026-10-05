//! One request/response contract. Each provider maps it onto its HTTP API.

use std::time::Duration;

use reqwest::Client;
use serde_json::{Value, json};

use crate::EdgelinkError;
use crate::runtime::egress::{EgressPolicy, EgressPurpose};

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

    pub(crate) fn as_str(self) -> &'static str {
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

#[derive(Clone)]
pub(crate) struct ProviderSettings {
    pub kind: ProviderKind,
    pub base_url: String,
    pub api_key: String,
    pub organization: Option<String>,
}

impl std::fmt::Debug for ProviderSettings {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProviderSettings")
            .field("kind", &self.kind)
            .field("base_url", &self.base_url)
            .field("api_key", &"***")
            .field("organization", &self.organization)
            .finish()
    }
}

impl std::fmt::Display for ProviderSettings {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "ProviderSettings {{ kind: {:?}, base_url: {}, api_key: *** }}", self.kind, self.base_url)
    }
}

#[cfg(test)]
pub(crate) async fn complete(
    client: &Client,
    settings: &ProviderSettings,
    request: &ChatRequest,
) -> crate::Result<ChatResponse> {
    complete_inner(client, None, settings, request).await
}

pub(crate) async fn complete_with_policy(
    client: &Client,
    policy: &EgressPolicy,
    settings: &ProviderSettings,
    request: &ChatRequest,
) -> crate::Result<ChatResponse> {
    if policy.mode() == crate::runtime::egress::EgressMode::Off {
        complete_inner(client, None, settings, request).await
    } else {
        complete_inner(client, Some(policy), settings, request).await
    }
}

async fn complete_inner(
    client: &Client,
    policy: Option<&EgressPolicy>,
    settings: &ProviderSettings,
    request: &ChatRequest,
) -> crate::Result<ChatResponse> {
    match settings.kind {
        ProviderKind::Openai | ProviderKind::Xai => responses_complete(client, policy, settings, request).await,
        ProviderKind::Anthropic => anthropic_complete(client, policy, settings, request).await,
        ProviderKind::Cortex => chat_completions_complete(client, policy, settings, request).await,
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
    policy: Option<&EgressPolicy>,
    settings: &ProviderSettings,
    url: &str,
    headers: &[(&str, String)],
    body: Value,
    timeout: Duration,
) -> crate::Result<Value> {
    let governed;
    let client = if let Some(policy) = policy {
        governed = policy.http_client(EgressPurpose::AiProvider, url).await?;
        &governed
    } else {
        client
    };
    let timeout = policy.map_or(timeout, |policy| timeout.min(policy.request_timeout()));
    let mut req = client.post(url).timeout(timeout).header("content-type", "application/json");
    for (name, value) in headers {
        req = req.header(*name, value);
    }
    let response = req.json(&body).send().await.map_err(|err| {
        if policy.is_some() {
            EdgelinkError::invalid_operation("ai request failed")
        } else {
            EdgelinkError::invalid_operation(&hide_secret(&format!("ai request failed: {err}"), &settings.api_key))
        }
    })?;
    let status = response.status();
    let text = if let Some(policy) = policy {
        let mut response = response;
        let mut bytes = Vec::new();
        while let Some(chunk) = tokio::time::timeout(policy.idle_timeout(), response.chunk())
            .await
            .map_err(|_| EdgelinkError::Timeout)?
            .map_err(|_| EdgelinkError::invalid_operation("ai response read failed"))?
        {
            if bytes.len().saturating_add(chunk.len()) > policy.max_response_bytes() {
                return Err(EdgelinkError::invalid_operation("ai response exceeds configured limit"));
            }
            bytes.extend_from_slice(&chunk);
        }
        String::from_utf8(bytes).map_err(|_| EdgelinkError::invalid_operation("ai response is not UTF-8"))?
    } else {
        response
            .text()
            .await
            .map_err(|err| EdgelinkError::invalid_operation(&hide_secret(&err.to_string(), &settings.api_key)))?
    };
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
    policy: Option<&EgressPolicy>,
    settings: &ProviderSettings,
    request: &ChatRequest,
) -> crate::Result<ChatResponse> {
    let url = join_url(&settings.base_url, "responses");
    let mut body = json!({
        "model": request.model,
        "input": user_messages_as_input(request),
        "store": false,
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
    let value = send_json(client, policy, settings, &url, &headers, body, request.timeout).await?;
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
    policy: Option<&EgressPolicy>,
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
    let value = send_json(client, policy, settings, &url, &headers, body, request.timeout).await?;
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
    policy: Option<&EgressPolicy>,
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
    let value = send_json(client, policy, settings, &url, &headers, body, request.timeout).await?;
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

#[cfg(feature = "nodes_ai_embeddings")]
#[derive(Clone, Debug)]
pub(crate) struct EmbedRequest {
    pub model: String,
    pub input: Vec<String>,
    pub dimensions: Option<u32>,
    pub timeout: Duration,
}

#[cfg(feature = "nodes_ai_embeddings")]
#[derive(Clone, Debug)]
pub(crate) struct EmbedResponse {
    pub vectors: Vec<Vec<f64>>,
    pub model: String,
    pub provider: String,
    pub prompt_tokens: Option<u32>,
}

#[cfg(feature = "nodes_ai_agent")]
#[derive(Clone, Debug)]
pub(crate) struct ToolSpec {
    pub name: &'static str,
    pub description: &'static str,
    pub parameters: Value,
}

#[cfg(feature = "nodes_ai_agent")]
#[derive(Clone, Debug)]
pub(crate) enum TranscriptItem {
    UserText(String),
    #[allow(dead_code)]
    AssistantText(String),
    FunctionCall {
        call_id: String,
        name: String,
        arguments_raw: String,
    },
    FunctionResult {
        call_id: String,
        output: String,
    },
}

#[cfg(feature = "nodes_ai_agent")]
#[derive(Clone, Debug)]
pub(crate) struct ToolChatRequest {
    pub model: String,
    pub system: Option<String>,
    pub items: Vec<TranscriptItem>,
    pub tools: Vec<ToolSpec>,
    pub temperature: Option<f64>,
    pub max_tokens: Option<u32>,
    pub timeout: Duration,
}

#[cfg(feature = "nodes_ai_agent")]
#[derive(Clone, Debug)]
pub(crate) enum ToolChatOutput {
    Text(ChatResponse),
    Calls(Vec<ToolCall>),
}

#[cfg(feature = "nodes_ai_agent")]
#[derive(Clone, Debug)]
pub(crate) struct ToolCall {
    pub call_id: String,
    pub name: String,
    pub arguments: Value,
}

fn retryable(err: &EdgelinkError) -> bool {
    matches!(err, EdgelinkError::Timeout) || {
        let text = err.to_string();
        text.contains("ai request failed")
            || text.contains("returned 429")
            || text.contains("returned 500")
            || text.contains("returned 502")
            || text.contains("returned 503")
            || text.contains("returned 504")
    }
}

#[cfg(feature = "nodes_ai_embeddings")]
pub(crate) async fn embed_with_policy(
    client: &Client,
    policy: &crate::runtime::egress::EgressPolicy,
    settings: &ProviderSettings,
    request: &EmbedRequest,
) -> crate::Result<EmbedResponse> {
    match settings.kind {
        ProviderKind::Openai | ProviderKind::Xai => {}
        ProviderKind::Anthropic | ProviderKind::Cortex => {
            return Err(EdgelinkError::NotSupported(format!(
                "embeddings are not supported for provider '{}'",
                settings.kind.as_str()
            )));
        }
    }
    let policy = if policy.mode() == crate::runtime::egress::EgressMode::Off { None } else { Some(policy) };
    let mut last = embed_once(client, policy, settings, request).await;
    if let Err(err) = &last
        && retryable(err)
    {
        last = embed_once(client, policy, settings, request).await;
    }
    last
}

#[cfg(feature = "nodes_ai_embeddings")]
async fn embed_once(
    client: &Client,
    policy: Option<&crate::runtime::egress::EgressPolicy>,
    settings: &ProviderSettings,
    request: &EmbedRequest,
) -> crate::Result<EmbedResponse> {
    let url = join_url(&settings.base_url, "embeddings");
    let mut body = json!({
        "model": request.model,
        "input": request.input,
    });
    if let Some(dimensions) = request.dimensions {
        if settings.kind == ProviderKind::Xai {
            return Err(EdgelinkError::NotSupported(
                "xAI embeddings do not accept dimensions in this build".to_owned(),
            ));
        }
        body["dimensions"] = json!(dimensions);
    }
    let mut headers = vec![("authorization", format!("Bearer {}", settings.api_key))];
    if let Some(org) = &settings.organization {
        headers.push(("openai-organization", org.clone()));
    }
    let value = send_json(client, policy, settings, &url, &headers, body, request.timeout).await?;
    parse_embed(value, request, settings)
}

#[cfg(feature = "nodes_ai_embeddings")]
fn parse_embed(value: Value, request: &EmbedRequest, settings: &ProviderSettings) -> crate::Result<EmbedResponse> {
    let data = value
        .get("data")
        .and_then(Value::as_array)
        .ok_or_else(|| EdgelinkError::invalid_operation("ai embedding response has no data"))?;
    let mut indexed: Vec<(u64, Vec<f64>)> = Vec::new();
    for item in data {
        let index = item.get("index").and_then(Value::as_u64).unwrap_or(indexed.len() as u64);
        let embedding = item
            .get("embedding")
            .and_then(Value::as_array)
            .ok_or_else(|| EdgelinkError::invalid_operation("ai embedding is missing"))?;
        if embedding.len() > 4096 {
            return Err(EdgelinkError::invalid_operation("ai embedding exceeds 4096 dimensions"));
        }
        let mut vector = Vec::with_capacity(embedding.len());
        for component in embedding {
            let number =
                component.as_f64().ok_or_else(|| EdgelinkError::invalid_operation("ai embedding is not float"))?;
            vector.push(number);
        }
        indexed.push((index, vector));
    }
    indexed.sort_by_key(|(index, _)| *index);
    let vectors: Vec<Vec<f64>> = indexed.into_iter().map(|(_, v)| v).collect();
    if vectors.len() != request.input.len() {
        return Err(EdgelinkError::invalid_operation("ai embedding count does not match input"));
    }
    let prompt_tokens = value.pointer("/usage/prompt_tokens").and_then(Value::as_u64).map(|n| n as u32);
    Ok(EmbedResponse {
        vectors,
        model: value.get("model").and_then(Value::as_str).unwrap_or(&request.model).to_owned(),
        provider: settings.kind.as_str().to_owned(),
        prompt_tokens,
    })
}

#[cfg(feature = "nodes_ai_agent")]
pub(crate) async fn complete_tools_with_policy(
    client: &Client,
    policy: &crate::runtime::egress::EgressPolicy,
    settings: &ProviderSettings,
    request: &ToolChatRequest,
) -> crate::Result<ToolChatOutput> {
    match settings.kind {
        ProviderKind::Openai | ProviderKind::Xai | ProviderKind::Anthropic => {}
        ProviderKind::Cortex => {
            return Err(EdgelinkError::NotSupported("agent tools are not supported for cortex".to_owned()));
        }
    }
    let policy = if policy.mode() == crate::runtime::egress::EgressMode::Off { None } else { Some(policy) };
    let mut last = tools_once(client, policy, settings, request).await;
    if let Err(err) = &last
        && retryable(err)
    {
        last = tools_once(client, policy, settings, request).await;
    }
    last
}

#[cfg(feature = "nodes_ai_agent")]
async fn tools_once(
    client: &Client,
    policy: Option<&crate::runtime::egress::EgressPolicy>,
    settings: &ProviderSettings,
    request: &ToolChatRequest,
) -> crate::Result<ToolChatOutput> {
    match settings.kind {
        ProviderKind::Openai | ProviderKind::Xai => responses_tools(client, policy, settings, request).await,
        ProviderKind::Anthropic => anthropic_tools(client, policy, settings, request).await,
        ProviderKind::Cortex => Err(EdgelinkError::NotSupported("agent tools are not supported for cortex".to_owned())),
    }
}

#[cfg(feature = "nodes_ai_agent")]
async fn responses_tools(
    client: &Client,
    policy: Option<&crate::runtime::egress::EgressPolicy>,
    settings: &ProviderSettings,
    request: &ToolChatRequest,
) -> crate::Result<ToolChatOutput> {
    let url = join_url(&settings.base_url, "responses");
    let mut input = Vec::new();
    if let Some(system) = request.system.as_ref().filter(|text| !text.is_empty()) {
        input.push(json!({ "role": "system", "content": system }));
    }
    for item in &request.items {
        match item {
            TranscriptItem::UserText(text) => input.push(json!({ "role": "user", "content": text })),
            TranscriptItem::AssistantText(text) => input.push(json!({ "role": "assistant", "content": text })),
            TranscriptItem::FunctionCall { call_id, name, arguments_raw } => input.push(json!({
                "type": "function_call",
                "call_id": call_id,
                "name": name,
                "arguments": arguments_raw,
            })),
            TranscriptItem::FunctionResult { call_id, output } => input.push(json!({
                "type": "function_call_output",
                "call_id": call_id,
                "output": output,
            })),
        }
    }
    let tools: Vec<Value> = request
        .tools
        .iter()
        .map(|tool| {
            json!({
                "type": "function",
                "name": tool.name,
                "description": tool.description,
                "parameters": tool.parameters,
            })
        })
        .collect();
    let mut body = json!({
        "model": request.model,
        "store": false,
        "max_output_tokens": request.max_tokens.unwrap_or(1024),
        "tools": tools,
        "input": input,
    });
    if let Some(temperature) = request.temperature {
        body["temperature"] = json!(temperature);
    }
    let mut headers = vec![("authorization", format!("Bearer {}", settings.api_key))];
    if let Some(org) = &settings.organization {
        headers.push(("openai-organization", org.clone()));
    }
    let value = send_json(client, policy, settings, &url, &headers, body, request.timeout).await?;
    parse_responses_tools(&value, request, settings)
}

#[cfg(feature = "nodes_ai_agent")]
fn parse_responses_tools(
    value: &Value,
    request: &ToolChatRequest,
    settings: &ProviderSettings,
) -> crate::Result<ToolChatOutput> {
    let mut calls = Vec::new();
    if let Some(output) = value.get("output").and_then(Value::as_array) {
        for item in output {
            if item.get("type").and_then(Value::as_str) == Some("function_call") {
                let call_id = item
                    .get("call_id")
                    .and_then(Value::as_str)
                    .ok_or_else(|| EdgelinkError::invalid_operation("tool call is missing call_id"))?
                    .to_owned();
                let name = item.get("name").and_then(Value::as_str).unwrap_or("").to_owned();
                let raw = item.get("arguments").and_then(Value::as_str).unwrap_or("{}");
                let arguments: Value = serde_json::from_str(raw).unwrap_or_else(|_| json!({}));
                calls.push(ToolCall { call_id, name, arguments });
            }
        }
    }
    if !calls.is_empty() {
        return Ok(ToolChatOutput::Calls(calls));
    }
    let text = responses_text(value)?;
    Ok(ToolChatOutput::Text(ChatResponse {
        text,
        model: value.get("model").and_then(Value::as_str).unwrap_or(&request.model).to_owned(),
        provider: settings.kind.as_str().to_owned(),
    }))
}

#[cfg(feature = "nodes_ai_agent")]
async fn anthropic_tools(
    client: &Client,
    policy: Option<&crate::runtime::egress::EgressPolicy>,
    settings: &ProviderSettings,
    request: &ToolChatRequest,
) -> crate::Result<ToolChatOutput> {
    let url = join_url(&settings.base_url, "v1/messages");
    let mut messages = Vec::new();
    for item in &request.items {
        match item {
            TranscriptItem::UserText(text) => messages.push(json!({ "role": "user", "content": text })),
            TranscriptItem::AssistantText(text) => messages.push(json!({ "role": "assistant", "content": text })),
            TranscriptItem::FunctionCall { call_id, name, arguments_raw } => {
                let input: Value = serde_json::from_str(arguments_raw).unwrap_or_else(|_| json!({}));
                messages.push(json!({
                    "role": "assistant",
                    "content": [{ "type": "tool_use", "id": call_id, "name": name, "input": input }]
                }));
            }
            TranscriptItem::FunctionResult { call_id, output } => {
                messages.push(json!({
                    "role": "user",
                    "content": [{ "type": "tool_result", "tool_use_id": call_id, "content": output }]
                }));
            }
        }
    }
    let tools: Vec<Value> = request
        .tools
        .iter()
        .map(|tool| {
            json!({
                "name": tool.name,
                "description": tool.description,
                "input_schema": tool.parameters,
            })
        })
        .collect();
    let mut body = json!({
        "model": request.model,
        "max_tokens": request.max_tokens.unwrap_or(1024),
        "tools": tools,
        "messages": messages,
    });
    if let Some(system) = request.system.as_ref().filter(|text| !text.is_empty()) {
        body["system"] = json!(system);
    }
    if let Some(temperature) = request.temperature {
        body["temperature"] = json!(temperature);
    }
    let headers = vec![("x-api-key", settings.api_key.clone()), ("anthropic-version", "2023-06-01".to_owned())];
    let value = send_json(client, policy, settings, &url, &headers, body, request.timeout).await?;
    parse_anthropic_tools(&value, request, settings)
}

#[cfg(feature = "nodes_ai_agent")]
fn parse_anthropic_tools(
    value: &Value,
    request: &ToolChatRequest,
    settings: &ProviderSettings,
) -> crate::Result<ToolChatOutput> {
    let mut calls = Vec::new();
    let mut text = String::new();
    if let Some(content) = value.get("content").and_then(Value::as_array) {
        for part in content {
            match part.get("type").and_then(Value::as_str) {
                Some("tool_use") => {
                    let call_id = part
                        .get("id")
                        .and_then(Value::as_str)
                        .ok_or_else(|| EdgelinkError::invalid_operation("tool_use is missing id"))?
                        .to_owned();
                    let name = part.get("name").and_then(Value::as_str).unwrap_or("").to_owned();
                    let arguments = part.get("input").cloned().unwrap_or_else(|| json!({}));
                    calls.push(ToolCall { call_id, name, arguments });
                }
                Some("text") => {
                    if let Some(piece) = part.get("text").and_then(Value::as_str) {
                        text.push_str(piece);
                    }
                }
                _ => {}
            }
        }
    }
    if !calls.is_empty() {
        return Ok(ToolChatOutput::Calls(calls));
    }
    if text.is_empty() {
        return Err(EdgelinkError::invalid_operation("ai provider returned no text"));
    }
    Ok(ToolChatOutput::Text(ChatResponse {
        text,
        model: value.get("model").and_then(Value::as_str).unwrap_or(&request.model).to_owned(),
        provider: settings.kind.as_str().to_owned(),
    }))
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
                assert_eq!(body["store"], false);
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

    #[tokio::test]
    async fn governed_provider_response_is_bounded() {
        let app = Router::new().route(
            "/v1/responses",
            post(|| async { Json(json!({ "output_text": "this response is deliberately too large" })) }),
        );
        let (base, task) = serve(app).await;
        let port = url::Url::parse(&base).unwrap().port().unwrap();
        let cfg = config::Config::builder()
            .add_source(config::File::from_str(
                &format!(
                    r#"
                    [egress]
                    mode = "enforce"
                    max_response_bytes = 12

                    [[egress.allow]]
                    protocols = ["http"]
                    host = "127.0.0.1"
                    ports = [{port}]
                    "#
                ),
                config::FileFormat::Toml,
            ))
            .build()
            .unwrap();
        let policy = EgressPolicy::load(Some(&cfg)).unwrap();
        let settings = ProviderSettings {
            kind: ProviderKind::Openai,
            base_url: format!("{base}/v1"),
            api_key: "secret-not-in-error".into(),
            organization: None,
        };
        let err = complete_with_policy(&client(), &policy, &settings, &request()).await.unwrap_err();
        assert!(err.to_string().contains("configured limit"), "{err}");
        assert!(!err.to_string().contains("secret-not-in-error"), "{err}");
        task.abort();
    }

    #[tokio::test]
    async fn governed_provider_request_timeout_is_enforced() {
        let app = Router::new().route(
            "/v1/responses",
            post(|| async {
                tokio::time::sleep(Duration::from_millis(100)).await;
                Json(json!({ "output_text": "late" }))
            }),
        );
        let (base, task) = serve(app).await;
        let port = url::Url::parse(&base).unwrap().port().unwrap();
        let cfg = config::Config::builder()
            .add_source(config::File::from_str(
                &format!(
                    r#"
                    [egress]
                    mode = "enforce"
                    request_timeout_ms = 20

                    [[egress.allow]]
                    protocols = ["http"]
                    host = "127.0.0.1"
                    ports = [{port}]
                    "#
                ),
                config::FileFormat::Toml,
            ))
            .build()
            .unwrap();
        let policy = EgressPolicy::load(Some(&cfg)).unwrap();
        let settings = ProviderSettings {
            kind: ProviderKind::Openai,
            base_url: format!("{base}/v1"),
            api_key: "secret-not-in-error".into(),
            organization: None,
        };
        let err = complete_with_policy(&client(), &policy, &settings, &request()).await.unwrap_err();
        assert_eq!(err.to_string(), "ai request failed");
        task.abort();
    }
}
