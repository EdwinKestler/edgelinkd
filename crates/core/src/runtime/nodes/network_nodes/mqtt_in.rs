// Licensed under the Apache License, Version 2.0
// Copyright EdgeLink contributors
// Based on Node-RED 10-mqtt.js MQTT In node

//! MQTT In Node
//!
//! This node is compatible with Node-RED's MQTT In node. It can:
//! - Subscribe to MQTT topics and receive messages
//! - Support static topic subscription from configuration
//! - Support dynamic topic subscription via input messages
//! - Handle various data types and output formats
//! - MQTT v5 subscription options when the broker is protocol 5
//!
//! Configuration:
//! - `broker`: Broker configuration node ID
//! - `topic`: Topic to subscribe to (supports wildcards)
//! - `qos`: Quality of Service level (0, 1, or 2)
//! - `datatype`: Output format ("auto-detect", "buffer", "utf8", "base64", "json")
//! - `nl`, `rap`, `rh`, and `subscriptionIdentifier` on a protocol 5 broker
//!
//! Dynamic subscription (when inputs=1):
//! - `msg.action`: "subscribe", "unsubscribe", "connect", "disconnect", "getSubscriptions"
//! - `msg.topic`: Topic(s) to subscribe/unsubscribe
//! - `msg.qos`: QoS level for subscription
//!
//! Output message:
//! - `msg.topic`: The topic the message was received on
//! - `msg.payload`: The message payload (converted according to datatype)
//! - `msg.qos`: QoS level of received message
//! - `msg.retain`: Retain flag of received message
//! - `msg._topic`: Original topic
//! - MQTT 5.0 properties, when the broker sent them: `responseTopic`,
//!   `correlationData`, `contentType`, `userProperties`, `messageExpiryInterval`,
//!   `payloadFormatIndicator`, `reasonString`, `subscriptionIdentifier`
//!
//! Behavior matches Node-RED:
//! - Wildcard topic support (+ and #)
//! - Auto-detection of payload format
//! - JSON parsing when appropriate
//! - Buffer/string conversion based on content

use std::collections::HashMap;
use std::sync::Arc;

use serde::Deserialize;
use tokio::sync::RwLock;

use super::mqtt_broker::{self, BrokerSession, qos_of};
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

fn default_in_qos() -> MqttQoS {
    MqttQoS::Exactly
}

fn deserialize_in_qos<'de, D>(deserializer: D) -> Result<MqttQoS, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = serde_json::Value::deserialize(deserializer)?;
    Ok(match qos_of(Some(&value), 2) {
        rumqttc::QoS::AtMostOnce => MqttQoS::AtMost,
        rumqttc::QoS::AtLeastOnce => MqttQoS::AtLeast,
        rumqttc::QoS::ExactlyOnce => MqttQoS::Exactly,
    })
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Default)]
enum MqttDataType {
    #[serde(rename = "auto")]
    Auto,
    #[serde(rename = "auto-detect")]
    AutoDetect,
    #[serde(rename = "buffer")]
    Buffer,
    #[serde(rename = "utf8")]
    #[default]
    Utf8,
    #[serde(rename = "base64")]
    Base64,
    #[serde(rename = "json")]
    Json,
}

#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)]
struct MqttInNodeConfig {
    /// MQTT broker connection ID (reference to broker config node)
    broker: String,

    /// Topic to subscribe to (supports wildcards + and #)
    #[serde(default)]
    topic: String,

    /// QoS level for subscription. Node-RED's default is 2.
    #[serde(default = "default_in_qos", deserialize_with = "deserialize_in_qos")]
    qos: MqttQoS,

    /// Data type for output
    #[serde(default)]
    datatype: MqttDataType,

    /// Number of inputs (0 = static subscription, 1 = dynamic subscription)
    #[serde(default)]
    inputs: u8,

    /// MQTT v5 subscription identifier
    #[serde(rename = "subscriptionIdentifier", default)]
    subscription_identifier: Option<u32>,

    /// MQTT v5 no local flag
    #[serde(default)]
    nl: Option<bool>,

    /// MQTT v5 retain as published flag
    #[serde(default)]
    rap: Option<bool>,

