use crate::metrics::BlockMetrics;
use std::collections::HashMap;
type SharedMetrics = Arc<Mutex<HashMap<u64, BlockMetrics>>>;
use std::io::Write;
use std::sync::Arc;

use anyhow::Result;
use chrono::Utc;
use clap::Parser;
use env_logger::{Builder, Env};
// TODO: Replace log for tracing crate
use log::{error, warn};
use tokio::fs::create_dir_all;
use tokio::sync::Mutex;
use tokio::task;

#[cfg(zisk_hints)]
use alloy_consensus::crypto::install_default_provider;
#[cfg(zisk_hints)]
use guest_reth::CustomEvmCrypto;
#[cfg(zisk_hints)]
use revm::install_crypto;

mod api;
mod cliargs;
mod db;
mod hooks;
mod input;
mod metrics;
mod process;
mod prove;
mod state;
mod telegram;

use crate::cliargs::CliArgs;
use crate::state::AppState;
use input::{process_inputs_from_folder, process_inputs_from_rpc};

#[tokio::main]
async fn main() -> Result<()> {
    // Check if LOG_RUST is set; if not, set it to "info" for this application
    if std::env::var("RUST_LOG").is_err() {
        unsafe {
            std::env::set_var("RUST_LOG", "ethproofs_client=info");
        }
    }

    // Initialize the logger
    Builder::from_env(Env::default())
        .format(|buf, record| {
            let timestamp = Utc::now().format("%Y-%m-%dT%H:%M:%S%.6fZ");
            writeln!(buf, "{} [{}] - {}", timestamp, record.level(), record.args())
        })
        .init();

    // Load environment variables from .env file (if present)
    dotenv::dotenv().ok();

    // Parse CLI args
    let cliargs = CliArgs::parse();

    // Handle hidden subcommands
    if let Some(cmd) = &cliargs.command {
        match cmd {
            cliargs::Commands::InputServer => {
                warn!("InputServer subcommand is not yet implemented");
                return Ok(());
            }
        }
    }

    // Initialize application state
    let mut app_state = match AppState::new(cliargs).await {
        Ok(state) => state,
        Err(e) => {
            error!("Failed to initialize application state, error: {}", e);
            return Err(e);
        }
    };

    // Ensure all *_over_12s_total and *_under_12s_total counters appear in /metrics
    crate::metrics::TIME_TO_PROOF_UNDER_12S_TOTAL.inc_by(0);
    crate::metrics::TIME_TO_PROOF_OVER_12S_TOTAL.inc_by(0);
    crate::metrics::PROVING_UNDER_12S_TOTAL.inc_by(0);
    crate::metrics::PROVING_OVER_12S_TOTAL.inc_by(0);

    // Ensure input, output, and log directories exist
    create_dir_all(&app_state.cliargs.inputs.folder).await?;

    // Launch the metrics server if enabled
    if app_state.cliargs.metrics.enabled {
        let state_clone = app_state.clone();
        task::spawn(async move {
            if let Err(e) = metrics::start_metrics_server(state_clone).await {
                panic!("Metrics server exited, error: {}", e);
            }
        });
    }

    // Install custom EVM crypto
    #[cfg(zisk_hints)]
    {
        install_crypto(CustomEvmCrypto::default());
        install_default_provider(Arc::new(CustomEvmCrypto::default())).unwrap();
    }

    // Select input generation method
    match app_state.cliargs.inputs.mode {
        cliargs::InputGen::Rpc => {
            process_inputs_from_rpc(&mut app_state).await?;
        }
        cliargs::InputGen::Folder => {
            process_inputs_from_folder(&mut app_state).await?;
        }
    }
    Ok(())
}
