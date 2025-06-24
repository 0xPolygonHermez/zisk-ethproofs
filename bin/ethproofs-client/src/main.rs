use std::env;
use std::{fs, path::PathBuf};

use anyhow::{Context, Result};
use clap::Parser;
use dotenv::dotenv;
use ethers::providers::{Middleware, Provider, Ws};
use ethproofs_api::EthProofsApi;
use futures_util::StreamExt;
use log::{error, info, warn};
use rand::rand_core::block;
use tokio::fs::create_dir_all;
use tokio_tungstenite::{connect_async, tungstenite::Message};

mod input;
mod prove;
mod telegram;
use input::generate_input_file;
use prove::{generate_proof, get_proof_b64};
use telegram::{send_telegram_alert, AlertType};

// Constants
const INPUT_FOLDER: &str = "input";
const OUTPUT_FOLDER: &str = "output";
const PROGRAM_FOLDER: &str = "program";
const LOG_FOLDER: &str = "log";

const SERVER_URI: &str = "ws://localhost:8765";
const OUTPUT_DIR: &str = "./received_inputs";

// Command line arguments
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

    /// Disable distributed proving
    #[arg(short, long)]
    disable_distributed: bool,

    /// Keep input file
    #[arg(short = 'i', long)]
    keep_input: bool,

    /// Keep ouput folder
    #[arg(short = 'o', long)]
    keep_output: bool,
}

/// Parses a binary WebSocket message into (filename, file_content)
fn parse_message(data: &[u8]) -> Option<(&str, &[u8])> {
    let split_index = data.iter().position(|&b| b == b'\n')?;
    let (name, content) = data.split_at(split_index);
    let content = &content[1..]; // skip newline
    let name_str = std::str::from_utf8(name).ok()?;
    Some((name_str, content))
}

