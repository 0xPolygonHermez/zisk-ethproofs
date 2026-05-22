use std::net::SocketAddr;

use anyhow::{anyhow, Result};
use axum::routing::get;
use axum::{http::StatusCode, response::IntoResponse, Router};
use lazy_static::lazy_static;
use log::info;
use prometheus::register_histogram_vec;
use prometheus::register_int_counter;
use prometheus::HistogramVec;
use prometheus::IntCounter;
use prometheus::{register_int_gauge, Encoder, TextEncoder};

use crate::state::AppState;

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
    pub static ref TIME_TO_INPUT_HIST: HistogramVec = register_histogram_vec!(
        "time_to_input_hist",
        "Histogram of time to input per block",
        &[],
        vec![1000.0, 2000.0, 3000.0, 4000.0, 5000.0, 6000.0, 7000.0, 8000.0, 9000.0, 10000.0, 11000.0, 12000.0, 13000.0, 14000.0, 15000.0]
    ).unwrap();
    pub static ref TIME_TO_PROOF_HIST: HistogramVec = register_histogram_vec!(
        "time_to_proof_hist",
        "Histogram of time to proof per block",
        &[],
        vec![1000.0, 2000.0, 3000.0, 4000.0, 5000.0, 6000.0, 7000.0, 8000.0, 9000.0, 10000.0, 11000.0, 12000.0, 13000.0, 14000.0, 15000.0]
    ).unwrap();
    pub static ref PROVING_TIME_HIST: HistogramVec = register_histogram_vec!(
        "proving_time_hist",
        "Histogram of proving time per block",
        &[],
        vec![1000.0, 2000.0, 3000.0, 4000.0, 5000.0, 6000.0, 7000.0, 8000.0, 9000.0, 10000.0, 11000.0, 12000.0, 13000.0, 14000.0, 15000.0]
    ).unwrap();

    // Counters for block processing
    pub static ref TIME_TO_PROOF_UNDER_12S_TOTAL: IntCounter = register_int_counter!(
        "time_to_proof_under_12s_total",
        "Total number of blocks with time to proof under 12s"
    ).unwrap();
    pub static ref TIME_TO_PROOF_OVER_12S_TOTAL: IntCounter = register_int_counter!(
        "time_to_proof_over_12s_total",
        "Total number of blocks with time to proof over 12s"
    ).unwrap();
    pub static ref PROVING_UNDER_12S_TOTAL: IntCounter = register_int_counter!(
        "proving_under_12s_total",
        "Total number of blocks proved in under 12s"
    ).unwrap();
    pub static ref PROVING_OVER_12S_TOTAL: IntCounter = register_int_counter!(
        "proving_over_12s_total",
        "Total number of blocks proved in over 12s"
    ).unwrap();
    pub static ref BLOCKS_MISSING_TOTAL: IntCounter = register_int_counter!(
        "blocks_missing_total",
        "Total number of missing blocks (not received)"
    ).unwrap();
    pub static ref BLOCKS_RECEIVED_TOTAL: IntCounter = register_int_counter!(
        "blocks_received_total",
        "Total number of blocks received"
    ).unwrap();

}

/// Prune gauge to keep only the last N block labels
// Removed prune_gauge_last_n and all label logic

/// Ensure cumulative under/over-12s counters appear in /metrics output by
/// incrementing them by zero on startup.
pub fn init_counters() {
    TIME_TO_PROOF_UNDER_12S_TOTAL.inc_by(0);
    TIME_TO_PROOF_OVER_12S_TOTAL.inc_by(0);
    PROVING_UNDER_12S_TOTAL.inc_by(0);
    PROVING_OVER_12S_TOTAL.inc_by(0);
}

/// Number of missing blocks since the previously reported LATEST_BLOCK_NUMBER.
/// Must be called before updating LATEST_BLOCK_NUMBER for the current block.
fn compute_missing_diff(current_block: u64) -> u64 {
    let previous_block = LATEST_BLOCK_NUMBER.get() as u64;
    if current_block > previous_block && previous_block != 0 {
        current_block - previous_block - 1
    } else {
        0
    }
}

