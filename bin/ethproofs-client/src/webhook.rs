use std::{env, net::SocketAddr};

use anyhow::{anyhow, Result};
use axum::{
    extract::Path, extract::State, http::StatusCode, response::IntoResponse, routing::post, Json,
    Router,
};
use base64::{engine::general_purpose, Engine};
use log::{error, info, warn};
use prometheus::{Encoder, TextEncoder, register_int_gauge_vec, IntGaugeVec};
use lazy_static::lazy_static;
use axum::{routing::get};

// Prometheus metrics
lazy_static! {
    static ref PROVING_TIME_GAUGE: IntGaugeVec = register_int_gauge_vec!(
        "proving_time_ms",
        "Proof generation time in milliseconds",
        &["block"]
    ).unwrap();
    static ref SUBMIT_TIME_GAUGE: IntGaugeVec = register_int_gauge_vec!(
        "submit_time_ms",
        "Proof submit time in milliseconds",
        &["block"]
    ).unwrap();
    static ref PROVING_CYCLES_GAUGE: IntGaugeVec = register_int_gauge_vec!(
        "proving_cycles",
        "Proof generation cycles",
        &["block"]
    ).unwrap();
}

/// Prune gauge to keep only the last N block labels
fn prune_gauge_last_n(gauge: &IntGaugeVec, keep_last: usize) {
    use prometheus::core::Collector;
    let mut blocks: Vec<u64> = gauge
        .collect()
        .iter()
        .flat_map(|mf| {
            mf.get_metric().iter().filter_map(|m| {
                m.get_label().iter().find(|l| l.name() == "block")
                    .and_then(|l| l.value().parse::<u64>().ok())
            })
        })
        .collect();
    blocks.sort_unstable();
    if blocks.len() > keep_last {
        for old_block in &blocks[..blocks.len() - keep_last] {
            let _ = gauge.remove_label_values(&[&old_block.to_string()]);
        }
    }
}

async fn metrics_handler() -> impl IntoResponse {
    let encoder = TextEncoder::new();
    let metric_families = prometheus::gather();
    let mut buffer = Vec::new();
    encoder.encode(&metric_families, &mut buffer).unwrap();
    (
        StatusCode::OK,
        [(axum::http::header::CONTENT_TYPE, encoder.format_type().to_string())],
        buffer,
    )
}
use zisk_distributed_common::WebhookPayloadDto;

use crate::{db::BlockProof, prove::generate_proof};
use crate::{
    state::AppState,
    telegram::{send_telegram_alert, AlertType},
};

const DEFAULT_WEBHOOK_PORT: u16 = 8051;
const DEFAULT_METRICS_PORT: u16 = 8384;

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

async fn process_webhook(proved_block: u64, payload: WebhookPayloadDto, state: AppState) {
    // Extract proving time and cycles
    let proving_time_ms: u32 = payload.duration_ms as u32;
    let proving_cycles = payload.executed_steps.unwrap_or(0);

    info!(
        "✅  Proof generated for block {}, proving_time: {} ms, cycles: {}, job: {}",
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
                error!("❌  Failed to get compressed proof in base64, error: {}", e);
                return;
            }
        },
        None => {
            error!("❌  No proof data in payload");
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
                proved_block,
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
                    "Proof submitted to ethproofs for block {}, submit_time: {} ms",
                    proved_block,
                    submit_time
                );
                (true, submit_time)
            },
            Err(e) => {
                error!("❌  Failed to submit proof to EthProofs: {}", e);
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
                Err(e) => error!("❌  Failed to insert proof into DB: {}", e),
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

    // Update Prometheus metrics for proof generation
    let start = std::time::Instant::now();
    let block_label = proved_block.to_string();
    PROVING_TIME_GAUGE.with_label_values(&[&block_label]).set(proving_time_ms as i64);
    PROVING_CYCLES_GAUGE.with_label_values(&[&block_label]).set(proving_cycles as i64);
    prune_gauge_last_n(&PROVING_TIME_GAUGE, 300);
    prune_gauge_last_n(&PROVING_CYCLES_GAUGE, 300);
    if submitted {
        SUBMIT_TIME_GAUGE.with_label_values(&[&block_label]).set(submit_time as i64);
        prune_gauge_last_n(&SUBMIT_TIME_GAUGE, 300);
    }
    info!(
        "Updated metrics for block {}, update time: {} ms",
        proved_block,
        start.elapsed().as_millis()
    );

    // Delete input file if not needed
    state.delete_input_file(proved_block);
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

    tokio::spawn(async move {
        process_webhook(proved_block, payload, state.clone()).await;
    });

    (StatusCode::OK, "OK").into_response()
}

pub async fn start_webhook_server(state: AppState) -> Result<()> {
    let webhook_port = env::var("WEBHOOK_PORT").unwrap_or(DEFAULT_WEBHOOK_PORT.to_string());
    let metrics_port = env::var("METRICS_PORT").unwrap_or(DEFAULT_METRICS_PORT.to_string());

    let webhook_addr: SocketAddr = format!("127.0.0.1:{}", webhook_port)
        .parse()
        .map_err(|e| anyhow!("Invalid webhook bind address, error: {e}"))?;
    let metrics_addr: SocketAddr = format!("0.0.0.0:{}", metrics_port)
        .parse()
        .map_err(|e| anyhow!("Invalid metrics bind address, error: {e}"))?;

    let webhook_app = Router::new()
        .route("/:job_id", post(webhook_handler))
        .with_state(state);

    let metrics_app = Router::new()
        .route("/metrics", get(metrics_handler));

    info!("Webhook server running at http://{}", webhook_addr);
    info!("Metrics server running at http://{}", metrics_addr);

    let webhook_server = async move {
        axum::serve(tokio::net::TcpListener::bind(webhook_addr).await?, webhook_app)
            .await
            .map_err(|e| anyhow!("Webhookserver error: {e}"))
    };

    let metrics_server = async move {
        axum::serve(tokio::net::TcpListener::bind(metrics_addr).await?, metrics_app)
            .await
            .map_err(|e| anyhow!("Metrics server error: {e}"))
    };

    // Run both servers concurrently
    tokio::try_join!(webhook_server, metrics_server)?;
    Ok(())
}
