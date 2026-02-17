use std::fs::File;
use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use log::{error, info, warn};

#[cfg(zisk_hints)]
use ziskos::hints::{close_hints, init_hints_socket, init_hints_file};

use crate::cliargs::TelegramEvent;
use crate::metrics::BlockMetrics;
use crate::prove::generate_proof;
use crate::state::AppState;
use crate::telegram::{send_telegram_alert, AlertType};
use ethproofs_common::protocol::BlockInfo;

#[cfg(zisk_hints)]
use tokio::sync::oneshot;

#[cfg(zisk_hints)]
#[inline(always)]
pub async fn launch_hints_generation(block_info: &BlockInfo, content: Vec<u8>, app_state: &AppState) {
    let block_number = block_info.block_number;
    let app_state_clone = app_state.clone();
    let (ready_tx, ready_rx) = tokio::sync::oneshot::channel::<()>();

    #[cfg(feature = "hints-debug")]
    {
        use std::process::Command;

        std::env::remove_var("DEBUG_HINTS_REF");

        // Construimos el patrón con wildcard
        let input_pattern = format!(
            "/root/git/zisk-ethproofs/inputs/{}",
            block_info.filename()
        );

        let copy_cmd = format!(
            "cp {} /root/git/zisk-eth-client/bin/guest/build/input.bin",
            input_pattern
        );

        let status_cp = Command::new("bash")
            .arg("-c")
            .arg(&copy_cmd)
            .status()
            .expect("Failed to execute cp command");

        if !status_cp.success() {
            panic!("cp command failed with status {:?}", status_cp);
        }

        println!("Executing zec-reth");
        // Ejecutar zec-reth
        let _ = Command::new("./target/debug/zec-reth")
            .current_dir("/root/git/zisk-eth-client/bin/guest")
            .status()
            .expect("Failed to execute zec-reth");


        std::env::set_var("DEBUG_HINTS_REF", format!("/root/git/zisk-eth-client/bin/guest/hints/{}_hints.bin", block_number));
    }

    let permit = app_state_clone
    .calling_reth
    .clone()
    .acquire_owned()
    .await
    .expect("Semaphore closed");

    let handle = tokio::task::spawn_blocking(move || {
        let _permit = permit;

        generate_hints(block_number, content.as_slice(), app_state_clone, Some(ready_tx));
    });

    let _ = ready_rx.await;

    if app_state.cliargs.hints == crate::cliargs::Hints::File {
        handle.await.ok();
    }
}

#[cfg(zisk_hints)]
pub fn generate_hints(block_number: u64, content: &[u8], app_state: AppState, ready: Option<oneshot::Sender<()>>) {
    // Execute the block to get precompile hints populated

    use stateless_validator_reth::guest::StatelessValidatorRethInput;

    info!("Generating hints for block {}", block_number);

    let start_hints = Instant::now();

    let hints_init_result = match app_state.cliargs.hints {
        crate::cliargs::Hints::Socket => {
            let hint_debug_file = if app_state.cliargs.hints_debug {
                Some(PathBuf::from(format!("{}/{}_hints_debug.bin", app_state.cliargs.hints_debug_path, block_number)))
            } else {
                None
            };
            let res = init_hints_socket(PathBuf::from(&app_state.cliargs.hints_socket), hint_debug_file, ready);
            res
        }
        crate::cliargs::Hints::File => {
            // Create ./hints directory if it doesn't exist
            let hints_dir = std::path::PathBuf::from("./hints");
            if !hints_dir.exists() {
                std::fs::create_dir_all(&hints_dir).expect("Failed to create hints directory");
            }
            init_hints_file(PathBuf::from(format!("{}/{}_hints.bin", hints_dir.display(), block_number)), ready)
        }
    };

    let start_execution = Instant::now();

    if let Err(e) = hints_init_result {
        error!("Failed to init hints for block {}, error: {}", block_number, e);
        return;
    }

    let reth_input: StatelessValidatorRethInput = bincode::deserialize(content).unwrap_or_else(|e| {
        panic!(
            "Failed to deserialize input for block {}, error: {}",
            block_number, e
        )
    });

    if let Err(e) = guest::validate_block(reth_input) {
        error!("Failed to execute block {} for hints generation, error: {}", block_number, e);
        return;
    }

    info!("Block {} execution done in {} ms", block_number, start_execution.elapsed().as_millis());

    if let Err(e) = close_hints() {
        error!("Failed to close hints for block {}, error: {}", block_number, e);
        return;
    }
    info!("Hints for block {} generated in {} ms", block_number, start_hints.elapsed().as_millis());
}

pub(crate) fn process_queued(block_number: u64, app_state: &AppState) {
    {
        let mut queued_start = app_state.queued_start.lock().unwrap();
        *queued_start = Instant::now();
    }

    info!("Received queued command for block {}", block_number);

    if let Some(client) = app_state.ethproofs_client.clone() {
        let cluster_id = app_state.ethproofs_cluster_id.unwrap();
        tokio::spawn(async move {
            let start = std::time::Instant::now();
            match client.proof_queued(cluster_id, block_number).await {
                Ok(_) => {
                    info!(
                        "Reported queued state to EthProofs for block {}, request_time: {} ms",
                        block_number,
                        start.elapsed().as_millis()
                    );
                }
                Err(e) => {
                    error!(
                        "Failed to report queued state to EthProofs for block {}, error: {}",
                        block_number, e
                    );
                }
            }
        });
    }
}

