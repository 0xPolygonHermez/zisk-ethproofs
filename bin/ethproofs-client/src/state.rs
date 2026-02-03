use std::{
    env,
    sync::{Arc, Mutex},
    time::Instant,
};

use anyhow::Context;
use clap::Parser;
use log::warn;
// serde derive imports no longer needed after moving protocol types
use ethproofs_common::protocol::BlockInfo;
use tonic::transport::Channel;

use crate::{
    api::EthProofsApi,
    cliargs::CliArgs,
    db::{self, DbBlockProofs},
};

#[derive(Debug, Default)]
pub(crate) struct FiredAlerts {
    pub(crate) skipped: bool,
    pub(crate) failed: bool,
}

pub const DEFAULT_INPUTS_FOLDER: &str = "inputs";
pub const DEFAULT_COORDINATOR_URL: &str = "http://localhost:50051";
pub const DEFAULT_WEBHOOK_PORT: u16 = 8051;
pub const DEFAULT_METRICS_PORT: u16 = 8384;

// Protocol types now imported from ethproofs-protocol crate

#[derive(Clone)]
pub struct AppState {
    pub shared_metrics: crate::SharedMetrics,
    pub cliargs: CliArgs,
    pub proving_block: Arc<Mutex<Option<BlockInfo>>>,
    pub next_proving_block: Arc<Mutex<Option<BlockInfo>>>,
    pub current_job_id: Arc<Mutex<String>>,
    pub queued_start: Arc<Mutex<Instant>>,
    pub ethproofs_client: Option<EthProofsApi>,
    pub ethproofs_cluster_id: Option<u32>,
    pub coordinator_channel: Option<Channel>,
    pub block_modulus: u64,
    pub rpc_ws_url: String,
    pub webhook_port: u16,
    pub metrics_port: u16,
    pub inputs_folder: String,
    pub input_gen_server_url: String,
    pub compute_capacity: u32,
    pub db_block_proofs: Option<DbBlockProofs>,
    pub fired_alerts: Arc<Mutex<FiredAlerts>>,
}

