use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use ethproofs_common::protocol::BlockInfo;
use guest_reth::RethInputWitness;
use log::{error, info, warn};
use guest_reth::RethInputPublic;

#[cfg(zisk_hints)]
use guest_reth::{get_chain_spec, validate_block_stateless, verify_signatures};

#[cfg(zisk_hints)]
use tokio::sync::oneshot;

use zisk_sdk::ZiskStdin;
#[cfg(zisk_hints)]
use ziskos::hints::{close_hints, init_hints_file, init_hints_socket};

use crate::cliargs::TelegramEvent;
use crate::metrics::BlockMetrics;
use crate::prove::generate_proof;
use crate::state::AppState;
use crate::state::ZiskStdinWrapper;
use crate::telegram::{send_telegram_alert, AlertType};

#[cfg(zisk_hints)]
#[inline(always)]
pub async fn launch_hints_generation(
    block_info: &BlockInfo,
    app_state: &AppState,
) ->  tokio::task::JoinHandle<()> {
    let block_number = block_info.block_number;
    let app_state_clone = app_state.clone();
    let (ready_tx, ready_rx) = tokio::sync::oneshot::channel::<()>();

    let permit_reth =
        app_state_clone.calling_reth.clone().acquire_owned().await.expect("Semaphore closed");

    let handle = tokio::task::spawn_blocking(move || {
        let _permit_reth = permit_reth;

        generate_hints(block_number, app_state_clone, Some(ready_tx));
    });

    // Wait hint socket to be ready before proceeding to generate proof, so that we can ensure zisk-coordinator can connect to de socket
    let _ = ready_rx.await;

    handle
}