pub(crate) async fn process_input(block_info: BlockInfo, content: &[u8], app_state: &AppState) {
    let filename = block_info.filename();
    let block_number = block_info.block_number;
    let filepath = PathBuf::from(&app_state.inputs_folder).join(&filename);

    // Save the input to a file.
    let mut file = if let Ok(f) = File::create(filepath) {
        f
    } else {
        error!("Cannot create the file {} for block {}", &filename, &block_number);
        return;
    };
    if let Err(e) = file.write_all(content) {
        error!("Failed to write to file {} for block {}, error: {}", &filename, &block_number, e);
        return;
    }
    if let Err(e) = file.flush() {
        error!(
            "Failed to flush file buffer to OS for file {} for block {}, error: {}",
            &filename, &block_number, e
        );
        return;
    }
    if let Err(e) = file.sync_all() {
        error!(
            "Failed to sync file {} to disk for block {}, error: {}",
            &filename, &block_number, e
        );
        return;
    }

    let input_time = {
        let queued_start = app_state.queued_start.lock().unwrap();
        queued_start.elapsed().as_millis()
    };

    let block_timestamp_ms = block_info.timestamp.as_u64() as u128 * 1000;
    let time_to_input = match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(now) => now.as_millis() as u128 - block_timestamp_ms,
        Err(_) => 0,
    };

    if app_state.cliargs.enable_metrics {
        let mut metrics_map = app_state.shared_metrics.lock().await;
        metrics_map.insert(
            block_number,
            BlockMetrics {
                block_number,
                received_time_ms: input_time as i64,
                time_to_input_ms: time_to_input as i64,
                mgas: block_info.mgas,
                tx_count: block_info.tx_count as u64,
                timestamp: block_info.timestamp.as_u64() as i64,
                proving_time_ms: None,
                proving_cycles: None,
                submit_time_ms: None,
                success: false,
            },
        );
    }

    info!(
        "Input file saved for block {}, file: {}, time: {} ms, time-to-input: {} ms",
        block_number, filename, input_time, time_to_input
    );

    if app_state.cliargs.skip_proving {
        info!("Skipping proving for block {} as per configuration", block_number);
        app_state.delete_input_file(&filename);
        return;
    }

    let proving_block_shared_clone = Arc::clone(&app_state.proving_block);
    let mut proving_block = proving_block_shared_clone.lock().unwrap();
    if proving_block.is_some() {
        warn!("⚠️ Already proving block, saving next block {}", block_number);
        let next_proving_block_shared_clone = Arc::clone(&app_state.next_proving_block);
        let mut next_proving_block = next_proving_block_shared_clone.lock().unwrap();
        *next_proving_block = Some(block_info);

        let proving_block_number = proving_block.clone().unwrap().clone().block_number;
        if block_number - proving_block_number > app_state.cliargs.skipped_threshold as u64 {
            let msg_alert = format!(
                "Skipped {} consecutive blocks. Currently proving block {}, next queued block is {}.",
                block_number - proving_block_number - 1,
                proving_block_number,
                block_number
            );
            warn!("{}", msg_alert);
            let mut alert_handle = None;
            if app_state.cliargs.telegram_enabled(TelegramEvent::SkippedThreshold)
                && !app_state.skipped_alert()
            {
                app_state.set_skipped_alert(true);
                let msg_alert_clone = msg_alert.clone();
                alert_handle = Some(tokio::spawn(async move {
                    if let Err(e) = send_telegram_alert(&msg_alert_clone, AlertType::Warning).await
                    {
                        warn!("Failed to send Telegram alert: {}, error: {}", msg_alert_clone, e);
                    }
                }));
            }
            if app_state.cliargs.panic_on_skipped {
                if let Some(handle) = alert_handle {
                    handle.await.ok();
                }
                panic!("Skipped blocks exceeded threshold, panicking as per configuration");
            }
        } else if app_state.cliargs.telegram_enabled(TelegramEvent::SkippedThreshold)
            && app_state.skipped_alert()
        {
            app_state.set_skipped_alert(false);
            tokio::spawn(async move {
                let msg_alert =
                    format!("Resumed proving. Now proving block {}.", proving_block_number);
                if let Err(e) = send_telegram_alert(&msg_alert, AlertType::Info).await {
                    warn!("Failed to send Telegram alert: {}, error: {}", msg_alert, e);
                }
            });
        }
        return;
    }

    #[cfg(zisk_hints)]
    {
        let content_clone = content.to_vec();
        launch_hints_generation(&block_info, content_clone, app_state).await;
    }

    let result = generate_proof(block_info.clone(), app_state.clone()).await;
    match result {
        Ok(job_id) => {
            *proving_block = Some(block_info.clone());
            let current_job_id_shared_clone = Arc::clone(&app_state.current_job_id);
            let mut current_job_id = current_job_id_shared_clone.lock().unwrap();
            *current_job_id = job_id;

            if app_state.cliargs.telegram_enabled(TelegramEvent::ProofFailed) {
                if app_state.failed_alert() {
                    app_state.set_failed_alert(false);
                    let msg_alert =
                        format!("Resumed proving. Now proving block {}.", block_info.block_number);
                    tokio::spawn(async move {
                        if let Err(e) = send_telegram_alert(&msg_alert, AlertType::Info).await {
                            warn!("Failed to send Telegram alert: {}, error: {}", msg_alert, e);
                        }
                    });
                }
            }
        }
        Err(e) => {
            let msg_alert =
                format!("Proof generation failed for block {}, error: {}", block_number, e);
            error!("❌ {}", &msg_alert);
            if app_state.cliargs.telegram_enabled(TelegramEvent::ProofFailed) {
                if !app_state.failed_alert() {
                    app_state.set_failed_alert(true);
                    tokio::spawn(async move {
                        if let Err(e) = send_telegram_alert(&msg_alert, AlertType::Error).await {
                            warn!("Failed to send Telegram alert: {}, error: {}", msg_alert, e);
                        }
                    });
                }
            }

            app_state.delete_input_file(&filename);
        }
    }
}
