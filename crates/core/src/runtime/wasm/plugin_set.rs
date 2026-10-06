//! Active plugin generations and their interned `MetaNode`s.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

use sha2::{Digest, Sha256};

use crate::runtime::nodes::{MetaNode, NodeFactory, NodeKind};

#[cfg(test)]
use super::manifest::ConfigField;
use super::manifest::{LimitRequest, Manifest, split_id};
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
    /// Requests from the manifest; clamped against the settings' ceilings at deploy.
    pub limits: LimitRequest,
    /// The validated manifest (editor metadata, configuration fields).
    pub manifest: Arc<Manifest>,
}

impl PluginSpec {
    /// A spec for raw module bytes without a manifest (tests only).
    #[cfg(test)]
    pub(crate) fn new(id: &str, version: semver::Version, wasm: Vec<u8>, outputs: u8) -> crate::Result<Self> {
        let manifest = super::manifest::synthetic(id, &version, outputs, Vec::new());
        Self::build(id, version, wasm, Arc::new(manifest))
    }

    /// Test helper: replace the limit requests and configuration fields.
    #[cfg(test)]
    pub(crate) fn configured(mut self, limits: LimitRequest, config: Vec<ConfigField>) -> Self {
        let manifest = Arc::make_mut(&mut self.manifest);
        manifest.limits = limits;
        manifest.node.config = config;
        self.limits = limits;
        self
    }

    /// A spec for a packaged plugin: identity, outputs and limits come from its manifest.
    pub(crate) fn from_package(wasm: Vec<u8>, manifest: &Manifest) -> crate::Result<Self> {
        Self::build(&manifest.plugin.id, manifest.version(), wasm, Arc::new(manifest.clone()))
    }

    fn build(id: &str, version: semver::Version, wasm: Vec<u8>, manifest: Arc<Manifest>) -> crate::Result<Self> {
        let (publisher, name) = split_id(id)?;
        let outputs = manifest.node.outputs;
        if outputs > 16 {
            return Err(crate::N2linkError::invalid_operation("a WASM plugin node has at most 16 outputs"));
        }
        let sha256: [u8; 32] = Sha256::digest(&wasm).into();
        Ok(Self {
            type_name: format!("wasm-{publisher}-{name}"),
            id: id.to_owned(),
            version,
            wasm: Arc::new(wasm),
            sha256,
            outputs,
            limits: manifest.limits,
            manifest,
        })
    }
}

#[derive(Debug)]
pub struct ActivePlugins {
    specs: HashMap<String, PluginSpec>,
}

impl ActivePlugins {
    /// A set from packed modules (manifest section included). Validates framing and manifests
    /// but does not compile or self-test: use the plugin store for installation.
    pub fn from_packages(packages: Vec<Vec<u8>>) -> crate::Result<Arc<Self>> {
        let mut specs = Vec::with_capacity(packages.len());
        for bytes in packages {
            let manifest = Manifest::parse(&super::section::manifest_text(&bytes, usize::MAX)?)?;
            specs.push(PluginSpec::from_package(bytes, &manifest)?);
        }
        Ok(Self::from_specs(specs))
    }

    pub(crate) fn from_specs(specs: Vec<PluginSpec>) -> Arc<Self> {
        Arc::new(Self { specs: specs.into_iter().map(|spec| (spec.type_name.clone(), spec)).collect() })
    }

    pub(crate) fn specs(&self) -> impl Iterator<Item = &PluginSpec> {
        self.specs.values()
    }

    pub(crate) fn get(&self, type_name: &str) -> Option<&PluginSpec> {
        self.specs.get(type_name)
    }

    /// The interned `MetaNode` of an active plugin type.
    pub fn meta(&self, type_name: &str) -> Option<&'static MetaNode> {
        self.specs.get(type_name).map(intern_meta)
    }

    pub fn len(&self) -> usize {
        self.specs.len()
    }

    pub fn is_empty(&self) -> bool {
        self.specs.is_empty()
    }

    /// Active plugins sorted by type name, for the editor, `/nodes` and the Copilot catalog.
    pub fn views(&self) -> Vec<PluginView<'_>> {
        let mut views: Vec<PluginView<'_>> = self
            .specs
            .values()
            .map(|spec| PluginView {
                type_name: &spec.type_name,
                id: &spec.id,
                version: &spec.version,
                manifest: &spec.manifest,
            })
            .collect();
        views.sort_by(|a, b| a.type_name.cmp(b.type_name));
        views
    }
}

/// Read-only description of one active plugin generation.
#[derive(Debug, Clone, Copy)]
pub struct PluginView<'a> {
    /// `wasm-<publisher>-<name>`.
    pub type_name: &'a str,
    /// `<publisher>/<name>`.
    pub id: &'a str,
    pub version: &'a semver::Version,
    pub manifest: &'a Manifest,
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
