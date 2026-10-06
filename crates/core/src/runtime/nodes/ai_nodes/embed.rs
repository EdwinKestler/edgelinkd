//! Embeddings through an existing `ai-provider`.

use std::sync::Arc;
use std::time::Duration;

use serde::Deserialize;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::EdgelinkError;
use crate::runtime::flow::Flow;
use crate::runtime::model::json::RedFlowNodeConfig;
use crate::runtime::model::{MsgHandle, Variant};
use crate::runtime::nodes::*;
use edgelink_macro::*;

use super::adapter::{EmbedRequest, ProviderKind, embed_with_policy};
use super::provider::provider_from_flow;

const DEFAULT_TIMEOUT_MS: u64 = 30_000;
const MIN_TIMEOUT_MS: u64 = 100;
const MAX_TIMEOUT_MS: u64 = 120_000;

crate::node_hints!("ai-embed", refs = ["provider" => "ai-provider"], caps = ["ai", "network"], input = "string|array", outs = ["embedding" => "array"]);

#[flow_node("ai-embed", red_name = "ai-embed", inputs = 1, outputs = 1)]
struct AiEmbedNode {
    base: BaseFlowNodeState,
    config: ResolvedEmbed,
}

#[derive(Debug, Deserialize)]
struct EmbedConfig {
    #[serde(default)]
    provider: String,
    #[serde(default)]
    model: String,
    #[serde(default, deserialize_with = "empty_as_none_u32")]
    dimensions: Option<u32>,
    #[serde(default, rename = "timeoutMs", deserialize_with = "empty_as_none_u64")]
    timeout_ms: Option<u64>,
    #[serde(default)]
    property: String,
}

struct ResolvedEmbed {
    provider: String,
    model: String,
    dimensions: Option<u32>,
    timeout: Duration,
    property: String,
}

impl AiEmbedNode {
    fn build(
        flow: &Flow,
        base_node: BaseFlowNodeState,
        config: &RedFlowNodeConfig,
        _options: Option<&config::Config>,
    ) -> crate::Result<Box<dyn FlowNodeBehavior>> {
        reject_unsupported(&config.rest)?;
        let raw = EmbedConfig::deserialize(&config.rest)?;
        if raw.provider.trim().is_empty() {
            return Err(EdgelinkError::invalid_operation("ai-embed provider is required"));
        }
        if raw.model.trim().is_empty() {
            return Err(EdgelinkError::invalid_operation("ai-embed model is required"));
        }
        let (settings, _, _, _) = provider_from_flow(flow, &raw.provider)?;
        match settings.kind {
            ProviderKind::Openai | ProviderKind::Xai => {}
            other => {
                return Err(EdgelinkError::NotSupported(format!(
                    "embeddings are not supported for provider '{}'",
                    other.as_str()
                )));
            }
        }
        if raw.dimensions.is_some() && settings.kind == ProviderKind::Xai {
            return Err(EdgelinkError::NotSupported(
                "xAI embeddings do not accept dimensions in this build".to_owned(),
            ));
        }
        if let Some(dimensions) = raw.dimensions
            && !(1..=4096).contains(&dimensions)
        {
            return Err(EdgelinkError::invalid_operation("ai-embed dimensions is out of range"));
        }
        let timeout_ms = raw.timeout_ms.unwrap_or(DEFAULT_TIMEOUT_MS);
        if !(MIN_TIMEOUT_MS..=MAX_TIMEOUT_MS).contains(&timeout_ms) {
            return Err(EdgelinkError::invalid_operation("ai-embed timeoutMs is out of range"));
        }
        let property = if raw.property.trim().is_empty() { "payload".to_owned() } else { raw.property };
        Ok(Box::new(AiEmbedNode {
            base: base_node,
            config: ResolvedEmbed {
                provider: raw.provider,
                model: raw.model,
                dimensions: raw.dimensions,
                timeout: Duration::from_millis(timeout_ms),
                property,
            },
        }))
    }