#[cfg(zisk_hints)]
pub fn generate_hints(
    block_number: u64,
    app_state: AppState,
    ready: Option<oneshot::Sender<()>>,
) {
    // Execute the block to get precompile hints populated
    info!("Generating hints for block {}", block_number);

    let start_hints = Instant::now();

    let hints_init_result = match app_state.cliargs.hints {
        crate::cliargs::Hints::Socket => {
            #[cfg(zisk_hints)]
            let hint_debug_file = app_state.cliargs.hints_debug.then(|| {
                PathBuf::from(format!(
                    "{}/{}_hints_debug.bin",
                    app_state.cliargs.hints_debug_path, block_number
                ))
            });

            #[cfg(not(zisk_hints))]
            let hint_debug_file: Option<PathBuf> = None;

            init_hints_socket(
                PathBuf::from(&app_state.cliargs.hints_socket),
                hint_debug_file,
                None,
                ready,
            )
        }
        crate::cliargs::Hints::File => {
            // Create ./hints directory if it doesn't exist
            let hints_dir = std::path::PathBuf::from("./hints");
            if !hints_dir.exists() {
                std::fs::create_dir_all(&hints_dir).expect("Failed to create hints directory");
            }
            init_hints_file(
                PathBuf::from(format!("{}/{}_hints.bin", hints_dir.display(), block_number)),
                ready,
            )
        }
    };

    if let Err(e) = hints_init_result {
        error!("Failed to init hints for block {}, error: {}", block_number, e);
        return;
    }

    let start_execution = Instant::now();

    let input_pk: RethInputPublic = {
        let zisk_stdin_shared = Arc::clone(&app_state.zisk_stdin);
        let mut zisk_stdin_lock = zisk_stdin_shared.lock().unwrap();
        let zisk_stdin: ZiskStdinWrapper = zisk_stdin_lock.as_mut().unwrap().clone();
        match zisk_stdin.read() {
            Ok(input) => input,
            Err(e) => {
                error!("Failed to read public keys input for block {} from zisk_stdin, error: {}", block_number, e);
                return;
            }
        }
    };

    // Get chain config
    let chain_config = input_pk.chain_config().clone();

    // Verify signatures
    let chain_spec = get_chain_spec(&chain_config);
    let block = input_pk.block().clone();
    let recoverd_block = match verify_signatures(block, chain_spec.clone(), input_pk.public_keys) {
        Ok(b) => b,
        Err(e) => {
            error!("Signature verification failed for block {}, error: {}", block_number, e);
            return;
        }
    };

    // // Wait for the signal that witness input is ready
    // match app_state.zisk_stdin_ready.clone() {
    //     Some(sem) => tokio::task::block_in_place(|| {
    //         let _permit = tokio::runtime::Handle::current().block_on(async {
    //             sem.acquire_owned().await.expect("semaphore closed")
    //         });
    //     }),
    //     None => {
    //         error!("zisk_stdin_ready semaphore is not initialized for block {}", block_number);
    //         return;
    //     }
    // }

    let input_witness: RethInputWitness = {
        let zisk_stdin_shared = Arc::clone(&app_state.zisk_stdin);
        let mut zisk_stdin_lock = zisk_stdin_shared.lock().unwrap();
        let zisk_stdin: ZiskStdinWrapper = zisk_stdin_lock.as_mut().unwrap().clone();
        match zisk_stdin.read() {
            Ok(input) => input,
            Err(e) => {
                error!("Failed to read witness input for block {} from zisk_stdin, error: {}", block_number, e);
                return;
            }
        }
    };

    let execution_witness = input_witness.witness().clone();
    if let Err(e) = validate_block_stateless(recoverd_block, execution_witness, chain_spec) {
        error!("Stateless validation failed for block {}, error: {}", block_number, e);
        return;
    }

    info!("Block {} validation done in {} ms", block_number, start_execution.elapsed().as_millis());

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

pub(crate) async fn process_input(block_info: BlockInfo, input_pk: &RethInputPublic, input_witness: &RethInputWitness, app_state: &mut AppState) {
    let input_file_path = PathBuf::from(&app_state.inputs_folder).join(block_info.filename());
    let block_number = block_info.block_number;

    let zisk_stdin = ZiskStdin::new();
    zisk_stdin.write(input_pk);
    zisk_stdin.write(input_witness);

    // let zisk_input_ready = Arc::new(tokio::sync::Semaphore::new(0));
    // app_state.zisk_stdin_ready = Some(zisk_input_ready.clone());

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

    if app_state.cliargs.skip_proving {
        info!("Skipping proving for block {} as per configuration", block_number);
    } else {
        let proving_block_shared_clone = Arc::clone(&app_state.proving_block);
        let proving_block = proving_block_shared_clone.lock().unwrap();
        if proving_block.is_some() {
            warn!("⚠️ Already proving block, saving next block {}", block_number);

            // Save input file
            if let Err(e) = zisk_stdin.save(&input_file_path) {
                error!("Failed to save input to file {} for block {}, error: {}", input_file_path.display(), block_number, e);
                return;
            }

            // Set next proving block to this block
            let next_proving_block_shared_clone = Arc::clone(&app_state.next_proving_block);
            let mut next_proving_block = next_proving_block_shared_clone.lock().unwrap();
            *next_proving_block = Some(block_info);

            // Check if skipped blocks exceed threshold and send Telegram alert if enabled
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
    }


    // Save input file if -i flag is set
    if app_state.cliargs.keep_input {
        if let Err(e) = zisk_stdin.save(&input_file_path) {
            error!("Failed to save input to file {} for block {}, error: {}", input_file_path.display(), block_number, e);
        }
    }

    // Write input to zisk_stdin wrapper for hint generation and proof generation
    {
        let zisk_stdin_wrapper = ZiskStdinWrapper::from_zisk_stdin(zisk_stdin);

        let zisk_stdin_shared = Arc::clone(&app_state.zisk_stdin);
        let mut zisk_stdin_lock = zisk_stdin_shared.lock().unwrap();
        *zisk_stdin_lock = Some(zisk_stdin_wrapper);
    }

    #[cfg(zisk_hints)]
    {
        let handle = launch_hints_generation(&block_info, app_state).await;

        // match app_state.zisk_stdin_ready.as_ref() {
        //     Some(sem) => {
        //         sem.add_permits(1);
        //     },
        //     None => {
        //         error!("zisk_stdin_ready semaphore is not initialized for block {} when calling add_permits", block_number);
        //         return;
        //     }
        // }

        // If we are using file-based hints, we need to wait for the hint generation to finish before generating the proof, otherwise the proof generation will fail due to missing hints.
        // If we are using socket-based hints, we can generate the proof in parallel with hint generation, so we don't wait.
        if app_state.cliargs.hints == crate::cliargs::Hints::File {
            handle.await.ok();
        }
    }

    if app_state.cliargs.skip_proving {
        return;
    }

    let result = generate_proof(block_info.clone(), app_state.clone()).await;
    
    let proving_block_shared_clone = Arc::clone(&app_state.proving_block);
    let mut proving_block = proving_block_shared_clone.lock().unwrap();
    
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
        }
    }
}
