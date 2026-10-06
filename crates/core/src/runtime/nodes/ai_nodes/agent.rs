//! Bounded agent/tool loop.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::Deserialize;
use serde_json::Value;
use sha2::{Digest, Sha256};
use tokio_util::sync::CancellationToken;

use crate::EdgelinkError;
use crate::runtime::context::Context;
use crate::runtime::flow::Flow;
use crate::runtime::model::ContextHolder;
use crate::runtime::model::json::RedFlowNodeConfig;
use crate::runtime::model::{MsgHandle, Variant};
use crate::runtime::nodes::*;
use edgelink_macro::*;

use super::adapter::{
    ProviderKind, ToolCall, ToolChatOutput, ToolChatRequest, ToolSpec, TranscriptItem, complete_tools_with_policy,
};
use super::provider::provider_from_flow;
use super::schema::CompiledSchema;
use super::tools::{self, valid_context_key};

const DEFAULT_TIMEOUT_MS: u64 = 30_000;
const MIN_TIMEOUT_MS: u64 = 100;
const MAX_TIMEOUT_MS: u64 = 120_000;

crate::node_hints!("ai-agent", refs = ["provider" => "ai-provider"], caps = ["ai", "network"], input = "string", outs = ["reply" => "string"]);

#[flow_node("ai-agent", red_name = "ai-agent", inputs = 1, outputs = 1)]
struct AiAgentNode {
    base: BaseFlowNodeState,
    config: ResolvedAgent,
    get_schema: CompiledSchema,
    set_schema: CompiledSchema,
}

#[derive(Debug, Deserialize)]
struct AgentConfig {
    #[serde(default)]
    provider: String,
    #[serde(default)]
    model: String,
    #[serde(default)]
    system: String,
    #[serde(default)]
    tools: Vec<String>,
    #[serde(default, rename = "maxTurns", deserialize_with = "empty_as_none_u32")]
    max_turns: Option<u32>,
    #[serde(default, rename = "maxToolCalls", deserialize_with = "empty_as_none_u32")]
    max_tool_calls: Option<u32>,
    #[serde(default, rename = "timeoutMs", deserialize_with = "empty_as_none_u64")]
    timeout_ms: Option<u64>,
    #[serde(default, rename = "maxTokens", deserialize_with = "empty_as_none_u32")]
    max_tokens: Option<u32>,
    #[serde(default, rename = "maxContextChars", deserialize_with = "empty_as_none_u32")]
    max_context_chars: Option<u32>,
    #[serde(default, rename = "maxToolResultChars", deserialize_with = "empty_as_none_u32")]
    max_tool_result_chars: Option<u32>,
    #[serde(default, deserialize_with = "empty_as_none_f64")]
    temperature: Option<f64>,
}

struct ResolvedAgent {
    provider: String,
    model: String,
    system: String,
    tools: Vec<ToolSpec>,
    max_turns: u32,
    max_tool_calls: u32,
    timeout: Duration,
    max_tokens: u32,
    max_context_chars: usize,
    max_tool_result_chars: usize,
    temperature: Option<f64>,
}