    /// MQTT v5 retain handling
    #[serde(default)]
    rh: Option<u8>,
}

fn reject_v5(config: &MqttInNodeConfig, v5: bool) -> crate::Result<()> {
    let rh = config.rh.unwrap_or(0);
    if rh > 2 {
        return Err(EdgelinkError::NotSupported(format!("MQTT retain handling {rh} is not supported")));
    }
    let non_default = config.subscription_identifier.unwrap_or(0) != 0
        || config.nl == Some(true)
        || rh != 0
        || config.rap == Some(false);
    if non_default && !v5 {
        return Err(EdgelinkError::NotSupported("MQTT v5 subscription properties are not supported".to_owned()));
    }
    Ok(())
}

/// Subscription information for dynamic subscriptions
#[derive(Debug, Clone)]
struct DynamicSubscription {
    topic: String,
    qos: MqttQoS,
    datatype: MqttDataType,
}

crate::node_hints!("mqtt in", refs = ["broker" => "mqtt-broker"], caps = ["network"]);

#[flow_node("mqtt in", red_name = "mqtt", inputs = 0, outputs = 1)]
struct MqttInNode {
    base: BaseFlowNodeState,
    config: MqttInNodeConfig,
    session: BrokerSession,
    dynamic_subscriptions: RwLock<HashMap<String, DynamicSubscription>>,
    /// Whether this node supports dynamic subscriptions
    is_dynamic: bool,
}

impl std::fmt::Debug for MqttInNode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MqttInNode")
            .field("id", &self.base.id)
            .field("topic", &self.config.topic)
            .field("dynamic", &self.is_dynamic)
            .finish()
    }
}

impl MqttInNode {
    fn build(
        flow: &Flow,
        base_node: BaseFlowNodeState,
        config: &RedFlowNodeConfig,
        _options: Option<&config::Config>,
    ) -> crate::Result<Box<dyn FlowNodeBehavior>> {
        let mqtt_config = MqttInNodeConfig::deserialize(&config.rest)?;
        let is_dynamic = mqtt_config.inputs == 1;
        let session = mqtt_broker::attach_broker(flow, &mqtt_config.broker)?;
        reject_v5(&mqtt_config, session.options().is_v5())?;

        let node = MqttInNode {
            base: base_node,
            config: mqtt_config,
            session,
            dynamic_subscriptions: RwLock::new(HashMap::new()),
            is_dynamic,
        };

        Ok(Box::new(node))
    }

    async fn subscribe_static(&self) -> crate::Result<()> {
        if self.is_dynamic || self.config.topic.is_empty() {
            return Ok(());
        }
        if !self.is_valid_subscription_topic(&self.config.topic) {
            return Err(EdgelinkError::invalid_operation(&format!(
                "Invalid topic for subscription: '{}'",
                self.config.topic
            )));
        }
        self.session.subscribe(&self.owner(), self.subscription_for(&self.config.topic, self.config.qos)).await
    }

    fn subscription_for(&self, topic: &str, qos: MqttQoS) -> mqtt_broker::Subscription {
        mqtt_broker::Subscription {
            topic: topic.to_owned(),
            qos: self.mqtt_qos_to_rumqttc(qos),
            nolocal: self.config.nl.unwrap_or(false),
            retain_as_published: self.config.rap.unwrap_or(true),
            retain_handling: self.config.rh.unwrap_or(0),
            subscription_identifier: self.config.subscription_identifier.filter(|id| *id != 0),
        }
    }

    /// Validate topic for subscription (allows wildcards)
    fn is_valid_subscription_topic(&self, topic: &str) -> bool {
        if topic.is_empty() {
            return false;
        }

        // Simple validation for MQTT subscription topics
        // Allows wildcards + and # but ensures they are properly placed
        !topic.chars().any(|c| matches!(c, '\x08' | '\x0C' | '\n' | '\r' | '\t' | '\x0B' | '\0'))
    }

    /// Convert internal QoS enum to rumqttc QoS
    fn mqtt_qos_to_rumqttc(&self, qos: MqttQoS) -> rumqttc::QoS {
        match qos {
            MqttQoS::AtMost => rumqttc::QoS::AtMostOnce,
            MqttQoS::AtLeast => rumqttc::QoS::AtLeastOnce,
            MqttQoS::Exactly => rumqttc::QoS::ExactlyOnce,
        }
    }

