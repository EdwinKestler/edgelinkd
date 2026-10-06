// Licensed under the Apache License, Version 2.0
// Copyright EdgeLink contributors
// Based on Node-RED 10-mqtt.js MQTT Broker Config node

//! MQTT broker config node.
//!
//! MQTT in and out share this node's connection. Protocol 4 uses the `rumqttc` v4
//! client. Protocol 5 uses `rumqttc::v5`. TLS, WebSocket, MQTT 3.1, and enhanced
//! AUTH are rejected at deploy.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::time::Duration;

use serde_json::Value;
use tokio::sync::{Mutex, Notify, mpsc, watch};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use crate::N2linkError;
use crate::runtime::egress::{EgressMode, EgressPolicyHandle, EgressPurpose, NetworkProtocol};
use crate::runtime::flow::Flow;
use crate::runtime::model::{Msg, MsgHandle};
use crate::runtime::nodes::*;
use n2link_macro::*;
use rumqttc::v5::mqttbytes::QoS as V5Qos;
use rumqttc::v5::mqttbytes::v5::{
    ConnectProperties, Filter, LastWill as V5LastWill, LastWillProperties, Packet as V5Packet, PublishProperties,
    RetainForwardRule, SubscribeProperties,
};
use rumqttc::v5::{
    AsyncClient as V5Client, ConnectionError as V5Error, Event as V5Event, EventLoop as V5Loop,
    MqttOptions as V5Options,
};
use rumqttc::{AsyncClient, EventLoop, LastWill, MqttOptions, QoS};

const DEFAULT_PORT: u16 = 1883;
const DEFAULT_KEEPALIVE_SECS: u64 = 60;
const DEFAULT_RECONNECT: Duration = Duration::from_millis(5000);
const CONNECT_WAIT: Duration = Duration::from_secs(10);
const CLOSE_FLUSH: Duration = Duration::from_millis(200);

#[derive(Clone, Default, PartialEq, Eq)]
pub(crate) struct PublishProps {
    pub response_topic: Option<String>,
    pub correlation_data: Option<Vec<u8>>,
    pub content_type: Option<String>,
    pub message_expiry: Option<u32>,
    pub user_properties: Vec<(String, String)>,
}

impl PublishProps {
    fn is_set(&self) -> bool {
        self.response_topic.is_some()
            || self.correlation_data.is_some()
            || self.content_type.is_some()
            || self.message_expiry.is_some()
            || !self.user_properties.is_empty()
    }
}

#[derive(Clone)]
pub(crate) struct MqttNotice {
    pub topic: String,
    pub payload: Vec<u8>,
    pub qos: QoS,
    pub retain: bool,
    pub props: PublishProps,
    pub will_delay: Option<u32>,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Subscription {
    pub topic: String,
    pub qos: QoS,
    pub nolocal: bool,
    pub retain_as_published: bool,
    pub retain_handling: u8,
    pub subscription_identifier: Option<u32>,
}

#[derive(Clone, Debug, PartialEq)]
enum BrokerCommand {
    Subscribe(Subscription),
    Unsubscribe(String),
}

#[derive(Clone)]
pub(crate) struct IncomingProperties {
    pub response_topic: Option<String>,
    pub correlation_data: Option<bytes::Bytes>,
    pub content_type: Option<String>,
    pub user_properties: Vec<(String, String)>,
    pub message_expiry: Option<u32>,
    pub subscription_identifiers: Vec<usize>,
    pub payload_format_utf8: Option<bool>,
    pub reason_string: Option<String>,
}

#[derive(Clone)]
pub(crate) struct IncomingPublish {
    pub topic: String,
    pub payload: bytes::Bytes,
    pub qos: QoS,
    pub retain: bool,
    pub properties: Option<IncomingProperties>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Protocol {
    V4,
    V5,
}

#[derive(Clone, Default)]
struct ConnectSettings {
    session_expiry: Option<u32>,
    user_properties: Vec<(String, String)>,
}

#[derive(Clone)]
pub(crate) struct BrokerOptions {
    host: String,
    port: u16,
    /// Node-RED's broker URL string. The socket uses `host` and `port`.
    #[cfg_attr(not(test), allow(dead_code))]
    brokerurl: String,
    client_id: String,
    keepalive_secs: u64,
    clean: bool,
    auto_connect: bool,
    auto_unsubscribe: bool,
    reconnect_period: Duration,
    username: Option<String>,
    password: Option<String>,
    protocol: Protocol,
    connect: ConnectSettings,
    will: Option<MqttNotice>,
    birth: Option<MqttNotice>,
    close: Option<MqttNotice>,
}

impl BrokerOptions {
    #[cfg(test)]
    pub(crate) fn host(&self) -> &str {
        &self.host
    }

    #[cfg(test)]
    pub(crate) fn port(&self) -> u16 {
        self.port
    }

    #[cfg(test)]
    pub(crate) fn brokerurl(&self) -> &str {
        &self.brokerurl
    }

    #[cfg(test)]
    pub(crate) fn client_id(&self) -> &str {
        &self.client_id
    }

    #[cfg(test)]
    pub(crate) fn keepalive_secs(&self) -> u64 {
        self.keepalive_secs
    }

    #[cfg(test)]
    pub(crate) fn clean(&self) -> bool {
        self.clean
    }

    pub(crate) fn auto_connect(&self) -> bool {
        self.auto_connect
    }

    pub(crate) fn auto_unsubscribe(&self) -> bool {
        self.auto_unsubscribe
    }

    pub(crate) fn reconnect_period(&self) -> Duration {
        self.reconnect_period
    }

    pub(crate) fn is_v5(&self) -> bool {
        self.protocol == Protocol::V5
    }

    fn v4_options(&self) -> MqttOptions {
        let mut opts = MqttOptions::new(self.client_id.clone(), self.host.clone(), self.port);
        opts.set_keep_alive(Duration::from_secs(self.keepalive_secs));
        opts.set_clean_session(self.clean);
        if let Some(username) = &self.username {
            opts.set_credentials(username, self.password.clone().unwrap_or_default());
        }
        if let Some(will) = &self.will {
            opts.set_last_will(LastWill {
                topic: will.topic.clone(),
                message: will.payload.clone().into(),
                qos: will.qos,
                retain: will.retain,
            });
        }
        opts
    }

    fn v5_connect_properties(&self) -> ConnectProperties {
        let mut props = ConnectProperties::new();
        props.session_expiry_interval = self.connect.session_expiry;
        props.user_properties = self.connect.user_properties.clone();
        props.request_response_info = Some(1);
        props.request_problem_info = Some(1);
        props
    }

    fn v5_options(&self) -> V5Options {
        let mut opts = V5Options::new(self.client_id.clone(), self.host.clone(), self.port);
        // rumqttc's v5 client panics when keep alive is below 5 seconds. Deploy rejects that.
        opts.set_keep_alive(Duration::from_secs(self.keepalive_secs));
        opts.set_clean_start(self.clean);
        opts.set_connection_timeout(CONNECT_WAIT.as_secs());
        opts.set_connect_properties(self.v5_connect_properties());
        if let Some(username) = &self.username {
            opts.set_credentials(username, self.password.clone().unwrap_or_default());
        }
        if let Some(will) = &self.will {
            opts.set_last_will(v5_last_will(will));
        }
        opts
    }
}

#[derive(Clone, PartialEq, Eq)]
enum Phase {
    Idle,
    Connecting,
    Up,
    Refused(String),
    Down(String),
}

#[derive(Clone, PartialEq, Eq)]
struct LinkState {
    generation: u64,
    phase: Phase,
    retry_at: Option<Instant>,
}

impl LinkState {
    fn idle() -> Self {
        Self { generation: 0, phase: Phase::Idle, retry_at: None }
    }
}

/// A failed attempt must not satisfy the next wait. `Up` is not a new attempt.
fn wait_generation(state: &LinkState) -> u64 {
    match state.phase {
        Phase::Refused(_) | Phase::Down(_) => state.generation.saturating_add(1),
        _ => state.generation,
    }
}

pub(crate) fn editor_status(state_text: &str, up: bool, connecting: bool) -> StatusObject {
    if up {
        return StatusObject {
            fill: Some(StatusFill::Green),
            shape: Some(StatusShape::Dot),
            text: Some("node-red:common.status.connected".to_owned()),
        };
    }
    if connecting {
        return StatusObject {
            fill: Some(StatusFill::Yellow),
            shape: Some(StatusShape::Ring),
            text: Some("node-red:common.status.connecting".to_owned()),
        };
    }
    let text =
        if state_text.is_empty() { "node-red:common.status.disconnected".to_owned() } else { state_text.to_owned() };
    StatusObject { fill: Some(StatusFill::Red), shape: Some(StatusShape::Ring), text: Some(text) }
}

fn status_view(state: &LinkState) -> StatusObject {
    match &state.phase {
        Phase::Up => editor_status("", true, false),
        Phase::Connecting => editor_status("", false, true),
        Phase::Refused(text) => editor_status(text, false, false),
        Phase::Idle | Phase::Down(_) => editor_status("", false, false),
    }
}

#[derive(Clone)]
enum SharedClient {
    V4(AsyncClient),
    V5(V5Client),
}

impl SharedClient {
    async fn publish_notice(&self, notice: &MqttNotice) -> crate::Result<()> {
        match self {
            SharedClient::V4(client) => client
                .publish(notice.topic.clone(), notice.qos, notice.retain, notice.payload.clone())
                .await
                .map_err(|err| N2linkError::invalid_operation(&format!("MQTT publish failed: {err}"))),
            SharedClient::V5(client) => {
                let payload = bytes::Bytes::from(notice.payload.clone());
                let qos = v5_qos(notice.qos);
                let sent = match v5_publish_props(&notice.props) {
                    Some(props) => {
                        client.publish_with_properties(notice.topic.clone(), qos, notice.retain, payload, props).await
                    }
                    None => client.publish(notice.topic.clone(), qos, notice.retain, payload).await,
                };
                sent.map_err(|err| N2linkError::invalid_operation(&format!("MQTT publish failed: {err}")))
            }
        }
    }

