use crate::metrics::BlockMetrics;
use std::collections::HashMap;
type SharedMetrics = Arc<Mutex<HashMap<u64, BlockMetrics>>>;
use std::io::Write;
use std::sync::Arc;
use std::{fs, path::PathBuf};

use anyhow::Result;
use chrono::Utc;
use env_logger::{Builder, Env};
use futures_util::{SinkExt, StreamExt};
use log::{debug, error, info, warn};
use regex::Regex;
use serde_json;
use tokio::fs::create_dir_all;
use tokio::sync::Mutex;
use tokio::task;
use tokio::time::{self, sleep, Duration, Instant};
use tokio_tungstenite::connect_async_with_config;
use tokio_tungstenite::tungstenite::Message;

mod api;
mod process;
mod cliargs;
mod db;
mod metrics;
mod prove;
mod state;
mod telegram;
mod webhook;

use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
use webhook::start_webhook_server;

use process::{process_input, process_queued, FiredAlerts};
use crate::state::AppState;
use ethproofs_common::inputgen::generate_input;
use ethproofs_common::protocol::{BlockCommand, BlockInfo, BlockMessage};

// Constants
const PING_INTERVAL: Duration = Duration::from_secs(15);
const IDLE_TIMEOUT: Duration = Duration::from_secs(30 * 60); // 30 min

/// Parses a binary WebSocket message into (BlockMessage, file_content)
fn parse_binary_input(data: &[u8]) -> Option<(BlockMessage, &[u8])> {
    let split_index = data.iter().position(|&b| b == b'\n')?;
    let (header_bytes, content_with_newline) = data.split_at(split_index);
    let content = &content_with_newline[1..]; // skip newline
    let header_str = std::str::from_utf8(header_bytes).ok()?;
    let msg: BlockMessage = serde_json::from_str(header_str).ok()?;
    Some((msg, content))
}

/// Connect (and reconnect) loop to the input generation server, handling queued/input messages.
async fn connect_to_input_gen_server(app_state: AppState) {
    let ws_cfg = WebSocketConfig {
        max_frame_size: Some(32 << 20),    // 32 MiB per frame
        max_message_size: Some(128 << 20), // 128 MiB per message
        max_write_buffer_size: 32 << 20,   // 32 MiB write buffer
        ..Default::default()
    };

    let mut attempt: u32 = 0;
    let mut fired_alerts = FiredAlerts::default();
    loop {
        info!("Connecting to input-gen-server at {}", app_state.input_gen_server_url);
        let (ws_stream, _) =
            match connect_async_with_config(&app_state.input_gen_server_url, Some(ws_cfg), false)
                .await
            {
                Ok(s) => {
                    info!("Connected to input-gen-server");
                    attempt = 0; // reset backoff
                    s
                }
                Err(e) => {
                    let backoff_ms = 1_000u64.saturating_mul(2u64.saturating_pow(attempt.min(6)));
                    warn!("Connect failed: {e}. Retrying in {} ms", backoff_ms);
                    time::sleep(Duration::from_millis(backoff_ms)).await;
                    attempt = attempt.saturating_add(1);
                    continue;
                }
            };

        let (writer, mut reader) = ws_stream.split();
        let writer = Arc::new(Mutex::new(writer));
        let mut ping_ticker = time::interval(PING_INTERVAL);
        let mut last_activity = Instant::now();
        let mut queued_start = std::time::Instant::now();

        loop {
            tokio::select! {
                next = reader.next() => {
                    match next {
                        Some(Ok(Message::Binary(payload))) => {
                            last_activity = Instant::now();
                            if let Some((block_msg, content)) = parse_binary_input(&payload) {
                                match block_msg.command {
                                    BlockCommand::Queued => process_queued(block_msg.info.block_number, &app_state, &mut queued_start),
                                    BlockCommand::Input => process_input(block_msg.info, content, &app_state, &queued_start, &mut fired_alerts).await,
                                }
                            } else {
                                error!("Malformed binary message received (cannot parse header JSON)");
                            }
                        }
                        Some(Ok(Message::Close(frame))) => { warn!("Server closed connection: {:?}", frame); break; }
                        Some(Ok(other)) => { last_activity = Instant::now(); debug!("Ignoring unhandled WS message: {:?}", other); }
                        Some(Err(e)) => { warn!("WebSocket error: {e}"); break; }
                        None => { warn!("WebSocket stream ended"); break; }
                    }
                }
                _ = ping_ticker.tick() => {
                    let writer_clone = Arc::clone(&writer);
                    tokio::spawn(async move {
                        let mut w = writer_clone.lock().await;
                        if let Err(e) = w.send(Message::Ping(Vec::new())).await { warn!("Failed to send Ping, error: {}", e); }
                    });
                }
                _ = time::sleep_until(last_activity + IDLE_TIMEOUT) => { warn!("Idle timeout: no input file received in {:?}", IDLE_TIMEOUT); break; }
            }
        }

        let backoff_ms = 1_000u64.saturating_mul(2u64.saturating_pow(attempt.min(6)));
        warn!("Reconnecting in {} ms...", backoff_ms);
        time::sleep(Duration::from_millis(backoff_ms)).await;
        attempt = attempt.saturating_add(1);
    }
}

