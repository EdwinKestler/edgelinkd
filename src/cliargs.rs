use std::path::PathBuf;

use clap::{Parser, Subcommand};

const LONG_ABOUT: &str = r#"
n2link Daemon Program

n2link is a Node-RED compatible back-end engine implemented in Rust.

Copyright (C) 2023-TODAY Li Wei and contributors. All rights reserved.

For more information, visit the website: https://github.com/oldrev/edgelink
"#;

#[derive(Parser, Debug, Clone)]
#[command(
    version = concat!(env!("CARGO_PKG_VERSION"), " • #", env!("N2LINK_BUILD_GIT_HASH"), " • built at ", env!("N2LINK_BUILD_TIME")), 
    about,
    author,
    long_about=LONG_ABOUT,
    color=clap::ColorChoice::Always
)]
pub struct CliArgs {
    /// Use verbose output, '0' means quiet, no output printed to stdout.
    #[arg(short, long, default_value_t = 2, global = true)]
    pub verbose: usize,

    /// Home directory of n2link, default is `~/.n2linkd`
    #[arg(long, global = true)]
    pub home: Option<String>,

    /// Path of the log configuration file.
    #[arg(short, long, global = true)]
    pub log_path: Option<String>,

    /// Set the running environment in 'dev' or 'prod', default is `dev`
    #[arg(long, global = true)]
    pub env: Option<String>,

    /// Use specified user directory
    #[arg(short = 'u', long, global = true)]
    pub user_dir: Option<String>,

    #[command(subcommand)]
    pub command: Option<Commands>,
}

#[derive(Subcommand, Debug, Clone)]
pub enum Commands {
    /// Run the n2link workflow engine
    Run {
        /// Path of the 'flows.json' file.
        #[arg()]
        flows_path: Option<String>,

        /// Run in headless mode (do not start web server)
        #[arg(long, default_value_t = false)]
        headless: bool,

        /// Server bind address for web interface
        #[arg(long, default_value = "127.0.0.1:1888")]
        bind: String,
    },
    /// List all available node types
    List,
    /// Inspect, migrate, rotate, recover, or export the credential sidecar
    Credentials {
        #[command(subcommand)]
        command: CredentialCommand,
    },
    /// Manage WASM plugins (offline; the runtime must be stopped)
    Plugin {
        #[command(subcommand)]
        command: PluginCommand,
    },
}

#[derive(Subcommand, Debug, Clone)]
pub enum PluginCommand {
    /// List active generations and quarantined packages as JSON
    List,
    /// Validate, quarantine and self-test a packed .wasm package
    Stage {
        /// Packed module (`n2linkd plugin pack` output)
        file: std::path::PathBuf,
    },
    /// Make a self-tested package current; the deployed flows must build with it
    Activate {
        /// Plugin id, `<publisher>/<name>`
        id: String,
        /// SHA-256 of the package, as printed by `stage`
        #[arg(long)]
        sha256: String,
    },
    /// Swap a plugin's current and previous generations
    Rollback {
        /// Plugin id, `<publisher>/<name>`
        id: String,
        /// SHA-256 of the previous generation, which becomes current
        #[arg(long)]
        sha256: String,
    },
    /// Deactivate a plugin no deployed node uses; its packages return to quarantine
    Remove {
        /// Plugin id, `<publisher>/<name>`
        id: String,
    },
    /// Delete a quarantined package
    Discard {
        /// SHA-256 of the quarantined package
        sha256: String,
    },
    /// Re-hash every active generation; exits non-zero on any problem
    Verify,
    /// Embed a manifest into a module as the `n2link.manifest` custom section
    Pack {
        /// Compiled wasm32 module
        module: std::path::PathBuf,
        /// Manifest (TOML, schema 1)
        manifest: std::path::PathBuf,
        /// Output path; must not exist
        #[arg(short, long)]
        output: std::path::PathBuf,
    },
}

#[derive(Subcommand, Debug, Clone)]
pub enum CredentialCommand {
    /// Report sidecar formats and key availability without exposing credentials
    Status,
    /// Encrypt plaintext sidecars after writing an explicit backup
    Migrate {
        /// Validate and report the migration without writing files
        #[arg(long)]
        dry_run: bool,
        /// Empty directory that receives the pre-migration installation files
        #[arg(long)]
        backup_dir: Option<PathBuf>,
    },
    /// Generate a new local key and re-encrypt both sidecar generations
    Rotate {
        /// New file that receives the prior keyring for recovery
        #[arg(long)]
        backup_key: PathBuf,
    },
    /// Restore a local keyring after proving it decrypts both sidecar generations
    Recover {
        /// Existing keyring backup to validate and install
        #[arg(long)]
        key_file: PathBuf,
    },
    /// Write an explicit private plaintext bundle for downgrade or recovery
    Export {
        /// New plaintext sidecar path; its matching .prev path must also not exist
        #[arg(long)]
        output: PathBuf,
    },
}

impl CliArgs {
    /*
    /// Get the actual flows path, considering user_dir if provided
    pub fn get_flows_path(&self, flows_path: Option<String>) -> String {
        if let Some(flows_path) = flows_path {
            flows_path
        } else {
            let base_dir = if let Some(ref user_dir) = self.user_dir {
                std::path::PathBuf::from(user_dir)
            } else {
                dirs_next::home_dir().expect("Can not found the $HOME dir!!!").join(n2link_core::compat::HOME_DIR_NAME)
            };
            base_dir.join("flows.json").to_string_lossy().to_string()
        }
    }

    /// Returns true if flows_path is user-specified, false if default
    pub fn is_flows_path_user(&self, flows_path: &Option<String>) -> bool {
        flows_path.is_some()
    }
    */

    /// Merge CliArgs into config::Config, overriding config values with CLI values if set
    pub fn merge_into_config_builder(
        &self,
        builder: config::ConfigBuilder<config::builder::DefaultState>,
    ) -> Result<config::ConfigBuilder<config::builder::DefaultState>, config::ConfigError> {
        let mut builder = builder;
        // Handle all Run subcommand parameters together
        if let Some(Commands::Run { flows_path, bind, headless }) = &self.command {
            if let Some(fp) = flows_path {
                builder = builder.set_override("flows_path", fp.clone())?;
            }
            builder = builder.set_override("bind", bind.clone())?;
            builder = builder.set_override("headless", *headless)?;
        }
        // verbose
        builder = builder.set_override("verbose", self.verbose as i64)?;
        // log_path
        if let Some(ref log_path) = self.log_path {
            builder = builder.set_override("log_path", log_path.clone())?;
        }
        // env
        if let Some(ref env) = self.env {
            builder = builder.set_override("env", env.clone())?;
        }
        // user_dir
        if let Some(ref user_dir) = self.user_dir {
            builder = builder.set_override("user_dir", user_dir.clone())?;
        }
        // home
        if let Some(ref home) = self.home {
            builder = builder.set_override("home", home.clone())?;
        }
        Ok(builder)
    }
}