/// Update all per-block latest gauges from the given metrics entry and
/// increment BLOCKS_MISSING_TOTAL / BLOCKS_RECEIVED_TOTAL accordingly.
fn publish_block_gauges(metrics: &BlockMetrics) {
    let diff = compute_missing_diff(metrics.block_number);
    LATEST_BLOCK_NUMBER.set(metrics.block_number as i64);
    LATEST_RECEIVED_TIME_MS.set(metrics.received_time_ms);
    LATEST_TIME_TO_INPUT_MS.set(metrics.time_to_input_ms);
    LATEST_MGAS.set(metrics.mgas as i64);
    LATEST_TX_COUNT.set(metrics.tx_count as i64);
    LATEST_PROVING_TIME_MS.set(metrics.proving_time_ms.unwrap_or(0));
    LATEST_PROVING_CYCLES.set(metrics.proving_cycles.unwrap_or(0));
    LATEST_SUBMIT_TIME_MS.set(metrics.submit_time_ms.unwrap_or(0));
    LATEST_BLOCK_TIMESTAMP.set(metrics.timestamp);
    BLOCKS_MISSING_TOTAL.inc_by(diff);
    BLOCKS_RECEIVED_TOTAL.inc();
}

/// Reset all per-block latest gauges to zero (except LATEST_BLOCK_NUMBER, which
/// is set to the given block number) and increment BLOCKS_MISSING_TOTAL /
/// BLOCKS_RECEIVED_TOTAL accordingly.
fn publish_empty_block_gauges(block_number: u64) {
    let diff = compute_missing_diff(block_number);
    LATEST_BLOCK_NUMBER.set(block_number as i64);
    LATEST_BLOCK_TIMESTAMP.set(0);
    LATEST_SUBMIT_TIME_MS.set(0);
    LATEST_MGAS.set(0);
    LATEST_TX_COUNT.set(0);
    LATEST_PROVING_TIME_MS.set(0);
    LATEST_PROVING_CYCLES.set(0);
    LATEST_TIME_TO_INPUT_MS.set(0);
    LATEST_RECEIVED_TIME_MS.set(0);
    BLOCKS_RECEIVED_TOTAL.inc();
    BLOCKS_MISSING_TOTAL.inc_by(diff);
}

/// Publish metrics for a successfully proved block: update latest gauges, push
/// histogram observations and increment success / under-over-12s counters.
pub fn publish_proof_success(metrics: &BlockMetrics, proving_time_ms: u64) {
    publish_block_gauges(metrics);
    PROOF_SUCCESS_TOTAL.inc();

    let time_to_proof = metrics.time_to_input_ms + proving_time_ms as i64;

    TIME_TO_INPUT_HIST
        .with_label_values(&[] as &[&str])
        .observe(metrics.time_to_input_ms as f64);

    TIME_TO_PROOF_HIST
        .with_label_values(&[] as &[&str])
        .observe(time_to_proof as f64);
    if time_to_proof <= 12000 {
        TIME_TO_PROOF_UNDER_12S_TOTAL.inc();
    } else {
        TIME_TO_PROOF_OVER_12S_TOTAL.inc();
    }

    PROVING_TIME_HIST
        .with_label_values(&[] as &[&str])
        .observe(proving_time_ms as f64);
    if proving_time_ms <= 12000 {
        PROVING_UNDER_12S_TOTAL.inc();
    } else {
        PROVING_OVER_12S_TOTAL.inc();
    }
}

/// Publish failure metrics when a BlockMetrics entry is available.
pub fn publish_proof_failure_with_metrics(metrics: &BlockMetrics) {
    PROOF_FAILURE_TOTAL.inc();
    publish_block_gauges(metrics);
}

/// Publish failure metrics when no BlockMetrics entry exists for the block.
pub fn publish_proof_failure_no_metrics(block_number: u64) {
    PROOF_FAILURE_TOTAL.inc();
    publish_empty_block_gauges(block_number);
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

pub async fn start_metrics_server(state: AppState) -> Result<()> {
    let metrics_addr: SocketAddr = format!("0.0.0.0:{}", state.cliargs.metrics.port)
        .parse()
        .map_err(|e| anyhow!("Invalid metrics bind address, error: {e}"))?;

    let metrics_app = Router::new().route("/metrics", get(metrics_handler));

    info!("Metrics server running at http://{}", metrics_addr);

    let metrics_server = async move {
        axum::serve(tokio::net::TcpListener::bind(metrics_addr).await?, metrics_app)
            .await
            .map_err(|e| anyhow!("Metrics server error: {e}"))
    };

    tokio::try_join!(metrics_server)?;

    Ok(())
}