/// Local block listener: subscribes directly to Ethereum WS provider and generates inputs locally
async fn run_local_block_listener(app_state: AppState) -> anyhow::Result<()> {
    use ethers::providers::{Middleware, Provider, Ws};

    let mut max_input_time: u128 = 0;
    let mut min_input_time: u128 = u128::MAX;
    let mut total_input_time: u128 = 0;
    let mut input_count: u64 = 0;

    let mut fired_alerts = FiredAlerts::default();
    let mut queued_start = std::time::Instant::now();

    loop {
        let provider = match Provider::<Ws>::connect(&app_state.rpc_ws_url).await {
            Ok(p) => p,
            Err(e) => {
                warn!(
                    "Failed to connect to WS RPC provider at {}, error: {}",
                    app_state.rpc_ws_url, e
                );
                info!("Retrying in 1 seconds...");
                time::sleep(Duration::from_secs(1)).await;
                continue;
            }
        };
        let mut stream = match provider.subscribe_blocks().await {
            Ok(s) => s,
            Err(e) => {
                warn!("Subscription failed, error: {}", e);
                info!("Retrying in 1 seconds...");
                time::sleep(Duration::from_secs(1)).await;
                continue;
            }
        };

        info!("Listening for new blocks on Ethereum Mainnet...");

        // Read blocks until stream ends or error triggers reconnect
        while let Some(block) = stream.next().await {
            let block_number = match block.number {
                Some(n) => n.as_u64(),
                None => {
                    warn!("Block without number, skipping...");
                    continue;
                }
            };

            if block_number % app_state.block_modulus != 0 {
                info!("Received block {}, skipping...", block_number);
                continue;
            }

            info!("Received block {}, processing...", block_number);

            process_queued(block_number, &app_state, &mut queued_start);

            let block_info = BlockInfo {
                block_number,
                timestamp: block.timestamp,
                block_hash: block
                    .hash
                    .map(|h| format!("{:x}", h))
                    .unwrap_or_else(|| "nohash".into()),
                tx_count: block.transactions.len(),
                mgas: block.gas_used.as_u64() / 1_000_000,
            };

            info!("Generating input file for block {}", block_number);
            let input_file_result =
                generate_input(app_state.cliargs.guest.clone(), block_info.clone()).await;

            match input_file_result {
                Ok((input_file_time, input_result)) => {
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
                    process_input(
                        block_info,
                        &input_result.input,
                        &app_state,
                        &queued_start,
                        &mut fired_alerts,
                    )
                    .await;
                }
                Err(e) => {
                    error!("Input file generation failed for block  {}, error: {}", block_number, e)
                }
            }
        }

        // Stream ended, reconnect (next loop iteration will handle reconnect)
        warn!("Block subscription ended, reconnecting...");
    }
}

