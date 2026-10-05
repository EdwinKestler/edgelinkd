use std::fmt;
use std::sync::{Arc, Weak};

use async_trait::async_trait;
use runtime::engine::Engine;
use runtime::group::{Group, WeakGroup};
use serde::Serialize;
use serde::Serializer;
use serde::{self, Deserialize};
use smallvec::SmallVec;
use tokio::select;
use tokio_util::sync::CancellationToken;

use super::context::Context;
use crate::EdgelinkError;
use crate::runtime::flow::*;
use crate::runtime::model::json::{RedFlowNodeConfig, RedGlobalNodeConfig};
use crate::runtime::model::*;
use crate::runtime::red_env::*;
use crate::*;

pub(crate) mod common_nodes;
mod function_nodes;

mod parser_nodes;
mod sequence_nodes;

#[cfg(feature = "nodes_storage")]
mod storage_nodes;

#[cfg(feature = "nodes_network")]
mod network_nodes;

#[cfg(feature = "nodes_ai")]
pub(crate) mod ai_nodes;

#[cfg(any(feature = "nodes_postgres", feature = "nodes_redis"))]
mod db_nodes;

#[cfg(all(feature = "nodes_modbus", not(feature = "nodes_network")))]
#[path = "network_nodes/modbus.rs"]
mod modbus;

pub mod wellknown_names {
    pub const UNKNOWN_GLOBAL_NODE: &str = "unknown.global";
    pub const UNKNOWN_FLOW_NODE: &str = "unknown";
}

#[derive(Debug, Clone, Copy)]
pub enum NodeState {
    Starting = 0,
    Idle,
    Busy,
    Stopping,
    Stopped,
}

#[derive(Debug, Clone, Copy)]
pub enum NodeKind {
    Flow = 0,
    Global = 1,
}

impl fmt::Display for NodeKind {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match *self {
            NodeKind::Flow => write!(f, "FlowNode"),
            NodeKind::Global => write!(f, "GlobalNode"),
        }
    }
}

type GlobalNodeFactoryFn =
    fn(&Engine, &RedGlobalNodeConfig, Option<&config::Config>) -> crate::Result<Box<dyn GlobalNodeBehavior>>;

type FlowNodeFactoryFn = fn(
    &Flow,
    BaseFlowNodeState,
    &RedFlowNodeConfig,
    Option<&config::Config>,
) -> crate::Result<Box<dyn FlowNodeBehavior>>;

#[derive(Debug, Clone, Copy)]
pub enum NodeFactory {
    Global(GlobalNodeFactoryFn),
    Flow(FlowNodeFactoryFn),
}

/// Copilot/schema version for [`MetaNode`] ports and [`NodeHints`].
pub const NODE_METADATA_VERSION: u32 = 1;

/// Structural ports for Flow Copilot. `dynamic_outputs` allows 0..=15 like switch.
#[derive(Debug, Clone, Copy)]
pub struct NodePorts {
    pub inputs: u8,
    pub outputs: u8,
    pub dynamic_outputs: bool,
}

/// Optional hints submitted beside a node. Secret **names** only.
#[derive(Debug, Clone, Copy)]
pub struct NodeHints {
    pub type_: &'static str,
    pub config_refs: &'static [(&'static str, &'static str)],
    pub secret_fields: &'static [&'static str],
    pub capabilities: &'static [&'static str],
}

inventory::collect!(NodeHints);

/// Submit Copilot hints next to a node registration. Secret names only, never values.
#[macro_export]
macro_rules! node_hints {
    ($type:literal $(, refs = [$($prop:literal => $refty:literal),* $(,)?])? $(, secrets = [$($sec:literal),* $(,)?])? $(, caps = [$($cap:literal),* $(,)?])?) => {
        inventory::submit! {
            $crate::runtime::nodes::NodeHints {
                type_: $type,
                config_refs: &[$($(($prop, $refty)),*)?],
                secret_fields: &[$($($sec),*)?],
                capabilities: &[$($($cap),*)?],
            }
        }
    };
}

#[derive(Debug)]
pub struct MetaNode {
    /// The tag of the element
    pub(crate) kind: NodeKind,
    pub(crate) type_: &'static str,
    pub(crate) factory: NodeFactory,
    // Node-RED related metadata
    pub(crate) red_id: &'static str,   // Like "node-red/inject"
    pub(crate) red_name: &'static str, // Like "inject"
    pub(crate) module: &'static str,   // Like "node-red"
    pub(crate) version: &'static str,  // Like"4.0.9"
    pub(crate) local: bool,            // Default: false
    pub(crate) user: bool,             // Default: false
    pub(crate) ports: NodePorts,
}

