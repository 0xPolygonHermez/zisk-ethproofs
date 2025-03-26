use std::env;

use anyhow::Result;
use clap::Parser;
use dotenv::dotenv;
use ethers::providers::{Middleware, Provider, Ws};
use ethproofs_client::EthProofsClient;
use futures_util::StreamExt;
use log::{error, info, warn};

mod input;
mod prove;
mod telegram;
use input::generate_input_file;
use prove::{generate_proof, get_proof_b64};
use telegram::{send_telegram_alert, AlertType};

const INPUT_FOLDER: &str = "input";
const PROOF_FOLDER: &str = "proof";
const PROGRAM_FOLDER: &str = "program";
const LOG_FOLDER: &str = "log";

#[derive(Parser)]
struct CliArgs {
    /// Disable ethproofs integration, only prove blocks
    #[arg(short, long)]
    no_ethproofs: bool,

    /// Send telegram alert when block is submitted to ethproofs
    #[arg(short, long)]
    block_submit_alert: bool,

    /// Test block number
    #[arg(short, long)]
    test_block: Option<u64>,
}

#[tokio::main]
async fn main() -> Result<()> {
    // Load environment variables from .env file
    dotenv().ok();

    // Check if LOG_RUST is set; if not, set it to "info"
    if std::env::var("RUST_LOG").is_err() {
        std::env::set_var("RUST_LOG", "info");
    }

    // Initialize the logger using the LOG_RUST variable
    env_logger::init();

    // Load environment variables from .env file
    dotenv().ok();

    // Parse the command line arguments
    let args = CliArgs::parse();

    let ethproofs_submit = !args.no_ethproofs;

    // Modulus used to select the blocks to prove
    let block_modulus: u64 = env::var("BLOCK_MODULUS").unwrap_or("100".to_string()).parse().unwrap();

    // RPC WebSocket URL
    let rpc_ws_url = env::var("RPC_WS_URL").expect("RPC_WS_URL must be set");

    // Initialize the EthProofsClient
    let ethproofs_api_url = env::var("ETHPROOFS_API_URL").expect("ETHPROOFS_API_URL must be set");
    let ethproofs_api_token = env::var("ETHPROOFS_API_TOKEN").expect("ETHPROOFS_API_TOKEN must be set");
    let ethproofs_client = EthProofsClient::new(ethproofs_api_url, ethproofs_api_token);

    // Cluster ID for the EthProofs API
    let ethproofs_cluster_id: u32 = env::var("ETHPROOFS_CLUSTER_ID")
        .expect("ETHPROOFS_CLUSTER_ID must be set")
        .parse()
        .expect("ETHPROOFS_CLUSTER_ID must be a valid u32");

    loop {
        let mut block_number: u64 = 0;
        let rpc_provider = Provider::<Ws>::connect(rpc_ws_url.clone()).await?;

        if args.test_block.is_none() {
            // Subscribe to new block headers on Ethereum Mainnet
            let mut stream = rpc_provider.subscribe_blocks().await?;

            info!("Listening for new blocks on Ethereum Mainnet...");

            // Get new blocks from Ethereum Mainnet
            while let Some(block) = stream.next().await {
                block_number = block.number.unwrap().as_u64();
                if block_number % block_modulus == 0 {
                    break;
                } else {
                    info!("Skipping block number: {}", block_number);
                    continue;
                }
            }
        } else {
            block_number = args.test_block.unwrap();
        }
        
        // Guardamos rutas por adelantado
        let proof_folder = format!("{}/{}", PROOF_FOLDER, block_number);
        let input_file = format!("{}/{}.bin", INPUT_FOLDER, block_number);

        let result = (|| async {
            let block = rpc_provider
                .get_block(block_number).await?
                .ok_or_else(|| anyhow::anyhow!("Block {} not found", block_number))?;

            // Proof queued while generating the input file
            if ethproofs_submit {
                ethproofs_client.proof_queued(ethproofs_cluster_id, block_number).await?;
            }
            info!("Generating input file for block number {}, txs: {}, gas: {}", block_number, block.transactions.len(), block.gas_used);
            let input_file_time = generate_input_file(block_number).await?;
            info!("Input file generated for block number {}, time: {}s", block_number, input_file_time / 1000);

            // Generate the proof
            if ethproofs_submit {
                ethproofs_client.proof_proving(ethproofs_cluster_id, block_number).await?;
            }

            info!("Generating proof for block number {}", block_number);
            let (proving_time, cycles) = generate_proof(block_number).await?;
            info!("Proof generated for block number {}, proving_time: {}s, cycles: {}", block_number, proving_time / 1000, cycles);

            // Submit the proof to EthProofs
            let proof_base64 = get_proof_b64(block_number)?;
            if ethproofs_submit {
                ethproofs_client.proof_proved(ethproofs_cluster_id, block_number, proving_time, cycles, proof_base64, "verifier".to_string()).await?;
            }

            info!("Proof submitted for block number {}", block_number);
            if args.block_submit_alert {
                send_telegram_alert(
                    &format!("Proof submitted for block number {}, txs: {}, gas: {}, cycles: {}, proving_time: {}",
                    block_number, block.transactions.len(), block.gas_used, cycles, proving_time / 1000),
                    AlertType::Success
                ).await?;
            }
            
            Ok::<(), anyhow::Error>(())
        })().await;

        // Clean up
        if let Err(e) = std::fs::remove_dir_all(&proof_folder) {
            warn!("Failed to remove proof folder {}, error: {}", proof_folder, e);
        }
        if let Err(e) = std::fs::remove_file(&input_file) {
            warn!("Failed to remove input file {}, error: {}", input_file, e);
        }
    
        // Handle errors
        if let Err(e) = result {
            let msg = format!("Error processing block {}, error: {}", block_number, e);
            error!("{}", msg);
            send_telegram_alert(&msg, AlertType::Error).await?;
        }
        
        // Exit if we are testing a block
        if args.test_block.is_some() {
            break;
        }
    }

    Ok(())
}