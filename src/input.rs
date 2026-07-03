use std::{fs, path::PathBuf};

use anyhow::Result;
use chrono::Utc;
use ethers::providers::{Middleware, Provider, Ws};
use futures_util::StreamExt;
use input::{create_client, RpcConfig};
use log::{error, info, warn};
use regex::Regex;
use tokio::time::{sleep, Duration, Instant};

use zisk_sdk::ZiskStdin;

use crate::{
    process::{process_input, process_queued},
    state::{AppState, BlockInfo},
    telegram::send_started_alert,
};

/// Run process to generate input files by connecting to an Ethereum node via RPC
pub(crate) async fn process_inputs_from_rpc(app_state: &mut AppState) -> Result<()> {
    let mut max_input_time: u128 = 0;
    let mut min_input_time: u128 = u128::MAX;
    let mut total_input_time: u128 = 0;
    let mut input_count: u64 = 0;

    let rpc_ws_url = app_state.cliargs.rpc.ws_url.clone();
    let rpc_http_url = app_state.cliargs.rpc.http_url.clone();
    let block_modulus = app_state.cliargs.inputs.block_modulus;

    send_started_alert(app_state, "rpc");

    loop {
        info!("Connecting to node WS RPC provider at {}", rpc_ws_url);
        let provider = match Provider::<Ws>::connect(&rpc_ws_url).await {
            Ok(p) => p,
            Err(e) => {
                error!("Failed to connect to node WS RPC provider, error: {}", e);
                info!("Retrying in 5 seconds...");
                sleep(Duration::from_secs(5)).await;
                continue;
            }
        };
        info!("Connected to node WS RPC provider");

        let mut stream = match provider.subscribe_blocks().await {
            Ok(s) => s,
            Err(e) => {
                error!("Failed to subscribe to blocks, error {}", e);
                info!("Retrying in 5 seconds...");
                sleep(Duration::from_secs(5)).await;
                continue;
            }
        };
        info!("Listening for new blocks on Ethereum Mainnet...");

        while let Some(block) = stream.next().await {
            let block_number = match block.number {
                Some(n) => n.as_u64(),
                None => {
                    warn!("Block without number, skipping...");
                    continue;
                }
            };

            if block_number % block_modulus != 0 {
                info!("Received block {}, skipping...", block_number);
                continue;
            }

            // Stop accepting new blocks once the configured '--run-time' has elapsed
            if app_state
                .run_deadline
                .is_some_and(|deadline| std::time::Instant::now() >= deadline)
            {
                if app_state.proving_block.lock().unwrap().is_none() {
                    info!("Run time elapsed and no current proof in-progress, exiting.");
                    return Ok(());
                }
                info!(
                    "Run time elapsed, skipping block {} and waiting for in-progress proof to finish...",
                    block_number
                );
                continue;
            }

            info!("Received block {}, processing...", block_number);

            process_queued(block_number, &app_state);

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

            info!("Fetching input for block {}", block_number);
            let start_time = Instant::now();

            let input_result = {
                let _permit = app_state.calling_client.acquire().await.unwrap();
                create_client(app_state.cliargs.client)
                    .from_rpc(&RpcConfig::new(rpc_http_url.clone()), block_number)
                    .await
            };

            let (zisk_stdin, _stats) = match input_result {
                Ok(result) => result,
                Err(e) => {
                    error!(
                        "Input generation failed for block {}: {}, skipping...",
                        block_number, e
                    );
                    continue;
                }
            };

            let input_time = start_time.elapsed().as_millis();
            total_input_time += input_time;
            input_count += 1;
            max_input_time = max_input_time.max(input_time);
            min_input_time = min_input_time.min(input_time);

            info!(
                "Input fetched for block {}, time: {} ms, avg: {} ms, max: {} ms, min: {} ms",
                block_number,
                input_time,
                total_input_time / input_count as u128,
                max_input_time,
                min_input_time
            );

            process_input(block_info, zisk_stdin, app_state).await;
        }

        warn!("Block subscription ended, reconnecting...");
    }
}

