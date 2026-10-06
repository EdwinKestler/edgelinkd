//! WASM plugin host. The `wasm-` type prefix is always reserved; the interpreter is feature-gated.

use crate::N2linkError;

#[cfg(feature = "nodes_wasm")]
mod convert;
#[cfg(feature = "nodes_wasm")]
mod exec;
#[cfg(feature = "nodes_wasm")]
mod host;
#[cfg(feature = "nodes_wasm")]
mod manifest;
#[cfg(feature = "nodes_wasm")]
mod plugin_node;
#[cfg(feature = "nodes_wasm")]
mod plugin_set;
#[cfg(feature = "nodes_wasm")]
mod section;
#[cfg(feature = "nodes_wasm")]
mod settings;
#[cfg(feature = "nodes_wasm")]
mod store;
#[cfg(feature = "nodes_wasm")]
pub(crate) use host::WasmRuntime;
#[cfg(feature = "nodes_wasm")]
pub use manifest::{ConfigField, ConfigKind, LimitRequest, Manifest, NodeSpec, PluginMeta};
#[cfg(feature = "nodes_wasm")]
pub use plugin_set::PluginView;

/// Plugin host state for `/status`.
#[cfg(feature = "nodes_wasm")]
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WasmStatus {
    /// `disabled`, `idle` (no Wasmi engine) or `active`.
    pub state: &'static str,
    pub plugins: usize,
    pub engine_live: bool,
    pub permits_in_use: usize,
    pub max_concurrent: u32,
    pub memory_reserved_kib: u64,
    pub memory_budget_kib: u32,
}
#[cfg(feature = "nodes_wasm")]
pub use plugin_set::ActivePlugins;
#[cfg(feature = "nodes_wasm")]
pub use section::append_manifest;
#[cfg(feature = "nodes_wasm")]
pub(crate) use settings::WasmSettings;
#[cfg(feature = "nodes_wasm")]
pub use store::{ActiveEntry, Listing, PackageStatus, PendingChange, PluginStore, PrepareFn, StageReport};

use crate::runtime::engine::Engine;
use crate::runtime::nodes::MetaNode;

/// Error for a `wasm-*` type that cannot run in this process.
pub(crate) fn resolve_plugin_meta(engine: &Engine, type_name: &str) -> crate::Result<&'static MetaNode> {
    #[cfg(not(feature = "nodes_wasm"))]
    {
        let _ = engine;
        Err(unavailable_plugin_error(type_name))
    }
    #[cfg(feature = "nodes_wasm")]
    {
        engine.wasm_meta(type_name)
    }
}

pub(crate) fn unavailable_plugin_error(type_name: &str) -> N2linkError {
    #[cfg(not(feature = "nodes_wasm"))]
    {
        N2linkError::NotSupported(format!(
            "node type '{type_name}' is not compiled in this build (requires nodes_wasm)"
        ))
    }
    #[cfg(feature = "nodes_wasm")]
    {
        settings::disabled_plugin_error(type_name)
    }
}

/// Open the plugin store and attach its active set to `reg` when `[runtime.wasm] enabled = true`.
/// Disabled → `(reg, None)` and nothing on disk is touched. The returned store holds the lock
/// for as long as it lives. Generations that fail verification are logged and left out, so
/// flows that use them fail to deploy naming the plugin.
#[cfg(feature = "nodes_wasm")]
pub fn attach_store(
    reg: &crate::runtime::registry::RegistryHandle,
    cfg: &config::Config,
) -> crate::Result<(crate::runtime::registry::RegistryHandle, Option<PluginStore>)> {
    if !WasmSettings::from_config(Some(cfg))?.enabled {
        return Ok((reg.clone(), None));
    }
    let store = PluginStore::open(cfg)?;
    let (plugins, problems) = store.active_plugins()?;
    for problem in &problems {
        log::error!("[WASM] plugin left out: {problem}");
    }
    log::info!("[WASM] plugin store {} open, {} plugin(s) active", store.root().display(), plugins.specs().count());
    Ok((reg.with_wasm(plugins), Some(store)))
}

/// `wasm-<publisher>-<name>` → `<publisher>/<name>` (segments contain no dashes).
#[cfg(any(test, feature = "nodes_wasm"))]
pub(crate) fn plugin_id_of(type_name: &str) -> String {
    let rest = type_name.strip_prefix("wasm-").unwrap_or(type_name);
    match rest.split_once('-') {
        Some((publisher, name)) => format!("{publisher}/{name}"),
        None => rest.to_owned(),
    }
}