    async fn subscribe_one(&self, sub: &Subscription) -> crate::Result<()> {
        match self {
            SharedClient::V4(client) => client
                .subscribe(sub.topic.clone(), sub.qos)
                .await
                .map_err(|err| N2linkError::invalid_operation(&format!("MQTT subscribe failed: {err}"))),
            SharedClient::V5(client) => {
                let filter = v5_filter(sub);
                let sent = match v5_subscribe_properties(sub) {
                    Some(props) => client.subscribe_many_with_properties(vec![filter], props).await,
                    None => client.subscribe_many(vec![filter]).await,
                };
                sent.map_err(|err| N2linkError::invalid_operation(&format!("MQTT subscribe failed: {err}")))
            }
        }
    }

    async fn unsubscribe_one(&self, topic: &str) -> crate::Result<()> {
        let failed = match self {
            SharedClient::V4(client) => client.unsubscribe(topic).await.err().map(|err| err.to_string()),
            SharedClient::V5(client) => client.unsubscribe(topic).await.err().map(|err| err.to_string()),
        };
        match failed {
            Some(err) => Err(N2linkError::invalid_operation(&format!("MQTT unsubscribe failed: {err}"))),
            None => Ok(()),
        }
    }

    async fn disconnect_link(&self) {
        match self {
            SharedClient::V4(client) => {
                let _ = client.disconnect().await;
            }
            SharedClient::V5(client) => {
                let _ = client.disconnect().await;
            }
        }
    }
}

enum Link {
    V4(Box<EventLoop>),
    V5(Box<V5Loop>),
}

enum LinkEvent {
    Accepted { subscription_ids: bool },
    Refused(String),
    Publish(Box<IncomingPublish>),
    Down(String),
    Ignore,
}

impl Link {
    async fn poll_event(&mut self) -> LinkEvent {
        match self {
            Link::V4(eventloop) => match eventloop.poll().await {
                Ok(rumqttc::Event::Incoming(rumqttc::Packet::ConnAck(connack))) => {
                    if connack.code == rumqttc::ConnectReturnCode::Success {
                        LinkEvent::Accepted { subscription_ids: true }
                    } else {
                        LinkEvent::Refused(format!("connection refused: {:?}", connack.code))
                    }
                }
                Ok(rumqttc::Event::Incoming(rumqttc::Packet::Publish(publish))) => {
                    LinkEvent::Publish(Box::new(IncomingPublish {
                        topic: publish.topic,
                        payload: publish.payload,
                        qos: publish.qos,
                        retain: publish.retain,
                        properties: None,
                    }))
                }
                Ok(_) => LinkEvent::Ignore,
                Err(err) => LinkEvent::Down(err.to_string()),
            },
            Link::V5(eventloop) => match eventloop.poll().await {
                Ok(V5Event::Incoming(V5Packet::ConnAck(connack))) => {
                    LinkEvent::Accepted { subscription_ids: subscription_ids_available(connack.properties.as_ref()) }
                }
                Ok(V5Event::Incoming(V5Packet::Publish(publish))) => LinkEvent::Publish(Box::new(incoming_v5(publish))),
                Ok(_) => LinkEvent::Ignore,
                Err(V5Error::ConnectionRefused(code)) => LinkEvent::Refused(format!("connection refused: {code:?}")),
                Err(err) => LinkEvent::Down(err.to_string()),
            },
        }
    }

    fn clean(&mut self) {
        match self {
            Link::V4(eventloop) => eventloop.clean(),
            Link::V5(eventloop) => eventloop.clean(),
        }
    }
}

struct Listener {
    tx: mpsc::Sender<IncomingPublish>,
    overflow: Arc<AtomicU64>,
    notify: Arc<Notify>,
}

/// Bounded receive side for one mqtt-in node.
pub(crate) struct Incoming {
    pub rx: mpsc::Receiver<IncomingPublish>,
    overflow: Arc<AtomicU64>,
    notify: Arc<Notify>,
}

impl Incoming {
    #[cfg(test)]
    pub fn overflow(&self) -> u64 {
        self.overflow.swap(0, Ordering::AcqRel)
    }

    pub fn notify_handle(&self) -> Arc<Notify> {
        self.notify.clone()
    }

    pub fn overflow_handle(&self) -> Arc<AtomicU64> {
        self.overflow.clone()
    }
}

struct Inner {
    options: BrokerOptions,
    egress: EgressPolicyHandle,
    client: Mutex<Option<SharedClient>>,
    task: Mutex<Option<tokio::task::JoinHandle<()>>>,
    cancel: Mutex<CancellationToken>,
    status: watch::Sender<LinkState>,
    manual_down: AtomicBool,
    users: AtomicUsize,
    opens: AtomicU64,
    ids_available: AtomicBool,
    listeners: std::sync::Mutex<Vec<Listener>>,
    /// topic -> owner id -> subscription. Broker SUBSCRIBE is sent for the first owner.
    subs: Mutex<HashMap<String, HashMap<String, Subscription>>>,
    commands: Mutex<Vec<BrokerCommand>>,
    fault: Mutex<Option<String>>,
    start_gate: Mutex<()>,
}

#[derive(Clone)]
pub(crate) struct BrokerSession {
    inner: Arc<Inner>,
}

impl BrokerSession {
    #[cfg(test)]
    fn from_options(options: BrokerOptions) -> Self {
        Self::from_options_with_policy(options, EgressPolicyHandle::default())
    }

    fn from_options_with_policy(options: BrokerOptions, egress: EgressPolicyHandle) -> Self {
        Self {
            inner: Arc::new(Inner {
                options,
                egress,
                client: Mutex::new(None),
                task: Mutex::new(None),
                cancel: Mutex::new(CancellationToken::new()),
                status: watch::channel(LinkState::idle()).0,
                manual_down: AtomicBool::new(false),
                users: AtomicUsize::new(0),
                opens: AtomicU64::new(0),
                ids_available: AtomicBool::new(true),
                listeners: std::sync::Mutex::new(Vec::new()),
                subs: Mutex::new(HashMap::new()),
                commands: Mutex::new(Vec::new()),
                fault: Mutex::new(None),
                start_gate: Mutex::new(()),
            }),
        }
    }

    pub(crate) fn options(&self) -> &BrokerOptions {
        &self.inner.options
    }

    pub(crate) fn retain_user(&self) {
        self.inner.users.fetch_add(1, Ordering::AcqRel);
    }

    /// Drop one user. The last user stops the shared connection.
    pub(crate) async fn release_user(&self) {
        let previous = loop {
            let current = self.inner.users.load(Ordering::Acquire);
            if current == 0 {
                return;
            }
            if self.inner.users.compare_exchange(current, current - 1, Ordering::AcqRel, Ordering::Acquire).is_ok() {
                break current;
            }
        };
        if previous == 1 {
            self.shutdown().await;
        }
    }

    pub(crate) fn listen(&self, capacity: usize) -> Incoming {
        let capacity = capacity.max(1);
        let (tx, rx) = mpsc::channel(capacity);
        let overflow = Arc::new(AtomicU64::new(0));
        let notify = Arc::new(Notify::new());
        let mut guard = match self.inner.listeners.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        guard.push(Listener { tx, overflow: overflow.clone(), notify: notify.clone() });
        Incoming { rx, overflow, notify }
    }

    pub(crate) async fn connect(&self) -> crate::Result<()> {
        self.inner.manual_down.store(false, Ordering::Release);
        self.ensure_started().await
    }

    pub(crate) async fn ensure_started(&self) -> crate::Result<()> {
        if self.inner.manual_down.load(Ordering::Acquire) {
            return Err(N2linkError::invalid_operation("mqtt broker is disconnected"));
        }
        if matches!(self.state().phase, Phase::Up) && self.inner.client.lock().await.is_some() {
            return Ok(());
        }
        if self.inner.client.lock().await.is_none() {
            self.spawn().await?;
        }
        self.wait_ready().await
    }

    /// Publish on the shared client. `autoConnect: false` does not open the socket.
    pub(crate) async fn publish(
        &self,
        topic: &str,
        qos: QoS,
        retain: bool,
        payload: Vec<u8>,
        props: PublishProps,
    ) -> crate::Result<()> {
        if !self.options().is_v5() && props.is_set() {
            return Err(N2linkError::NotSupported("MQTT v5 publish properties are not supported".to_owned()));
        }
        let notice = MqttNotice { topic: topic.to_owned(), payload, qos, retain, props, will_delay: None };
        let client = if self.options().auto_connect() {
            self.ensure_started().await?;
            self.require_client().await?
        } else {
            self.connected_client().await?
        };
        client.publish_notice(&notice).await
    }

