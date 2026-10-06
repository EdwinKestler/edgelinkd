//! Explicit credential-sidecar lifecycle commands.

use std::path::PathBuf;

use n2link_core::runtime::credential_storage::CredentialStore;

use crate::cliargs::{CliArgs, CredentialCommand};
use crate::config::load_config;

pub async fn execute(args: &CliArgs, command: &CredentialCommand) -> n2link_core::Result<()> {
    let cfg = load_config(args)?;
    let flows =
        PathBuf::from(cfg.get_string("flows_path").map_err(|_| anyhow::anyhow!("flows_path is not configured"))?);
    if !flows.exists() {
        return Err(anyhow::anyhow!("the flows file does not exist: {}", flows.display()).into());
    }
    let store = CredentialStore::from_config(Some(&cfg)).map_err(anyhow::Error::msg)?;

    match command {
        CredentialCommand::Status => print_json(&store.status(&flows).await.map_err(anyhow::Error::msg)?)?,
        CredentialCommand::Migrate { dry_run, backup_dir } => {
            print_json(&store.migrate(&flows, *dry_run, backup_dir.as_deref()).await.map_err(anyhow::Error::msg)?)?
        }
        CredentialCommand::Rotate { backup_key } => {
            print_json(&store.rotate(&flows, backup_key).await.map_err(anyhow::Error::msg)?)?
        }
        CredentialCommand::Recover { key_file } => {
            let key_id = store.recover_key(&flows, key_file).await.map_err(anyhow::Error::msg)?;
            print_json(&serde_json::json!({ "recovered": true, "keyId": key_id }))?;
        }
        CredentialCommand::Export { output } => {
            let result = store.export(&flows, output).await.map_err(anyhow::Error::msg)?;
            print_json(&result)?;
        }
    }
    Ok(())
}

fn print_json(value: &impl serde::Serialize) -> n2link_core::Result<()> {
    println!("{}", serde_json::to_string_pretty(value).map_err(anyhow::Error::from)?);
    Ok(())
}