    /// Convert rumqttc QoS to number for message
    fn rumqttc_qos_to_number(&self, qos: rumqttc::QoS) -> u8 {
        match qos {
            rumqttc::QoS::AtMostOnce => 0,
            rumqttc::QoS::AtLeastOnce => 1,
            rumqttc::QoS::ExactlyOnce => 2,
        }
    }

    /// Convert JSON value to Variant
    fn json_value_to_variant(json_val: serde_json::Value) -> Variant {
        match json_val {
            serde_json::Value::Null => Variant::Null,
            serde_json::Value::Bool(b) => Variant::Bool(b),
            serde_json::Value::Number(n) => Variant::Number(n),
            serde_json::Value::String(s) => Variant::String(s),
            serde_json::Value::Array(arr) => {
                let variants: Vec<Variant> = arr.into_iter().map(Self::json_value_to_variant).collect();
                Variant::Array(variants)
            }
            serde_json::Value::Object(obj) => {
                let map: std::collections::BTreeMap<String, Variant> =
                    obj.into_iter().map(|(k, v)| (k, Self::json_value_to_variant(v))).collect();
                Variant::Object(map)
            }
        }
    }

    /// Handle subscription action
    async fn handle_subscribe_action(
        &self,
        topics: Vec<String>,
        qos_level: Option<u8>,
        datatype: Option<MqttDataType>,
    ) -> crate::Result<()> {
        let dt = datatype.unwrap_or(self.config.datatype.clone());

        for topic in topics {
            if !self.is_valid_subscription_topic(&topic) {
                log::warn!("Invalid subscription topic: '{topic}'");
                continue;
            }

            let qos = match qos_level {
                None => MqttQoS::Exactly,
                Some(0) => MqttQoS::AtMost,
                Some(1) => MqttQoS::AtLeast,
                Some(2) => MqttQoS::Exactly,
                Some(level) => {
                    return Err(EdgelinkError::invalid_operation(&format!("MQTT qos {level} is out of range")));
                }
            };
            self.session.subscribe(&self.owner(), self.subscription_for(&topic, qos)).await?;
            self.dynamic_subscriptions
                .write()
                .await
                .insert(topic.clone(), DynamicSubscription { topic: topic.clone(), qos, datatype: dt.clone() });
            log::info!("Subscribed to topic: {topic}");
        }

        Ok(())
    }

    /// Handle unsubscribe action
    async fn handle_unsubscribe_action(&self, topics: Vec<String>) -> crate::Result<()> {
        for topic in topics {
            let known = self.dynamic_subscriptions.read().await.contains_key(&topic);
            if !known {
                continue;
            }
            if let Err(err) = self.session.unsubscribe(&self.owner(), &topic).await {
                log::warn!("Failed to unsubscribe from '{topic}': {err}");
            } else {
                log::info!("Unsubscribed from topic: {topic}");
            }
            self.dynamic_subscriptions.write().await.remove(&topic);
        }

        Ok(())
    }

