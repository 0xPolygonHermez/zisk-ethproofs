use std::{fs, path::PathBuf};

use anyhow::Result;
use chrono::Utc;
use ethers::providers::{Middleware, Provider, Ws};
use futures_util::StreamExt;
use guest_reth::{RethInputPublic, RethInputWitness};
use input::FromRpc;
use log::{error, info, warn};
use regex::Regex;
use tokio::time::{sleep, Duration, Instant};
use zisk_sdk::{ZiskFileStdin, ZiskIO};

use crate::{
    process::{process_input, process_queued},
    state::{AppState, BlockInfo},
};

/// Run process to generate input files locally by connecting to Ethereum node
pub(crate) async fn process_inputs_locally(app_state: &mut AppState) -> Result<()> {
    let mut max_input_time: u128 = 0;
    let mut min_input_time: u128 = u128::MAX;
    let mut total_input_time: u128 = 0;
    let mut input_count: u64 = 0;

    loop {
        info!("Connecting to node WS RPC provider at {}", app_state.cliargs.rpc.ws_url);
        let provider = match Provider::<Ws>::connect(&app_state.cliargs.rpc.ws_url).await {
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

            if block_number % app_state.cliargs.inputs.block_modulus != 0 {
                info!("Received block {}, skipping...", block_number);
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

            info!("Generating input file for block {}", block_number);
            let start_time = Instant::now();

            let inputs_result = {
                let _permit_reth = app_state.calling_reth.acquire().await.unwrap();
                async {
                    let pk = guest_reth::RethInputPublic::from_rpc(
                        &app_state.cliargs.rpc.http_url,
                        block_number,
                    )
                    .await
                    .map_err(|e| format!("Public input generation failed: {e}"))?;
                    let witness = guest_reth::RethInputWitness::from_rpc(
                        &app_state.cliargs.rpc.http_url,
                        block_number,
                    )
                    .await
                    .map_err(|e| format!("Witness input generation failed: {e}"))?;
                    Ok::<_, String>((pk, witness))
                }
                .await
            };

            let (input_pk, input_witness) = match inputs_result {
                Ok(result) => result,
                Err(msg) => {
                    error!("{} for block {}, skipping...", msg, block_number);
                    continue;
                }
            };

            let input_time = start_time.elapsed().as_millis();
            total_input_time += input_time;
            input_count += 1;
            max_input_time = max_input_time.max(input_time);
            min_input_time = min_input_time.min(input_time);

            info!(
                "Input file generated for block {}, time: {} ms, avg: {} ms, max: {} ms, min: {} ms",
                block_number,
                input_time,
                total_input_time / input_count as u128,
                max_input_time,
                min_input_time
            );

            process_input(block_info, &input_pk, &input_witness, app_state).await;
        }

        warn!("Block subscription ended, reconnecting...");
    }
}

/// Run process to get input files from a folder
pub(crate) async fn process_inputs_from_folder(app_state: &mut AppState) -> Result<()> {
    let mut current_timestamp = if app_state.cliargs.folder.initial_timestamp == 0 {
        Utc::now().timestamp() as u64
    } else {
        app_state.cliargs.folder.initial_timestamp
    };
    let inputs_queue = app_state.cliargs.folder.path.clone();
    let mut max_input_time: u128 = 0;
    let mut min_input_time: u128 = u128::MAX;
    let mut total_input_time: u128 = 0;
    let mut input_count: u64 = 0;

    let entries = match fs::read_dir(&inputs_queue) {
        Ok(e) => e,
        Err(e) => {
            error!("Failed to read input directory {}: {}", inputs_queue, e);
            return Err(e.into());
        }
    };

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

    files.sort_by_key(|(block_number, _, _)| *block_number);

    info!("Found {} input files to send", files.len());

    for (block_number, hash, file_path) in &files {
        info!("Reading block {}, processing...", block_number);

        info!("Generating input file for block {}", block_number);
        sleep(Duration::from_millis(app_state.cliargs.folder.input_time)).await;

        let dest_path =
            PathBuf::from(&app_state.cliargs.inputs.folder).join(file_path.file_name().unwrap());
        match fs::copy(file_path, &dest_path) {
            Ok(_) => {
                info!("Copied file {:?} to {:?}", file_path, dest_path);
            }
            Err(e) => {
                error!("Failed to copy file {:?} to {:?}: {}", file_path, dest_path, e);
            }
        }

        process_queued(*block_number, &app_state);
        let block_info = BlockInfo {
            block_number: *block_number,
            timestamp: current_timestamp.into(),
            block_hash: hash.clone(),
            tx_count: 0,
            mgas: 0,
        };
        current_timestamp += app_state.cliargs.folder.interval;
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
        let zisk_stdin_file = ZiskFileStdin::new(&path)?;
        let input_pk: RethInputPublic = match zisk_stdin_file.read() {
            Ok(pk) => pk,
            Err(e) => {
                error!("Error reading public input from file {}: {}", path.display(), e);
                continue;
            }
        };
        let input_witness: RethInputWitness = match zisk_stdin_file.read() {
            Ok(witness) => witness,
            Err(e) => {
                error!("Error reading witness input from file {}: {}", path.display(), e);
                continue;
            }
        };
        process_input(block_info, &input_pk, &input_witness, app_state).await;

        sleep(Duration::from_secs(app_state.cliargs.folder.interval)).await;
    }

    Ok(())
}