/// Local folder: reads input files from a local folder at intervals
async fn run_local_folder(app_state: AppState) -> anyhow::Result<()> {
    let mut current_timestamp = if app_state.cliargs.initial_timestamp == 0 {
        chrono::Utc::now().timestamp() as u64
    } else {
        app_state.cliargs.initial_timestamp
    };
    let inputs_queue = app_state.cliargs.inputs_queue.clone();
    let mut queued_start = std::time::Instant::now();
    let mut max_input_time: u128 = 0;
    let mut min_input_time: u128 = u128::MAX;
    let mut total_input_time: u128 = 0;
    let mut input_count: u64 = 0;
    let mut fired_alerts = FiredAlerts::default();

    // Read all files in the input directory
    let entries = match fs::read_dir(&inputs_queue) {
        Ok(e) => e,
        Err(e) => {
            error!("Failed to read input directory {}: {}", inputs_queue, e);
            return Err(e.into());
        }
    };

    // Collect all files matching the pattern and their block numbers
    let hash_re = Regex::new(r"^(\d+)_([a-fA-F0-9]+)\.bin$").unwrap();
    let mut files: Vec<(u64, String, PathBuf)> = vec![];
    for entry in entries {
        if let Ok(entry) = entry {
            let fname = entry.file_name();
            let fname_str = fname.to_string_lossy();
            if let Some(caps) = hash_re.captures(&fname_str) {
                if let (Some(block_str), Some(hash_str)) = (caps.get(1), caps.get(2)) {
                    if let Ok(block_number) = block_str.as_str().parse::<u64>() {
                        let hash = hash_str.as_str().to_string();
                        files.push((block_number, hash, entry.path()));
                    }
                }
            }
        }
    }

    // Sort files by block number (ascending)
    files.sort_by_key(|(block_number, _, _)| *block_number);

    info!("Found {} input files to send", files.len());

    // Send each file
    for (block_number, hash, file_path) in &files {
        info!("Reading block {}, processing...", block_number);

        info!("Generating input file for block {}", block_number);
        sleep(Duration::from_millis(app_state.cliargs.simulated_processed_time)).await;

        // Copy file to inputs folder
        let dest_path =
            PathBuf::from(&app_state.inputs_folder).join(file_path.file_name().unwrap());
        match fs::copy(file_path, &dest_path) {
            Ok(_) => {
                info!("Copied file {:?} to {:?}", file_path, dest_path);
            }
            Err(e) => {
                error!("Failed to copy file {:?} to {:?}: {}", file_path, dest_path, e);
            }
        }

        process_queued(*block_number, &app_state, &mut queued_start);
        let block_info = BlockInfo {
            block_number: *block_number,
            timestamp: current_timestamp.into(),
            block_hash: hash.clone(),
            tx_count: 0,
            mgas: 0,
        };
        current_timestamp += app_state.cliargs.interval_secs;
        total_input_time += app_state.cliargs.simulated_processed_time as u128;
        input_count += 1;
        max_input_time = max_input_time.max(app_state.cliargs.simulated_processed_time as u128);
        min_input_time = min_input_time.min(app_state.cliargs.simulated_processed_time as u128);

        info!(
            "Input file generated for block {}, time: {} ms, avg: {} ms, max: {} ms, min: {} ms",
            block_number,
            app_state.cliargs.simulated_processed_time,
            total_input_time / input_count as u128,
            max_input_time,
            min_input_time
        );

        let path = PathBuf::from(&app_state.inputs_folder).join(block_info.filename());
        match fs::read(&path) {
            Ok(content) => {
                process_input(
                    block_info,
                    &content,
                    &app_state,
                    &queued_start,
                    &mut fired_alerts,
                )
                .await;
            }
            Err(e) => {
                error!("Error reading input file {}: {}", path.display(), e);
                if app_state.cliargs.enable_metrics {
                    crate::metrics::INPUT_FILE_ERROR_TOTAL.inc();
                }
            }
        }
        sleep(Duration::from_secs(app_state.cliargs.interval_secs)).await;
    }

    Ok(())
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
    let app_state = match AppState::new().await {
        Ok(state) => state,
        Err(e) => {
            error!("Failed to initialize application state, error: {}", e);
            return Err(e);
        }
    };

    // Ensure all *_over_12s_total and *_under_12s_total counters appear in /metrics
    crate::metrics::TIME_TO_PROOF_UNDER_12S_TOTAL.inc_by(0);
    crate::metrics::TIME_TO_PROOF_OVER_12S_TOTAL.inc_by(0);
    crate::metrics::PROVING_UNDER_12S_TOTAL.inc_by(0);
    crate::metrics::PROVING_OVER_12S_TOTAL.inc_by(0);

    // Ensure input, output, and log directories exist
    create_dir_all(&app_state.inputs_folder).await?;

    // Launch the webhook server
    let state_clone = app_state.clone();
    task::spawn(async move {
        if let Err(e) = start_webhook_server(state_clone).await {
            panic!("Webhook server exited, error: {}", e);
        }
    });

    // Launch the metrics server if enabled
    if app_state.cliargs.enable_metrics {
        let state_clone = app_state.clone();
        task::spawn(async move {
            if let Err(e) = metrics::start_metrics_server(state_clone).await {
                panic!("Metrics server exited, error: {}", e);
            }
        });
    }

    // Select input generation method
    match app_state.cliargs.input_gen {
        cliargs::InputGen::Server => {
            connect_to_input_gen_server(app_state).await;
        }
        cliargs::InputGen::Local => {
            run_local_block_listener(app_state.clone()).await?;
        }
        cliargs::InputGen::Folder => {
            run_local_folder(app_state.clone()).await?;
        }
    }
    Ok(())
}