    pub(crate) async fn subscribe(&self, owner: &str, sub: Subscription) -> crate::Result<()> {
        if !self.options().is_v5() && subscription_uses_v5(&sub) {
            return Err(N2linkError::NotSupported("MQTT v5 subscription properties are not supported".to_owned()));
        }
        if self.identifier_rejected(&sub) {
            return Err(N2linkError::invalid_operation("MQTT subscription identifiers are not available"));
        }
        let (effective, send) = {
            let mut subs = self.inner.subs.lock().await;
            let owners = subs.entry(sub.topic.clone()).or_default();
            let previous = if owners.is_empty() { None } else { Some(effective_subscription(owners)?) };
            let mut proposed = owners.clone();
            proposed.insert(owner.to_string(), sub.clone());
            let effective = effective_subscription(&proposed)?;
            let send = previous.as_ref() != Some(&effective);
            *owners = proposed;
            (effective, send)
        };
        if send {
            self.note_command(BrokerCommand::Subscribe(effective.clone())).await;
            if let Some(client) = self.inner.client.lock().await.clone()
                && let Err(err) = client.subscribe_one(&effective).await
            {
                let _ = self.unsubscribe(owner, &effective.topic).await;
                return Err(err);
            }
        }
        Ok(())
    }

    pub(crate) async fn unsubscribe(&self, owner: &str, topic: &str) -> crate::Result<()> {
        let outcome = {
            let mut subs = self.inner.subs.lock().await;
            let Some(owners) = subs.get_mut(topic) else {
                return Ok(());
            };
            let previous = effective_subscription(owners).ok();
            owners.remove(owner);
            if owners.is_empty() {
                subs.remove(topic);
                None
            } else {
                let remaining = effective_subscription(owners)?;
                Some((previous, remaining))
            }
        };
        match outcome {
            None => {
                self.note_command(BrokerCommand::Unsubscribe(topic.to_string())).await;
                if let Some(client) = self.inner.client.lock().await.clone() {
                    client.unsubscribe_one(topic).await?;
                }
            }
            Some((previous, remaining)) => {
                if previous.as_ref() != Some(&remaining) {
                    self.note_command(BrokerCommand::Subscribe(remaining.clone())).await;
                    if let Some(client) = self.inner.client.lock().await.clone() {
                        client.subscribe_one(&remaining).await?;
                    }
                }
            }
        }
        Ok(())
    }

    pub(crate) async fn unsubscribe_owner(&self, owner: &str) -> crate::Result<()> {
        let topics: Vec<String> = {
            let subs = self.inner.subs.lock().await;
            subs.iter().filter(|(_, owners)| owners.contains_key(owner)).map(|(topic, _)| topic.clone()).collect()
        };
        for topic in topics {
            self.unsubscribe(owner, &topic).await?;
        }
        Ok(())
    }

    async fn note_command(&self, command: BrokerCommand) {
        self.inner.commands.lock().await.push(command);
    }

    #[cfg(test)]
    async fn owners_of(&self, topic: &str) -> Vec<String> {
        let subs = self.inner.subs.lock().await;
        subs.get(topic).map(|owners| owners.keys().cloned().collect()).unwrap_or_default()
    }

    #[cfg(test)]
    async fn commands(&self) -> Vec<BrokerCommand> {
        self.inner.commands.lock().await.clone()
    }

    #[cfg(test)]
    fn disable_subscription_ids(&self) {
        self.inner.ids_available.store(false, Ordering::Release);
    }

    #[cfg(test)]
    async fn planned_resubscribes(&self) -> crate::Result<Vec<Subscription>> {
        let subs = self.inner.subs.lock().await;
        subs.values().map(effective_subscription).collect()
    }

    /// User `disconnect` action. The socket stays down until [`Self::connect`].
    pub(crate) async fn disconnect(&self) -> crate::Result<()> {
        self.inner.manual_down.store(true, Ordering::Release);
        self.shutdown().await;
        Ok(())
    }

    pub(crate) async fn subscription_fault(&self) -> Option<String> {
        self.inner.fault.lock().await.clone()
    }

    fn watch_status(&self) -> watch::Receiver<LinkState> {
        self.inner.status.subscribe()
    }

    fn state(&self) -> LinkState {
        self.inner.status.borrow().clone()
    }

    fn mark(&self, generation: u64, phase: Phase, retry_at: Option<Instant>) {
        // `send` drops the value when nobody is watching yet. The next waiter must still see it.
        let _ = self.inner.status.send_replace(LinkState { generation, phase, retry_at });
    }

    fn identifier_rejected(&self, sub: &Subscription) -> bool {
        self.options().is_v5()
            && sub.subscription_identifier.unwrap_or(0) != 0
            && !self.inner.ids_available.load(Ordering::Acquire)
    }

    async fn require_client(&self) -> crate::Result<SharedClient> {
        self.inner.client.lock().await.clone().ok_or_else(|| N2linkError::invalid_operation("MQTT connection failed"))
    }

    async fn connected_client(&self) -> crate::Result<SharedClient> {
        if self.inner.manual_down.load(Ordering::Acquire) || !matches!(self.state().phase, Phase::Up) {
            return Err(N2linkError::invalid_operation("mqtt broker is not connected"));
        }
        self.require_client().await
    }

    async fn spawn(&self) -> crate::Result<()> {
        let _gate = self.inner.start_gate.lock().await;
        if self.inner.client.lock().await.is_some() {
            return Ok(());
        }
        let approved = self
            .inner
            .egress
            .approve(EgressPurpose::Mqtt, NetworkProtocol::Mqtt, &self.inner.options.host, self.inner.options.port)
            .await?;
        let mut options = self.inner.options.clone();
        if self.inner.egress.mode() != EgressMode::Off
            && let Some(address) = approved.addresses.first()
        {
            options.host = address.to_string();
        }
        self.inner.opens.fetch_add(1, Ordering::AcqRel);
        let generation = 1;
        self.mark(generation, Phase::Connecting, None);
        let (client, link) = match options.protocol {
            Protocol::V4 => {
                let (client, mut eventloop) = AsyncClient::new(options.v4_options(), 100);
                eventloop.network_options.set_connection_timeout(CONNECT_WAIT.as_secs());
                (SharedClient::V4(client), Link::V4(Box::new(eventloop)))
            }
            Protocol::V5 => {
                let (client, eventloop) = V5Client::new(options.v5_options(), 100);
                (SharedClient::V5(client), Link::V5(Box::new(eventloop)))
            }
        };
        *self.inner.client.lock().await = Some(client.clone());
        let cancel = CancellationToken::new();
        *self.inner.cancel.lock().await = cancel.clone();
        let inner = self.inner.clone();
        let task = tokio::spawn(async move { poll_loop(inner, link, client, cancel, generation).await });
        *self.inner.task.lock().await = Some(task);
        Ok(())
    }

    async fn wait_ready(&self) -> crate::Result<()> {
        let mut status = self.inner.status.subscribe();
        let observed = status.borrow().clone();
        if matches!(observed.phase, Phase::Up) {
            return Ok(());
        }
        let target = wait_generation(&observed);
        let limit = wait_limit(&self.inner.options, &observed);
        let wait = async {
            loop {
                let state = status.borrow().clone();
                if state.generation >= target {
                    match &state.phase {
                        Phase::Up => return Ok(()),
                        Phase::Refused(text) | Phase::Down(text) => {
                            return Err(N2linkError::invalid_operation(&format!("MQTT connection failed: {text}")));
                        }
                        Phase::Idle | Phase::Connecting => {}
                    }
                }
                if status.changed().await.is_err() {
                    return Err(N2linkError::invalid_operation("MQTT connection failed"));
                }
            }
        };
        match tokio::time::timeout(limit, wait).await {
            Ok(result) => result,
            Err(_) => Err(N2linkError::invalid_operation("MQTT connection timed out")),
        }
    }

