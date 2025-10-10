use std::sync::Arc;
use std::{fs, path::PathBuf};
use std::io::Write;

use anyhow::Result;
use chrono::Utc;
use env_logger::{Builder, Env};
use futures_util::{StreamExt, SinkExt};
use log::{error, info, warn, debug};
use tokio::fs::create_dir_all;
use tokio::time::{self, Duration, Instant};
use tokio::task;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message;

mod api;
mod cliargs;
mod db;
mod prove;
mod state;
mod telegram;
mod webhook;

use prove::generate_proof;
use webhook::start_webhook_server;

use crate::state::AppState;

// Constants
const OUTPUT_FOLDER: &str = "output";
const LOG_FOLDER: &str = "log";
const DEFAULT_INPUTS_FOLDER: &str = "upload_inputs";

const PING_INTERVAL: Duration = Duration::from_secs(15);
const IDLE_TIMEOUT: Duration  = Duration::from_secs(30 * 60); // 30 min

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

    // Initialize application state
    let app_state = AppState::new().await;

    // Ensure output directory exists
    create_dir_all(&app_state.inputs_folder).await.unwrap();

    // Launch the webhook server
    let state_clone = app_state.clone();
    task::spawn(async move {
        if let Err(e) = start_webhook_server(state_clone).await {
            panic!("Webhook server exited with error: {}", e);
        }
    });


    // Loop to connect (re-connect) to the input generator server
    let mut attempt: u32 = 0;
    loop {
        info!("Connecting to input-gen-server at {}", app_state.input_gen_server_url);

        let (ws_stream, _) = match connect_async(&app_state.input_gen_server_url).await {
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
                continue;
            }
        };

        let (mut writer, mut reader) = ws_stream.split();

        // Heartbeat + idle tracking
        let mut ping_ticker = time::interval(PING_INTERVAL);
        let mut last_activity = Instant::now();

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

                                    if let Some(client) = &app_state.ethproofs_client {
                                        client.proof_queued(app_state.ethproofs_cluster_id.unwrap(), block_number).await?;
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
                                let filepath = PathBuf::from(&app_state.inputs_folder).join(filename);
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

                                let proving_block_shared_clone = Arc::clone(&app_state.proving_block);
                                let mut proving_block = proving_block_shared_clone.lock().unwrap();
                                *proving_block = block_number;

                                // Report to EthProofs that we are generating the proof
                                if let Some(client) = &app_state.ethproofs_client {
                                    let start = std::time::Instant::now();
                                    client.proof_proving(app_state.ethproofs_cluster_id.unwrap(), block_number).await?;
                                    debug!("Report proving state for block number {}, request_time: {} ms", block_number, start.elapsed().as_millis());
                                }

                                info!("Generating proof for block number {}", block_number);
                                if let Err(e) = generate_proof(block_number, app_state.clone()).await {
                                    error!("Proof generation failed for block number {}, error: {}", block_number, e);
                                    continue;
                                }
                                info!("Proof generation started for block number {}", block_number);
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
    }
}
