use std::collections::HashMap;
use std::ops::Deref;
use std::sync::Arc;

use crate::runtime::nodes::*;

inventory::collect!(MetaNode);

pub trait Registry: 'static + Send + Sync {
    fn all(&self) -> &HashMap<&'static str, &'static MetaNode>;
    fn get(&self, type_name: &str) -> Option<&'static MetaNode>;
    fn hints(&self, type_name: &str) -> Option<&'static crate::runtime::nodes::NodeHints>;
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

        let result =
            RegistryHandle(Arc::new(RegistryImpl { meta_nodes: Arc::new(self.meta_nodes), hints: Arc::new(hints) }));
        Ok(result)
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
        assert_eq!(NODE_METADATA_VERSION, 1);
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
}
