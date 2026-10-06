//! Flow node that runs an active WASM plugin.
//!
//! One instance per node, created on the first message and reused, so guest state survives
//! between messages. Calls run on the blocking pool under a global permit; every call is bounded
//! by fuel, a wall-clock deadline and cancellation (see `exec.rs`). A fault (fuel, deadline,
//! trap, host bound) discards the instance; `failure_threshold` faults within `failure_window_s`
//! put the node in a failed state until the flow is redeployed.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio_util::sync::CancellationToken;

use crate::EdgelinkError;
use crate::runtime::flow::Flow;
use crate::runtime::model::json::RedFlowNodeConfig;
use crate::runtime::model::{Envelope, FlowsElement, MsgHandle, Variant};
use crate::runtime::nodes::*;

use super::convert::{decode_variant, encode_variant};
use super::exec::{CallError, CallOutput, EngineCell, GuestLogLevel, GuestStatus, Instance};
use super::host::{EffectiveLimits, WasmRuntime};
use super::plugin_set::PluginSpec;

/// Node properties Node-RED's editor writes for any node. Anything else is plugin
/// configuration, which needs the manifest schema this prototype does not implement yet.
const EDITOR_PROPERTIES: &[&str] = &["x", "y", "info", "l", "wasmPlugin"];
const MSG_ID: &str = "_msgid";
const GUEST_LOGS_PER_SECOND: u32 = 50;

#[derive(Default)]
struct NodeRuntimeState {
    cell: Option<Arc<EngineCell>>,
    instance: Option<Instance>,
    faults: VecDeque<Instant>,
    failed: Option<String>,
    log_window: Option<(Instant, u32)>,
    logs_dropped: u64,
}

pub(crate) struct WasmPluginNode {
    base: BaseFlowNodeState,
    spec: PluginSpec,
    limits: EffectiveLimits,
    runtime: Arc<WasmRuntime>,
    state: Mutex<NodeRuntimeState>,
}

fn not_supported(text: String) -> EdgelinkError {
    EdgelinkError::NotSupported(text)
}