    /// Handle input message for dynamic subscriptions
    async fn handle_input_message(&self, msg: &Msg) -> crate::Result<Option<Msg>> {
        if let Some(action) = msg.get("action").and_then(|v| v.as_str()) {
            match action {
                "connect" => {
                    if msg.get("broker").is_some() {
                        return Err(EdgelinkError::NotSupported(
                            "a broker supplied on the message is not supported".to_owned(),
                        ));
                    }
                    self.session.connect().await?;
                    self.subscribe_static().await?;
                    log::info!("MQTT In connection established");
                }
                "disconnect" => {
                    self.session.disconnect().await?;
                    log::info!("MQTT In disconnected");
                }
                "subscribe" => {
                    let topics = self.extract_topics_from_message(msg)?;
                    let qos = msg.get("qos").and_then(|v| v.as_number()).and_then(|n| n.as_u64()).map(|n| n as u8);
                    let datatype = msg.get("datatype").and_then(|v| v.as_str()).and_then(|s| match s {
                        "auto" => Some(MqttDataType::Auto),
                        "auto-detect" => Some(MqttDataType::AutoDetect),
                        "buffer" => Some(MqttDataType::Buffer),
                        "utf8" => Some(MqttDataType::Utf8),
                        "base64" => Some(MqttDataType::Base64),
                        "json" => Some(MqttDataType::Json),
                        _ => None,
                    });

                    self.handle_subscribe_action(topics, qos, datatype).await?;
                    // Node-RED completes this action. It does not send the message on.
                    return Ok(None);
                }
                "unsubscribe" => {
                    let topics = if let Some(Variant::Bool(true)) = msg.get("topic") {
                        // Unsubscribe from all
                        let subs = self.dynamic_subscriptions.read().await;
                        subs.keys().cloned().collect()
                    } else {
                        self.extract_topics_from_message(msg)?
                    };

                    self.handle_unsubscribe_action(topics).await?;
                    return Ok(None);
                }
                "getSubscriptions" => {
                    let subs = self.dynamic_subscriptions.read().await;
                    let sub_list: Vec<Variant> = subs
                        .values()
                        .map(|s| {
                            let mut sub_obj = std::collections::BTreeMap::new();
                            sub_obj.insert("topic".to_string(), Variant::String(s.topic.clone()));
                            sub_obj.insert("qos".to_string(), Variant::Number(serde_json::Number::from(s.qos as u8)));
                            Variant::Object(sub_obj)
                        })
                        .collect();

                    let mut response_msg = msg.clone();
                    response_msg.set("topic".to_string(), Variant::String("subscriptions".to_string()));
                    response_msg.set("payload".to_string(), Variant::Array(sub_list));
                    return Ok(Some(response_msg));
                }
                _ => {
                    return Err(crate::EdgelinkError::invalid_operation(&format!(
                        "Invalid MQTT In action: '{action}'. Valid actions are 'connect', 'disconnect', 'subscribe', 'unsubscribe', 'getSubscriptions'"
                    )));
                }
            }
        }

        Ok(None)
    }

    /// Extract topics from message
    fn extract_topics_from_message(&self, msg: &Msg) -> crate::Result<Vec<String>> {
        if let Some(topic_val) = msg.get("topic") {
            match topic_val {
                Variant::String(s) => Ok(vec![s.clone()]),
                Variant::Array(arr) => {
                    let mut topics = Vec::new();
                    for item in arr {
                        if let Variant::String(s) = item {
                            topics.push(s.clone());
                        } else if let Variant::Object(obj) = item
                            && let Some(Variant::String(topic)) = obj.get("topic")
                        {
                            topics.push(topic.clone());
                        }
                    }
                    Ok(topics)
                }
                _ => Err(crate::EdgelinkError::invalid_operation("Invalid topic format in message")),
            }
        } else {
            Err(crate::EdgelinkError::invalid_operation("No topic specified in message"))
        }
    }
}

#[async_trait]
impl FlowNodeBehavior for MqttInNode {
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
        // Static nodes take connect and disconnect actions too. Record the static
        // filter before the socket opens so a birth publish on CONNACK is in scope.
        {
            let node = self.clone();
            let input_stop_token = stop_token.clone();
            tokio::spawn(async move {
                while !input_stop_token.is_cancelled() {
                    let node = node.clone();
                    let cancel = input_stop_token.clone();
                    with_uow(node.as_ref(), cancel.clone(), |node, msg| async move {
                        let outcome = {
                            let msg_guard = msg.read().await;
                            node.handle_input_message(&msg_guard).await
                        };
                        match outcome {
                            Ok(Some(response_msg)) => {
                                let response_handle = MsgHandle::new(response_msg);
                                node.fan_out_one(Envelope { port: 0, msg: response_handle }, cancel).await?;
                            }
                            Ok(None) => {}
                            Err(err) => return Err(err),
                        }
                        Ok(())
                    })
                    .await;
                }
            });
        }
        if let Err(err) = self.subscribe_static().await {
            log::warn!("MQTT In subscribe failed: {err}");
            self.report_error(err.to_string(), MsgHandle::new(Msg::default()), stop_token.child_token()).await;
        }

