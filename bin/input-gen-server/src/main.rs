use std::io::Write;
use std::{env, path::Path, time::Instant};
use std::{fs, path::PathBuf};

use anyhow::{Context, Result};
use clap::Parser;
use dotenv::dotenv;
use ethers::providers::{Middleware, Provider, Ws};
use futures_util::{SinkExt, StreamExt};
use input::{GuestProgram, Network};
use log::{error, info, warn};
use ethproofs_protocol::{BlockCommand, BlockInfo, BlockMessage};
use tokio::{
    net::{TcpListener, TcpStream},
    sync::broadcast::{self, Receiver, Sender},
    task,
};
use tokio_tungstenite::accept_async_with_config;
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
use tokio_tungstenite::tungstenite::Message;

const WS_LISTEN_IP: &str = "0.0.0.0";
const WS_DEFAULT_PORT: &str = "8765";

#[derive(Debug, Clone, Parser)]
pub struct InputGenServerArgs {
    /// Guest program for which to generate inputs
    #[clap(long, short, value_enum, default_value_t = GuestProgram::Rsp)]
    pub guest: GuestProgram,
}

// BlockCommand, BlockMessage and short_hash now provided by ethproofs-protocol crate

/// Generate the guest input file for the given block number and return the time taken in milliseconds
pub async fn generate_input_file(
    guest: GuestProgram,
    block_info: BlockInfo,
    inputs_folder: String,
) -> Result<u128> {
    let start = Instant::now();
    let block_number = block_info.block_number;

    // Load RPC URL from environment variable
    let rpc_url = env::var("RPC_URL").expect("RPC_URL must be set");

    // Create the inputs folder if it doesn't exist
    let input_folder = Path::new(&inputs_folder);
    std::fs::create_dir_all(input_folder)?;

    let input_path = input_folder.join(block_info.filename());

    let start_input_gen = Instant::now();
    // Use spawn_blocking to handle the non-Send future
    let result = tokio::task::spawn_blocking(move || {
        let rt = tokio::runtime::Handle::current();
        rt.block_on(async move {
            let input_builder =
                input::build_input_generator(guest, &rpc_url, Network::Mainnet);
            input_builder.generate(block_number).await
        })
    })
    .await??;
    info!(
        "Input generation for block {} took {} ms",
        block_number,
        start_input_gen.elapsed().as_millis()
    );

    let save_file_start = Instant::now();
    let mut input_file = std::fs::File::create(&input_path)?;
    input_file.write_all(&result.input)?;
    info!(
        "Saving input file for block {} took {} ms",
        block_number,
        save_file_start.elapsed().as_millis()
    );

    let input_file_time = start.elapsed().as_millis();

    Ok(input_file_time)
}

/// Listens for new blocks on the Ethereum network and generates input files for them
async fn block_listener(guest: GuestProgram, tx: Sender<String>) -> Result<()> {
    let rpc_ws_url = env::var("RPC_WS_URL").expect("RPC_WS_URL must be set");
    let inputs_folder = env::var("INPUTS_FOLDER").unwrap_or("inputs".to_string());
    let block_modulus: u64 = env::var("BLOCK_MODULUS")
        .unwrap_or("100".to_string())
        .parse()
        .expect("BLOCK_MODULUS must be a valid integer");

    let mut max_input_time: u128 = 0;
    let mut min_input_time: u128 = u128::MAX;
    let mut total_input_time: u128 = 0;
    let mut input_count: u64 = 0;

    loop {
        let rpc_provider = Provider::<Ws>::connect(rpc_ws_url.clone())
            .await
            .context("Failed to connect to WS RPC provider")?;

        let mut stream = rpc_provider.subscribe_blocks().await?;
        info!("Listening for new blocks on Ethereum Mainnet...");

        let mut selected_block: Option<ethers::types::Block<ethers::types::TxHash>> = None;
        while let Some(block) = stream.next().await {
            if let Some(number) = block.number {
                let bn = number.as_u64();
                if bn % block_modulus == 0 {
                    selected_block = Some(block);
                    info!("Received block {}, processing...", bn);
                    break;
                } else {
                    info!("Received block {}, skipping...", bn);
                }
            } else {
                warn!("Received block without number, skipping...");
            }
        }

        let block = if let Some(block) = selected_block {
            block
        } else {
            warn!("No block selected for processing, skipping...");
            return Ok(());
        };

        let block_number = if let Some (bn) =block.number {
            bn.as_u64()
        } else {
            warn!("Selected block has no number, skipping...");
            return Ok(());
        };

        if let Err(e) = async {
            // Send queued message as JSON
            let queued_message = BlockMessage::new_queued(block_number);
            let queued_message_json = serde_json::to_string(&queued_message).unwrap();
            if tx.send(queued_message_json).is_err() {
                info!(
                    "No active receivers, skipping input file generation for block {}",
                    block_number
                );
                return Ok::<(), anyhow::Error>(());
            }

            info!("Generating input file for block {}", block_number);

            let block_hash = if let Some(h) = block.hash {
                format!("{:x}", h)
            } else {
                String::from("nohash")
            };

            let input_message = BlockMessage::new_input(
                block_number,
                block.timestamp,
                block_hash,
                block.transactions.len(),
                block.gas_used.as_u64() / 1_000_000,
            );

            let input_file_time =
                generate_input_file(guest.clone(), input_message.clone().info, inputs_folder.clone()).await?;

            let input_message_json = serde_json::to_string(&input_message).unwrap();

            total_input_time += input_file_time;
            input_count += 1;

            max_input_time = max_input_time.max(input_file_time);
            min_input_time = min_input_time.min(input_file_time);

            info!(
                "Input file generated for block {}, time: {} ms, avg: {} ms, max: {} ms, min: {} ms",
                block_number,
                input_file_time,
                total_input_time / input_count as u128,
                max_input_time,
                min_input_time
            );

            if tx.send(input_message_json).is_err() {
                warn!("No active receivers for broadcast channel");
            }

            Ok::<(), anyhow::Error>(())
        }
        .await
        {
            error!("Error processing block {}, error: {:?}", block_number, e);
        }
    }
}

