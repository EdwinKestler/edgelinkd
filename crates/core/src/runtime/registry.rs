use std::collections::HashMap;
use std::ops::Deref;
use std::sync::Arc;

use crate::runtime::nodes::*;

inventory::collect!(MetaNode);

pub trait Registry: 'static + Send + Sync {
    fn all(&self) -> &HashMap<&'static str, &'static MetaNode>;
    fn get(&self, type_name: &str) -> Option<&'static MetaNode>;
    fn hints(&self, type_name: &str) -> Option<&'static crate::runtime::nodes::NodeHints>;
    #[cfg(feature = "nodes_wasm")]
    fn wasm(&self) -> Option<&Arc<crate::runtime::wasm::ActivePlugins>>;
}

#[derive(Debug, Clone)]
pub struct RegistryHandle(Arc<dyn Registry>);

impl Deref for RegistryHandle {
    type Target = Arc<dyn Registry>;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

#[derive(Debug, Clone)]
struct RegistryImpl {
    meta_nodes: Arc<HashMap<&'static str, &'static MetaNode>>,
    hints: Arc<HashMap<&'static str, &'static crate::runtime::nodes::NodeHints>>,
    #[cfg(feature = "nodes_wasm")]
    wasm: Option<Arc<crate::runtime::wasm::ActivePlugins>>,
}

#[derive(Debug)]
pub struct RegistryBuilder {
    meta_nodes: HashMap<&'static str, &'static MetaNode>,
}

impl Default for RegistryBuilder {
    fn default() -> Self {
        Self::new().with_builtins()
    }
}

impl RegistryBuilder {
    pub fn new() -> Self {
        Self { meta_nodes: HashMap::new() }
    }

    pub fn register(mut self, meta_node: &'static MetaNode) -> Self {
        self.meta_nodes.insert(meta_node.type_, meta_node);
        self
    }

    pub fn with_builtins(mut self) -> Self {
        for meta in inventory::iter::<MetaNode> {
            log::debug!("[REGISTRY] Available built-in Node: '{}'", meta.type_);
            self.meta_nodes.insert(meta.type_, meta);
        }
        self
    }

    pub fn build(self) -> crate::Result<RegistryHandle> {
        if self.meta_nodes.is_empty() {
            log::warn!("There are no meta node in the Registry!");
        }
        let mut hints = HashMap::new();
        for hint in inventory::iter::<crate::runtime::nodes::NodeHints> {
            hints.insert(hint.type_, hint);
        }

        let result = RegistryHandle(Arc::new(RegistryImpl {
            meta_nodes: Arc::new(self.meta_nodes),
            hints: Arc::new(hints),
            #[cfg(feature = "nodes_wasm")]
            wasm: None,
        }));
        Ok(result)
    }
}

impl RegistryHandle {
    #[cfg(feature = "nodes_wasm")]
    pub fn with_wasm(&self, plugins: Arc<crate::runtime::wasm::ActivePlugins>) -> RegistryHandle {
        let mut hints = HashMap::new();
        for name in self.all().keys() {
            if let Some(hint) = self.hints(name) {
                hints.insert(*name, hint);
            }
        }
        RegistryHandle(Arc::new(RegistryImpl {
            meta_nodes: Arc::new(self.all().clone()),
            hints: Arc::new(hints),
            wasm: Some(plugins),
        }))
    }
}

impl RegistryImpl {}

impl Registry for RegistryImpl {
    fn all(&self) -> &HashMap<&'static str, &'static MetaNode> {
        &self.meta_nodes
    }

    fn get(&self, type_name: &str) -> Option<&'static MetaNode> {
        self.meta_nodes.get(type_name).copied()
    }

    fn hints(&self, type_name: &str) -> Option<&'static crate::runtime::nodes::NodeHints> {
        self.hints.get(type_name).copied()
    }

    #[cfg(feature = "nodes_wasm")]
    fn wasm(&self) -> Option<&Arc<crate::runtime::wasm::ActivePlugins>> {
        self.wasm.as_ref()
    }
}

