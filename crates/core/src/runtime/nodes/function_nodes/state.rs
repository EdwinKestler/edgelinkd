//! A state machine held as a JSON table.
//!
//! `initial` names the starting state. `period` is a tick in milliseconds; `0` disables it.
//! Each state has `on` (edges, first match wins) and `entry` (context writes on entry).
//! An edge `when` is a compare against a message property (`"type": "msg"`) or a context key
//! (`"flow"`, `"global"`, `"node"`). `op` is `eq` or `neq`. A message, including one with
//! `msg.tick`, and each period tick, re-check the current state's edges. A match sets
//! `msg.state` to the state that was entered and sends that one message.
//!
//! Parallel branches, history states, JSONata actions, and any other condition type or
//! operator are rejected when the flow is deployed.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::time::Duration;

use serde::Deserialize;
use tokio::sync::Mutex;

use crate::runtime::flow::Flow;
use crate::runtime::model::json::RedFlowNodeConfig;
use crate::runtime::nodes::*;
use edgelink_macro::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CondSource {
    Msg,
    Flow,
    Global,
    Node,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Cmp {
    Eq,
    Neq,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ActionScope {
    Flow,
    Global,
    Node,
}

struct Condition {
    source: CondSource,
    property: String,
    op: Cmp,
    value: Variant,
}

struct Transition {
    when: Condition,
    to: String,
}

struct Action {
    scope: ActionScope,
    key: String,
    value: Variant,
}

struct StateDef {
    on: Vec<Transition>,
    entry: Vec<Action>,
}

struct StateNodeConfig {
    initial: String,
    /// Milliseconds between ticks. Zero means the node waits for a message.
    period: u64,
    states: Vec<StateDef>,
    index: HashMap<String, usize>,
}

#[derive(Debug, Deserialize)]
struct RawConfig {
    initial: String,
    #[serde(default, deserialize_with = "deserialize_period")]
    period: u64,
    states: Vec<RawState>,
}

#[derive(Debug, Deserialize)]
struct RawState {
    name: String,
    #[serde(default)]
    on: Vec<RawEdge>,
    #[serde(default)]
    entry: Vec<RawAction>,
}

#[derive(Debug, Deserialize)]
struct RawEdge {
    when: RawCondition,
    to: String,
}

#[derive(Debug, Deserialize)]
struct RawCondition {
    #[serde(rename = "type")]
    kind: String,
    property: String,
    #[serde(default = "eq_op")]
    op: String,
    value: Variant,
}

#[derive(Debug, Deserialize)]
struct RawAction {
    scope: String,
    key: String,
    value: Variant,
}

fn eq_op() -> String {
    "eq".to_owned()
}

fn deserialize_period<'de, D>(deserializer: D) -> Result<u64, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = serde_json::Value::deserialize(deserializer)?;
    match value {
        serde_json::Value::Null => Ok(0),
        serde_json::Value::Number(number) => number
            .as_u64()
            .ok_or_else(|| serde::de::Error::custom("period must be a non-negative whole number of milliseconds")),
        serde_json::Value::String(text) if text.is_empty() => Ok(0),
        serde_json::Value::String(text) => text.parse::<u64>().map_err(serde::de::Error::custom),
        _ => Err(serde::de::Error::custom("period must be a number of milliseconds")),
    }
}

fn compile(raw: RawConfig) -> crate::Result<StateNodeConfig> {
    if raw.states.is_empty() {
        return Err(EdgelinkError::InvalidOperation("state node has no states".to_owned()));
    }
    let mut index = HashMap::with_capacity(raw.states.len());
    for (slot, state) in raw.states.iter().enumerate() {
        if state.name.is_empty() {
            return Err(EdgelinkError::InvalidOperation("a state has no name".to_owned()));
        }
        if index.insert(state.name.clone(), slot).is_some() {
            return Err(EdgelinkError::InvalidOperation(format!("state '{}' is listed more than once", state.name)));
        }
    }
    if !index.contains_key(&raw.initial) {
        return Err(EdgelinkError::InvalidOperation(format!("initial state '{}' is not in the table", raw.initial)));
    }

    let mut states = Vec::with_capacity(raw.states.len());
    for state in raw.states {
        let mut on = Vec::with_capacity(state.on.len());
        for edge in state.on {
            if !index.contains_key(&edge.to) {
                return Err(EdgelinkError::InvalidOperation(format!(
                    "state '{}' steps to '{}', which is not in the table",
                    state.name, edge.to
                )));
            }
            on.push(Transition { when: compile_condition(&edge.when)?, to: edge.to });
        }
        let mut entry = Vec::with_capacity(state.entry.len());
        for action in state.entry {
            entry.push(compile_action(&action)?);
        }
        states.push(StateDef { on, entry });
    }

    Ok(StateNodeConfig { initial: raw.initial, period: raw.period, states, index })
}

fn compile_condition(raw: &RawCondition) -> crate::Result<Condition> {
    if raw.property.is_empty() {
        return Err(EdgelinkError::InvalidOperation("a condition has no property".to_owned()));
    }
    let source = match raw.kind.as_str() {
        "msg" => CondSource::Msg,
        "flow" => CondSource::Flow,
        "global" => CondSource::Global,
        "node" => CondSource::Node,
        other => {
            return Err(EdgelinkError::NotSupported(format!("condition type '{other}' is not supported")));
        }
    };
    let op = match raw.op.as_str() {
        "eq" => Cmp::Eq,
        "neq" => Cmp::Neq,
        other => {
            return Err(EdgelinkError::NotSupported(format!("condition operator '{other}' is not supported")));
        }
    };
    Ok(Condition { source, property: raw.property.clone(), op, value: raw.value.clone() })
}

fn compile_action(raw: &RawAction) -> crate::Result<Action> {
    if raw.key.is_empty() {
        return Err(EdgelinkError::InvalidOperation("an entry action has no key".to_owned()));
    }
    let scope = match raw.scope.as_str() {
        "flow" => ActionScope::Flow,
        "global" => ActionScope::Global,
        "node" => ActionScope::Node,
        other => {
            return Err(EdgelinkError::NotSupported(format!("action scope '{other}' is not supported")));
        }
    };
    Ok(Action { scope, key: raw.key.clone(), value: raw.value.clone() })
}

fn values_match(op: Cmp, left: Option<&Variant>, right: &Variant) -> bool {
    match op {
        Cmp::Eq => left.is_some_and(|value| value == right),
        // A missing property is not the compared value.
        Cmp::Neq => left.is_none_or(|value| value != right),
    }
}

fn tick_message() -> MsgHandle {
    MsgHandle::with_properties(BTreeMap::from([
        (wellknown::MSG_ID_PROPERTY.to_owned(), Msg::generate_id_variant()),
        ("tick".to_owned(), Variant::from(true)),
    ]))
}

#[flow_node("state", red_name = "state")]
struct StateNode {
    base: BaseFlowNodeState,
    config: StateNodeConfig,
}

impl StateNode {
    fn build(
        _flow: &Flow,
        base_node: BaseFlowNodeState,
        config: &RedFlowNodeConfig,
        _options: Option<&config::Config>,
    ) -> crate::Result<Box<dyn FlowNodeBehavior>> {
        let raw = RawConfig::deserialize(&config.rest)?;
        let config = compile(raw)?;
        Ok(Box::new(StateNode { base: base_node, config }))
    }

    fn state_def(&self, name: &str) -> Option<&StateDef> {
        self.config.index.get(name).map(|slot| &self.config.states[*slot])
    }

    fn context_for(&self, scope: ActionScope) -> crate::Result<crate::runtime::context::Context> {
        match scope {
            ActionScope::Node => Ok(self.get_base().context().clone()),
            ActionScope::Flow => self
                .flow()
                .map(|flow| flow.context().clone())
                .ok_or_else(|| EdgelinkError::InvalidOperation("state node has no flow context".to_owned())),
            ActionScope::Global => self
                .engine()
                .map(|engine| engine.context().clone())
                .ok_or_else(|| EdgelinkError::InvalidOperation("state node has no global context".to_owned())),
        }
    }

    async fn read_context(&self, source: CondSource, key: &str) -> Option<Variant> {
        let scope = match source {
            CondSource::Msg => return None,
            CondSource::Flow => ActionScope::Flow,
            CondSource::Global => ActionScope::Global,
            CondSource::Node => ActionScope::Node,
        };
        let ctx = self.context_for(scope).ok()?;
        ctx.get_one(None, key, &[]).await
    }

    async fn enter(&self, state: &str, cancel: CancellationToken) -> crate::Result<()> {
        let def = self
            .state_def(state)
            .ok_or_else(|| EdgelinkError::InvalidOperation(format!("state '{state}' is not in the table")))?;
        for action in &def.entry {
            let ctx = self.context_for(action.scope)?;
            ctx.set_one(None, &action.key, Some(action.value.clone()), &[]).await?;
        }
        self.report_status(
            StatusObject { fill: Some(StatusFill::Green), shape: Some(StatusShape::Dot), text: Some(state.to_owned()) },
            cancel,
        )
        .await;
        Ok(())
    }

    /// `Ok(None)` leaves the machine where it is and sends nothing.
    async fn first_match(&self, state: &str, msg: &MsgHandle) -> Option<String> {
        let def = self.state_def(state)?;
        for edge in &def.on {
            let matched = match edge.when.source {
                CondSource::Msg => {
                    let guard = msg.read().await;
                    values_match(edge.when.op, guard.get_nav_stripped(&edge.when.property), &edge.when.value)
                }
                source => {
                    let current = self.read_context(source, &edge.when.property).await;
                    values_match(edge.when.op, current.as_ref(), &edge.when.value)
                }
            };
            if matched {
                return Some(edge.to.clone());
            }
        }
        None
    }

    async fn period_loop(self: Arc<Self>, cancel: CancellationToken) {
        let period = Duration::from_millis(self.config.period);
        loop {
            tokio::select! {
                _ = cancel.cancelled() => break,
                _ = tokio::time::sleep(period) => {
                    if self.inject_msg(tick_message(), cancel.child_token()).await.is_err() {
                        break;
                    }
                }
            }
        }
    }
}

#[async_trait]
impl FlowNodeBehavior for StateNode {
    fn get_base(&self) -> &BaseFlowNodeState {
        &self.base
    }

    async fn run(self: Arc<Self>, stop_token: CancellationToken) {
        let current = Arc::new(Mutex::new(self.config.initial.clone()));
        {
            let name = current.lock().await.clone();
            if let Err(err) = self.enter(&name, stop_token.child_token()).await {
                log::error!("State node '{}': {err}", self.name());
                self.report_status(
                    StatusObject {
                        fill: Some(StatusFill::Red),
                        shape: Some(StatusShape::Dot),
                        text: Some(err.to_string()),
                    },
                    stop_token.child_token(),
                )
                .await;
            }
        }
        if self.config.period > 0 {
            let node = Arc::clone(&self);
            let cancel = stop_token.child_token();
            tokio::spawn(async move { node.period_loop(cancel).await });
        }

        while !stop_token.is_cancelled() {
            let current = Arc::clone(&current);
            let cancel = stop_token.child_token();
            let step_cancel = cancel.clone();
            with_uow(self.as_ref(), cancel, move |node, msg| {
                let current = Arc::clone(&current);
                async move {
                    let next = {
                        let state = current.lock().await;
                        node.first_match(&state, &msg).await
                    };
                    let Some(next) = next else {
                        return Ok(());
                    };
                    {
                        let mut guard = msg.write().await;
                        guard.set("state".into(), Variant::String(next.clone()));
                    }
                    node.enter(&next, step_cancel.child_token()).await?;
                    *current.lock().await = next;
                    node.fan_out_one(Envelope { port: 0, msg }, step_cancel).await
                }
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

    fn machine(states: serde_json::Value, initial: &str, period: u64) -> serde_json::Value {
        json!([
            { "id": "100", "type": "tab", "label": "Flow 1" },
            {
                "id": "1", "z": "100", "type": "state", "name": "machine",
                "initial": initial, "period": period, "states": states,
                "wires": [["2"]]
            },
            { "id": "2", "z": "100", "type": "test-once" }
        ])
    }

    #[tokio::test]
    async fn period_tick_enters_when_the_context_edge_matches() {
        let flows = machine(
            json!([
                {
                    "name": "idle",
                    "entry": [{ "scope": "flow", "key": "go", "value": true }],
                    "on": [{ "when": { "type": "flow", "property": "go", "op": "eq", "value": true }, "to": "run" }]
                },
                { "name": "run", "on": [] }
            ]),
            "idle",
            30,
        );
        let engine = crate::runtime::engine::build_test_engine(flows).unwrap();
        let msgs = engine.run_once(1, Duration::from_secs(2)).await.unwrap();
        assert_eq!(msgs[0]["state"], "run".into());
        assert_eq!(msgs[0]["tick"], true.into());
    }

    #[tokio::test]
    async fn neq_and_node_context_take_the_first_match() {
        let flows = machine(
            json!([
                {
                    "name": "idle",
                    "entry": [{ "scope": "node", "key": "count", "value": 1 }],
                    "on": [
                        { "when": { "type": "msg", "property": "payload", "op": "neq", "value": "stay" }, "to": "run" },
                        { "when": { "type": "node", "property": "count", "op": "eq", "value": 1 }, "to": "fault" }
                    ]
                },
                { "name": "run", "on": [] },
                { "name": "fault", "on": [] }
            ]),
            "idle",
            0,
        );
        let engine = crate::runtime::engine::build_test_engine(flows).unwrap();
        let injected: Vec<(ElementId, Msg)> = Vec::deserialize(json!([["1", { "payload": "start" }]])).unwrap();
        let msgs = engine.run_once_with_inject(1, Duration::from_millis(500), injected).await.unwrap();
        assert_eq!(msgs[0]["state"], "run".into());
        assert_eq!(msgs[0]["payload"], "start".into());
    }

    #[test]
    fn unknown_operator_is_rejected_at_deploy() {
        let flows = machine(
            json!([
                {
                    "name": "idle",
                    "on": [{ "when": { "type": "msg", "property": "payload", "op": "gt", "value": 1 }, "to": "run" }]
                },
                { "name": "run", "on": [] }
            ]),
            "idle",
            0,
        );
        let err = crate::runtime::engine::build_test_engine(flows).unwrap_err();
        let text = err.to_string();
        assert!(text.contains("not supported"), "{text}");
        assert!(text.contains("gt"), "{text}");
    }
}
