use std::env;
use std::sync::{Arc, Mutex};
use std::{fs, path::PathBuf};
use std::io::Write;

use anyhow::Result;
use clap::Parser;
use chrono::Utc;
use dotenv::dotenv;
use env_logger::{Builder, Env};
use futures_util::{StreamExt, SinkExt};
use log::{error, info, warn, debug};
use tokio::fs::create_dir_all;
use tokio::time::{self, Duration, Instant};
use tokio::task;
use tokio_tungstenite::{connect_async, WebSocketStream, MaybeTlsStream};
use tokio_tungstenite::tungstenite::Message;
use tokio::net::TcpStream;

mod api;
mod prove;
mod telegram;
mod webhook;
use api::EthProofsApi;
use prove::{generate_proof, get_proof_b64};
use telegram::{send_telegram_alert, AlertType};
use webhook::start_webhook_server;

// Constants
const OUTPUT_FOLDER: &str = "output";
const PROGRAM_FOLDER: &str = "elf";
const LOG_FOLDER: &str = "log";
const DEFAULT_INPUTS_FOLDER: &str = "upload_inputs";

const PING_INTERVAL: Duration = Duration::from_secs(15);
const IDLE_TIMEOUT: Duration  = Duration::from_secs(30 * 60); // 30 min

// Command line arguments
#[derive(Parser)]
struct CliArgs {
    /// Enable submit proofs to ethproofs
    #[arg(short = 's', long)]
    submit_ethproofs: bool,

    /// Send telegram alert when block is submitted to ethproofs
    #[arg(short = 'a', long)]
    submit_alert: bool,

    /// Test block number
    #[arg(short = 't', long)]
    test_block: Option<u64>,

