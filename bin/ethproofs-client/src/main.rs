use std::io::Write;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use std::{fs, path::Path, path::PathBuf};

use anyhow::Result;
use chrono::Utc;
use env_logger::{Builder, Env};
use futures_util::{SinkExt, StreamExt};
use log::{debug, error, info, warn};
use tokio::fs::create_dir_all;
use tokio::sync::Mutex;
use tokio::task;
use tokio::time::{self, Duration, Instant};
use tokio_tungstenite::connect_async_with_config;
use tokio_tungstenite::tungstenite::Message;
use serde_json;

mod api;
mod cliargs;
mod db;
mod metrics;
mod prove;
mod state;
mod telegram;
mod webhook;

use prove::generate_proof;
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
use webhook::start_webhook_server;

use crate::cliargs::TelegramEvent;
use crate::metrics::{BLOCK_TIMESTAMP_GAUGE, RECEIVED_TIME_GAUGE, TIME_TO_INPUT_GAUGE, prune_gauge_last_n};
use crate::state::{AppState, LOG_FOLDER, OUTPUT_FOLDER};
use ethproofs_protocol::{BlockCommand, BlockInfo, BlockMessage};
use crate::telegram::{AlertType, send_telegram_alert};

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

fn process_queued(block_number: u64, app_state: &AppState, queued_start: &mut std::time::Instant) {
    *queued_start = std::time::Instant::now();

    // Set Prometheus block_timestamp in seconds
    if app_state.cliargs.enable_metrics {
        let now_secs = chrono::Utc::now().timestamp();
        let block_label = block_number.to_string();
        BLOCK_TIMESTAMP_GAUGE.with_label_values(&[&block_label]).set(now_secs);
        prune_gauge_last_n(&BLOCK_TIMESTAMP_GAUGE, 300);
    }

    info!("Received queued command for block {}", block_number);

    if let Some(client) = app_state.ethproofs_client.clone() {
        let cluster_id = app_state.ethproofs_cluster_id.unwrap();
        tokio::spawn(async move {
            let start = std::time::Instant::now();
            match client.proof_queued(cluster_id, block_number).await {
                Ok(_) => {
                    info!(
                        "Reported queued state to EthProofs for block {}, request_time: {} ms",
                        block_number,
                        start.elapsed().as_millis()
                    );
                }
                Err(e) => {
                    error!("Failed to report queued state to EthProofs for block {}: {}", block_number, e);
                }
            }
        });
    }
}