    async fn handle(&self, msg: MsgHandle, cancel: CancellationToken) -> crate::Result<()> {
        let flow = self.flow().ok_or_else(|| EdgelinkError::invalid_operation("ai-embed has no flow"))?;
        let (settings, _, client, egress) = provider_from_flow(&flow, &self.config.provider)?;
        let input = {
            let guard = msg.read().await;
            read_input(guard.get(&self.config.property))?
        };
        let request = EmbedRequest {
            model: self.config.model.clone(),
            input,
            dimensions: self.config.dimensions,
            timeout: self.config.timeout,
        };
        let egress = egress.snapshot();
        tokio::select! {
            _ = cancel.cancelled() => Err(EdgelinkError::TaskCancelled),
            reply = embed_with_policy(&client, &egress, &settings, &request) => {
                let reply = reply?;
                let payload = if reply.vectors.len() == 1 {
                    vector_variant(&reply.vectors[0])
                } else {
                    Variant::Array(reply.vectors.iter().map(|row| vector_variant(row)).collect())
                };
                let mut guard = msg.write().await;
                guard.set(self.config.property.clone(), payload);
                guard.set("ai".to_owned(), embed_meta(&reply));
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
}

fn embed_meta(reply: &super::adapter::EmbedResponse) -> Variant {
    let mut object = crate::runtime::model::VariantObjectMap::new();
    object.insert("provider".to_owned(), Variant::String(reply.provider.clone()));
    object.insert("model".to_owned(), Variant::String(reply.model.clone()));
    if let Some(tokens) = reply.prompt_tokens {
        object.insert("promptTokens".to_owned(), Variant::from(tokens));
    }
    Variant::Object(object)
}

fn vector_variant(row: &[f64]) -> Variant {
    Variant::Array(
        row.iter().map(|n| serde_json::Number::from_f64(*n).map(Variant::Number).unwrap_or(Variant::Null)).collect(),
    )
}

fn read_input(value: Option<&Variant>) -> crate::Result<Vec<String>> {
    let Some(value) = value else {
        return Err(EdgelinkError::invalid_operation("ai-embed input is empty"));
    };
    if let Some(text) = value.as_str() {
        if text.is_empty() {
            return Err(EdgelinkError::invalid_operation("ai-embed input is empty"));
        }
        if text.chars().count() > 8192 {
            return Err(EdgelinkError::invalid_operation("ai-embed input exceeds 8192 characters"));
        }
        return Ok(vec![text.to_owned()]);
    }
    let Some(list) = value.as_array() else {
        return Err(EdgelinkError::invalid_operation("ai-embed input must be a string or array of strings"));
    };
    if list.is_empty() || list.len() > 32 {
        return Err(EdgelinkError::invalid_operation("ai-embed batch must have 1 to 32 strings"));
    }
    let mut out = Vec::new();
    let mut bytes = 0usize;
    for item in list {
        let text = item.as_str().ok_or_else(|| EdgelinkError::invalid_operation("ai-embed batch must be strings"))?;
        if text.is_empty() {
            return Err(EdgelinkError::invalid_operation("ai-embed input is empty"));
        }
        if text.chars().count() > 8192 {
            return Err(EdgelinkError::invalid_operation("ai-embed input exceeds 8192 characters"));
        }
        bytes = bytes.saturating_add(text.len());
        if bytes > 64 * 1024 {
            return Err(EdgelinkError::invalid_operation("ai-embed batch exceeds 64 KiB"));
        }
        out.push(text.to_owned());
    }
    Ok(out)
}

fn reject_unsupported(value: &Value) -> crate::Result<()> {
    for key in ["stream", "encoding_format", "encodingFormat"] {
        if value.get(key).is_some_and(|item| !item.is_null() && item != &Value::Bool(false) && item != "") {
            return Err(EdgelinkError::NotSupported(format!("AI option '{key}' is not supported")));
        }
    }
    Ok(())
}

fn empty_as_none_u32<'de, D>(deserializer: D) -> Result<Option<u32>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = Option::<Value>::deserialize(deserializer)?;
    match value {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(text)) if text.trim().is_empty() => Ok(None),
        Some(Value::String(text)) => text.parse().map(Some).map_err(serde::de::Error::custom),
        Some(Value::Number(number)) => {
            number.as_u64().map(|n| Some(n as u32)).ok_or_else(|| serde::de::Error::custom("not a number"))
        }
        Some(_) => Err(serde::de::Error::custom("not a number")),
    }
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

#[async_trait::async_trait]
impl FlowNodeBehavior for AiEmbedNode {
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
    #[tokio::test]
    async fn embed_live() {
        if std::env::var("EDGELINK_AI_LIVE").ok().as_deref() != Some("1") {
            return;
        }
        panic!("set provider keys and replace this stub with a live OpenAI and xAI embedding call");
    }
}
