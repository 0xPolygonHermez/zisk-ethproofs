use std::{env, net::SocketAddr};

use anyhow::{anyhow, Result};
use axum::{
    extract::Path, extract::State, http::StatusCode, response::IntoResponse, routing::post, Json,
    Router,
};
use base64::{engine::general_purpose, Engine};
use log::{error, info, warn};
use zisk_distributed_common::WebhookPayloadDto;
use zstd::Encoder;

use crate::{db::BlockProof, prove::generate_proof};
use crate::{
    state::AppState,
    telegram::{send_telegram_alert, AlertType},
};

const DEFAULT_WEBHOOK_PORT: u16 = 8051;

pub fn get_proof_b64(proof_data: &[u64]) -> Result<String> {
    // Convert &[u64] to &[u8] without copying
    let proof_bytes = bytemuck::cast_slice::<u64, u8>(proof_data);

    let mut compressed = Vec::new();
    {
        // Compression level 1 (as in your original code)
        let mut encoder = Encoder::new(&mut compressed, 1)?;
        use std::io::Write;
        encoder.write_all(proof_bytes)?;
        encoder.finish()?;
    }

    Ok(general_purpose::STANDARD.encode(&compressed))
}

#[axum::debug_handler]
async fn webhook_handler(
    Path(job_id): Path<String>,
    State(state): State<AppState>,
    Json(payload): Json<WebhookPayloadDto>,
) -> impl IntoResponse {
    // Read and capture the current proving block in atomic scope
    let proved_block: u64 = {
        let proving_block = state.proving_block.lock().unwrap_or_else(|e| e.into_inner());
        *proving_block
    };

    // Validate that the webhook corresponds to the current proving block
    if proved_block == 0 {
        warn!("Received webhook for job {}, but no block is currently being proved", payload.job_id);
        return (StatusCode::OK, "OK").into_response();
    }

    // Check if the job ID matches the current job ID
    let current_job_id = {
        let job_id = state.current_job_id.lock().unwrap_or_else(|e| e.into_inner());
        job_id.clone()
    };
    if current_job_id.eq(&job_id) {
        warn!("Received webhook for job {}, but current job is {}", payload.job_id, current_job_id);
        return (StatusCode::OK, "OK").into_response();
    }

    // Check if proof generation was successful
    if !payload.success {
        match payload.error {
            Some(err) => error!(
                "❌  Failed proof for block number {}, job: {}, error: {}-{}",
                proved_block, payload.job_id, err.code, err.message
            ),
            None => error!(
                "❌  Failed proof for block number {}, job: {}",
                proved_block, payload.job_id
            ),
        }
        return (StatusCode::OK, "OK").into_response();
    }

    let proving_time_ms: u32 = payload.duration_ms as u32;
    let proving_cycles = payload.executed_steps.unwrap_or(0);

    info!(
        "✅  Proof generated for block {}, proving_time: {}ms, cycles: {}, job: {}",
        proved_block, payload.duration_ms, proving_cycles, payload.job_id
    );

    // Get next_block_number in atomic scope
    let next_block_number: u64 = {
        let mut next_proving_block =
            state.next_proving_block.lock().unwrap_or_else(|e| e.into_inner());
        if *next_proving_block != 0 {
            let next = *next_proving_block;
            *next_proving_block = 0;
            next
        } else {
            0
        }
    };

    // Check and start proof generation for next block if set
    if next_block_number != 0 {
        // Set proving_block to next_block_number in atomic scope
        {
            let mut proving_block = state.proving_block.lock().unwrap_or_else(|e| e.into_inner());
            *proving_block = next_block_number;
        }

        let result = generate_proof(next_block_number, state.clone()).await;

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
                    *proving_block = 0;
                }
                error!("❌  Proof generation failed for next block number {}, error: {}", next_block_number, e);

                // Clean up input file if not needed
                state.delete_input_file(next_block_number);
            }
        }
    } else {
        // Reset proving_block
        let mut proving_block = state.proving_block.lock().unwrap_or_else(|e| e.into_inner());
        *proving_block = 0;
    }

    // Encode compressed proof to base64
    let proof_base64 = match payload.proof.as_ref() {
        Some(proof) => match get_proof_b64(proof) {
            Ok(b64) => b64,
            Err(e) => {
                error!("Failed to get compressed proof in base64: {e}");
                return (StatusCode::BAD_REQUEST, "Error getting compressing proof")
                    .into_response();
            }
        },
        None => return (StatusCode::BAD_REQUEST, "No proof data in payload").into_response(),
    };

    // Submit to EthProofs if enabled
    if state.cliargs.submit_ethproofs {
        let state = state.clone();
        let client = &state.ethproofs_client.unwrap();
        let cluster_id = state.ethproofs_cluster_id.unwrap();
        let start = std::time::Instant::now();
        match client
            .proof_proved(
                cluster_id,
                proved_block,
                proving_time_ms as u128,
                proving_cycles,
                &proof_base64,
                payload.job_id.clone(),
            )
            .await
        {
            Ok(_) => info!(
                "Proof submitted to ethproofs for block {}, submit_time: {} ms",
                proved_block,
                start.elapsed().as_millis()
            ),
            Err(e) => error!("Failed to submit proof to ethproofs: {}", e),
        }
    }

    // Insert into DB if enabled
    if state.cliargs.insert_db {
        if let Some(db) = &state.db_block_proofs {
            let start = std::time::Instant::now();
            let block_proof = BlockProof {
                block_number: proved_block,
                zisk_version: "0.12.0".to_string(),
                hardware: "128 vCPU, 512GB RAM, 1 RTX4090 GPU".to_string(),
                proving_time: proving_time_ms as u32,
                proof: proof_base64,
                steps: proving_cycles,
            };
            match db.enqueue(block_proof).await {
                Ok(_) => info!(
                    "Proof inserted into DB for block {}, insert_time: {} ms",
                    proved_block,
                    start.elapsed().as_millis()
                ),
                Err(e) => error!("Failed to insert proof into DB: {}", e),
            }
        } else {
            warn!("DB handle not initialized, cannot insert proof for block {}", proved_block);
        }
    }

    // Send Telegram alert if enabled
    if state.cliargs.submit_alert {
        let msg = format!(
            "Proof generated for block {}, proving_time: {}s, cycles: {}",
            proved_block,
            proving_time_ms / 1000,
            proving_cycles,
        );

        if let Err(e) = send_telegram_alert(&msg, AlertType::Success).await {
            warn!("Failed to send Telegram alert: {}", e);
        }
    }

    // Delete input file if not needed
    state.delete_input_file(proved_block);

    (StatusCode::OK, "OK").into_response()
}

pub async fn start_webhook_server(state: AppState) -> Result<()> {
    let webhook_port = env::var("WEBHOOK_PORT").unwrap_or(DEFAULT_WEBHOOK_PORT.to_string());
    let addr: SocketAddr = format!("127.0.0.1:{}", webhook_port)
        .parse()
        .map_err(|e| anyhow!("invalid bind addr: {e}"))?;

    let app = Router::new().route("/:job_id", post(webhook_handler)).with_state(state);

    info!("Webhook server running at http://{}", addr);

    axum::serve(tokio::net::TcpListener::bind(addr).await?, app)
        .await
        .map_err(|e| anyhow!("Webhookserver error: {e}"))
}