        if self.session.options().auto_connect() {
            loop {
                if stop_token.is_cancelled() {
                    break;
                }
                match self.session.connect().await {
                    Ok(()) => {
                        if let Err(err) = self.subscribe_static().await {
                            log::warn!("MQTT In subscribe failed: {err}");
                            self.report_error(
                                err.to_string(),
                                MsgHandle::new(Msg::default()),
                                stop_token.child_token(),
                            )
                            .await;
                        }
                        break;
                    }
                    Err(err) => {
                        log::warn!("MQTT In connection failed: {err}");
                        tokio::select! {
                            _ = stop_token.cancelled() => break,
                            _ = tokio::time::sleep(self.session.options().reconnect_period()) => {}
                        }
                    }
                }
            }
        }

        let cap = self.flow().map(|flow| flow.settings().node_msg_queue_capacity).unwrap_or(16);
        let mut incoming = self.session.listen(cap);
        let notify = incoming.notify_handle();
        let overflow = incoming.overflow_handle();
        loop {
            tokio::select! {
                _ = stop_token.cancelled() => break,
                _ = notify.notified() => {
                    let dropped = overflow.swap(0, std::sync::atomic::Ordering::AcqRel);
                    if dropped > 0 {
                        let text = format!("mqtt delivery queue is full ({dropped} dropped)");
                        self.report_status(
                            StatusObject {
                                fill: Some(StatusFill::Red),
                                shape: Some(StatusShape::Dot),
                                text: Some(text.clone()),
                            },
                            stop_token.child_token(),
                        )
                        .await;
                        self.report_error(text, MsgHandle::new(Msg::default()), stop_token.child_token()).await;
                    }
                }
                frame = incoming.rx.recv() => {
                    let Some(frame) = frame else { break };
                    self.emit_publish(frame, &stop_token).await;
                }
            }
        }

        if self.session.options().auto_unsubscribe() {
            self.drop_subscriptions().await;
        }
        self.session.release_user().await;
        log::debug!("MqttInNode process() task has been terminated.");
    }
}

impl MqttInNode {
    async fn emit_publish(&self, frame: mqtt_broker::IncomingPublish, cancel: &CancellationToken) {
        let Some(datatype) = self.datatype_for(&frame.topic).await else {
            return;
        };
        let mut mqtt_msg = Msg::default();
        mqtt_msg.set("topic".to_string(), Variant::String(frame.topic.clone()));
        mqtt_msg
            .set("qos".to_string(), Variant::Number(serde_json::Number::from(self.rumqttc_qos_to_number(frame.qos))));
        mqtt_msg.set("retain".to_string(), Variant::Bool(frame.retain));
        if let Some(properties) = &frame.properties {
            apply_incoming_properties(&mut mqtt_msg, properties);
        }
        match decode_payload(&frame.payload, &datatype, frame.properties.as_ref()) {
            Ok(payload) => mqtt_msg.set("payload".to_string(), payload),
            Err(err) => {
                mqtt_msg.set("payload".to_string(), Variant::Bytes(frame.payload.to_vec()));
                self.report_error(err.to_string(), MsgHandle::new(mqtt_msg), cancel.child_token()).await;
                return;
            }
        }
        mqtt_msg.set("_topic".to_string(), Variant::String(frame.topic));
        let msg_handle = MsgHandle::new(mqtt_msg);
        if let Err(err) = self.fan_out_one(Envelope { port: 0, msg: msg_handle }, cancel.clone()).await {
            log::warn!("Failed to send MQTT message: {err}");
        }
    }

    async fn datatype_for(&self, topic: &str) -> Option<MqttDataType> {
        if self.is_dynamic {
            let subs = self.dynamic_subscriptions.read().await;
            return subs
                .values()
                .find(|sub| mqtt_broker::topic_matches(&sub.topic, topic))
                .map(|sub| sub.datatype.clone());
        }
        if self.config.topic.is_empty() || !mqtt_broker::topic_matches(&self.config.topic, topic) {
            return None;
        }
        Some(self.config.datatype.clone())
    }

    fn owner(&self) -> String {
        self.id().to_string()
    }

    async fn drop_subscriptions(&self) {
        let _ = self.session.unsubscribe_owner(&self.owner()).await;
        self.dynamic_subscriptions.write().await.clear();
    }
}