impl MetaNode {
    #[allow(clippy::too_many_arguments)]
    pub const fn new(
        kind: NodeKind,
        type_: &'static str,
        factory: NodeFactory,
        red_id: &'static str,
        red_name: &'static str,
        module: &'static str,
        version: &'static str,
        local: bool,
        user: bool,
        inputs: u8,
        outputs: u8,
        dynamic_outputs: bool,
    ) -> Self {
        Self {
            kind,
            type_,
            factory,
            red_id,
            red_name,
            module,
            version,
            local,
            user,
            ports: NodePorts { inputs, outputs, dynamic_outputs },
        }
    }

    pub const fn kind(&self) -> NodeKind {
        self.kind
    }

    pub const fn type_(&self) -> &'static str {
        self.type_
    }

    pub const fn factory(&self) -> NodeFactory {
        self.factory
    }

    pub const fn red_id(&self) -> &'static str {
        self.red_id
    }

    pub const fn red_name(&self) -> &'static str {
        self.red_name
    }

    pub const fn module(&self) -> &'static str {
        self.module
    }

    pub const fn version(&self) -> &'static str {
        self.version
    }

    pub const fn local(&self) -> bool {
        self.local
    }

    pub const fn user(&self) -> bool {
        self.user
    }

    pub const fn ports(&self) -> NodePorts {
        self.ports
    }
}

#[derive(Debug)]
pub struct BaseFlowNodeState {
    pub(crate) id: ElementId,
    pub(crate) name: String,
    pub(crate) type_str: &'static str,
    pub(crate) ordering: usize,
    pub(crate) disabled: bool,
    pub(crate) flow: WeakFlow,
    pub(crate) msg_tx: MsgSender,
    pub(crate) msg_rx: MsgReceiverHolder,
    pub(crate) ports: Vec<Port>,
    pub(crate) group: Option<WeakGroup>,
    pub(crate) envs: RedEnvs,
    pub(crate) context: Context,

    pub(crate) on_received: MsgEventSender,
    // Constructed with every node. Nothing subscribes yet; `on_received` is the one the runtime reads.
    #[allow(dead_code)]
    pub(crate) on_completed: MsgEventSender,
    #[allow(dead_code)]
    pub(crate) on_error: MsgEventSender,
}

impl BaseFlowNodeState {
    pub const fn id(&self) -> ElementId {
        self.id
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub const fn type_str(&self) -> &'static str {
        self.type_str
    }

    pub const fn ordering(&self) -> usize {
        self.ordering
    }

    pub const fn disabled(&self) -> bool {
        self.disabled
    }

    pub const fn flow(&self) -> &WeakFlow {
        &self.flow
    }

    pub const fn context(&self) -> &Context {
        &self.context
    }
}

#[derive(Debug)]
pub struct BaseGlobalNodeState {
    pub(crate) id: ElementId,
    pub(crate) name: String,
    pub(crate) type_str: &'static str,
    pub(crate) ordering: usize,
    pub(crate) context: Context,
    pub(crate) disabled: bool,
}

impl BaseGlobalNodeState {
    pub const fn id(&self) -> ElementId {
        self.id
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub const fn type_str(&self) -> &'static str {
        self.type_str
    }

    pub const fn ordering(&self) -> usize {
        self.ordering
    }

    pub const fn disabled(&self) -> bool {
        self.disabled
    }

    pub const fn context(&self) -> &Context {
        &self.context
    }
}

pub trait ScopedNodeBehavior {
    fn get_scope(&self) -> &FlowNodeScope;
}

#[async_trait]
pub trait GlobalNodeBehavior: Send + Sync + FlowsElement {
    fn get_base(&self) -> &BaseGlobalNodeState;
}

#[async_trait]
pub trait FlowNodeBehavior: Send + Sync + FlowsElement {
    fn get_base(&self) -> &BaseFlowNodeState;

    async fn run(self: Arc<Self>, stop_token: CancellationToken);

    fn group(&self) -> Option<Group> {
        self.get_base().group.clone().and_then(|x| x.upgrade())
    }

    fn flow(&self) -> Option<Flow> {
        self.get_base().flow.upgrade()
    }

    fn envs(&self) -> &RedEnvs {
        &self.get_base().envs
    }