    /// Disable distributed proving
    #[arg(short = 'd', long)]
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

async fn connect_ws(url: &str) -> anyhow::Result<WebSocketStream<MaybeTlsStream<TcpStream>>> {
    let (ws, _) = connect_async(url).await?;
    Ok(ws)
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
    Builder::from_env(Env::default())
        .format(|buf, record| {
            let timestamp = Utc::now().format("%Y-%m-%dT%H:%M:%S%.6fZ");
            writeln!(buf, "{} [{}] - {}", timestamp, record.level(), record.args())
        })
        .init();

    // Parse the command line arguments
    let args = CliArgs::parse();

    // Determine if we should submit proofs to ethproofs
    let mut ethproofs_client: Option<EthProofsApi> = None;
    let mut ethproofs_cluster_id = 0_u32;
    if args.submit_ethproofs {
        // Initialize the EthProofsApi
        let ethproofs_api_url = env::var("ETHPROOFS_API_URL").expect("ETHPROOFS_API_URL must be set");
        let ethproofs_api_token = env::var("ETHPROOFS_API_TOKEN").expect("ETHPROOFS_API_TOKEN must be set");
        ethproofs_client = Some(EthProofsApi::new(ethproofs_api_url, ethproofs_api_token));

        // Cluster ID for the EthProofs API
        ethproofs_cluster_id = env::var("ETHPROOFS_CLUSTER_ID")
            .expect("ETHPROOFS_CLUSTER_ID must be set")
            .parse()
            .expect("ETHPROOFS_CLUSTER_ID must be a valid u32");
    }

    let inputs_folder = env::var("UPLOAD_FOLDER").unwrap_or(DEFAULT_INPUTS_FOLDER.to_string());
    // Ensure output directory exists
    create_dir_all(&inputs_folder).await.unwrap();

    // Shared the currently proving block number
    let proving_block_shared = Arc::new(Mutex::new(0u64));

    // Launch the webhook server
    let proving_block_shared_for_webhook = Arc::clone(&proving_block_shared);
    task::spawn(async move {
        start_webhook_server(proving_block_shared_for_webhook).await;
    });

    info!("test proof");
    let result = generate_proof(1, args.disable_distributed, inputs_folder.clone()).await?;

    let input_gen_server_url = env::var("INPUT_GEN_SERVER_URL").expect("INPUT_GEN_SERVER_URL must be set");

    // Loop to connect (re-connect) to the input generator server
    let mut attempt: u32 = 0;
    'reconnect: loop {
        info!("Connecting to input-gen-server at {}", input_gen_server_url);

        let ws_stream = match connect_ws(&input_gen_server_url).await {
            Ok(s) => {
                info!("Connected to input-gen-server");
                attempt = 0; // reset backoff on successful connection
                s
            }
            Err(e) => {
                let backoff_ms = 1_000u64.saturating_mul(2u64.saturating_pow(attempt.min(6))); // 1s,2s,4s,...,64s
                warn!("Connect failed: {e}. Retrying in {} ms", backoff_ms);
                time::sleep(Duration::from_millis(backoff_ms)).await;
                attempt = attempt.saturating_add(1);
                continue 'reconnect;
            }
        };

        let (mut writer, mut reader) = ws_stream.split();

        // Heartbeat + idle tracking
        let mut ping_ticker = time::interval(PING_INTERVAL);
        let mut last_activity = Instant::now(); // se actualiza SOLO con tráfico entrante

        let mut queued_start = std::time::Instant::now();

        loop {
            tokio::select! {
                // 1) Reading messages from server
                next = reader.next() => {
                    #[allow(unreachable_patterns)]
                    match next {
                        Some(Ok(Message::Text(command))) => {
                            last_activity = Instant::now();

                            let args_text = command.split_whitespace().collect::<Vec<&str>>();
                            if args_text.is_empty() {
                                error!("Received empty command");
                                continue;
                            }

                            match args_text[0] {
                                "queued" => {
                                    queued_start = std::time::Instant::now();

                                    let block_number: u64 = args_text[1]
                                        .parse()
                                        .expect("Failed to parse block number from command queued");

                                    info!("Received queued command for block {}", block_number);

                                    if let Some(client) = &ethproofs_client {
                                        client.proof_queued(ethproofs_cluster_id, block_number).await?;
                                    }
                                }
                                _ => {
                                    error!("Unknown command received: {}", command);
                                }
                            }
                        }

                        Some(Ok(Message::Binary(payload))) => {
                            last_activity = Instant::now();

                            if let Some((filename, content)) = parse_message(&payload) {
                                let filepath = PathBuf::from(&inputs_folder).join(filename);
                                match fs::write(&filepath, content) {
                                    Ok(_) => info!("Received and saved input file: {}, time: {} ms", filepath.display(), queued_start.elapsed().as_millis()),
                                    Err(e) => { error!("Failed to save input file {}, error: {}", filepath.display(), e); continue; }
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

                                let proving_block_shared_clone = Arc::clone(&proving_block_shared);
                                let mut proving_block = proving_block_shared_clone.lock().unwrap();
                                *proving_block = block_number;

                                // Report to EthProofs that we are generating the proof
                                if let Some(client) = &ethproofs_client {
                                    let start = std::time::Instant::now();
                                    client.proof_proving(ethproofs_cluster_id, block_number).await?;
                                    debug!("Report proving state for block number {}, request_time: {} ms", block_number, start.elapsed().as_millis());
                                }

                                info!("Generating proof for block number {}", block_number);
                                let result = generate_proof(block_number, args.disable_distributed, inputs_folder.clone()).await?;
                                info!("Proof generated for block number {}, proving_time: {}s, cycles: {}", block_number, result.time / 1000, result.cycles);

                                // Submit the proof to EthProofs
                                let proof_base64 = get_proof_b64(block_number)?;
                                if let Some(client) = &ethproofs_client {
                                    let start = std::time::Instant::now();
                                    client.proof_proved(ethproofs_cluster_id, block_number, result.time, result.cycles, proof_base64, result.id).await?;
                                    info!("Proof submitted to ethproofs for block number {}, submit_time: {} ms", block_number, start.elapsed().as_millis());
                                }

                                // Send success alert to Telegram if enabled
                                if args.submit_alert {
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

                        Some(Ok(Message::Close(frame))) => {
                            warn!("Server closed connection: {:?}", frame);
                            break;
                        }

                        // Ignore other message types
                        Some(Ok(other)) => {
                            last_activity = Instant::now();
                            debug!("Ignoring unhandled WS message: {:?}", other);
                        }

                        Some(Err(e)) => {
                            warn!("WebSocket error: {e}");
                            break;
                        }

                        None => {
                            warn!("WebSocket stream ended");
                            break;
                        }
                    }
                }

                // 2) Heartbeat: send ping each PING_INTERVAL
                _ = ping_ticker.tick() => {
                    if let Err(e) = writer.send(Message::Ping(Vec::new())).await {
                        warn!("Failed to send Ping: {e}");
                        break; // reconnect
                    } else {
                        debug!("Ping sent");
                    }
                }

                // 3) Watchdog: if no incoming messages for IDLE_TIMEOUT, reconnect
                _ = time::sleep_until(last_activity + IDLE_TIMEOUT) => {
                    warn!("Idle timeout: no input file received in {:?}", IDLE_TIMEOUT);
                    break; // reconnect
                }
            }
        }

        // Backoff before reconnecting
        let backoff_ms = 1_000u64.saturating_mul(2u64.saturating_pow(attempt.min(6))); // 1s,2s,4s,...,64s
        warn!("Reconnecting in {} ms...", backoff_ms);
        time::sleep(Duration::from_millis(backoff_ms)).await;
        attempt = attempt.saturating_add(1);
        continue 'reconnect;
    }
}
