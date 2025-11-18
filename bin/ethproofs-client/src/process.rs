use std::fs::File;
use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use log::{error, info, warn};

use crate::cliargs::TelegramEvent;
use crate::metrics::BlockMetrics;
use crate::prove::generate_proof;
use crate::state::AppState;
use crate::telegram::{send_telegram_alert, AlertType};
use ethproofs_common::protocol::BlockInfo;

#[derive(Debug, Default)]
pub(crate) struct FiredAlerts {
    pub(crate) skipped: bool,
    pub(crate) failed: bool,
}

pub(crate) fn process_queued(
    block_number: u64,
    app_state: &AppState,
    queued_start: &mut std::time::Instant,
) {
    *queued_start = std::time::Instant::now();

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
                    error!(
                        "Failed to report queued state to EthProofs for block {}, error: {}",
                        block_number, e
                    );
                }
            }
        });
    }
}

pub(crate) async fn process_input(
    block_info: BlockInfo,
    content: &[u8],
    app_state: &AppState,
    queued_start: &std::time::Instant,
    fired_alerts: &mut FiredAlerts,
) {
    let filename = block_info.filename();
    let block_number = block_info.block_number;
    let filepath = PathBuf::from(&app_state.inputs_folder).join(&filename);

    let mut file = File::create(filepath)
        .expect(&format!("Cannot create the file: {} for block {}", &filename, &block_number));

    file.write_all(content)
        .expect(&format!("Failed to write to file: {} for block {}", &filename, &block_number));
    file.flush().expect(&format!(
        "Failed to flush file buffer to OS for file: {} for block {}",
        &filename, &block_number
    ));
    file.sync_all()
        .expect(&format!("Failed to sync file {} to disk for block {}", &filename, &block_number));

    let elapsed = queued_start.elapsed().as_millis();

    let block_timestamp_ms = block_info.timestamp.as_u64() as u128 * 1000;
    let time_to_input = match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(now) => now.as_millis() as u128 - block_timestamp_ms,
        Err(_) => 0,
    };

    if app_state.cliargs.enable_metrics {
        let mut metrics_map = app_state.shared_metrics.lock().await;
        metrics_map.insert(
            block_number,
            BlockMetrics {
                block_number,
                received_time_ms: elapsed as i64,
                time_to_input_ms: time_to_input as i64,
                mgas: block_info.mgas,
                tx_count: block_info.tx_count as u64,
                timestamp: block_info.timestamp.as_u64() as i64,
                proving_time_ms: None,
                proving_cycles: None,
                submit_time_ms: None,
                success: false,
            },
        );
    }

    info!(
        "Received and saved input for block {}, file: {}, time: {} ms, time-to-input: {} ms",
        block_number, filename, elapsed, time_to_input
    );

    if app_state.cliargs.skip_proving {
        info!("Skipping proving for block {} as per configuration", block_number);
        app_state.delete_input_file(&filename);
        return;
    }

    let proving_block_shared_clone = Arc::clone(&app_state.proving_block);
    let mut proving_block = proving_block_shared_clone.lock().unwrap();
    if proving_block.is_some() {
        warn!("⚠️ Already proving block, saving next block {}", block_number);
        let next_proving_block_shared_clone = Arc::clone(&app_state.next_proving_block);
        let mut next_proving_block = next_proving_block_shared_clone.lock().unwrap();
        *next_proving_block = Some(block_info);

        let proving_block_number = proving_block.clone().unwrap().clone().block_number;
        if block_number - proving_block_number > app_state.cliargs.skipped_threshold as u64 {
            let msg_alert = format!(
                "Skipped {} consecutive blocks. Currently proving block {}, next queued block is {}.",
                block_number - proving_block_number - 1,
                proving_block_number,
                block_number
            );
            warn!("{}", msg_alert);
            if app_state
                .cliargs
                .telegram_enabled(TelegramEvent::SkippedThreshold)
                && !fired_alerts.skipped
            {
                let handle = tokio::spawn(async move {
                    if let Err(e) = send_telegram_alert(&msg_alert, AlertType::Warning).await {
                        warn!("Failed to send Telegram alert: {}, error: {}", msg_alert, e);
                    }
                });
                if app_state.cliargs.panic_on_skipped {
                    handle.await.ok();
                }
                fired_alerts.skipped = true;
            }
            if app_state.cliargs.panic_on_skipped {
                panic!("Skipped blocks exceeded threshold, panicking as per configuration");
            }
        } else if app_state
            .cliargs
            .telegram_enabled(TelegramEvent::SkippedThreshold)
            && fired_alerts.skipped
        {
            tokio::spawn(async move {
                let msg_alert = format!("Resumed proving. Now proving block {}.", proving_block_number);
                if let Err(e) = send_telegram_alert(&msg_alert, AlertType::Info).await {
                    warn!("Failed to send Telegram alert: {}, error: {}", msg_alert, e);
                }
            });
            fired_alerts.skipped = false;
        }
        return;
    }

    let result = generate_proof(block_info.clone(), app_state.clone()).await;
    match result {
        Ok(job_id) => {
            *proving_block = Some(block_info.clone());
            let current_job_id_shared_clone = Arc::clone(&app_state.current_job_id);
            let mut current_job_id = current_job_id_shared_clone.lock().unwrap();
            *current_job_id = job_id;

            if app_state.cliargs.telegram_enabled(TelegramEvent::ProofFailed) && fired_alerts.failed
            {
                let msg_alert = format!("Resumed proving. Now proving block {}.", block_info.block_number);
                tokio::spawn(async move {
                    if let Err(e) = send_telegram_alert(&msg_alert, AlertType::Info).await {
                        warn!("Failed to send Telegram alert: {}, error: {}", msg_alert, e);
                    }
                });
                fired_alerts.failed = false;
            }
        }
        Err(e) => {
            let msg_alert = format!("Proof generation failed for block {}, error: {}", block_number, e);
            error!("❌ {}", &msg_alert);
            if app_state.cliargs.telegram_enabled(TelegramEvent::ProofFailed) && !fired_alerts.failed
            {
                tokio::spawn(async move {
                    if let Err(e) = send_telegram_alert(&msg_alert, AlertType::Info).await {
                        warn!("Failed to send Telegram alert: {}, error: {}", msg_alert, e);
                    }
                });
                fired_alerts.failed = true;
            }

            app_state.delete_input_file(&filename);
        }
    }
}