impl WasmPluginNode {
    pub(crate) fn build(
        flow: &Flow,
        base_node: BaseFlowNodeState,
        config: &RedFlowNodeConfig,
        _options: Option<&config::Config>,
    ) -> crate::Result<Box<dyn FlowNodeBehavior>> {
        let type_name = base_node.type_str;
        let engine = flow.engine().ok_or_else(|| EdgelinkError::invalid_operation("WASM plugin has no engine"))?;
        let runtime = engine.wasm_runtime();
        if !runtime.settings().enabled {
            return Err(super::settings::disabled_plugin_error(type_name));
        }
        let spec = engine
            .wasm_plugins()
            .and_then(|set| set.get(type_name).cloned())
            .ok_or_else(|| super::not_active_error(type_name))?;
        check_node_properties(&spec, config)?;
        let limits = runtime.effective_limits(&spec)?;
        runtime.admit(&spec, &limits)?;
        Ok(Box::new(Self { base: base_node, spec, limits, runtime, state: Mutex::new(NodeRuntimeState::default()) }))
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, NodeRuntimeState> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn error(&self, text: impl std::fmt::Display) -> EdgelinkError {
        EdgelinkError::invalid_operation(&format!("wasm {}: {text}", self.spec.id))
    }

    async fn handle(&self, msg: MsgHandle, cancel: CancellationToken) -> crate::Result<()> {
        if let Some(reason) = self.lock().failed.clone() {
            return Err(self.error(format!("plugin failed ({reason}); redeploy to reset")));
        }
        let (body, input_id) = {
            let guard = msg.read().await;
            (guard.as_variant().clone(), guard.get(MSG_ID).cloned())
        };
        let bytes = encode_variant(&body).map_err(|err| self.error(format!("input: {err}")))?;
        let max_input = self.runtime.max_input_bytes();
        if bytes.len() > max_input {
            return Err(
                self.error(format!("input {} bytes exceeds {max_input} ([runtime.wasm] max_input_kib)", bytes.len()))
            );
        }

        let permit = tokio::time::timeout(self.limits.deadline, self.runtime.permits().acquire_owned())
            .await
            .map_err(|_| self.error("concurrency limit ([runtime.wasm] max_concurrent)"))?
            .map_err(|_| self.error("concurrency limit closed"))?;

        let (cell, instance) = {
            let mut state = self.lock();
            if state.cell.is_none() {
                state.cell = Some(self.runtime.engine_cell()?);
            }
            (state.cell.clone().expect("engine cell"), state.instance.take())
        };
        let spec = self.spec.clone();
        let pages = self.limits.memory_pages;
        let budget = self.runtime.budget(&self.limits);
        let cancelled = Arc::new(AtomicBool::new(false));
        let flag = cancelled.clone();
        let mut join = tokio::task::spawn_blocking(move || {
            let _permit = permit;
            let mut instance = match instance {
                Some(instance) => instance,
                None => {
                    let created = cell
                        .module(spec.sha256, &spec.wasm)
                        .and_then(|module| cell.instantiate(&module, pages, spec.outputs, &budget));
                    match created {
                        Ok(instance) => instance,
                        Err(err) => return (None, Err(CallError::Fault(err.to_string()))),
                    }
                }
            };
            let result = instance.call(&bytes, &budget, &flag);
            (Some(instance), result)
        });
        let joined = tokio::select! {
            joined = &mut join => joined,
            _ = cancel.cancelled() => {
                cancelled.store(true, Ordering::Release);
                join.await
            }
        };
        if cancel.is_cancelled() {
            // Stop/redeploy: the instance goes with the flow; shutdown is not a node error.
            return Ok(());
        }
        let (instance, result) = joined.unwrap_or_else(|_| (None, Err(CallError::Fault("host panic".to_owned()))));

        match result {
            Ok(output) => {
                self.lock().instance = instance;
                self.deliver(&msg, input_id.as_ref(), output, cancel).await
            }
            Err(CallError::Guest { text, logs }) => {
                self.lock().instance = instance;
                self.write_logs(logs);
                Err(self.error(text))
            }
            Err(CallError::Cancelled) => Ok(()),
            Err(CallError::Fault(text)) => Err(self.fault(text, cancel).await),
        }
    }

    /// Record a fault: drop the instance, count it, and enter the failed state at the threshold.
    async fn fault(&self, text: String, cancel: CancellationToken) -> EdgelinkError {
        let failed = {
            let mut state = self.lock();
            state.instance = None;
            let now = Instant::now();
            let window = Duration::from_secs(self.runtime.settings().failure_window_s);
            state.faults.push_back(now);
            while state.faults.front().is_some_and(|at| now.duration_since(*at) > window) {
                state.faults.pop_front();
            }
            let threshold = self.runtime.settings().failure_threshold as usize;
            if state.faults.len() >= threshold && state.failed.is_none() {
                let reason = format!("{} faults within {}s", state.faults.len(), window.as_secs());
                state.failed = Some(reason.clone());
                Some(reason)
            } else {
                None
            }
        };
        if let Some(reason) = failed {
            let status = StatusObject {
                fill: Some(StatusFill::Red),
                shape: Some(StatusShape::Ring),
                text: Some(format!("plugin failed ({reason})")),
            };
            self.report_status(status, cancel).await;
        }
        self.error(text)
    }

    async fn deliver(
        &self,
        msg: &MsgHandle,
        input_id: Option<&Variant>,
        output: CallOutput,
        cancel: CancellationToken,
    ) -> crate::Result<()> {
        self.write_logs(output.logs);
        let mut envelopes = Vec::with_capacity(output.outputs.len());
        for (port, payload) in output.outputs {
            let body = match decode_variant(&payload, self.runtime.max_input_bytes() as u32) {
                Ok(Variant::Object(map)) => map,
                Ok(_) => return Err(self.fault(format!("output on port {port} is not an object"), cancel).await),
                Err(err) => return Err(self.fault(format!("output on port {port}: {err}"), cancel).await),
            };
            match (body.get(MSG_ID), input_id) {
                (Some(out), Some(input)) if out != input => {
                    return Err(self.fault(format!("output on port {port} changes {MSG_ID}"), cancel).await);
                }
                _ => {}
            }
            let handle = msg.deep_clone(false).await;
            {
                let mut guard = handle.write().await;
                *guard.as_variant_mut() = Variant::Object(body);
                if let Some(id) = input_id
                    && guard.get(MSG_ID).is_none()
                {
                    guard.set(MSG_ID.to_owned(), id.clone());
                }
            }
            envelopes.push(Envelope { port: port as usize, msg: handle });
        }
        if let Some(status) = output.status {
            self.report_status(status_object(status), cancel.clone()).await;
        }
        // Emit order is preserved: one envelope at a time.
        for envelope in envelopes {
            self.fan_out_one(envelope, cancel.clone()).await?;
        }
        Ok(())
    }

    fn write_logs(&self, logs: Vec<(GuestLogLevel, String)>) {
        if logs.is_empty() {
            return;
        }
        let mut state = self.lock();
        let now = Instant::now();
        let (start, count) = state.log_window.get_or_insert((now, 0));
        if now.duration_since(*start) >= Duration::from_secs(1) {
            *start = now;
            *count = 0;
        }
        for (level, text) in logs {
            let Some((_, count)) = state.log_window.as_mut() else { break };
            if *count >= GUEST_LOGS_PER_SECOND {
                state.logs_dropped += 1;
                continue;
            }
            *count += 1;
            let level = match level {
                GuestLogLevel::Debug => log::Level::Debug,
                GuestLogLevel::Info => log::Level::Info,
                GuestLogLevel::Warn => log::Level::Warn,
                GuestLogLevel::Error => log::Level::Error,
            };
            log::log!(target: "edgelink::wasm", level, "[{} {}] {}", self.spec.id, self.id(), text);
        }
        if state.logs_dropped > 0 && state.logs_dropped.is_power_of_two() {
            log::warn!(target: "edgelink::wasm", "[{} {}] {} guest log lines dropped (rate limit)", self.spec.id, self.id(), state.logs_dropped);
        }
    }
}

fn status_object(status: GuestStatus) -> StatusObject {
    let fill = match status.fill {
        0 => StatusFill::Red,
        1 => StatusFill::Green,
        2 => StatusFill::Yellow,
        3 => StatusFill::Blue,
        _ => StatusFill::Grey,
    };
    let shape = if status.shape == 0 { StatusShape::Ring } else { StatusShape::Dot };
    StatusObject { fill: Some(fill), shape: Some(shape), text: Some(status.text) }
}

/// Reject configuration the prototype cannot honour and check `wasmPlugin` version pinning.
fn check_node_properties(spec: &PluginSpec, config: &RedFlowNodeConfig) -> crate::Result<()> {
    let Some(rest) = config.rest.as_object() else {
        return Ok(());
    };
    for key in rest.keys() {
        if !EDITOR_PROPERTIES.contains(&key.as_str()) {
            return Err(not_supported(format!(
                "node type '{}' property '{key}': WASM plugin configuration is not implemented in this prototype",
                spec.type_name
            )));
        }
    }
    if let Some(pin) = rest.get("wasmPlugin") {
        let pin = pin.as_str().unwrap_or_default();
        let expected = format!("{}@{}", spec.id, spec.version.major);
        if pin != expected {
            return Err(not_supported(format!(
                "node type '{}' requires {pin}, active is {}@{}",
                spec.type_name, spec.id, spec.version
            )));
        }
    }
    Ok(())
}

impl FlowsElement for WasmPluginNode {
    fn id(&self) -> crate::runtime::model::ElementId {
        self.base.id()
    }
    fn name(&self) -> &str {
        self.base.name()
    }
    fn type_str(&self) -> &'static str {
        self.base.type_str()
    }
    fn ordering(&self) -> usize {
        self.base.ordering()
    }
    fn is_disabled(&self) -> bool {
        self.base.disabled()
    }
    fn parent_element(&self) -> Option<crate::runtime::model::ElementId> {
        self.base.flow().upgrade().map(|flow| flow.id())
    }
    fn as_any(&self) -> &dyn ::std::any::Any {
        self
    }
    fn get_path(&self) -> String {
        match self.base.flow().upgrade() {
            Some(flow) => format!("{}/{}", flow.get_path(), self.id()),
            None => self.id().to_string(),
        }
    }
}

