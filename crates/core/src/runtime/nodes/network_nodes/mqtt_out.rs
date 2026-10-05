// Licensed under the Apache License, Version 2.0
// Copyright EdgeLink contributors
// Based on Node-RED 10-mqtt.js MQTT Out node

//! MQTT Out Node
//!
//! This node is compatible with Node-RED's MQTT Out node. It can:
//! - Publish messages to an MQTT broker
//! - Support dynamic topic and QoS from incoming messages
//! - Handle connect/disconnect actions
//! - Validate topics for publishing (no wildcards)
//! - Convert various payload types to MQTT message format
//!
//! Configuration:
//! - `broker`: Broker configuration node ID
//! - `topic`: Topic. A non-empty node topic wins over the message
//! - `qos`: QoS level. A node value of 1 or 2 wins. An empty node value uses `msg.qos`
//! - `retain`: Retain flag. A node value of true wins
//! - MQTT 5.0 publish properties when the broker is protocol 5. A set node value wins
//!
//! Message properties:
//! - `msg.topic`: Topic to publish to when the node topic is empty
//! - `msg.payload`: Payload to publish (required unless action is specified)
//! - `msg.qos`: QoS level when the node QoS is 0. Any other value is a node error
//! - `msg.retain`: Retain flag when the node retain is false
//! - `msg.action`: Special actions ("connect", "disconnect")
//! - On a protocol 5 broker: `responseTopic`, `correlationData`, `contentType`,
//!   `userProperties`, `messageExpiryInterval`
//!
//! Behavior matches Node-RED where this runtime supports the option:
//! - If no payload property exists, the message passes through without publishing
//! - An invalid topic or an illegal QoS is a node error. The task keeps running
//! - Supports JSON stringification for objects and arrays
//! - A publish while the broker is down is an error. There is no offline buffer

use std::sync::Arc;

use serde::Deserialize;

use super::mqtt_broker::{self, BrokerSession, PublishProps, qos_of};
use crate::EdgelinkError;
use crate::runtime::flow::Flow;
use crate::runtime::nodes::*;
use edgelink_macro::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MqttQoS {
    AtMost = 0,
    AtLeast = 1,
    Exactly = 2,
}

fn default_out_qos() -> MqttQoS {
    MqttQoS::AtMost
}

fn deserialize_out_qos<'de, D>(deserializer: D) -> Result<MqttQoS, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = serde_json::Value::deserialize(deserializer)?;
    Ok(match qos_of(Some(&value), 0) {
        rumqttc::QoS::AtMostOnce => MqttQoS::AtMost,
        rumqttc::QoS::AtLeastOnce => MqttQoS::AtLeast,
        rumqttc::QoS::ExactlyOnce => MqttQoS::Exactly,
    })
}

fn deserialize_retain<'de, D>(deserializer: D) -> Result<bool, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = serde_json::Value::deserialize(deserializer)?;
    Ok(match value {
        serde_json::Value::Bool(flag) => flag,
        serde_json::Value::String(text) => text == "true",
        serde_json::Value::Number(number) => number.as_u64().unwrap_or(0) != 0,
        _ => false,
    })
}

fn deserialize_optional_u32<'de, D>(deserializer: D) -> Result<Option<u32>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = serde_json::Value::deserialize(deserializer)?;
    match value {
        serde_json::Value::Null => Ok(None),
        serde_json::Value::String(text) if text.trim().is_empty() => Ok(None),
        serde_json::Value::Number(number) => Ok(number.as_u64().and_then(|n| u32::try_from(n).ok())),
        serde_json::Value::String(text) => Ok(text.trim().parse().ok()),
        _ => Ok(None),
    }
}

#[derive(Debug, Clone, Deserialize)]
struct MqttOutNodeConfig {
    /// MQTT broker connection ID (reference to broker config node)
    broker: String,

    /// Default topic to publish to (can be overridden by message)
    #[serde(default)]
    topic: String,