/// Run process to get input files from a folder
pub(crate) async fn process_inputs_from_folder(app_state: &mut AppState) -> Result<()> {
    let inputs_queue = app_state.cliargs.folder.path.clone();
    let mut max_input_time: u128 = 0;
    let mut min_input_time: u128 = u128::MAX;
    let mut total_input_time: u128 = 0;
    let mut input_count: u64 = 0;

    send_started_alert(app_state, "folder");

    let entries = match fs::read_dir(&inputs_queue) {
        Ok(e) => e,
        Err(e) => {
            error!("Failed to read input directory {}: {}", inputs_queue, e);
            return Err(e.into());
        }
    };

    let hash_re = Regex::new(r"^.+\.bin$").unwrap();
    let input_files_filter = &app_state.cliargs.folder.input_files;
    let mut files: Vec<(u64, String, PathBuf)> = vec![];
    for entry in entries {
        if let Ok(entry) = entry {
            let file_name = entry.file_name();
            let file_name = file_name.to_string_lossy();
            if !hash_re.is_match(&file_name) {
                continue;
            }
            if !input_files_filter.is_empty()
                && !input_files_filter.iter().any(|f| f == file_name.as_ref())
            {
                continue;
            }

            // Folder mode is client-agnostic: confirm the file loads as a
            // ZiskStdin, but derive block metadata from the filename rather than
            // decoding client-specific input types.
            let path = entry.path();
            if let Err(e) = ZiskStdin::from_file(&path) {
                return Err(anyhow::anyhow!("Error opening input file {}: {}", path.display(), e));
            }

            let stem =
                path.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
            // First underscore-separated numeric token is treated as the block number.
            let block_number_from_name =
                stem.split('_').find_map(|tok| tok.parse::<u64>().ok()).unwrap_or(u64::MAX);
            let hash_str: String = stem.chars().take(6).collect();

            files.push((block_number_from_name, hash_str, entry.path()));
        }
    }

    files.sort_by_key(|(block_number, _, _)| *block_number);

    info!("Found {} input files to send", files.len());

    let mut current_timestamp = if app_state.cliargs.folder.initial_timestamp == 0 {
        Utc::now().timestamp() as u64
    } else {
        app_state.cliargs.folder.initial_timestamp
    };
    let mut last_block_processed_time = None;

    for (idx, (block_number, hash, file_path)) in files.iter().enumerate() {
        let block_number = if *block_number == u64::MAX { idx as u64 } else { *block_number };
        info!("Reading block {}, processing...", block_number);

        // Calculate elapsed time since timestamp of last block processed
        let elapsed_since_previous =
            last_block_processed_time.map_or(0, |t: Instant| t.elapsed().as_secs());
        // Calculate timestamp for current block by adding elapsed time to timestamp of last block
        current_timestamp = current_timestamp.saturating_add(elapsed_since_previous);
        // Reset last_block_processed_time to now for next iteration
        last_block_processed_time = Some(Instant::now());

        let block_info = BlockInfo {
            block_number,
            timestamp: current_timestamp.into(),
            block_hash: hash.clone(),
            tx_count: 0,
            mgas: 0,
        };

        info!("Generating input file for block {}", block_number);
        sleep(Duration::from_millis(app_state.cliargs.folder.input_time)).await;

        let dest_path = PathBuf::from(&app_state.cliargs.inputs.folder).join(block_info.filename());
        match fs::copy(file_path, &dest_path) {
            Ok(_) => {
                info!("Copied file {:?} to {:?}", file_path, dest_path);
            }
            Err(e) => {
                error!("Failed to copy file {:?} to {:?}: {}", file_path, dest_path, e);
            }
        }

        process_queued(block_number, &app_state);

        total_input_time += app_state.cliargs.folder.input_time as u128;
        input_count += 1;
        max_input_time = max_input_time.max(app_state.cliargs.folder.input_time as u128);
        min_input_time = min_input_time.min(app_state.cliargs.folder.input_time as u128);

        info!(
            "Input file generated for block {}, time: {} ms, avg: {} ms, max: {} ms, min: {} ms",
            block_number,
            app_state.cliargs.folder.input_time,
            total_input_time / input_count as u128,
            max_input_time,
            min_input_time
        );

        let path = PathBuf::from(&app_state.cliargs.inputs.folder).join(block_info.filename());
        let zisk_stdin = match ZiskStdin::from_file(&path) {
            Ok(stdin) => stdin,
            Err(e) => {
                error!("Error reading input from file {}: {}", path.display(), e);
                continue;
            }
        };
        process_input(block_info, zisk_stdin, app_state).await;

        info!("Waiting for block {} proof completion before processing next file...", block_number);
        app_state.proof_done_signal.notified().await;
    }

    Ok(())
}