impl AiAgentNode {
    fn build(
        flow: &Flow,
        base_node: BaseFlowNodeState,
        config: &RedFlowNodeConfig,
        options: Option<&config::Config>,
    ) -> crate::Result<Box<dyn FlowNodeBehavior>> {
        require_memory_store(options)?;
        let raw = AgentConfig::deserialize(&config.rest)?;
        if raw.provider.trim().is_empty() {
            return Err(EdgelinkError::invalid_operation("ai-agent provider is required"));
        }
        if raw.tools.is_empty() {
            return Err(EdgelinkError::invalid_operation("ai-agent tools are required"));
        }
        if raw.system.len() > 4096 {
            return Err(EdgelinkError::invalid_operation("ai-agent system exceeds 4 KiB"));
        }
        let (settings, default_model, _, _) = provider_from_flow(flow, &raw.provider)?;
        if settings.kind == ProviderKind::Cortex {
            return Err(EdgelinkError::NotSupported("agent tools are not supported for cortex".to_owned()));
        }
        let mut tools = Vec::new();
        for name in &raw.tools {
            tools.push(tools::spec_for(name)?);
        }
        let timeout_ms = raw.timeout_ms.unwrap_or(DEFAULT_TIMEOUT_MS);
        if !(MIN_TIMEOUT_MS..=MAX_TIMEOUT_MS).contains(&timeout_ms) {
            return Err(EdgelinkError::invalid_operation("ai-agent timeoutMs is out of range"));
        }
        let max_turns = raw.max_turns.unwrap_or(4);
        if !(1..=8).contains(&max_turns) {
            return Err(EdgelinkError::invalid_operation("ai-agent maxTurns is out of range"));
        }
        let max_tool_calls = raw.max_tool_calls.unwrap_or(8);
        if !(1..=16).contains(&max_tool_calls) {
            return Err(EdgelinkError::invalid_operation("ai-agent maxToolCalls is out of range"));
        }
        let max_tokens = raw.max_tokens.unwrap_or(1024);
        if !(1..=8192).contains(&max_tokens) {
            return Err(EdgelinkError::invalid_operation("ai-agent maxTokens is out of range"));
        }
        let max_context_chars = raw.max_context_chars.unwrap_or(8000) as usize;
        if !(1..=32000).contains(&max_context_chars) {
            return Err(EdgelinkError::invalid_operation("ai-agent maxContextChars is out of range"));
        }
        let max_tool_result_chars = raw.max_tool_result_chars.unwrap_or(1024) as usize;
        if !(1..=4096).contains(&max_tool_result_chars) {
            return Err(EdgelinkError::invalid_operation("ai-agent maxToolResultChars is out of range"));
        }
        if let Some(temperature) = raw.temperature
            && !(0.0..=2.0).contains(&temperature)
        {
            return Err(EdgelinkError::invalid_operation("ai-agent temperature must be between 0 and 2"));
        }
        let model = if raw.model.trim().is_empty() { default_model } else { raw.model };
        Ok(Box::new(AiAgentNode {
            base: base_node,
            config: ResolvedAgent {
                provider: raw.provider,
                model,
                system: raw.system,
                tools,
                max_turns,
                max_tool_calls,
                timeout: Duration::from_millis(timeout_ms),
                max_tokens,
                max_context_chars,
                max_tool_result_chars,
                temperature: raw.temperature,
            },
            get_schema: CompiledSchema::compile(&tools::context_get_spec().parameters)?,
            set_schema: CompiledSchema::compile(&tools::context_set_spec().parameters)?,
        }))
    }

    async fn handle(&self, msg: MsgHandle, cancel: CancellationToken) -> crate::Result<()> {
        let flow = self.flow().ok_or_else(|| EdgelinkError::invalid_operation("ai-agent has no flow"))?;
        let engine = flow.engine().ok_or_else(|| EdgelinkError::invalid_operation("ai-agent has no engine"))?;
        let slot = engine.agent_slots();
        let deadline = Instant::now() + self.config.timeout;
        let permit = tokio::time::timeout(remaining(deadline), slot.acquire())
            .await
            .map_err(|_| EdgelinkError::invalid_operation("ai-agent concurrency limit"))?
            .map_err(|_| EdgelinkError::invalid_operation("ai-agent concurrency limit"))?;
        let result = self.run_loop(&msg, &flow, deadline, &cancel).await;
        drop(permit);
        result?;
        self.report_status(
            StatusObject { fill: Some(StatusFill::Green), shape: Some(StatusShape::Dot), text: Some("ok".to_owned()) },
            cancel.clone(),
        )
        .await;
        self.fan_out_one(Envelope { port: 0, msg }, cancel).await
    }

