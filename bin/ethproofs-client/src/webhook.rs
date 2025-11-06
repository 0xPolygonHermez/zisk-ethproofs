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
use crate::metrics::{PROVING_CYCLES_GAUGE, PROVING_TIME_GAUGE, SUBMIT_TIME_GAUGE, prune_gauge_last_n};
use ethproofs_protocol::BlockInfo;
use crate::{db::BlockProof, prove::generate_proof};
use crate::{
    state::AppState,
    telegram::{send_telegram_alert, AlertType},
};

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

async fn process_webhook(proved_block_info: BlockInfo, payload: WebhookPayloadDto, state: AppState) {
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
        let result = generate_proof(next_block.clone(), state.clone()).await;

        match result {
            Ok(job_id) => {
                // Store current job ID
                let mut current_job_id = state.current_job_id.lock().unwrap_or_else(|e| e.into_inner());
                *current_job_id = job_id;
            }
            Err(e) => {
                // If generation failed, reset proving_block in atomic scope
                {
                    let mut proving_block = state.proving_block.lock().unwrap_or_else(|e| e.into_inner());
                    *proving_block = None;
                }

                let next_block_number = next_block.block_number;

                let msg = format!("Proof generation failed for next block {}, error: {}", next_block_number, e);
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

        return;
    }

    // If proof generation was successful, proceed to process the block proof

    // Encode compressed proof to base64
    let proof_base64 = match payload.proof.as_ref() {
        Some(proof) => match get_proof_b64(proof) {
            Ok(b64) => b64,
            Err(e) => {
                error!("❌ Failed to get compressed proof in base64 for block {}, error: {}", proved_block_number, e);
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
                    proved_block_number,
                    submit_time
                );
                (true, submit_time)
            },
            Err(e) => {
                error!("❌ Failed to submit proof to EthProofs for block {}, error: {}", proved_block_number, e);
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
                Err(e) => error!("❌ Failed to insert proof into DB for block {}, error: {}", proved_block_number, e),
            }
        } else {
            warn!("DB handle not initialized, cannot insert proof for block {}", proved_block_number);
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
        let block_label = proved_block_number.to_string();
        PROVING_TIME_GAUGE.with_label_values(&[&block_label]).set(proving_time_ms as i64);
        PROVING_CYCLES_GAUGE.with_label_values(&[&block_label]).set(proving_cycles as i64);
        prune_gauge_last_n(&PROVING_TIME_GAUGE, 300);
        prune_gauge_last_n(&PROVING_CYCLES_GAUGE, 300);
        if submitted {
            SUBMIT_TIME_GAUGE.with_label_values(&[&block_label]).set(submit_time as i64);
            prune_gauge_last_n(&SUBMIT_TIME_GAUGE, 300);
        }
        debug!(
            "Updated metrics for block {}, update time: {} ms",
            proved_block_number,
            start.elapsed().as_millis()
        );
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
        warn!("Received webhook for job {}, but no block is currently being proved. Ignoring...", payload.job_id);
        return (StatusCode::OK, "OK").into_response();
    }

    // Check if the job ID matches the current job ID
    let current_job_id = {
        let job_id = state.current_job_id.lock().unwrap_or_else(|e| e.into_inner());
        job_id.clone()
    };
    if current_job_id != job_id {
        warn!("Received webhook for job {}, but current job is {}. Ignoring...", payload.job_id, current_job_id);
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

    let webhook_app = Router::new()
        .route("/:job_id", post(webhook_handler))
        .with_state(state.clone());

    let webhook_server = async move {
        axum::serve(tokio::net::TcpListener::bind(webhook_addr).await?, webhook_app)
            .await
            .map_err(|e| anyhow!("Webhookserver error: {e}"))
    };

    info!("Webhook server running at http://{}", webhook_addr);

    tokio::try_join!(webhook_server)?;

    Ok(())
}