    /// Default QoS level. An empty editor value is 0.
    #[serde(default = "default_out_qos", deserialize_with = "deserialize_out_qos")]
    qos: MqttQoS,

    /// Default retain flag. An empty editor value is false.
    #[serde(default, deserialize_with = "deserialize_retain")]
    retain: bool,

    /// Response topic for MQTT v5
    #[serde(rename = "respTopic", default)]
    response_topic: String,

    /// Correlation data for MQTT v5
    #[serde(rename = "correl", default)]
    correlation_data: String,

    /// Content type for MQTT v5
    #[serde(rename = "contentType", default)]
    content_type: String,

    /// Message expiry interval for MQTT v5
    #[serde(rename = "expiry", default, deserialize_with = "deserialize_optional_u32")]
    message_expiry_interval: Option<u32>,

    /// User properties for MQTT v5 (JSON string)
    #[serde(rename = "userProps", default)]
    user_properties: String,
}

crate::node_hints!("mqtt out", refs = ["broker" => "mqtt-broker"], caps = ["network"]);

#[flow_node("mqtt out", red_name = "mqtt", inputs = 1, outputs = 0)]
struct MqttOutNode {
    base: BaseFlowNodeState,
    config: MqttOutNodeConfig,
    session: BrokerSession,
}

impl std::fmt::Debug for MqttOutNode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MqttOutNode").field("id", &self.base.id).field("topic", &self.config.topic).finish()
    }
}

impl MqttOutNode {
    fn build(
        flow: &Flow,
        base_node: BaseFlowNodeState,
        config: &RedFlowNodeConfig,
        _options: Option<&config::Config>,
    ) -> crate::Result<Box<dyn FlowNodeBehavior>> {
        let mqtt_config = MqttOutNodeConfig::deserialize(&config.rest)?;
        let session = mqtt_broker::attach_broker(flow, &mqtt_config.broker)?;
        reject_publish_config(&mqtt_config, session.options().is_v5())?;
        Ok(Box::new(MqttOutNode { base: base_node, config: mqtt_config, session }))
    }

    async fn publish_message(&self, msg: &Msg) -> crate::Result<()> {
        let Some(payload) = msg.get("payload") else {
            return Ok(());
        };
        let topic = planned_topic(&self.config.topic, msg.get("topic").and_then(|value| value.as_str()));
        if topic.is_empty() {
            return Err(EdgelinkError::invalid_operation("No topic specified for MQTT publish"));
        }
        if !valid_publish_topic(&topic) {
            return Err(EdgelinkError::invalid_operation(&format!("Invalid topic for publishing: '{topic}'")));
        }
        let qos = planned_qos(self.config.qos, msg.get("qos"))?;
        let retain = planned_retain(self.config.retain, msg.get("retain"));
        let mut props = config_props(&self.config)?;
        merge_message_props(&mut props, msg, self.session.options().is_v5())?;
        let payload_bytes = self.convert_payload_to_bytes(payload)?;
        self.session.publish(&topic, qos, retain, payload_bytes, props).await
    }

    /// Convert payload to bytes following Node-RED rules
    fn convert_payload_to_bytes(&self, payload: &Variant) -> crate::Result<Vec<u8>> {
        match payload {
            Variant::Null => Ok(Vec::new()),
            Variant::String(s) => Ok(s.as_bytes().to_vec()),
            Variant::Bytes(bytes) => Ok(bytes.clone()),
            Variant::Number(n) => Ok(n.to_string().into_bytes()),
            Variant::Bool(b) => Ok(b.to_string().into_bytes()),
            Variant::Object(_) | Variant::Array(_) => {
                // For objects and arrays, stringify to JSON
                serde_json::to_vec(payload)
                    .map_err(|e| crate::EdgelinkError::invalid_operation(&format!("Failed to serialize payload: {e}")))
            }
            Variant::Date(d) => {
                // Convert SystemTime to ISO 8601 string
                match d.duration_since(std::time::UNIX_EPOCH) {
                    Ok(duration) => {
                        let timestamp = duration.as_secs();
                        Ok(format!("{timestamp}Z").into_bytes())
                    }
                    Err(_) => Ok("Invalid Date".to_string().into_bytes()),
                }
            }
            Variant::Regexp(r) => Ok(format!("/{r}/").into_bytes()),
        }
    }

