//! Local structured-output validator.

use std::sync::Arc;

use serde::Deserialize;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::EdgelinkError;
use crate::runtime::flow::Flow;
use crate::runtime::model::json::RedFlowNodeConfig;
use crate::runtime::model::{MsgHandle, Variant};
use crate::runtime::nodes::*;
use edgelink_macro::*;

use super::schema::CompiledSchema;

crate::node_hints!("ai-structured", caps = ["ai"], input = "string|object", outs = ["parsed" => "object"]);

#[flow_node("ai-structured", red_name = "ai-structured", inputs = 1, outputs = 1)]
struct AiStructuredNode {
    base: BaseFlowNodeState,
    config: ResolvedStructured,
}

#[derive(Debug, Deserialize)]
struct StructuredConfig {
    #[serde(default)]
    property: String,
    #[serde(default, rename = "schemaSource")]
    schema_source: String,
    #[serde(default)]
    schema: Value,
    #[serde(default)]
    output: String,
}

struct ResolvedStructured {
    property: String,
    schema_source: SchemaSource,
    schema: Option<CompiledSchema>,
    output: String,
}

enum SchemaSource {
    Node,
    Msg,
}

impl AiStructuredNode {
    fn build(
        _flow: &Flow,
        base_node: BaseFlowNodeState,
        config: &RedFlowNodeConfig,
        _options: Option<&config::Config>,
    ) -> crate::Result<Box<dyn FlowNodeBehavior>> {
        let raw = StructuredConfig::deserialize(&config.rest)?;
        let property = if raw.property.trim().is_empty() { "payload".to_owned() } else { raw.property };
        let output = if raw.output.trim().is_empty() { "payload".to_owned() } else { raw.output };
        let source = match raw.schema_source.trim() {
            "" | "node" => SchemaSource::Node,
            "msg" => SchemaSource::Msg,
            other => {
                return Err(EdgelinkError::invalid_operation(&format!(
                    "ai-structured schemaSource '{other}' is not supported"
                )));
            }
        };
        let schema = match source {
            SchemaSource::Node => {
                let compiled = compile_field(&raw.schema)?;
                Some(compiled)
            }
            SchemaSource::Msg => {
                if !raw.schema.is_null()
                    && raw.schema != Value::String(String::new())
                    && raw.schema != json_empty_object()
                {
                    return Err(EdgelinkError::invalid_operation(
                        "ai-structured schema must be empty when schemaSource is msg",
                    ));
                }
                None
            }
        };
        Ok(Box::new(AiStructuredNode {
            base: base_node,
            config: ResolvedStructured { property, schema_source: source, schema, output },
        }))
    }

    async fn handle(&self, msg: MsgHandle, cancel: CancellationToken) -> crate::Result<()> {
        let (parsed, drop_schema) = {
            let guard = msg.read().await;
            let input = guard
                .get(&self.config.property)
                .ok_or_else(|| EdgelinkError::invalid_operation("ai-structured input is empty"))?;
            let parsed = parse_input(input)?;
            match &self.config.schema_source {
                SchemaSource::Node => {
                    if guard.get("schema").is_some() {
                        return Err(EdgelinkError::invalid_operation(
                            "ai-structured refuses mixed node and msg schema",
                        ));
                    }
                    self.config.schema.as_ref().expect("node schema").validate(&parsed)?;
                    (parsed, false)
                }
                SchemaSource::Msg => {
                    let raw = guard
                        .get("schema")
                        .ok_or_else(|| EdgelinkError::invalid_operation("ai-structured msg.schema is required"))?;
                    let object = variant_to_json(raw)?;
                    CompiledSchema::compile(&object)?.validate(&parsed)?;
                    (parsed, true)
                }
            }
        };
        let mut guard = msg.write().await;
        if drop_schema {
            guard.remove("schema");
        }
        guard.set(self.config.output.clone(), Variant::from(parsed));
        drop(guard);
        self.report_status(
            StatusObject { fill: Some(StatusFill::Green), shape: Some(StatusShape::Dot), text: Some("ok".to_owned()) },
            cancel.clone(),
        )
        .await;
        self.fan_out_one(Envelope { port: 0, msg }, cancel).await
    }
}

fn json_empty_object() -> Value {
    Value::Object(serde_json::Map::new())
}

fn compile_field(schema: &Value) -> crate::Result<CompiledSchema> {
    match schema {
        Value::Null => Err(EdgelinkError::invalid_operation("ai-structured schema is required")),
        Value::String(s) if s.is_empty() => Err(EdgelinkError::invalid_operation("ai-structured schema is required")),
        Value::String(text) => {
            let parsed: Value = serde_json::from_str(text)
                .map_err(|err| EdgelinkError::invalid_operation(&format!("ai-structured schema is not JSON: {err}")))?;
            CompiledSchema::compile(&parsed)
        }
        Value::Object(_) => CompiledSchema::compile(schema),
        _ => Err(EdgelinkError::invalid_operation("ai-structured schema must be an object or JSON string")),
    }
}

fn parse_input(value: &Variant) -> crate::Result<Value> {
    if let Some(text) = value.as_str() {
        return serde_json::from_str(text)
            .map_err(|err| EdgelinkError::invalid_operation(&format!("ai-structured payload is not JSON: {err}")));
    }
    if value.as_object().is_some() || value.as_array().is_some() {
        return variant_to_json(value);
    }
    Err(EdgelinkError::invalid_operation("ai-structured input must be a JSON string, object, or array"))
}

fn variant_to_json(value: &Variant) -> crate::Result<Value> {
    serde_json::to_value(value).map_err(|err| EdgelinkError::invalid_operation(&err.to_string()))
}

#[async_trait::async_trait]
impl FlowNodeBehavior for AiStructuredNode {
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
    use serde::Deserialize;
    use serde_json::json;

    #[tokio::test]
    async fn validates_a_json_string_payload() {
        let flows = json!([
            { "id": "100", "type": "tab" },
            {
                "id": "1",
                "z": "100",
                "type": "ai-structured",
                "schema": { "type": "object", "properties": { "ok": { "type": "boolean" } }, "required": ["ok"], "additionalProperties": false },
                "wires": [["2"]]
            },
            { "id": "2", "z": "100", "type": "test-once" }
        ]);
        let engine = crate::runtime::engine::build_test_engine(flows).unwrap();
        let injected: Vec<(crate::runtime::model::ElementId, crate::runtime::model::Msg)> =
            Vec::deserialize(json!([["1", { "payload": "{\"ok\":true}" }]])).unwrap();
        let msgs = engine.run_once_with_inject(1, std::time::Duration::from_secs(2), injected).await.unwrap();
        assert_eq!(msgs.len(), 1);
        let payload = msgs[0].get("payload").and_then(Variant::as_object).expect("object");
        assert_eq!(payload.get("ok"), Some(&Variant::Bool(true)));
    }

    #[test]
    fn mixed_schema_source_msg_requires_empty_node_schema() {
        let flows = json!([
            { "id": "100", "type": "tab" },
            { "id": "1", "z": "100", "type": "ai-structured", "schemaSource": "msg", "schema": { "type": "string" }, "wires": [[]] }
        ]);
        assert!(crate::runtime::engine::build_test_engine(flows).is_err());
    }
}
