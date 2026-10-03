//! Global AI provider configuration. The API key lives in `credentials.apiKey`.

use reqwest::Client;
use serde_json::Value;

use crate::EdgelinkError;
use crate::runtime::engine::Engine;
use crate::runtime::flow::Flow;
use crate::runtime::model::json::RedGlobalNodeConfig;
use crate::runtime::nodes::*;
use edgelink_macro::*;

use super::adapter::{ProviderKind, ProviderSettings};

#[global_node("ai-provider", red_name = "ai-provider")]
pub struct AiProviderNode {
    base: BaseGlobalNodeState,
    pub(crate) settings: ProviderSettings,
    pub(crate) default_model: String,
    pub(crate) client: Client,
}

impl AiProviderNode {
    fn build(
        engine: &Engine,
        config: &RedGlobalNodeConfig,
        _options: Option<&config::Config>,
    ) -> crate::Result<Box<dyn GlobalNodeBehavior>> {
        let settings = resolve_provider(&config.rest)?;
        let default_model = json_string(&config.rest, "defaultModel")
            .ok_or_else(|| EdgelinkError::invalid_operation("ai-provider defaultModel is required"))?;
        if default_model.is_empty() {
            return Err(EdgelinkError::invalid_operation("ai-provider defaultModel is required"));
        }
        let node = AiProviderNode {
            base: BaseGlobalNodeState {
                id: config.id,
                name: config.name.clone(),
                type_str: "ai-provider",
                ordering: config.ordering,
                context: engine.get_context_manager().new_context(engine.context(), config.id.to_string()),
                disabled: config.disabled,
            },
            settings,
            default_model,
            client: Client::builder()
                .build()
                .map_err(|err| EdgelinkError::invalid_operation(&format!("ai HTTP client failed: {err}")))?,
        };
        Ok(Box::new(node))
    }
}

#[async_trait::async_trait]
impl GlobalNodeBehavior for AiProviderNode {
    fn get_base(&self) -> &BaseGlobalNodeState {
        &self.base
    }
}

pub(crate) fn provider_from_flow(flow: &Flow, provider_id: &str) -> crate::Result<(ProviderSettings, String, Client)> {
    if provider_id.is_empty() {
        return Err(EdgelinkError::invalid_operation("ai-chat has no provider"));
    }
    let id: crate::runtime::model::ElementId = provider_id
        .parse()
        .map_err(|_| EdgelinkError::invalid_operation(&format!("ai provider id '{provider_id}' is not a node id")))?;
    let engine = flow.engine().ok_or_else(|| EdgelinkError::invalid_operation("ai-chat has no engine"))?;
    let global = engine
        .find_global_node_by_id(&id)
        .ok_or_else(|| EdgelinkError::invalid_operation(&format!("ai provider '{id}' was not loaded")))?;
    let node = global
        .as_any()
        .downcast_ref::<AiProviderNode>()
        .ok_or_else(|| EdgelinkError::invalid_operation(&format!("node '{id}' is not an ai-provider")))?;
    Ok((node.settings.clone(), node.default_model.clone(), node.client.clone()))
}

pub(crate) fn resolve_provider(value: &Value) -> crate::Result<ProviderSettings> {
    reject_unsupported(value)?;
    let kind = ProviderKind::parse(json_string(value, "provider").as_deref().unwrap_or(""))?;
    let base_url = json_string(value, "baseUrl")
        .filter(|text| !text.is_empty())
        .or_else(|| kind.default_base().map(str::to_owned))
        .ok_or_else(|| EdgelinkError::invalid_operation("cortex baseUrl is required"))?;
    let api_key = json_string(value, "apiKey")
        .or_else(|| {
            value.get("credentials").and_then(|creds| json_string(creds, "apiKey")).filter(|text| !text.is_empty())
        })
        .ok_or_else(|| EdgelinkError::invalid_operation("ai-provider apiKey is required"))?;
    if api_key.is_empty() {
        return Err(EdgelinkError::invalid_operation("ai-provider apiKey is required"));
    }
    Ok(ProviderSettings {
        kind,
        base_url: base_url.trim_end_matches('/').to_owned(),
        api_key,
        organization: json_string(value, "organization").filter(|text| !text.is_empty()),
    })
}

fn reject_unsupported(value: &Value) -> crate::Result<()> {
    for key in ["stream", "tools", "functions", "tool_choice", "response_format", "usetls", "tls"] {
        if value.get(key).is_some_and(option_is_set) {
            return Err(EdgelinkError::NotSupported(format!("AI option '{key}' is not supported")));
        }
    }
    Ok(())
}

fn option_is_set(item: &Value) -> bool {
    match item {
        Value::Null => false,
        Value::Bool(false) => false,
        Value::String(text) if text.is_empty() => false,
        _ => true,
    }
}

fn json_string(value: &Value, key: &str) -> Option<String> {
    value.get(key).and_then(Value::as_str).map(str::trim).filter(|text| !text.is_empty()).map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn cortex_requires_a_base_url() {
        let err = resolve_provider(&json!({
            "provider": "cortex",
            "defaultModel": "llama3",
            "credentials": { "apiKey": "k" }
        }))
        .unwrap_err();
        assert!(err.to_string().contains("baseUrl"), "{err}");
    }

    #[test]
    fn streaming_is_rejected() {
        let err = resolve_provider(&json!({
            "provider": "openai",
            "defaultModel": "gpt-4o-mini",
            "stream": true,
            "credentials": { "apiKey": "k" }
        }))
        .unwrap_err();
        assert!(err.to_string().contains("not supported"), "{err}");
    }

    #[test]
    fn missing_key_is_rejected() {
        let err = resolve_provider(&json!({
            "provider": "openai",
            "defaultModel": "gpt-4o-mini"
        }))
        .unwrap_err();
        assert!(err.to_string().contains("apiKey"), "{err}");
    }
}