#[tokio::main]
async fn main() -> Result<()> {
    // Load environment variables from .env file
    dotenv().ok();

    // Check if LOG_RUST is set; if not, set it to "info"
    if std::env::var("RUST_LOG").is_err() {
        std::env::set_var("RUST_LOG", "ethproofs_client=info");
    }

    // Initialize the logger
    env_logger::init();

    // Load environment variables from .env file
    dotenv().ok();

    // Parse the command line arguments
    let args = CliArgs::parse();

    // Determine if we should submit proofs to ethproofs
    let ethproofs_submit = !args.no_ethproofs;

    // Initialize the EthProofsApi
    let ethproofs_api_url = env::var("ETHPROOFS_API_URL").expect("ETHPROOFS_API_URL must be set");
    let ethproofs_api_token = env::var("ETHPROOFS_API_TOKEN").expect("ETHPROOFS_API_TOKEN must be set");
    let ethproofs_client = EthProofsApi::new(ethproofs_api_url, ethproofs_api_token);

    // Cluster ID for the EthProofs API
    let ethproofs_cluster_id: u32 = env::var("ETHPROOFS_CLUSTER_ID")
        .expect("ETHPROOFS_CLUSTER_ID must be set")
        .parse()
        .expect("ETHPROOFS_CLUSTER_ID must be a valid u32");

    // Ensure output directory exists
    create_dir_all(OUTPUT_DIR).await.unwrap();

    // Connect to the input generator server
    let (ws_stream, _) = connect_async(SERVER_URI)
        .await
        .expect("Failed to connect to WebSocket server");

    info!("Connected to input-gen-server at {}", SERVER_URI);
    let (_, mut reader) = ws_stream.split();

    let mut queued_start = std::time::Instant::now();
    while let Some(Ok(msg)) = reader.next().await {
        match msg {
            Message::Text(command) => {
                let args = command.split_whitespace().collect::<Vec<&str>>();
                if args.is_empty() {
                    error!("Received empty command");
                    continue;
                }

                match args[0] {
                    "queued" => {
                        queued_start = std::time::Instant::now();
                        
                        let block_number: u64 = args[1]
                            .parse()
                            .expect("Failed to parse block number from command queued");

                        info!("Received queued command for block {}", block_number);

                        // Report to EthProofs that the proof is queued while generating the input file
                        if ethproofs_submit {
                            ethproofs_client.proof_queued(ethproofs_cluster_id, block_number).await?;
                        }                        
                    }
                    _ => {
                        error!("Unknown command received: {}", command);
                    }
                }
            }
            Message::Binary(payload) => {
                if let Some((filename, content)) = parse_message(&payload) {
                    let filepath = PathBuf::from(OUTPUT_DIR).join(filename);
                    match fs::write(&filepath, content) {
                        Ok(_) => info!("Received and saved input file: {}, time: {}ms", filepath.display(), queued_start.elapsed().as_millis()),
                        Err(e) => {
                            error!("Failed to save input file {}: {}", filepath.display(), e);
                            continue;
                        }
                    }

                    // Get block number from filename
                    let block_number = filepath.file_stem().unwrap().to_string_lossy().parse().map_err(|e| {
                        error!("Failed to parse block number from filename {}, error: {}", filepath.display(), e);
                        continue;
                    });

                    // Report to EthProof that we are generating the proof
                    if ethproofs_submit {
                        ethproofs_client.proof_proving(ethproofs_cluster_id, block_number).await?;
                    }

                    info!("Generating proof for block number {}", block_number);
                    // let result = generate_proof(block_number, args.disable_distributed).await?;
                    // info!("Proof generated for block number {}, proving_time: {}s, cycles: {}", block_number, result.time / 1000, result.cycles);

                    // // Submit the proof to EthProofs
                    let proof_base64 = get_proof_b64(block_number)?;
                    if ethproofs_submit {
                        ethproofs_client.proof_proved(ethproofs_cluster_id, block_number, result.time, result.cycles, proof_base64, result.id).await?;
                        info!("Proof submitted to ethproofs for block number {}", block_number);
                    }

                    // Send success alert to Telegram if enabled
                    // if args.block_submit_alert {
                    //     send_telegram_alert(
                    //         &format!("Proof submitted for block number {}, txs: {}, gas: {}, cycles: {}, proving_time: {}s",
                    //         block_number, block.transactions.len(), block.gas_used, result.cycles, result.time / 1000),
                    //         AlertType::Success
                    //     ).await?;
                    // }     

                    // Clean up
                    if !args.keep_output {
                        if let Err(e) = std::fs::remove_dir_all(&proof_folder) {
                            warn!("Failed to remove proof folder {}, error: {}", proof_folder, e);
                        }
                    }
                    if !args.keep_input {
                        if let Err(e) = std::fs::remove_file(&filepath) {
                            warn!("Failed to remove input file {}, error: {}", filepath.to_string_lossy(), e);
                        }
                    }                                   
                } else {
                    error!("Malformed message received");
                }
            }
            _ => {
                error!("Received unsupported message type: {:?}", msg);
            }
        }
    }

    info!("Server connection closed.");

    // loop {
    //     let mut block_number: u64 = 0;
    //     let rpc_provider = Provider::<Ws>::connect(rpc_ws_url.clone()).await.context("Failed to connect to WS RPC provider")?;

    //     if args.test_block.is_none() {
    //         // Subscribe to new block headers on Ethereum Mainnet
    //         let mut stream = rpc_provider.subscribe_blocks().await?;

    //         info!("Listening for new blocks on Ethereum Mainnet...");

    //         // Get new blocks from Ethereum Mainnet
    //         while let Some(block) = stream.next().await {
    //             block_number = block.number.unwrap().as_u64();
    //             if block_number % block_modulus == 0 {
    //                 // Block selected to be proved
    //                 break;
    //             } else {
    //                 info!("Skipping block number: {}", block_number);
    //                 continue;
    //             }
    //         }
    //     } else {
    //         block_number = args.test_block.unwrap();
    //     }
        
    //     // Get proof folder and input file path for the block
    //     let proof_folder = format!("{}/{}", OUTPUT_FOLDER, block_number);
    //     let input_file = format!("{}/{}.bin", INPUT_FOLDER, block_number);

    //     let result = (|| async {
    //         // Get block data
    //         let block = rpc_provider
    //             .get_block(block_number).await?
    //             .ok_or_else(|| anyhow::anyhow!("Block {} not found", block_number))?;

    //         // Report to EthProofs that the proof is queued while generating the input file
    //         if ethproofs_submit {
    //             ethproofs_client.proof_queued(ethproofs_cluster_id, block_number).await?;
    //         }
    //         info!("Generating input file for block number {}, txs: {}, gas: {}", block_number, block.transactions.len(), block.gas_used);
    //         let input_file_time = generate_input_file(block_number).await?;
    //         info!("Input file generated for block number {}, time: {}s", block_number, input_file_time / 1000);

    //         // Report to EthProof that we are generating the proof
    //         if ethproofs_submit {
    //             ethproofs_client.proof_proving(ethproofs_cluster_id, block_number).await?;
    //         }

    //         info!("Generating proof for block number {}", block_number);
    //         let result = generate_proof(block_number, args.disable_distributed).await?;
    //         info!("Proof generated for block number {}, proving_time: {}s, cycles: {}", block_number, result.time / 1000, result.cycles);

    //         // Submit the proof to EthProofs
    //         let proof_base64 = get_proof_b64(block_number)?;
    //         if ethproofs_submit {
    //             ethproofs_client.proof_proved(ethproofs_cluster_id, block_number, result.time, result.cycles, proof_base64, result.id).await?;
    //         }

    //         info!("Proof submitted to ethproofs for block number {}", block_number);
    //         if args.block_submit_alert {
    //             send_telegram_alert(
    //                 &format!("Proof submitted for block number {}, txs: {}, gas: {}, cycles: {}, proving_time: {}s",
    //                 block_number, block.transactions.len(), block.gas_used, result.cycles, result.time / 1000),
    //                 AlertType::Success
    //             ).await?;
    //         }
            
    //         Ok::<(), anyhow::Error>(())
    //     })().await;

    //     // Clean up
    //     if !args.keep_output {
    //         if let Err(e) = std::fs::remove_dir_all(&proof_folder) {
    //             warn!("Failed to remove proof folder {}, error: {}", proof_folder, e);
    //         }
    //     }
    //     if !args.keep_input {
    //         if let Err(e) = std::fs::remove_file(&input_file) {
    //             warn!("Failed to remove input file {}, error: {}", input_file, e);
    //         }
    //     }
    
    //     // Handle errors
    //     if let Err(e) = result {
    //         let msg = format!("Error processing block {}, error: {}", block_number, e);
    //         error!("{}", msg);
    //         send_telegram_alert(&msg, AlertType::Error).await?;
    //     }
        
    //     // Exit if we are testing a block
    //     if args.test_block.is_some() {
    //         break;
    //     }
    // }

    Ok(())
}