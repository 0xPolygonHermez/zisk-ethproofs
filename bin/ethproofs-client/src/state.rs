use std::{
    env,
    sync::{Arc, Mutex},
    time::Instant,
};

use anyhow::{Context, Result};
use clap::Parser;
use log::warn;
// serde derive imports no longer needed after moving protocol types
use ethproofs_common::protocol::BlockInfo;
use serde::de::DeserializeOwned;
use tokio::sync::Semaphore;
use tonic::transport::Channel;
use zisk_sdk::ZiskStdin;

use crate::{
    api::EthProofsApi,
    cliargs::CliArgs,
    db::{self, DbBlockProofs},
    metrics::SharedMetrics,
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

#[cfg(zisk_hints)]
extern "C" {
    fn hint_input_data(input_data_ptr: *const u8, input_data_len: usize);
}

#[cfg(zisk_hints_debug)]
extern "C" {
    fn hint_log_c(msg: *const std::os::raw::c_char);
}

#[cfg(zisk_hints_debug)]
pub fn hint_log<S: AsRef<str>>(msg: S) {
    // On native we call external C function to log hints, since it controls if hints are paused or not
    #[cfg(not(all(target_os = "zkvm", target_vendor = "zisk")))]
    {
        use std::ffi::CString;

        if let Ok(c) = CString::new(msg.as_ref()) {
            unsafe { hint_log_c(c.as_ptr()) };
        }
    }
    // On zkvm/zisk, we can just print directly
    #[cfg(all(target_os = "zkvm", target_vendor = "zisk"))]
    {
        println!("{}", msg.as_ref());
    }
}

#[derive(Clone)]
pub struct ZiskStdinWrapper {
    pub stdin: ZiskStdin,
}

impl ZiskStdinWrapper {
    pub fn new() -> Self {
        Self {
            stdin: ZiskStdin::new(),
         }
    }

    pub fn read<T: DeserializeOwned>(&self) -> Result<T> {
        let input_bytes = self.stdin.read_bytes();

        #[cfg(zisk_hints)]
        unsafe {
            hint_input_data(input_bytes.as_ptr(), input_bytes.len());
        }

        #[cfg(zisk_hints_debug)]
        {
            let start_bytes = &input_bytes[..input_bytes.len().min(64)];
            let ellipsis = if input_bytes.len() > 64 { "..." } else { "" };
            hint_log(format!(
                "hint_input_data (input_data: {:x?}{} , input_data_len: {}",
                start_bytes,
                ellipsis,
                input_bytes.len()
            ));
        }

        bincode::deserialize(&input_bytes).context("Failed to deserialize input data")
    }

    pub fn write<T: serde::Serialize>(&self, data: &T) {
        self.stdin.write(data);
    }

    pub fn save(&self, path: &std::path::Path) -> Result<()> {
        self.stdin.save(path).context("Failed to save input data to file")
    }
}
#[derive(Clone)]
pub struct AppState {
    pub shared_metrics: SharedMetrics,
    pub cliargs: CliArgs,
    pub calling_reth: Arc<tokio::sync::Semaphore>,
    pub proving_block: Arc<Mutex<Option<BlockInfo>>>,
    pub next_proving_block: Arc<Mutex<Option<BlockInfo>>>,
    pub zisk_stdin: Arc<Mutex<Option<ZiskStdinWrapper>>>,
    pub zisk_stdin_ready: Option<Arc<Semaphore>>,
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

        let calling_reth = Arc::new(tokio::sync::Semaphore::new(1));
        let proving_block = Arc::new(Mutex::new(None));
        let next_proving_block = Arc::new(Mutex::new(None));
        let zisk_stdin = Arc::new(Mutex::new(None));
        let zisk_stdin_ready = None;
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

        #[cfg(zisk_hints)]
        if cliargs.hints.debug {
            std::fs::create_dir_all(&cliargs.hints.debug_path)
                .context("Failed to create hints debug directory")?;
        }

        Ok(Self {
            cliargs,
            calling_reth,
            proving_block,
            next_proving_block,
            zisk_stdin,
            zisk_stdin_ready,
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
            shared_metrics: SharedMetrics::default(),
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