fn apply_incoming_properties(msg: &mut Msg, properties: &mqtt_broker::IncomingProperties) {
    if let Some(topic) = properties.response_topic.as_ref().filter(|text| !text.is_empty()) {
        msg.set("responseTopic".to_string(), Variant::String(topic.clone()));
    }
    if let Some(data) = &properties.correlation_data {
        msg.set("correlationData".to_string(), Variant::Bytes(data.to_vec()));
    }
    if let Some(content_type) = properties.content_type.as_ref().filter(|text| !text.is_empty()) {
        msg.set("contentType".to_string(), Variant::String(content_type.clone()));
    }
    if let Some(expiry) = properties.message_expiry {
        let expiry = u64::from(expiry);
        msg.set("messageExpiryInterval".to_string(), Variant::Number(serde_json::Number::from(expiry)));
    }
    if let Some(flag) = properties.payload_format_utf8 {
        msg.set("payloadFormatIndicator".to_string(), Variant::Bool(flag));
    }
    if let Some(reason) = properties.reason_string.as_ref().filter(|text| !text.is_empty()) {
        msg.set("reasonString".to_string(), Variant::String(reason.clone()));
    }
    if !properties.user_properties.is_empty() {
        let map = properties
            .user_properties
            .iter()
            .map(|(key, value)| (key.clone(), Variant::String(value.clone())))
            .collect();
        msg.set("userProperties".to_string(), Variant::Object(map));
    }
    match properties.subscription_identifiers.as_slice() {
        [] => {}
        [id] => {
            let id = u64::try_from(*id).unwrap_or(u64::MAX);
            msg.set("subscriptionIdentifier".to_string(), Variant::Number(serde_json::Number::from(id)));
        }
        ids => {
            let values = ids
                .iter()
                .map(|id| Variant::Number(serde_json::Number::from(u64::try_from(*id).unwrap_or(u64::MAX))))
                .collect();
            msg.set("subscriptionIdentifier".to_string(), Variant::Array(values));
        }
    }
}

/// Buffer, utf8, base64, and json keep their existing conversion.
/// `auto` and `auto-detect` follow the broker content type. A JSON content type
/// that does not parse is an error for both. Only `auto-detect` replaces the
/// payload with the parsed value.
fn decode_payload(
    payload: &[u8],
    datatype: &MqttDataType,
    properties: Option<&mqtt_broker::IncomingProperties>,
) -> crate::Result<Variant> {
    match datatype {
        MqttDataType::Buffer => Ok(Variant::Bytes(payload.to_vec())),
        MqttDataType::Base64 => {
            use base64::{Engine as _, engine::general_purpose};
            Ok(Variant::String(general_purpose::STANDARD.encode(payload)))
        }
        MqttDataType::Utf8 => match std::str::from_utf8(payload) {
            Ok(text) => Ok(Variant::String(text.to_owned())),
            Err(_) => Ok(Variant::Bytes(payload.to_vec())),
        },
        MqttDataType::Json => decode_json_mode(payload),
        MqttDataType::Auto | MqttDataType::AutoDetect => decode_auto(payload, datatype, properties),
    }
}

/// Node-RED `datatype === "json"` reports `node.error` and does not send the message.
fn decode_json_mode(payload: &[u8]) -> crate::Result<Variant> {
    let text = std::str::from_utf8(payload).map_err(|_| EdgelinkError::invalid_operation("Invalid JSON string"))?;
    serde_json::from_str::<serde_json::Value>(text)
        .map(MqttInNode::json_value_to_variant)
        .map_err(|_| EdgelinkError::invalid_operation("Failed to parse JSON string"))
}

fn decode_auto(
    payload: &[u8],
    datatype: &MqttDataType,
    properties: Option<&mqtt_broker::IncomingProperties>,
) -> crate::Result<Variant> {
    let format_utf8 = properties.and_then(|props| props.payload_format_utf8) == Some(true);
    let content_type = properties.and_then(|props| props.content_type.as_deref()).filter(|text| !text.is_empty());
    if format_utf8 || content_type.is_some() {
        match content_type.and_then(media_kind) {
            Some("string") => Ok(Variant::String(text_payload(payload))),
            Some("buffer") => Ok(Variant::Bytes(payload.to_vec())),
            Some("json") => decode_json_media(payload, datatype),
            _ if format_utf8 || std::str::from_utf8(payload).is_ok() => decode_utf8_auto(payload, datatype),
            _ => Ok(Variant::Bytes(payload.to_vec())),
        }
    } else if std::str::from_utf8(payload).is_ok() {
        decode_utf8_auto(payload, datatype)
    } else {
        Ok(Variant::Bytes(payload.to_vec()))
    }
}

