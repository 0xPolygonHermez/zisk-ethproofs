use std::{
    fs::File,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex,
    },
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use log::{info, warn};
// serde derive imports no longer needed after moving protocol types
use ethproofs_common::protocol::BlockInfo;
use serde::de::DeserializeOwned;
//use tokio::sync::Semaphore;
use zisk_sdk::{GuestProgram, ProverClient, RemoteClient, ZiskStdin};

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

#[allow(dead_code)]
#[derive(Clone)]
// Wrapper around ZiskStdin to provide hint generation support when reading
pub struct ZiskStdinWrapper {
    pub stdin: ZiskStdin,
}

#[allow(dead_code)]
impl ZiskStdinWrapper {
    pub fn new() -> Self {
        Self { stdin: ZiskStdin::new() }
    }

    pub fn from_zisk_stdin(zisk_stdin: ZiskStdin) -> Self {
        Self { stdin: zisk_stdin }
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

        bincode::serde::decode_from_slice(&input_bytes, bincode::config::standard())
            .map(|(v, _)| v)
            .context("Failed to deserialize input data")
    }

    pub fn write<T: serde::Serialize>(&self, data: &T) {
        self.stdin.write(data);
    }
}
#[derive(Clone)]
pub struct AppState {
    pub shared_metrics: crate::SharedMetrics,
    pub cliargs: CliArgs,
    pub calling_reth: Arc<tokio::sync::Semaphore>,
    pub proving_block: Arc<Mutex<Option<BlockInfo>>>,
    pub next_proving_block: Arc<Mutex<Option<BlockInfo>>>,
    pub zisk_stdin: Arc<Mutex<Option<ZiskStdinWrapper>>>,
    pub prover_client: Arc<Mutex<RemoteClient>>,
    pub guest_program: Arc<Mutex<GuestProgram>>,
    // pub zisk_stdin_ready: Option<Arc<Semaphore>>,
    pub current_job_id: Arc<Mutex<String>>,
    pub queued_start: Arc<Mutex<Instant>>,
    pub ethproofs_client: Option<EthProofsApi>,
    pub db_block_proofs: Option<DbBlockProofs>,
    pub fired_alerts: Arc<Mutex<FiredAlerts>>,
    pub proof_done_signal: Arc<tokio::sync::Notify>,
    pub run_deadline: Option<Instant>,
    pub proved_blocks: Arc<AtomicU64>,
    pub proved_csv: Option<Arc<Mutex<File>>>,
}