    async fn shutdown(&self) {
        let cancel = self.inner.cancel.lock().await.clone();
        cancel.cancel();
        let task = self.inner.task.lock().await.take();
        if let Some(task) = task {
            let _ = task.await;
        }
        *self.inner.client.lock().await = None;
        self.mark(0, Phase::Idle, None);
    }
}

fn wait_limit(options: &BrokerOptions, state: &LinkState) -> Duration {
    let remaining = match state.retry_at {
        Some(at) => at.saturating_duration_since(Instant::now()),
        None => match state.phase {
            Phase::Refused(_) | Phase::Down(_) => options.reconnect_period,
            _ => Duration::ZERO,
        },
    };
    remaining + CONNECT_WAIT
}

async fn poll_loop(
    inner: Arc<Inner>,
    mut link: Link,
    client: SharedClient,
    cancel: CancellationToken,
    mut generation: u64,
) {
    loop {
        tokio::select! {
            _ = cancel.cancelled() => {
                let live = matches!(inner.status.borrow().phase, Phase::Up);
                flush_close(&inner, &mut link, &client, live).await;
                break;
            }
            event = link.poll_event() => {
                match event {
                    LinkEvent::Accepted { subscription_ids } => {
                        inner.ids_available.store(subscription_ids, Ordering::Release);
                        resubscribe(&inner, &client).await;
                        publish_notice(&client, inner.options.birth.clone()).await;
                        let _ = inner.status.send_replace(LinkState { generation, phase: Phase::Up, retry_at: None });
                    }
                    LinkEvent::Refused(text) => {
                        if !backoff(&inner, &mut link, &cancel, &mut generation, Phase::Refused(text)).await {
                            break;
                        }
                    }
                    LinkEvent::Publish(frame) => dispatch(&inner, *frame),
                    LinkEvent::Down(text) => {
                        if !backoff(&inner, &mut link, &cancel, &mut generation, Phase::Down(text)).await {
                            break;
                        }
                    }
                    LinkEvent::Ignore => {}
                }
            }
        }
    }
}

/// Drop the socket, publish the failure, then wait one reconnect period.
/// The next attempt uses a new generation so a waiter cannot observe this failure.
async fn backoff(
    inner: &Inner,
    link: &mut Link,
    cancel: &CancellationToken,
    generation: &mut u64,
    phase: Phase,
) -> bool {
    link.clean();
    let retry_at = Instant::now() + inner.options.reconnect_period;
    let _ = inner.status.send_replace(LinkState { generation: *generation, phase, retry_at: Some(retry_at) });
    tokio::select! {
        _ = cancel.cancelled() => false,
        _ = tokio::time::sleep(inner.options.reconnect_period) => {
            *generation = generation.saturating_add(1);
            let _ = inner.status.send_replace(LinkState {
                generation: *generation,
                phase: Phase::Connecting,
                retry_at: None,
            });
            true
        }
    }
}

fn dispatch(inner: &Inner, frame: IncomingPublish) {
    let mut guard = match inner.listeners.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };
    guard.retain(|listener| match listener.tx.try_send(frame.clone()) {
        Ok(()) => true,
        Err(mpsc::error::TrySendError::Full(_)) => {
            listener.overflow.fetch_add(1, Ordering::AcqRel);
            listener.notify.notify_one();
            true
        }
        Err(mpsc::error::TrySendError::Closed(_)) => false,
    });
}

fn qos_rank(qos: QoS) -> u8 {
    match qos {
        QoS::AtMostOnce => 0,
        QoS::AtLeastOnce => 1,
        QoS::ExactlyOnce => 2,
    }
}

fn effective_subscription(owners: &HashMap<String, Subscription>) -> crate::Result<Subscription> {
    let mut iter = owners.values();
    let Some(first) = iter.next() else {
        return Err(N2linkError::invalid_operation("mqtt topic has no owners"));
    };
    let mut effective = first.clone();
    for other in iter {
        if other.nolocal != effective.nolocal
            || other.retain_as_published != effective.retain_as_published
            || other.retain_handling != effective.retain_handling
            || other.subscription_identifier != effective.subscription_identifier
        {
            return Err(N2linkError::invalid_operation("MQTT subscription options conflict on the same topic"));
        }
        if qos_rank(other.qos) > qos_rank(effective.qos) {
            effective.qos = other.qos;
        }
    }
    Ok(effective)
}

async fn resubscribe(inner: &Inner, client: &SharedClient) {
    let union: Vec<Subscription> = {
        let subs = inner.subs.lock().await;
        match subs.values().map(effective_subscription).collect::<crate::Result<Vec<_>>>() {
            Ok(union) => union,
            Err(err) => {
                *inner.fault.lock().await = Some(err.to_string());
                return;
            }
        }
    };
    let mut fault = None;
    for sub in union {
        if inner.options.is_v5()
            && sub.subscription_identifier.unwrap_or(0) != 0
            && !inner.ids_available.load(Ordering::Acquire)
        {
            fault = Some("MQTT subscription identifiers are not available".to_owned());
            continue;
        }
        inner.commands.lock().await.push(BrokerCommand::Subscribe(sub.clone()));
        if let Err(err) = client.subscribe_one(&sub).await {
            log::warn!("MQTT resubscribe failed: {err}");
        }
    }
    *inner.fault.lock().await = fault;
}

async fn publish_notice(client: &SharedClient, notice: Option<MqttNotice>) {
    let Some(notice) = notice else {
        return;
    };
    if let Err(err) = client.publish_notice(&notice).await {
        log::warn!("MQTT notice publish failed: {err}");
    }
}

async fn flush_close(inner: &Inner, link: &mut Link, client: &SharedClient, live: bool) {
    if live && let Some(close) = inner.options.close.clone() {
        let _ = client.publish_notice(&close).await;
        let _ = tokio::time::timeout(CLOSE_FLUSH, link.poll_event()).await;
    }
    client.disconnect_link().await;
}

pub(crate) async fn paint_connection<N>(session: &BrokerSession, node: &N, stop: &CancellationToken)
where
    N: FlowNodeBehavior,
{
    let mut watch = session.watch_status();
    loop {
        let state = watch.borrow().clone();
        if matches!(state.phase, Phase::Up)
            && let Some(fault) = session.subscription_fault().await
        {
            node.report_error(fault, MsgHandle::new(Msg::default()), stop.child_token()).await;
        }
        node.report_status(status_view(&state), stop.child_token()).await;
        tokio::select! {
            _ = stop.cancelled() => break,
            changed = watch.changed() => {
                if changed.is_err() {
                    break;
                }
            }
        }
    }
}

/// Look up the broker config node. A missing or wrong id is an error.
pub(crate) fn attach_broker(flow: &Flow, broker_id: &str) -> crate::Result<BrokerSession> {
    if broker_id.is_empty() {
        return Err(N2linkError::invalid_operation("mqtt node has no broker"));
    }
    let id: ElementId = broker_id
        .parse()
        .map_err(|_| N2linkError::invalid_operation(&format!("mqtt broker id '{broker_id}' is not a node id")))?;
    let engine = flow.engine().ok_or_else(|| N2linkError::invalid_operation("mqtt node has no engine"))?;
    let global = engine
        .find_global_node_by_id(&id)
        .ok_or_else(|| N2linkError::invalid_operation(&format!("mqtt broker '{id}' was not loaded")))?;
    global
        .as_any()
        .downcast_ref::<MqttBrokerNode>()
        .map(|node| node.session.clone())
        .ok_or_else(|| N2linkError::invalid_operation(&format!("node '{id}' is not an mqtt-broker")))
}

pub(crate) fn topic_matches(filter: &str, topic: &str) -> bool {
    let pattern: Vec<&str> = filter.split('/').collect();
    let name: Vec<&str> = topic.split('/').collect();
    let mut pattern_index = 0;
    let mut name_index = 0;
    while pattern_index < pattern.len() {
        if pattern[pattern_index] == "#" {
            return pattern_index == pattern.len() - 1;
        }
        if name_index >= name.len() {
            return false;
        }
        if pattern[pattern_index] != "+" && pattern[pattern_index] != name[name_index] {
            return false;
        }
        pattern_index += 1;
        name_index += 1;
    }
    name_index == name.len()
}

pub(crate) fn qos_of(value: Option<&Value>, default_level: u8) -> QoS {
    let level = value.and_then(number_u64).map(|level| level as u8).unwrap_or(default_level);
    match level {
        0 => QoS::AtMostOnce,
        1 => QoS::AtLeastOnce,
        2 => QoS::ExactlyOnce,
        _ => match default_level {
            0 => QoS::AtMostOnce,
            1 => QoS::AtLeastOnce,
            _ => QoS::ExactlyOnce,
        },
    }
}

/// Build the connection options the editor saved, or reject a feature we do not implement.
pub(crate) fn resolve_broker(value: &Value) -> crate::Result<BrokerOptions> {
    reject_transport(value)?;
    let protocol = protocol_of(value)?;
    let (host, port, brokerurl) = endpoint(value)?;
    let mut client_id = json_str(value, "clientid").unwrap_or("").trim().to_owned();
    if client_id.is_empty() {
        let generated = uuid::Uuid::new_v4().simple().to_string();
        client_id = format!("nodered{}", &generated[..16]);
    }
    let mut clean = json_bool(value, "cleansession").unwrap_or(true);
    if !clean && json_str(value, "clientid").unwrap_or("").trim().is_empty() {
        // Node-RED forces a clean session when no client id was supplied.
        clean = true;
    }
    let keepalive_secs = match value.get("keepalive") {
        None | Some(Value::Null) => DEFAULT_KEEPALIVE_SECS,
        Some(other) => {
            number_u64(other).ok_or_else(|| N2linkError::invalid_operation("mqtt keepalive is not a number"))?
        }
    };
    if keepalive_secs > u64::from(u16::MAX) {
        return Err(N2linkError::invalid_operation("mqtt keepalive is out of range"));
    }
    if protocol == Protocol::V5 && keepalive_secs < 5 {
        return Err(N2linkError::NotSupported("MQTT v5 keep alive below 5 seconds is not supported".to_owned()));
    }
    let connect = connect_settings(value, protocol)?;
    let (username, password) = credentials(value);
    Ok(BrokerOptions {
        host,
        port,
        brokerurl,
        client_id,
        keepalive_secs,
        clean,
        auto_connect: json_bool(value, "autoConnect").unwrap_or(true),
        auto_unsubscribe: match value.get("autoUnsubscribe") {
            Some(Value::Bool(flag)) => *flag,
            _ => true,
        },
        reconnect_period: DEFAULT_RECONNECT,
        username,
        password,
        protocol,
        connect,
        will: notice(value, "will", protocol)?,
        birth: notice(value, "birth", protocol)?,
        close: notice(value, "close", protocol)?,
    })
}

fn protocol_of(value: &Value) -> crate::Result<Protocol> {
    match value.get("protocolVersion") {
        None | Some(Value::Null) => Ok(Protocol::V4),
        Some(other) => match number_u64(other) {
            Some(4) => Ok(Protocol::V4),
            Some(5) => Ok(Protocol::V5),
            Some(3) => Err(N2linkError::NotSupported("MQTT 3.1 compatibility mode is not supported".to_owned())),
            Some(version) => {
                Err(N2linkError::NotSupported(format!("MQTT protocol version {version} is not supported")))
            }
            None => Err(N2linkError::invalid_operation("mqtt protocolVersion is not a number")),
        },
    }
}

fn reject_transport(value: &Value) -> crate::Result<()> {
    if json_bool(value, "usetls").unwrap_or(false) || matches!(json_str(value, "tls"), Some(tls) if !tls.is_empty()) {
        return Err(N2linkError::NotSupported("MQTT TLS is not supported".to_owned()));
    }
    if json_bool(value, "compatmode").unwrap_or(false) {
        return Err(N2linkError::NotSupported("MQTT 3.1 compatibility mode is not supported".to_owned()));
    }
    for key in ["topicAliasMaximum", "maximumPacketSize", "receiveMaximum"] {
        if value.get(key).and_then(number_u64).unwrap_or(0) != 0 {
            return Err(N2linkError::NotSupported(format!("MQTT {key} is not supported")));
        }
    }
    for key in ["authenticationMethod", "authMethod"] {
        if json_str(value, key).is_some_and(|text| !text.trim().is_empty()) {
            return Err(N2linkError::NotSupported("MQTT enhanced authentication is not supported".to_owned()));
        }
    }
    for key in ["url", "broker"] {
        if let Some(raw) = json_str(value, key) {
            let lower = raw.trim().to_ascii_lowercase();
            if lower.starts_with("ws://") || lower.starts_with("wss://") {
                return Err(N2linkError::NotSupported("MQTT WebSocket URLs are not supported".to_owned()));
            }
            if lower.starts_with("mqtts://") || lower.starts_with("ssl://") {
                return Err(N2linkError::NotSupported("MQTT TLS is not supported".to_owned()));
            }
        }
    }
    Ok(())
}

fn connect_settings(value: &Value, protocol: Protocol) -> crate::Result<ConnectSettings> {
    let user_properties = user_properties_field(value, protocol)?;
    let session_expiry = session_expiry_field(value, protocol)?;
    Ok(ConnectSettings { session_expiry, user_properties })
}

fn user_properties_field(value: &Value, protocol: Protocol) -> crate::Result<Vec<(String, String)>> {
    let Some(raw) = value.get("userProps").or_else(|| value.get("userProperties")) else {
        return Ok(Vec::new());
    };
    if raw.is_null() {
        return Ok(Vec::new());
    }
    if let Value::String(text) = raw
        && text.trim().is_empty()
    {
        return Ok(Vec::new());
    }
    if protocol != Protocol::V5 {
        if field_is_set(Some(raw)) {
            return Err(N2linkError::NotSupported("MQTT v5 properties are not supported".to_owned()));
        }
        return Ok(Vec::new());
    }
    parse_user_properties(raw)
}

fn session_expiry_field(value: &Value, protocol: Protocol) -> crate::Result<Option<u32>> {
    let Some(raw) = value.get("sessionExpiry").or_else(|| value.get("sessionExpiryInterval")) else {
        return Ok(None);
    };
    if !field_is_set(Some(raw)) {
        return Ok(None);
    }
    if protocol != Protocol::V5 {
        return Err(N2linkError::NotSupported("MQTT v5 properties are not supported".to_owned()));
    }
    optional_u32(raw, "sessionExpiry")
}

fn endpoint(value: &Value) -> crate::Result<(String, u16, String)> {
    let url = json_str(value, "url").unwrap_or("").trim().to_owned();
    if !url.is_empty() {
        return split_endpoint(&url);
    }
    let broker = json_str(value, "broker").unwrap_or("").trim().to_owned();
    if broker.contains("://") {
        return split_endpoint(&broker);
    }
    let host = if broker.is_empty() { "localhost".to_owned() } else { broker };
    let port = match value.get("port") {
        None | Some(Value::Null) => DEFAULT_PORT,
        Some(Value::String(text)) if text.trim().is_empty() => DEFAULT_PORT,
        Some(other) => {
            let port = number_u64(other).ok_or_else(|| N2linkError::invalid_operation("mqtt port is not a number"))?;
            u16::try_from(port).map_err(|_| N2linkError::invalid_operation("mqtt port is out of range"))?
        }
    };
    let port = if port == 0 { DEFAULT_PORT } else { port };
    let brokerurl =
        if host.contains(':') { format!("mqtt://[{host}]:{port}") } else { format!("mqtt://{host}:{port}") };
    Ok((host, port, brokerurl))
}

fn split_endpoint(raw: &str) -> crate::Result<(String, u16, String)> {
    let parsed =
        url::Url::parse(raw).map_err(|_| N2linkError::invalid_operation(&format!("mqtt url '{raw}' is invalid")))?;
    match parsed.scheme() {
        "mqtt" | "tcp" => {}
        "ws" | "wss" => return Err(N2linkError::NotSupported("MQTT WebSocket URLs are not supported".to_owned())),
        "mqtts" | "ssl" => return Err(N2linkError::NotSupported("MQTT TLS is not supported".to_owned())),
        other => return Err(N2linkError::NotSupported(format!("MQTT URL scheme '{other}' is not supported"))),
    }
    let host = parsed.host_str().filter(|host| !host.is_empty()).unwrap_or("localhost").to_owned();
    let port = parsed.port().unwrap_or(DEFAULT_PORT);
    let brokerurl =
        if host.contains(':') { format!("mqtt://[{host}]:{port}") } else { format!("mqtt://{host}:{port}") };
    Ok((host, port, brokerurl))
}

fn notice(value: &Value, prefix: &str, protocol: Protocol) -> crate::Result<Option<MqttNotice>> {
    let topic_key = format!("{prefix}Topic");
    let topic = json_str(value, &topic_key).unwrap_or("").trim().to_owned();
    if topic.is_empty() {
        let section_key = format!("{prefix}Msg");
        if protocol != Protocol::V5 && field_is_set(value.get(&section_key)) && section_has_v5(value.get(&section_key))
        {
            return Err(N2linkError::NotSupported("MQTT v5 properties are not supported".to_owned()));
        }
        return Ok(None);
    }
    if !valid_publish_topic(&topic) {
        return Err(N2linkError::invalid_operation(&format!("mqtt {prefix} topic '{topic}' is invalid")));
    }
    let payload_key = format!("{prefix}Payload");
    let payload = match value.get(&payload_key) {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::String(text)) => text.as_bytes().to_vec(),
        Some(other) => other.to_string().into_bytes(),
    };
    let qos_key = format!("{prefix}Qos");
    let qos = qos_of(value.get(&qos_key), 0);
    let retain = json_bool(value, &format!("{prefix}Retain")).unwrap_or(false);
    let (props, will_delay) = section_props(value, prefix, protocol)?;
    Ok(Some(MqttNotice { topic, payload, qos, retain, props, will_delay }))
}

