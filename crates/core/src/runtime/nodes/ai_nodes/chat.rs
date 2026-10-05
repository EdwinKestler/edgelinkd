//! One-shot and conversational model calls.

use std::sync::Arc;
use std::time::Duration;

use serde::Deserialize;
use serde_json::Value;

use crate::EdgelinkError;
use crate::runtime::flow::Flow;
use crate::runtime::model::json::RedFlowNodeConfig;
use crate::runtime::model::{Msg, MsgHandle, Variant};
use crate::runtime::nodes::*;
use edgelink_macro::*;

use super::adapter::{ChatMessage, ChatRequest, complete_with_policy};
use super::provider::provider_from_flow;

const DEFAULT_TIMEOUT_MS: u64 = 30_000;
const MIN_TIMEOUT_MS: u64 = 100;
const MAX_TIMEOUT_MS: u64 = 120_000;

#[derive(Debug, Deserialize)]
struct ChatConfig {
    #[serde(default)]
    provider: String,
    #[serde(default)]
    model: String,
    #[serde(default)]
    prompt: String,
    #[serde(default)]
    system: String,
    #[serde(default)]
    output: String,
    #[serde(default, deserialize_with = "empty_as_none_f64")]
    temperature: Option<f64>,
    #[serde(default, rename = "maxTokens", deserialize_with = "empty_as_none_u32")]
    max_tokens: Option<u32>,
    #[serde(default, rename = "timeoutMs", deserialize_with = "empty_as_none_u64")]
    timeout_ms: Option<u64>,
}

crate::node_hints!("ai-chat", refs = ["provider" => "ai-provider"], caps = ["ai", "network"]);

#[flow_node("ai-chat", red_name = "ai-chat", inputs = 1, outputs = 1)]
struct AiChatNode {
    base: BaseFlowNodeState,
    config: ResolvedChat,
}

struct ResolvedChat {
    provider: String,
    model: String,
    prompt: String,
    system: String,
    output: String,
    temperature: Option<f64>,
    max_tokens: Option<u32>,
    timeout: Duration,
}

impl AiChatNode {
    fn build(
        _flow: &Flow,
        base_node: BaseFlowNodeState,
        config: &RedFlowNodeConfig,
        _options: Option<&config::Config>,
    ) -> crate::Result<Box<dyn FlowNodeBehavior>> {
        reject_unsupported(&config.rest)?;
        let raw = ChatConfig::deserialize(&config.rest)?;
        if raw.provider.trim().is_empty() {
            return Err(EdgelinkError::invalid_operation("ai-chat provider is required"));
        }
        if let Some(temperature) = raw.temperature
            && !(0.0..=2.0).contains(&temperature)
        {
            return Err(EdgelinkError::invalid_operation("ai-chat temperature must be between 0 and 2"));
        }
        if let Some(0) = raw.max_tokens {
            return Err(EdgelinkError::invalid_operation("ai-chat maxTokens must be greater than 0"));
        }
        let timeout_ms = raw.timeout_ms.unwrap_or(DEFAULT_TIMEOUT_MS);
        if !(MIN_TIMEOUT_MS..=MAX_TIMEOUT_MS).contains(&timeout_ms) {
            return Err(EdgelinkError::invalid_operation("ai-chat timeoutMs is out of range"));
        }
        let output = if raw.output.trim().is_empty() { "payload".to_owned() } else { raw.output };
        Ok(Box::new(AiChatNode {
            base: base_node,
            config: ResolvedChat {
                provider: raw.provider,
                model: raw.model,
                prompt: raw.prompt,
                system: raw.system,
                output,
                temperature: raw.temperature,
                max_tokens: raw.max_tokens,
                timeout: Duration::from_millis(timeout_ms),
            },
        }))
    }

