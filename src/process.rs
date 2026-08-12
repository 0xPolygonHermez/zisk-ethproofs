use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use log::{error, info, warn};

#[cfg(zisk_hints)]
use input::{create_client, generate_hints_to_file, generate_hints_to_socket};
#[cfg(zisk_hints)]
use tokio::sync::oneshot;

use zisk_sdk::ZiskStdin;

use crate::metrics::BlockMetrics;
use crate::prove::generate_proof;
use crate::state::AppState;
use crate::state::BlockInfo;
use crate::telegram::{
    send_proof_failed_alert, send_proof_resumed_alert, send_skipped_resumed_alert,
    send_skipped_threshold_alert,
};

#[cfg(zisk_hints)]
#[inline(always)]
pub async fn launch_hints_generation(
    block_info: &BlockInfo,
    app_state: &AppState,
) -> tokio::task::JoinHandle<()> {
    let block_number = block_info.block_number;
    let app_state_clone = app_state.clone();
    let (ready_tx, ready_rx) = tokio::sync::oneshot::channel::<()>();

    let permit =
        app_state_clone.calling_client.clone().acquire_owned().await.expect("Semaphore closed");

    let handle = tokio::task::spawn_blocking(move || {
        let _permit = permit;

        generate_hints(block_number, app_state_clone, Some(ready_tx));
    });

    // Wait hint socket to be ready before proceeding to generate proof, so that we can ensure zisk-coordinator can connect to de socket
    let _ = ready_rx.await;

    handle
}

#[cfg(zisk_hints)]
pub fn generate_hints(block_number: u64, app_state: AppState, ready: Option<oneshot::Sender<()>>) {
    info!("Generating hints for block {}", block_number);
    let start_hints = Instant::now();

    let zisk_stdin = {
        let lock = app_state.zisk_stdin.lock().unwrap();
        match lock.as_ref() {
            Some(stdin) => stdin.clone(),
            None => {
                error!("ZiskStdin not available for block {} hint generation", block_number);
                return;
            }
        }
    };

    let client = create_client(app_state.cliargs.client);

    let debug_file = app_state.cliargs.hints.debug.then(|| {
        PathBuf::from(format!(
            "{}/{}_hints_debug.bin",
            app_state.cliargs.hints.debug_folder, block_number
        ))
    });

    let result = match app_state.cliargs.hints.mode {
        crate::cliargs::HintsMode::Socket => {
            info!(
                "Streaming hints over socket for block {}, socket: {}",
                block_number, app_state.cliargs.hints.socket
            );
            generate_hints_to_socket(
                &zisk_stdin,
                PathBuf::from(&app_state.cliargs.hints.socket),
                debug_file,
                None,
                ready,
                client.as_ref(),
            )
        }
        crate::cliargs::HintsMode::File => {
            // File mode does not wait for a prover; signal readiness up front.
            if let Some(tx) = ready {
                let _ = tx.send(());
            }
            let hints_dir = std::path::PathBuf::from("./hints");
            if !hints_dir.exists() {
                if let Err(e) = std::fs::create_dir_all(&hints_dir) {
                    error!("Failed to create hints directory for block {}: {}", block_number, e);
                    return;
                }
            }
            generate_hints_to_file(
                &zisk_stdin,
                hints_dir.join(format!("{}_hints.bin", block_number)),
                client.as_ref(),
            )
        }
    };

    match result {
        Ok(_) => info!(
            "Hints for block {} generated in {} ms",
            block_number,
            start_hints.elapsed().as_millis()
        ),
        Err(e) => error!("Hint generation failed for block {}: {}", block_number, e),
    }
}

pub(crate) fn process_queued(block_number: u64, app_state: &AppState) {
    {
        let mut queued_start = app_state.queued_start.lock().unwrap();
        *queued_start = Instant::now();
    }

    info!("Received queued command for block {}", block_number);

    if let Some(client) = &app_state.ethproofs_client {
        let cluster_id = app_state.cliargs.ethproofs.cluster_id.unwrap();
        client.proof_queued(cluster_id, block_number);
    }
}

