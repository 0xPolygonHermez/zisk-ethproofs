use std::sync::Arc;

use anyhow::{anyhow, Result};
use log::{debug, error, info};
use zisk_distributed_grpc_api::{
    zisk_distributed_api_client::ZiskDistributedApiClient, LaunchProofRequest,
};

use crate::{prove, state::AppState};
use ethproofs_common::protocol::BlockInfo;

pub async fn generate_proof(block_info: BlockInfo, state: AppState) -> Result<String> {
    let block_number = block_info.block_number;

    info!("🔄 Generating proof for block {}", block_number);

    let prover_client_clone = Arc::clone(&state.prover_client);
    let mut prover_client = prover_client_clone.lock().unwrap();

    prover_client
        .generate_proof(block_info.clone())
        .await
        .map_err(|e| anyhow!("Failed to generate proof for block {}: {}", block_number, e))?;

    // Report to EthProofs that we are proving this block
    if let Some(client) = state.ethproofs_client {
        tokio::spawn(async move {
            let start = std::time::Instant::now();
            match client.proof_proving(state.ethproofs_cluster_id.unwrap(), block_number).await {
                Ok(_) => {
                    info!(
                        "Reported proving state to EthProofs for block {}, request_time: {} ms",
                        block_number,
                        start.elapsed().as_millis()
                    );
                }
                Err(e) => {
                    error!(
                        "Failed to report proving state to EthProofs for block {}, error: {}",
                        block_number, e
                    );
                }
            }
        });
    }

    Ok("".to_string())
}