    async fn handle_action(&self, msg: &Msg) -> crate::Result<()> {
        if let Some(action) = msg.get("action").and_then(|v| v.as_str()) {
            match action {
                "connect" => {
                    if msg.get("broker").is_some() {
                        return Err(EdgelinkError::NotSupported(
                            "a broker supplied on the message is not supported".to_owned(),
                        ));
                    }
                    let force = msg.get("force").and_then(|value| value.as_bool()).unwrap_or(false);
                    if force {
                        self.session.disconnect().await?;
                    }
                    self.session.connect().await?;
                    log::info!("MQTT connection established");
                }
                "disconnect" => {
                    self.session.disconnect().await?;
                    log::info!("MQTT disconnected");
                }
                _ => {
                    return Err(crate::EdgelinkError::invalid_operation(&format!(
                        "Invalid MQTT action: '{action}'. Valid actions are 'connect' and 'disconnect'"
                    )));
                }
            }
            Ok(())
        } else {
            // No action, this is a publish request - follow Node-RED doPublish logic
            self.publish_message(msg).await
        }
    }
}

#[async_trait]
impl FlowNodeBehavior for MqttOutNode {
    fn get_base(&self) -> &BaseFlowNodeState {
        &self.base
    }

    async fn run(self: Arc<Self>, stop_token: CancellationToken) {
        self.session.retain_user();
        let painter = self.clone();
        let paint_stop = stop_token.clone();
        tokio::spawn(async move {
            mqtt_broker::paint_connection(&painter.session, painter.as_ref(), &paint_stop).await;
        });
        if self.session.options().auto_connect()
            && let Err(err) = self.session.connect().await
        {
            log::warn!("MQTT out connection failed: {err}");
        }
        while !stop_token.is_cancelled() {
            let node = self.clone();

            with_uow(node.as_ref(), stop_token.clone(), |node, msg| async move {
                let msg_guard = msg.read().await;
                node.handle_action(&msg_guard).await
            })
            .await;
        }

        self.session.release_user().await;
        log::debug!("MqttOutNode process() task has been terminated.");
    }
}

fn planned_topic(node_topic: &str, message_topic: Option<&str>) -> String {
    if !node_topic.is_empty() { node_topic.to_owned() } else { message_topic.unwrap_or("").to_owned() }
}

/// Node QoS 1 and 2 win. Node QoS 0 is the editor's empty value, so the message is used.
/// A message QoS outside 0, 1, and 2 is an error.
fn planned_qos(node_qos: MqttQoS, message_qos: Option<&Variant>) -> crate::Result<rumqttc::QoS> {
    match node_qos {
        MqttQoS::AtLeast => Ok(rumqttc::QoS::AtLeastOnce),
        MqttQoS::Exactly => Ok(rumqttc::QoS::ExactlyOnce),
        MqttQoS::AtMost => match message_qos {
            Some(value) => message_qos_value(value),
            None => Ok(rumqttc::QoS::AtMostOnce),
        },
    }
}

fn message_qos_value(value: &Variant) -> crate::Result<rumqttc::QoS> {
    let level = match value {
        Variant::Null => return Ok(rumqttc::QoS::AtMostOnce),
        Variant::String(text) if text.trim().is_empty() => return Ok(rumqttc::QoS::AtMostOnce),
        Variant::Number(number) => number.as_u64(),
        Variant::String(text) => text.trim().parse::<u64>().ok(),
        _ => None,
    };
    match level {
        Some(0) => Ok(rumqttc::QoS::AtMostOnce),
        Some(1) => Ok(rumqttc::QoS::AtLeastOnce),
        Some(2) => Ok(rumqttc::QoS::ExactlyOnce),
        Some(other) => Err(EdgelinkError::invalid_operation(&format!("MQTT qos {other} is out of range"))),
        None => Err(EdgelinkError::invalid_operation("MQTT qos is out of range")),
    }
}

