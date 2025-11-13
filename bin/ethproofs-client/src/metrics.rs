use std::net::SocketAddr;

use anyhow::{anyhow, Result};
use axum::{http::StatusCode, response::IntoResponse, Router};
use log::{info};
use prometheus::{Encoder, TextEncoder, register_int_gauge};
use lazy_static::lazy_static;
use axum::{routing::get};

use crate::{state::AppState};

#[derive(Clone, Debug)]
pub struct BlockMetrics {
    pub block_number: u64,
    pub received_time_ms: i64,
    pub time_to_input_ms: i64,
    pub mgas: u64,
    pub tx_count: u64,
    pub timestamp: i64, // Unix timestamp (seconds)
    pub proving_time_ms: Option<i64>,
    pub proving_cycles: Option<i64>,
    pub submit_time_ms: Option<i64>,
    pub success: bool,
}

// Prometheus metrics
lazy_static! {
    pub static ref LATEST_MGAS: prometheus::IntGauge = prometheus::register_int_gauge!(
        "latest_mgas",
        "Latest mgas value for processed block"
    ).unwrap();
    pub static ref LATEST_TX_COUNT: prometheus::IntGauge = prometheus::register_int_gauge!(
        "latest_tx_count",
        "Latest tx_count value for processed block"
    ).unwrap();
    pub static ref LATEST_PROVING_TIME_MS: prometheus::IntGauge = prometheus::register_int_gauge!(
        "latest_proving_time_ms",
        "Latest proof generation time in milliseconds"
    ).unwrap();
    pub static ref LATEST_SUBMIT_TIME_MS: prometheus::IntGauge = prometheus::register_int_gauge!(
        "latest_submit_time_ms",
        "Latest proof submit time in milliseconds"
    ).unwrap();
    pub static ref LATEST_PROVING_CYCLES: prometheus::IntGauge = prometheus::register_int_gauge!(
        "latest_proving_cycles",
        "Latest proof generation cycles"
    ).unwrap();
    pub static ref LATEST_RECEIVED_TIME_MS: prometheus::IntGauge = register_int_gauge!(
        "latest_received_input_time_ms",
        "Latest time (milliseconds) to receive and save input file"
    ).unwrap();
    pub static ref LATEST_TIME_TO_INPUT_MS: prometheus::IntGauge = register_int_gauge!(
        "latest_time_to_input_ms",
        "Latest time (milliseconds) elapsed from block timestamp to time where input was received and saved"
    ).unwrap();
    pub static ref LATEST_BLOCK_TIMESTAMP: prometheus::IntGauge = register_int_gauge!(
        "latest_block_timestamp",
        "Latest timestamp (seconds) when block was queued"
    ).unwrap();
    pub static ref LATEST_BLOCK_NUMBER: prometheus::IntGauge = register_int_gauge!(
        "latest_block_number",
        "Latest block number processed"
    ).unwrap();
    pub static ref PROOF_SUCCESS_TOTAL: prometheus::IntCounter = prometheus::register_int_counter!(
        "proof_success_total",
        "Total number of successful proofs"
    ).unwrap();
    pub static ref PROOF_FAILURE_TOTAL: prometheus::IntCounter = prometheus::register_int_counter!(
        "proof_failure_total",
        "Total number of failed proofs"
    ).unwrap();
    pub static ref INPUT_FILE_ERROR_TOTAL: prometheus::IntCounter = prometheus::register_int_counter!(
        "input_file_error_total",
        "Total number of input file errors"
    ).unwrap();
    pub static ref INPUT_TIME_MAX_MS: prometheus::IntGauge = prometheus::register_int_gauge!(
        "input_time_max_ms",
        "Maximum input file generation time in ms"
    ).unwrap();
    pub static ref INPUT_TIME_MIN_MS: prometheus::IntGauge = prometheus::register_int_gauge!(
        "input_time_min_ms",
        "Minimum input file generation time in ms"
    ).unwrap();
    pub static ref INPUT_TIME_AVG_MS: prometheus::IntGauge = prometheus::register_int_gauge!(
        "input_time_avg_ms",
        "Average input file generation time in ms"
    ).unwrap();
}

/// Prune gauge to keep only the last N block labels
// Removed prune_gauge_last_n and all label logic

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

pub async fn start_metrics_server(state: AppState) -> Result<()> {
    let metrics_addr: SocketAddr = format!("0.0.0.0:{}", state.metrics_port)
        .parse()
        .map_err(|e| anyhow!("Invalid metrics bind address, error: {e}"))?;

    let metrics_app = Router::new()
        .route("/metrics", get(metrics_handler));

    info!("Metrics server running at http://{}", metrics_addr);

    let metrics_server = async move {
        axum::serve(tokio::net::TcpListener::bind(metrics_addr).await?, metrics_app)
            .await
            .map_err(|e| anyhow!("Metrics server error: {e}"))
    };

    tokio::try_join!(metrics_server)?;

    Ok(())
}