    async fn run_loop(
        &self,
        msg: &MsgHandle,
        flow: &Flow,
        deadline: Instant,
        cancel: &CancellationToken,
    ) -> crate::Result<()> {
        let (settings, default_model, client, egress) = provider_from_flow(flow, &self.config.provider)?;
        let model = if self.config.model.is_empty() { default_model } else { self.config.model.clone() };
        let prompt = {
            let guard = msg.read().await;
            payload_text(guard.get("payload"))?
        };
        let mut items = vec![TranscriptItem::UserText(prompt)];
        let mut tool_calls = 0u32;
        let mut denials: HashMap<String, u32> = HashMap::new();
        let mut repeats: HashMap<String, u32> = HashMap::new();
        let egress = egress.snapshot();
        for _turn in 0..self.config.max_turns {
            if cancel.is_cancelled() {
                return Err(EdgelinkError::TaskCancelled);
            }
            let left = remaining(deadline);
            if left < Duration::from_millis(MIN_TIMEOUT_MS) {
                return Err(EdgelinkError::Timeout);
            }
            if transcript_chars(&items) > self.config.max_context_chars {
                return Err(EdgelinkError::invalid_operation("ai-agent maxContextChars exceeded"));
            }
            let request = ToolChatRequest {
                model: model.clone(),
                system: if self.config.system.is_empty() { None } else { Some(self.config.system.clone()) },
                items: items.clone(),
                tools: self.config.tools.clone(),
                temperature: self.config.temperature,
                max_tokens: Some(self.config.max_tokens),
                timeout: left.min(self.config.timeout),
            };
            let output = tokio::select! {
                _ = cancel.cancelled() => return Err(EdgelinkError::TaskCancelled),
                _ = tokio::time::sleep_until(tokio::time::Instant::from_std(deadline)) => return Err(EdgelinkError::Timeout),
                reply = complete_tools_with_policy(&client, &egress, &settings, &request) => reply?,
            };
            match output {
                ToolChatOutput::Text(reply) => {
                    let mut guard = msg.write().await;
                    guard.set("payload".to_owned(), Variant::String(reply.text));
                    guard.set(
                        "ai".to_owned(),
                        Variant::Object(crate::runtime::model::VariantObjectMap::from([
                            ("provider".to_owned(), Variant::String(reply.provider)),
                            ("model".to_owned(), Variant::String(reply.model)),
                        ])),
                    );
                    return Ok(());
                }
                ToolChatOutput::Calls(calls) => {
                    for call in calls {
                        if cancel.is_cancelled() {
                            return Err(EdgelinkError::TaskCancelled);
                        }
                        if tool_calls >= self.config.max_tool_calls {
                            return Err(EdgelinkError::invalid_operation("tool_limit"));
                        }
                        let allow = self.config.tools.iter().any(|tool| tool.name == call.name);
                        if !allow {
                            let count = denials.entry(call.name.clone()).or_insert(0);
                            *count += 1;
                            tool_calls += 1;
                            if *count >= 2 {
                                return Err(EdgelinkError::invalid_operation("tool_denied"));
                            }
                            items.push(TranscriptItem::FunctionCall {
                                call_id: call.call_id.clone(),
                                name: call.name.clone(),
                                arguments_raw: "{}".to_owned(),
                            });
                            items.push(TranscriptItem::FunctionResult {
                                call_id: call.call_id,
                                output: cap("tool denied"),
                            });
                            continue;
                        }
                        let raw = serde_json::to_string(&call.arguments).unwrap_or_else(|_| "{}".to_owned());
                        if raw.len() > 4096 {
                            return Err(EdgelinkError::invalid_operation("tool_args_limit"));
                        }
                        let digest = repeat_hash(&call.name, &call.arguments);
                        let seen = repeats.entry(digest).or_insert(0);
                        *seen += 1;
                        if *seen >= 3 {
                            return Err(EdgelinkError::invalid_operation("repeated_tool_call"));
                        }
                        items.push(TranscriptItem::FunctionCall {
                            call_id: call.call_id.clone(),
                            name: call.name.clone(),
                            arguments_raw: raw,
                        });
                        let output = self.execute_tool(flow, &call).await;
                        tool_calls += 1;
                        if output.len() > self.config.max_tool_result_chars {
                            return Err(EdgelinkError::invalid_operation("tool_result_limit"));
                        }
                        items.push(TranscriptItem::FunctionResult { call_id: call.call_id, output });
                    }
                }
            }
        }
        Err(EdgelinkError::invalid_operation("ai-agent maxTurns exceeded"))
    }