async fn process_input(
    block_info: BlockInfo,
    content: &[u8],
    app_state: &AppState,
    queued_start: &std::time::Instant,
    fired_skipped_alert: &mut bool,
) {
    let filename = block_info.filename();
    let block_number = block_info.block_number;
    let filepath = PathBuf::from(&app_state.inputs_folder).join(&filename);

    let result = fs::write(&filepath, content);
    let elapsed = queued_start.elapsed().as_millis();

    match result {
        Ok(_) => {
            let block_label = block_number.to_string();
            if app_state.cliargs.enable_metrics {
                RECEIVED_TIME_GAUGE.with_label_values(&[&block_label]).set(elapsed as i64);
                prune_gauge_last_n(&RECEIVED_TIME_GAUGE, 300);
            }
        }
        Err(e) => {
            error!("Failed to save input for block {}, file: {}, error: {}", block_number, filename, e);
            return;
        }
    }

    let block_timestamp_ms = block_info.timestamp.as_u64() as u128 * 1000;
    let time_to_input = match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(now) => now.as_millis() as u128 - block_timestamp_ms,
        Err(_) => 0,
    };

    if app_state.cliargs.enable_metrics {
        let block_label = block_number.to_string();
        TIME_TO_INPUT_GAUGE.with_label_values(&[&block_label]).set(time_to_input as i64);
        prune_gauge_last_n(&TIME_TO_INPUT_GAUGE, 300);
    }


    info!("Received and saved input for block {}, file: {}, time: {} ms, time-to-input: {} ms", block_number, filename, elapsed, time_to_input);

    if app_state.cliargs.skip_proving {
        info!("Skipping proving for block {} as per configuration", block_number);
        app_state.delete_input_file(&filename);
        return;
    }

    // Check if already proving a block
    let proving_block_shared_clone = Arc::clone(&app_state.proving_block);
    let mut proving_block = proving_block_shared_clone.lock().unwrap();
    if proving_block.is_some() {
        warn!("⚠️ Already proving block, saving next block {}", block_number);
        let next_proving_block_shared_clone = Arc::clone(&app_state.next_proving_block);
        let mut next_proving_block = next_proving_block_shared_clone.lock().unwrap();
        *next_proving_block = Some(block_info);

        let proving_block_number = proving_block.clone().unwrap().clone().block_number;
        // Check for skipped blocks threshold
        if block_number - proving_block_number > app_state.cliargs.skipped_threshold as u64 {
            let msg_alert = format!(
                "Skipped {} consecutive blocks. Currently proving block {}, next queued block is {}.",
                block_number - proving_block_number - 1,
                proving_block_number,
                block_number
            );
            warn!("{}", msg_alert);
            if app_state.cliargs.telegram_enabled(TelegramEvent::SkippedThreshold) {
                if !*fired_skipped_alert {
                    let handle = tokio::spawn(async move {
                        if let Err(e) = send_telegram_alert(&msg_alert, AlertType::Warning).await {
                            warn!("Failed to send Telegram alert: {}, error: {}", msg_alert, e);
                        }
                    });
                    if app_state.cliargs.panic_on_skipped { handle.await.ok(); }
                }
            }
            if app_state.cliargs.panic_on_skipped { panic!("Skipped blocks exceeded threshold, panicking as per configuration"); }
            *fired_skipped_alert = true;
        } else if *fired_skipped_alert {
            let msg_alert = format!("Resumed proving. Now proving block {}.", proving_block_number);
            info!("{}", msg_alert);
            if app_state.cliargs.telegram_enabled(TelegramEvent::SkippedThreshold) {
                tokio::spawn(async move {
                    if let Err(e) = send_telegram_alert(&msg_alert, AlertType::Info).await {
                        warn!("Failed to send Telegram alert: {}, error: {}", msg_alert, e);
                    }
                });
            }
            *fired_skipped_alert = false;
        }
        return;
    }

    // Start proof generation
    let result = generate_proof(block_info.clone(), app_state.clone()).await;
    match result {
        Ok(job_id) => {
            *proving_block = Some(block_info.clone());
            let current_job_id_shared_clone = Arc::clone(&app_state.current_job_id);
            let mut current_job_id = current_job_id_shared_clone.lock().unwrap();
            *current_job_id = job_id;
        }
        Err(e) => {
            let msg_alert = format!("Proof generation failed for block {}, error: {}", block_number, e);
            error!("❌ {}", &msg_alert);
            if app_state.cliargs.telegram_enabled(TelegramEvent::SkippedThreshold) {
                tokio::spawn(async move {
                    if let Err(e) = send_telegram_alert(&msg_alert, AlertType::Info).await {
                        warn!("Failed to send Telegram alert: {}, error: {}", msg_alert, e);
                    }
                });
            }
            app_state.delete_input_file(&filename);
        }
    }
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

    // Ensure input, output, and log directories exist
    create_dir_all(&app_state.inputs_folder).await?;
    create_dir_all(Path::new(OUTPUT_FOLDER)).await?;
    create_dir_all(Path::new(LOG_FOLDER)).await?;

    // Launch the webhook server
    let state_clone = app_state.clone();
    task::spawn(async move {
        if let Err(e) = start_webhook_server(state_clone).await {
            panic!("Webhook server exited with error: {}", e);
        }
    });

    // Launch the metrics server if enabled
    if app_state.cliargs.enable_metrics {
        let state_clone = app_state.clone();
        task::spawn(async move {
            if let Err(e) = metrics::start_metrics_server(state_clone).await {
                panic!("Metrics server exited with error: {}", e);
            }
        });
    }

    // Configure WebSocket connection
    let ws_cfg = WebSocketConfig {
        max_frame_size:   Some(32 << 20),   // 32 MiB per frame
        max_message_size: Some(128 << 20),  // 128 MiB per message
        max_write_buffer_size: 32 << 20,    // 32 MiB write buffer
        ..Default::default()
    };

    // Loop to connect (re-connect) to the input generator server
    let mut attempt: u32 = 0;
    let mut fired_skipped_alert = false;
    loop {
        info!("Connecting to input-gen-server at {}", app_state.input_gen_server_url);

        let (ws_stream, _) = match connect_async_with_config(&app_state.input_gen_server_url, Some(ws_cfg), false).await {
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

        let (writer, mut reader) = ws_stream.split();
        let writer = Arc::new(Mutex::new(writer));

        // Heartbeat and idle tracking
        let mut ping_ticker = time::interval(PING_INTERVAL);
        let mut last_activity = Instant::now();

        let mut queued_start = std::time::Instant::now();

        loop {
            tokio::select! {
                // 1) Reading messages from server
                next = reader.next() => {
                    #[allow(unreachable_patterns)]
                    match next {
                        Some(Ok(Message::Binary(payload))) => {
                            last_activity = Instant::now();
                            if let Some((block_msg, content)) = parse_binary_input(&payload) {
                                match block_msg.command {
                                    BlockCommand::Queued => {
                                        process_queued(block_msg.info.block_number, &app_state, &mut queued_start);
                                    }
                                    BlockCommand::Input => {
                                        process_input(block_msg.info, content, &app_state, &queued_start, &mut fired_skipped_alert).await;
                                    }
                                }
                            } else {
                                error!("Malformed binary message received (cannot parse header JSON)");
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

                // 2) Heartbeat: send ping each PING_INTERVAL in a separate task
                _ = ping_ticker.tick() => {
                    let writer_clone = Arc::clone(&writer);
                    tokio::spawn(async move {
                        let mut w = writer_clone.lock().await;
                        if let Err(e) = w.send(Message::Ping(Vec::new())).await {
                            warn!("Failed to send Ping: {e}");
                            true
                        } else {
                            false
                        }
                    });
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