    async fn handle(&self, msg: MsgHandle, cancel: CancellationToken) -> crate::Result<()> {
        let flow = self.flow().ok_or_else(|| EdgelinkError::invalid_operation("ai-chat has no flow"))?;
        let (settings, default_model, client, egress) = provider_from_flow(&flow, &self.config.provider)?;
        let model = if self.config.model.trim().is_empty() { default_model } else { self.config.model.clone() };
        if model.is_empty() {
            return Err(EdgelinkError::invalid_operation("ai-chat model is required"));
        }
        let (system, messages) = {
            let guard = msg.read().await;
            self.messages_from(&guard)?
        };
        let request = ChatRequest {
            model: model.clone(),
            messages,
            system,
            temperature: self.config.temperature,
            max_tokens: self.config.max_tokens,
            timeout: self.config.timeout,
        };
        let egress = egress.snapshot();
        tokio::select! {
            _ = cancel.cancelled() => Err(EdgelinkError::TaskCancelled),
            reply = complete_with_policy(&client, &egress, &settings, &request) => {
                let reply = reply?;
                let mut guard = msg.write().await;
                guard.set(self.config.output.clone(), Variant::String(reply.text));
                guard.set("ai".to_owned(), ai_meta(&reply.provider, &reply.model));
                drop(guard);
                self.report_status(
                    StatusObject {
                        fill: Some(StatusFill::Green),
                        shape: Some(StatusShape::Dot),
                        text: Some("ok".to_owned()),
                    },
                    cancel.clone(),
                )
                .await;
                self.fan_out_one(Envelope { port: 0, msg }, cancel).await
            }
        }
    }

    fn messages_from(&self, msg: &Msg) -> crate::Result<(Option<String>, Vec<ChatMessage>)> {
        let system = if self.config.system.trim().is_empty() { None } else { Some(self.config.system.clone()) };
        if let Some(list) = msg.get("messages").and_then(array_messages) {
            return Ok((system, list?));
        }
        if let Some(payload) = msg.get("payload")
            && let Some(list) = array_messages(payload)
        {
            return Ok((system, list?));
        }
        let text = if !self.config.prompt.trim().is_empty() {
            self.config.prompt.clone()
        } else {
            payload_text(msg.get("payload"))?
        };
        Ok((system, vec![ChatMessage { role: "user".into(), content: text }]))
    }
}

fn ai_meta(provider: &str, model: &str) -> Variant {
    Variant::Object(crate::runtime::model::VariantObjectMap::from([
        ("provider".to_owned(), Variant::String(provider.to_owned())),
        ("model".to_owned(), Variant::String(model.to_owned())),
    ]))
}

fn payload_text(value: Option<&Variant>) -> crate::Result<String> {
    let Some(value) = value else {
        return Err(EdgelinkError::invalid_operation("ai-chat prompt is empty"));
    };
    if value.is_null() {
        return Err(EdgelinkError::invalid_operation("ai-chat prompt is empty"));
    }
    if let Some(text) = value.as_str() {
        if text.is_empty() {
            return Err(EdgelinkError::invalid_operation("ai-chat prompt is empty"));
        }
        return Ok(text.to_owned());
    }
    value.to_string().map_err(|err| EdgelinkError::invalid_operation(&format!("ai-chat prompt is not text: {err}")))
}

fn array_messages(value: &Variant) -> Option<crate::Result<Vec<ChatMessage>>> {
    let object_list = value.as_array()?;
    if object_list.is_empty() {
        return None;
    }
    let mut messages = Vec::new();
    for item in object_list {
        let Some(object) = item.as_object() else {
            return Some(Err(EdgelinkError::invalid_operation("ai-chat messages must be objects")));
        };
        let role = object.get("role").and_then(Variant::as_str).unwrap_or("user");
        let content = match object.get("content") {
            Some(content) => match content.as_str() {
                Some(text) => text.to_owned(),
                None => return Some(Err(EdgelinkError::invalid_operation("ai-chat message content must be text"))),
            },
            None => return Some(Err(EdgelinkError::invalid_operation("ai-chat message content is required"))),
        };
        messages.push(ChatMessage { role: role.to_owned(), content });
    }
    Some(Ok(messages))
}

fn empty_as_none_f64<'de, D>(deserializer: D) -> Result<Option<f64>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = Option::<Value>::deserialize(deserializer)?;
    match value {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(text)) if text.trim().is_empty() => Ok(None),
        Some(Value::String(text)) => text.parse().map(Some).map_err(serde::de::Error::custom),
        Some(Value::Number(number)) => {
            number.as_f64().map(Some).ok_or_else(|| serde::de::Error::custom("not a number"))
        }
        Some(_) => Err(serde::de::Error::custom("temperature is not a number")),
    }
}

