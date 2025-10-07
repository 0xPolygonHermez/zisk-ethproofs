use std::{env, net::SocketAddr};

use anyhow::{anyhow, Result};
use axum::{extract::State, http::StatusCode, extract::Path, response::{IntoResponse, Response}, routing::post, Json, Router};
use base64::{Engine, engine::general_purpose};
use log::{error, info, warn};
use zisk_distributed_common::WebhookPayloadDto;
use zstd::Encoder;

use crate::{telegram::{send_telegram_alert, AlertType}, state::AppState};
use crate::DEFAULT_INPUTS_FOLDER;

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

async fn webhook_handler(
    Path(_job_id): Path<String>,
    State(state): State<AppState>,
    Json(payload): Json<WebhookPayloadDto>,
) -> Response {
    // Get shared block number
    let proved_block = { *state.proving_block.lock().unwrap_or_else(|e| e.into_inner()) };

    if !payload.success {
        error!("Failed proof for block number {}, job: {}", proved_block, payload.job_id);
        return (StatusCode::OK, "OK").into_response();
    }

    let proving_time_ms: u128 = payload.duration_ms.into();
    let proving_cycles = payload.executed_steps.unwrap_or(0);

    info!(
        "Proof generated for block {}, proving_time: {}ms, cycles: {}, job: {}",
        proved_block,
        payload.duration_ms,
        proving_cycles,
        payload.job_id
    );

    // Encode compressed proof to base64
    let proof_base64 = match payload.proof.as_ref() {
        Some(proof) => match get_proof_b64(proof) {
            Ok(b64) => b64,
            Err(e) => {
                error!("Failed to get compressed proof in base64: {e}");
                return (StatusCode::BAD_REQUEST, "Error getting compressing proof").into_response();
            }
        },
        None => return (StatusCode::BAD_REQUEST, "No proof data in payload").into_response(),
    };

    // Submit to EthProofs if enabled
    if state.cliargs.submit_ethproofs {
        let client = &state.ethproofs_client.unwrap();
        let cluster_id = state.ethproofs_cluster_id.unwrap();
        let start = std::time::Instant::now();
        match client.proof_proved(cluster_id, proved_block, proving_time_ms, proving_cycles, proof_base64, payload.job_id.clone()).await {
            Ok(_) => info!("Proof submitted to ethproofs for block {}, submit_time: {} ms", proved_block, start.elapsed().as_millis()),
            Err(e) => error!("Failed to submit proof to ethproofs: {}", e),
        }
    }

    // Send Telegram alert if enabled
    if state.cliargs.submit_alert {
        let msg = format!(
            "Proof generated for block {}, proving_time: {}s, cycles: {}",
            proved_block,
            proving_time_ms,
            proving_cycles,
        );

        if let Err(e) = send_telegram_alert(&msg, AlertType::Success).await {
            warn!("Failed to send Telegram alert: {}", e);
        }
    }

    // Clean up input file if not needed
    if !state.cliargs.keep_input {
        let inputs_folder = env::var("UPLOAD_FOLDER").unwrap_or(DEFAULT_INPUTS_FOLDER.to_string());
        let input_file_path = format!("{}/input_{}.json", inputs_folder, proved_block);
        if let Err(e) = std::fs::remove_file(&input_file_path) {
            warn!("Failed to remove input file {}, error: {}", input_file_path, e);
        }
    }

    (StatusCode::OK, "OK").into_response()
}

pub async fn start_webhook_server(state: AppState) -> Result<()> {
    let webhook_port = env::var("WEBHOOK_PORT").unwrap_or(DEFAULT_WEBHOOK_PORT.to_string());
    let addr: SocketAddr = format!("127.0.0.1:{}", webhook_port)
        .parse()
        .map_err(|e| anyhow!("invalid bind addr: {e}"))?;

    let app = Router::new()
        .route("/:job_id", post(webhook_handler))
        .with_state(state);

    info!("Webhook server running at http://{}", addr);

    axum::serve(tokio::net::TcpListener::bind(addr).await?, app)
        .await
        .map_err(|e| anyhow!("Webhookserver error: {e}"))
}
