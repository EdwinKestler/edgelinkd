//! Active plugin generations and their interned `MetaNode`s.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

use sha2::{Digest, Sha256};

use crate::runtime::nodes::{MetaNode, NodeFactory, NodeKind};

use super::plugin_node::WasmPluginNode;

/// One active plugin generation. Built by the (future) plugin store, or directly by tests.
#[derive(Clone, Debug)]
pub(crate) struct PluginSpec {
    /// `wasm-<publisher>-<name>`.
    pub type_name: String,
    /// `<publisher>/<name>`.
    pub id: String,
    pub version: semver::Version,
    pub wasm: Arc<Vec<u8>>,
    pub sha256: [u8; 32],
    pub outputs: u8,
}

// Constructed by tests today and by the plugin store once it exists (DESIGN.md §9).
#[cfg_attr(not(test), allow(dead_code))]
impl PluginSpec {
    pub(crate) fn new(id: &str, version: semver::Version, wasm: Vec<u8>, outputs: u8) -> crate::Result<Self> {
        let (publisher, name) = id
            .split_once('/')
            .filter(|(p, n)| valid_segment(p) && valid_segment(n))
            .ok_or_else(|| crate::EdgelinkError::invalid_operation(&format!("invalid WASM plugin id '{id}'")))?;
        if outputs > 16 {
            return Err(crate::EdgelinkError::invalid_operation("a WASM plugin node has at most 16 outputs"));
        }
        let sha256: [u8; 32] = Sha256::digest(&wasm).into();
        Ok(Self {
            type_name: format!("wasm-{publisher}-{name}"),
            id: id.to_owned(),
            version,
            wasm: Arc::new(wasm),
            sha256,
            outputs,
        })
    }
}

/// `[a-z][a-z0-9]{0,31}`: no dashes, so `wasm-<publisher>-<name>` is injective.
#[cfg_attr(not(test), allow(dead_code))]
fn valid_segment(segment: &str) -> bool {
    let mut chars = segment.chars();
    matches!(chars.next(), Some('a'..='z'))
        && segment.len() <= 32
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
}

#[derive(Debug)]
pub struct ActivePlugins {
    specs: HashMap<String, PluginSpec>,
}

impl ActivePlugins {
    // Built by tests today and by the plugin store once it exists (DESIGN.md §9).
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn from_specs(specs: Vec<PluginSpec>) -> Arc<Self> {
        Arc::new(Self { specs: specs.into_iter().map(|spec| (spec.type_name.clone(), spec)).collect() })
    }

    pub(crate) fn get(&self, type_name: &str) -> Option<&PluginSpec> {
        self.specs.get(type_name)
    }

    pub(crate) fn meta(&self, type_name: &str) -> Option<&'static MetaNode> {
        self.specs.get(type_name).map(intern_meta)
    }
}

/// `BaseFlowNodeState::type_str` is `&'static str` and is exported in flow JSON, so each plugin
/// type needs a `'static` `MetaNode`. Each distinct (type, outputs, version) is leaked exactly
/// once, so the total is bounded by the plugin generations a process ever activates.
fn intern_meta(spec: &PluginSpec) -> &'static MetaNode {
    type Key = (String, u8, String);
    static METAS: OnceLock<Mutex<HashMap<Key, &'static MetaNode>>> = OnceLock::new();
    let key: Key = (spec.type_name.clone(), spec.outputs, spec.version.to_string());
    let mut metas = METAS.get_or_init(|| Mutex::new(HashMap::new())).lock().unwrap_or_else(|e| e.into_inner());
    if let Some(meta) = metas.get(&key) {
        return meta;
    }
    let type_name: &'static str = Box::leak(spec.type_name.clone().into_boxed_str());
    let module: &'static str = Box::leak(format!("wasm/{}", spec.id).into_boxed_str());
    let version: &'static str = Box::leak(spec.version.to_string().into_boxed_str());
    let meta: &'static MetaNode = Box::leak(Box::new(MetaNode::new(
        NodeKind::Flow,
        type_name,
        NodeFactory::Flow(WasmPluginNode::build),
        type_name,
        type_name,
        module,
        version,
        false,
        true,
        1,
        spec.outputs,
        false,
    )));
    metas.insert(key, meta);
    meta
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_map_to_injective_type_names() {
        let spec = PluginSpec::new("acme/csvparse", semver::Version::new(1, 2, 0), vec![0], 1).unwrap();
        assert_eq!(spec.type_name, "wasm-acme-csvparse");
        for bad in ["acme", "Acme/x", "a-b/c", "a/b/c", "../x", "1a/b", ""] {
            assert!(PluginSpec::new(bad, semver::Version::new(1, 0, 0), vec![0], 1).is_err(), "{bad}");
        }
    }

    #[test]
    fn meta_is_interned_once_per_generation() {
        let spec = PluginSpec::new("acme/intern", semver::Version::new(1, 0, 0), vec![0], 2).unwrap();
        let set = ActivePlugins::from_specs(vec![spec.clone()]);
        let again = ActivePlugins::from_specs(vec![spec]);
        let a = set.meta("wasm-acme-intern").unwrap();
        let b = again.meta("wasm-acme-intern").unwrap();
        assert!(std::ptr::eq(a, b));
        assert_eq!(a.type_(), "wasm-acme-intern");
        assert_eq!(a.ports().outputs, 2);
        assert_eq!(a.version(), "1.0.0");
    }
}
