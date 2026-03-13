use std::{
    sync::{Arc, Mutex},
    time::Instant,
};

use anyhow::{Context, Result};
use clap::Parser;
use ethers::types::U256;
use log::warn;
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use tokio::sync::Semaphore;
use tonic::transport::Channel;
use zisk_sdk::ZiskStdin;

use crate::{
    api::EthProofsApi,
    cliargs::CliArgs,
    db::{self, DbBlockProofs},
    metrics::SharedMetrics,
};

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

#[derive(Debug, Default)]
pub struct FiredAlerts {
    pub skipped: bool,
    pub failed: bool,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct BlockInfo {
    pub block_number: u64,
    pub timestamp: U256,
    pub block_hash: String,
    pub tx_count: usize,
    pub mgas: u64,
}

impl BlockInfo {
    pub fn short_hash(&self) -> String {
        self.block_hash.chars().take(6).collect()
    }
    pub fn filename(&self) -> String {
        format!("{}_{}.bin", self.block_number, self.short_hash())
    }
}

#[derive(Clone)]
pub struct ZiskStdinWrapper {
    pub stdin: ZiskStdin,
}

#[allow(dead_code)]
impl ZiskStdinWrapper {
    pub fn new() -> Self {
        Self { stdin: ZiskStdin::new() }
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
    pub coordinator_channel: Option<Channel>,
    pub db_block_proofs: Option<DbBlockProofs>,
    pub fired_alerts: Arc<Mutex<FiredAlerts>>,
}

impl AppState {
    pub async fn new() -> anyhow::Result<Self> {
        // Parse the command line arguments
        let cliargs = CliArgs::parse();

        let ethproofs_client = if cliargs.ethproofs.submit {
            let ethproofs_url = cliargs.ethproofs.api_url.clone().unwrap();
            let ethproofs_token = cliargs.ethproofs.api_token.clone().unwrap();

            Some(EthProofsApi::new(ethproofs_url, ethproofs_token))
        } else {
            None
        };

        let coordinator_channel = if !cliargs.skip_proving {
            let coordinator_channel = Channel::from_shared(cliargs.coordinator.url.clone())
                .context("Failed to create coordinator channel")?
                .connect()
                .await
                .with_context(|| {
                    format!("Failed to connect to coordinator at {}", cliargs.coordinator.url)
                })?;
            Some(coordinator_channel)
        } else {
            None
        };

        let calling_reth = Arc::new(tokio::sync::Semaphore::new(1));
        let proving_block = Arc::new(Mutex::new(None));
        let next_proving_block = Arc::new(Mutex::new(None));
        let zisk_stdin = Arc::new(Mutex::new(None));
        let zisk_stdin_ready = None;
        let current_job_id = Arc::new(Mutex::new(String::new()));
        let queued_start = Arc::new(Mutex::new(Instant::now()));

        let db_block_proofs = if cliargs.db.enabled {
            Some(
                db::DbBlockProofs::new(
                    &cliargs.db.dsn.clone().unwrap(),
                    db::DbBlockProofsConfig::default(),
                )
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
            coordinator_channel,
            db_block_proofs,
            fired_alerts,
            queued_start,
            shared_metrics: SharedMetrics::default(),
        })
    }

    pub fn delete_input_file(&self, filename: &String) {
        if !self.cliargs.inputs.keep {
            let input_file_path = format!("{}/{}", &self.cliargs.inputs.folder, filename);
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
