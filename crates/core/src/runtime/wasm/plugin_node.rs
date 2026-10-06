//! Flow node that runs an active WASM plugin.
//!
//! One instance per node, created on the first message and reused, so guest state survives
//! between messages. A new instance first receives the node configuration through `el_init`;
//! `el_close` runs when the node stops with an idle instance. Calls run on the blocking pool under a global permit; every call is bounded
//! by fuel, a wall-clock deadline and cancellation (see `exec.rs`). A fault (fuel, deadline,
//! trap, host bound) discards the instance; `failure_threshold` faults within `failure_window_s`
//! put the node in a failed state until the flow is redeployed.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio_util::sync::CancellationToken;

use crate::N2linkError;
use crate::runtime::flow::Flow;
use crate::runtime::model::json::RedFlowNodeConfig;
use crate::runtime::model::{Envelope, FlowsElement, MsgHandle, Variant};
use crate::runtime::nodes::*;

use super::convert::{decode_variant, encode_variant};
use super::exec::{CallError, CallOutput, EngineCell, GuestLogLevel, GuestStatus, Instance};
use super::host::{EffectiveLimits, WasmRuntime};
use super::manifest::ConfigKind;
use super::plugin_set::PluginSpec;

/// Node properties Node-RED's editor writes for any node. Anything else must be a
/// `[[node.config]]` field the plugin declares.
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
    /// EVE-encoded configuration object passed to `el_init`.
    config: Arc<Vec<u8>>,
    runtime: Arc<WasmRuntime>,
    state: Mutex<NodeRuntimeState>,
}

fn not_supported(text: String) -> N2linkError {
    N2linkError::NotSupported(text)
}