#[cfg(feature = "nodes_wasm")]
pub(crate) fn not_active_error(type_name: &str) -> N2linkError {
    N2linkError::NotSupported(format!(
        "node type '{type_name}' requires WASM plugin {} which is not active",
        plugin_id_of(type_name)
    ))
}

/// A boolean setting that must be a TOML boolean: absent → `None`; `"yes"`, `1` or any other
/// kind → error (the `config` crate would otherwise coerce strings such as `"yes"`).
pub(crate) fn strict_bool(cfg: &config::Config, key: &str) -> crate::Result<Option<bool>> {
    match cfg.get::<config::Value>(key) {
        Err(_) => Ok(None),
        Ok(value) => match value.kind {
            config::ValueKind::Boolean(flag) => Ok(Some(flag)),
            _ => Err(N2linkError::invalid_operation(&format!("{key} must be true or false"))),
        },
    }
}

/// `[runtime.wasm] enabled = true` is refused unless `nodes_wasm` is compiled in.
pub(crate) fn reject_enabled_without_feature(cfg: Option<&config::Config>) -> crate::Result<()> {
    let Some(cfg) = cfg else {
        return Ok(());
    };
    let Some(enabled) = strict_bool(cfg, "runtime.wasm.enabled")? else {
        return Ok(());
    };
    if enabled {
        #[cfg(not(feature = "nodes_wasm"))]
        {
            return Err(N2linkError::NotSupported(
                "WASM plugins are not compiled in this build (requires nodes_wasm)".to_owned(),
            ));
        }
        #[cfg(feature = "nodes_wasm")]
        {
            let _ = enabled;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn wasm_prefix_fails_loud_when_the_feature_is_off() {
        let flows = json!([
            { "id": "100", "type": "tab" },
            { "id": "1", "z": "100", "type": "wasm-acme-csvparse", "wires": [[]] }
        ]);
        let err = crate::runtime::engine::build_test_engine(flows).unwrap_err();
        let text = err.to_string();
        #[cfg(not(feature = "nodes_wasm"))]
        assert!(text.contains("not compiled in this build (requires nodes_wasm)"), "{text}");
        // With the feature, the test engine has no configuration, so plugins are off.
        #[cfg(feature = "nodes_wasm")]
        assert!(text.contains("disabled by configuration"), "{text}");
        assert!(text.contains("wasm-acme-csvparse"), "{text}");
        assert!(matches!(err, N2linkError::NotSupported(_)), "{err:?}");
    }

    #[test]
    fn a_wasm_config_node_is_never_unknown() {
        let flows = json!([
            { "id": "100", "type": "tab" },
            { "id": "9", "type": "wasm-acme-settings" }
        ]);
        let err = crate::runtime::engine::build_test_engine(flows).unwrap_err();
        assert!(matches!(err, N2linkError::NotSupported(_)), "{err:?}");
    }

    #[test]
    fn plugin_ids_come_from_type_names() {
        assert_eq!(plugin_id_of("wasm-acme-csvparse"), "acme/csvparse");
    }

    #[test]
    fn enabled_must_be_a_boolean() {
        let cfg = config::Config::builder()
            .add_source(config::File::from_str("[runtime.wasm]\nenabled = \"yes\"", config::FileFormat::Toml))
            .build()
            .unwrap();
        assert!(reject_enabled_without_feature(Some(&cfg)).is_err());
    }

    #[test]
    fn enabled_true_without_the_feature_is_not_supported() {
        let cfg = config::Config::builder()
            .add_source(config::File::from_str(
                r#"
                [runtime.context]
                default = "memory"
                [runtime.context.stores]
                memory = { provider = "memory" }
                [runtime.wasm]
                enabled = true
                "#,
                config::FileFormat::Toml,
            ))
            .build()
            .unwrap();
        let result = reject_enabled_without_feature(Some(&cfg));
        #[cfg(not(feature = "nodes_wasm"))]
        {
            let err = result.unwrap_err();
            assert!(err.to_string().contains("nodes_wasm"), "{err}");
        }
        #[cfg(feature = "nodes_wasm")]
        {
            result.unwrap();
        }
    }
}