    fn get_env(&self, key: &str) -> Option<Variant> {
        self.get_base().envs.evalute_env(key)
    }

    fn engine(&self) -> Option<Engine> {
        self.get_base().flow.upgrade()?.engine()
    }

    async fn inject_msg(&self, msg: MsgHandle, cancel: CancellationToken) -> crate::Result<()> {
        select! {
            result = self.get_base().msg_tx.send(msg) => result.map_err(|e| e.into()),
            _ = cancel.cancelled() => Err(EdgelinkError::TaskCancelled),
        }
    }

    async fn recv_msg(&self, stop_token: CancellationToken) -> crate::Result<MsgHandle> {
        let msg = self.get_base().msg_rx.recv_msg(stop_token).await?;
        if self.get_base().on_received.receiver_count() > 0 {
            self.get_base().on_received.send(msg.clone())?;
        }
        Ok(msg)
    }

    async fn notify_uow_completed(&self, msg: MsgHandle, cancel: CancellationToken) {
        let (node_id, flow) = { (self.id(), self.get_base().flow.upgrade()) };
        if let Some(flow) = flow {
            flow.notify_node_uow_completed(&node_id, msg, cancel).await;
        } else {
            // The flow (and the engine owning it) is already released, e.g. the node was stopped by
            // a redeploy before this unit of work finished: there is nobody left to notify.
            log::debug!(
                "Cannot notify the completion of the UOW: the flow of Node(id='{}', type='{}') has been released",
                self.id(),
                self.type_str()
            );
        }
    }

    async fn fan_out_one(&self, envelope: Envelope, cancel: CancellationToken) -> crate::Result<()> {
        if self.get_base().ports.is_empty() {
            log::warn!("No output wires in this node: Node(id='{}', name='{}')", self.id(), self.name());
            return Ok(());
        }
        if envelope.port >= self.get_base().ports.len() {
            return Err(crate::EdgelinkError::BadArgument("envelope"))
                .with_context(|| format!("Invalid port index {}", envelope.port));
        }

        let port = &self.get_base().ports[envelope.port];

        let mut msg_sent = false;
        for wire in port.wires.iter() {
            let msg_to_send = if msg_sent { envelope.msg.deep_clone(true).await } else { envelope.msg.clone() };

            wire.tx(msg_to_send, cancel.clone()).await?;
            msg_sent = true;
        }
        Ok(())
    }

    /// Fan out one envelope per output port.
    ///
    /// Unlike a plain loop over the ports, every envelope is dispatched
    /// concurrently, so a saturated (or slow) wire on one output port no longer
    /// holds back the messages leaving through the other ports. The wire order
    /// *within* a single port is still preserved by [`Self::fan_out_one`], but
    /// the relative order of the messages sent to *different* ports is not
    /// deterministic anymore.
    async fn fan_out_many(&self, envelopes: SmallVec<[Envelope; 4]>, cancel: CancellationToken) -> crate::Result<()> {
        if self.get_base().ports.is_empty() {
            log::warn!("No output wires in this node: Node(id='{}')", self.id());
            return Ok(());
        }

        // The single-envelope case (by far the most common one) has nothing to parallelize.
        if envelopes.len() < 2 {
            for e in envelopes.into_iter() {
                self.fan_out_one(e, cancel.child_token()).await?;
            }
            return Ok(());
        }

        // Once the sends are in flight, dropping the remaining futures would silently
        // lose those messages, so we always drive every send to completion and report
        // the first error afterwards.
        let results = futures_util::future::join_all(
            envelopes.into_iter().map(|envelope| self.fan_out_one(envelope, cancel.child_token())),
        )
        .await;

        match results.into_iter().find_map(|result| result.err()) {
            Some(err) => Err(err),
            None => Ok(()),
        }
    }

    async fn report_status(&self, status: StatusObject, cancel: CancellationToken) {
        // Report to flow
        if let Some(flow) = self.flow() {
            if let Some(node) = flow.get_node_by_id(&self.id()) {
                if let Err(e) = flow.handle_status(node.as_ref(), &status, None, cancel.clone()).await {
                    log::warn!("Failed to handle status: {e}");
                }
            } else {
                log::debug!("Cannot report the status of Node(id='{}'): it is no longer in its flow", self.id());
            }
        }

        // Report to engine
        if let Some(engine) = self.engine() {
            engine.report_node_status(self.id(), status);
        } else {
            log::error!("Failed to get engine instance!");
        }
    }

