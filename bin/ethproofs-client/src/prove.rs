use std::{env, fs::{self, create_dir_all, File}, io::{BufRead, BufReader, Write}, path::Path, process::{Command,Stdio}};
use std::thread;
use std::time::Duration;
use anyhow::{anyhow, Context, Ok, Result};
use base64::{engine::general_purpose, Engine};
use log::{debug, info};
use serde::Deserialize;
use serde_json::Value;

use crate::{LOG_FOLDER, OUTPUT_FOLDER, PROGRAM_FOLDER};

#[derive(Debug, Deserialize)]
pub struct ProofResult {
    pub cycles: u64,
    pub id: String,
    #[serde(deserialize_with = "time_to_millisecs")]
    pub time: u128,
}

fn time_to_millisecs<'de, D>(deserializer: D) -> std::result::Result<u128, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let f = f64::deserialize(deserializer)?;
    std::result::Result::Ok((f.trunc() * 1000.0) as u128)
}

pub async fn wait_prove_done() -> Result<()> {
    let command = String::from("cargo-zisk prove-client status --port 6100");

    let start_time = std::time::Instant::now();

    loop {
        debug!("Executing command: {}", command);
        let mut child = Command::new("sh")
            .arg("-c")
            .arg(&command)
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()?;

        let stdout = child.stdout.take().expect("Failed to capture stdout");
        let reader = BufReader::new(stdout);

        let mut captured_output = Vec::new();

        for line in reader.lines() {
            let line = line?;
            println!("{}", line);
            captured_output.push(format!("{}\n", line));

            if let Some(start_idx) = line.find('{') {
                let json_str = &line[start_idx..];

                debug!("Received JSON: {}", json_str);
                if let std::result::Result::Ok(json) = serde_json::from_str::<Value>(json_str) {
                    let node = json.get("node").and_then(|n| n.as_u64());
                    let result = json.get("result").and_then(|r| r.as_str());
                    let status = json.get("status").and_then(|c| c.as_str());

                    debug!("Parsed JSON: node: {:?}, result: {:?}, status: {:?}", node, result, status);
                    if node == Some(0) && result == Some("ok") && status == Some("idle") {
                        return Ok(());
                    }
                }
            }
        }

        if start_time.elapsed().as_secs() > 300 {
            return Err(anyhow!("Proof generation timed out after 300 seconds"));
        }

        thread::sleep(Duration::from_secs(1));
    }
}

pub async fn generate_proof(block_number: u64, no_distributed: bool, no_server: bool, input_folder: String) -> Result<ProofResult> {
    let elf_file = format!(
        "{}/{}",
        PROGRAM_FOLDER,
        env::var("ELF_FILE").expect("ELF_FILE must be set")
    );
    let input_file = format!("{}/{}.bin", input_folder, block_number);
    let output_folder = format!("{}/{}", OUTPUT_FOLDER, block_number);

    create_dir_all(Path::new(OUTPUT_FOLDER))?;
    create_dir_all(Path::new(LOG_FOLDER))?;

    let num_processes =
        env::var("DISTRIBUTED_PROVE_PROCESSES").expect("DISTRIBUTED_PROVE_PROCESSES must be set");
    let num_threads =
        env::var("DISTRIBUTED_PROVE_THREADS").expect("DISTRIBUTED_PROVE_THREADS must be set");
    let prove_params =
        env::var("PROVE_PARAMS").unwrap_or_default();

    let command = if no_distributed {
        if no_server {
            info!("Generating proof without distributed proving");
            format!(
                "cargo-zisk prove -e {} -i {} -o {} -a -u {}",
                elf_file, input_file, output_folder, prove_params
            )
        } else {
            info!("Generating proof without distributed proving using server");
            format!(
                "cargo-zisk prove-client prove -i {} -a --port 6100 -p {} -o {} {}",
                input_file, block_number, output_folder, prove_params
            )
        }
    } else {
        if no_server {
            info!("Generating proof with distributed proving without server");
            format!(
                "mpirun --allow-run-as-root --bind-to none -np {} -x OMP_NUM_THREADS={} -x RAYON_NUM_THREADS={} cargo-zisk prove -e {} -i {} -o {} -a -u {}",
                num_processes, num_threads, num_threads, elf_file, input_file, output_folder, prove_params
            )
        } else {
            info!("Generating proof with distributed proving using server");
            format!(
                "mpirun --allow-run-as-root --bind-to none -np {} cargo-zisk prove-client prove -i {} -a --port 6100 -p {} -o {} {}",
                num_processes, input_file, block_number, output_folder, prove_params
            )
        }
    };

    let start = std::time::Instant::now();
    debug!("Starting proof generation for block number {} with command: {}", block_number, command);
    let mut child = Command::new("sh")
        .arg("-c")
        .arg(command)
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()?;
    debug!("Proof generation command started for block number {}", block_number);

    let stdout = child.stdout.take().expect("Failed to capture stdout");
    let reader = BufReader::new(stdout);
    debug!("Waiting for proof generation to start for block number {}", block_number);

    let mut proving = false;
    let mut captured_output = Vec::new();

    for line in reader.lines() {
        let line = line?;
        debug!("{}", line);
        captured_output.push(format!("{}\n", line));

        if let Some(start_idx) = line.find('{') {
            let json_str = &line[start_idx..];

            info!("Received JSON: {}", json_str);
            if let std::result::Result::Ok(json) = serde_json::from_str::<Value>(json_str) {
                let node = json.get("node").and_then(|n| n.as_u64());
                let result = json.get("result").and_then(|r| r.as_str());
                let code = json.get("code").and_then(|c| c.as_u64());

                info!("Parsed JSON: node: {:?}, result: {:?}, code: {:?}", node, result, code);
                if node == Some(0) && result == Some("in_progress") && code == Some(0) {
                    info!("Proof generation accepted for block number {}", block_number);
                    proving = true;
                    break;
                }
            }
        }
    }

    if !proving {
        return Err(anyhow!("Failed to start proof generation for block number {}", block_number));
    }

    let _status = child.wait()?;

    info!("Waiting for proof generation to complete for block number {}", block_number);
    wait_prove_done().await?;
    info!("Proof generated for block number {}, time: {}ms", block_number, start.elapsed().as_millis());

    if proving {
        let file = File::open(format!("{}/{}-result.json", output_folder, block_number))
            .context(format!(
                "Failed to open result.json for block number {}",
                block_number
            ))?;

        let reader = BufReader::new(file);
        let proof_result: ProofResult = serde_json::from_reader(reader)?;
        Ok(proof_result)
    } else {
        let log_path = format!("{}/{}.log", LOG_FOLDER, block_number);
        let mut log_file = File::create(&log_path)?;
        write!(log_file, "{}", captured_output.concat())?;

        Err(anyhow!("Proof verification failed for block number {}. Log saved at {}", block_number, log_path))
    }
}

/// Get the proof file for the given block number and return it as base64 encoded string
pub fn get_proof_b64(block_number: u64) -> Result<String> {
    let start = std::time::Instant::now();
    let proof_file = format!("{}/{}/{}-vadcop_final_proof.compressed.bin", OUTPUT_FOLDER, block_number, block_number);
    let buffer = fs::read(proof_file)?;
    let base64_encoded = general_purpose::STANDARD.encode(&buffer);
    info!("Compressed proof file for block number {} encoded to base64 in {}ms", block_number, start.elapsed().as_millis());

    Ok(base64_encoded)
}