fn planned_retain(node_retain: bool, message_retain: Option<&Variant>) -> bool {
    if node_retain {
        return true;
    }
    message_retain.is_some_and(retain_flag)
}

fn retain_flag(value: &Variant) -> bool {
    match value {
        Variant::Bool(flag) => *flag,
        Variant::String(text) => text == "true",
        Variant::Number(number) => number.as_u64().unwrap_or(0) != 0,
        _ => false,
    }
}

fn valid_publish_topic(topic: &str) -> bool {
    !topic.is_empty()
        && !topic
            .chars()
            .any(|ch| matches!(ch, '+' | '#' | '\u{0008}' | '\u{000c}' | '\n' | '\r' | '\t' | '\u{000b}' | '\0'))
}

/// Editor blanks (`""`, `"0"`, `"{}"`) are not a configured value.
fn configured_text(value: &str) -> Option<String> {
    let text = value.trim();
    if text.is_empty() || text == "0" || text == "{}" { None } else { Some(text.to_owned()) }
}

fn reject_publish_config(config: &MqttOutNodeConfig, v5: bool) -> crate::Result<()> {
    let user_set = configured_text(&config.user_properties).is_some();
    let properties = configured_text(&config.response_topic).is_some()
        || configured_text(&config.correlation_data).is_some()
        || configured_text(&config.content_type).is_some()
        || config.message_expiry_interval.unwrap_or(0) != 0
        || user_set;
    if !v5 {
        return if properties {
            Err(EdgelinkError::NotSupported("MQTT v5 publish properties are not supported".to_owned()))
        } else {
            Ok(())
        };
    }
    if user_set {
        parse_user_props_text(&config.user_properties)?;
    }
    Ok(())
}

fn config_props(config: &MqttOutNodeConfig) -> crate::Result<PublishProps> {
    let user_properties = match configured_text(&config.user_properties) {
        Some(_) => parse_user_props_text(&config.user_properties)?,
        None => Vec::new(),
    };
    Ok(PublishProps {
        response_topic: configured_text(&config.response_topic),
        correlation_data: configured_text(&config.correlation_data).map(String::into_bytes),
        content_type: configured_text(&config.content_type),
        message_expiry: config.message_expiry_interval.filter(|value| *value != 0),
        user_properties,
    })
}

fn parse_user_props_text(raw: &str) -> crate::Result<Vec<(String, String)>> {
    let value: serde_json::Value = serde_json::from_str(raw.trim())
        .map_err(|_| EdgelinkError::invalid_operation("mqtt userProps is not a JSON object of strings"))?;
    let serde_json::Value::Object(map) = value else {
        return Err(EdgelinkError::invalid_operation("mqtt userProps is not a JSON object of strings"));
    };
    let mut pairs = Vec::with_capacity(map.len());
    for (key, item) in map {
        let serde_json::Value::String(text) = item else {
            return Err(EdgelinkError::invalid_operation("mqtt userProps is not a JSON object of strings"));
        };
        pairs.push((key, text));
    }
    Ok(pairs)
}