fn section_props(value: &Value, prefix: &str, protocol: Protocol) -> crate::Result<(PublishProps, Option<u32>)> {
    let key = format!("{prefix}Msg");
    let Some(section) = value.get(&key) else {
        return Ok((PublishProps::default(), None));
    };
    if section.is_null() || !section_has_v5(Some(section)) {
        return Ok((PublishProps::default(), None));
    }
    if protocol != Protocol::V5 {
        return Err(N2linkError::NotSupported("MQTT v5 properties are not supported".to_owned()));
    }
    let Value::Object(_) = section else {
        return Err(N2linkError::invalid_operation(&format!("mqtt {key} is not an object")));
    };
    let props = PublishProps {
        response_topic: optional_text(section, "respTopic")?,
        correlation_data: optional_text(section, "correl")?.map(|text| text.into_bytes()),
        content_type: optional_text(section, "contentType")?,
        message_expiry: match section.get("expiry") {
            None => None,
            Some(raw) => optional_u32(raw, "expiry")?,
        },
        user_properties: match section.get("userProps").or_else(|| section.get("userProperties")) {
            None => Vec::new(),
            Some(raw) if !field_is_set(Some(raw)) => Vec::new(),
            Some(raw) => parse_user_properties(raw)?,
        },
    };
    let will_delay = if prefix == "will" {
        match section.get("delay") {
            None => None,
            Some(raw) => optional_u32(raw, "delay")?,
        }
    } else if field_is_set(section.get("delay")) {
        return Err(N2linkError::NotSupported("MQTT will delay is only valid on the will".to_owned()));
    } else {
        None
    };
    Ok((props, will_delay))
}

fn section_has_v5(section: Option<&Value>) -> bool {
    let Some(Value::Object(map)) = section else {
        return false;
    };
    ["contentType", "userProps", "userProperties", "respTopic", "correl", "expiry", "delay"]
        .iter()
        .any(|key| field_is_set(map.get(*key)))
}

fn optional_text(section: &Value, key: &str) -> crate::Result<Option<String>> {
    match section.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(text)) => {
            let text = text.trim();
            if text.is_empty() { Ok(None) } else { Ok(Some(text.to_owned())) }
        }
        Some(_) => Err(N2linkError::invalid_operation(&format!("mqtt {key} is not a string"))),
    }
}

fn field_is_set(value: Option<&Value>) -> bool {
    match value {
        None | Some(Value::Null) => false,
        Some(Value::String(text)) => {
            let text = text.trim();
            !text.is_empty() && text != "0" && text != "{}"
        }
        Some(Value::Number(number)) => number.as_u64().unwrap_or(1) != 0,
        Some(Value::Object(map)) => !map.is_empty(),
        Some(Value::Array(items)) => !items.is_empty(),
        Some(Value::Bool(flag)) => *flag,
    }
}

fn parse_user_properties(raw: &Value) -> crate::Result<Vec<(String, String)>> {
    let value = match raw {
        Value::String(text) => serde_json::from_str::<Value>(text.trim())
            .map_err(|_| N2linkError::invalid_operation("mqtt userProps is not a JSON object of strings"))?,
        other => other.clone(),
    };
    let Value::Object(map) = value else {
        return Err(N2linkError::invalid_operation("mqtt userProps is not a JSON object of strings"));
    };
    let mut pairs = Vec::with_capacity(map.len());
    for (key, item) in map {
        let Value::String(text) = item else {
            return Err(N2linkError::invalid_operation("mqtt userProps is not a JSON object of strings"));
        };
        pairs.push((key, text));
    }
    Ok(pairs)
}