/// Handles a single WebSocket client connection
async fn handle_client(stream: TcpStream, mut rx: Receiver<String>) {
    let inputs_folder = env::var("INPUTS_FOLDER").unwrap_or("inputs".to_string());

    match stream.set_nodelay(true) {
        Ok(_) => (),
        Err(e) => {
            error!("Failed to set TCP_NODELAY, error: {}", e);
            return;
        }
    }

    // Configure WebSocket connection
    let ws_cfg = WebSocketConfig {
        max_frame_size:   Some(32 << 20),   // 32 MiB per frame
        max_message_size: Some(128 << 20),  // 128 MiB per message
        max_write_buffer_size: 32 << 20,    // 32 MiB write buffer
        ..Default::default()
    };

    let ws_stream = match accept_async_with_config(stream, Some(ws_cfg)).await {
        Ok(ws) => ws,
        Err(e) => {
            error!("WebSocket handshake failed, error: {}", e);
            return;
        }
    };

    let peer_addr = match ws_stream.get_ref().peer_addr() {
        Ok(addr) => addr.to_string(),
        Err(_) => "unknown".to_string(),
    };

    info!("Client connected {}", peer_addr);
    let (mut ws_sender, _) = ws_stream.split();

    while let Ok(command) = rx.recv().await {
        let parsed: serde_json::Result<BlockMessage> = serde_json::from_str(&command);
        match parsed {
            Ok(block_message) => match block_message.command {
                BlockCommand::Queued => {
                    info!("Sending block {} queued", block_message.info.block_number);
                    let command_bytes = command.as_bytes();
                    let mut payload = Vec::with_capacity(command_bytes.len() + 1);
                    payload.extend_from_slice(command_bytes);
                    payload.push(b'\n');
                    match ws_sender.send(Message::Binary(payload)).await {
                        Ok(_) => {}
                        Err(e) => {
                            error!("Client disconnected, error: {}", e);
                            break;
                        }
                    }
                }
                BlockCommand::Input => {
                    let file = block_message.info.filename();
                    info!("Sending input for block {}, file: {}", block_message.info.block_number,file);
                    let start_send = Instant::now();
                    let filepath = PathBuf::from(&inputs_folder).join(&file);
                    match fs::read(&filepath) {
                        Ok(content) => {
                            let command_bytes = command.as_bytes();
                            let mut payload = Vec::with_capacity(command_bytes.len() + 1 + content.len());
                            payload.extend_from_slice(command_bytes);
                            payload.push(b'\n');
                            payload.extend_from_slice(&content);
                            match ws_sender.send(Message::Binary(payload)).await {
                                Ok(_) => {}
                                Err(e) => {
                                    error!("Client disconnected, error: {}", e);
                                    break;
                                }
                            }
                            info!(
                                "Input file sent to client: {}, time: {} ms, txs: {:?}, mgas: {:?}",
                                file,
                                start_send.elapsed().as_millis(),
                                block_message.info.tx_count,
                                block_message.info.mgas
                            );
                        }
                        Err(e) => {
                            error!("Error reading input file {}, error: {}", file, e);
                        }
                    }
                }
            },
            Err(_) => {
                error!("Invalid JSON command received: {}", command);
                continue;
            }
        }
    }
}

#[tokio::main]
async fn main() {
    // Load environment variables from .env file
    dotenv().ok();

    // Check if LOG_RUST is set; if not, set it to "info"
    if std::env::var("RUST_LOG").is_err() {
        std::env::set_var("RUST_LOG", "input_gen_server=info");
    }

    // Initialize the logger
    env_logger::init();

    // Load environment variables from .env file
    dotenv().ok();

    let args = InputGenServerArgs::parse();

    // Create broadcast channel for sending input file metadata + names
    let (tx, _) = broadcast::channel::<String>(100);

    // Launch eth block input file generator
    let tx_clone = tx.clone();
    task::spawn(async move {
        let _ = block_listener(args.guest, tx_clone).await;
    });

    // Start listening for WebSocket clients
    let ws_port = env::var("WS_PORT").unwrap_or(WS_DEFAULT_PORT.to_string());
    let ws_addr = format!("{}:{}", WS_LISTEN_IP, ws_port);
    let listener = TcpListener::bind(&ws_addr).await.unwrap();
    info!("WebSocket server listening on ws://{}", ws_addr);

    while let Ok((stream, _)) = listener.accept().await {
        let tx_for_client = tx.clone();
        let rx = tx_for_client.subscribe();
        task::spawn(handle_client(stream, rx));
    }
}