pub(crate) async fn process_input(
    block_info: BlockInfo,
    zisk_stdin: ZiskStdin,
    app_state: &mut AppState,
) {
    let input_file_path =
        PathBuf::from(&app_state.cliargs.inputs.folder).join(block_info.filename());
    let block_number = block_info.block_number;

    let input_time = {
        let queued_start = app_state.queued_start.lock().unwrap();
        queued_start.elapsed().as_millis()
    };

    let block_timestamp_ms = block_info.timestamp.as_u64() as u128 * 1000;
    let time_to_input = match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(now) => now.as_millis() as u128 - block_timestamp_ms,
        Err(_) => 0,
    };

    // info!(
    //     "Input generated for block {}, time: {} ms, time-to-input: {} ms",
    //     block_number, input_time, time_to_input
    // );

    if app_state.cliargs.metrics.enabled {
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
                success: false,
            },
        );
    }

    if app_state.cliargs.skip_proving {
        info!("Skipping proving for block {} as per configuration", block_number);
        return;
    }

    let proving_block_shared_clone = Arc::clone(&app_state.proving_block);
    let mut proving_block = proving_block_shared_clone.lock().unwrap();
    if proving_block.is_some() {
        warn!("⚠️ Already proving block, saving next block {}", block_number);

        // Save input file
        if let Err(e) = zisk_stdin.save(&input_file_path) {
            error!(
                "Failed to save input to file {} for block {}, error: {}",
                input_file_path.display(),
                block_number,
                e
            );
            return;
        }

        // Set next proving block to this block
        let next_proving_block_shared_clone = Arc::clone(&app_state.next_proving_block);
        let mut next_proving_block = next_proving_block_shared_clone.lock().unwrap();
        *next_proving_block = Some(block_info);

        // Check if skipped blocks exceed threshold and send Telegram alert if enabled
        let proving_block_number = proving_block.clone().unwrap().clone().block_number;
        if block_number - proving_block_number > app_state.cliargs.skipped.threshold as u64 {
            let skipped_count = block_number - proving_block_number - 1;
            warn!(
                "Skipped {} consecutive blocks. Currently proving block {}, next queued block is {}.",
                skipped_count, proving_block_number, block_number
            );

            // Run blocks-skipped hook if configured
            if let Some(script) = &app_state.cliargs.hooks.blocks_skipped {
                let job_id = app_state
                    .current_job_id
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .clone();
                crate::hooks::run_hook(
                    script,
                    vec![proving_block_number.to_string(), job_id],
                );
            }

            let mut alert_handle = None;
            if !app_state.skipped_alert() {
                alert_handle = send_skipped_threshold_alert(
                    app_state,
                    proving_block_number,
                    block_number,
                    skipped_count,
                );
                if alert_handle.is_some() {
                    app_state.set_skipped_alert(true);
                }
            }
            if app_state.cliargs.skipped.panic {
                if let Some(handle) = alert_handle {
                    handle.await.ok();
                }
                panic!("Skipped blocks exceeded threshold, panicking as per configuration");
            }
        } else if app_state.skipped_alert() {
            app_state.set_skipped_alert(false);
            send_skipped_resumed_alert(app_state, proving_block_number);
        }
        return;
    }

    // Save input file if input.keep flag is set
    if app_state.cliargs.inputs.keep {
        if let Err(e) = zisk_stdin.save(&input_file_path) {
            error!(
                "Failed to save input to file {} for block {}, error: {}",
                input_file_path.display(),
                block_number,
                e
            );
        }
    }

    // Store input for hint generation and proof generation
    {
        let zisk_stdin_shared = Arc::clone(&app_state.zisk_stdin);
        let mut zisk_stdin_lock = zisk_stdin_shared.lock().unwrap();
        *zisk_stdin_lock = Some(zisk_stdin);
    }

    // #[cfg(zisk_hints)]
    // {
    //     let handle = launch_hints_generation(&block_info, app_state).await;

    //     // match app_state.zisk_stdin_ready.as_ref() {
    //     //     Some(sem) => {
    //     //         sem.add_permits(1);
    //     //     },
    //     //     None => {
    //     //         error!("zisk_stdin_ready semaphore is not initialized for block {} when calling add_permits", block_number);
    //     //         return;
    //     //     }
    //     // }

    //     // If we are using file-based hints, we need to wait for the hint generation to finish before generating the proof, otherwise the proof generation will fail due to missing hints.
    //     // If we are using socket-based hints, we can generate the proof in parallel with hint generation, so we don't wait.
    //     if app_state.cliargs.hints.mode == crate::cliargs::HintsMode::File {
    //         handle.await.ok();
    //     }
    // }

    let result = generate_proof(block_info.clone(), app_state.clone()).await;
    match result {
        Ok(job_id) => {
            *proving_block = Some(block_info.clone());
            let current_job_id_shared_clone = Arc::clone(&app_state.current_job_id);
            let mut current_job_id = current_job_id_shared_clone.lock().unwrap();
            *current_job_id = job_id;

            if app_state.failed_alert() {
                app_state.set_failed_alert(false);
                send_proof_resumed_alert(app_state, block_info.block_number);
            }
        }
        Err(e) => {
            let msg_alert =
                format!("Proof generation failed for block {}, error: {}", block_number, e);
            error!("❌ {}", &msg_alert);

            // Run proof-failed hook if configured
            if let Some(script) = &app_state.cliargs.hooks.proof_failed {
                let job_id = app_state
                    .current_job_id
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .clone();
                crate::hooks::run_hook(
                    script,
                    vec![block_number.to_string(), job_id],
                );
            }

            if !app_state.failed_alert() {
                if send_proof_failed_alert(app_state, msg_alert).is_some() {
                    app_state.set_failed_alert(true);
                }
            }
        }
    }
}