fn optional_u32(raw: &Value, name: &str) -> crate::Result<Option<u32>> {
    match raw {
        Value::Null => Ok(None),
        Value::String(text) if text.trim().is_empty() || text.trim() == "0" => Ok(None),
        Value::Number(number) if number.as_u64() == Some(0) => Ok(None),
        Value::Number(number) => number
            .as_u64()
            .and_then(|value| u32::try_from(value).ok())
            .map(Some)
            .ok_or_else(|| N2linkError::invalid_operation(&format!("mqtt {name} is out of range"))),
        Value::String(text) => text
            .trim()
            .parse::<u32>()
            .map(|value| if value == 0 { None } else { Some(value) })
            .map_err(|_| N2linkError::invalid_operation(&format!("mqtt {name} is not a number"))),
        _ => Err(N2linkError::invalid_operation(&format!("mqtt {name} is not a number"))),
    }
}

fn credentials(value: &Value) -> (Option<String>, Option<String>) {
    let from_node = json_str(value, "username").or_else(|| json_str(value, "user")).filter(|text| !text.is_empty());
    let from_creds = value
        .get("credentials")
        .and_then(|creds| json_str(creds, "user").or_else(|| json_str(creds, "username")))
        .filter(|text| !text.is_empty());
    let username = from_node.or(from_creds).map(str::to_owned);
    if username.is_none() {
        return (None, None);
    }
    let password = json_str(value, "password")
        .or_else(|| value.get("credentials").and_then(|creds| json_str(creds, "password")))
        .unwrap_or("")
        .to_owned();
    (username, Some(password))
}

fn valid_publish_topic(topic: &str) -> bool {
    !topic.is_empty()
        && !topic
            .chars()
            .any(|ch| matches!(ch, '+' | '#' | '\u{0008}' | '\u{000c}' | '\n' | '\r' | '\t' | '\u{000b}' | '\0'))
}

fn json_str<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value.get(key).and_then(Value::as_str)
}

fn json_bool(value: &Value, key: &str) -> Option<bool> {
    match value.get(key)? {
        Value::Bool(flag) => Some(*flag),
        Value::String(text) if text == "true" => Some(true),
        Value::String(text) if text == "false" => Some(false),
        _ => None,
    }
}

fn number_u64(value: &Value) -> Option<u64> {
    match value {
        Value::Number(number) => number.as_u64(),
        Value::String(text) => text.trim().parse().ok(),
        _ => None,
    }
}

fn subscription_uses_v5(sub: &Subscription) -> bool {
    sub.nolocal || !sub.retain_as_published || sub.retain_handling != 0 || sub.subscription_identifier.unwrap_or(0) != 0
}

fn v5_qos(qos: QoS) -> V5Qos {
    match qos {
        QoS::AtMostOnce => V5Qos::AtMostOnce,
        QoS::AtLeastOnce => V5Qos::AtLeastOnce,
        QoS::ExactlyOnce => V5Qos::ExactlyOnce,
    }
}

fn qos_from_v5(qos: V5Qos) -> QoS {
    match qos {
        V5Qos::AtMostOnce => QoS::AtMostOnce,
        V5Qos::AtLeastOnce => QoS::AtLeastOnce,
        V5Qos::ExactlyOnce => QoS::ExactlyOnce,
    }
}

/// `Filter::new` leaves retain-as-published false. The editor default is true, so the bits are set here.
fn v5_filter(sub: &Subscription) -> Filter {
    Filter {
        path: sub.topic.clone(),
        qos: v5_qos(sub.qos),
        nolocal: sub.nolocal,
        preserve_retain: sub.retain_as_published,
        retain_forward_rule: match sub.retain_handling {
            1 => RetainForwardRule::OnNewSubscribe,
            2 => RetainForwardRule::Never,
            _ => RetainForwardRule::OnEverySubscribe,
        },
    }
}

fn v5_subscribe_properties(sub: &Subscription) -> Option<SubscribeProperties> {
    let id = sub.subscription_identifier.filter(|id| *id != 0)?;
    Some(SubscribeProperties { id: Some(usize::try_from(id).unwrap_or(usize::MAX)), user_properties: Vec::new() })
}

fn v5_publish_props(props: &PublishProps) -> Option<PublishProperties> {
    if !props.is_set() {
        return None;
    }
    Some(PublishProperties {
        response_topic: props.response_topic.clone(),
        correlation_data: props.correlation_data.clone().map(bytes::Bytes::from),
        content_type: props.content_type.clone(),
        message_expiry_interval: props.message_expiry,
        user_properties: props.user_properties.clone(),
        ..PublishProperties::default()
    })
}

fn v5_last_will(notice: &MqttNotice) -> V5LastWill {
    let properties = if notice.props.is_set() || notice.will_delay.is_some() {
        Some(LastWillProperties {
            delay_interval: notice.will_delay,
            payload_format_indicator: None,
            message_expiry_interval: notice.props.message_expiry,
            content_type: notice.props.content_type.clone(),
            response_topic: notice.props.response_topic.clone(),
            correlation_data: notice.props.correlation_data.clone().map(bytes::Bytes::from),
            user_properties: notice.props.user_properties.clone(),
        })
    } else {
        None
    };
    V5LastWill::new(notice.topic.clone(), notice.payload.clone(), v5_qos(notice.qos), notice.retain, properties)
}

fn subscription_ids_available(properties: Option<&rumqttc::v5::mqttbytes::v5::ConnAckProperties>) -> bool {
    match properties.and_then(|props| props.subscription_identifiers_available) {
        // Absent means available. That is the MQTT 5.0 default.
        None => true,
        Some(0) => false,
        Some(_) => true,
    }
}

fn incoming_v5(publish: rumqttc::v5::mqttbytes::v5::Publish) -> IncomingPublish {
    let properties = publish.properties.as_ref().map(|props| IncomingProperties {
        response_topic: props.response_topic.clone(),
        correlation_data: props.correlation_data.clone(),
        content_type: props.content_type.clone(),
        user_properties: props.user_properties.clone(),
        message_expiry: props.message_expiry_interval,
        subscription_identifiers: props.subscription_identifiers.clone(),
        payload_format_utf8: props.payload_format_indicator.map(|flag| flag == 1),
        reason_string: None,
    });
    IncomingPublish {
        topic: String::from_utf8_lossy(&publish.topic).into_owned(),
        payload: publish.payload,
        qos: qos_from_v5(publish.qos),
        retain: publish.retain,
        properties,
    }
}

crate::node_hints!("mqtt-broker", secrets = ["user", "password"], caps = ["network"]);

#[global_node("mqtt-broker", red_name = "mqtt-broker", module = "node-red")]
pub struct MqttBrokerNode {
    base: BaseGlobalNodeState,
    session: BrokerSession,
}

impl MqttBrokerNode {
    fn build(
        engine: &crate::runtime::engine::Engine,
        config: &RedGlobalNodeConfig,
        _options: Option<&config::Config>,
    ) -> crate::Result<Box<dyn GlobalNodeBehavior>> {
        let options = resolve_broker(&config.rest)?;
        let session = BrokerSession::from_options_with_policy(options, engine.egress_policy().clone());
        let node = MqttBrokerNode {
            base: BaseGlobalNodeState {
                id: config.id,
                name: config.name.clone(),
                type_str: "mqtt-broker",
                ordering: config.ordering,
                context: engine.get_context_manager().new_context(engine.context(), config.id.to_string()),
                disabled: config.disabled,
            },
            session,
        };
        Ok(Box::new(node))
    }
}

