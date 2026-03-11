use std::net::SocketAddr;

use anyhow::{anyhow, Result};
use axum::{
    extract::Path, extract::State, http::StatusCode, response::IntoResponse, routing::post, Json,
    Router,
};
use base64::{engine::general_purpose, Engine};
use log::{debug, error, info, warn};

use zisk_distributed_common::WebhookPayloadDto;

use crate::cliargs::TelegramEvent;
use crate::{db::BlockProof, prove::generate_proof};
use crate::{
    state::AppState,
    telegram::{send_telegram_alert, AlertType},
};
use ethproofs_common::protocol::BlockInfo;

pub fn get_proof_b64(proof_data: &[u64]) -> Result<String> {
    // Convert &[u64] to &[u8] without copying
    let proof_bytes = bytemuck::cast_slice::<u64, u8>(proof_data);

    let mut compressed = Vec::new();
    {
        // Compression level 1 (as in your original code)
        let mut encoder = zstd::stream::Encoder::new(&mut compressed, 1)?;
        use std::io::Write;
        encoder.write_all(proof_bytes)?;
        encoder.finish()?;
    }

    Ok(general_purpose::STANDARD.encode(&compressed))
}

async fn process_webhook(
    proved_block_info: BlockInfo,
    payload: WebhookPayloadDto,
    state: AppState,
) {
    // Extract proving time and cycles
    let proving_time_ms: u32 = payload.duration_ms as u32;
    let proving_cycles = payload.executed_steps.unwrap_or(0);
    let proved_block_number = proved_block_info.block_number;

    // We show now successful log to show it before the log showing proof generation of next block
    // This way, logs are in order of events
    if payload.success {
        info!(
            "✅ Proof generated for block {}, proving_time: {} ms, cycles: {}, job: {}",
            proved_block_number, payload.duration_ms, proving_cycles, payload.job_id
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

        #[cfg(zisk_hints)]
        {
            use std::path::PathBuf;
            use std::sync::Arc;
            use crate::state::ZiskStdinWrapper;
            use guest_reth::{RethInputPublic, RethInputWitness};
            use zisk_sdk::{ZiskFileStdin, ZiskIO};

            use crate::process::launch_hints_generation;

            let input_filename =
                format!("{}/{}", state.inputs_folder.clone(), next_block.filename());

            let path = PathBuf::from(&input_filename);
            let zisk_stdin_file = match ZiskFileStdin::new(&path) {
                Ok(stdin) => stdin,
                Err(e) => {
                    error!("Error opening input file {}: {}", path.display(), e);
                    return;
                }
            };
            let input_pk: RethInputPublic = match zisk_stdin_file.read() {
                Ok(pk) => pk,
                Err(e) => {
                    error!("Error reading public input from file {}: {}", path.display(), e);
                    return;
                }
            };
            let input_witness: RethInputWitness = match zisk_stdin_file.read() {
                Ok(witness) => witness,
                Err(e) => {
                    error!("Error reading witness input from file {}: {}", path.display(), e);
                    return;
                }
            };

            {
                let zisk_stdin = ZiskStdinWrapper::new();
                zisk_stdin.write(&input_pk);
                zisk_stdin.write(&input_witness);

                let zisk_stdin_shared = Arc::clone(&state.zisk_stdin);
                let mut zisk_stdin_lock = zisk_stdin_shared.lock().unwrap();
                *zisk_stdin_lock = Some(zisk_stdin);
            }

            launch_hints_generation(&next_block, &state).await;
        }

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
    }

    // Return if proof generation was not successful logging the error
    if !payload.success {
        let (err_code, err_message) = payload
            .error
            .map(|err| (err.code, err.message))
            .unwrap_or_else(|| ("0".to_string(), "Unknown error".to_string()));

        let msg = format!(
            "Failed proof for block {}, job: {}, error: {}-{}",
            proved_block_number, payload.job_id, err_code, err_message
        );
        error!("❌ {}", &msg);

        if state.cliargs.telegram_enabled(TelegramEvent::ProofFailed) {
            tokio::spawn(async move {
                if let Err(e) = send_telegram_alert(&msg, AlertType::Error).await {
                    warn!("Failed to send Telegram alert: {}, error: {}", msg, e);
                }
            });
        }

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
        return;
    }

    // If proof generation was successful, proceed to process the block proof

    // Encode compressed proof to base64
    let proof_base64 = match payload.proof.as_ref() {
        Some(proof) => match get_proof_b64(proof) {
            Ok(b64) => b64,
            Err(e) => {
                error!(
                    "❌ Failed to get compressed proof in base64 for block {}, error: {}",
                    proved_block_number, e
                );
                return;
            }
        },
        None => {
            error!("❌ No proof data in payload fro block {}", proved_block_number);
            return;
        }
    };

    // Submit to EthProofs if enabled
    let (submitted, submit_time) = if state.cliargs.submit_ethproofs {
        let state = state.clone();
        let client = &state.ethproofs_client.unwrap();
        let cluster_id = state.ethproofs_cluster_id.unwrap();
        let start = std::time::Instant::now();
        match client
            .proof_proved(
                cluster_id,
                proved_block_number,
                proving_time_ms as u128,
                proving_cycles,
                &proof_base64,
                payload.job_id.clone(),
            )
            .await
        {
            Ok(_) => {
                let submit_time = start.elapsed().as_millis() as f64;
                info!(
                    "Reported proved state to EthProofs for block {}, request_time: {} ms",
                    proved_block_number, submit_time
                );
                (true, submit_time)
            }
            Err(e) => {
                error!(
                    "❌ Failed to submit proof to EthProofs for block {}, error: {}",
                    proved_block_number, e
                );
                (false, 0f64)
            }
        }
    } else {
        (false, 0f64)
    };

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
            metrics.submit_time_ms = if submitted { Some(submit_time as i64) } else { Some(0) };
            metrics.success = payload.success;

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
    state.delete_input_file(&proved_block_info.filename());
}

#[axum::debug_handler]
async fn webhook_handler(
    Path(job_id): Path<String>,
    State(state): State<AppState>,
    Json(payload): Json<WebhookPayloadDto>,
) -> impl IntoResponse {
    // Read and capture the current proving block in atomic scope
    let proved_block = {
        let proving_block = state.proving_block.lock().unwrap_or_else(|e| e.into_inner());
        proving_block.clone()
    };

    // Validate that the webhook corresponds to the current proving block
    if proved_block.is_none() {
        warn!(
            "Received webhook for job {}, but no block is currently being proved. Ignoring...",
            payload.job_id
        );
        return (StatusCode::OK, "OK").into_response();
    }

    // Check if the job ID matches the current job ID
    let current_job_id = {
        let job_id = state.current_job_id.lock().unwrap_or_else(|e| e.into_inner());
        job_id.clone()
    };
    if current_job_id != job_id {
        warn!(
            "Received webhook for job {}, but current job is {}. Ignoring...",
            payload.job_id, current_job_id
        );
        return (StatusCode::OK, "OK").into_response();
    }

    tokio::spawn(async move {
        process_webhook(proved_block.unwrap(), payload, state.clone()).await;
    });

    (StatusCode::OK, "OK").into_response()
}

pub async fn start_webhook_server(state: AppState) -> Result<()> {
    let webhook_addr: SocketAddr = format!("127.0.0.1:{}", state.webhook_port)
        .parse()
        .map_err(|e| anyhow!("Invalid webhook bind address, error: {e}"))?;

    let webhook_app =
        Router::new().route("/:job_id", post(webhook_handler)).with_state(state.clone());

    let webhook_server = async move {
        axum::serve(tokio::net::TcpListener::bind(webhook_addr).await?, webhook_app)
            .await
            .map_err(|e| anyhow!("Webhookserver error: {e}"))
    };

    info!("Webhook server running at http://{}", webhook_addr);

    tokio::try_join!(webhook_server)?;

    Ok(())
}