fn merge_message_props(props: &mut PublishProps, msg: &Msg, v5: bool) -> crate::Result<()> {
    if !v5 {
        return if message_property_set(msg) {
            Err(EdgelinkError::NotSupported("MQTT v5 publish properties are not supported".to_owned()))
        } else {
            Ok(())
        };
    }
    if props.response_topic.is_none()
        && let Some(value) = msg.get("responseTopic")
    {
        props.response_topic = take_text(value, "responseTopic")?;
    }
    if props.correlation_data.is_none()
        && let Some(value) = msg.get("correlationData")
    {
        props.correlation_data = take_bytes(value, "correlationData")?;
    }
    if props.content_type.is_none()
        && let Some(value) = msg.get("contentType")
    {
        props.content_type = take_text(value, "contentType")?;
    }
    if props.message_expiry.is_none()
        && let Some(value) = msg.get("messageExpiryInterval")
    {
        props.message_expiry = optional_expiry(value)?;
    }
    if props.user_properties.is_empty()
        && let Some(value) = msg.get("userProperties")
        && !matches!(value, Variant::Null)
    {
        let parsed = user_properties_from_message(value)?;
        if !parsed.is_empty() {
            props.user_properties = parsed;
        }
    }
    Ok(())
}

fn message_property_set(msg: &Msg) -> bool {
    ["responseTopic", "correlationData", "contentType", "userProperties", "messageExpiryInterval"]
        .iter()
        .any(|key| msg.get(key).is_some_and(variant_is_publish_prop))
}

fn variant_is_publish_prop(value: &Variant) -> bool {
    match value {
        Variant::Null => false,
        Variant::String(text) => !text.is_empty(),
        Variant::Bytes(bytes) => !bytes.is_empty(),
        Variant::Object(map) => !map.is_empty(),
        Variant::Array(items) => !items.is_empty(),
        Variant::Number(_) | Variant::Bool(_) | Variant::Date(_) | Variant::Regexp(_) => true,
    }
}

fn take_text(value: &Variant, name: &str) -> crate::Result<Option<String>> {
    match value {
        Variant::Null => Ok(None),
        Variant::String(text) if text.is_empty() => Ok(None),
        Variant::String(text) => Ok(Some(text.clone())),
        _ => Err(EdgelinkError::invalid_operation(&format!("mqtt {name} is not a string"))),
    }
}

fn take_bytes(value: &Variant, name: &str) -> crate::Result<Option<Vec<u8>>> {
    match value {
        Variant::Null => Ok(None),
        Variant::String(text) if text.is_empty() => Ok(None),
        Variant::String(text) => Ok(Some(text.as_bytes().to_vec())),
        Variant::Bytes(bytes) if bytes.is_empty() => Ok(None),
        Variant::Bytes(bytes) => Ok(Some(bytes.clone())),
        _ => Err(EdgelinkError::invalid_operation(&format!("mqtt {name} is not bytes"))),
    }
}

fn optional_expiry(value: &Variant) -> crate::Result<Option<u32>> {
    match value {
        Variant::Null => Ok(None),
        Variant::String(text) if text.trim().is_empty() => Ok(None),
        Variant::Number(number) => number
            .as_u64()
            .and_then(|level| u32::try_from(level).ok())
            .map(Some)
            .ok_or_else(|| EdgelinkError::invalid_operation("mqtt messageExpiryInterval is out of range")),
        Variant::String(text) => text
            .trim()
            .parse::<u32>()
            .map(Some)
            .map_err(|_| EdgelinkError::invalid_operation("mqtt messageExpiryInterval is not a number")),
        _ => Err(EdgelinkError::invalid_operation("mqtt messageExpiryInterval is not a number")),
    }
}

