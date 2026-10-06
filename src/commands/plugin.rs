//! Offline WASM plugin store commands (DESIGN.md §9). They take the store lock, so a running
//! edgelinkd that has the store open makes them fail instead of racing it.

use std::sync::Arc;

use crate::cliargs::{CliArgs, PluginCommand};

#[cfg(not(feature = "nodes_wasm"))]
pub async fn execute(_args: &Arc<CliArgs>, _command: &PluginCommand) -> n2link_core::Result<()> {
    Err(n2link_core::N2linkError::NotSupported(
        "WASM plugins are not compiled in this build (requires --features nodes_wasm)".to_owned(),
    ))
}

#[cfg(feature = "nodes_wasm")]
pub async fn execute(args: &Arc<CliArgs>, command: &PluginCommand) -> n2link_core::Result<()> {
    imp::execute(args, command).await
}

#[cfg(feature = "nodes_wasm")]
mod imp {
    use std::path::Path;
    use std::sync::Arc;

    use n2link_core::runtime::engine::Engine;
    use n2link_core::runtime::flow_credentials::flows_value_with_credentials;
    use n2link_core::runtime::wasm::{append_manifest, ActivePlugins, PluginStore};
    use n2link_core::N2linkError;
    use serde_json::Value;

    use crate::cliargs::{CliArgs, PluginCommand};
    use crate::config::load_config;
    use crate::registry::create_registry;

    pub async fn execute(args: &Arc<CliArgs>, command: &PluginCommand) -> n2link_core::Result<()> {
        if let PluginCommand::Pack { module, manifest, output } = command {
            return pack(module, manifest, output);
        }
        let cfg = load_config(args)?;
        let store = PluginStore::open(&cfg)?;
        match command {
            PluginCommand::List => print_json(&store.list()?),
            PluginCommand::Stage { file } => {
                let bytes = read_bounded(file, store_limit(&cfg))?;
                print_json(&store.stage(&bytes)?)
            }
            PluginCommand::Activate { id, sha256 } => {
                let flows = deployed_flows(&cfg).await?;
                let prepare = preparer(&cfg, &flows)?;
                print_json(&store.activate(id, sha256, &prepare)?)
            }
            PluginCommand::Rollback { id, sha256 } => {
                let flows = deployed_flows(&cfg).await?;
                let prepare = preparer(&cfg, &flows)?;
                print_json(&store.rollback(id, sha256, &prepare)?)
            }
            PluginCommand::Remove { id } => {
                let flows = deployed_flows(&cfg).await?;
                let users = nodes_using(&flows, id);
                if !users.is_empty() {
                    return Err(N2linkError::invalid_operation(&format!(
                        "plugin {id} is used by deployed node(s) {}; remove them from the flows first",
                        users.join(", ")
                    )));
                }
                store.remove(id)?;
                print_json(&serde_json::json!({ "removed": id }))
            }
            PluginCommand::Discard { sha256 } => {
                store.discard(sha256)?;
                print_json(&serde_json::json!({ "discarded": sha256 }))
            }
            PluginCommand::Verify => {
                let problems = store.verify()?;
                print_json(&serde_json::json!({ "ok": problems.is_empty(), "problems": problems }))?;
                if problems.is_empty() {
                    Ok(())
                } else {
                    Err(N2linkError::invalid_operation(&format!("{} plugin store problem(s)", problems.len())))
                }
            }
            PluginCommand::Pack { .. } => unreachable!("handled above"),
        }
    }

    /// `[runtime.wasm] max_module_kib` (default 512) plus room for the manifest section.
    fn store_limit(cfg: &config::Config) -> u64 {
        let kib = cfg.get_int("runtime.wasm.max_module_kib").ok().and_then(|v| u64::try_from(v).ok()).unwrap_or(512);
        kib.saturating_mul(1024).saturating_add(64 * 1024)
    }

    /// Read at most `limit` bytes; a larger file is refused without being read whole.
    fn read_bounded(path: &Path, limit: u64) -> n2link_core::Result<Vec<u8>> {
        use std::io::Read;
        let file = std::fs::File::open(path)?;
        let mut bytes = Vec::new();
        file.take(limit + 1).read_to_end(&mut bytes)?;
        if bytes.len() as u64 > limit {
            return Err(N2linkError::invalid_operation(&format!(
                "{} is larger than {limit} bytes; raise [runtime.wasm] max_module_kib if this is intended",
                path.display()
            )));
        }
        Ok(bytes)
    }

    fn pack(module: &Path, manifest: &Path, output: &Path) -> n2link_core::Result<()> {
        if output.exists() {
            return Err(N2linkError::invalid_operation(&format!("{} already exists", output.display())));
        }
        let module_bytes = read_bounded(module, 64 * 1024 * 1024)?;
        let text = std::fs::read_to_string(manifest)?;
        let packed = append_manifest(&module_bytes, &text)?;
        std::fs::write(output, &packed)?;
        print_json(&serde_json::json!({ "output": output.display().to_string(), "bytes": packed.len() }))
    }

    /// The flows the runtime would deploy on its next start (`[]` when there is no flows file).
    async fn deployed_flows(cfg: &config::Config) -> n2link_core::Result<Value> {
        let path =
            cfg.get_string("flows_path").map_err(|_| N2linkError::invalid_operation("flows_path is not configured"))?;
        let path = Path::new(&path);
        if !path.exists() {
            return Ok(Value::Array(Vec::new()));
        }
        flows_value_with_credentials(path, Some(cfg)).await
    }

    /// The deployed graph must build with the candidate plugin set, as it would at startup with
    /// `[runtime.wasm] enabled = true`. Nothing is started.
    fn preparer(
        cfg: &config::Config,
        flows: &Value,
    ) -> n2link_core::Result<impl Fn(Arc<ActivePlugins>) -> n2link_core::Result<()>> {
        let reg = create_registry()?;
        let enabled = config::Config::builder()
            .add_source(cfg.clone())
            .set_override("runtime.wasm.enabled", true)
            .and_then(|builder| builder.build())
            .map_err(|e| N2linkError::invalid_operation(&e.to_string()))?;
        let flows = flows.clone();
        Ok(move |plugins: Arc<ActivePlugins>| {
            Engine::prepare_flows(&flows, &reg.with_wasm(plugins), Some(enabled.clone()))
        })
    }

    /// Ids of flow nodes whose type is this plugin's `wasm-<publisher>-<name>`.
    fn nodes_using(flows: &Value, id: &str) -> Vec<String> {
        let type_name = format!("wasm-{}", id.replacen('/', "-", 1));
        flows
            .as_array()
            .into_iter()
            .flatten()
            .filter(|node| node.get("type").and_then(Value::as_str) == Some(type_name.as_str()))
            .map(|node| node.get("id").and_then(Value::as_str).unwrap_or("?").to_owned())
            .collect()
    }

    fn print_json(value: &impl serde::Serialize) -> n2link_core::Result<()> {
        println!("{}", serde_json::to_string_pretty(value).map_err(anyhow::Error::from)?);
        Ok(())
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn nodes_using_matches_only_the_plugin_type() {
            let flows = serde_json::json!([
                { "id": "a", "type": "wasm-acme-upper" },
                { "id": "b", "type": "wasm-acme-upperx" },
                { "id": "c", "type": "function" },
            ]);
            assert_eq!(nodes_using(&flows, "acme/upper"), vec!["a".to_owned()]);
            assert!(nodes_using(&serde_json::json!({}), "acme/upper").is_empty());
        }
    }
}