    async fn execute_tool(&self, flow: &Flow, call: &ToolCall) -> String {
        let schema = match call.name.as_str() {
            tools::CONTEXT_GET => &self.get_schema,
            tools::CONTEXT_SET => &self.set_schema,
            _ => return cap("unknown tool"),
        };
        if let Err(err) = schema.validate(&call.arguments) {
            return cap(&err.to_string());
        }
        let scope = call.arguments.get("scope").and_then(Value::as_str).unwrap_or("");
        let key = call.arguments.get("key").and_then(Value::as_str).unwrap_or("");
        if !valid_context_key(key) {
            return cap("invalid context key");
        }
        match call.name.as_str() {
            tools::CONTEXT_GET => match self.context_for(flow, scope) {
                Ok(ctx) => match ctx.get_one(None, key, &[]).await {
                    Some(value) => match serde_json::to_string(&json_ok(Some(value))) {
                        Ok(text) => text,
                        Err(_) => cap("context get failed"),
                    },
                    None => cap(r#"{"ok":false,"error":"missing"}"#),
                },
                Err(err) => cap(&err.to_string()),
            },
            tools::CONTEXT_SET => {
                let Some(raw) = call.arguments.get("value") else {
                    return cap("missing value");
                };
                let encoded = serde_json::to_string(raw).unwrap_or_default();
                if encoded.len() > 4096 {
                    return cap("value exceeds 4 KiB");
                }
                match self.context_for(flow, scope) {
                    Ok(ctx) => {
                        if ctx.set_one(None, key, Some(Variant::from(raw.clone())), &[]).await.is_err() {
                            return cap("context set failed");
                        }
                        r#"{"ok":true}"#.to_owned()
                    }
                    Err(err) => cap(&err.to_string()),
                }
            }
            _ => cap("unknown tool"),
        }
    }

    fn context_for(&self, flow: &Flow, scope: &str) -> crate::Result<Context> {
        match scope {
            "node" => Ok(self.get_base().context().clone()),
            "flow" => Ok(flow.context().clone()),
            "global" => {
                let engine = flow.engine().ok_or_else(|| EdgelinkError::invalid_operation("ai-agent has no engine"))?;
                Ok(engine.context().clone())
            }
            _ => Err(EdgelinkError::invalid_operation("invalid scope")),
        }
    }
}

fn require_memory_store(options: Option<&config::Config>) -> crate::Result<()> {
    let Some(cfg) = options else {
        return Ok(());
    };
    let default = cfg.get_string("runtime.context.default").unwrap_or_else(|_| "memory".to_owned());
    let key = format!("runtime.context.stores.{default}.provider");
    let provider = cfg.get_string(&key).unwrap_or_else(|_| "memory".to_owned());
    if provider != "memory" {
        return Err(EdgelinkError::NotSupported(
            "ai-agent requires the default context store to use the memory provider".to_owned(),
        ));
    }
    Ok(())
}

fn remaining(deadline: Instant) -> Duration {
    deadline.saturating_duration_since(Instant::now())
}

fn transcript_chars(items: &[TranscriptItem]) -> usize {
    items
        .iter()
        .map(|item| match item {
            TranscriptItem::UserText(text)
            | TranscriptItem::AssistantText(text)
            | TranscriptItem::FunctionResult { output: text, .. } => text.chars().count(),
            TranscriptItem::FunctionCall { arguments_raw, .. } => arguments_raw.chars().count(),
        })
        .sum()
}

fn repeat_hash(name: &str, arguments: &Value) -> String {
    let canonical = canonical_json(arguments);
    let mut hasher = Sha256::new();
    hasher.update(name.as_bytes());
    hasher.update([0x1f]);
    hasher.update(canonical.as_bytes());
    format!("{:x}", hasher.finalize())
}

fn canonical_json(value: &Value) -> String {
    match value {
        Value::Object(map) => {
            let mut keys: Vec<_> = map.keys().collect();
            keys.sort();
            let mut out = String::from("{");
            for (i, key) in keys.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                out.push_str(&serde_json::to_string(key).unwrap_or_else(|_| "\"\"".to_owned()));
                out.push(':');
                out.push_str(&canonical_json(&map[key.as_str()]));
            }
            out.push('}');
            out
        }
        Value::Array(list) => {
            let inner: Vec<String> = list.iter().map(canonical_json).collect();
            format!("[{}]", inner.join(","))
        }
        other => serde_json::to_string(other).unwrap_or_else(|_| "null".to_owned()),
    }
}

fn json_ok(value: Option<Variant>) -> Value {
    match value {
        Some(variant) => json_from_variant(&variant),
        None => serde_json::json!({ "ok": false }),
    }
}

fn json_from_variant(value: &Variant) -> Value {
    serde_json::to_value(value).unwrap_or(Value::Null)
}

fn cap(text: &str) -> String {
    text.chars().take(512).collect()
}

fn payload_text(value: Option<&Variant>) -> crate::Result<String> {
    let Some(value) = value else {
        return Err(EdgelinkError::invalid_operation("ai-agent prompt is empty"));
    };
    value
        .as_str()
        .filter(|text| !text.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| EdgelinkError::invalid_operation("ai-agent prompt is empty"))
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
        Some(_) => Err(serde::de::Error::custom("not a number")),
    }
}

#[async_trait::async_trait]
impl FlowNodeBehavior for AiAgentNode {
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
    async fn agent_live() {
        if std::env::var("EDGELINK_AI_LIVE").ok().as_deref() != Some("1") {
            return;
        }
        panic!("set provider keys and replace this stub with live OpenAI and Anthropic agent runs");
    }
}