#[async_trait::async_trait]
impl GlobalNodeBehavior for MqttBrokerNode {
    fn get_base(&self) -> &BaseGlobalNodeState {
        &self.base
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;
    use serde_json::json;

    fn options(value: serde_json::Value) -> BrokerOptions {
        resolve_broker(&value).unwrap()
    }

    #[test]
    fn v4_defaults_match_the_load_spec() {
        let opts = options(json!({ "broker": "localhost", "autoConnect": false }));
        assert_eq!(opts.host(), "localhost");
        assert_eq!(opts.port(), 1883);
        assert!(opts.brokerurl().starts_with("mqtt://localhost:1883"));
        assert!(!opts.auto_connect());
        assert!(opts.auto_unsubscribe());
        assert!(opts.clean());
        assert!(opts.client_id().contains("nodered"));
        assert_eq!(opts.keepalive_secs(), 60);
        assert_eq!(opts.reconnect_period(), DEFAULT_RECONNECT);
        assert!(!opts.is_v5());
    }

    #[test]
    fn v4_connect_is_not_protocol_level_5() {
        let opts = options(json!({ "broker": "localhost", "clientid": "clientid" }));
        assert!(!opts.is_v5());
        let mut connect = rumqttc::Connect::new(opts.client_id());
        connect.clean_session = opts.clean();
        connect.keep_alive = u16::try_from(opts.keepalive_secs()).unwrap();
        let mut buf = bytes::BytesMut::new();
        connect.write(&mut buf).unwrap();
        assert!(buf.windows(5).any(|window| window == b"MQTT\x04"));
        assert!(!buf.windows(5).any(|window| window == b"MQTT\x05"));
    }

    #[test]
    fn clean_session_false_is_kept_when_a_client_id_is_set() {
        let opts = options(json!({
            "broker": "plant",
            "port": "1884",
            "clientid": "clientid",
            "keepalive": 35,
            "cleansession": false
        }));
        assert_eq!(opts.host(), "plant");
        assert_eq!(opts.port(), 1884);
        assert_eq!(opts.client_id(), "clientid");
        assert_eq!(opts.keepalive_secs(), 35);
        assert!(!opts.clean());
    }

    #[test]
    fn clean_session_without_a_client_id_is_forced_on() {
        let opts = options(json!({ "broker": "localhost", "cleansession": false }));
        assert!(opts.clean());
        assert!(opts.client_id().contains("nodered"));
    }

    #[test]
    fn empty_broker_uses_localhost() {
        let opts = options(json!({}));
        assert_eq!(opts.host(), "localhost");
        assert_eq!(opts.port(), 1883);
        assert!(opts.auto_connect());
    }

    #[test]
    fn url_form_is_accepted() {
        let opts = options(json!({ "url": "mqtt://broker.local:1885" }));
        assert_eq!(opts.host(), "broker.local");
        assert_eq!(opts.port(), 1885);
    }

    #[test]
    fn will_birth_and_close_are_kept() {
        let opts = options(json!({
            "broker": "localhost",
            "willTopic": "plant/down",
            "willPayload": "gone",
            "willQos": 1,
            "willRetain": true,
            "birthTopic": "plant/up",
            "birthPayload": "here",
            "closeTopic": "plant/close",
            "closePayload": "bye"
        }));
        assert_eq!(opts.will.as_ref().unwrap().topic, "plant/down");
        assert_eq!(opts.will.as_ref().unwrap().payload, b"gone");
        assert_eq!(opts.birth.as_ref().unwrap().topic, "plant/up");
        assert_eq!(opts.close.as_ref().unwrap().topic, "plant/close");
    }

    #[test]
    fn v5_connect_properties_follow_the_editor() {
        let opts = options(json!({
            "broker": "localhost",
            "protocolVersion": 5,
            "clientid": "clientid",
            "sessionExpiry": "6000",
            "userProps": "{\"prop\":\"val\"}",
            "birthTopic": "plant/up",
            "birthMsg": { "respTopic": "reply", "contentType": "text/plain" },
            "willTopic": "plant/down",
            "willMsg": { "delay": "10", "respTopic": "down-reply" }
        }));
        assert!(opts.is_v5());
        let props = opts.v5_connect_properties();
        assert_eq!(props.session_expiry_interval, Some(6000));
        assert_eq!(props.user_properties, vec![("prop".to_owned(), "val".to_owned())]);
        assert_eq!(props.request_response_info, Some(1));
        assert_eq!(props.request_problem_info, Some(1));
        assert_eq!(opts.birth.as_ref().unwrap().props.response_topic.as_deref(), Some("reply"));
        assert_eq!(opts.birth.as_ref().unwrap().props.content_type.as_deref(), Some("text/plain"));
        assert_eq!(opts.will.as_ref().unwrap().will_delay, Some(10));
        assert_eq!(opts.will.as_ref().unwrap().props.response_topic.as_deref(), Some("down-reply"));
    }

    #[test]
    fn a_bad_user_props_value_fails_at_deploy() {
        let Err(err) = resolve_broker(&json!({
            "broker": "localhost",
            "protocolVersion": 5,
            "userProps": "nope"
        })) else {
            panic!("bad user props must fail at deploy");
        };
        assert!(err.to_string().contains("userProps"), "{err}");
    }

    #[test]
    fn v5_keep_alive_below_five_seconds_is_rejected() {
        let Err(err) = resolve_broker(&json!({
            "broker": "localhost",
            "protocolVersion": 5,
            "keepalive": 0
        })) else {
            panic!("v5 keep alive below 5 seconds must be rejected");
        };
        assert!(err.to_string().starts_with("not supported"), "{err}");
    }

    #[test]
    fn v5_tls_and_websocket_are_rejected() {
        for value in [
            json!({ "broker": "localhost", "usetls": true }),
            json!({ "broker": "localhost", "tls": "abc" }),
            json!({ "broker": "ws://localhost:9001" }),
            json!({ "url": "wss://localhost/mqtt" }),
            json!({ "broker": "mqtts://localhost:8883" }),
            json!({ "broker": "localhost", "compatmode": true }),
            json!({ "broker": "localhost", "protocolVersion": 3 }),
            json!({ "broker": "localhost", "sessionExpiry": "6000" }),
            json!({ "broker": "localhost", "userProps": "{\"prop\":\"val\"}" }),
            json!({ "broker": "localhost", "protocolVersion": 5, "topicAliasMaximum": 4 }),
            json!({ "broker": "localhost", "protocolVersion": 5, "receiveMaximum": 8 }),
            json!({ "broker": "localhost", "protocolVersion": 5, "maximumPacketSize": 1024 }),
            json!({ "broker": "localhost", "protocolVersion": 5, "authenticationMethod": "SCRAM-SHA-256" }),
        ] {
            let Err(err) = resolve_broker(&value) else {
                panic!("transport, protocol 3, and unimplemented v5 fields must be rejected: {value}");
            };
            assert!(err.to_string().starts_with("not supported"), "{err}");
        }
    }

    #[test]
    fn a_v5_filter_keeps_the_editor_subscription_bits() {
        let sub = Subscription {
            topic: "plant/speed".to_owned(),
            qos: QoS::AtLeastOnce,
            nolocal: true,
            retain_as_published: true,
            retain_handling: 2,
            subscription_identifier: Some(7),
        };
        let filter = v5_filter(&sub);
        assert!(filter.nolocal);
        assert!(filter.preserve_retain);
        assert_eq!(filter.retain_forward_rule, RetainForwardRule::Never);
        assert_eq!(filter.qos, V5Qos::AtLeastOnce);
        let props = v5_subscribe_properties(&sub).unwrap();
        assert_eq!(props.id, Some(7));
    }

    #[test]
    fn an_absent_subscription_identifier_property_means_available() {
        assert!(subscription_ids_available(None));
    }

    #[test]
    fn a_refused_generation_is_not_the_next_wait() {
        let refused = LinkState { generation: 3, phase: Phase::Refused("no".to_owned()), retry_at: None };
        assert_eq!(wait_generation(&refused), 4);
        let up = LinkState { generation: 3, phase: Phase::Up, retry_at: None };
        assert_eq!(wait_generation(&up), 3);
    }

    #[test]
    fn topic_filter_honours_wildcards() {
        assert!(topic_matches("plant/+/speed", "plant/1/speed"));
        assert!(!topic_matches("plant/+/speed", "plant/1/speed/extra"));
        assert!(topic_matches("plant/#", "plant/1/speed"));
        assert!(!topic_matches("plant/#", "other"));
    }

    async fn prime(session: &BrokerSession, phase: Phase, generation: u64) {
        let (client, eventloop) = AsyncClient::new(MqttOptions::new("clientid", "localhost", 1883), 8);
        drop(eventloop);
        *session.inner.client.lock().await = Some(SharedClient::V4(client));
        let retry_at =
            matches!(phase, Phase::Refused(_) | Phase::Down(_)).then(|| Instant::now() + Duration::from_secs(30));
        session.mark(generation, phase, retry_at);
    }

    #[tokio::test]
    async fn a_second_connect_while_up_does_not_open_another_session() {
        let session = BrokerSession::from_options(options(json!({ "broker": "localhost", "autoConnect": false })));
        prime(&session, Phase::Up, 1).await;
        session.connect().await.unwrap();
        session.connect().await.unwrap();
        assert_eq!(session.inner.opens.load(Ordering::Acquire), 0);
        assert!(session.inner.task.lock().await.is_none());
    }

    #[tokio::test]
    async fn a_connect_after_refusal_waits_for_the_next_generation() {
        let session = BrokerSession::from_options(options(json!({ "broker": "localhost", "autoConnect": false })));
        prime(&session, Phase::Refused("connection refused: NotAuthorized".to_owned()), 1).await;
        let flag = Arc::new(AtomicBool::new(false));
        let done = flag.clone();
        let waiting = session.clone();
        tokio::spawn(async move {
            waiting.connect().await.unwrap();
            done.store(true, Ordering::Release);
        });
        tokio::time::sleep(Duration::from_millis(80)).await;
        assert!(!flag.load(Ordering::Acquire), "the old refusal completed the wait");
        assert_eq!(session.inner.opens.load(Ordering::Acquire), 0);
        session.mark(2, Phase::Up, None);
        tokio::time::sleep(Duration::from_millis(80)).await;
        assert!(flag.load(Ordering::Acquire));
    }

    #[tokio::test]
    async fn a_missing_broker_node_is_rejected_at_deploy() {
        let flows = json!([
            { "id": "100", "type": "tab" },
            { "id": "2", "z": "100", "type": "mqtt out", "broker": "99", "topic": "out", "wires": [] }
        ]);
        let err = crate::runtime::engine::build_test_engine(flows).unwrap_err();
        let text = err.to_string();
        assert!(text.contains("was not loaded"), "{text}");
    }

    #[tokio::test]
    async fn mqtt_out_deploys_against_the_broker_node() {
        let flows = json!([
            { "id": "100", "type": "tab" },
            { "id": "b1", "type": "mqtt-broker", "broker": "localhost", "port": 1883, "autoConnect": false },
            { "id": "2", "z": "100", "type": "mqtt out", "broker": "b1", "topic": "out", "wires": [] }
        ]);
        crate::runtime::engine::build_test_engine(flows).unwrap();
    }

    #[tokio::test]
    async fn a_v5_broker_deploys() {
        let flows = json!([
            { "id": "b1", "type": "mqtt-broker", "broker": "localhost", "protocolVersion": 5, "clientid": "clientid" }
        ]);
        crate::runtime::engine::build_test_engine(flows).unwrap();
    }

    #[tokio::test]
    async fn a_v4_broker_rejects_a_response_topic() {
        let flows = json!([
            { "id": "100", "type": "tab" },
            { "id": "b1", "type": "mqtt-broker", "broker": "localhost", "autoConnect": false },
            { "id": "2", "z": "100", "type": "mqtt out", "broker": "b1", "topic": "out", "respTopic": "reply", "wires": [] }
        ]);
        let err = crate::runtime::engine::build_test_engine(flows).unwrap_err();
        assert!(err.to_string().contains("not supported"), "{err}");
    }

    #[tokio::test]
    async fn a_v5_broker_accepts_a_response_topic() {
        let flows = json!([
            { "id": "100", "type": "tab" },
            { "id": "b1", "type": "mqtt-broker", "broker": "localhost", "protocolVersion": 5, "clientid": "clientid", "autoConnect": false },
            { "id": "2", "z": "100", "type": "mqtt out", "broker": "b1", "topic": "out", "respTopic": "reply", "wires": [] }
        ]);
        crate::runtime::engine::build_test_engine(flows).unwrap();
    }

    /// Round-trip against a broker already listening on 127.0.0.1:1883.
    /// Set EDGELINK_MQTT_LIVE=1. The unit suite stays quiet without it.
    /// Optional EDGELINK_MQTT_USER and EDGELINK_MQTT_PASSWORD are sent on CONNECT.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_live_broker_round_trip_on_localhost() {
        if std::env::var("EDGELINK_MQTT_LIVE").ok().as_deref() != Some("1") {
            return;
        }
        live_round_trip(4, "edgelinkd/live/v4", "edgelink-live-v4", "ping-v4", None).await;
        live_round_trip(5, "edgelinkd/live/v5", "edgelink-live-v5", "ping-v5", Some("edgelinkd/live/v5/reply")).await;
    }

