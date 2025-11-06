use std::net::SocketAddr;

use anyhow::{anyhow, Result};
use axum::{http::StatusCode, response::IntoResponse, Router};
use log::{info};
use prometheus::{Encoder, TextEncoder, register_int_gauge_vec, IntGaugeVec};
use lazy_static::lazy_static;
use axum::{routing::get};

use crate::{state::AppState};

// Prometheus metrics
lazy_static! {
    pub static ref PROVING_TIME_GAUGE: IntGaugeVec = register_int_gauge_vec!(
        "proving_time_ms",
        "Proof generation time in milliseconds",
        &["block"]
    ).unwrap();
    pub static ref SUBMIT_TIME_GAUGE: IntGaugeVec = register_int_gauge_vec!(
        "submit_time_ms",
        "Proof submit time in milliseconds",
        &["block"]
    ).unwrap();
    pub static ref PROVING_CYCLES_GAUGE: IntGaugeVec = register_int_gauge_vec!(
        "proving_cycles",
        "Proof generation cycles",
        &["block"]
    ).unwrap();
    pub static ref RECEIVED_TIME_GAUGE: IntGaugeVec = register_int_gauge_vec!(
        "received_input_time_ms",
        "Time (milliseconds) to receive and save input file",
        &["block"]
    ).unwrap();
    pub static ref TIME_TO_INPUT_GAUGE: IntGaugeVec = register_int_gauge_vec!(
        "time_to_input_ms",
        "Time (milliseconds) elapsed from block timestamp to time where input was received and saved",
        &["block"]
    ).unwrap();
    pub static ref BLOCK_TIMESTAMP_GAUGE: IntGaugeVec = register_int_gauge_vec!(
        "block_timestamp",
        "Timestamp (seconds) when block was queued",
        &["block"]
    ).unwrap();
}

/// Prune gauge to keep only the last N block labels
pub fn prune_gauge_last_n(gauge: &IntGaugeVec, keep_last: usize) {
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