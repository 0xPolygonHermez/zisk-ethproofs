use std::sync::Arc;

use anyhow::{anyhow, Result};
use log::{debug, error, info};
use zisk_sdk::ExecutorKind;

use crate::{prove, state::AppState};
use ethproofs_common::protocol::BlockInfo;

pub async fn generate_proof(block_info: BlockInfo, state: AppState) -> Result<String> {
    let block_number = block_info.block_number;

    info!("🔄 Generating proof for block {}", block_number);

    let prover_client_clone = Arc::clone(&state.prover_client);
    let prover_client = prover_client_clone.lock().unwrap();
    let guest_program_clone = Arc::clone(&state.guest_program);
    let guest_program = guest_program_clone.lock().unwrap();
    let stdin = state.zisk_stdin.lock().unwrap().take().ok_or_else(|| {
        anyhow!("ZiskStdin not available for block {}", block_number)
    })?;

    let result = prover_client.prove(&guest_program, stdin.stdin).executor(ExecutorKind::Assembly).run()?.await?;
    println!("Proof generated successfully in {:?}", result.get_proving_time());
    println!("Execution steps: {}", result.get_execution_steps());

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
