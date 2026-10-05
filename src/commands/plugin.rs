use std::sync::Arc;

use crate::cliargs::{CliArgs, PluginCommand};

pub async fn execute(_args: &Arc<CliArgs>, command: &PluginCommand) -> edgelink_core::Result<()> {
    match command {
        PluginCommand::List => {
            #[cfg(not(feature = "nodes_wasm"))]
            {
                Err(edgelink_core::EdgelinkError::NotSupported(
                    "WASM plugins are not compiled in this build (requires --features nodes_wasm)".to_owned(),
                ))
            }
            #[cfg(feature = "nodes_wasm")]
            {
                Err(edgelink_core::EdgelinkError::NotSupported(
                    "the WASM plugin store is not implemented in this prototype".to_owned(),
                ))
            }
        }
    }
}
