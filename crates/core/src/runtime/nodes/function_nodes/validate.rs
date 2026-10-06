//! Checks one message property, then either forwards the message or raises a node error.
//!
//! `check` is `type`, `range`, or `age`.
//! `type` requires `expect`: string, number, boolean, object, array, buffer, or null.
//! `range` requires a number and at least one of `min` and `max`, inclusive.
//! `age` requires `maxAgeMs`. The property is Unix milliseconds. Older than that limit is an error.
//! Any other check, or a missing bound, is rejected when the flow is deployed.

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Deserialize;

use crate::N2linkError;
use crate::runtime::flow::Flow;
use crate::runtime::model::Variant;
use crate::runtime::model::json::RedFlowNodeConfig;
use crate::runtime::nodes::*;
use n2link_macro::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Check {
    Type,
    Range,
    Age,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Expect {
    String,
    Number,
    Boolean,
    Object,
    Array,
    Buffer,
    Null,
}

#[derive(Debug)]
struct ValidateConfig {
    property: String,
    check: Check,
    expect: Option<Expect>,
    min: Option<f64>,
    max: Option<f64>,
    max_age_ms: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct RawConfig {
    property: String,
    check: String,
    #[serde(default)]
    expect: Option<String>,
    #[serde(default)]
    min: Option<f64>,
    #[serde(default)]
    max: Option<f64>,
    #[serde(default, rename = "maxAgeMs")]
    max_age_ms: Option<u64>,
}

fn compile(raw: RawConfig) -> crate::Result<ValidateConfig> {
    if raw.property.is_empty() {
        return Err(N2linkError::invalid_operation("validate property is empty"));
    }
    let check = match raw.check.as_str() {
        "type" => Check::Type,
        "range" => Check::Range,
        "age" => Check::Age,
        other => return Err(N2linkError::NotSupported(format!("validate check '{other}' is not supported"))),
    };
    let expect = match (check, raw.expect.as_deref()) {
        (Check::Type, Some("string")) => Some(Expect::String),
        (Check::Type, Some("number")) => Some(Expect::Number),
        (Check::Type, Some("boolean")) => Some(Expect::Boolean),
        (Check::Type, Some("object")) => Some(Expect::Object),
        (Check::Type, Some("array")) => Some(Expect::Array),
        (Check::Type, Some("buffer")) => Some(Expect::Buffer),
        (Check::Type, Some("null")) => Some(Expect::Null),
        (Check::Type, Some(other)) => {
            return Err(N2linkError::NotSupported(format!("validate type '{other}' is not supported")));
        }
        (Check::Type, None) => {
            return Err(N2linkError::invalid_operation("validate type check has no expect"));
        }
        _ => None,
    };
    if check == Check::Range && raw.min.is_none() && raw.max.is_none() {
        return Err(N2linkError::invalid_operation("validate range has no min or max"));
    }
    if check == Check::Age && raw.max_age_ms.is_none() {
        return Err(N2linkError::invalid_operation("validate age has no maxAgeMs"));
    }
    Ok(ValidateConfig { property: raw.property, check, expect, min: raw.min, max: raw.max, max_age_ms: raw.max_age_ms })
}

fn type_name(value: &Variant) -> &'static str {
    match value {
        Variant::Null => "null",
        Variant::Number(_) => "number",
        Variant::String(_) => "string",
        Variant::Bool(_) => "boolean",
        Variant::Object(_) => "object",
        Variant::Array(_) => "array",
        Variant::Bytes(_) => "buffer",
        Variant::Date(_) => "date",
        Variant::Regexp(_) => "regexp",
    }
}

fn number_of(value: &Variant) -> Option<f64> {
    match value {
        Variant::Number(number) => number.as_f64().or_else(|| number.as_i64().map(|item| item as f64)),
        _ => None,
    }
}

fn matches_type(value: &Variant, expect: Expect) -> bool {
    matches!(
        (value, expect),
        (Variant::String(_), Expect::String)
            | (Variant::Number(_), Expect::Number)
            | (Variant::Bool(_), Expect::Boolean)
            | (Variant::Object(_), Expect::Object)
            | (Variant::Array(_), Expect::Array)
            | (Variant::Bytes(_), Expect::Buffer)
            | (Variant::Null, Expect::Null)
    )
}

/// `now_ms` is Unix milliseconds. Tests pass a fixed clock.
fn judge(config: &ValidateConfig, value: Option<&Variant>, now_ms: u64) -> crate::Result<()> {
    let Some(value) = value else {
        return Err(N2linkError::InvalidOperation(format!("validate property '{}' is missing", config.property)));
    };
    match config.check {
        Check::Type => {
            let expect =
                config.expect.ok_or_else(|| N2linkError::invalid_operation("validate type check has no expect"))?;
            if matches_type(value, expect) {
                Ok(())
            } else {
                Err(N2linkError::InvalidOperation(format!(
                    "validate property '{}' is {}, expected {}",
                    config.property,
                    type_name(value),
                    expect_name(expect)
                )))
            }
        }
        Check::Range => {
            let Some(number) = number_of(value) else {
                return Err(N2linkError::InvalidOperation(format!(
                    "validate property '{}' is not a number",
                    config.property
                )));
            };
            if config.min.is_some_and(|min| number < min) || config.max.is_some_and(|max| number > max) {
                return Err(N2linkError::InvalidOperation(format!(
                    "validate property '{}' is outside the range",
                    config.property
                )));
            }
            Ok(())
        }
        Check::Age => {
            let limit =
                config.max_age_ms.ok_or_else(|| N2linkError::invalid_operation("validate age has no maxAgeMs"))?;
            let Some(stamp) = number_of(value) else {
                return Err(N2linkError::InvalidOperation(format!(
                    "validate property '{}' is not a timestamp",
                    config.property
                )));
            };
            if stamp < 0.0 {
                return Err(N2linkError::InvalidOperation(format!(
                    "validate property '{}' is not a timestamp",
                    config.property
                )));
            }
            let stamp = stamp as u64;
            let age = now_ms.saturating_sub(stamp);
            if age > limit {
                Err(N2linkError::InvalidOperation(format!(
                    "validate property '{}' is older than {limit} ms",
                    config.property
                )))
            } else {
                Ok(())
            }
        }
    }
}

fn expect_name(expect: Expect) -> &'static str {
    match expect {
        Expect::String => "string",
        Expect::Number => "number",
        Expect::Boolean => "boolean",
        Expect::Object => "object",
        Expect::Array => "array",
        Expect::Buffer => "buffer",
        Expect::Null => "null",
    }
}

fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|elapsed| elapsed.as_millis() as u64).unwrap_or(0)
}

#[flow_node("validate", red_name = "validate")]
struct ValidateNode {
    base: BaseFlowNodeState,
    config: ValidateConfig,
}

impl ValidateNode {
    fn build(
        _flow: &Flow,
        base_node: BaseFlowNodeState,
        config: &RedFlowNodeConfig,
        _options: Option<&config::Config>,
    ) -> crate::Result<Box<dyn FlowNodeBehavior>> {
        let raw = RawConfig::deserialize(&config.rest)?;
        Ok(Box::new(ValidateNode { base: base_node, config: compile(raw)? }))
    }
}

#[async_trait]
impl FlowNodeBehavior for ValidateNode {
    fn get_base(&self) -> &BaseFlowNodeState {
        &self.base
    }

    async fn run(self: Arc<Self>, stop_token: CancellationToken) {
        while !stop_token.is_cancelled() {
            let cancel = stop_token.child_token();
            let step_cancel = cancel.clone();
            with_uow(self.as_ref(), cancel, move |node, msg| async move {
                let found = {
                    let guard = msg.read().await;
                    guard.get_nav_stripped(&node.config.property).cloned()
                };
                judge(&node.config, found.as_ref(), now_ms())?;
                node.fan_out_one(Envelope { port: 0, msg }, step_cancel).await
            })
            .await;
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use serde::Deserialize;
    use serde_json::json;

    use super::*;
    use crate::runtime::engine::build_test_engine;
    use crate::runtime::model::Msg;

    fn rule(check: &str, extra: serde_json::Value) -> ValidateConfig {
        let mut raw = json!({ "property": "payload", "check": check });
        if let (serde_json::Value::Object(base), serde_json::Value::Object(more)) = (&mut raw, extra) {
            for (key, value) in more {
                base.insert(key, value);
            }
        }
        compile(RawConfig::deserialize(&raw).unwrap()).unwrap()
    }

    #[test]
    fn an_unknown_check_is_rejected() {
        let raw = json!({ "property": "payload", "check": "schema" });
        let err = compile(RawConfig::deserialize(&raw).unwrap()).unwrap_err();
        assert!(err.to_string().contains("not supported"));
        assert!(err.to_string().contains("schema"));
    }

    #[test]
    fn type_range_and_age_judge_the_value() {
        let typed = rule("type", json!({ "expect": "number" }));
        assert!(judge(&typed, Some(&Variant::from(1)), 0).is_ok());
        assert!(judge(&typed, Some(&Variant::from("1")), 0).unwrap_err().to_string().contains("string"));

        let ranged = rule("range", json!({ "min": 0.0, "max": 10.0 }));
        assert!(judge(&ranged, Some(&Variant::from(10)), 0).is_ok());
        assert!(judge(&ranged, Some(&Variant::from(11)), 0).unwrap_err().to_string().contains("range"));

        let aged = rule("age", json!({ "maxAgeMs": 1000 }));
        assert!(judge(&aged, Some(&Variant::from(5_000)), 5_500).is_ok());
        assert!(judge(&aged, Some(&Variant::from(1_000)), 5_500).unwrap_err().to_string().contains("older"));
        assert!(judge(&aged, None, 5_500).unwrap_err().to_string().contains("missing"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_value_inside_the_range_is_forwarded() {
        let flows = json!([
            { "id": "100", "type": "tab" },
            {
                "id": "1", "z": "100", "type": "validate", "property": "payload",
                "check": "range", "min": 0, "max": 10, "wires": [["2"]]
            },
            { "id": "2", "z": "100", "type": "test-once" }
        ]);
        let engine = build_test_engine(flows).unwrap();
        let mut msg = Msg::default();
        msg.set("payload".to_string(), Variant::from(4));
        let inject = vec![("1".parse().unwrap(), msg)];
        let msgs = engine.run_once_with_inject(1, Duration::from_secs(2), inject).await.unwrap();
        assert_eq!(msgs[0]["payload"], Variant::from(4));
    }
}