impl AppState {
    pub async fn new(cliargs: CliArgs) -> anyhow::Result<Self> {
        info!("Starting EthProofs client with config");

        let ethproofs_client = if cliargs.ethproofs.submit {
            let api_url = cliargs
                .ethproofs
                .api_url
                .clone()
                .expect("ethproofs.api-url required when ethproofs.submit is enabled");
            let api_token = cliargs
                .ethproofs
                .api_token
                .clone()
                .expect("ethproofs.api-token required when ethproofs.submit is enabled");

            Some(EthProofsApi::new(api_url, api_token))
        } else {
            None
        };

        let calling_reth = Arc::new(tokio::sync::Semaphore::new(1));
        let proving_block = Arc::new(Mutex::new(None));
        let next_proving_block = Arc::new(Mutex::new(None));
        let zisk_stdin = Arc::new(Mutex::new(None));

        // Initialize the Zisk prover client
        let guest_path = std::fs::canonicalize(&cliargs.guest)
            .with_context(|| format!("Failed to resolve guest ELF path: {}", cliargs.guest))?;
        let guest = GuestProgram::from_uri(&format!("file://{}", guest_path.display()))?;
        let client = ProverClient::remote(cliargs.coordinator_url.clone()).build()?;

        // Upload the guest program
        info!("Uploading guest program {} to coordinator...", guest_path.display());
        client.upload(&guest).run()?;

        // Perform guest program setup
        #[cfg(zisk_hints)]
        {
            info!("Performing guest program setup with hints...");
            client.setup(&guest).with_hints().run()?.await?;
        }
        #[cfg(not(zisk_hints))]
        {
            info!("Performing guest program setup...");
            client.setup(&guest).run()?.await?;
        }
        info!("Guest program setup complete");

        let prover_client = Arc::new(Mutex::new(client));
        let guest_program = Arc::new(Mutex::new(guest));

        //let zisk_stdin_ready = None;
        let current_job_id = Arc::new(Mutex::new(String::new()));
        let queued_start = Arc::new(Mutex::new(Instant::now()));

        let db_block_proofs = if cliargs.db.enabled {
            let dsn = cliargs.db.dsn.as_ref().expect("db.dsn required when db.enabled is set");
            Some(
                db::DbBlockProofs::new(dsn, db::DbBlockProofsConfig::default())
                    .await
                    .expect("Failed to create DbBlockProofs"),
            )
        } else {
            None
        };

        let fired_alerts = Arc::new(Mutex::new(FiredAlerts::default()));
        let proof_done_signal = Arc::new(tokio::sync::Notify::new());

        let proved_blocks = Arc::new(AtomicU64::new(0));

        // Open the proved-blocks CSV file (if configured) and write its header.
        let proved_csv = if let Some(path) = &cliargs.proof.csv {
            use std::io::Write;
            let mut file = File::create(path)
                .with_context(|| format!("Failed to create proof CSV file: {}", path))?;
            writeln!(file, "block_number,mgas,steps,proving_time_ms")
                .context("Failed to write header to proof CSV file")?;
            info!("Storing proved block info in CSV file: {}", path);
            Some(Arc::new(Mutex::new(file)))
        } else {
            None
        };

        let run_deadline = if cliargs.inputs.mode == crate::cliargs::InputGen::Rpc {
            cliargs.run_time.map(|minutes| {
                info!("Client will run for {} minute(s) before exiting", minutes);
                Instant::now() + Duration::from_secs(minutes * 60)
            })
        } else {
            None
        };

        #[cfg(zisk_hints)]
        if cliargs.hints.debug {
            std::fs::create_dir_all(&cliargs.hints.debug_folder)
                .context("Failed to create hints debug directory")?;
        }

        Ok(Self {
            cliargs,
            calling_reth,
            proving_block,
            next_proving_block,
            zisk_stdin,
            prover_client,
            guest_program,
            //zisk_stdin_ready,
            current_job_id,
            ethproofs_client,
            db_block_proofs,
            fired_alerts,
            proof_done_signal,
            run_deadline,
            proved_blocks,
            proved_csv,
            queued_start,
            shared_metrics: crate::SharedMetrics::default(),
        })
    }

    /// Record a successfully proved block: increment the proved-blocks counter and, if a
    /// '--proof.csv' file is configured, append a row with its info.
    pub fn record_proved_block(&self, block_number: u64, mgas: u64, steps: u64, proving_time_ms: u64) {
        self.proved_blocks.fetch_add(1, Ordering::SeqCst);

        if let Some(csv) = &self.proved_csv {
            use std::io::Write;
            let mut file = csv.lock().unwrap_or_else(|e| e.into_inner());
            if let Err(e) = writeln!(file, "{},{},{},{}", block_number, mgas, steps, proving_time_ms)
            {
                warn!("Failed to write block {} to proof CSV file, error: {}", block_number, e);
            }
        }
    }

    /// Log a summary with the number of blocks proved successfully.
    pub fn log_proved_blocks_summary(&self) {
        info!(
            "Proved {} block(s) successfully during this run",
            self.proved_blocks.load(Ordering::SeqCst)
        );
    }

    pub fn delete_input_file(&self, filename: &String) {
        if !self.cliargs.inputs.keep {
            let input_file_path = format!("{}/{}", &self.cliargs.inputs.folder, filename);
            if std::fs::exists(&input_file_path).unwrap_or(false) {
                if let Err(e) = std::fs::remove_file(&input_file_path) {
                    warn!("Failed to remove input file {}, error: {}", input_file_path, e);
                }
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
