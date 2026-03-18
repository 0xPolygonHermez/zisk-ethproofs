use anyhow::{anyhow, Result};
use log::{debug, error, info};
use zisk_distributed_grpc_api::{
    zisk_distributed_api_client::ZiskDistributedApiClient, LaunchProofRequest,
};
use std::collections::HashMap;

use crate::state::AppState;
use ethproofs_common::protocol::BlockInfo;

pub async fn generate_proof(block_info: BlockInfo, state: AppState) -> Result<String> {
    let block_number = block_info.block_number;

    info!("🔄 Generating proof for block {}", block_number);

    debug!(
        "Sending coordinator request for block {} with {} compute units",
        block_number, state.compute_capacity
    );

    let start = std::time::Instant::now();

    // Get gRPC client
    let mut client = ZiskDistributedApiClient::new(state.coordinator_channel.clone().unwrap());

    #[cfg(zisk_hints)]
    let (input_mode, inputs_uri, hints_mode, hints_uri) =
        if state.cliargs.hints == crate::cliargs::Hints::Socket {
            (0, None, 2, Some(format!("unix://{}", state.cliargs.hints_socket)))
        } else {
            // TODO: Add arg to specify hints directory
            (0, None, 1, Some(format!("./hints/{}_hints.bin", block_number)))
        };

    #[cfg(not(zisk_hints))]
    let (input_mode, inputs_uri, hints_mode, hints_uri) = {
        let filepath = std::path::PathBuf::from(&state.inputs_folder)
            .join(&block_info.filename())
            .to_string_lossy()
            .to_string();
        
        (2, Some(filepath), 0, None)
    };

    let metadata = HashMap::from([
        ("Block Number".to_string(), block_number.to_string()),
    ]);

    let launch_proof_request = LaunchProofRequest {
        data_id: block_number.to_string(),
        compute_capacity: state.compute_capacity,
        minimal_compute_capacity: state.compute_capacity,
        inputs_uri,
        inputs_mode: input_mode,
        hints_mode,
        hints_uri,
        simulated_node: None,
        execution_only: false,
        metadata,
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