fn decode_json_media(payload: &[u8], datatype: &MqttDataType) -> crate::Result<Variant> {
    let text = text_payload(payload);
    let parsed = serde_json::from_str::<serde_json::Value>(&text)
        .map_err(|_| EdgelinkError::invalid_operation("Failed to parse JSON string"))?;
    if datatype == &MqttDataType::AutoDetect {
        Ok(MqttInNode::json_value_to_variant(parsed))
    } else {
        Ok(Variant::String(text))
    }
}

fn decode_utf8_auto(payload: &[u8], datatype: &MqttDataType) -> crate::Result<Variant> {
    let text = text_payload(payload);
    if datatype == &MqttDataType::AutoDetect
        && let Ok(json_val) = serde_json::from_str::<serde_json::Value>(&text)
    {
        return Ok(MqttInNode::json_value_to_variant(json_val));
    }
    Ok(Variant::String(text))
}

fn text_payload(payload: &[u8]) -> String {
    match std::str::from_utf8(payload) {
        Ok(text) => text.to_owned(),
        Err(_) => String::from_utf8_lossy(payload).into_owned(),
    }
}

/// Node-RED `knownMediaTypes`. The match is the whole content type, lowercased.
fn media_kind(content_type: &str) -> Option<&'static str> {
    match content_type.to_ascii_lowercase().as_str() {
        "text/css" | "text/html" | "text/plain" | "application/xml" => Some("string"),
        "application/json" => Some("json"),
        "application/octet-stream"
        | "application/pdf"
        | "application/x-gtar"
        | "application/x-gzip"
        | "application/x-tar"
        | "application/zip"
        | "audio/aac"
        | "audio/ac3"
        | "audio/basic"
        | "audio/mp4"
        | "audio/ogg"
        | "image/bmp"
        | "image/gif"
        | "image/jpeg"
        | "image/tiff"
        | "image/png" => Some("buffer"),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn json_properties() -> mqtt_broker::IncomingProperties {
        mqtt_broker::IncomingProperties {
            response_topic: None,
            correlation_data: None,
            content_type: Some("application/json".to_owned()),
            user_properties: Vec::new(),
            message_expiry: None,
            subscription_identifiers: Vec::new(),
            payload_format_utf8: Some(true),
            reason_string: None,
        }
    }

    #[test]
    fn a_json_content_type_that_is_not_json_is_an_error() {
        let props = json_properties();
        for datatype in [MqttDataType::Auto, MqttDataType::AutoDetect] {
            let err = decode_payload(b"nope", &datatype, Some(&props)).unwrap_err();
            assert!(err.to_string().contains("JSON"), "{err}");
        }
    }

    #[test]
    fn auto_detect_parses_json_and_auto_keeps_the_string() {
        let props = json_properties();
        let parsed = decode_payload(br#"{"a":1}"#, &MqttDataType::AutoDetect, Some(&props)).unwrap();
        assert!(matches!(parsed, Variant::Object(_)));
        let text = decode_payload(br#"{"a":1}"#, &MqttDataType::Auto, Some(&props)).unwrap();
        assert_eq!(text, Variant::String(r#"{"a":1}"#.to_owned()));
    }

    #[test]
    fn buffer_ignores_a_json_content_type_and_json_mode_errors() {
        let props = json_properties();
        let bytes = decode_payload(b"nope", &MqttDataType::Buffer, Some(&props)).unwrap();
        assert_eq!(bytes, Variant::Bytes(b"nope".to_vec()));
        let err = decode_payload(b"nope", &MqttDataType::Json, Some(&props)).unwrap_err();
        assert!(err.to_string().contains("JSON"), "{err}");
    }
}
