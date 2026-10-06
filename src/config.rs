use n2link_core::compat;

use crate::cliargs::CliArgs;
use crate::defaults::create_default_config_file;

pub fn load_config(cli_args: &CliArgs) -> anyhow::Result<config::Config> {
    // Collect config file paths for logging
    let mut config_files = Vec::new();
    // Priority order: --user-dir > --home > N2LINK_HOME (or EDGELINK_HOME) > ~/.n2linkd (or ~/.edgelinkd).
    // The default is resolved only when nothing else names a home, so its legacy warning is not
    // printed for an explicit home.
    let env_home = compat::env_var("HOME").map_err(anyhow::Error::msg)?;
    let env_run_env = compat::env_var("RUN_ENV").map_err(anyhow::Error::msg)?;
    let run_env = cli_args.env.clone().or(env_run_env).unwrap_or("dev".to_owned());
    let edgelink_home_dir = match cli_args.user_dir.clone().or(cli_args.home.clone()).or(env_home) {
        Some(dir) => Some(dir),
        None => Some(
            dirs_next::home_dir()
                .map(|x| compat::default_home_dir(&x).to_string_lossy().to_string())
                .expect("Cannot get the `~/home` directory"),
        ),
    };

    // Only set default flows_path if not specified by any source
    let mut builder = config::Config::builder();
    let mut home_dir_val = None;
    if let Some(ref hd) = edgelink_home_dir {
        home_dir_val = Some(hd.clone());
        builder = builder.set_override("home_dir", hd.clone())?;
        // Add config file paths for logging
        let main_cfg = compat::config_file(std::path::Path::new(hd), None);
        let env_cfg = compat::config_file(std::path::Path::new(hd), Some(&run_env));
        config_files.push(main_cfg.display().to_string());
        config_files.push(env_cfg.display().to_string());
        // Actually add config files to builder
        builder = builder
            .add_source(config::File::with_name(&main_cfg.to_string_lossy()).required(false))
            .add_source(config::File::with_name(&env_cfg.to_string_lossy()).required(false));
    }
    // Merge CLI args into config builder (higher priority than default flows_path)
    builder = cli_args.merge_into_config_builder(builder)?;

    // Check if flows_path is specified by any source (CLI, config file, env)
    let flows_path = builder.clone().build()?.get_string("flows_path").ok();
    if flows_path.is_none() {
        if let Some(ref hd) = home_dir_val {
            use std::path::PathBuf;
            let default_flows = PathBuf::from(hd).join("flows.json").to_string_lossy().to_string();
            builder = builder.set_override("flows_path", default_flows)?;
            builder = builder.set_override("flows_path_is_default", true)?;
        }
    } else {
        builder = builder.set_override("flows_path_is_default", false)?;
    }

    if cli_args.verbose > 0 {
        if let Some(ref x) = edgelink_home_dir {
            eprintln!("$N2LINK_HOME={x}");
            eprintln!("Loading config files:");
            for f in &config_files {
                eprintln!("\t- `{f}`");
            }
        }
    }

    // Ensure the config directory exists and has default config
    if let Some(ref config_dir) = edgelink_home_dir {
        create_default_config_file(config_dir)?;
    }

    // Continue to set other config items
    builder = builder.set_override("run_env", run_env)?;
    builder = builder.set_override("node.msg_queue_capacity", 1)?;

    let config = builder.build()?;
    Ok(config)
}
