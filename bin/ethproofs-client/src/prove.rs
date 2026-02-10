use anyhow::{anyhow, Result};
use log::{debug, error, info};
use zisk_distributed_grpc_api::{
    zisk_distributed_api_client::ZiskDistributedApiClient, LaunchProofRequest,
};

use crate::state::AppState;
use ethproofs_common::protocol::BlockInfo;
use std::path::PathBuf;

pub async fn generate_proof(block_info: BlockInfo, state: AppState) -> Result<String> {
    let block_number = block_info.block_number;

    info!("🔄 Generating proof for block {}", block_number);

    // Get input file name
    let input_file = block_info.filename();

    let filepath =
        PathBuf::from(&state.inputs_folder).join(&input_file).to_string_lossy().to_string();

    debug!(
        "Sending coordinator request for block {} with {} compute units",
        block_number, state.compute_capacity
    );

    let start = std::time::Instant::now();

    // Get gRPC client
    let mut client = ZiskDistributedApiClient::new(state.coordinator_channel.clone().unwrap());

    #[cfg(zisk_hints)]
    // Build request
    let (hints_mode, hints_uri) = if state.cliargs.hints == crate::cliargs::Hints::Socket {
        (2, Some(state.cliargs.hints_socket.clone()))
    } else {
        (0, None)
    };

    #[cfg(not(zisk_hints))]
    let (hints_mode, hints_uri) = (0, None);

    let launch_proof_request = LaunchProofRequest {
        data_id: block_number.to_string(),
        compute_capacity: state.compute_capacity,
        minimal_compute_capacity: state.compute_capacity,
        inputs_uri: Some(filepath),
        inputs_mode: 2,
        hints_mode,
        hints_uri,
        simulated_node: None,
    };

    info!("launch_proof_request: {:?}", launch_proof_request);

    // Send the coordinator prove request
    let response = client.launch_proof(launch_proof_request).await?;

    // Handle coordinator response
    let job_id = match response.into_inner().result {
        Some(zisk_distributed_grpc_api::launch_proof_response::Result::JobId(job_id)) => {
            info!(
                "Proof generation started for block {}, job_id: {}, request time: {} ms",
                block_number,
                job_id,
                start.elapsed().as_millis()
            );
            job_id
        }
        Some(zisk_distributed_grpc_api::launch_proof_response::Result::Error(error)) => {
            return Err(anyhow!(
                "Coordinator proof request failed: {} - {}",
                error.code,
                error.message
            ));
        }
        None => {
            return Err(anyhow!("Received empty response from coordinator"));
        }
    };

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

    Ok(job_id)
}