impl std::fmt::Debug for dyn Registry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Registry").field("meta_nodes", self.all()).finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::nodes::{NODE_METADATA_VERSION, NodeKind};

    #[test]
    fn every_registered_node_has_valid_metadata() {
        let registry = RegistryBuilder::default().build().unwrap();
        assert_eq!(NODE_METADATA_VERSION, 2);
        assert!(!registry.all().is_empty());
        for (name, meta) in registry.all() {
            let ports = meta.ports();
            assert!(ports.inputs <= 8, "{name} inputs");
            assert!(ports.outputs <= 16, "{name} outputs");
            if matches!(meta.kind(), NodeKind::Global) {
                assert_eq!(ports.inputs, 0, "{name} global inputs");
                assert_eq!(ports.outputs, 0, "{name} global outputs");
            }
        }
        let inject = registry.get("inject").unwrap();
        assert_eq!(inject.ports().inputs, 0);
        assert_eq!(inject.ports().outputs, 1);
        let debug = registry.get("debug").unwrap();
        assert_eq!(debug.ports().outputs, 0);
        let mqtt_in = registry.get("mqtt in").unwrap();
        assert_eq!(mqtt_in.ports().inputs, 0);
        let hints = registry.hints("mqtt in").unwrap();
        assert_eq!(hints.config_refs, &[("broker", "mqtt-broker")]);
        assert!(registry.hints("mqtt-broker").unwrap().secret_fields.contains(&"password"));
    }

    /// Declared ports must match the registered cardinality, and every node whose output index is
    /// ambiguous (more than one output, or dynamic outputs) must name its ports.
    #[test]
    fn port_hints_match_registered_ports() {
        use crate::runtime::nodes::{valid_payload_type, valid_port_name};
        let registry = RegistryBuilder::default().build().unwrap();
        let mut seen = std::collections::HashSet::new();
        for hint in inventory::iter::<NodeHints> {
            assert!(seen.insert(hint.type_), "{} submits node_hints! twice; the registry keeps one", hint.type_);
            assert!(valid_payload_type(hint.input), "{} input payload '{}'", hint.type_, hint.input);
            for port in hint.outputs {
                assert!(valid_port_name(port.name), "{} port name '{}'", hint.type_, port.name);
                assert!(
                    valid_payload_type(port.payload),
                    "{} port '{}' payload '{}'",
                    hint.type_,
                    port.name,
                    port.payload
                );
            }
            for (i, port) in hint.outputs.iter().enumerate() {
                assert!(
                    !hint.outputs[..i].iter().any(|p| p.name == port.name),
                    "{} duplicate port '{}'",
                    hint.type_,
                    port.name
                );
            }
            let Some(meta) = registry.get(hint.type_) else { continue };
            let ports = meta.ports();
            if ports.inputs == 0 {
                assert_eq!(hint.input, "any", "{} declares an input payload without an input", hint.type_);
            }
            if hint.outputs.is_empty() {
                continue;
            }
            if ports.dynamic_outputs {
                assert_eq!(hint.outputs.len(), 1, "{} dynamic outputs declare one repeating port", hint.type_);
                assert!(hint.outputs[0].name.contains("{n}"), "{} repeating port name needs {{n}}", hint.type_);
            } else {
                assert_eq!(hint.outputs.len(), usize::from(ports.outputs), "{} port count", hint.type_);
            }
        }
        for (name, meta) in registry.all() {
            let ports = meta.ports();
            if matches!(meta.kind(), NodeKind::Flow) && (ports.outputs > 1 || ports.dynamic_outputs) {
                let named = registry.hints(name).is_some_and(|h| !h.outputs.is_empty());
                assert!(named, "{name} has an ambiguous output index and must name its ports");
            }
        }
        let exec = registry.hints("exec").unwrap();
        assert_eq!(exec.outputs.iter().map(|p| p.name).collect::<Vec<_>>(), ["stdout", "stderr", "return code"]);
    }

    #[test]
    fn payload_type_vocabulary() {
        use crate::runtime::nodes::valid_payload_type;
        for ok in ["any", "string", "string|buffer", "object|array|null"] {
            assert!(valid_payload_type(ok), "{ok}");
        }
        for bad in ["", "str", "string|", "string|string", "any|string", "String", "string | buffer"] {
            assert!(!valid_payload_type(bad), "{bad}");
        }
    }

    #[test]
    fn owned_feature_gated_types_are_listed() {
        let registry = RegistryBuilder::default().build().unwrap();
        for name in registry.all().keys() {
            if name.starts_with("ai-") || matches!(*name, "postgres" | "postgres-config" | "redis" | "redis-config") {
                assert!(crate::runtime::nodes::n2link_owned_node_type(name), "{name}");
            }
        }
        assert!(crate::runtime::nodes::n2link_owned_node_type("ai-agent"));
        #[cfg(not(feature = "nodes_ai_agent"))]
        assert!(registry.get("ai-agent").is_none());
        assert!(!crate::runtime::nodes::n2link_owned_node_type("nodered-foo"));
        for name in registry.all().keys() {
            assert!(!name.starts_with("wasm-"), "built-in type {name} uses the reserved wasm- prefix");
        }
        assert!(crate::runtime::nodes::is_wasm_plugin_type("wasm-acme-csvparse"));
        assert!(!crate::runtime::nodes::is_wasm_plugin_type("function"));
    }
}