    async fn report_error(&self, log_message: String, msg: MsgHandle, cancel: CancellationToken) {
        let node = self.flow().and_then(|flow| flow.get_node_by_id(&self.id()).map(|node| (flow, node)));
        let handled = if let Some((flow, node)) = node {
            flow.handle_error(node.as_ref(), &log_message, Some(msg), None, cancel).await.unwrap_or(false)
        } else {
            false
        };
        if !handled {
            log::error!("[{}:{}] {}", self.type_str(), self.name(), log_message);
        }
    }

    // events
    fn on_loaded(&self) {}

    async fn on_starting(&self) {}
}

impl dyn GlobalNodeBehavior {
    pub fn type_id(&self) -> ::std::any::TypeId {
        self.as_any().type_id()
    }
}

impl fmt::Debug for dyn GlobalNodeBehavior {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_fmt(format_args!(
            "GlobalNode(id='{}', type='{}', name='{}')",
            self.id(),
            self.get_base().type_str,
            self.name(),
        ))
    }
}

impl fmt::Display for dyn GlobalNodeBehavior {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_fmt(format_args!(
            "GlobalNode(id='{}', type='{}', name='{}')",
            self.id(),
            self.get_base().type_str,
            self.name(),
        ))
    }
}

impl dyn FlowNodeBehavior {
    pub fn type_id(&self) -> ::std::any::TypeId {
        self.as_any().type_id()
    }
}

impl fmt::Debug for dyn FlowNodeBehavior {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_fmt(format_args!("FlowNode(id='{}', type='{}', name='{}')", self.id(), self.type_str(), self.name(),))
    }
}

impl fmt::Display for dyn FlowNodeBehavior {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_fmt(format_args!("FlowNode(id='{}', type='{}', name='{}')", self.id(), self.type_str(), self.name(),))
    }
}

