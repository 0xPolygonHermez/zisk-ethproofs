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
use tokio::{
    net::{TcpListener, TcpStream},
    sync::broadcast::{self, Receiver, Sender},
    task,
};
use tokio_tungstenite::{accept_async, tungstenite::Message};

const WS_LISTEN_IP: &str = "0.0.0.0";
const WS_DEFAULT_PORT: &str = "8765";

#[derive(Debug, Clone, Parser)]
pub struct InputGenServerArgs {
    #[clap(long, short)]
    pub guest: GuestProgram,
}

/// Generate the rsp input file for the given block number and return the time taken in milliseconds
pub async fn generate_input_file(
    guest: GuestProgram,
    block_number: u64,
    inputs_folder: String,
) -> Result<u128> {
    let start = Instant::now();

    // Load RPC URL from environment variable
    let rpc_url = env::var("RPC_URL").expect("RPC_URL must be set");

    // Create the inputs folder if it doesn't exist
    let input_folder = Path::new(&inputs_folder);
    std::fs::create_dir_all(input_folder)?;

    let input_path = input_folder.join(format!("{}.bin", block_number));

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
        let mut block_number: u64 = 0;
        let rpc_provider = Provider::<Ws>::connect(rpc_ws_url.clone())
            .await
            .context("Failed to connect to WS RPC provider")?;

        let mut stream = rpc_provider.subscribe_blocks().await?;
        info!("Listening for new blocks on Ethereum Mainnet...");

        while let Some(block) = stream.next().await {
            if let Some(number) = block.number {
                block_number = number.as_u64();
            } else {
                warn!("Received block without number, skipping...");
                continue;
            }

            if block_number % block_modulus == 0 {
                info!("Received block number {}, processing...", block_number);
                break;
            } else {
                info!("Received block number {}, skipping...", block_number);
            }
        }

        if let Err(e) = async {
            if tx.send(format!("queued {}", block_number)).is_err() {
                info!(
                    "No active receivers, skipping input file generation for block {}",
                    block_number
                );
                return Ok::<(), anyhow::Error>(());
            }

            info!("Generating input file for block {}", block_number);

            let input_file_time =
                generate_input_file(guest.clone(), block_number, inputs_folder.clone()).await?;

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

            if tx.send(format!("input {}.bin", block_number)).is_err() {
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
            error!("Failed to set TCP_NODELAY: {}", e);
            return;
        }
    }

    let ws_stream = match accept_async(stream).await {
        Ok(ws) => ws,
        Err(e) => {
            error!("WebSocket handshake failed: {}", e);
            return;
        }
    };

    info!("Client connected");
    let (mut ws_sender, _) = ws_stream.split();

    while let Ok(command) = rx.recv().await {
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
                let start_send = Instant::now();

                let filepath = PathBuf::from(&inputs_folder).join(&file);
                match fs::read(&filepath) {
                    Ok(content) => {
                        let payload = format!("{}\n", file)
                            .into_bytes()
                            .into_iter()
                            .chain(content)
                            .collect::<Vec<u8>>();

                        if ws_sender.send(Message::Binary(payload)).await.is_err() {
                            error!("Client disconnected");
                            break;
                        }

                        info!(
                            "Input file sent to client: {}, time: {} ms",
                            file,
                            start_send.elapsed().as_millis()
                        );
                    }
                    Err(e) => {
                        error!("Error reading input file {}: {}", file, e);
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
        std::env::set_var("RUST_LOG", "input_gen_server=info");
    }

    // Initialize the logger
    env_logger::init();

    // Load environment variables from .env file
    dotenv().ok();

    let args = InputGenServerArgs::parse();

    // Create broadcast channel for sending input file names to clients
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
