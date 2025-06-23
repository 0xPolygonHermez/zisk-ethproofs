use std::{collections::HashSet, fs, path::PathBuf};
use std::{env, path::Path, time::Instant, sync::Arc};

use anyhow::{Context, Result};
use dotenv::dotenv;
use ethers::providers::{Middleware, Provider, Ws};
use futures_util::{SinkExt, StreamExt};
use log::{error, info, warn};
use rsp_host_executor::EthHostExecutor;
use rsp_primitives::genesis::Genesis;
use rsp_provider::create_provider;
use rsp_rpc_db::RpcDb;
use tokio::{
    net::{TcpListener, TcpStream},
    sync::broadcast::{self, Receiver, Sender},
    task,
};
use tokio_tungstenite::{accept_async, tungstenite::Message};
use url::Url;

const INPUTS_FOLDER: &str = "./inputs";
const WS_ADDR: &str = "0.0.0.0:8765";

/// Generate the input file for the given block number and return the time taken in milliseconds
pub async fn generate_input_file(block_number: u64) -> Result<u128> {
    // Load RPC URL from environment variable
    let rpc_url = env::var("RPC_URL").expect("RPC_URL must be set");

    // Create the RPC provider and database.
    let provider = create_provider(
        Url::parse(rpc_url.as_str())
            .expect("Invalid RPC URL"),
    );
    let rpc_db = RpcDb::new(provider.clone(), block_number - 1);
    let genesis = Genesis::Mainnet;

    let executor = EthHostExecutor::eth(
        Arc::new(
            (&genesis).try_into().expect("Failed to convert genesis block into the required type"),
        ),
        None,
    );

    let start = Instant::now();

    // Execute the host to get the client input
    let input = executor
        .execute(block_number, &rpc_db, &provider, genesis.clone(), None, false)
        .await
        .expect("Failed to execute client");

    // Create the inputs folder if it doesn't exist
    let input_folder = Path::new(INPUTS_FOLDER);
    std::fs::create_dir_all(input_folder)?;

    // Serialize the client input to a binary file
    let input_path = input_folder.join(format!("{}.bin", block_number));
    let mut input_file = std::fs::File::create(input_path.clone())?;
    bincode::serialize_into(&mut input_file, &input)?;

    let input_file_time = start.elapsed().as_millis();

    Ok(input_file_time)
}

async fn input_file_generator(tx: Sender<String>) -> Result<()> {
    let rpc_ws_url = env::var("RPC_WS_URL").expect("RPC_WS_URL must be set");
    let block_modulus: u64 = env::var("BLOCK_MODULUS")
        .unwrap_or("100".to_string())
        .parse()
        .expect("BLOCK_MODULUS must be a valid integer");

    loop {
        let mut block_number: u64 = 0;
        let rpc_provider = Provider::<Ws>::connect(rpc_ws_url.clone())
            .await
            .context("Failed to connect to WS RPC provider")?;

        let mut stream = rpc_provider.subscribe_blocks().await?;
        info!("Listening for new blocks on Ethereum Mainnet...");

        while let Some(block) = stream.next().await {
            if let Some(number) = block.number {
                block_number = number.as_u64();
                info!("Received block number {}", block_number);
            } else {
                warn!("Received block without number, skipping...");
                continue;
            }

            if block_number % block_modulus == 0 {
                break;
            } else {
                info!("Skipping block number: {}", block_number);
            }
        }

        if let Err(e) = (|| async {
            let block = rpc_provider
                .get_block(block_number).await?
                .ok_or_else(|| anyhow::anyhow!("Block {} not found", block_number))?;

            if tx.send(format!("queued {}", block_number)).is_err() {
                warn!("No active receivers, skipping generate input file for block {}", block_number);
                return Ok::<(), anyhow::Error>(());
            }

            info!(
                "Generating input file for block {}, txs: {}, gas: {}",
                block_number,
                block.transactions.len(),
                block.gas_used
            );

            let input_file_time = generate_input_file(block_number).await?;
            info!(
                "Input file generated for block {}, time: {}ms",
                block_number,
                input_file_time
            );

            if tx.send(format!("input {}.bin", block_number)).is_err() {
                warn!("No active receivers for broadcast channel");
            }

            Ok::<(), anyhow::Error>(())
        })().await {
            error!("Error processing block {}, error: {:?}", block_number, e);
        }
    }
}

/// Handles a single WebSocket client connection
async fn handle_client(stream: TcpStream, mut rx: Receiver<String>) {
    let ws_stream = match accept_async(stream).await {
        Ok(ws) => ws,
        Err(e) => {
            error!("WebSocket handshake failed: {}", e);
            return;
        }
    };

    info!("Client connected");
    let (mut ws_sender, _) = ws_stream.split();
    let mut sent_files = HashSet::new();

    while let Ok(command) = rx.recv().await {
        if sent_files.contains(&command) {
            continue;
        }

        let args: Vec<&str> = command.split_whitespace().collect();

        match args.as_slice() {
            ["queued", block_number] => {
                info!("Sending block {} queued", block_number);

                let payload = format!("queued {}", block_number);
                if ws_sender.send(Message::Text(payload)).await.is_err() {
                    error!("Client disconnected");
                    break;
                }
            }
            ["input", file] => {
                info!("Sending input file: {}", file);

                let filepath = PathBuf::from(INPUTS_FOLDER).join(&file);
                match fs::read(&filepath) {
                    Ok(content) => {
                        let payload = format!("{}\n", command)
                            .into_bytes()
                            .into_iter()
                            .chain(content)
                            .collect::<Vec<u8>>();

                        if ws_sender.send(Message::Binary(payload)).await.is_err() {
                            error!("Client disconnected");
                            break;
                        }

                        info!("Input file sent to client: {}", command);
                        sent_files.insert(command);
                    }
                    Err(e) => {
                        error!("Error reading input file {}: {}", command, e);
                    }
                }                
            }
            _ => {
                error!("Unknown command received: {}", command);
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
        std::env::set_var("RUST_LOG", "ig_server=info");
    }

    // Initialize the logger
    env_logger::init();

    // Load environment variables from .env file
    dotenv().ok();

    // Create broadcast channel for sending input file names to clients
    let (tx, _) = broadcast::channel::<String>(100);

    // Launch eth block input file generator
    let tx_clone = tx.clone();
    task::spawn(async move {
        let _ = input_file_generator(tx_clone).await;
    });

    // Start listening for WebSocket clients
    let listener = TcpListener::bind(WS_ADDR).await.unwrap();
    info!("WebSocket server listening on ws://{}", WS_ADDR);

    while let Ok((stream, _)) = listener.accept().await {
        let tx_for_client = tx.clone();
        let rx = tx_for_client.subscribe();
        task::spawn(handle_client(stream, rx));
    }
}