pub async fn with_uow<'a, B, F, T>(node: &'a B, cancel: CancellationToken, proc: F)
where
    B: FlowNodeBehavior,
    F: FnOnce(&'a B, MsgHandle) -> T,
    T: std::future::Future<Output = crate::Result<()>>,
{
    match node.recv_msg(cancel.clone()).await {
        Ok(msg) => {
            if let Err(ref err) = proc(node, msg.clone()).await {
                let flow = node.flow().expect("flow");
                let error_message = err.to_string();

                match flow.handle_error(node, &error_message, Some(msg.clone()), None, cancel.clone()).await {
                    Ok(_) => (),
                    Err(e) => {
                        log::error!("Failed to handle error: {e:?}");
                    }
                }
            }

            // Report the completion
            node.notify_uow_completed(msg, cancel.clone()).await;
        }
        Err(ref err) => {
            if err.is_cancelled() {
                return;
            }

            log::warn!("[{}:{}] {}", node.type_str(), node.name(), err);
        }
    }
}

#[async_trait]
pub trait LinkCallNodeBehavior: Send + Sync + FlowNodeBehavior {
    /// Receive the returning message
    async fn return_msg(
        &self,
        msg: MsgHandle,
        stack_id: ElementId,
        return_from_node_id: ElementId,
        return_from_flow_id: ElementId,
        cancel: CancellationToken,
    ) -> crate::Result<()>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
#[derive(Default)]
pub enum StatusFill {
    #[default]
    Red,

    Green,

    Yellow,

    Grey,

    Blue,
}

impl StatusFill {
    pub fn as_str(&self) -> &'static str {
        match self {
            StatusFill::Red => "red",
            StatusFill::Green => "green",
            StatusFill::Yellow => "yellow",
            StatusFill::Grey => "grey",
            StatusFill::Blue => "blue",
        }
    }
}

impl fmt::Display for StatusFill {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
#[derive(Default)]
pub enum StatusShape {
    Ring,

    #[default]
    Dot,
}

impl StatusShape {
    pub fn as_str(&self) -> &'static str {
        match self {
            StatusShape::Ring => "ring",
            StatusShape::Dot => "dot",
        }
    }
}

impl fmt::Display for StatusShape {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

#[derive(Deserialize, Debug, Clone, Serialize, PartialEq)]
pub struct StatusObject {
    #[serde(skip_serializing_if = "Option::is_none", serialize_with = "to_string_opt")]
    pub fill: Option<StatusFill>,

    #[serde(skip_serializing_if = "Option::is_none", serialize_with = "to_string_opt")]
    pub shape: Option<StatusShape>,

    #[serde(skip_serializing_if = "Option::is_none", serialize_with = "to_string_opt")]
    pub text: Option<String>,
}

fn to_string_opt<S, T>(x: &Option<T>, s: S) -> Result<S::Ok, S::Error>
where
    S: Serializer,
    T: std::fmt::Display,
{
    match x {
        Some(v) => s.serialize_some(&v.to_string()),
        None => s.serialize_none(),
    }
}

#[allow(dead_code)]
impl StatusObject {
    fn empty() -> Self {
        Self { fill: None, shape: None, text: None }
    }

    fn is_empty(&self) -> bool {
        matches!(self, &StatusObject { fill: None, shape: None, text: None })
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum FlowNodeScope {
    #[default]
    All,
    SameGroup,
    Nodes(Vec<ElementId>),
}

impl FlowNodeScope {
    pub fn as_bool(&self) -> bool {
        !matches!(self, FlowNodeScope::All)
    }
}

impl<'de> Deserialize<'de> for FlowNodeScope {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        struct NodeScopeVisitor;

        impl<'de> serde::de::Visitor<'de> for NodeScopeVisitor {
            type Value = FlowNodeScope;

            fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
                formatter.write_str("a string, null, or an array of strings")
            }

            fn visit_unit<E>(self) -> Result<FlowNodeScope, E>
            where
                E: serde::de::Error,
            {
                Ok(FlowNodeScope::All)
            }

            fn visit_str<E>(self, value: &str) -> Result<FlowNodeScope, E>
            where
                E: serde::de::Error,
            {
                match value {
                    "group" => Ok(FlowNodeScope::SameGroup),
                    _ => Err(serde::de::Error::invalid_value(serde::de::Unexpected::Str(value), &self)),
                }
            }

            fn visit_seq<A>(self, seq: A) -> Result<FlowNodeScope, A::Error>
            where
                A: serde::de::SeqAccess<'de>,
            {
                let vec: Vec<ElementId> = Deserialize::deserialize(serde::de::value::SeqAccessDeserializer::new(seq))?;
                Ok(FlowNodeScope::Nodes(vec))
            }
        }

        deserializer.deserialize_any(NodeScopeVisitor)
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use async_trait::async_trait;
    use serde_json::json;

    use super::*;
    use crate::runtime::engine::build_test_engine;

    /// Capacity of the input channels built by these tests. The runtime's node channels take their
    /// capacity from `runtime.flow.node_msg_queue_capacity`.
    const NODE_MSG_CHANNEL_CAPACITY: usize = 16;

    /// A do-nothing node; these tests only exercise the default `fan_out_*` implementations.
    struct FanOutTestNode {
        base: BaseFlowNodeState,
    }

    impl FlowsElement for FanOutTestNode {
        fn id(&self) -> ElementId {
            self.base.id
        }

        fn name(&self) -> &str {
            &self.base.name
        }

        fn type_str(&self) -> &'static str {
            self.base.type_str
        }

        fn ordering(&self) -> usize {
            self.base.ordering
        }

        fn is_disabled(&self) -> bool {
            self.base.disabled
        }

        fn as_any(&self) -> &dyn ::std::any::Any {
            self
        }

        fn parent_element(&self) -> Option<ElementId> {
            self.base.flow.upgrade().map(|flow| flow.id())
        }

        fn get_path(&self) -> String {
            format!("{}/{}", self.base.flow.upgrade().expect("flow").get_path(), self.id())
        }
    }

    #[async_trait]
    impl FlowNodeBehavior for FanOutTestNode {
        fn get_base(&self) -> &BaseFlowNodeState {
            &self.base
        }

        async fn run(self: Arc<Self>, _stop_token: CancellationToken) {
            unreachable!("The fan-out test node is never started")
        }
    }

    fn make_test_node(ports: Vec<Port>) -> Arc<FanOutTestNode> {
        let engine = build_test_engine(json!([{ "id": "100", "type": "tab", "label": "Fan-out test flow" }])).unwrap();
        let flow = engine.get_flow(&"100".parse().expect("valid flow id")).expect("The flow must be loaded");
        let context = engine.get_context_manager().new_context(engine.context(), "fan-out-test".to_owned());
        let (msg_tx, msg_rx) = tokio::sync::mpsc::channel(NODE_MSG_CHANNEL_CAPACITY);

        let base = BaseFlowNodeState {
            id: ElementId::new(),
            name: "fan-out-test".to_owned(),
            type_str: "fan-out-test",
            ordering: 0,
            disabled: false,
            flow: flow.downgrade(),
            msg_tx,
            msg_rx: MsgReceiverHolder::new(msg_rx),
            ports,
            group: None,
            envs: crate::runtime::red_env::RedEnvStoreBuilder::default().build(),
            context,
            on_received: MsgEventSender::new(1),
            on_completed: MsgEventSender::new(1),
            on_error: MsgEventSender::new(1),
        };

        Arc::new(FanOutTestNode { base })
    }

    fn make_port(msg_sender: MsgSender) -> Port {
        Port { wires: vec![PortWire { msg_sender }] }
    }

    fn make_envelope(port: usize, payload: &str) -> Envelope {
        Envelope { port, msg: MsgHandle::with_payload(Variant::from(payload)) }
    }

    async fn payload_of(msg: MsgHandle) -> Variant {
        let guard = msg.read().await;
        guard["payload"].clone()
    }

    /// Regression test: an output port whose queue is already saturated must not delay
    /// the fan-out of the messages leaving through the other output ports.
    #[tokio::test]
    async fn test_fan_out_many_should_not_be_blocked_by_a_saturated_port() {
        let (tx0, mut rx0) = tokio::sync::mpsc::channel::<MsgHandle>(1);
        let (tx1, mut rx1) = tokio::sync::mpsc::channel::<MsgHandle>(1);

        // Saturate port 0, so that any further send to it can only ever block.
        tx0.send(MsgHandle::default()).await.unwrap();

        // Port 0 is released only *after* the message addressed to port 1 got through.
        // A sequential fan-out never reaches port 1, so this would deadlock.
        let release_port0 = tokio::spawn(async move {
            let msg = rx1.recv().await.expect("The message addressed to port 1 must not be delayed");
            assert_eq!(payload_of(msg).await, Variant::from("port-1"));
            let _ = rx0.recv().await;
            // Hand the receivers back so that they outlive `fan_out_many()`; dropping them
            // here would close the channels and make the still pending send to port 0 fail.
            (rx0, rx1)
        });

        let node = make_test_node(vec![make_port(tx0), make_port(tx1)]);
        let envelopes: SmallVec<[Envelope; 4]> =
            SmallVec::from_vec(vec![make_envelope(0, "port-0"), make_envelope(1, "port-1")]);

        tokio::time::timeout(Duration::from_secs(5), node.fan_out_many(envelopes, CancellationToken::new()))
            .await
            .expect("fan_out_many() was blocked by the saturated output port")
            .expect("fan_out_many() must succeed");

        let _receivers = release_port0.await.expect("The message addressed to port 1 must have been delivered");
    }

    /// Without back-pressure every envelope must still reach its own port.
    #[tokio::test]
    async fn test_fan_out_many_should_deliver_every_port() {
        let (tx0, mut rx0) = tokio::sync::mpsc::channel::<MsgHandle>(NODE_MSG_CHANNEL_CAPACITY);
        let (tx1, mut rx1) = tokio::sync::mpsc::channel::<MsgHandle>(NODE_MSG_CHANNEL_CAPACITY);

        let node = make_test_node(vec![make_port(tx0), make_port(tx1)]);
        let envelopes: SmallVec<[Envelope; 4]> =
            SmallVec::from_vec(vec![make_envelope(0, "port-0"), make_envelope(1, "port-1")]);

        node.fan_out_many(envelopes, CancellationToken::new()).await.unwrap();

        assert_eq!(payload_of(rx0.recv().await.unwrap()).await, Variant::from("port-0"));
        assert_eq!(payload_of(rx1.recv().await.unwrap()).await, Variant::from("port-1"));
    }

    /// An out-of-range port index must still be reported as an error.
    #[tokio::test]
    async fn test_fan_out_many_should_report_an_invalid_port() {
        let (tx0, _rx0) = tokio::sync::mpsc::channel::<MsgHandle>(NODE_MSG_CHANNEL_CAPACITY);
        let (tx1, _rx1) = tokio::sync::mpsc::channel::<MsgHandle>(NODE_MSG_CHANNEL_CAPACITY);

        let node = make_test_node(vec![make_port(tx0), make_port(tx1)]);
        let envelopes: SmallVec<[Envelope; 4]> =
            SmallVec::from_vec(vec![make_envelope(0, "port-0"), make_envelope(7, "port-7")]);

        assert!(node.fan_out_many(envelopes, CancellationToken::new()).await.is_err());
    }
}
