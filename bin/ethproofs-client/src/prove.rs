use anyhow::{anyhow, Ok, Result};
use log::{debug, info};
use tonic::transport::Channel;
use zisk_distributed_grpc_api::{
    zisk_distributed_api_client::ZiskDistributedApiClient, LaunchProofRequest,
};

use crate::state::AppState;

pub async fn generate_proof(block_number: u64, state: AppState) -> Result<String> {
    info!("🔄 Generating proof for block number {}", block_number);

    // Report to EthProofs that we are generating the proof
    if let Some(client) = state.ethproofs_client {
        let start = std::time::Instant::now();
        client.proof_proving(state.ethproofs_cluster_id.unwrap(), block_number).await?;
        debug!(
            "Report proving state for block number {}, request_time: {} ms",
            block_number,
            start.elapsed().as_millis()
        );
    }

    // Prepare input file
    let input_file = format!("{}.bin", block_number);

    // Connect to the coordinator
    debug!("Connecting to coordinator on {}", state.coordinator_url);

    let channel = Channel::from_shared(state.coordinator_url)?.connect().await?;
    let mut client = ZiskDistributedApiClient::new(channel);

    // Build request
    let launch_proof_request = LaunchProofRequest {
        block_id: block_number.to_string(),
        compute_capacity: state.compute_capacity,
        input_path: input_file,
        simulated_node: None,
    };

    // Make the coordinator prove request
    info!(
        "Sending coordinator request for block {} with {} compute units",
        block_number, state.compute_capacity
    );
    let response = client.launch_proof(launch_proof_request).await?;

    // Handle response
    match response.into_inner().result {
        Some(zisk_distributed_grpc_api::launch_proof_response::Result::JobId(job_id)) => {
            info!("Proof generation started for block number {}, job_id: {}", block_number, job_id);
            Ok(job_id)
        }
        Some(zisk_distributed_grpc_api::launch_proof_response::Result::Error(error)) => {
            return Err(anyhow!("Proof job failed: {} - {}", error.code, error.message));
        }
        None => {
            return Err(anyhow!("Received empty response from coordinator"));
        }
    }
}