#[async_trait::async_trait]
impl FlowNodeBehavior for WasmPluginNode {
    fn get_base(&self) -> &BaseFlowNodeState {
        &self.base
    }

    async fn run(self: Arc<Self>, stop_token: CancellationToken) {
        while !stop_token.is_cancelled() {
            let cancel = stop_token.child_token();
            with_uow(self.as_ref(), cancel.child_token(), |node, msg| async move { node.handle(msg, cancel).await })
                .await;
        }
        let mut state = self.lock();
        state.instance = None;
        state.cell = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::model::{ElementId, Msg};
    use crate::runtime::registry::RegistryHandle;
    use serde::Deserialize;
    use serde_json::{Value, json};

    const ENABLED: &str = r#"
        [runtime.context]
        default = "memory"
        [runtime.context.stores]
        memory = { provider = "memory" }
        [runtime.wasm]
        enabled = true
        failure_threshold = 2
    "#;

    fn config(toml: &str) -> config::Config {
        config::Config::builder().add_source(config::File::from_str(toml, config::FileFormat::Toml)).build().unwrap()
    }

    fn registry(id: &str, wat: &str, outputs: u8) -> RegistryHandle {
        let spec = PluginSpec::new(id, semver::Version::new(1, 2, 0), wat::parse_str(wat).unwrap(), outputs).unwrap();
        crate::runtime::registry::RegistryBuilder::default()
            .build()
            .unwrap()
            .with_wasm(crate::runtime::wasm::ActivePlugins::from_specs(vec![spec]))
    }

    fn inject(payloads: Value) -> Vec<(ElementId, Msg)> {
        Vec::deserialize(payloads).unwrap()
    }

    /// Emits a fixed EVE object `{ "_msgid": "x" }` regardless of input.
    const FORGE_MSGID: &str = r#"(module
      (import "edgelink:node/v1" "emit" (func $emit (param i32 i32 i32) (result i32)))
      (memory (export "memory") 1 1)
      (data (i32.const 0) "\ee\01\09\01\00\00\00\06\00\00\00_msgid\06\01\00\00\00x")
      (func (export "el_abi_version") (result i32) i32.const 1)
      (func (export "el_alloc") (param i32) (result i32) i32.const 1024)
      (func (export "el_on_input") (param i32 i32) (result i32)
        (call $emit (i32.const 0) (i32.const 0) (i32.const 23))))"#;

