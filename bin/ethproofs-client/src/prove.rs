use std::{path::PathBuf, sync::Arc, time::Duration};

use anyhow::{anyhow, Result};
use base64::{engine::general_purpose, Engine};
use log::{debug, error, info, warn};
use zisk_sdk::ZiskStdin;
use zisk_sdk::{ExecutorKind, ProofKind};

use crate::state::ZiskStdinWrapper;
use crate::{
    cliargs::TelegramEvent,
    db::BlockProof,
    state::AppState,
    telegram::{send_telegram_alert, AlertType},
};
use ethproofs_common::protocol::BlockInfo;

pub fn get_proof_b64(proof_data: &[u8]) -> Result<String> {
    Ok(general_purpose::STANDARD.encode(proof_data))
}

pub async fn generate_proof(block_info: BlockInfo, state: AppState) -> Result<String> {
    let proved_block_number = block_info.block_number;

    let prover_client_clone = Arc::clone(&state.prover_client);
    let guest_program_clone = Arc::clone(&state.guest_program);
    let ethproofs_client = state.ethproofs_client.clone();
    let ethproofs_cluster_id = state.ethproofs_cluster_id;

    tokio::task::spawn_blocking(move || {
        let rt = tokio::runtime::Handle::current();
        rt.block_on(async {
            let prover_client = prover_client_clone.lock().unwrap();
            let guest_program = guest_program_clone.lock().unwrap();

            #[cfg(zisk_hints)]
            let handle = {
                use zisk_sdk::ZiskStream;

                use crate::process::launch_hints_generation;

                let hints_handle = launch_hints_generation(&block_info, &state).await;

                //TODO: Implement hints file handling
                if state.cliargs.hints == crate::cliargs::Hints::File {
                    hints_handle.await.ok();
                }

                let hints_stream = ZiskStream::unix_external(&state.cliargs.hints_socket);

                prover_client
                    .prove(&guest_program, ZiskStdin::new())
                    .hints(hints_stream)
                    .timeout(Duration::from_secs(state.cliargs.prove_timeout))
                    .executor(ExecutorKind::Assembly)
                    .wrap(ProofKind::VadcopFinalMinimal)
                    .run()
            };

            #[cfg(not(zisk_hints))]
            let handle = {
                let zisk_stdin_clone = Arc::clone(&state.zisk_stdin);

                let stdin = zisk_stdin_clone
                    .lock()
                    .unwrap()
                    .take()
                    .ok_or_else(|| anyhow!("ZiskStdin not available for block {}, when attempting to generate proof", proved_block_number))
                    .unwrap();

                prover_client
                    .prove(&guest_program, stdin.stdin)
                    .timeout(Duration::from_secs(state.cliargs.prove_timeout))
                    .executor(ExecutorKind::Assembly)
                    .wrap(ProofKind::VadcopFinalMinimal)
                    .run()

            };

            let prove_result = match handle {
                Ok(handle) => {
                    let job_id = handle.job_id().map(|id| id.to_string()).unwrap_or_else(|| "N/A".to_string());
                    info!("🔄 Generating proof for block {}, job_id: {}", proved_block_number, job_id);

                    handle.await
                }
                Err(e) => {
                    Err(anyhow!("Failed to start proof generation for block {}: {}", proved_block_number, e))
                }
            };

            if let Ok(result) = &prove_result {
                let proving_time_ms = result.get_proving_time();
                let proving_cycles = result.get_execution_steps();
                let job_id_str = result.job_id().map(|id| id.to_string()).unwrap_or_else(|| "N/A".to_string());

                info!(
                    "✅ Proof generated for block {}, proving_time: {} ms, cycles: {}, job: {}",
                    proved_block_number, proving_time_ms, proving_cycles, job_id_str
                );
            }

            // Get next_block_number in atomic scope
            let next_block = {
                let mut next_proving_block =
                    state.next_proving_block.lock().unwrap_or_else(|e| e.into_inner());

                if next_proving_block.is_some() {
                    let next = next_proving_block.clone().unwrap();
                    *next_proving_block = None;
                    Some(next)
                } else {
                    None
                }
            };

            // Check and start proof generation for next block if set
            if next_block.is_some() {
                // Set proving_block to next_block_number in atomic scope
                {
                    let mut proving_block = state.proving_block.lock().unwrap_or_else(|e| e.into_inner());
                    *proving_block = next_block.clone();
                }

                let next_block = next_block.unwrap();

                // Get input file and for next block to prove
                let input_filename =
                    format!("{}/{}", state.inputs_folder.clone(), next_block.filename());

                // Read input file into ZiskStdin
                let path = PathBuf::from(&input_filename);
                let zisk_stdin = match ZiskStdin::from_file(&path) {
                    Ok(stdin) => stdin,
                    Err(e) => {
                        error!("Error opening input file {}: {}", path.display(), e);
                        return;
                    }
                };

                // Wrap ZiskStdin in ZiskStdinWrapper
                let zisk_stdin_wrapper = ZiskStdinWrapper::from_zisk_stdin(zisk_stdin);

                // Store ZiskStdinWrapper in shared state for next proof generation
                {
                    let zisk_stdin_shared = Arc::clone(&state.zisk_stdin);
                    let mut zisk_stdin_lock = zisk_stdin_shared.lock().unwrap();
                    *zisk_stdin_lock = Some(zisk_stdin_wrapper);
                }

                // Start proof generation for next block without waiting for current block proof to complete
                let result = generate_proof(next_block.clone(), state.clone()).await;

                match result {
                    Ok(job_id) => {
                        // Store current job ID
                        let mut current_job_id =
                            state.current_job_id.lock().unwrap_or_else(|e| e.into_inner());
                        *current_job_id = job_id;
                    }
                    Err(e) => {
                        // If generation failed, reset proving_block in atomic scope
                        {
                            let mut proving_block =
                                state.proving_block.lock().unwrap_or_else(|e| e.into_inner());
                            *proving_block = None;
                        }

                        let next_block_number = next_block.block_number;

                        let msg = format!(
                            "Proof generation failed for next block {}, error: {}",
                            next_block_number, e
                        );
                        error!("❌ {}", msg);

                        if state.cliargs.telegram_enabled(TelegramEvent::ProofFailed) {
                            tokio::spawn(async move {
                                if let Err(e) = send_telegram_alert(&msg, AlertType::Error).await {
                                    warn!("Failed to send Telegram alert: {}, error: {}", msg, e);
                                }
                            });
                        }

                        // Clean up input file if not needed
                        state.delete_input_file(&next_block.filename());
                    }
                }
            } else {
                // Reset proving_block
                let mut proving_block = state.proving_block.lock().unwrap_or_else(|e| e.into_inner());
                *proving_block = None;

                // Notify folder input generation that the current proof cycle has completed.
                state.proof_done_signal.notify_waiters();
            }

            match prove_result {
                Ok(result) => {
                    // If proof generation was successful, proceed to process the block proof

                    let proving_time_ms = result.get_proving_time();
                    let proving_cycles = result.get_execution_steps();
                    let job_id_str = result.job_id().map(|id| id.to_string()).unwrap_or_else(|| "N/A".to_string());

                    // Encode compressed proof to base64
                    let proof_bytes = match result.get_proof_u64() {
                        Ok(bytes) => {
                            // Convert Vec<u64> to Vec<u8> (little-endian)
                            bytes.iter().flat_map(|x| x.to_le_bytes()).collect::<Vec<u8>>()
                        }
                        Err(e) => {
                            error!(
                                "❌ Failed to get proof bytes for block {}, error: {}",
                                proved_block_number, e
                            );
                            return;
                        }
                    };

                    // Save proof to disk if enabled
                    if state.cliargs.save_proof {
                        let proof_dir = PathBuf::from(&state.cliargs.save_proof_folder);
                        if let Err(e) = std::fs::create_dir_all(&proof_dir) {
                            error!(
                                "❌ Failed to create proof directory {} for block {}, error: {}",
                                proof_dir.display(), proved_block_number, e
                            );
                        } else {
                            let proof_path = proof_dir.join(format!("{}_proof.bin", proved_block_number));
                            match std::fs::write(&proof_path, proof_bytes.as_slice()) {
                                Ok(_) => info!(
                                    "Proof saved to {} for block {}",
                                    proof_path.display(), proved_block_number
                                ),
                                Err(e) => error!(
                                    "❌ Failed to save proof to {} for block {}, error: {}",
                                    proof_path.display(), proved_block_number, e
                                ),
                            }
                        }
                    }

                    let proof_base64 = match get_proof_b64(proof_bytes.as_slice()) {
                        Ok(b64) => b64,
                        Err(e) => {
                            error!(
                                "❌ Failed to get compressed proof in base64 for block {}, error: {}",
                                proved_block_number, e
                            );
                            return;
                        }
                    };

                    // Report proved state to EthProofs in a separate parallel task
                    if state.cliargs.submit_ethproofs {
                        let client = state.ethproofs_client.clone().unwrap();
                        let cluster_id = state.ethproofs_cluster_id.unwrap();
                        let proof_base64 = proof_base64.clone();
                        let job_id_str = job_id_str.clone();
                        tokio::spawn(async move {
                            let start = std::time::Instant::now();
                            match client
                                .proof_proved(
                                    cluster_id,
                                    proved_block_number,
                                    proving_time_ms as u128,
                                    proving_cycles,
                                    &proof_base64,
                                    job_id_str,
                                )
                                .await
                            {
                                Ok(_) => {
                                    info!(
                                        "Reported proved state to EthProofs for block {}, request_time: {} ms",
                                        proved_block_number,
                                        start.elapsed().as_millis()
                                    );
                                }
                                Err(e) => {
                                    error!(
                                        "❌ Failed to submit proof to EthProofs for block {}, error: {}",
                                        proved_block_number, e
                                    );
                                }
                            }
                        });
                    }

                    // Insert into DB if enabled
                    if state.cliargs.insert_db {
                        if let Some(db) = &state.db_block_proofs {
                            let start = std::time::Instant::now();
                            let block_proof = BlockProof {
                                block_number: proved_block_number,
                                zisk_version: "0.12.0".to_string(),
                                hardware: "128 vCPU, 512GB RAM, 1 RTX4090 GPU".to_string(),
                                proving_time: proving_time_ms as u32,
                                proof: proof_base64,
                                steps: proving_cycles,
                            };
                            match db.enqueue(block_proof).await {
                                Ok(_) => info!(
                                    "Proof inserted into DB for block {}, insert_time: {} ms",
                                    proved_block_number,
                                    start.elapsed().as_millis()
                                ),
                                Err(e) => error!(
                                    "❌ Failed to insert proof into DB for block {}, error: {}",
                                    proved_block_number, e
                                ),
                            }
                        } else {
                            warn!(
                                "DB handle not initialized, cannot insert proof for block {}",
                                proved_block_number
                            );
                        }
                    }

                    // Send Telegram alert if enabled
                    if state.cliargs.telegram_enabled(TelegramEvent::BlockProved) {
                        let msg = format!(
                            "Proof generated for block {}, proving_time: {}s, cycles: {}",
                            proved_block_number,
                            proving_time_ms / 1000,
                            proving_cycles,
                        );

                        if let Err(e) = send_telegram_alert(&msg, AlertType::Success).await {
                            warn!("Failed to send Telegram alert: {}, error: {}", msg, e);
                        }
                    }

                    // Update Prometheus metrics for proof generation if metrics enabled
                    if state.cliargs.enable_metrics {
                        let start = std::time::Instant::now();
                        // Update the shared HashMap and publish/remove metrics only when the block is complete
                        let mut shared_metrics = state.shared_metrics.lock().await;
                        let entry = shared_metrics.get_mut(&proved_block_number);
                        if let Some(metrics) = entry {
                            metrics.proving_time_ms = Some(proving_time_ms as i64);
                            metrics.proving_cycles = Some(proving_cycles as i64);
                            metrics.submit_time_ms = Some(0);
                            metrics.success = true;

                            // Publish all metrics for the current block
                            let previous_block = crate::metrics::LATEST_BLOCK_NUMBER.get() as u64;
                            let diff = if proved_block_number > previous_block && previous_block != 0 {
                                proved_block_number - previous_block - 1
                            } else {
                                0
                            };

                            crate::metrics::LATEST_BLOCK_NUMBER.set(metrics.block_number as i64);
                            crate::metrics::LATEST_RECEIVED_TIME_MS.set(metrics.received_time_ms);
                            crate::metrics::LATEST_TIME_TO_INPUT_MS.set(metrics.time_to_input_ms);
                            crate::metrics::LATEST_MGAS.set(metrics.mgas as i64);
                            crate::metrics::LATEST_TX_COUNT.set(metrics.tx_count as i64);
                            crate::metrics::LATEST_PROVING_TIME_MS.set(metrics.proving_time_ms.unwrap_or(0));
                            crate::metrics::LATEST_PROVING_CYCLES.set(metrics.proving_cycles.unwrap_or(0));

                            crate::metrics::LATEST_SUBMIT_TIME_MS.set(metrics.submit_time_ms.unwrap_or(0));
                            crate::metrics::LATEST_BLOCK_TIMESTAMP.set(metrics.timestamp);

                            crate::metrics::BLOCKS_MISSING_TOTAL.inc_by(diff);
                            crate::metrics::BLOCKS_RECEIVED_TOTAL.inc();

                            crate::metrics::PROOF_SUCCESS_TOTAL.inc();

                            let time_to_proof = metrics.time_to_input_ms + proving_time_ms as i64;

                            crate::metrics::TIME_TO_INPUT_HIST
                                .with_label_values(&[] as &[&str])
                                .observe(metrics.time_to_input_ms as f64);

                            crate::metrics::TIME_TO_PROOF_HIST
                                .with_label_values(&[] as &[&str])
                                .observe(time_to_proof as f64);
                            if time_to_proof <= 12000 {
                                crate::metrics::TIME_TO_PROOF_UNDER_12S_TOTAL.inc();
                            } else {
                                crate::metrics::TIME_TO_PROOF_OVER_12S_TOTAL.inc();
                            }

                            crate::metrics::PROVING_TIME_HIST
                                .with_label_values(&[] as &[&str])
                                .observe(proving_time_ms as f64);
                            if proving_time_ms <= 12000 {
                                crate::metrics::PROVING_UNDER_12S_TOTAL.inc();
                            } else {
                                crate::metrics::PROVING_OVER_12S_TOTAL.inc();
                            }

                            debug!(
                                "Published metrics for block {}, update time: {} ms",
                                metrics.block_number,
                                start.elapsed().as_millis()
                            );
                            // Remove the entry for the processed block
                            shared_metrics.remove(&proved_block_number);
                        } else {
                            warn!(
                                "No metrics entry found for block {} when trying to publish metrics",
                                proved_block_number
                            );
                        }
                    }

                    // Delete input file if not needed
                    state.delete_input_file(&block_info.filename());
                }
                Err(e) => {
                    let msg = format!(
                        "Failed proof for block {}, error: {}",
                        proved_block_number, e
                    );
                    error!("❌ {}", &msg);

                    let telegram_task = if state.cliargs.telegram_enabled(TelegramEvent::ProofFailed) {
                        let msg_clone = msg.clone();
                        Some(tokio::spawn(async move {
                            if let Err(e) = send_telegram_alert(&msg_clone, AlertType::Error).await {
                                warn!("Failed to send Telegram alert: {}, error: {}", msg_clone, e);
                            }
                        }))
                    } else {
                        None
                    };

                    if state.cliargs.enable_metrics {
                        crate::metrics::PROOF_FAILURE_TOTAL.inc();
                        // Publish all available metrics for this block
                        let mut shared_metrics = state.shared_metrics.lock().await;
                        let entry = shared_metrics.get(&proved_block_number);
                        if let Some(metrics) = entry {
                            let previous_block = crate::metrics::LATEST_BLOCK_NUMBER.get() as u64;
                            let diff = if proved_block_number > previous_block && previous_block != 0 {
                                proved_block_number - previous_block - 1
                            } else {
                                0
                            };
                            crate::metrics::BLOCKS_MISSING_TOTAL.inc_by(diff);

                            crate::metrics::LATEST_BLOCK_NUMBER.set(metrics.block_number as i64);
                            crate::metrics::LATEST_RECEIVED_TIME_MS.set(metrics.received_time_ms);
                            crate::metrics::LATEST_TIME_TO_INPUT_MS.set(metrics.time_to_input_ms);
                            crate::metrics::LATEST_MGAS.set(metrics.mgas as i64);
                            crate::metrics::LATEST_TX_COUNT.set(metrics.tx_count as i64);
                            crate::metrics::LATEST_PROVING_TIME_MS.set(metrics.proving_time_ms.unwrap_or(0));
                            crate::metrics::LATEST_PROVING_CYCLES.set(metrics.proving_cycles.unwrap_or(0));
                            crate::metrics::LATEST_BLOCK_TIMESTAMP.set(metrics.timestamp);
                            crate::metrics::LATEST_SUBMIT_TIME_MS.set(metrics.submit_time_ms.unwrap_or(0));
                            crate::metrics::BLOCKS_RECEIVED_TOTAL.inc();

                            debug!("Published failure metrics for block {}", metrics.block_number);
                            // Remove the entry for the processed block
                            shared_metrics.remove(&proved_block_number);
                        } else {
                            let previous_block = crate::metrics::LATEST_BLOCK_NUMBER.get() as u64;
                            let diff = if proved_block_number > previous_block && previous_block != 0 {
                                proved_block_number - previous_block - 1
                            } else {
                                0
                            };
                            crate::metrics::LATEST_BLOCK_NUMBER.set(proved_block_number as i64);
                            crate::metrics::BLOCKS_RECEIVED_TOTAL.inc();
                            crate::metrics::LATEST_BLOCK_TIMESTAMP.set(0);
                            crate::metrics::LATEST_SUBMIT_TIME_MS.set(0);
                            crate::metrics::LATEST_MGAS.set(0);
                            crate::metrics::LATEST_TX_COUNT.set(0);
                            crate::metrics::LATEST_PROVING_TIME_MS.set(0);
                            crate::metrics::LATEST_PROVING_CYCLES.set(0);
                            crate::metrics::LATEST_TIME_TO_INPUT_MS.set(0);
                            crate::metrics::LATEST_RECEIVED_TIME_MS.set(0);

                            crate::metrics::BLOCKS_MISSING_TOTAL.inc_by(diff);

                            warn!(
                                "No metrics entry found for block {} when trying to publish failure metrics",
                                proved_block_number
                            );
                        }
                    }

                    if state.cliargs.exit_on_error {
                        if let Some(handle) = telegram_task {
                            handle.await.ok();
                        }
                        std::process::exit(1);
                    }
                    return;
                }
            };
        })
    });

    // Report to EthProofs that we are proving this block in a separate parallel task
    if let Some(client) = ethproofs_client {
        tokio::spawn(async move {
            let start = std::time::Instant::now();
            match client.proof_proving(ethproofs_cluster_id.unwrap(), proved_block_number).await {
                Ok(_) => {
                    info!(
                        "Reported proving state to EthProofs for block {}, request_time: {} ms",
                        proved_block_number,
                        start.elapsed().as_millis()
                    );
                }
                Err(e) => {
                    error!(
                        "Failed to report proving state to EthProofs for block {}, error: {}",
                        proved_block_number, e
                    );
                }
            }
        });
    }

    Ok("".to_string())
}
