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
const OUTPUT_FOLDER: &str = "output";
const PROGRAM_FOLDER: &str = "elf";
const LOG_FOLDER: &str = "log";
const DEFAULT_UPLOAD_FOLDER: &str = "upload_inputs";

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

    let upload_folder = env::var("UPLOAD_FOLDER").unwrap_or(DEFAULT_UPLOAD_FOLDER.to_string());       
    // Ensure output directory exists
    create_dir_all(&upload_folder).await.unwrap();

    let input_gen_server_url = env::var("INPUT_GEN_SERVER_URL").expect("INPUT_GEN_SERVER_URL must be set");
    // Connect to the input generator server
    info!("Connecting to input-gen-server at {}", input_gen_server_url);
    let (ws_stream, _) = connect_async(input_gen_server_url.clone())
        .await
        .expect("Failed to connect to WebSocket server");

    info!("Connected to input-gen-server");
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
                    let filepath = PathBuf::from(&upload_folder).join(filename);
                    match fs::write(&filepath, content) {
                        Ok(_) => info!("Received and saved input file: {}, time: {}ms", filepath.display(), queued_start.elapsed().as_millis()),
                        Err(e) => {
                            error!("Failed to save input file {}, error: {}", filepath.display(), e);
                            continue;
                        }
                    }

                    // Get block number from filename
                    let block_number = match filepath.file_stem() {
                        Some(stem) => match stem.to_string_lossy().parse::<u64>() {
                            Ok(num) => num,
                            Err(_) => {
                                error!("Failed to parse block number from input filename {}", filepath.display());
                                continue;
                            }
                        }
                        None => {
                            error!("Failed to get block number from filename {}", filepath.display());
                            continue;
                        }
                    };

                    // Report to EthProof that we are generating the proof
                    if ethproofs_submit {
                        ethproofs_client.proof_proving(ethproofs_cluster_id, block_number).await?;
                    }

                    info!("Generating proof for block number {}", block_number);
                    let result = generate_proof(block_number, args.disable_distributed, upload_folder.clone()).await?;
                    info!("Proof generated for block number {}, proving_time: {}s, cycles: {}", block_number, result.time / 1000, result.cycles);

                    // Submit the proof to EthProofs
                    let proof_base64 = get_proof_b64(block_number)?;
                    if ethproofs_submit {
                        ethproofs_client.proof_proved(ethproofs_cluster_id, block_number, result.time, result.cycles, proof_base64, result.id).await?;
                        info!("Proof submitted to ethproofs for block number {}", block_number);
                    }

                    // Send success alert to Telegram if enabled
                    if args.block_submit_alert {
                        // TODO: Log txs and mgas for the block
                        send_telegram_alert(
                            &format!("Proof submitted for block number {}, cycles: {}, proving_time: {}s",
                            block_number, result.cycles, result.time / 1000),
                            AlertType::Success
                        ).await?;
                    }     

                    let proof_folder = format!("{}/{}", OUTPUT_FOLDER, block_number);

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

    Ok(())
}