fn empty_as_none_u32<'de, D>(deserializer: D) -> Result<Option<u32>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Ok(empty_as_none_u64(deserializer)?.map(|value| value as u32))
}

fn empty_as_none_u64<'de, D>(deserializer: D) -> Result<Option<u64>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = Option::<Value>::deserialize(deserializer)?;
    match value {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(text)) if text.trim().is_empty() => Ok(None),
        Some(Value::String(text)) => text.parse().map(Some).map_err(serde::de::Error::custom),
        Some(Value::Number(number)) => {
            number.as_u64().map(Some).ok_or_else(|| serde::de::Error::custom("not a number"))
        }
        Some(_) => Err(serde::de::Error::custom("not a number")),
    }
}

fn reject_unsupported(value: &Value) -> crate::Result<()> {
    for key in ["stream", "tools", "functions", "tool_choice", "response_format"] {
        if value.get(key).is_some_and(|item| !item.is_null() && item != &Value::Bool(false)) {
            return Err(EdgelinkError::NotSupported(format!("AI option '{key}' is not supported")));
        }
    }
    Ok(())
}

#[async_trait::async_trait]
impl FlowNodeBehavior for AiChatNode {
    fn get_base(&self) -> &BaseFlowNodeState {
        &self.base
    }

    async fn run(self: Arc<Self>, stop_token: CancellationToken) {
        while !stop_token.is_cancelled() {
            let cancel = stop_token.child_token();
            with_uow(self.as_ref(), cancel.child_token(), |node, msg| async move { node.handle(msg, cancel).await })
                .await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::Json;
    use axum::Router;
    use axum::routing::post;
    use serde::Deserialize;
    use serde_json::json;
    use tokio::net::TcpListener;

    async fn openai_mock() -> (String, tokio::task::JoinHandle<()>) {
        let app = Router::new().route(
            "/v1/responses",
            post(|Json(body): Json<Value>| async move {
                assert_eq!(body["model"], "test-model");
                Json(json!({ "output_text": "hello-openai", "model": "test-model" }))
            }),
        );
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        (format!("http://{addr}/v1"), task)
    }

    #[tokio::test]
    async fn a_chat_node_writes_the_reply_to_payload() {
        let (base, task) = openai_mock().await;
        let flows = json!([
            { "id": "100", "type": "tab" },
            {
                "id": "b1",
                "type": "ai-provider",
                "provider": "openai",
                "baseUrl": base,
                "defaultModel": "test-model",
                "credentials": { "apiKey": "secret-key" }
            },
            { "id": "1", "z": "100", "type": "ai-chat", "provider": "b1", "wires": [["2"]] },
            { "id": "2", "z": "100", "type": "test-once" }
        ]);
        let engine = crate::runtime::engine::build_test_engine(flows).unwrap();
        let injected: Vec<(crate::runtime::model::ElementId, Msg)> =
            Vec::deserialize(json!([["1", { "payload": "hello" }]])).unwrap();
        let msgs = engine.run_once_with_inject(1, Duration::from_secs(2), injected).await.unwrap();
        assert_eq!(msgs[0].get("payload").and_then(Variant::as_str), Some("hello-openai"));
        assert_eq!(msgs[0].get_nav("ai.provider").and_then(Variant::as_str), Some("openai"));
        assert!(!format!("{:?}", msgs[0]).contains("secret-key"));
        task.abort();
    }

    #[tokio::test]
    async fn streaming_chat_is_rejected_at_deploy() {
        let flows = json!([
            { "id": "100", "type": "tab" },
            {
                "id": "b1",
                "type": "ai-provider",
                "provider": "openai",
                "defaultModel": "test-model",
                "credentials": { "apiKey": "k" }
            },
            { "id": "1", "z": "100", "type": "ai-chat", "provider": "b1", "stream": true, "wires": [[]] }
        ]);
        let err = crate::runtime::engine::build_test_engine(flows).unwrap_err();
        assert!(err.to_string().contains("not supported"), "{err}");
    }
}