fn user_properties_from_message(value: &Variant) -> crate::Result<Vec<(String, String)>> {
    let Variant::Object(map) = value else {
        return Err(EdgelinkError::invalid_operation("mqtt userProperties is not an object of strings"));
    };
    let mut pairs = Vec::with_capacity(map.len());
    for (key, item) in map {
        let Variant::String(text) = item else {
            return Err(EdgelinkError::invalid_operation("mqtt userProperties is not an object of strings"));
        };
        pairs.push((key.clone(), text.clone()));
    }
    Ok(pairs)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn out_config() -> MqttOutNodeConfig {
        MqttOutNodeConfig {
            broker: "b1".to_owned(),
            topic: String::new(),
            qos: MqttQoS::AtMost,
            retain: false,
            response_topic: String::new(),
            correlation_data: String::new(),
            content_type: String::new(),
            message_expiry_interval: None,
            user_properties: String::new(),
        }
    }

    #[test]
    fn a_set_node_topic_wins_over_the_message() {
        assert_eq!(planned_topic("plant/speed", Some("other")), "plant/speed");
        assert_eq!(planned_topic("", Some("other")), "other");
        assert_eq!(planned_topic("", None), "");
    }

    #[test]
    fn a_set_node_qos_wins_and_an_illegal_message_qos_is_an_error() {
        let two = Variant::Number(serde_json::Number::from(2));
        let nine = Variant::Number(serde_json::Number::from(9));
        assert_eq!(planned_qos(MqttQoS::AtLeast, Some(&nine)).unwrap(), rumqttc::QoS::AtLeastOnce);
        assert_eq!(planned_qos(MqttQoS::AtMost, Some(&two)).unwrap(), rumqttc::QoS::ExactlyOnce);
        assert_eq!(planned_qos(MqttQoS::AtMost, None).unwrap(), rumqttc::QoS::AtMostOnce);
        let err = planned_qos(MqttQoS::AtMost, Some(&nine)).unwrap_err();
        assert!(err.to_string().contains("out of range"), "{err}");
    }

    #[test]
    fn a_set_node_retain_wins() {
        assert!(planned_retain(true, Some(&Variant::Bool(false))));
        assert!(planned_retain(false, Some(&Variant::Bool(true))));
        assert!(!planned_retain(false, None));
    }

    #[test]
    fn a_set_node_response_topic_wins_on_v5() {
        let mut config = out_config();
        config.response_topic = "node-reply".to_owned();
        let mut msg = Msg::default();
        msg.set("responseTopic".to_string(), Variant::String("msg-reply".to_owned()));
        let mut props = config_props(&config).unwrap();
        merge_message_props(&mut props, &msg, true).unwrap();
        assert_eq!(props.response_topic.as_deref(), Some("node-reply"));

        config.response_topic.clear();
        let mut props = config_props(&config).unwrap();
        merge_message_props(&mut props, &msg, true).unwrap();
        assert_eq!(props.response_topic.as_deref(), Some("msg-reply"));
    }

    #[test]
    fn a_v4_message_response_topic_is_rejected() {
        let mut msg = Msg::default();
        msg.set("responseTopic".to_string(), Variant::String("reply".to_owned()));
        let mut props = PublishProps::default();
        let err = merge_message_props(&mut props, &msg, false).unwrap_err();
        assert!(err.to_string().starts_with("not supported"), "{err}");
    }

    #[test]
    fn a_non_string_user_property_is_rejected() {
        let mut msg = Msg::default();
        let mut map = std::collections::BTreeMap::new();
        map.insert("n".to_owned(), Variant::Number(serde_json::Number::from(1)));
        msg.set("userProperties".to_string(), Variant::Object(map));
        let mut props = PublishProps::default();
        let err = merge_message_props(&mut props, &msg, true).unwrap_err();
        assert!(err.to_string().contains("userProperties"), "{err}");
    }

    #[tokio::test]
    async fn bad_user_props_on_a_v5_out_node_fail_at_deploy() {
        let flows = serde_json::json!([
            { "id": "100", "type": "tab" },
            { "id": "b1", "type": "mqtt-broker", "broker": "localhost", "protocolVersion": 5, "clientid": "clientid", "autoConnect": false },
            { "id": "2", "z": "100", "type": "mqtt out", "broker": "b1", "topic": "out", "userProps": "nope", "wires": [] }
        ]);
        let err = crate::runtime::engine::build_test_engine(flows).unwrap_err();
        assert!(err.to_string().contains("userProps"), "{err}");
    }
}