impl AppState {
    pub async fn new() -> anyhow::Result<Self> {
        // Parse the command line arguments
        let cliargs = CliArgs::parse();

        // Load environment variables from env file
        dotenv::from_filename(&cliargs.env_file).ok();

        let (ethproofs_client, ethproofs_cluster_id) = if cliargs.submit_ethproofs {
            let ethproofs_url = match env::var("ETHPROOFS_API_URL") {
                Ok(url) => url,
                Err(_) => panic!("ETHPROOFS_API_URL not set"),
            };
            let ethproofs_token = match env::var("ETHPROOFS_API_TOKEN") {
                Ok(token) => token,
                Err(_) => panic!("ETHPROOFS_API_TOKEN not set"),
            };
            let ethproofs_cluster_id = match env::var("ETHPROOFS_CLUSTER_ID") {
                Ok(cid_str) => match cid_str.parse::<u32>() {
                    Ok(cid) => cid,
                    Err(_) => panic!("ETHPROOFS_CLUSTER_ID is not a valid u32"),
                },
                Err(_) => panic!("ETHPROOFS_CLUSTER_ID not set"),
            };

            (Some(EthProofsApi::new(ethproofs_url, ethproofs_token)), Some(ethproofs_cluster_id))
        } else {
            (None, None)
        };

        let coordinator_url =
            env::var("COORDINATOR_URL").unwrap_or(DEFAULT_COORDINATOR_URL.to_string());
        let coordinator_channel = if !cliargs.skip_proving {
            let coordinator_channel = Channel::from_shared(coordinator_url.clone())
                .context("Failed to create coordinator channel")?
                .connect()
                .await
                .with_context(|| {
                    format!("Failed to connect to coordinator at {}", coordinator_url)
                })?;
            Some(coordinator_channel)
        } else {
            None
        };

        let inputs_folder = env::var("INPUTS_FOLDER").unwrap_or(DEFAULT_INPUTS_FOLDER.to_string());

        let input_gen_server_url = if cliargs.input_gen == crate::cliargs::InputGen::Server {
            env::var("INPUT_GEN_SERVER_URL").expect("INPUT_GEN_SERVER_URL must be set")
        } else {
            "".to_string()
        };

        let proving_block = Arc::new(Mutex::new(None));
        let next_proving_block = Arc::new(Mutex::new(None));
        let current_job_id = Arc::new(Mutex::new(String::new()));
        let queued_start = Arc::new(Mutex::new(Instant::now()));
        let webhook_port = env::var("WEBHOOK_PORT")
            .unwrap_or(DEFAULT_WEBHOOK_PORT.to_string())
            .parse()
            .unwrap_or(DEFAULT_WEBHOOK_PORT);
        let metrics_port = env::var("METRICS_PORT")
            .unwrap_or(DEFAULT_METRICS_PORT.to_string())
            .parse()
            .unwrap_or(DEFAULT_METRICS_PORT);

        let block_modulus = match env::var("BLOCK_MODULUS") {
            Ok(modulus_str) => match modulus_str.parse::<u64>() {
                Ok(modulus) => modulus,
                Err(_) => panic!("BLOCK_MODULUS is not a valid u64"),
            },
            Err(_) => 1, // Default to 1 if not set
        };

        let rpc_ws_url = if cliargs.input_gen == crate::cliargs::InputGen::Local {
            match env::var("RPC_WS_URL") {
                Ok(url) => url,
                Err(_) => panic!("RPC_WS_URL must be set for local input generation"),
            }
        } else {
            "".to_string()
        };

        let compute_capacity = match env::var("COMPUTE_CAPACITY") {
            Ok(capacity_str) => match capacity_str.parse::<u32>() {
                Ok(capacity) => capacity,
                Err(_) => panic!("COMPUTE_CAPACITY is not a valid value"),
            },
            Err(_) => panic!("COMPUTE_CAPACITY not set"),
        };

        let db_dsn = if cliargs.insert_db {
            match env::var("DB_DSN") {
                Ok(dsn) => Some(dsn),
                Err(_) => panic!("DB_DSN not set"),
            }
        } else {
            None
        };

        let db_block_proofs = if let Some(dsn) = &db_dsn {
            Some(
                db::DbBlockProofs::new(dsn, db::DbBlockProofsConfig::default())
                    .await
                    .expect("Failed to create DbBlockProofs"),
            )
        } else {
            None
        };

        let fired_alerts = Arc::new(Mutex::new(FiredAlerts::default()));

        Ok(Self {
            cliargs,
            proving_block,
            next_proving_block,
            current_job_id,
            ethproofs_client,
            ethproofs_cluster_id,
            coordinator_channel,
            block_modulus,
            rpc_ws_url,
            webhook_port,
            metrics_port,
            inputs_folder,
            input_gen_server_url,
            compute_capacity,
            db_block_proofs,
            fired_alerts,
            queued_start,
            shared_metrics: crate::SharedMetrics::default(),
        })
    }

    pub fn delete_input_file(&self, filename: &String) {
        if !self.cliargs.keep_input {
            let input_file_path = format!("{}/{}", &self.inputs_folder, filename);
            if let Err(e) = std::fs::remove_file(&input_file_path) {
                warn!("Failed to remove input file {}, error: {}", input_file_path, e);
            }
        }
    }

    pub fn skipped_alert(&self) -> bool {
        self.fired_alerts.lock().unwrap().skipped
    }

    pub fn set_skipped_alert(&self, value: bool) {
        self.fired_alerts.lock().unwrap().skipped = value;
    }

    pub fn failed_alert(&self) -> bool {
        self.fired_alerts.lock().unwrap().failed
    }

    pub fn set_failed_alert(&self, value: bool) {
        self.fired_alerts.lock().unwrap().failed = value;
    }
}
