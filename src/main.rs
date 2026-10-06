use std::fmt::Display;
use std::process::ExitCode;
use std::sync::Arc;

// 3rd-party libs
use clap::Parser;

use n2link_core::Result;

include!(concat!(env!("OUT_DIR"), "/__use_node_plugins.rs"));

mod app;
mod cliargs;
mod commands;
mod config;
mod consts;
mod defaults;
mod env;
mod flows;
mod logging;
mod registry;
mod runner;

pub use cliargs::*;

#[tokio::main]
async fn main() -> ExitCode {
    let args = Arc::new(CliArgs::parse());
    match runner::run_app(args).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            report_application_error(&err);
            ExitCode::FAILURE
        }
    }
}

fn report_application_error(err: &impl Display) {
    if log::log_enabled!(log::Level::Error) {
        log::error!("Application error: {err}");
    } else {
        eprintln!("Application error: {err}");
    }
}