    async fn live_round_trip(protocol: u8, topic: &str, client_id: &str, payload: &str, response_topic: Option<&str>) {
        // ElementId deserializes hex only. "in"/"out" hash in the flow loader and then
        // fail the inject tuple, so the publish never reaches the out node.
        let mut out = json!({
            "id": "3", "z": "100", "type": "mqtt out", "broker": "b1",
            "topic": topic, "qos": 0, "retain": true, "wires": []
        });
        if let Some(reply) = response_topic {
            out["respTopic"] = json!(reply);
        }
        let mut broker = json!({
            "id": "b1", "type": "mqtt-broker", "broker": "127.0.0.1", "port": 1883,
            "protocolVersion": protocol, "clientid": client_id, "cleansession": true,
            "autoConnect": true, "keepalive": 60
        });
        if let Ok(user) = std::env::var("EDGELINK_MQTT_USER")
            && !user.is_empty()
        {
            broker["username"] = json!(user);
            broker["password"] = json!(std::env::var("EDGELINK_MQTT_PASSWORD").unwrap_or_default());
        }
        let flows = json!([
            { "id": "100", "type": "tab" },
            broker,
            {
                "id": "2", "z": "100", "type": "mqtt in", "broker": "b1", "topic": topic,
                "qos": 0, "datatype": "utf8", "wires": [["4"]]
            },
            out,
            { "id": "4", "z": "100", "type": "test-once" }
        ]);
        let cfg = config::Config::builder()
            .add_source(config::File::from_str(
                r#"
                [runtime.context]
                default = "memory"

                [runtime.context.stores]
                memory = { provider = "memory" }

                [egress]
                mode = "enforce"

                [[egress.allow]]
                protocols = ["mqtt"]
                host = "127.0.0.1"
                ports = [1883]
                "#,
                config::FileFormat::Toml,
            ))
            .build()
            .unwrap();
        let registry = crate::runtime::registry::RegistryBuilder::default().build().unwrap();
        let engine = crate::runtime::engine::Engine::with_json(&registry, flows, Some(cfg)).unwrap();
        let inject = Vec::<(crate::runtime::model::ElementId, crate::runtime::model::Msg, f64)>::deserialize(json!([
            ["3", {"payload": payload, "topic": topic}, 1500.0]
        ]))
        .unwrap();
        let msgs = engine.run_window_with_schedule(Duration::from_secs(6), inject).await.unwrap();
        assert!(!msgs.is_empty(), "protocol {protocol} produced no message on {topic}");
        assert_eq!(msgs[0].0["payload"], crate::runtime::model::Variant::String(payload.to_owned()));
        assert_eq!(msgs[0].0["topic"], crate::runtime::model::Variant::String(topic.to_owned()));
        if let Some(reply) = response_topic {
            assert_eq!(msgs[0].0["responseTopic"], crate::runtime::model::Variant::String(reply.to_owned()));
        }
    }

    fn frame(n: u8) -> IncomingPublish {
        IncomingPublish {
            topic: "shared".to_string(),
            payload: bytes::Bytes::from(vec![n]),
            qos: QoS::AtMostOnce,
            retain: false,
            properties: None,
        }
    }

    #[tokio::test]
    async fn two_owners_share_a_topic_until_the_last_leaves() {
        let session = BrokerSession::from_options(options(json!({ "broker": "localhost", "autoConnect": false })));
        session
            .subscribe(
                "static",
                Subscription {
                    topic: "shared".into(),
                    qos: QoS::AtMostOnce,
                    nolocal: false,
                    retain_as_published: true,
                    retain_handling: 0,
                    subscription_identifier: None,
                },
            )
            .await
            .unwrap();
        session
            .subscribe(
                "dynamic",
                Subscription {
                    topic: "shared".into(),
                    qos: QoS::AtMostOnce,
                    nolocal: false,
                    retain_as_published: true,
                    retain_handling: 0,
                    subscription_identifier: None,
                },
            )
            .await
            .unwrap();
        session
            .subscribe(
                "static",
                Subscription {
                    topic: "shared".into(),
                    qos: QoS::AtMostOnce,
                    nolocal: false,
                    retain_as_published: true,
                    retain_handling: 0,
                    subscription_identifier: None,
                },
            )
            .await
            .unwrap();
        let mut owners = session.owners_of("shared").await;
        owners.sort();
        assert_eq!(owners, vec!["dynamic".to_string(), "static".to_string()]);
        session.unsubscribe("dynamic", "shared").await.unwrap();
        assert_eq!(session.owners_of("shared").await, vec!["static".to_string()]);
        session.unsubscribe_owner("static").await.unwrap();
        assert!(session.owners_of("shared").await.is_empty());
        assert_eq!(
            session.commands().await,
            vec![BrokerCommand::Subscribe(sub("shared", QoS::AtMostOnce)), BrokerCommand::Unsubscribe("shared".into())]
        );
    }

    fn sub(topic: &str, qos: QoS) -> Subscription {
        Subscription {
            topic: topic.into(),
            qos,
            nolocal: false,
            retain_as_published: true,
            retain_handling: 0,
            subscription_identifier: None,
        }
    }

    #[tokio::test]
    async fn a_second_owner_raises_qos_and_conflict_is_loud() {
        let session = BrokerSession::from_options(options(json!({
            "broker": "localhost",
            "protocolVersion": 5,
            "autoConnect": false
        })));
        session.subscribe("static", sub("shared", QoS::AtMostOnce)).await.unwrap();
        session.subscribe("dynamic", sub("shared", QoS::AtLeastOnce)).await.unwrap();
        assert_eq!(
            session.commands().await,
            vec![
                BrokerCommand::Subscribe(sub("shared", QoS::AtMostOnce)),
                BrokerCommand::Subscribe(sub("shared", QoS::AtLeastOnce)),
            ]
        );
        let planned = session.planned_resubscribes().await.unwrap();
        assert_eq!(planned, vec![sub("shared", QoS::AtLeastOnce)]);

        let mut conflict = sub("shared", QoS::AtMostOnce);
        conflict.nolocal = true;
        let err = session.subscribe("other", conflict).await.unwrap_err();
        assert!(err.to_string().contains("conflict"), "{err}");
        let mut owners = session.owners_of("shared").await;
        owners.sort();
        assert_eq!(owners, vec!["dynamic".to_string(), "static".to_string()]);
    }

    #[tokio::test]
    async fn a_rejected_identifier_does_not_leave_a_ghost_owner() {
        let session = BrokerSession::from_options(options(json!({
            "broker": "localhost",
            "protocolVersion": 5,
            "autoConnect": false
        })));
        session.disable_subscription_ids();
        let err = session
            .subscribe(
                "n1",
                Subscription {
                    topic: "ids".into(),
                    qos: QoS::AtMostOnce,
                    nolocal: false,
                    retain_as_published: true,
                    retain_handling: 0,
                    subscription_identifier: Some(7),
                },
            )
            .await
            .unwrap_err();
        assert!(err.to_string().contains("identifiers"), "{err}");
        assert!(session.owners_of("ids").await.is_empty());
        assert!(session.commands().await.is_empty());
    }

    #[tokio::test]
    async fn a_slow_subscriber_does_not_block_another() {
        let session = BrokerSession::from_options(options(json!({ "broker": "localhost", "autoConnect": false })));
        let mut slow = session.listen(1);
        let mut fast = session.listen(8);
        for n in 0..4 {
            dispatch(&session.inner, frame(n));
        }
        assert!(slow.overflow() >= 1);
        let mut got = Vec::new();
        while let Ok(frame) = fast.rx.try_recv() {
            got.push(frame.payload[0]);
        }
        assert_eq!(got, vec![0, 1, 2, 3]);
        let mut slow_got = Vec::new();
        while let Ok(frame) = slow.rx.try_recv() {
            slow_got.push(frame.payload[0]);
        }
        assert!(!slow_got.is_empty());
        assert!(slow_got.windows(2).all(|pair| pair[0] <= pair[1]));
    }
}