    const MSGID: &str = "0123456789abcdef";

    #[tokio::test]
    async fn identity_plugin_round_trips_payload_and_msgid() {
        let registry = registry("test/identity", include_str!("fixtures/identity.wat"), 1);
        let flows = json!([
            { "id": "100", "type": "tab" },
            { "id": "1", "z": "100", "type": "wasm-test-identity", "x": 10, "y": 20, "wasmPlugin": "test/identity@1", "wires": [["2"]] },
            { "id": "2", "z": "100", "type": "test-once" }
        ]);
        let engine = crate::runtime::engine::Engine::with_json(&registry, flows, Some(config(ENABLED))).unwrap();
        let msgs = engine
            .run_once_with_inject(
                1,
                Duration::from_secs(2),
                inject(json!([["1", { "_msgid": MSGID, "payload": "hello" }]])),
            )
            .await
            .unwrap();
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].get("payload").and_then(Variant::as_str), Some("hello"));
        assert_eq!(msgs[0].get(MSG_ID).and_then(Variant::as_str), Some(MSGID));
    }

    #[test]
    fn disabled_by_default_even_with_the_feature() {
        let registry = registry("test/identity", include_str!("fixtures/identity.wat"), 1);
        let flows = json!([
            { "id": "100", "type": "tab" },
            { "id": "1", "z": "100", "type": "wasm-test-identity", "wires": [[]] }
        ]);
        let err = crate::runtime::engine::Engine::with_json(&registry, flows, None).unwrap_err().to_string();
        assert!(err.contains("disabled by configuration"), "{err}");
    }

    #[test]
    fn missing_plugin_names_its_identity() {
        let registry = registry("test/identity", include_str!("fixtures/identity.wat"), 1);
        let flows = json!([
            { "id": "100", "type": "tab" },
            { "id": "1", "z": "100", "type": "wasm-acme-csvparse", "wires": [[]] }
        ]);
        let err =
            crate::runtime::engine::Engine::with_json(&registry, flows, Some(config(ENABLED))).unwrap_err().to_string();
        assert!(err.contains("acme/csvparse") && err.contains("not active"), "{err}");
    }

    #[test]
    fn undeclared_configuration_and_wrong_pin_are_rejected() {
        let registry = registry("test/identity", include_str!("fixtures/identity.wat"), 1);
        for (extra, needle) in [
            (json!({ "delimiter": "," }), "property 'delimiter'"),
            (json!({ "wasmPlugin": "test/identity@2" }), "requires test/identity@2, active is test/identity@1.2.0"),
        ] {
            let mut node = json!({ "id": "1", "z": "100", "type": "wasm-test-identity", "wires": [[]] });
            node.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
            let flows = json!([{ "id": "100", "type": "tab" }, node]);
            let err = crate::runtime::engine::Engine::with_json(&registry, flows, Some(config(ENABLED)))
                .unwrap_err()
                .to_string();
            assert!(err.contains(needle), "{err}");
        }
    }

    #[test]
    fn memory_budget_is_admitted_per_graph() {
        let registry = registry("test/identity", include_str!("fixtures/identity.wat"), 1);
        let toml = format!("{ENABLED}\nmemory_budget_kib = 4096\n");
        let nodes: Vec<Value> = (1..=3)
            .map(|i| json!({ "id": format!("{i}"), "z": "100", "type": "wasm-test-identity", "wires": [[]] }))
            .collect();
        let mut flows = vec![json!({ "id": "100", "type": "tab" })];
        flows.extend(nodes);
        let err = crate::runtime::engine::Engine::with_json(&registry, Value::Array(flows), Some(config(&toml)))
            .unwrap_err()
            .to_string();
        assert!(err.contains("memory budget exceeded"), "{err}");
    }

    #[tokio::test]
    async fn forged_msgid_is_a_fault_caught_by_catch() {
        let registry = registry("test/forge", FORGE_MSGID, 1);
        let flows = json!([
            { "id": "100", "type": "tab" },
            { "id": "1", "z": "100", "type": "wasm-test-forge", "wires": [["2"]] },
            { "id": "2", "z": "100", "type": "test-once" },
            { "id": "3", "z": "100", "type": "catch", "scope": ["1"], "uncaught": false, "wires": [["2"]] }
        ]);
        let engine = crate::runtime::engine::Engine::with_json(&registry, flows, Some(config(ENABLED))).unwrap();
        let msgs = engine
            .run_once_with_inject(1, Duration::from_secs(2), inject(json!([["1", { "_msgid": MSGID, "payload": 1 }]])))
            .await
            .unwrap();
        let text = msgs[0].get_nav("error.message").and_then(Variant::as_str).unwrap_or_default().to_owned();
        assert!(text.contains("changes _msgid"), "{text}");
    }

    #[tokio::test]
    async fn repeated_faults_put_the_node_in_the_failed_state() {
        let registry = registry("test/spin", include_str!("fixtures/spin.wat"), 1);
        let toml = format!("{ENABLED}\ndefault_fuel = 2000000\nfuel_slice = 1000000\n");
        let flows = json!([
            { "id": "100", "type": "tab" },
            { "id": "1", "z": "100", "type": "wasm-test-spin", "wires": [["2"]] },
            { "id": "2", "z": "100", "type": "test-once" },
            { "id": "3", "z": "100", "type": "catch", "scope": ["1"], "uncaught": false, "wires": [["2"]] }
        ]);
        let engine = crate::runtime::engine::Engine::with_json(&registry, flows, Some(config(&toml))).unwrap();
        let msgs = engine
            .run_once_with_inject(
                3,
                Duration::from_secs(5),
                inject(json!([["1", { "payload": 1 }], ["1", { "payload": 2 }], ["1", { "payload": 3 }]])),
            )
            .await
            .unwrap();
        let texts: Vec<String> = msgs
            .iter()
            .map(|m| m.get_nav("error.message").and_then(Variant::as_str).unwrap_or_default().to_owned())
            .collect();
        assert!(texts[0].contains("fuel budget"), "{texts:?}");
        assert!(texts[1].contains("fuel budget"), "{texts:?}");
        assert!(texts[2].contains("plugin failed"), "{texts:?}");
    }
}