impl WasmPluginNode {
    pub(crate) fn build(
        flow: &Flow,
        base_node: BaseFlowNodeState,
        config: &RedFlowNodeConfig,
        _options: Option<&config::Config>,
    ) -> crate::Result<Box<dyn FlowNodeBehavior>> {
        let type_name = base_node.type_str;
        let engine = flow.engine().ok_or_else(|| N2linkError::invalid_operation("WASM plugin has no engine"))?;
        let runtime = engine.wasm_runtime();
        if !runtime.settings().enabled {
            return Err(super::settings::disabled_plugin_error(type_name));
        }
        let spec = engine
            .wasm_plugins()
            .and_then(|set| set.get(type_name).cloned())
            .ok_or_else(|| super::not_active_error(type_name))?;
        let node_config = check_node_properties(&spec, config)?;
        if node_config.len() > runtime.max_input_bytes() {
            return Err(not_supported(format!(
                "node type '{type_name}' configuration encodes to {} bytes, above [runtime.wasm] max_input_kib",
                node_config.len()
            )));
        }
        let limits = runtime.effective_limits(&spec)?;
        runtime.admit(&spec, &limits)?;
        Ok(Box::new(Self {
            base: base_node,
            spec,
            limits,
            config: Arc::new(node_config),
            runtime,
            state: Mutex::new(NodeRuntimeState::default()),
        }))
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, NodeRuntimeState> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn error(&self, text: impl std::fmt::Display) -> N2linkError {
        N2linkError::invalid_operation(&format!("wasm {}: {text}", self.spec.id))
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
        let node_config = self.config.clone();
        let pages = self.limits.memory_pages;
        let budget = self.runtime.budget(&self.limits);
        let cancelled = Arc::new(AtomicBool::new(false));
        let flag = cancelled.clone();
        let mut join = tokio::task::spawn_blocking(move || {
            let _permit = permit;
            let mut init_logs = Vec::new();
            let mut instance = match instance {
                Some(instance) => instance,
                None => {
                    let created = cell
                        .module(spec.sha256, &spec.wasm)
                        .and_then(|module| cell.instantiate(&module, pages, spec.outputs, &budget));
                    let mut instance = match created {
                        Ok(instance) => instance,
                        Err(err) => return (None, Err(CallError::Fault(err.to_string()))),
                    };
                    // A guest that rejects its configuration is a fault: it counts towards the
                    // failed state instead of being retried silently on every message.
                    match instance.init(&node_config, &budget, &flag) {
                        Ok(output) => init_logs = output.logs,
                        Err(CallError::Guest { text, .. }) | Err(CallError::Fault(text)) => {
                            return (None, Err(CallError::Fault(text)));
                        }
                        Err(CallError::Cancelled) => return (None, Err(CallError::Cancelled)),
                    }
                    instance
                }
            };
            let mut result = instance.call(&bytes, &budget, &flag);
            if let Ok(output) = &mut result {
                init_logs.append(&mut output.logs);
                output.logs = init_logs;
            }
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
    async fn fault(&self, text: String, cancel: CancellationToken) -> N2linkError {
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
            if let Some(engine) = self.base.flow().upgrade().and_then(|flow| flow.engine()) {
                let version = self.spec.version.to_string();
                engine.history().record_plugin(
                    "runtime",
                    "plugin.failed",
                    &self.spec.id,
                    Some(&version),
                    Some(&super::store::hex(&self.spec.sha256)),
                    Some("three_strikes"),
                );
            }
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

/// The configuration object `el_init` receives, as EVE bytes. Each declared field takes the
/// node's value, else the manifest default; a required field with neither is an error. An
/// empty string counts as absent for non-string kinds (the editor's empty number box).
pub(crate) fn resolve_config(
    spec: &PluginSpec,
    props: Option<&serde_json::Map<String, serde_json::Value>>,
) -> crate::Result<Vec<u8>> {
    use serde::Deserialize as _;
    let mut object = serde_json::Map::new();
    for field in &spec.manifest.node.config {
        let given = props.and_then(|p| p.get(&field.name)).filter(|value| {
            !value.is_null() && !(field.kind != ConfigKind::String && value.as_str().is_some_and(str::is_empty))
        });
        let Some(value) = given.or(field.default.as_ref()) else {
            if field.required {
                return Err(N2linkError::invalid_operation(&format!(
                    "node type '{}' property '{}' is required",
                    spec.type_name, field.name
                )));
            }
            continue;
        };
        let value = field.check(value).map_err(|why| {
            N2linkError::invalid_operation(&format!("node type '{}' property '{}' {why}", spec.type_name, field.name))
        })?;
        object.insert(field.name.clone(), value);
    }
    let variant = Variant::deserialize(serde_json::Value::Object(object))
        .map_err(|err| N2linkError::invalid_operation(&format!("plugin configuration: {err}")))?;
    encode_variant(&variant).map_err(|err| N2linkError::invalid_operation(&format!("plugin configuration: {err}")))
}

/// Reject properties the plugin does not declare, check `wasmPlugin` version pinning, and
/// resolve the configuration for `el_init`.
fn check_node_properties(spec: &PluginSpec, config: &RedFlowNodeConfig) -> crate::Result<Vec<u8>> {
    let Some(rest) = config.rest.as_object() else {
        return resolve_config(spec, None);
    };
    let declared = &spec.manifest.node.config;
    for key in rest.keys() {
        if !EDITOR_PROPERTIES.contains(&key.as_str()) && !declared.iter().any(|field| &field.name == key) {
            return Err(not_supported(format!(
                "node type '{}' property '{key}' is not declared by WASM plugin {}",
                spec.type_name, spec.id
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
    resolve_config(spec, Some(rest))
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
        let (instance, cell) = {
            let mut state = self.lock();
            (state.instance.take(), state.cell.take())
        };
        if let Some(instance) = instance {
            self.close_instance(instance).await;
        }
        drop(cell);
    }
}

impl WasmPluginNode {
    /// `el_close` for an idle instance, under a permit and the plugin's own budget. Problems
    /// are logged: stopping is never blocked beyond one deadline.
    async fn close_instance(&self, mut instance: Instance) {
        let budget = self.runtime.budget(&self.limits);
        let permit = match tokio::time::timeout(self.limits.deadline, self.runtime.permits().acquire_owned()).await {
            Ok(Ok(permit)) => permit,
            _ => {
                log::warn!(target: "edgelink::wasm", "[{} {}] el_close skipped: concurrency limit", self.spec.id, self.id());
                return;
            }
        };
        let joined = tokio::task::spawn_blocking(move || {
            let _permit = permit;
            instance.close(&budget, &AtomicBool::new(false))
        })
        .await;
        match joined {
            Ok(Ok(output)) => self.write_logs(output.logs),
            Ok(Err(CallError::Guest { text, logs })) => {
                self.write_logs(logs);
                log::warn!(target: "edgelink::wasm", "[{} {}] {text}", self.spec.id, self.id());
            }
            Ok(Err(err)) => log::warn!(target: "edgelink::wasm", "[{} {}] el_close: {err}", self.spec.id, self.id()),
            Err(_) => log::warn!(target: "edgelink::wasm", "[{} {}] el_close: host panic", self.spec.id, self.id()),
        }
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

    fn spec(id: &str, wat: &str, outputs: u8) -> PluginSpec {
        PluginSpec::new(id, semver::Version::new(1, 2, 0), wat::parse_str(wat).unwrap(), outputs).unwrap()
    }

    fn registry_of(specs: Vec<PluginSpec>) -> RegistryHandle {
        crate::runtime::registry::RegistryBuilder::default()
            .build()
            .unwrap()
            .with_wasm(crate::runtime::wasm::ActivePlugins::from_specs(specs))
    }

    fn registry(id: &str, wat: &str, outputs: u8) -> RegistryHandle {
        registry_of(vec![spec(id, wat, outputs)])
    }

    fn field(toml: &str) -> crate::runtime::wasm::manifest::ConfigField {
        toml_edit::de::from_str(toml).unwrap()
    }

    fn limits(fuel: u64, deadline_ms: u64) -> crate::runtime::wasm::manifest::LimitRequest {
        crate::runtime::wasm::manifest::LimitRequest {
            memory_pages: None,
            fuel_per_message: Some(fuel),
            deadline_ms: Some(deadline_ms),
        }
    }

    /// Emits `{"n": 2}` on port 1, then `{"n": 1}` on port 0.
    const TWO_PORTS: &str = r#"(module
      (import "edgelink:node/v1" "emit" (func $emit (param i32 i32 i32) (result i32)))
      (memory (export "memory") 1 1)
      (data (i32.const 0) "\ee\01\09\01\00\00\00\01\00\00\00n\03\01\00\00\00\00\00\00\00")
      (data (i32.const 32) "\ee\01\09\01\00\00\00\01\00\00\00n\03\02\00\00\00\00\00\00\00")
      (func (export "el_abi_version") (result i32) i32.const 1)
      (func (export "el_alloc") (param i32) (result i32) i32.const 1024)
      (func (export "el_on_input") (param i32 i32) (result i32)
        (drop (call $emit (i32.const 1) (i32.const 32) (i32.const 21)))
        (call $emit (i32.const 0) (i32.const 0) (i32.const 21))))"#;

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
            (json!({ "delimiter": "," }), "property 'delimiter' is not declared"),
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
        let toml = format!("{ENABLED}\nmemory_budget_kib = 1024\n");
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
    async fn configuration_reaches_el_init_with_defaults_and_overrides() {
        let configured = spec("test/config", include_str!("fixtures/config.wat"), 1).configured(
            Default::default(),
            vec![
                field("name = \"delimiter\"\nkind = \"string\"\ndefault = \",\""),
                field("name = \"limit\"\nkind = \"number\"\ninteger = true\nmax = 10.0\ndefault = 5"),
            ],
        );
        let registry = registry_of(vec![configured]);
        let flows = json!([
            { "id": "100", "type": "tab" },
            { "id": "1", "z": "100", "type": "wasm-test-config", "limit": "7", "wires": [["2"]] },
            { "id": "2", "z": "100", "type": "test-once" }
        ]);
        let engine = crate::runtime::engine::Engine::with_json(&registry, flows, Some(config(ENABLED))).unwrap();
        let msgs = engine
            .run_once_with_inject(1, Duration::from_secs(2), inject(json!([["1", { "_msgid": MSGID, "payload": 1 }]])))
            .await
            .unwrap();
        assert_eq!(msgs[0].get("delimiter").and_then(Variant::as_str), Some(","));
        assert_eq!(msgs[0].get("limit").and_then(Variant::as_i64), Some(7));
        assert_eq!(msgs[0].get(MSG_ID).and_then(Variant::as_str), Some(MSGID));
    }

    #[test]
    fn invalid_or_missing_configuration_fails_deploy() {
        let configured = spec("test/config", include_str!("fixtures/config.wat"), 1).configured(
            Default::default(),
            vec![
                field("name = \"limit\"\nkind = \"number\"\nmax = 10.0\ndefault = 5"),
                field("name = \"mode\"\nkind = \"enum\"\nvalues = [\"a\", \"b\"]\nrequired = true"),
            ],
        );
        let registry = registry_of(vec![configured]);
        for (props, needle) in [
            (json!({ "mode": "a", "limit": 11 }), "property 'limit' must be within"),
            (json!({ "mode": "c" }), "property 'mode' must be one of"),
            (json!({}), "property 'mode' is required"),
            (json!({ "mode": "" }), "property 'mode' is required"),
        ] {
            let mut node = json!({ "id": "1", "z": "100", "type": "wasm-test-config", "wires": [[]] });
            node.as_object_mut().unwrap().extend(props.as_object().unwrap().clone());
            let flows = json!([{ "id": "100", "type": "tab" }, node]);
            let err = crate::runtime::engine::Engine::with_json(&registry, flows, Some(config(ENABLED)))
                .unwrap_err()
                .to_string();
            assert!(err.contains(needle), "{props}: {err}");
        }
    }

    #[tokio::test]
    async fn outputs_on_several_ports_arrive_in_emit_order() {
        let registry = registry("test/ports", TWO_PORTS, 2);
        let flows = json!([
            { "id": "100", "type": "tab" },
            { "id": "1", "z": "100", "type": "wasm-test-ports", "wires": [["2"], ["2"]] },
            { "id": "2", "z": "100", "type": "test-once" }
        ]);
        let engine = crate::runtime::engine::Engine::with_json(&registry, flows, Some(config(ENABLED))).unwrap();
        let msgs = engine
            .run_once_with_inject(2, Duration::from_secs(2), inject(json!([["1", { "_msgid": MSGID }]])))
            .await
            .unwrap();
        let order: Vec<i64> = msgs.iter().map(|m| m.get("n").and_then(Variant::as_i64).unwrap()).collect();
        assert_eq!(order, vec![2, 1]);
        assert!(msgs.iter().all(|m| m.get(MSG_ID).and_then(Variant::as_str) == Some(MSGID)));
    }

    #[tokio::test]
    async fn link_call_returns_through_a_plugin() {
        let registry = registry("test/identity", include_str!("fixtures/identity.wat"), 1);
        let flows = json!([
            { "id": "100", "type": "tab" },
            { "id": "200", "type": "tab" },
            { "id": "1", "z": "100", "type": "link in", "wires": [["2"]] },
            { "id": "2", "z": "100", "type": "wasm-test-identity", "wires": [["3"]] },
            { "id": "3", "z": "100", "type": "link out", "mode": "return" },
            { "id": "4", "z": "200", "type": "link call", "links": ["1"], "wires": [["5"]] },
            { "id": "5", "z": "200", "type": "test-once" }
        ]);
        let engine = crate::runtime::engine::Engine::with_json(&registry, flows, Some(config(ENABLED))).unwrap();
        let msgs = engine
            .run_once_with_inject(1, Duration::from_secs(2), inject(json!([["4", { "payload": "via link" }]])))
            .await
            .unwrap();
        assert_eq!(msgs[0].get("payload").and_then(Variant::as_str), Some("via link"));
        assert!(msgs[0].link_call_stack.as_ref().is_none_or(|stack| stack.is_empty()));
    }

    #[tokio::test]
    async fn exhausted_global_permits_fail_the_message_after_its_deadline() {
        let quick =
            spec("test/identity", include_str!("fixtures/identity.wat"), 1).configured(limits(2_000_000, 50), vec![]);
        let registry = registry_of(vec![quick]);
        let toml = format!("{ENABLED}\nmax_concurrent = 1\n");
        let flows = json!([
            { "id": "100", "type": "tab" },
            { "id": "1", "z": "100", "type": "wasm-test-identity", "wires": [["2"]] },
            { "id": "2", "z": "100", "type": "test-once" },
            { "id": "3", "z": "100", "type": "catch", "scope": ["1"], "uncaught": false, "wires": [["2"]] }
        ]);
        let engine = crate::runtime::engine::Engine::with_json(&registry, flows, Some(config(&toml))).unwrap();
        // Another plugin call holds the only permit for the whole test.
        let _held = engine.wasm_runtime().permits().acquire_owned().await.unwrap();
        let started = Instant::now();
        let msgs = engine
            .run_once_with_inject(1, Duration::from_secs(2), inject(json!([["1", { "payload": 1 }]])))
            .await
            .unwrap();
        let text = msgs[0].get_nav("error.message").and_then(Variant::as_str).unwrap_or_default().to_owned();
        assert!(text.contains("concurrency limit"), "{text}");
        assert!(started.elapsed() < Duration::from_secs(1), "{:?}", started.elapsed());
    }

    #[tokio::test]
    async fn stopping_cancels_a_running_call_within_a_slice() {
        let spin =
            spec("test/spin", include_str!("fixtures/spin.wat"), 1).configured(limits(1_000_000_000, 5_000), vec![]);
        let registry = registry_of(vec![spin]);
        let flows = json!([
            { "id": "100", "type": "tab" },
            { "id": "1", "z": "100", "type": "wasm-test-spin", "wires": [[]] }
        ]);
        let engine = crate::runtime::engine::Engine::with_json(&registry, flows, Some(config(ENABLED))).unwrap();
        let runtime = engine.wasm_runtime();
        let permits = runtime.settings().max_concurrent as usize;
        engine.start().await.unwrap();
        let (id, msg) = inject(json!([["1", { "payload": 1 }]])).pop().unwrap();
        engine.inject_msg(&id, MsgHandle::new(msg), CancellationToken::new()).await.unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        while runtime.permits().available_permits() == permits && Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert_eq!(runtime.permits().available_permits(), permits - 1, "the guest is running");
        let stopped = Instant::now();
        engine.stop().await.unwrap();
        while runtime.permits().available_permits() < permits && stopped.elapsed() < Duration::from_secs(2) {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        // Without cancellation the guest would hold the permit until its fuel ran out (~0.7 s).
        assert!(stopped.elapsed() < Duration::from_millis(300), "{:?}", stopped.elapsed());
        assert_eq!(runtime.permits().available_permits(), permits);
    }

    /// End to end with the Rust example plugins built by `scripts/wasm-examples.sh --e2e`:
    /// stage (validation + self-test), activate, then run inject → uppercase → csvparse.
    #[tokio::test]
    #[ignore = "needs the example plugins built for wasm32: scripts/wasm-examples.sh --e2e"]
    async fn example_plugins_install_and_run() {
        let dir = std::env::var("N2LINK_WASM_EXAMPLES")
            .expect("N2LINK_WASM_EXAMPLES must name the directory with uppercase.wasm and csvparse.wasm");
        let home = std::env::temp_dir().join(format!("edgelink-wasm-e2e-{}", uuid::Uuid::new_v4()));
        let store = super::super::store::PluginStore::open_at(
            home.join("plugins"),
            super::super::settings::WasmSettings::default(),
        )
        .unwrap();
        for name in ["uppercase", "csvparse"] {
            let bytes = std::fs::read(format!("{dir}/{name}.wasm")).unwrap();
            let report = store.stage(&bytes).unwrap();
            assert_eq!(report.status, super::super::store::PackageStatus::Ready, "{report:?}");
            store.activate(&report.id, &report.sha256, &|_| Ok(())).unwrap();
        }
        let (set, problems) = store.active_plugins().unwrap();
        assert!(problems.is_empty(), "{problems:?}");
        let registry = registry_of(set.specs().cloned().collect());
        let flows = json!([
            { "id": "100", "type": "tab" },
            { "id": "1", "z": "100", "type": "wasm-edgelink-uppercase", "wasmPlugin": "edgelink/uppercase@1", "wires": [["2"]] },
            { "id": "2", "z": "100", "type": "wasm-edgelink-csvparse", "delimiter": ";", "header": true, "wires": [["3"]] },
            { "id": "3", "z": "100", "type": "test-once" }
        ]);
        let engine = crate::runtime::engine::Engine::with_json(&registry, flows, Some(config(ENABLED))).unwrap();
        let msgs = engine
            .run_once_with_inject(
                1,
                Duration::from_secs(5),
                inject(json!([["1", { "_msgid": MSGID, "payload": "name;qty\nbolt;4\nnut;7" }]])),
            )
            .await
            .unwrap();
        let rows = msgs[0].get("payload").cloned().unwrap();
        let rows: Value = serde_json::to_value(&rows).unwrap();
        assert_eq!(rows, json!([{ "NAME": "BOLT", "QTY": "4" }, { "NAME": "NUT", "QTY": "7" }]));
        assert_eq!(msgs[0].get(MSG_ID).and_then(Variant::as_str), Some(MSGID));
        drop(store);
        let _ = std::fs::remove_dir_all(home);
    }

    /// G2 message cost: 2,000 messages with a 1 KiB string payload through one `uppercase`
    /// node (EVE encode/decode, permit, blocking call, delivery), engine start/stop and the first
    /// instantiation included. Run with `--profile ci` for representative numbers.
    #[tokio::test]
    #[ignore = "measurement; needs N2LINK_WASM_EXAMPLES (scripts/wasm-examples.sh)"]
    async fn example_plugin_message_cost() {
        const N: usize = 2000;
        let dir = std::env::var("N2LINK_WASM_EXAMPLES").expect("N2LINK_WASM_EXAMPLES");
        let bytes = std::fs::read(format!("{dir}/uppercase.wasm")).unwrap();
        let set = crate::runtime::wasm::ActivePlugins::from_packages(vec![bytes]).unwrap();
        let registry = registry_of(set.specs().cloned().collect());
        let flows = json!([
            { "id": "100", "type": "tab" },
            { "id": "1", "z": "100", "type": "wasm-edgelink-uppercase", "wires": [["2"]] },
            { "id": "2", "z": "100", "type": "test-once" }
        ]);
        let engine = crate::runtime::engine::Engine::with_json(&registry, flows, Some(config(ENABLED))).unwrap();
        let payload = "x".repeat(1024);
        let msgs: Vec<Value> = (0..N).map(|_| json!(["1", { "payload": payload }])).collect();
        let started = Instant::now();
        let out = engine.run_once_with_inject(N, Duration::from_secs(120), inject(Value::Array(msgs))).await.unwrap();
        let elapsed = started.elapsed();
        assert_eq!(out.len(), N);
        assert_eq!(out[0].get("payload").and_then(Variant::as_str).map(str::len), Some(1024));
        println!(
            "{{\"case\":\"in-tree-uppercase-1KiB\",\"arch\":\"{}\",\"messages\":{N},\"us_per_message\":{:.1}}}",
            std::env::consts::ARCH,
            elapsed.as_secs_f64() * 1e6 / N as f64
        );
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
