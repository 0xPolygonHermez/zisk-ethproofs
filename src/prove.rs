use std::{
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::{anyhow, Result};
use base64::{engine::general_purpose, Engine};
use chrono::Utc;
use log::{debug, error, info, warn};
use zisk_sdk::{ExecutorKind, ProofKind, ProveResult, ZiskStdin};

use crate::{
    db::BlockProof,
    state::{AppState, BlockInfo},
    telegram::{send_block_proved_alert, send_proof_failed_alert},
};

pub fn get_proof_b64(proof_data: &[u8]) -> Result<String> {
    Ok(general_purpose::STANDARD.encode(proof_data))
}

pub async fn generate_proof(block_info: BlockInfo, state: AppState) -> Result<String> {
    let proved_block_number = block_info.block_number;

    let prover_client_clone = Arc::clone(&state.prover_client);
    let guest_program_clone = Arc::clone(&state.guest_program);
    let ethproofs_client = state.ethproofs_client.clone();
    let ethproofs_cluster_id = state.cliargs.ethproofs.cluster_id;

    tokio::task::spawn_blocking(move || {
        let rt = tokio::runtime::Handle::current();
        rt.block_on(async {
            let prover_client = prover_client_clone.lock().unwrap();
            let guest_program = guest_program_clone.lock().unwrap();

            // Wall-clock timestamp marking the start of this block's proof generation.
            let proof_start_ts = Utc::now().format("%Y-%m-%dT%H:%M:%S%.6fZ").to_string();

            #[cfg(zisk_hints)]
            let handle = {
                use zisk_sdk::ZiskStream;

                use crate::process::launch_hints_generation;

                let hints_handle = launch_hints_generation(&block_info, &state).await;

                //TODO: Implement hints file handling
                if state.cliargs.hints.mode == crate::cliargs::HintsMode::File {
                    hints_handle.await.ok();
                }

                let hints_stream = ZiskStream::unix_external(&state.cliargs.hints.socket);

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
                    .prove(&guest_program, stdin)
                    .timeout(Duration::from_secs(state.cliargs.prove_timeout))
                    .executor(ExecutorKind::Assembly)
                    .wrap(ProofKind::VadcopFinalMinimal)
                    .run()

            };

            let mut prove_job_id = "N/A".to_string();
            let prove_result = match handle {
                Ok(handle) => {
                    prove_job_id = handle.job_id().map(|id| id.to_string()).unwrap_or_else(|| "N/A".to_string());
                    info!("🔄 Generating proof for block {}, job_id: {}", proved_block_number, prove_job_id);

                    handle.await.map_err(|e| anyhow!(e))
                }
                Err(e) => {
                    Err(anyhow!("Failed to start proof generation for block {}: {}", proved_block_number, e))
                }
            };

            // Report to EthProofs that we are proving this block
            if let Some(client) = &ethproofs_client {
                client.proof_proving(ethproofs_cluster_id.unwrap(), proved_block_number);
            }

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

            // If the configured '--run-time' has elapsed, we must not start proving any queued
            // block; the process exits right after the current proof has been fully processed.
            let deadline_reached =
                state.run_deadline.is_some_and(|deadline| Instant::now() >= deadline);

            // Check and start proof generation for next block if set (unless the run time is up)
            if next_block.is_some() && !deadline_reached {
                // Set proving_block to next_block_number in atomic scope
                {
                    let mut proving_block = state.proving_block.lock().unwrap_or_else(|e| e.into_inner());
                    *proving_block = next_block.clone();
                }

                let next_block = next_block.unwrap();

                // Get input file and for next block to prove
                let input_filename =
                    format!("{}/{}", state.cliargs.inputs.folder, next_block.filename());

                // Read input file into ZiskStdin
                let path = PathBuf::from(&input_filename);
                let zisk_stdin = match ZiskStdin::from_file(&path) {
                    Ok(stdin) => stdin,
                    Err(e) => {
                        error!("Error opening input file {}: {}", path.display(), e);
                        return;
                    }
                };

                // Store input in shared state for next proof generation
                {
                    let zisk_stdin_shared = Arc::clone(&state.zisk_stdin);
                    let mut zisk_stdin_lock = zisk_stdin_shared.lock().unwrap();
                    *zisk_stdin_lock = Some(zisk_stdin);
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

                        // Run proof-failed hook if configured
                        if let Some(script) = &state.cliargs.hooks.proof_failed {
                            let job_id =
                                state.current_job_id.lock().unwrap_or_else(|e| e.into_inner()).clone();
                            crate::hooks::run_hook(
                                script,
                                vec![next_block_number.to_string(), job_id],
                            );
                        }

                        send_proof_failed_alert(&state, msg);

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
                    process_proof_success(result, block_info, proved_block_number, proof_start_ts, state.clone()).await;
                }
                Err(e) => {
                    process_proof_failure(e, proved_block_number, state.clone(), prove_job_id).await;
                }
            };

            if deadline_reached {
                info!("Configured run time elapsed, exiting after proof completion.");
                state.log_proved_blocks_summary();
                std::process::exit(0);
            }
        })
    });

    Ok("".to_string())
}

/// Process a successful proof generation: encode, optionally save to disk,
/// submit to EthProofs, store in DB, send Telegram alert and publish metrics.
async fn process_proof_success(
    result: ProveResult,
    block_info: BlockInfo,
    proved_block_number: u64,
    proof_start_ts: String,
    state: AppState,
) {
    let proving_time_ms = result.get_proving_time();
    let proving_cycles = result.get_execution_steps();
    let job_id_str = result.job_id().map(|id| id.to_string()).unwrap_or_else(|| "N/A".to_string());

    // Record the successfully proved block (updates the counter and the '--proof.csv' file).
    state.record_proved_block(
        &proof_start_ts,
        proved_block_number,
        block_info.mgas,
        block_info.tx_count,
        proving_cycles,
        proving_time_ms as u64,
    );

    // Encode compressed proof to base64
    let proof_bytes = match result.get_proof_u64() {
        Ok(bytes) => {
            // Convert Vec<u64> to Vec<u8> (little-endian)
            bytes.iter().flat_map(|x| x.to_le_bytes()).collect::<Vec<u8>>()
        }
        Err(e) => {
            error!("❌ Failed to get proof bytes for block {}, error: {}", proved_block_number, e);
            return;
        }
    };

    // Save proof to disk if enabled
    if state.cliargs.proof.save {
        let proof_dir = PathBuf::from(&state.cliargs.proof.folder);
        if let Err(e) = std::fs::create_dir_all(&proof_dir) {
            error!(
                "❌ Failed to create proof directory {} for block {}, error: {}",
                proof_dir.display(),
                proved_block_number,
                e
            );
        } else {
            let proof_path = proof_dir.join(format!("{}_proof.bin", proved_block_number));
            match std::fs::write(&proof_path, proof_bytes.as_slice()) {
                Ok(_) => info!(
                    "Proof saved to {} for block {}",
                    proof_path.display(),
                    proved_block_number
                ),
                Err(e) => error!(
                    "❌ Failed to save proof to {} for block {}, error: {}",
                    proof_path.display(),
                    proved_block_number,
                    e
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

    // Submit to EthProofs if enabled
    if state.cliargs.ethproofs.submit {
        let client = state.ethproofs_client.as_ref().unwrap();
        let cluster_id = state.cliargs.ethproofs.cluster_id.unwrap();
        client.proof_proved(
            cluster_id,
            proved_block_number,
            proving_time_ms as u128,
            proving_cycles,
            proof_base64.clone(),
            job_id_str,
        );
    }

    // Insert into DB if enabled
    if state.cliargs.db.enabled {
        if let Some(db) = &state.db_block_proofs {
            let start = std::time::Instant::now();
            let block_proof = BlockProof {
                block_number: proved_block_number,
                zisk_version: state.cliargs.db.zisk_version.clone().unwrap_or_default(),
                hardware: state.cliargs.db.hardware.clone().unwrap_or_default(),
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
    send_block_proved_alert(&state, proved_block_number, proving_time_ms, proving_cycles);

    // Update Prometheus metrics for proof generation if metrics enabled
    if state.cliargs.metrics.enabled {
        let start = std::time::Instant::now();
        let mut shared_metrics = state.shared_metrics.lock().await;
        let entry = shared_metrics.get_mut(&proved_block_number);
        if let Some(metrics) = entry {
            metrics.proving_time_ms = Some(proving_time_ms as i64);
            metrics.proving_cycles = Some(proving_cycles as i64);
            metrics.success = true;

            crate::metrics::publish_proof_success(metrics, proving_time_ms);

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

/// Process a failed proof generation: log, run hook, send Telegram alert,
/// publish failure metrics and optionally exit the process.
async fn process_proof_failure(
    e: anyhow::Error,
    proved_block_number: u64,
    state: AppState,
    prove_job_id: String,
) {
    let msg = format!("Failed proof for block {}, error: {}", proved_block_number, e);
    error!("❌ {}", &msg);

    // Run proof-failed hook if configured
    if let Some(script) = &state.cliargs.hooks.proof_failed {
        crate::hooks::run_hook(script, vec![proved_block_number.to_string(), prove_job_id]);
    }

    let telegram_task = send_proof_failed_alert(&state, msg);

    if state.cliargs.metrics.enabled {
        let mut shared_metrics = state.shared_metrics.lock().await;
        let entry = shared_metrics.get(&proved_block_number);
        if let Some(metrics) = entry {
            crate::metrics::publish_proof_failure_with_metrics(metrics);
            debug!("Published failure metrics for block {}", metrics.block_number);
            // Remove the entry for the processed block
            shared_metrics.remove(&proved_block_number);
        } else {
            crate::metrics::publish_proof_failure_no_metrics(proved_block_number);
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
        state.log_proved_blocks_summary();
        std::process::exit(1);
    }
}
