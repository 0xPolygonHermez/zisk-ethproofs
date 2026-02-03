use std::fs::File;
use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use log::{error, info, warn};

#[cfg(zisk_hints)]
use ziskos_hints::hints::{close_hints, init_hints_socket, init_hints_file};
#[cfg(zisk_hints)]
use zisk_common::io::{StreamWrite, UnixSocketStreamWriter};
#[cfg(zisk_hints)]
use zeth_core::{Input, EthEvmConfig, validate_block};
#[cfg(zisk_hints)]
use zeth_chainspec::MAINNET;

use crate::cliargs::TelegramEvent;
use crate::metrics::BlockMetrics;
use crate::prove::generate_proof;
use crate::state::AppState;
use crate::telegram::{send_telegram_alert, AlertType};
use ethproofs_common::protocol::BlockInfo;

#[cfg(zisk_hints)]
fn generate_hints(block_number: u64, content: &[u8], app_state: AppState) {
    // Execute the block to get precompile hints populated
    info!("Generating hints for block {}", block_number);

    let start_hints = Instant::now();

    // TODO: Move this code to init_hints function
    let hints_uri = &app_state.cliargs.hints_uri;
    let hints_init_result = if hints_uri.starts_with("unix://") {
        // Remove prefix "unix://" from hints_uri
        let socket_path = hints_uri.strip_prefix("unix://").unwrap();
        init_hints_socket(PathBuf::from(socket_path))
    } else {
        // Remove prefix "file://" from hints_uri
        let file_path = hints_uri.strip_prefix("file://").unwrap();
        init_hints_file(PathBuf::from(file_path))
    };

    if let Err(e) = hints_init_result {
        error!("Failed to init hints for block {}, error: {}", block_number, e);
        return;
    }

    #[cfg(feature = "zec-rsp")]
    {
        let start_exec = Instant::now();
        let input = bincode::deserialize::<EthClientExecutorInput>(&content).unwrap();
        let executor = EthClientExecutor::eth(
            Arc::new(
                (&input.genesis)
                    .try_into()
                    .expect("Failed to convert genesis block into the required type"),
            ),
            input.custom_beneficiary,
        );

        let header = match executor.execute(input) {
            Ok(h) => h,
            Err(e) => {
                error!("Failed to execute block {}, error: {}", block_number, e);
                return;
            }
        };
        // Calculate block hash
        let block_hash = header.hash_slow();
        info!("Executed block {} in {} ms, hash: {}", block_number, start_exec.elapsed().as_millis(), block_hash);
    }
    #[cfg(not(feature = "zec-rsp"))]
    {
        let start_exec = Instant::now();

        let input = bincode::deserialize::<Input>(&content).unwrap();
        let block_number = input.block.header.number;

        let evm_config = EthEvmConfig::new(MAINNET.clone());

        let block_hash = validate_block(input.clone(), evm_config).expect("Failed to validate block");
        info!("Executed block {} in {} ms, txs: {}, hash: {}", block_number, start_exec.elapsed().as_millis(), input.block.body.transactions.len(), block_hash);
    }

    if let Err(e) = close_hints() {
        error!("Failed to close precompile hints for block {}, error: {}", block_number, e);
        return;
    }
    info!("Precompiles for block {} generated in {} ms", block_number, start_hints.elapsed().as_millis());
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
        "Received and saved input for block {}, file: {}, time: {} ms, time-to-input: {} ms",
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
        let app_state_clone = app_state.clone();
        tokio::spawn(async move {
            generate_hints(block_number, content_clone.as_slice(), app_state_clone);
        });
    }

    // Sleep 50 ms to ensure hints generation starts before proving
    tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;

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
